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
///
/// A cycle attempts BOTH directions independently (F1): a push failure never prevents the
/// pull, and a pull failure never masks the push's progress. Each direction's error rides
/// on `push_error` / `pull_error` rather than aborting the cycle, so the caller can surface
/// each failure yet still credit whatever the other direction accomplished. `drain_once`
/// returns `Err` ONLY for a cycle-fatal LOCAL condition (a replay/DB error) — a transport
/// error on either direction is non-fatal and reported here instead.
///
/// Not `Clone`/`Copy`: `SyncError` is not `Clone`, and a report is consumed by-value in the
/// single drain loop that produces it.
#[derive(Debug, Default)]
pub struct DrainReport {
    pub pushed: usize,
    pub applied: usize,
    pub quarantined: usize,
    pub replayed: usize,
    /// Changesets skipped because they failed to decrypt/decode (§11.3
    /// crypto-quarantine — surfaced, never silently dropped, never fatal).
    pub crypto_skipped: usize,
    /// The push direction's non-fatal error this cycle (a transport 401 / oversized-guard /
    /// network error, or a local capture/outbox error), if any. `None` on a clean push.
    pub push_error: Option<SyncError>,
    /// The pull direction's non-fatal error this cycle, if any. `None` on a clean pull.
    pub pull_error: Option<SyncError>,
    /// This device's pull watermark (highest fully-merged commit-ordered `changeset_seq`) as
    /// of the END of this cycle. Compared against [`Self::server_max`] to decide the honest
    /// "up to date" vs. "catching up" state (R12) — an empty outbox alone is NOT up to date.
    pub pull_watermark: i64,
    /// The last-known tenant tip (`max_changeset_seq`) THIS cycle disclosed: from a push
    /// response's `max_changeset_seq`, and — after a clean full pull — the tip we drained to
    /// (which equals the final `pull_watermark`). `None` when no server response this cycle
    /// revealed it (nothing to push AND the pull failed), so the caller keeps its previous
    /// last-known tip rather than regressing it. `changeset_seq` is dense/monotonic, so this
    /// is folded as a running max and never rewinds.
    pub server_max: Option<i64>,
}

impl DrainReport {
    /// True when EITHER direction failed with a 401. The desktop drain refreshes the access
    /// token once and retries the WHOLE cycle (both directions) on this (T023, now covering
    /// pull as well as push).
    pub fn has_unauthorized(&self) -> bool {
        matches!(self.push_error, Some(SyncError::Unauthorized))
            || matches!(self.pull_error, Some(SyncError::Unauthorized))
    }

    /// The first non-fatal transport error observed this cycle (push preferred over pull),
    /// for the caller to surface. `None` when both directions were clean.
    pub fn first_transport_error(&self) -> Option<&SyncError> {
        self.push_error.as_ref().or(self.pull_error.as_ref())
    }

    /// Changesets this device is still behind the last-known relay tip — `server_max -
    /// pull_watermark`, clamped at 0. `None` when the tip is unknown this cycle (the caller
    /// keeps its previous last-known value). The PULL side is honestly caught up ONLY when
    /// this is `Some(0)`; any `Some(n > 0)` means a "catching up" state, never "up to date"
    /// (R12). Counts CHANGESETS (commit-ordered seqs), not cells/changes.
    pub fn pull_behind(&self) -> Option<i64> {
        self.server_max.map(|m| (m - self.pull_watermark).max(0))
    }
}

/// Fold a freshly-observed tenant tip into a report's running `server_max` max. A `None`
/// observation leaves the field untouched; `changeset_seq` is dense/monotonic so this only
/// ever advances (never rewinds a previously-known-higher tip).
fn fold_server_max(report: &mut DrainReport, observed: Option<i64>) {
    if let Some(v) = observed {
        report.server_max = Some(report.server_max.map_or(v, |m| m.max(v)));
    }
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

    // BEGIN IMMEDIATE: grab the write lock up front. This capture path READS then WRITES the
    // `client_seq` counter (state::next_client_seq) AND the outbox, all in one transaction. A
    // DEFERRED transaction takes the write lock only at the first write, so a second connection
    // (drain vs. capture) that read the same counter could force a read-then-write upgrade into
    // `SQLITE_BUSY` or a primary-key conflict on `client_seq`. IMMEDIATE + the connection's
    // `busy_timeout` (crsqlite::apply_sync_pragmas) makes the two writers serialize cleanly
    // (T023 Judge). `new_unchecked` because we hold only a `&Connection` here, not `&mut`.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
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
) -> Result<(usize, Option<i64>), SyncError> {
    ensure_outbox_table(conn)?;
    let mut total_acked = 0usize;
    // R12: the tenant tip disclosed by the last push response this cycle. `None` when we
    // never made a push request (empty outbox) — the caller then learns the tip from the
    // pull instead. Every `PushResponse.max_changeset_seq` is the tenant's tip AFTER that
    // push, so it is a fresh last-known tip for the honest up-to-date/catching-up decision.
    let mut server_max: Option<i64> = None;

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

        let resp = match transport.push(PushRequest { changes }).await {
            Ok(r) => r,
            // G3 — the relay returned 409 Conflict: our counter regressed and re-minted a seq
            // the relay already holds under DIFFERENT ciphertext. Recover in-band (reseed past
            // the tail, reset the watermark, drop the colliding entries) and return NON-FATALLY
            // so the next cycle re-walks and re-pushes under fresh seqs — instead of collapsing
            // into a permanent Failing loop (the wedge this closes). If completeness is ALSO
            // unreachable, `recover_from_conflict` propagates that error as push_error.
            Err(e) if crate::transport::is_conflict(&e) => {
                recover_from_conflict(conn, client_id, transport).await?;
                return Ok((total_acked, server_max));
            }
            Err(e) => return Err(e),
        };
        // R12: record the tenant tip this push disclosed (monotonic, so keep the max).
        server_max =
            Some(server_max.map_or(resp.max_changeset_seq, |m| m.max(resp.max_changeset_seq)));

        // Apply the genuinely-new acks (deduplicated=false) unconditionally: these entries
        // were freshly stored under their pushed seq.
        let mut applied = 0usize;
        for ack in resp.acks.iter().filter(|a| !a.deduplicated) {
            conn.execute(
                "UPDATE _yapstack_outbox SET acked=1, changeset_seq=?1 WHERE client_seq=?2",
                rusqlite::params![ack.changeset_seq, ack.client_seq],
            )?;
            applied += 1;
        }

        // G2 — deduplicated-ack backstop (for a relay that does NOT implement G3). A
        // `deduplicated=true` ack means the relay already holds this `(client_id, client_seq)`.
        // Against a G3 relay a real regression surfaces as a 409 (different ciphertext under the
        // reused pair, handled above), so here a dedup is normally the benign retry-after-lost-
        // ack (we pushed this exact entry before, its ack was lost, the relay holds OUR
        // ciphertext). Against a NO-G3 relay a dedup is AMBIGUOUS — it could instead be a counter
        // REGRESSION (a DB restore reused the seq over DIFFERENT content, the silent-loss bug the
        // old code hid by acking unconditionally). We disambiguate best-effort by asking the
        // relay for our tail (G1 reconcile): a regressed local counter is behind the relay tail.
        // This is heuristic, not exact — a regressed re-mint landing EXACTLY at the tail reads as
        // "no regression" and is acked, but that false-negative is benign (the relay's stored
        // copy of that pair is the same-or-newer converged data). Only a detected regression
        // reseeds. G1's per-spawn reconcile-before-push is the primary guard; this only catches
        // a residual regression that slipped a stale completeness view.
        let dedup_count = resp.acks.iter().filter(|a| a.deduplicated).count();
        if dedup_count > 0 {
            if reconcile_seq_counter(conn, client_id, transport).await? {
                // Regression confirmed: reconcile advanced the counter, reset the push
                // watermark, and DROPPED these unacked colliding entries — they re-mint under
                // fresh, non-colliding seqs on the next cycle's full re-walk. We do NOT ack
                // them (acking was the bug). Stop this drain's push here.
                tracing::error!(
                    target: "yapstack::sync",
                    deduplicated_entries = dedup_count,
                    "relay deduplicated a first-time push under a regressed client_seq counter; \
                     entries left UNACKED and re-queued for re-mint (silent data loss averted)"
                );
                total_acked += applied;
                break;
            }
            // No regression: a benign retry — the relay already holds OUR ciphertext under this
            // seq. Ack these entries as the idempotency contract intends.
            for ack in resp.acks.iter().filter(|a| a.deduplicated) {
                conn.execute(
                    "UPDATE _yapstack_outbox SET acked=1, changeset_seq=?1 WHERE client_seq=?2",
                    rusqlite::params![ack.changeset_seq, ack.client_seq],
                )?;
                applied += 1;
            }
        }

        total_acked += applied;

        // Defensive: a server that acks nothing must not spin the loop forever.
        if resp.acks.is_empty() {
            break;
        }
        // Yield to pulls on a truly enormous initial sync rather than starving them.
        if total_acked >= PUSH_MAX_ENTRIES_PER_CYCLE {
            break;
        }
    }

    Ok((total_acked, server_max))
}

