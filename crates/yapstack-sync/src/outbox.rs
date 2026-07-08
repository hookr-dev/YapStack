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

use crate::change::{read_local_changes_since, ChangeRow, Changeset};
use crate::crypto::ChangesetCipher;
use crate::quarantine::{merge_changeset, replay_pending};
use crate::state;
use crate::transport::SyncTransport;
use crate::SyncError;
use yapstack_common::sync::{PushChange, PushRequest, MAX_PUSH_BYTES, MAX_PUSH_CHANGES};

const PULL_LIMIT: i64 = 500;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Max rows captured into a single outbox entry (chunk) before flushing, independent of
/// byte size. Bounds the worst-case entry count and keeps each entry cheap to decrypt.
const CHUNK_ROWS: usize = 5_000;

/// Soft byte budget (pre-encryption plaintext) for one captured chunk. A chunk flushes
/// when EITHER `CHUNK_ROWS` rows OR this many serialized bytes accumulate, so a handful
/// of large blob/text values can't inflate one entry past the push limits. ~0.5 MiB
/// plaintext → ~0.5 MiB ciphertext → ~0.68 MiB base64, comfortably below the per-request
/// budget, so several entries still fit in one `POST /sync/push`.
const CHUNK_PLAINTEXT_BUDGET: usize = 512 * 1024;

/// Per-request budget for the base64-encoded ciphertext carried in one `POST /sync/push`.
///
/// The relay decodes base64 and caps the RAW ciphertext sum at [`MAX_PUSH_BYTES`] (5 MiB,
/// a 400), but the BINDING gate is the HTTP body itself: axum's default request-body limit
/// (2 MiB) sees the base64 JSON and rejects overflow with `413 Payload Too Large` — the
/// symptom this fixes. We keep the base64 ciphertext sum well under 2 MiB, leaving ample
/// headroom for the surrounding JSON envelope (field names, UUIDs).
const PUSH_WIRE_BUDGET: usize = 1_500_000;

/// Generous per-drain-cycle cap on entries uploaded, so an enormous initial sync still
/// eventually yields the connection to pulls instead of monopolizing the cycle. Pushes
/// keep looping across cycles until the outbox is drained.
const PUSH_MAX_ENTRIES_PER_CYCLE: usize = 100_000;

/// Serialized size one row contributes to a [`Changeset::encode`] payload. Mirrors the
/// `change.rs` codec exactly so the capture byte budget is measured, not guessed.
fn row_encoded_size(r: &ChangeRow) -> usize {
    use rusqlite::types::Value;
    let val = match &r.val {
        Value::Null => 1,
        Value::Integer(_) | Value::Real(_) => 1 + 8,
        Value::Text(s) => 1 + 4 + s.len(),
        Value::Blob(b) => 1 + 4 + b.len(),
    };
    let site = 1 + r.site_id.as_ref().map_or(0, |s| 4 + s.len());
    (4 + r.table.len()) + (4 + r.pk.len()) + (4 + r.cid.len()) + val + 8 + 8 + site + 8 + 8
}

/// Length of the STANDARD (padded) base64 encoding of `raw` bytes — what the relay
/// receives on the wire and what axum's body limit measures.
fn b64_len(raw: usize) -> usize {
    raw.div_ceil(3) * 4
}

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

/// A cheap point-in-time view of the outbox backlog, read straight from
/// `_yapstack_outbox` with a single `COUNT(*)` + `SUM(length(ciphertext))` over the
/// UNACKED rows. At this scale (a full initial sync is ~hundreds of in-budget entries)
/// this is a trivial scan, so it is cheap enough to call every drain cycle — no separate
/// stateful counter to keep in step with the table. Drives the desktop's push-progress
/// indicator (unacked count + unacked bytes).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OutboxPending {
    /// Number of unacked (captured-but-not-yet-acked) entries still to push.
    pub entries: u64,
    /// Total ciphertext bytes across those unacked entries. The base64 upload is
    /// ~4/3 of this; it is the honest on-device backlog size for a progress read.
    pub bytes: u64,
}

/// Read the current unacked backlog — count of entries AND total ciphertext bytes —
/// directly from the outbox. Returns zeroes when there is nothing pending. Ensures the
/// table exists first so a call before the first capture is a clean `{0,0}` rather than
/// an error.
pub fn pending(conn: &Connection) -> Result<OutboxPending, SyncError> {
    ensure_outbox_table(conn)?;
    let (entries, bytes): (i64, i64) = conn.query_row(
        "SELECT count(*), coalesce(sum(length(ciphertext)), 0) \
         FROM _yapstack_outbox WHERE acked=0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(OutboxPending {
        entries: entries.max(0) as u64,
        bytes: bytes.max(0) as u64,
    })
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

