// SPDX-License-Identifier: AGPL-3.0-only
//! The encrypted outbox and the push/pull drain.
//!
//! Local writes are captured as `crsql_changes` batches, encrypted under a
//! vault-derived data key (CRYPTO_SPEC §4/§5, see `crypto`), and staged in the
//! non-synced `_yapstack_outbox` before upload. The drain pushes unacked entries via
//! `/sync/push` (idempotent on `(client_id, client_seq)`), then pulls new changesets
//! via `/sync/pull`, decrypts, and feeds them through `crsql_changes` (merge), with
//! unknown-column changes quarantined (R7). The runtime NEVER sends plaintext.

use base64::Engine;
use rusqlite::Connection;

use crate::change::read_local_changes_since;
use crate::crypto::ChangesetCipher;
use crate::quarantine::{merge_changeset, replay_pending};
use crate::state;
use crate::transport::SyncTransport;
use crate::SyncError;
use yapstack_common::sync::{PushChange, PushRequest};

const PULL_LIMIT: i64 = 500;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// One drain cycle's outcome (diagnostics / tests).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    pub pushed: usize,
    pub applied: usize,
    pub quarantined: usize,
    pub replayed: usize,
    /// Changesets skipped because they failed to decrypt/decode (§11.3
    /// crypto-quarantine — surfaced, never silently dropped, never fatal).
    pub crypto_skipped: usize,
}

pub fn ensure_outbox_table(conn: &Connection) -> Result<(), SyncError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _yapstack_outbox (\
            client_seq INTEGER PRIMARY KEY, \
            ciphertext BLOB NOT NULL, \
            schema_version INTEGER NOT NULL, \
            engine_version INTEGER NOT NULL, \
            acked INTEGER NOT NULL DEFAULT 0, \
            changeset_seq INTEGER, \
            created_at TEXT NOT NULL DEFAULT (datetime('now')));",
    )?;
    Ok(())
}

/// Capture this device's local writes since the push watermark into a fresh,
/// encrypted outbox entry. Returns the assigned `client_seq`, or `None` if there
/// were no new local changes. Advances the push watermark to the batch's max
/// `db_version` so the same writes are never re-enqueued.
pub fn enqueue_local(
    conn: &Connection,
    cipher: &ChangesetCipher,
    client_id: uuid::Uuid,
    schema_version: i32,
    engine_version: i32,
) -> Result<Option<i64>, SyncError> {
    state::ensure_meta_table(conn)?;
    ensure_outbox_table(conn)?;
    let wm = state::push_watermark(conn)?;
    let cs = read_local_changes_since(conn, wm)?;
    if cs.rows.is_empty() {
        return Ok(None);
    }
    let max_dbv = cs.rows.iter().map(|r| r.db_version).max().unwrap_or(wm);
    let seq = state::next_client_seq(conn)?;
    let blob = cipher.encrypt(client_id, seq, &cs.encode())?;
    conn.execute(
        "INSERT INTO _yapstack_outbox \
         (client_seq, ciphertext, schema_version, engine_version, acked) \
         VALUES (?1,?2,?3,?4,0)",
        rusqlite::params![seq, blob, schema_version, engine_version],
    )?;
    state::set_push_watermark(conn, max_dbv)?;
    Ok(Some(seq))
}

/// Push all unacked outbox entries, marking each acked with its assigned
/// `changeset_seq`. Idempotent: a re-push returns the ORIGINAL seq.
async fn push_outbox<T: SyncTransport + ?Sized>(
    conn: &Connection,
    client_id: uuid::Uuid,
    transport: &T,
) -> Result<usize, SyncError> {
    ensure_outbox_table(conn)?;
    let pending: Vec<(i64, Vec<u8>, i32, i32)> = {
        let mut stmt = conn.prepare(
            "SELECT client_seq, ciphertext, schema_version, engine_version \
             FROM _yapstack_outbox WHERE acked=0 ORDER BY client_seq",
        )?;
        let v = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        v
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let changes = pending
        .into_iter()
        .map(|(client_seq, blob, sv, ev)| PushChange {
            client_id,
            client_seq,
            ciphertext: B64.encode(&blob),
            schema_version: sv,
            engine_version: ev,
        })
        .collect::<Vec<_>>();
    let resp = transport.push(PushRequest { changes }).await?;
    for ack in &resp.acks {
        conn.execute(
            "UPDATE _yapstack_outbox SET acked=1, changeset_seq=?1 WHERE client_seq=?2",
            rusqlite::params![ack.changeset_seq, ack.client_seq],
        )?;
    }
    Ok(resp.acks.len())
}

/// One full drain cycle: enqueue local writes, push, pull+decrypt+merge, replay.
///
/// Runs on a single-threaded runtime (it holds `&Connection` across awaits, so the
/// future is intentionally `!Send` — the desktop runs this on a dedicated thread).
pub async fn drain_once<T: SyncTransport + ?Sized>(
    conn: &Connection,
    cipher: &ChangesetCipher,
    transport: &T,
    client_id: uuid::Uuid,
    schema_version: i32,
    engine_version: i32,
) -> Result<DrainReport, SyncError> {
    let mut report = DrainReport::default();
    enqueue_local(conn, cipher, client_id, schema_version, engine_version)?;
    report.pushed = push_outbox(conn, client_id, transport).await?;

    loop {
        let since = state::pull_watermark(conn)?;
        let resp = transport.pull(since, PULL_LIMIT).await?;
        if resp.changes.is_empty() {
            break;
        }
        for pc in &resp.changes {
            if pc.client_id == client_id {
                continue; // our own echo; already local.
            }
            let blob = match B64.decode(pc.ciphertext.as_bytes()) {
                Ok(b) => b,
                Err(_) => {
                    report.crypto_skipped += 1;
                    continue;
                }
            };
            let pt = match cipher.decrypt(pc.client_id, pc.client_seq, &blob) {
                Ok(pt) => pt,
                Err(_) => {
                    report.crypto_skipped += 1;
                    continue;
                }
            };
            let cs = match crate::change::Changeset::decode(&pt) {
                Ok(cs) => cs,
                Err(_) => {
                    report.crypto_skipped += 1;
                    continue;
                }
            };
            let (a, q) = merge_changeset(conn, &cs)?;
            report.applied += a;
            report.quarantined += q;
        }
        state::set_pull_watermark(conn, resp.next_seq)?;
        if !resp.has_more {
            break;
        }
    }

    report.replayed = replay_pending(conn)?;
    Ok(report)
}