/// One full drain cycle: enqueue local writes, push, pull+decrypt+merge, replay.
///
/// F1 — the two directions are DECOUPLED. Push (capture + upload) and pull (download +
/// merge) are each attempted every cycle: a push failure never aborts the pull, and a pull
/// failure never masks the push's progress. Each direction's error is preserved on the
/// returned [`DrainReport`] (`push_error` / `pull_error`) instead of short-circuiting the
/// cycle. Only a cycle-fatal LOCAL condition (a replay/DB error) returns `Err`.
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

    // PUSH direction: capture fresh local writes, then upload the outbox. A failure in EITHER
    // step rides on `push_error` and MUST NOT prevent the pull below (F1).
    match push_direction(
        conn,
        cipher,
        transport,
        client_id,
        schema_version,
        engine_version,
    )
    .await
    {
        Ok((pushed, srv_max)) => {
            report.pushed = pushed;
            fold_server_max(&mut report, srv_max);
        }
        Err(e) => report.push_error = Some(e),
    }

    // PULL direction: always attempted, even when push failed. A transport/pull error rides
    // on `pull_error`; whatever it managed to apply before failing is retained (F1).
    if let Err(e) = pull_direction(conn, cipher, transport, client_id, &mut report).await {
        report.pull_error = Some(e);
    }

    // R12: record the pull watermark reached this cycle so the caller can decide the honest
    // up-to-date vs. catching-up state. A CLEAN pull (no `pull_error`) drains to the relay
    // tip, so the watermark it reached IS a fresh last-known tip (folded as a running max);
    // a FAILED pull leaves the tip to whatever push disclosed (so the shortfall shows as a
    // real "behind" count rather than being masked).
    report.pull_watermark = state::pull_watermark(conn).unwrap_or(report.pull_watermark);
    if report.pull_error.is_none() {
        let wm = report.pull_watermark;
        fold_server_max(&mut report, Some(wm));
    }

    // Replay quarantined rows now this cycle's merges have landed. A failure here is a LOCAL
    // (DB-level) fault, hence cycle-fatal — surfaced as `Err`, not a per-direction error.
    report.replayed = replay_pending(conn)?;
    Ok(report)
}

/// G1 — reconcile this device's `client_seq` counter against the relay's record of our OWN
/// tail, repairing a counter REGRESSION (a DB restore that reset the counter to a value the
/// relay has already moved past — the confirmed silent-loss bug).
///
/// Fetches `GET /sync/completeness`, finds our `client_id`'s `max_client_seq`, and compares
/// it to the local last-assigned counter. If the relay holds a HIGHER seq, the counter
/// regressed and we:
/// - advance the counter to the server max, so the next mint is `server_max + 1` — past
///   every pair the relay already holds, so we stop re-minting the colliding pairs the relay
///   silently dedups;
/// - reset the push watermark to 0 so the next capture RE-WALKS the full change stream,
///   recovering edits whose watermark advanced but whose pushes were swallowed (this makes a
///   regressed device re-push its full history ONCE — idempotent: cr-sqlite merge converges,
///   and the re-encrypted entries carry NEW seqs so they are genuinely-new pairs, not dedups);
/// - DROP the unacked outbox entries, which carry the regressed colliding seqs; the re-walk
///   re-mints them under fresh seqs.
///
/// Logs a WARN naming the old counter and the server max (never key/token/ciphertext). Returns
/// `true` when a regression was repaired, `false` when the counter was already consistent. On
/// either success it records the per-spawn reconciled marker so steady-state drains this spawn
/// skip the round-trip (the marker is connection-scoped, so the next spawn reconciles again).
async fn reconcile_seq_counter<T: SyncTransport + ?Sized>(
    conn: &Connection,
    client_id: uuid::Uuid,
    transport: &T,
) -> Result<bool, SyncError> {
    state::ensure_meta_table(conn)?;
    ensure_outbox_table(conn)?;
    let resp = transport.completeness().await?;
    let server_max = resp
        .per_client
        .iter()
        .find(|t| t.client_id == client_id)
        .map(|t| t.max_client_seq)
        .unwrap_or(0);
    let local = state::client_seq(conn)?;
    if server_max <= local {
        // The counter is at or ahead of the relay tail — no regression. Record that we have
        // reconciled this spawn so future drains skip the completeness round-trip.
        state::mark_seq_reconciled(conn)?;
        return Ok(false);
    }

    tracing::warn!(
        target: "yapstack::sync",
        old_client_seq = local,
        server_max_client_seq = server_max,
        "client_seq counter regressed below the relay tail (DB restore?): advancing counter to \
         the server tail, resetting the push watermark to re-walk the full change stream, and \
         dropping unacked outbox entries for re-mint under fresh seqs"
    );
    reseed_counter_to_tail(conn, server_max)?;
    state::mark_seq_reconciled(conn)?;
    Ok(true)
}

/// Reseed the local `client_seq` counter to `target` (the relay tail), reset the push
/// watermark so the next capture RE-WALKS the full change stream, and DROP the unacked outbox
/// entries whose seqs may collide — all in ONE IMMEDIATE transaction so the reseed, watermark
/// reset, and purge are atomic with respect to a concurrent capture writer (same discipline as
/// `enqueue_local`). Shared by the G1 regression repair and the G3 conflict recovery. Never
/// rewinds: callers only ever pass a `target >= local`.
fn reseed_counter_to_tail(conn: &Connection, target: i64) -> Result<(), SyncError> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    state::set_client_seq(&tx, target)?;
    state::set_push_watermark(&tx, 0)?;
    tx.execute("DELETE FROM _yapstack_outbox WHERE acked=0", [])?;
    tx.commit()?;
    Ok(())
}