/// Split `rows` into bounded chunks (each ≤ [`CHUNK_ROWS`] rows AND ≤
/// [`CHUNK_PLAINTEXT_BUDGET`] serialized bytes) and insert each as its own encrypted
/// outbox entry, in row order, returning the assigned `client_seq`s. This is the ONE
/// capture-time chunking policy used by [`enqueue_local`] (fresh local writes). The
/// caller supplies the transaction.
fn chunk_and_insert(
    tx: &Connection,
    cipher: &ChangesetCipher,
    client_id: uuid::Uuid,
    schema_version: i32,
    engine_version: i32,
    rows: Vec<ChangeRow>,
) -> Result<Vec<i64>, SyncError> {
    let mut assigned = Vec::new();
    let mut buf: Vec<ChangeRow> = Vec::new();
    let mut acc = 4usize; // Changeset::encode row-count header
    for row in rows {
        let sz = row_encoded_size(&row);
        if !buf.is_empty() && (buf.len() >= CHUNK_ROWS || acc + sz > CHUNK_PLAINTEXT_BUDGET) {
            assigned.push(insert_chunk(
                tx,
                cipher,
                client_id,
                schema_version,
                engine_version,
                std::mem::take(&mut buf),
            )?);
            acc = 4;
        }
        acc += sz;
        buf.push(row);
    }
    if !buf.is_empty() {
        assigned.push(insert_chunk(
            tx,
            cipher,
            client_id,
            schema_version,
            engine_version,
            buf,
        )?);
    }
    Ok(assigned)
}

/// Encrypt one chunk of rows into a fresh outbox entry under the next `client_seq`.
fn insert_chunk(
    tx: &Connection,
    cipher: &ChangesetCipher,
    client_id: uuid::Uuid,
    schema_version: i32,
    engine_version: i32,
    rows: Vec<ChangeRow>,
) -> Result<i64, SyncError> {
    let seq = state::next_client_seq(tx)?;
    let blob = cipher.encrypt(client_id, seq, &Changeset { rows }.encode())?;
    tx.execute(
        "INSERT INTO _yapstack_outbox \
         (client_seq, ciphertext, schema_version, engine_version, acked) \
         VALUES (?1,?2,?3,?4,0)",
        rusqlite::params![seq, blob, schema_version, engine_version],
    )?;
    Ok(seq)
}

/// Capture this device's local writes since the push watermark into one or MORE
/// encrypted outbox entries, returning the assigned `client_seq`s (empty when there are
/// no new local changes).
///
/// On the FIRST sync of a real library the changes-since-watermark span hundreds of
/// thousands of `crsql_changes` rows (tens of MiB) — far past a single push. So the rows
/// are split into bounded chunks (each ≤ `CHUNK_ROWS` rows AND ≤ `CHUNK_PLAINTEXT_BUDGET`
/// serialized bytes), and every chunk becomes its own outbox row with its own monotonic
/// `client_seq`, in `crsql_changes` order. Capture + watermark advance run in ONE
/// transaction: a partial failure enqueues nothing and leaves the watermark untouched, so
/// the same rows retry cleanly (no data loss, no double-enqueue).
pub fn enqueue_local(
    conn: &Connection,
    cipher: &ChangesetCipher,
    client_id: uuid::Uuid,
    schema_version: i32,
    engine_version: i32,
) -> Result<Vec<i64>, SyncError> {
    state::ensure_meta_table(conn)?;
    ensure_outbox_table(conn)?;
    let wm = state::push_watermark(conn)?;
    let cs = read_local_changes_since(conn, wm)?;
    if cs.rows.is_empty() {
        return Ok(Vec::new());
    }
    let max_dbv = cs.rows.iter().map(|r| r.db_version).max().unwrap_or(wm);

    let tx = conn.unchecked_transaction()?;
    let assigned = chunk_and_insert(
        &tx,
        cipher,
        client_id,
        schema_version,
        engine_version,
        cs.rows,
    )?;
    state::set_push_watermark(&tx, max_dbv)?;
    tx.commit()?;
    Ok(assigned)
}