/// G3 — recover from a relay `409 Conflict`. The relay rejected a push whose
/// `(client_id, client_seq)` it already holds under DIFFERENT ciphertext: this device's
/// counter regressed (a DB restore) and re-minted a seq the relay already passed. Clear the
/// per-spawn reconciled marker, fetch the relay tail, then reseed the counter to at least that
/// tail — the 409 is proof of a collision AT the current seq, so we must mint strictly PAST it
/// (hence `server_max.max(local)`, never a rewind), reset the watermark, and drop the unacked
/// colliding entries. The next cycle re-walks and re-pushes under fresh, non-colliding seqs;
/// because the reseed advances past the tail, the same conflict CANNOT recur (no hot loop). If
/// completeness is unreachable, the error propagates as `push_error` (surfaced, never silent).
async fn recover_from_conflict<T: SyncTransport + ?Sized>(
    conn: &Connection,
    client_id: uuid::Uuid,
    transport: &T,
) -> Result<(), SyncError> {
    state::ensure_meta_table(conn)?;
    ensure_outbox_table(conn)?;
    state::clear_seq_reconciled(conn)?;
    let resp = transport.completeness().await?;
    let server_max = resp
        .per_client
        .iter()
        .find(|t| t.client_id == client_id)
        .map(|t| t.max_client_seq)
        .unwrap_or(0);
    let local = state::client_seq(conn)?;
    let target = server_max.max(local);
    tracing::warn!(
        target: "yapstack::sync",
        old_client_seq = local,
        server_max_client_seq = server_max,
        "relay rejected a colliding push (409 conflict): reseeding the counter past the relay \
         tail, resetting the push watermark to re-walk, and dropping unacked entries for re-mint \
         under fresh seqs (silent data loss averted)"
    );
    reseed_counter_to_tail(conn, target)?;
    // We have now reconciled this spawn against the relay tail.
    state::mark_seq_reconciled(conn)?;
    Ok(())
}

/// Capture this device's local writes, then push the outbox. The push half of one
/// [`drain_once`] cycle; its error is captured into `DrainReport::push_error` so it cannot
/// abort the pull. Preserves the capture-before-push ordering and the [`SyncError::Oversized`]
/// push guard.
async fn push_direction<T: SyncTransport + ?Sized>(
    conn: &Connection,
    cipher: &ChangesetCipher,
    transport: &T,
    client_id: uuid::Uuid,
    schema_version: i32,
    engine_version: i32,
) -> Result<(usize, Option<i64>), SyncError> {
    // G1 — before minting or pushing anything, reconcile the counter against the relay tail so
    // a regressed counter (DB restore) never emits colliding pairs. Runs exactly ONCE PER DRAIN
    // SPAWN: the reconciled marker is connection-scoped (a TEMP marker, see
    // `state::seq_reconciled`), so a fresh spawn (app launch / enable / re-login) re-checks and
    // every file-level restore self-heals — while steady-state cycles this spawn skip the
    // completeness round-trip. Pushes are GATED until one successful reconcile: if completeness
    // is unreachable we do NOT capture-or-push new entries under a possibly-regressed counter
    // (that is the collision window), we leave the marker unset, and we surface the error as
    // push_error so the drain retries reconcile next cycle. The pull half still runs (F1), so
    // the drain never wedges. A 401 propagates so the drain's token-refresh path still fires.
    if !state::seq_reconciled(conn)? {
        reconcile_seq_counter(conn, client_id, transport).await?;
    }
    enqueue_local(conn, cipher, client_id, schema_version, engine_version)?;
    push_outbox(conn, client_id, transport).await
}

/// Pull new changesets, decrypt, and merge them IN COMMIT ORDER, advancing the pull
/// watermark only past changesets that were fully merged (or are our own echo). The pull
/// half of one [`drain_once`] cycle; its error is captured into `DrainReport::pull_error`
/// so a download failure cannot mask push progress. Own-echo changes (matching `client_id`)
/// are skipped as already-local.
///
/// Honest crypto accounting (R5): a changeset we cannot decrypt/decode is NOT
/// skipped-and-forgotten. Skipping it would silently DROP a peer's write while advancing the
/// watermark past it — the "up to date" lie. Instead the pull STOPS at the first such
/// changeset: the watermark is left at the last fully-merged seq (so the next cycle retries
/// from exactly here) and no later, higher-seq changeset is merged out of order. The failure
/// rides back as [`SyncError::CryptoSkip`] (→ `DrainReport::pull_error`), carrying only the
/// seq / author `client_id` / a stage label — never key or plaintext material.
async fn pull_direction<T: SyncTransport + ?Sized>(
    conn: &Connection,
    cipher: &ChangesetCipher,
    transport: &T,
    client_id: uuid::Uuid,
    report: &mut DrainReport,
) -> Result<(), SyncError> {
    loop {
        let since = state::pull_watermark(conn)?;
        let resp = transport.pull(since, PULL_LIMIT).await?;
        if resp.changes.is_empty() {
            break;
        }
        // Highest seq FULLY consumed so far this page — merged, or legitimately skipped as our
        // own echo. On a crypto failure the watermark is pinned here so the retry resumes from
        // the failed changeset and no later one is applied ahead of it.
        let mut last_good = since;
        for pc in &resp.changes {
            if pc.client_id == client_id {
                last_good = pc.changeset_seq; // our own echo; already local.
                continue;
            }
            let cs = match decode_pulled_changeset(cipher, pc) {
                Ok(cs) => cs,
                Err(detail) => {
                    report.crypto_skipped += 1;
                    // Do NOT advance past this changeset. Pin the watermark at the last good
                    // seq and stop the pull; the next cycle retries from here.
                    state::set_pull_watermark(conn, last_good)?;
                    return Err(SyncError::CryptoSkip {
                        seq: pc.changeset_seq,
                        author_client_id: pc.client_id,
                        detail,
                    });
                }
            };
            // Merge THIS changeset in its own transaction (merge_changeset BEGIN..COMMIT),
            // then persist the watermark and yield BEFORE touching the next one. Item 1(a):
            // each changeset is a bounded merge transaction whose write lock is RELEASED at
            // its COMMIT, and the watermark only advances after that commit — so a concurrent
            // db_service writer (the frontend) can win the lock between changesets instead of
            // being starved for the whole page, and a crash mid-backlog resumes from exactly
            // the last committed changeset (the R5 halt-at-first-crypto-failure invariant is
            // unchanged: a decode failure above still pins the watermark at `last_good`).
            let (a, q) = merge_changeset(conn, &cs)?;
            report.applied += a;
            report.quarantined += q;
            last_good = pc.changeset_seq;
            state::set_pull_watermark(conn, last_good)?;
            // Cooperative yield point between batches. On the drain's single-thread runtime
            // this hands the executor a scheduling point between write transactions rather
            // than monopolising it across a long backlog page.
            tokio::task::yield_now().await;
        }
        // Whole page consumed cleanly — advance to the relay's reported next_seq (>= last_good;
        // covers a trailing run of our own echoes with no merge to move the watermark).
        state::set_pull_watermark(conn, resp.next_seq)?;
        if !resp.has_more {
            break;
        }
    }
    Ok(())
}

/// Base64-decode, decrypt, and decode one pulled changeset. On failure returns a SHORT,
/// non-sensitive label naming the stage that failed — deliberately NO ciphertext, plaintext,
/// key, nonce, or error detail that could leak key/plaintext material — so the caller can log
/// and surface it safely (R5).
fn decode_pulled_changeset(
    cipher: &ChangesetCipher,
    pc: &yapstack_common::sync::PulledChange,
) -> Result<Changeset, String> {
    let blob = B64
        .decode(pc.ciphertext.as_bytes())
        .map_err(|_| "ciphertext base64".to_string())?;
    let pt = cipher
        .decrypt(pc.client_id, pc.client_seq, &blob)
        .map_err(|_| "decrypt/authenticate".to_string())?;
    Changeset::decode(&pt).map_err(|_| "changeset decode".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::SnapshotMeta;
    use async_trait::async_trait;
    use rusqlite::types::Value;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// Two connections to the SAME on-disk sync DB, each running `BEGIN IMMEDIATE` write
    /// transactions that allocate `client_seq` (the exact read-then-write the capture path
    /// performs), must BOTH complete without `SQLITE_BUSY`: the `busy_timeout` pragma makes the
    /// second writer WAIT for the first to commit instead of erroring, and IMMEDIATE takes the
    /// write lock up front so there is no read-then-write upgrade race (T023 Judge). The final
    /// counter equals the total allocations — no lost or duplicated seq.
    #[test]
    fn concurrent_immediate_writers_serialize_without_busy() {
        use rusqlite::{Transaction, TransactionBehavior};
        let dir = std::env::temp_dir().join(format!("yapstack-busy-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("contend.db");
        {
            let c = Connection::open(&path).unwrap();
            state::ensure_meta_table(&c).unwrap();
        }

        const PER_THREAD: i64 = 50;
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let conn = Connection::open(&p).unwrap();
                    crate::crsqlite::apply_sync_pragmas(&conn).unwrap();
                    for _ in 0..PER_THREAD {
                        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)
                            .expect("BEGIN IMMEDIATE must wait for the lock, not fail busy");
                        // Read-then-write the shared counter, exactly like next_client_seq.
                        state::next_client_seq(&tx).unwrap();
                        tx.commit().unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            state::client_seq(&conn).unwrap(),
            PER_THREAD * 2,
            "every allocation must commit exactly once (no busy error, no lost seq)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
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
        let (acked, server_max) = push_outbox(&conn, Uuid::from_u128(1), &t).await.unwrap();
        assert_eq!(acked, 6, "all entries acked in one drain");
        assert_eq!(
            server_max,
            Some(6),
            "push discloses the tenant tip after the batch"
        );
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
        let (acked, _) = push_outbox(&conn, Uuid::from_u128(2), &t).await.unwrap();
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

    // ----- F1: push and pull are decoupled within one drain cycle -----

    use crate::transport::MockRelay;
    use crate::{CrsqlDb, CRSQLITE_ENGINE_VERSION, SYNC_SCHEMA_VERSION};
    use std::sync::Arc;

    const F1_TENANT: Uuid = Uuid::from_u128(0x2222_3333_4444_5555_6666_7777_8888_9999);
    const F1_VAULT: [u8; 32] = [11u8; 32];

    fn f1_cipher() -> ChangesetCipher {
        ChangesetCipher::new(
            F1_VAULT,
            0,
            F1_TENANT,
            SYNC_SCHEMA_VERSION,
            CRSQLITE_ENGINE_VERSION,
        )
    }

    fn make_kv(db: &CrsqlDb) {
        db.conn()
            .execute_batch(
                "CREATE TABLE kv(id TEXT NOT NULL PRIMARY KEY, v TEXT NOT NULL DEFAULT '');",
            )
            .unwrap();
        db.conn()
            .query_row("SELECT crsql_as_crr('kv')", [], |_| Ok(()))
            .unwrap();
    }

    /// Wraps a real [`MockRelay`] but ALWAYS fails push, delegating every other call — proves
    /// F1's "a push failure never prevents the pull".
    struct PushFails(Arc<MockRelay>);
    #[async_trait]
    impl SyncTransport for PushFails {
        async fn push(&self, _r: PushRequest) -> Result<PushResponse, SyncError> {
            Err(SyncError::Transport("push boom (test)".into()))
        }
        async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError> {
            self.0.pull(since, limit).await
        }
        async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
            self.0.completeness().await
        }
        async fn put_snapshot(&self, m: SnapshotMeta, c: &[u8]) -> Result<(), SyncError> {
            self.0.put_snapshot(m, c).await
        }
        async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
            self.0.get_snapshot().await
        }
    }

    /// Wraps a real [`MockRelay`] but ALWAYS fails pull, delegating every other call — proves
    /// F1's "a pull failure never masks push success".
    struct PullFails(Arc<MockRelay>);
    #[async_trait]
    impl SyncTransport for PullFails {
        async fn push(&self, r: PushRequest) -> Result<PushResponse, SyncError> {
            self.0.push(r).await
        }
        async fn pull(&self, _s: i64, _l: i64) -> Result<PullResponse, SyncError> {
            Err(SyncError::Transport("pull boom (test)".into()))
        }
        async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
            self.0.completeness().await
        }
        async fn put_snapshot(&self, m: SnapshotMeta, c: &[u8]) -> Result<(), SyncError> {
            self.0.put_snapshot(m, c).await
        }
        async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
            self.0.get_snapshot().await
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn push_fails_pull_proceeds() {
        // F1: with a transport whose push always errors but whose pull returns changesets,
        // one cycle STILL applies the pulled changes and advances the watermark — and the
        // push error is preserved on the report rather than aborting the cycle.
        let a = CrsqlDb::open_in_memory().unwrap();
        let b = CrsqlDb::open_in_memory().unwrap();
        make_kv(&a);
        make_kv(&b);
        let ca = state::client_id(a.conn()).unwrap();
        let cb = state::client_id(b.conn()).unwrap();
        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        // A publishes k1 to the relay through a healthy drain.
        a.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k1','A-val')", [])
            .unwrap();
        drain_once(a.conn(), &cipher, relay.as_ref(), ca, sv, ev)
            .await
            .unwrap();

        // B has a local write (so the push half has real work to fail on), then drains
        // through the push-failing transport.
        b.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k2','B-val')", [])
            .unwrap();
        let failing = PushFails(relay.clone());
        let report = drain_once(b.conn(), &cipher, &failing, cb, sv, ev)
            .await
            .unwrap();

        // Push failed and is reported; pull was clean and made progress.
        assert!(
            report.push_error.is_some(),
            "push error preserved on the report"
        );
        assert!(report.pull_error.is_none(), "pull was clean");
        assert_eq!(report.pushed, 0, "nothing acked (push errored)");
        assert!(
            report.applied > 0,
            "A's change applied despite the push failure"
        );
        let bk1: String = b
            .conn()
            .query_row("SELECT v FROM kv WHERE id='k1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bk1, "A-val", "the pulled change merged");
        assert!(
            state::pull_watermark(b.conn()).unwrap() > 0,
            "watermark advanced after the successful merge"
        );
        // B's local k2 never reached the relay (push failed): it still holds only A's entry.
        assert_eq!(relay.completeness().await.unwrap().count, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pull_fails_push_proceeds() {
        // F1 (inverse): with a transport whose pull always errors, the push half STILL
        // uploads local writes — the pull error is preserved rather than masking the push.
        let b = CrsqlDb::open_in_memory().unwrap();
        make_kv(&b);
        let cb = state::client_id(b.conn()).unwrap();
        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        b.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k9','B-only')", [])
            .unwrap();
        let failing = PullFails(relay.clone());
        let report = drain_once(b.conn(), &cipher, &failing, cb, sv, ev)
            .await
            .unwrap();

        // Push succeeded and is credited; pull failed and is reported.
        assert!(
            report.pull_error.is_some(),
            "pull error preserved on the report"
        );
        assert!(report.push_error.is_none(), "push was clean");
        assert_eq!(
            report.pushed, 1,
            "the local write was pushed despite the pull failure"
        );
        // B's change is durably on the relay.
        assert_eq!(relay.completeness().await.unwrap().count, 1);
    }

    // ----- R12: a fully-acked outbox is NOT "up to date" while the pull is behind -----

    #[tokio::test(flavor = "current_thread")]
    async fn full_outbox_ack_is_not_up_to_date_while_pull_behind() {
        // The R12 bug: "up to date" was declared whenever the OUTBOX drained, ignoring the
        // PULL backlog. The report now exposes `pull_behind()` (server tip − pull watermark)
        // so the caller can gate honestly: a cycle that fully acks the outbox while the pull
        // has NOT reached the relay tip reports `pull_behind() > 0` (NOT up to date); only
        // once the watermark reaches the tip does it read caught up (`Some(0)`).
        let a = CrsqlDb::open_in_memory().unwrap();
        let b = CrsqlDb::open_in_memory().unwrap();
        make_kv(&a);
        make_kv(&b);
        let ca = state::client_id(a.conn()).unwrap();
        let cb = state::client_id(b.conn()).unwrap();
        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        // A publishes three keys → the relay tip advances to 3.
        for i in 0..3 {
            a.conn()
                .execute(&format!("INSERT INTO kv(id,v) VALUES('a{i}','v{i}')"), [])
                .unwrap();
            drain_once(a.conn(), &cipher, relay.as_ref(), ca, sv, ev)
                .await
                .unwrap();
        }
        assert_eq!(relay.completeness().await.unwrap().max_changeset_seq, 3);

        // B pushes ONE local write (fully acked) but its PULL fails this cycle: the outbox
        // empties, yet B is still behind A's changesets. The push disclosed the tenant tip,
        // so `pull_behind()` is a POSITIVE, honest shortfall — never "up to date".
        b.conn()
            .execute("INSERT INTO kv(id,v) VALUES('b0','x')", [])
            .unwrap();
        let failing = PullFails(relay.clone());
        let behind_report = drain_once(b.conn(), &cipher, &failing, cb, sv, ev)
            .await
            .unwrap();
        assert_eq!(behind_report.pushed, 1, "B's local write was fully acked");
        assert!(
            behind_report.pull_error.is_some(),
            "the pull failed this cycle"
        );
        let behind = behind_report
            .pull_behind()
            .expect("push disclosed the tenant tip");
        assert!(
            behind > 0,
            "an empty outbox with an un-caught-up pull is NOT up to date (behind={behind})"
        );

        // A clean drain: B pulls A's changesets and reaches the relay tip → caught up.
        let caught_up = drain_once(b.conn(), &cipher, relay.as_ref(), cb, sv, ev)
            .await
            .unwrap();
        assert!(
            caught_up.first_transport_error().is_none(),
            "the catch-up cycle was clean"
        );
        assert_eq!(
            caught_up.pull_behind(),
            Some(0),
            "up to date ONLY once the watermark reached the relay tip"
        );
        // Sanity: B actually merged A's rows.
        let av: String = b
            .conn()
            .query_row("SELECT v FROM kv WHERE id='a2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(av, "v2");
    }

    // ----- R5: honest crypto accounting — a crypto skip stops the pull, never lies -----

    /// Wraps a [`MockRelay`] and, on pull, replaces the ciphertext of the changeset at
    /// `corrupt_seq` with a value that cannot be base64-decoded — modelling a peer write this
    /// device cannot decrypt/decode. Every genuine changeset passes through untouched.
    struct CorruptPullAt {
        inner: Arc<MockRelay>,
        corrupt_seq: i64,
    }
    #[async_trait]
    impl SyncTransport for CorruptPullAt {
        async fn push(&self, r: PushRequest) -> Result<PushResponse, SyncError> {
            self.inner.push(r).await
        }
        async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError> {
            let mut resp = self.inner.pull(since, limit).await?;
            for c in &mut resp.changes {
                if c.changeset_seq == self.corrupt_seq {
                    c.ciphertext = "!!! not base64 !!!".to_string();
                }
            }
            Ok(resp)
        }
        async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
            self.inner.completeness().await
        }
        async fn put_snapshot(&self, m: SnapshotMeta, c: &[u8]) -> Result<(), SyncError> {
            self.inner.put_snapshot(m, c).await
        }
        async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
            self.inner.get_snapshot().await
        }
    }

    /// Publish three distinct changesets from `a` through a healthy `relay` → dense seq 1,2,3.
    async fn publish_three(a: &CrsqlDb, relay: &MockRelay, cipher: &ChangesetCipher, ca: Uuid) {
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);
        for (k, v) in [("k1", "v1"), ("k2", "v2"), ("k3", "v3")] {
            a.conn()
                .execute(
                    "INSERT INTO kv(id,v) VALUES(?1,?2)",
                    rusqlite::params![k, v],
                )
                .unwrap();
            drain_once(a.conn(), cipher, relay, ca, sv, ev)
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pull_stops_at_first_crypto_failure_and_retries() {
        // R5: three changesets where changeset #2 fails to decode. The pull must merge #1, STOP
        // at #2 (never merging the later #3 out of order), leave the watermark at #1, and
        // surface a typed CryptoSkip. A later readable cycle resumes from #2 and fully catches
        // up. This is the "up to date" lie mechanism, defused.
        let a = CrsqlDb::open_in_memory().unwrap();
        let b = CrsqlDb::open_in_memory().unwrap();
        make_kv(&a);
        make_kv(&b);
        let ca = state::client_id(a.conn()).unwrap();
        let cb = state::client_id(b.conn()).unwrap();
        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        publish_three(&a, relay.as_ref(), &cipher, ca).await;
        assert_eq!(relay.completeness().await.unwrap().count, 3);

        let has = |k: &str| -> bool {
            let n: i64 = b
                .conn()
                .query_row("SELECT count(*) FROM kv WHERE id=?1", [k], |r| r.get(0))
                .unwrap();
            n > 0
        };

        // Cycle 1: pull through a transport that corrupts changeset seq=2.
        let corrupt = CorruptPullAt {
            inner: relay.clone(),
            corrupt_seq: 2,
        };
        let report = drain_once(b.conn(), &cipher, &corrupt, cb, sv, ev)
            .await
            .unwrap();

        match report.pull_error {
            Some(SyncError::CryptoSkip {
                seq,
                author_client_id,
                ..
            }) => {
                assert_eq!(seq, 2, "stopped at the FIRST failing changeset");
                assert_eq!(author_client_id, ca, "surfaces the authoring client_id");
            }
            other => panic!("expected CryptoSkip pull_error, got {other:?}"),
        }
        assert_eq!(
            report.crypto_skipped, 1,
            "exactly one crypto skip this cycle"
        );
        assert!(report.applied > 0, "#1 merged before the failure");
        assert_eq!(
            state::pull_watermark(b.conn()).unwrap(),
            1,
            "watermark pinned at the last good seq, NOT advanced past the failure"
        );
        assert!(has("k1"), "#1 (before the failure) applied");
        assert!(!has("k2"), "#2 (the failure) not applied");
        assert!(!has("k3"), "#3 (after the failure) NOT merged out of order");

        // Cycle 2: retry against the still-corrupt transport makes NO forward progress.
        let report2 = drain_once(b.conn(), &cipher, &corrupt, cb, sv, ev)
            .await
            .unwrap();
        assert!(
            matches!(
                report2.pull_error,
                Some(SyncError::CryptoSkip { seq: 2, .. })
            ),
            "retry re-hits the same block"
        );
        assert_eq!(
            state::pull_watermark(b.conn()).unwrap(),
            1,
            "still no progress past the block"
        );

        // Cycle 3: the changeset is readable again → resume from #2 and catch up fully.
        let report3 = drain_once(b.conn(), &cipher, relay.as_ref(), cb, sv, ev)
            .await
            .unwrap();
        assert!(report3.pull_error.is_none(), "clean pull after recovery");
        assert_eq!(report3.crypto_skipped, 0);
        assert_eq!(
            state::pull_watermark(b.conn()).unwrap(),
            3,
            "watermark fully caught up"
        );
        assert!(
            has("k2") && has("k3"),
            "the blocked and later changesets now applied"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn all_clean_pull_advances_fully() {
        // The happy path is unchanged: no crypto failures → watermark advances to the last seq,
        // every changeset merges, and the report is clean (no false crypto_skipped).
        let a = CrsqlDb::open_in_memory().unwrap();
        let b = CrsqlDb::open_in_memory().unwrap();
        make_kv(&a);
        make_kv(&b);
        let ca = state::client_id(a.conn()).unwrap();
        let cb = state::client_id(b.conn()).unwrap();
        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        publish_three(&a, relay.as_ref(), &cipher, ca).await;

        let report = drain_once(b.conn(), &cipher, relay.as_ref(), cb, sv, ev)
            .await
            .unwrap();
        assert!(report.pull_error.is_none());
        assert_eq!(
            report.crypto_skipped, 0,
            "no crypto skips on the clean path"
        );
        assert_eq!(
            state::pull_watermark(b.conn()).unwrap(),
            3,
            "advanced fully"
        );
        for k in ["k1", "k2", "k3"] {
            let n: i64 = b
                .conn()
                .query_row("SELECT count(*) FROM kv WHERE id=?1", [k], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1, "changeset {k} merged");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn merge_releases_write_lock_between_batches() {
        // Item 1(a): a pulled backlog is merged one changeset per BEGIN..COMMIT batch, and the
        // write lock MUST be released at each COMMIT so a concurrent writer (the frontend's
        // db_service connection) can win the lock BETWEEN batches instead of being starved
        // across the whole page. Modelled with a second, independent connection that writes
        // between two `merge_changeset` calls; a SHORT busy_timeout makes it fail fast if the
        // merge left a transaction open.
        use crate::quarantine::merge_changeset;

        // A source device authors TWO separate changesets (two db_versions).
        let src = CrsqlDb::open_in_memory().unwrap();
        make_kv(&src);
        src.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k1','v1')", [])
            .unwrap();
        let cs1 = crate::change::read_local_changes_since(src.conn(), 0).unwrap();
        let v1 = crate::change::max_local_db_version(src.conn()).unwrap();
        src.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k2','v2')", [])
            .unwrap();
        let cs2 = crate::change::read_local_changes_since(src.conn(), v1).unwrap();
        assert!(!cs1.rows.is_empty() && !cs2.rows.is_empty());

        // Target on-disk DB so a SECOND connection can contend for the file's write lock.
        let dir = std::env::temp_dir().join(format!("yapstack-merge-lock-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dst.db");
        let dst = CrsqlDb::open(&path).unwrap();
        dst.conn()
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
        make_kv(&dst);

        // The stand-in for db_service: an independent writer with SHORT patience.
        let probe = Connection::open(&path).unwrap();
        probe
            .busy_timeout(std::time::Duration::from_millis(200))
            .unwrap();
        probe
            .execute_batch("CREATE TABLE probe_t(id INTEGER PRIMARY KEY, n INTEGER);")
            .unwrap();

        // Batch 1 → lock released at COMMIT → the probe can write BETWEEN batches.
        merge_changeset(dst.conn(), &cs1).unwrap();
        assert!(
            dst.conn().is_autocommit(),
            "write lock released after batch 1 (connection back in autocommit)"
        );
        probe
            .execute("INSERT INTO probe_t(n) VALUES(1)", [])
            .expect("probe must win the write lock between batches");

        // Batch 2 → lock released again.
        merge_changeset(dst.conn(), &cs2).unwrap();
        assert!(
            dst.conn().is_autocommit(),
            "write lock released after batch 2"
        );
        probe
            .execute("INSERT INTO probe_t(n) VALUES(2)", [])
            .expect("probe must win the write lock after the final batch");

        let n: i64 = probe
            .query_row("SELECT count(*) FROM probe_t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "both interleaved probe writes committed");
        drop(probe);
        drop(dst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----- G1/G2: client_seq counter-regression recovery (silent-loss bug) -----

    /// Decrypt every changeset the `relay` holds for `client_id` and report whether any of
    /// them carries a `kv` row whose text value equals `needle` — i.e. proof the content
    /// actually reached the relay (not a silent loss).
    async fn relay_holds_kv_value(
        relay: &MockRelay,
        cipher: &ChangesetCipher,
        client_id: Uuid,
        needle: &str,
    ) -> bool {
        let pull = relay.pull(0, 100_000).await.unwrap();
        for pc in &pull.changes {
            if pc.client_id != client_id {
                continue;
            }
            let Ok(blob) = B64.decode(pc.ciphertext.as_bytes()) else {
                continue;
            };
            let Ok(pt) = cipher.decrypt(pc.client_id, pc.client_seq, &blob) else {
                continue;
            };
            let Ok(cs) = Changeset::decode(&pt) else {
                continue;
            };
            if cs
                .rows
                .iter()
                .any(|r| matches!(&r.val, Value::Text(s) if s == needle))
            {
                return true;
            }
        }
        false
    }

    /// Model a DB restore to a pre-sync backup for the IN-MEMORY tests: everything the local DB
    /// held rolls back — the `client_seq` counter, the watermarks, AND the outbox (all mutually
    /// consistent with each other in the backup) — while `client_id` PERSISTS (it comes from the
    /// credential store, not the DB, and survives the restore). Crucially there is NO persisted
    /// reconciled flag to restore any more (that was the bug): the marker is connection-scoped,
    /// so a real restore-then-launch opens a FRESH connection with no marker. These in-memory
    /// tests reuse one connection, so we drop the temp marker here to faithfully model that
    /// fresh-connection spawn. The relay is untouched, so it still holds the old
    /// `(client_id, client_seq)` pairs the restarted counter will collide with — the confirmed
    /// silent-loss setup. (The on-disk `restore_carrying_stale_counter_still_reaches_relay`
    /// proves the same via an actual connection reopen with meta left intact.)
    fn simulate_db_restore(conn: &Connection) {
        conn.execute(
            "DELETE FROM _yapstack_sync_meta WHERE key <> 'client_id'",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM _yapstack_outbox", []).unwrap();
        // Model the fresh drain connection a real restore-then-launch opens: the connection-
        // scoped reconciled marker is absent, so G1 reconcile re-runs this spawn.
        conn.execute("DROP TABLE IF EXISTS _yapstack_sync_session", [])
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn counter_regression_reseeds_and_recovers_without_silent_loss() {
        // The two-device silent-loss bug, reproduced end to end and fixed by G1. A device
        // pushes edits to the relay, then its DB is restored to a pre-CRR backup: the
        // client_seq counter restarts while the relay still holds the old pairs. A fresh edit
        // made AFTER the restore must still reach the relay — the bug swallowed it. G1 detects
        // the regression from the relay tail, reseeds the counter past it, and re-walks the
        // full change stream so the edit lands under a fresh, non-colliding seq.
        let dev = CrsqlDb::open_in_memory().unwrap();
        make_kv(&dev);
        let cid = state::client_id(dev.conn()).unwrap();
        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        // Original run: three edits pushed to the relay under this device's seqs 1..3.
        for (k, v) in [("k1", "v1"), ("k2", "v2"), ("k3", "v3")] {
            dev.conn()
                .execute(
                    "INSERT INTO kv(id,v) VALUES(?1,?2)",
                    rusqlite::params![k, v],
                )
                .unwrap();
            drain_once(dev.conn(), &cipher, relay.as_ref(), cid, sv, ev)
                .await
                .unwrap();
        }
        let comp = relay.completeness().await.unwrap();
        let tail = comp
            .per_client
            .iter()
            .find(|t| t.client_id == cid)
            .map(|t| t.max_client_seq)
            .unwrap_or(0);
        assert_eq!(tail, 3, "relay holds our seqs 1..3");
        let rows_before = comp.count;

        // Restore wipes the counter + reconciled flag; the relay is untouched.
        simulate_db_restore(dev.conn());
        assert_eq!(
            state::client_seq(dev.conn()).unwrap(),
            0,
            "counter restarted"
        );
        assert!(
            !state::seq_reconciled(dev.conn()).unwrap(),
            "reconciled flag lost with the DB restore"
        );

        // A new edit made AFTER the restore — the write the bug silently dropped.
        dev.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k4','post-restore')", [])
            .unwrap();

        let report = drain_once(dev.conn(), &cipher, relay.as_ref(), cid, sv, ev)
            .await
            .unwrap();
        assert!(
            report.push_error.is_none(),
            "push is clean after the reseed: {:?}",
            report.push_error
        );

        // The counter was reseeded past the relay tail, and the reconciled flag is set so
        // steady-state drains skip the completeness round-trip.
        assert!(
            state::client_seq(dev.conn()).unwrap() >= 3,
            "counter advanced to at least the server tail"
        );
        assert!(state::seq_reconciled(dev.conn()).unwrap());

        // No silent loss: the relay grew (the full re-walk landed under NEW pairs) and the
        // post-restore edit is decryptable on the relay under one of our fresh seqs.
        assert!(
            relay.completeness().await.unwrap().count > rows_before,
            "the re-walk pushed fresh changesets under new seqs"
        );
        assert!(
            relay_holds_kv_value(relay.as_ref(), &cipher, cid, "post-restore").await,
            "the post-restore edit reached the relay (no silent loss)"
        );
    }

    /// A relay that fails `completeness` a fixed number of times (then delegates), so a test
    /// can drive G1 to DEFER (completeness unreachable at drain start) yet let G2's later
    /// reconcile succeed. Every other call delegates to the inner [`MockRelay`].
    struct CompletenessFlaky {
        inner: Arc<MockRelay>,
        fail_remaining: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl SyncTransport for CompletenessFlaky {
        async fn push(&self, r: PushRequest) -> Result<PushResponse, SyncError> {
            self.inner.push(r).await
        }
        async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError> {
            self.inner.pull(since, limit).await
        }
        async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
            use std::sync::atomic::Ordering;
            if self
                .fail_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    if n > 0 {
                        Some(n - 1)
                    } else {
                        None
                    }
                })
                .is_ok()
            {
                return Err(SyncError::Network("completeness down (test)".into()));
            }
            self.inner.completeness().await
        }
        async fn put_snapshot(&self, m: SnapshotMeta, c: &[u8]) -> Result<(), SyncError> {
            self.inner.put_snapshot(m, c).await
        }
        async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
            self.inner.get_snapshot().await
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deferred_reconcile_gates_push_no_silent_loss() {
        // F1 gating: when G1 cannot reconcile at drain start (completeness unreachable), the
        // push is GATED — we do NOT capture-or-push new entries under a possibly-regressed
        // counter (the collision window). The failure rides on push_error (surfaced), nothing
        // is pushed, and the post-restore edit stays queued locally. The NEXT cycle (relay
        // healthy) reconciles, reseeds, re-walks, and the edit lands — no silent loss.
        let dev = CrsqlDb::open_in_memory().unwrap();
        make_kv(&dev);
        let cid = state::client_id(dev.conn()).unwrap();
        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        // Original run establishes relay seqs 1..2 for this device.
        for (k, v) in [("k1", "v1"), ("k2", "v2")] {
            dev.conn()
                .execute(
                    "INSERT INTO kv(id,v) VALUES(?1,?2)",
                    rusqlite::params![k, v],
                )
                .unwrap();
            drain_once(dev.conn(), &cipher, relay.as_ref(), cid, sv, ev)
                .await
                .unwrap();
        }
        simulate_db_restore(dev.conn());
        dev.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k3','post-restore-gate')", [])
            .unwrap();

        // Cycle 1: completeness is down, so reconcile fails and the push is gated.
        let flaky = CompletenessFlaky {
            inner: relay.clone(),
            fail_remaining: std::sync::atomic::AtomicUsize::new(1),
        };
        let report = drain_once(dev.conn(), &cipher, &flaky, cid, sv, ev)
            .await
            .unwrap();
        assert!(
            report.push_error.is_some(),
            "reconcile failure surfaces as push_error (never silent)"
        );
        assert_eq!(
            report.pushed, 0,
            "nothing pushed while reconcile is deferred"
        );
        // The counter was NOT reseeded (reconcile never succeeded) and the edit stayed local.
        assert!(
            !relay_holds_kv_value(relay.as_ref(), &cipher, cid, "post-restore-gate").await,
            "the gated push did not land — it is queued locally, not lost"
        );

        // Cycle 2: relay healthy → reconcile reseeds past the tail, re-walk re-mints, edit lands.
        let report2 = drain_once(dev.conn(), &cipher, relay.as_ref(), cid, sv, ev)
            .await
            .unwrap();
        assert!(report2.push_error.is_none());
        assert!(
            state::client_seq(dev.conn()).unwrap() >= 2,
            "counter reseeded to at least the relay tail once reconcile succeeded"
        );
        assert!(
            relay_holds_kv_value(relay.as_ref(), &cipher, cid, "post-restore-gate").await,
            "after reseed + re-mint the post-restore edit reaches the relay (no silent loss)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dedup_regression_backstop_reseeds_via_push_outbox() {
        // The no-G3-relay backstop inside push_outbox, exercised DIRECTLY (push_direction's G1
        // gate normally prevents an unreconciled push, so we call push_outbox on a deliberately
        // regressed counter to pin the backstop). MockRelay dedups silently (models a relay
        // WITHOUT G3): a regressed counter that re-mints a seq the relay already holds under
        // different content gets a `deduplicated=true` ack. The backstop asks the relay for our
        // tail, sees the local counter is BEHIND it, and reseeds instead of silently acking the
        // lost edit.
        let dev = CrsqlDb::open_in_memory().unwrap();
        make_kv(&dev);
        let cid = state::client_id(dev.conn()).unwrap();
        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        // Establish relay seqs 1..3 for this device, then clear the (acked) outbox.
        for (k, v) in [("k1", "v1"), ("k2", "v2"), ("k3", "v3")] {
            dev.conn()
                .execute(
                    "INSERT INTO kv(id,v) VALUES(?1,?2)",
                    rusqlite::params![k, v],
                )
                .unwrap();
            drain_once(dev.conn(), &cipher, relay.as_ref(), cid, sv, ev)
                .await
                .unwrap();
        }
        dev.conn()
            .execute("DELETE FROM _yapstack_outbox", [])
            .unwrap();

        // Regress the counter to 0 while leaving the push watermark past k1..k3, so the next
        // capture mints seq 1 for ONLY the new edit — colliding with the relay's seq 1 under
        // different content. local (=1) is strictly behind the relay tail (=3): a real
        // regression the backstop must detect.
        state::set_client_seq(dev.conn(), 0).unwrap();
        dev.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k4','post-restore-dedup')", [])
            .unwrap();
        let assigned = enqueue_local(dev.conn(), &cipher, cid, sv, ev).unwrap();
        assert_eq!(assigned, vec![1], "the new edit minted the colliding seq 1");

        // Push directly: the relay dedups seq 1; the backstop reconciles, detects the
        // regression (server_max 3 > local 1), reseeds past the tail, and drops the entry.
        let (acked, _) = push_outbox(dev.conn(), cid, relay.as_ref()).await.unwrap();
        assert_eq!(acked, 0, "the suspect dedup was NOT acked");
        assert!(
            state::client_seq(dev.conn()).unwrap() >= 3,
            "counter reseeded to at least the relay tail"
        );
        let unacked: i64 = dev
            .conn()
            .query_row(
                "SELECT count(*) FROM _yapstack_outbox WHERE acked=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unacked, 0, "the colliding entry was dropped for re-mint");
        assert!(
            !relay_holds_kv_value(relay.as_ref(), &cipher, cid, "post-restore-dedup").await,
            "the refused push did not silently land"
        );

        // Recovery: re-walk (watermark was reset to 0) re-mints under fresh seqs; the edit lands.
        enqueue_local(dev.conn(), &cipher, cid, sv, ev).unwrap();
        push_outbox(dev.conn(), cid, relay.as_ref()).await.unwrap();
        assert!(
            relay_holds_kv_value(relay.as_ref(), &cipher, cid, "post-restore-dedup").await,
            "after reseed + re-mint the edit reaches the relay (no silent loss)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restore_carrying_stale_counter_still_reaches_relay() {
        // F4(a) — the real file-restore model the persisted-flag design failed. A restore
        // rewrites the DB FILE from a backup that carries a STALE (lower) client_seq counter
        // with EVERYTHING ELSE INTACT — no meta deletion, no manual flag clearing — while the
        // relay tail is ahead. The old design persisted a "reconciled" flag IN the DB, so the
        // backup restored flag=1 alongside the stale counter and the drain SKIPPED reconcile
        // exactly when it was needed → the post-restore edit was silently lost. With the
        // connection-scoped marker, reopening the connection after the restore (a fresh spawn)
        // starts with the marker absent, so reconcile runs, reseeds past the tail, and the edit
        // lands. This test proves it via an ACTUAL file copy + connection reopen.
        let dir = std::env::temp_dir().join(format!("yapstack-restore-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("live.db");
        let backup = dir.join("backup.db");

        let relay = Arc::new(MockRelay::new());
        let cipher = f1_cipher();
        let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

        // A device with two synced edits; capture a BACKUP at this point (counter=2, relay 1..2,
        // meta + outbox all mutually consistent with the backup).
        let cid;
        {
            let dev = CrsqlDb::open(&live).unwrap();
            dev.conn()
                .execute_batch("PRAGMA journal_mode=WAL;")
                .unwrap();
            make_kv(&dev);
            cid = state::client_id(dev.conn()).unwrap();
            for (k, v) in [("k1", "v1"), ("k2", "v2")] {
                dev.conn()
                    .execute(
                        "INSERT INTO kv(id,v) VALUES(?1,?2)",
                        rusqlite::params![k, v],
                    )
                    .unwrap();
                drain_once(dev.conn(), &cipher, relay.as_ref(), cid, sv, ev)
                    .await
                    .unwrap();
            }
            assert_eq!(state::client_seq(dev.conn()).unwrap(), 2);
            assert!(state::seq_reconciled(dev.conn()).unwrap());
            // Checkpoint WAL into the main file so a plain file copy is a faithful backup.
            dev.conn()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
            std::fs::copy(&live, &backup).unwrap();

            // Move the relay tail ahead of the backup: a third edit synced AFTER the backup.
            dev.conn()
                .execute("INSERT INTO kv(id,v) VALUES('k3','v3')", [])
                .unwrap();
            drain_once(dev.conn(), &cipher, relay.as_ref(), cid, sv, ev)
                .await
                .unwrap();
            assert_eq!(state::client_seq(dev.conn()).unwrap(), 3);
        }
        let tail_before = relay.completeness().await.unwrap().count;
        assert_eq!(tail_before, 3, "relay holds our seqs 1..3");

        // RESTORE: overwrite the live DB with the backup file (counter=2, meta intact, NO flag
        // wiped) and REOPEN — the fresh connection a real restore-then-launch opens.
        std::fs::copy(&backup, &live).unwrap();
        let dev = CrsqlDb::open(&live).unwrap();
        dev.conn()
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
        assert_eq!(
            state::client_seq(dev.conn()).unwrap(),
            2,
            "restored DB carries the STALE counter"
        );
        assert!(
            !state::seq_reconciled(dev.conn()).unwrap(),
            "the connection-scoped marker cannot be restored from a file — fresh spawn re-checks"
        );

        // A post-restore edit — the write the persisted-flag design swallowed.
        dev.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k4','post-restore')", [])
            .unwrap();
        let report = drain_once(dev.conn(), &cipher, relay.as_ref(), cid, sv, ev)
            .await
            .unwrap();
        assert!(
            report.push_error.is_none(),
            "push clean after the per-spawn reseed: {:?}",
            report.push_error
        );
        assert!(
            state::client_seq(dev.conn()).unwrap() >= 3,
            "reconcile reseeded the counter past the relay tail"
        );
        assert!(
            relay.completeness().await.unwrap().count > tail_before,
            "the re-walk pushed fresh changesets under new seqs"
        );
        assert!(
            relay_holds_kv_value(relay.as_ref(), &cipher, cid, "post-restore").await,
            "the post-restore edit reached the relay (no silent loss)"
        );

        drop(dev);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn push_outbox_recovers_from_relay_409_conflict() {
        // F4(c) — the client-side G3 recovery, always-on (no live Postgres). A raw HTTP server
        // returns 409 Conflict for the push (the relay rejecting a colliding pair carrying
        // different content) and then serves a completeness JSON naming our tail. push_outbox
        // must classify the 409 as a conflict, reseed past the tail, drop the colliding entry,
        // and return NON-FATALLY (Ok) — never wedge in a permanent Failing loop.
        use crate::transport::HttpTransport;
        use std::io::{Read, Write};

        let cid = Uuid::from_u128(0xC0FFEE);
        // Seed a single unacked outbox entry (opaque bytes) whose client_seq (1) the relay will
        // report as already held at the tail — the collision.
        let conn = seed_outbox(&[vec![7u8; 32]]);

        let completeness_json = format!(
            "{{\"max_changeset_seq\":1,\"count\":1,\"contiguous\":true,\
             \"per_client\":[{{\"client_id\":\"{cid}\",\"max_client_seq\":1}}]}}"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            // Response 1: POST /sync/push → 409. Response 2: GET /sync/completeness → JSON.
            let bodies = [
                ("409 Conflict".to_string(), String::new()),
                ("200 OK".to_string(), completeness_json),
            ];
            for (status_line, body) in bodies {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let n = sock.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
        });

        let transport = HttpTransport::new(format!("http://{addr}"), "access-token");
        // The local counter starts at 0 (seed_outbox does not set it), so the reseed target is
        // the relay tail (1). The call must NOT error (non-fatal recovery).
        let (acked, _) = push_outbox(&conn, cid, &transport).await.unwrap();
        assert_eq!(acked, 0, "the colliding batch was not acked");
        assert!(
            state::client_seq(&conn).unwrap() >= 1,
            "counter reseeded to at least the relay tail"
        );
        let unacked: i64 = conn
            .query_row(
                "SELECT count(*) FROM _yapstack_outbox WHERE acked=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unacked, 0, "the colliding entry was dropped for re-mint");
        assert!(
            state::seq_reconciled(&conn).unwrap(),
            "the conflict recovery marks the counter reconciled this spawn"
        );
        server.join().unwrap();
    }
}