/// Push unacked outbox entries in as many back-to-back batches as it takes to drain them
/// (up to `PUSH_MAX_ENTRIES_PER_CYCLE`), marking each acked with its assigned
/// `changeset_seq`. Idempotent: a re-push of `(client_id, client_seq)` returns the
/// ORIGINAL seq and never double-inserts.
///
/// Every request respects BOTH server limits: at most [`MAX_PUSH_CHANGES`] entries AND a
/// base64 ciphertext body under [`PUSH_WIRE_BUDGET`] (which also keeps the raw sum under
/// [`MAX_PUSH_BYTES`]). Batches are planned from cheap `(seq, length)` metadata so we
/// never hold more blob bytes in memory than a single request carries. A mid-loop failure
/// simply leaves the remaining entries un-acked for the next cycle — no loss, no
/// double-count (the watermark advanced at capture time, and idempotency covers re-push).
async fn push_outbox<T: SyncTransport + ?Sized>(
    conn: &Connection,
    client_id: uuid::Uuid,
    transport: &T,
) -> Result<usize, SyncError> {
    ensure_outbox_table(conn)?;
    let mut total_acked = 0usize;

    loop {
        // Phase 1 — plan the next batch from cheap (seq, ciphertext length) metadata.
        let metas: Vec<(i64, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT client_seq, length(ciphertext) FROM _yapstack_outbox \
                 WHERE acked=0 ORDER BY client_seq LIMIT ?1",
            )?;
            let v = stmt
                .query_map([MAX_PUSH_CHANGES as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            v
        };
        if metas.is_empty() {
            break;
        }

        // Push GUARD (defensive, Bug B): the smallest-seq unacked entry is the one that
        // MUST go first (order + idempotency). If IT ALONE exceeds the wire budget (or the
        // raw byte cap) it can never be pushed as a single request — sending it is a
        // guaranteed 413. Capture-time chunking (T021) keeps every fresh entry in budget,
        // so this only fires for a pre-T021 dev-era entry no production install can have.
        // Refuse to make the HTTP call and return a DISTINCT error so the drain surfaces it
        // once instead of hot-looping every 5s.
        let (first_seq, first_len) = metas[0];
        let first_raw = first_len.max(0) as usize;
        let first_wire = b64_len(first_raw);
        if first_wire > PUSH_WIRE_BUDGET || first_raw > MAX_PUSH_BYTES {
            return Err(SyncError::Oversized {
                client_seq: first_seq,
                size: first_wire,
            });
        }

        let mut batch_seqs: Vec<i64> = Vec::new();
        let mut wire = 0usize;
        let mut raw = 0usize;
        for (seq, len) in &metas {
            let raw_len = (*len).max(0) as usize;
            let b64 = b64_len(raw_len);
            // The first (guard-checked, in-budget) entry always goes; subsequent entries
            // are added only while the batch stays under BOTH the wire budget and the raw
            // byte cap.
            if !batch_seqs.is_empty()
                && (batch_seqs.len() >= MAX_PUSH_CHANGES
                    || wire + b64 > PUSH_WIRE_BUDGET
                    || raw + raw_len > MAX_PUSH_BYTES)
            {
                break;
            }
            batch_seqs.push(*seq);
            wire += b64;
            raw += raw_len;
        }

        // Phase 2 — fetch just the planned entries' ciphertext and upload them.
        let mut changes = Vec::with_capacity(batch_seqs.len());
        {
            let mut stmt = conn.prepare(
                "SELECT ciphertext, schema_version, engine_version \
                 FROM _yapstack_outbox WHERE client_seq=?1",
            )?;
            for seq in &batch_seqs {
                let (blob, sv, ev): (Vec<u8>, i32, i32) =
                    stmt.query_row([seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                changes.push(PushChange {
                    client_id,
                    client_seq: *seq,
                    ciphertext: B64.encode(&blob),
                    schema_version: sv,
                    engine_version: ev,
                });
            }
        }

        let resp = transport.push(PushRequest { changes }).await?;
        for ack in &resp.acks {
            conn.execute(
                "UPDATE _yapstack_outbox SET acked=1, changeset_seq=?1 WHERE client_seq=?2",
                rusqlite::params![ack.changeset_seq, ack.client_seq],
            )?;
        }
        total_acked += resp.acks.len();

        // Defensive: a server that acks nothing must not spin the loop forever.
        if resp.acks.is_empty() {
            break;
        }
        // Yield to pulls on a truly enormous initial sync rather than starving them.
        if total_acked >= PUSH_MAX_ENTRIES_PER_CYCLE {
            break;
        }
    }

    Ok(total_acked)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::SnapshotMeta;
    use async_trait::async_trait;
    use rusqlite::types::Value;
    use std::sync::Mutex;
    use uuid::Uuid;
    use yapstack_common::sync::{CompletenessResponse, PullResponse, PushAck, PushResponse};

    /// A transport that ASSERTS every push obeys the wire limits and records each
    /// request's `(entries, raw_bytes, base64_bytes)` so tests can prove batching.
    #[derive(Default)]
    struct LimitAssertingTransport {
        requests: Mutex<Vec<(usize, usize, usize)>>,
        next_seq: Mutex<i64>,
    }

    #[async_trait]
    impl SyncTransport for LimitAssertingTransport {
        async fn push(&self, req: PushRequest) -> Result<PushResponse, SyncError> {
            let n = req.changes.len();
            assert!(n >= 1, "no empty requests");
            assert!(
                n <= MAX_PUSH_CHANGES,
                "entry count {n} exceeds MAX_PUSH_CHANGES"
            );
            let mut raw = 0usize;
            let mut b64 = 0usize;
            let mut acks = Vec::with_capacity(n);
            let mut seq = self.next_seq.lock().unwrap();
            for c in &req.changes {
                raw += B64.decode(c.ciphertext.as_bytes()).unwrap().len();
                b64 += c.ciphertext.len();
                *seq += 1;
                acks.push(PushAck {
                    client_id: c.client_id,
                    client_seq: c.client_seq,
                    changeset_seq: *seq,
                    deduplicated: false,
                });
            }
            assert!(
                raw <= MAX_PUSH_BYTES,
                "raw bytes {raw} exceeds MAX_PUSH_BYTES"
            );
            assert!(
                b64 <= PUSH_WIRE_BUDGET,
                "base64 body {b64} exceeds PUSH_WIRE_BUDGET"
            );
            self.requests.lock().unwrap().push((n, raw, b64));
            Ok(PushResponse {
                acks,
                max_changeset_seq: *seq,
            })
        }
        async fn pull(&self, _since: i64, _limit: i64) -> Result<PullResponse, SyncError> {
            Ok(PullResponse::default())
        }
        async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
            Ok(CompletenessResponse::default())
        }
        async fn put_snapshot(&self, _m: SnapshotMeta, _c: &[u8]) -> Result<(), SyncError> {
            Ok(())
        }
        async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
            Ok(None)
        }
    }

    /// A plain (no-CRR) in-memory DB with an `_yapstack_outbox` pre-seeded with the given
    /// opaque blobs. `push_outbox` touches only that table + the transport.
    fn seed_outbox(blobs: &[Vec<u8>]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_outbox_table(&conn).unwrap();
        for (i, blob) in blobs.iter().enumerate() {
            conn.execute(
                "INSERT INTO _yapstack_outbox \
                 (client_seq, ciphertext, schema_version, engine_version, acked) \
                 VALUES (?1,?2,1,16003,0)",
                rusqlite::params![(i as i64) + 1, blob],
            )
            .unwrap();
        }
        conn
    }

    #[tokio::test(flavor = "current_thread")]
    async fn push_splits_by_byte_budget_never_exceeding_limits() {
        // Six ~700 KiB entries: base64 ~933 KiB each, so only ONE fits per request.
        let blobs: Vec<Vec<u8>> = (0..6).map(|i| vec![i as u8; 700 * 1024]).collect();
        let conn = seed_outbox(&blobs);
        let t = LimitAssertingTransport::default();
        let acked = push_outbox(&conn, Uuid::from_u128(1), &t).await.unwrap();
        assert_eq!(acked, 6, "all entries acked in one drain");
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs.len(), 6, "one large entry per request");
        assert!(reqs.iter().all(|(n, _, _)| *n == 1));
        let unacked: i64 = conn
            .query_row(
                "SELECT count(*) FROM _yapstack_outbox WHERE acked=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unacked, 0, "outbox fully drained");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn push_caps_entry_count_at_max_push_changes() {
        // 2500 tiny entries → 1000 + 1000 + 500 across three requests.
        let blobs: Vec<Vec<u8>> = (0..2500).map(|_| vec![0u8; 8]).collect();
        let conn = seed_outbox(&blobs);
        let t = LimitAssertingTransport::default();
        let acked = push_outbox(&conn, Uuid::from_u128(2), &t).await.unwrap();
        assert_eq!(acked, 2500);
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!((reqs[0].0, reqs[1].0, reqs[2].0), (1000, 1000, 500));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn push_guard_refuses_oversized_entry_without_http_call() {
        // With repair skipped, an oversized first entry must yield SyncError::Oversized
        // and NEVER reach the transport (no guaranteed-413 HTTP call).
        struct NeverPush;
        #[async_trait]
        impl SyncTransport for NeverPush {
            async fn push(&self, _r: PushRequest) -> Result<PushResponse, SyncError> {
                panic!("push_outbox must NOT call the transport for an oversized entry");
            }
            async fn pull(&self, _s: i64, _l: i64) -> Result<PullResponse, SyncError> {
                Ok(PullResponse::default())
            }
            async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
                Ok(CompletenessResponse::default())
            }
            async fn put_snapshot(&self, _m: SnapshotMeta, _c: &[u8]) -> Result<(), SyncError> {
                Ok(())
            }
            async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
                Ok(None)
            }
        }

        let conn = Connection::open_in_memory().unwrap();
        ensure_outbox_table(&conn).unwrap();
        // Opaque bytes whose base64 wire size exceeds the budget (guard checks size only).
        let blob = vec![0u8; PUSH_WIRE_BUDGET];
        assert!(b64_len(blob.len()) > PUSH_WIRE_BUDGET);
        conn.execute(
            "INSERT INTO _yapstack_outbox \
             (client_seq, ciphertext, schema_version, engine_version, acked) \
             VALUES (1,?1,1,16003,0)",
            rusqlite::params![blob],
        )
        .unwrap();

        let err = push_outbox(&conn, Uuid::from_u128(1), &NeverPush)
            .await
            .unwrap_err();
        match err {
            SyncError::Oversized { client_seq, size } => {
                assert_eq!(client_seq, 1);
                assert!(size > PUSH_WIRE_BUDGET);
            }
            other => panic!("expected Oversized, got {other:?}"),
        }
    }

    #[test]
    fn pending_counts_and_sums_only_unacked_entries() {
        // Insert five entries of known ciphertext lengths, ack two, and assert the
        // pending() view reports exactly the unacked count AND the unacked byte sum
        // (acked entries excluded from both). This is the progress read the desktop
        // surfaces as "N items / X MiB remaining".
        let lens = [10usize, 20, 30, 40, 50];
        let blobs: Vec<Vec<u8>> = lens.iter().map(|&n| vec![0u8; n]).collect();
        let conn = seed_outbox(&blobs);

        // Fresh outbox: everything is unacked.
        let all = pending(&conn).unwrap();
        assert_eq!(all.entries, 5);
        assert_eq!(all.bytes, lens.iter().sum::<usize>() as u64); // 150

        // Ack the first two (client_seq 1 and 2, lengths 10 + 20).
        conn.execute(
            "UPDATE _yapstack_outbox SET acked=1 WHERE client_seq IN (1, 2)",
            [],
        )
        .unwrap();
        let after = pending(&conn).unwrap();
        assert_eq!(after.entries, 3, "only the three unacked entries remain");
        assert_eq!(
            after.bytes,
            (30 + 40 + 50) as u64,
            "sum excludes acked bytes"
        );

        // Ack the rest → fully drained reads as zero.
        conn.execute("UPDATE _yapstack_outbox SET acked=1", [])
            .unwrap();
        let drained = pending(&conn).unwrap();
        assert_eq!(drained, OutboxPending::default());
    }

    #[test]
    fn pending_on_empty_outbox_is_zero() {
        // A call before any capture must ensure the table and return {0,0}, never error.
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(pending(&conn).unwrap(), OutboxPending::default());
    }

    #[test]
    fn row_encoded_size_mirrors_the_codec_exactly() {
        // The capture byte budget is only safe if per-row size matches the real codec.
        let rows = vec![
            ChangeRow {
                table: "kv".into(),
                pk: vec![1, 2, 3],
                cid: "v".into(),
                val: Value::Text("héllo 世界".into()),
                col_version: 4,
                db_version: 9,
                site_id: Some(vec![9u8; 16]),
                cl: 1,
                seq: 0,
            },
            ChangeRow {
                table: "notes".into(),
                pk: vec![4, 5],
                cid: "-1".into(),
                val: Value::Blob(vec![7u8; 100]),
                col_version: 1,
                db_version: 9,
                site_id: None,
                cl: 1,
                seq: 1,
            },
        ];
        let est: usize = 4 + rows.iter().map(row_encoded_size).sum::<usize>();
        let actual = Changeset { rows }.encode().len();
        assert_eq!(
            est, actual,
            "row_encoded_size must mirror the codec exactly"
        );
    }
}
