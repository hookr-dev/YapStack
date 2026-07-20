// SPDX-License-Identifier: AGPL-3.0-only
//! Durable audio upload queue + background uploader (audio round-trip S1, D4/D9).
//!
//! Audio is a **best-effort background lane, independent of the correctness-critical
//! changeset outbox**: recording is NEVER blocked by an upload. On session finalize /
//! dictation save the desktop enqueues `(part_id, source_file, session_id)` at NORMAL
//! priority; the re-runnable backfill walk (D9) enqueues existing local parts at LOW
//! priority. The uploader drains ONE blob at a time (so a 599 MB upload never starves the
//! UI or the changeset drain), NORMAL before LOW, sealing each source file to an encrypted
//! temp file (O(chunk) memory), content-addressing it, and pushing it through the
//! existence-checked presign (D8).
//!
//! ## Server completeness invariant (owner)
//! Every device treats audio that exists locally but not on the server as outstanding
//! upload debt. The enforcing mechanisms are (a) finalize-enqueue and (b) the re-runnable
//! idempotent backfill walk — both applied on EVERY device that ever recorded locally.
//!
//! ## Durability & state machine
//! `_yapstack_audio_upload_queue` is a LOCAL, non-CRR'd table (underscore-prefixed like the
//! outbox), so it survives restart and never syncs. Each entry moves
//! `pending → sealing → uploading → done`, or `→ failed` (with `attempts` + `last_error`).
//! Failures are surfaced through [`lane_status`] under a DISTINCT lane label — never silent
//! (repo posture; SYNC_REMEDIATION F2). Retryable on app start ([`reset_in_flight`] +
//! [`retry_failed`]) and via a manual-retry seam. Enqueue is INSERT-OR-IGNORE by `part_id`,
//! so both finalize-enqueue and the backfill walk are idempotent.
//!
//! Deleted-part entries are dropped SILENTLY (not surfaced as errors), per D9.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::transport::SyncTransport;
use crate::SyncError;
use yapstack_crypto::audio_stream::{open_blob, seal_blob, AudioIdentity};

/// Filename prefix for the encrypted download temp files the fetch path writes (S3). A
/// distinct prefix (separate from the upload [`SEAL_TEMP_PREFIX`]) lets a startup sweep
/// reclaim a partial download a crash left behind WITHOUT ever touching a file it did not
/// create.
pub const FETCH_TEMP_PREFIX: &str = "yapstack-audio-fetch-";

/// Filename prefix for the encrypted seal temp files the uploader writes (advisory A3).
/// A distinct, unambiguous prefix makes an orphaned temp (left by a crash mid-seal)
/// identifiable so [`sweep_orphan_temps`] can reclaim it WITHOUT ever touching a file it
/// did not create.
pub const SEAL_TEMP_PREFIX: &str = "yapstack-audio-seal-";

/// NORMAL lane — new recordings; drained before the backfill lane.
pub const PRIORITY_NORMAL: i64 = 0;
/// LOW lane — the D9 backfill of the existing local library; drains only when NORMAL empty.
pub const PRIORITY_BACKFILL: i64 = 1;

const WALK_DONE_KEY: &str = "audio_backfill_walk_completed_at";

/// The upload lane's entry lifecycle (durable in `state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadState {
    Pending,
    Sealing,
    Uploading,
    Done,
    Failed,
}

impl UploadState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            UploadState::Pending => "pending",
            UploadState::Sealing => "sealing",
            UploadState::Uploading => "uploading",
            UploadState::Done => "done",
            UploadState::Failed => "failed",
        }
    }
}

/// The crypto context the uploader needs to seal a blob: the account vault key, the tenant
/// id (16-byte UUID), and the current vault-key rotation epoch. Supplied by the desktop
/// (vault key from the OS keychain) when it starts the lane; the engine never persists it.
#[derive(Clone)]
pub struct AudioSealContext {
    pub vault_key: [u8; 32],
    pub tenant_id: [u8; 16],
    pub epoch: u32,
}

/// A queued entry the uploader is about to process.
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub part_id: String,
    pub source_path: String,
    pub session_id: String,
    pub priority: i64,
    pub attempts: i64,
}

/// Point-in-time lane counts for the drain-health/status surface. Carries a DISTINCT lane
/// label so the desktop shows audio-upload health separately from changeset sync (never
/// silent). `failed` is the load-bearing "needs attention" number.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AudioLaneStatus {
    pub pending: u64,
    pub sealing: u64,
    pub uploading: u64,
    pub done: u64,
    pub failed: u64,
}

impl AudioLaneStatus {
    /// The distinct lane label the status surface renders (never merged with changeset sync).
    #[must_use]
    pub const fn lane_label(&self) -> &'static str {
        "audio-upload"
    }
    /// Outstanding work (not yet `done` and not `failed`).
    #[must_use]
    pub const fn outstanding(&self) -> u64 {
        self.pending + self.sealing + self.uploading
    }
}

/// One step of the uploader (it processes exactly ONE blob — the one-in-flight throttle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainStep {
    /// Nothing ready to upload.
    Idle,
    /// A blob reached the relay (or was already there via existence-checked dedup).
    Uploaded {
        part_id: String,
        already_exists: bool,
    },
    /// The entry's part row was deleted before upload — dropped silently (D9).
    DroppedDeleted { part_id: String },
    /// The upload failed; the entry is marked `failed` and surfaced via [`lane_status`].
    Failed { part_id: String, error: String },
}

/// Create the durable local queue table (idempotent).
///
/// # Errors
/// Propagates any sqlite error.
pub fn ensure_queue_table(conn: &Connection) -> Result<(), SyncError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _yapstack_audio_upload_queue (\
            part_id     TEXT PRIMARY KEY, \
            source_path TEXT NOT NULL, \
            session_id  TEXT NOT NULL, \
            priority    INTEGER NOT NULL DEFAULT 0, \
            state       TEXT NOT NULL DEFAULT 'pending', \
            attempts    INTEGER NOT NULL DEFAULT 0, \
            last_error  TEXT, \
            sha256      TEXT, \
            size_bytes  INTEGER, \
            created_at  TEXT NOT NULL DEFAULT (datetime('now')), \
            updated_at  TEXT NOT NULL DEFAULT (datetime('now')));",
    )?;
    Ok(())
}

/// Enqueue a part for upload. INSERT-OR-IGNORE by `part_id` — idempotent, so finalize and
/// the backfill walk can both target the same part without duplicating work. Returns `true`
/// iff a new row was inserted.
///
/// # Errors
/// Propagates any sqlite error.
pub fn enqueue(
    conn: &Connection,
    part_id: &str,
    source_path: &str,
    session_id: &str,
    priority: i64,
) -> Result<bool, SyncError> {
    ensure_queue_table(conn)?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO _yapstack_audio_upload_queue \
         (part_id, source_path, session_id, priority) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![part_id, source_path, session_id, priority],
    )?;
    Ok(n > 0)
}

/// Enqueue on session finalize / dictation save (NORMAL priority).
///
/// # Errors
/// Propagates any sqlite error.
pub fn enqueue_on_save(
    conn: &Connection,
    part_id: &str,
    source_path: &str,
    session_id: &str,
) -> Result<bool, SyncError> {
    enqueue(conn, part_id, source_path, session_id, PRIORITY_NORMAL)
}

/// Report of a backfill walk pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillReport {
    /// Local parts examined.
    pub examined: u64,
    /// Newly enqueued (not already queued/done).
    pub enqueued: u64,
    /// Parts skipped because their file is missing on this device.
    pub missing_file: u64,
}

/// Re-runnable idempotent backfill walk (D9). Reads `session_audio_parts` and, for each row
/// whose file exists on THIS device (per `file_exists`), enqueues it at LOW priority
/// (INSERT-OR-IGNORE by `part_id`). Records walk completion. Safe to re-run: a restart
/// mid-walk simply re-examines rows and skips already-queued ones. The completeness
/// invariant means this runs on EVERY device, not just the historical back-catalog device.
///
/// `file_exists` is injected so the walk stays engine-testable without a real filesystem
/// (the desktop passes a real `Path::exists` check against this device's audio dir).
///
/// # Errors
/// Propagates any sqlite error.
pub fn backfill_walk<F>(conn: &Connection, mut file_exists: F) -> Result<BackfillReport, SyncError>
where
    F: FnMut(&str) -> bool,
{
    ensure_queue_table(conn)?;
    crate::state::ensure_meta_table(conn)?;

    let mut report = BackfillReport::default();
    let mut stmt = conn.prepare(
        "SELECT id, session_id, file_path FROM session_audio_parts \
         WHERE file_path IS NOT NULL AND file_path <> ''",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    // Collect first so the SELECT cursor is closed before we enqueue (INSERTs into a
    // different table, but collecting keeps the borrow window tight and obvious).
    let mut parts: Vec<(String, String, String)> = Vec::new();
    for row in rows {
        parts.push(row?);
    }
    drop(stmt);
    for (part_id, session_id, file_path) in parts {
        report.examined += 1;
        if !file_exists(&file_path) {
            report.missing_file += 1;
            continue;
        }
        if enqueue(conn, &part_id, &file_path, &session_id, PRIORITY_BACKFILL)? {
            report.enqueued += 1;
        }
    }

    // S3 dictation fold-in: dictation audio has NO session_audio_parts row (the synthetic
    // dictation session_id has no `sessions` row, so the finalize path skips part
    // persistence). Its part identity is the dictation_history row's own UUID, with the AAD
    // `session_id` self-referential (the same value used at enqueue-on-save). Walk those rows
    // too so every dictation WAV that exists on THIS device becomes upload debt — the
    // server-completeness invariant covers dictation, not only session recordings.
    if table_exists(conn, "dictation_history") {
        let mut dstmt = conn.prepare(
            "SELECT id, wav_file_path FROM dictation_history \
             WHERE wav_file_path IS NOT NULL AND wav_file_path <> ''",
        )?;
        let drows =
            dstmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut dparts: Vec<(String, String)> = Vec::new();
        for row in drows {
            dparts.push(row?);
        }
        drop(dstmt);
        for (dict_id, file_path) in dparts {
            report.examined += 1;
            if !file_exists(&file_path) {
                report.missing_file += 1;
                continue;
            }
            // session_id == part_id (self): dictation has no owning session; the identity is
            // immutable so upload and fetch derive the SAME AAD regardless of later linking.
            if enqueue(conn, &dict_id, &file_path, &dict_id, PRIORITY_BACKFILL)? {
                report.enqueued += 1;
            }
        }
    }

    // Record completion (a timestamp; presence is the "walk ran" signal — re-runnable).
    crate::state::set_meta(conn, WALK_DONE_KEY, &chrono_now())?;
    Ok(report)
}

/// Whether a backfill walk has ever completed on this device.
///
/// # Errors
/// Propagates any sqlite error.
pub fn backfill_walk_completed(conn: &Connection) -> Result<bool, SyncError> {
    crate::state::ensure_meta_table(conn)?;
    Ok(crate::state::get_meta(conn, WALK_DONE_KEY)?.is_some())
}

/// Lane counts for the status surface.
///
/// # Errors
/// Propagates any sqlite error.
pub fn lane_status(conn: &Connection) -> Result<AudioLaneStatus, SyncError> {
    ensure_queue_table(conn)?;
    let mut s = AudioLaneStatus::default();
    let mut stmt =
        conn.prepare("SELECT state, count(*) FROM _yapstack_audio_upload_queue GROUP BY state")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (state, n) = row?;
        let n = n.max(0) as u64;
        match state.as_str() {
            "pending" => s.pending = n,
            "sealing" => s.sealing = n,
            "uploading" => s.uploading = n,
            "done" => s.done = n,
            "failed" => s.failed = n,
            _ => {}
        }
    }
    Ok(s)
}

/// On app start, any entry left `sealing`/`uploading` by a crash is reset to `pending` so a
/// re-run resumes it (uploads are idempotent via dedup + D8).
///
/// # Errors
/// Propagates any sqlite error.
pub fn reset_in_flight(conn: &Connection) -> Result<usize, SyncError> {
    ensure_queue_table(conn)?;
    let n = conn.execute(
        "UPDATE _yapstack_audio_upload_queue SET state='pending', updated_at=datetime('now') \
         WHERE state IN ('sealing','uploading')",
        [],
    )?;
    Ok(n)
}

/// Reset `failed` entries to `pending` (app-start retry + the manual-retry seam).
///
/// # Errors
/// Propagates any sqlite error.
pub fn retry_failed(conn: &Connection) -> Result<usize, SyncError> {
    ensure_queue_table(conn)?;
    let n = conn.execute(
        "UPDATE _yapstack_audio_upload_queue SET state='pending', last_error=NULL, \
         updated_at=datetime('now') WHERE state='failed'",
        [],
    )?;
    Ok(n)
}

/// Bounded startup sweep for orphaned seal temp files (advisory A3). A crash between
/// creating the encrypted temp and finishing the upload can leave a
/// [`SEAL_TEMP_PREFIX`]-named file in `temp_dir`; the drain is idempotent (it re-seals on
/// retry) so those temps are pure garbage. Removes ONLY files whose name starts with
/// [`SEAL_TEMP_PREFIX`] — every non-matching entry (and any directory) is left untouched —
/// and returns the count reclaimed. A missing `temp_dir` is not an error (nothing to
/// sweep). Best-effort per file: an individual unlink error is logged by the caller via the
/// returned error only if it is the sweep-opening `read_dir` that fails; per-file failures
/// are skipped so one stuck file never blocks uploader start.
///
/// # Errors
/// Propagates only a failure to OPEN `temp_dir` for reading (other than not-found).
pub fn sweep_orphan_temps(temp_dir: &Path) -> Result<u64, SyncError> {
    let entries = match std::fs::read_dir(temp_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(SyncError::Transport(format!("audio temp sweep: {e}"))),
    };
    let mut removed = 0u64;
    for entry in entries.flatten() {
        // Only ever touch a regular file whose name carries OUR prefix.
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(SEAL_TEMP_PREFIX) {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_file() => {}
            _ => continue, // never recurse into / remove a directory
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// The next entry to process: NORMAL before LOW, oldest first, only `pending`. `done`,
/// `failed`, and in-flight entries are skipped (failed is re-armed via [`retry_failed`]).
fn next_ready(conn: &Connection) -> Result<Option<QueueEntry>, SyncError> {
    ensure_queue_table(conn)?;
    let row = conn
        .query_row(
            "SELECT part_id, source_path, session_id, priority, attempts \
             FROM _yapstack_audio_upload_queue WHERE state='pending' \
             ORDER BY priority ASC, created_at ASC, part_id ASC LIMIT 1",
            [],
            |r| {
                Ok(QueueEntry {
                    part_id: r.get(0)?,
                    source_path: r.get(1)?,
                    session_id: r.get(2)?,
                    priority: r.get(3)?,
                    attempts: r.get(4)?,
                })
            },
        )
        .ok();
    Ok(row)
}

fn set_state(conn: &Connection, part_id: &str, state: UploadState) -> Result<(), SyncError> {
    conn.execute(
        "UPDATE _yapstack_audio_upload_queue SET state=?2, updated_at=datetime('now') \
         WHERE part_id=?1",
        rusqlite::params![part_id, state.as_str()],
    )?;
    Ok(())
}

fn set_sealed(conn: &Connection, part_id: &str, sha256: &str, size: u64) -> Result<(), SyncError> {
    conn.execute(
        "UPDATE _yapstack_audio_upload_queue \
         SET state='uploading', sha256=?2, size_bytes=?3, updated_at=datetime('now') \
         WHERE part_id=?1",
        rusqlite::params![part_id, sha256, size as i64],
    )?;
    Ok(())
}

fn mark_failed(conn: &Connection, part_id: &str, err: &str) -> Result<(), SyncError> {
    conn.execute(
        "UPDATE _yapstack_audio_upload_queue \
         SET state='failed', attempts=attempts+1, last_error=?2, updated_at=datetime('now') \
         WHERE part_id=?1",
        rusqlite::params![part_id, err],
    )?;
    Ok(())
}

fn delete_entry(conn: &Connection, part_id: &str) -> Result<(), SyncError> {
    conn.execute(
        "DELETE FROM _yapstack_audio_upload_queue WHERE part_id=?1",
        [part_id],
    )?;
    Ok(())
}

/// Whether a table exists (used to make the dictation fold-in tolerant of older schemas /
/// engine test DBs that predate `dictation_history`).
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Whether the audio part's SOURCE row still exists (a deleted source → drop the queue entry
/// silently, D9). An audio part is identified by its own UUID, which is EITHER a
/// `session_audio_parts.id` (session recordings) OR a `dictation_history.id` (dictation
/// audio — S3 dictation data-model fold-in: dictation WAVs have no `session_audio_parts`
/// row, so their `dictation_history.id` IS the part identity, self-referential as the AAD
/// `session_id`). Checking both tables keeps the drain, the fetch path, and the deleted-part
/// drop consistent across the two audio sources.
fn part_source_exists(conn: &Connection, part_id: &str) -> Result<bool, SyncError> {
    let n: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_audio_parts WHERE id=?1)",
        [part_id],
        |r| r.get(0),
    )?;
    if n > 0 {
        return Ok(true);
    }
    if table_exists(conn, "dictation_history") {
        let n: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM dictation_history WHERE id=?1)",
            [part_id],
            |r| r.get(0),
        )?;
        return Ok(n > 0);
    }
    Ok(false)
}

/// Process ONE queued blob (the one-in-flight throttle): pick the next entry, seal its
/// source to an encrypted temp file (content-addressed in the same pass), presign
/// (existence-checked, D8), and stream-upload it if the relay doesn't already have it. A
/// deleted part is dropped silently; any failure marks the entry `failed` (surfaced via
/// [`lane_status`]) and returns [`DrainStep::Failed`] WITHOUT propagating — the lane is
/// best-effort and must never wedge.
///
/// The caller loops until [`DrainStep::Idle`] to drain the lane, one blob at a time. Runs
/// on a current-thread runtime (the `&Connection` is held across `.await`, same pattern as
/// the changeset outbox drain).
///
/// # Errors
/// Only sqlite/queue-bookkeeping errors propagate; upload failures are captured as
/// `DrainStep::Failed`.
pub async fn drain_one<T: SyncTransport + ?Sized>(
    conn: &Connection,
    transport: &T,
    ctx: &AudioSealContext,
    temp_dir: &Path,
) -> Result<DrainStep, SyncError> {
    let Some(entry) = next_ready(conn)? else {
        return Ok(DrainStep::Idle);
    };

    // Deleted-part entries are dropped SILENTLY (D9). "Part row" here is the source row in
    // EITHER session_audio_parts or dictation_history (S3 dictation fold-in).
    if !part_source_exists(conn, &entry.part_id)? {
        delete_entry(conn, &entry.part_id)?;
        return Ok(DrainStep::DroppedDeleted {
            part_id: entry.part_id,
        });
    }

    match upload_entry(conn, transport, ctx, temp_dir, &entry).await {
        Ok(already_exists) => {
            set_state(conn, &entry.part_id, UploadState::Done)?;
            Ok(DrainStep::Uploaded {
                part_id: entry.part_id,
                already_exists,
            })
        }
        Err(e) => {
            let msg = e.to_string();
            mark_failed(conn, &entry.part_id, &msg)?;
            Ok(DrainStep::Failed {
                part_id: entry.part_id,
                error: msg,
            })
        }
    }
}

/// The fallible core of a single upload. Returns whether the relay already had the object
/// (existence-checked dedup, no bytes moved).
async fn upload_entry<T: SyncTransport + ?Sized>(
    conn: &Connection,
    transport: &T,
    ctx: &AudioSealContext,
    temp_dir: &Path,
    entry: &QueueEntry,
) -> Result<bool, SyncError> {
    set_state(conn, &entry.part_id, UploadState::Sealing)?;

    let part_bytes = uuid_bytes(&entry.part_id)
        .ok_or_else(|| SyncError::Codec(format!("part_id not a UUID: {}", entry.part_id)))?;
    let session_bytes = uuid_bytes(&entry.session_id)
        .ok_or_else(|| SyncError::Codec(format!("session_id not a UUID: {}", entry.session_id)))?;
    let id = AudioIdentity {
        tenant_id: ctx.tenant_id,
        session_id: session_bytes,
        part_id: part_bytes,
        epoch: ctx.epoch,
    };

    // Seal source file → encrypted temp file, computing sha256 + size in the SAME pass.
    std::fs::create_dir_all(temp_dir)
        .map_err(|e| SyncError::Transport(format!("audio temp dir: {e}")))?;
    let temp = tempfile::Builder::new()
        .prefix(SEAL_TEMP_PREFIX)
        .tempfile_in(temp_dir)
        .map_err(|e| SyncError::Transport(format!("audio temp file: {e}")))?;
    let src = std::fs::File::open(&entry.source_path)
        .map_err(|e| SyncError::Transport(format!("open audio source: {e}")))?;

    let (sha_hex, size) = {
        let file = std::fs::File::create(temp.path())
            .map_err(|e| SyncError::Transport(format!("audio temp create: {e}")))?;
        let mut hw = HashingWriter::new(std::io::BufWriter::new(file));
        seal_blob(&ctx.vault_key, &id, src, &mut hw)?;
        hw.finish()?
    };
    set_sealed(conn, &entry.part_id, &sha_hex, size)?;

    // Existence-checked presign (D8): `already_exists` → done, no bytes moved.
    let resp = transport
        .presign_audio(&sha_hex, size, &entry.part_id, &entry.session_id)
        .await?;
    if resp.already_exists {
        return Ok(true);
    }
    let url = resp
        .upload_url
        .ok_or_else(|| SyncError::Transport("audio presign returned no upload_url".into()))?;
    transport.put_audio(&url, temp.path(), size).await?;
    Ok(false)
}

/// A `Write` that tees into an inner writer while streaming a SHA-256 and byte count, so a
/// blob is content-addressed in the same pass it is sealed (no second read).
struct HashingWriter<W: std::io::Write> {
    inner: W,
    hasher: sha2::Sha256,
    len: u64,
}

impl<W: std::io::Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        use sha2::Digest;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
            len: 0,
        }
    }
    fn finish(mut self) -> Result<(String, u64), SyncError> {
        use sha2::Digest;
        self.inner
            .flush()
            .map_err(|e| SyncError::Transport(format!("audio seal flush: {e}")))?;
        Ok((hex::encode(self.hasher.finalize()), self.len))
    }
}

impl<W: std::io::Write> std::io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Parse a UUID string (hyphenated or simple 32-hex) into its 16 raw bytes for the crypto
/// identity AAD. Returns `None` if it is not 16 bytes of hex.
fn uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let hex_only: String = s.chars().filter(|c| *c != '-').collect();
    if hex_only.len() != 32 {
        return None;
    }
    let raw = hex::decode(hex_only).ok()?;
    let mut out = [0u8; 16];
    out.copy_from_slice(&raw);
    Some(out)
}

fn chrono_now() -> String {
    // A stable timestamp string; the value is informational (presence = walk ran).
    // Avoids a chrono dep in this crate by using SystemTime.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.to_string()
}

// ===================== Fetch-on-demand playback (S3, D2/D3) =====================
//
// When a synced session/dictation is opened on a peer, the audio bytes are not local
// (`file_path` is the SOURCE device's absolute path). The player resolves a part in D2
// order — (1) local fetch cache, (2) same-device legacy `file_path`, (3) fetch — and this
// module owns step 3: stream the encrypted blob down (O(frame) memory), streaming-decrypt it
// (`open_blob`, O(chunk) memory) into the keep-until-clear cache, and never hold the whole
// blob in memory. One in-flight fetch per part is enforced by [`FetchRegistry`] (single-flight
// coalescing); concurrent player views subscribe to the SAME [`FetchProgress`].

/// Live, pollable progress + cancel signal for a single in-flight fetch. Shared (behind an
/// `Arc` in the [`FetchRegistry`] slot) so every view watching the same part reads one set of
/// counters and a single cancel toggles the one download. All fields are lock-free atomics.
#[derive(Debug, Default)]
pub struct FetchProgress {
    /// Encrypted bytes downloaded so far.
    pub received: AtomicU64,
    /// Server-declared content-length (0 until known / unknown).
    pub total: AtomicU64,
    /// Cooperative cancel: the transport aborts the stream at the next frame boundary.
    pub cancel: AtomicBool,
}

impl FetchProgress {
    /// `(received, total)` for the "Fetching N%" bar; `total == 0` means not yet known.
    #[must_use]
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.received.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
    /// Request cancellation of the in-flight download (idempotent).
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    /// Whether a cancel has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Terminal classification of a fetch attempt. Transport-unreachable and decrypt/verification
/// failures ride as [`SyncError`] (`is_network()` / `SyncError::Crypto`) so the desktop can map
/// them to distinct honest player states; the outcomes here are the non-error terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchResult {
    /// The decrypted audio is now in the cache (or already was) — playable.
    Fetched,
    /// The relay has no blob for this part yet (row converged before its audio uploaded, or
    /// the source device never uploaded it). Stays the honest "audio is on <device>" state.
    NotOnServer,
    /// The caller cancelled the download mid-flight; the partial file was discarded.
    Cancelled,
    /// The cache volume positively reported insufficient space for this blob (disk
    /// precheck). `needed` is the TOTAL the operation requires from a clean disk —
    /// `content_length * 2 + `[`NO_SPACE_HEADROOM_BYTES`] (encrypted temp + decrypted
    /// output + headroom) — so the player can render "Not enough disk space (need ~X)".
    /// An honest terminal, never fabricated: it fires only when the injected
    /// free-space probe RETURNS a number that is too small (probe `None` = fail-open).
    NoSpace { needed: u64 },
}

/// Headroom the disk precheck demands on top of the blob's own footprint, so a fetch never
/// fills the volume to the brim (256 MiB).
pub const NO_SPACE_HEADROOM_BYTES: u64 = 256 * 1024 * 1024;

/// Injected free-space probe: available bytes on the volume containing the given path, or
/// `None` when the platform can't say (the check FAILS OPEN — a fetch is never blocked on
/// an indeterminate probe). The desktop supplies the real `statvfs`/`GetDiskFreeSpaceExW`
/// implementation; the engine stays platform-free and unit-testable.
pub type FreeSpaceFn = dyn Fn(&Path) -> Option<u64> + Send + Sync;

/// Resolve a part in D2 order for the fetch layer: a present cache file is an immediate hit
/// (step 1) and short-circuits any network work. `cache_path` is this device's cache entry
/// for the part (keyed by the caller — see the desktop's cache-key choice). Pure filesystem
/// check so the resolution order is unit-testable.
#[must_use]
pub fn cache_hit(cache_path: &Path) -> bool {
    cache_path.is_file()
}

/// The disk-precheck core: probe the cache volume and compare against the blob budget.
/// `blob_len` is the (encrypted ≈ decrypted) blob size; `already_on_disk` is how much of
/// that budget is ALREADY consumed (the encrypted temp, when checking post-download).
/// Returns `Some(needed_total)` — the full clean-disk budget, for the "need ~X" copy —
/// only when the probe POSITIVELY reports insufficient space; `None` = enough space, no
/// probe supplied, or the probe couldn't determine (fail-open, never fabricate).
fn space_shortfall(
    free_space: Option<&FreeSpaceFn>,
    cache_dir: &Path,
    blob_len: u64,
    already_on_disk: u64,
) -> Option<u64> {
    let free = free_space?(cache_dir)?;
    let needed_total = blob_len
        .saturating_mul(2)
        .saturating_add(NO_SPACE_HEADROOM_BYTES);
    let still_needed = needed_total.saturating_sub(already_on_disk);
    (free < still_needed).then_some(needed_total)
}

/// Fetch one blob to the local cache (fetch-path step 3). Streams the encrypted blob to a
/// temp file (progress-reported, cancellable), then streaming-decrypts it (`open_blob`,
/// verifying the identity wrap AAD for THIS `part_id`/`session_id`/`epoch`) into a temp in the
/// cache dir, and atomically promotes it to `cache_path`. Never holds the whole blob in
/// memory. Idempotent under coalescing: a pre-existing cache file returns [`FetchResult::Fetched`]
/// without touching the network (D2 step 1).
///
/// ## Disk precheck (S4-pulled-forward)
/// When a `free_space` probe is supplied, the fetch checks the cache volume as soon as the
/// server-declared content-length is known (a lightweight watcher beside the streaming
/// download — the transport seam is untouched) and once more just before decrypt (the
/// backstop for unknown content-length or space that shrank mid-download). Budget:
/// `content_length * 2 + `[`NO_SPACE_HEADROOM_BYTES`] (encrypted temp + decrypted output +
/// headroom). A positively-insufficient volume aborts the download and returns the honest
/// [`FetchResult::NoSpace`] terminal; a probe that returns `None` fails OPEN (the fetch
/// proceeds — never fabricate a NoSpace).
///
/// # Errors
/// - [`SyncError::Network`] — relay unreachable (desktop → "can't reach sync server").
/// - [`SyncError::Crypto`] — decrypt/verification failure (desktop → "audio failed
///   verification"; a tamper signal, logged at warn per the spec posture).
/// - other [`SyncError`] — transport/IO faults, surfaced verbatim.
#[allow(clippy::too_many_arguments)] // the fetch context is irreducible: transport+key+identity+paths+progress+probe
pub async fn fetch_blob_to_cache<T: SyncTransport + ?Sized>(
    transport: &T,
    vault_key: &[u8; 32],
    id: &AudioIdentity,
    part_id: &str,
    temp_dir: &Path,
    cache_path: &Path,
    progress: &FetchProgress,
    free_space: Option<&FreeSpaceFn>,
) -> Result<FetchResult, SyncError> {
    // D2 step 1: a present cache file wins outright — no network, no re-decrypt.
    if cache_hit(cache_path) {
        return Ok(FetchResult::Fetched);
    }
    if progress.is_cancelled() {
        return Ok(FetchResult::Cancelled);
    }
    std::fs::create_dir_all(temp_dir)
        .map_err(|e| SyncError::Transport(format!("audio fetch temp dir: {e}")))?;
    let cache_dir = cache_path
        .parent()
        .ok_or_else(|| SyncError::Transport("audio cache path has no parent".into()))?;
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| SyncError::Transport(format!("audio cache dir: {e}")))?;

    // 1. Stream the ENCRYPTED blob to a temp file (O(frame) memory, progress-reported).
    //    A lightweight watcher runs beside the download: the moment the transport declares
    //    the content-length (`progress.total`), the disk precheck runs ONCE; on a
    //    positively-insufficient volume it aborts the stream via the cooperative cancel
    //    (classified below as NoSpace, not a user cancel) instead of downloading hundreds
    //    of MB the disk can't hold. The temp dir and cache dir are siblings beside the DB,
    //    so one volume probe covers both.
    let enc_tmp = tempfile::Builder::new()
        .prefix(FETCH_TEMP_PREFIX)
        .tempfile_in(temp_dir)
        .map_err(|e| SyncError::Transport(format!("audio fetch temp file: {e}")))?;
    let mut no_space_needed: Option<u64> = None;
    let download = transport.get_audio_streaming(part_id, enc_tmp.path(), progress);
    tokio::pin!(download);
    let downloaded = loop {
        if free_space.is_none() || no_space_needed.is_some() {
            break download.await;
        }
        tokio::select! {
            res = &mut download => break res,
            () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let (_, total) = progress.snapshot();
                if total > 0 {
                    if let Some(needed) = space_shortfall(free_space, cache_dir, total, 0) {
                        no_space_needed = Some(needed);
                        progress.request_cancel();
                    } else {
                        // Checked once with a known total and it fit — stop polling.
                        break download.await;
                    }
                }
            }
        }
    };
    let available = match downloaded {
        Ok(a) => a,
        Err(SyncError::Transport(m)) if m == crate::transport::FETCH_CANCELLED => {
            return Ok(match no_space_needed {
                // The WATCHER aborted the stream: an honest NoSpace, not a user cancel.
                Some(needed) => FetchResult::NoSpace { needed },
                None => FetchResult::Cancelled,
            });
        }
        Err(e) => return Err(e),
    };
    if !available {
        return Ok(FetchResult::NotOnServer);
    }

    // Pre-decrypt backstop: the encrypted temp is on disk (its size = the true blob
    // length), so demand room for the decrypted output + headroom. Catches transports
    // that never declared a content-length and space that shrank during the download.
    let enc_len = std::fs::metadata(enc_tmp.path())
        .map(|m| m.len())
        .unwrap_or_else(|_| progress.snapshot().1);
    if let Some(needed) = space_shortfall(free_space, cache_dir, enc_len, enc_len) {
        return Ok(FetchResult::NoSpace { needed });
    }

    // 2. Streaming-decrypt (identity-verified) → a temp in the cache dir → atomic rename. A
    //    decrypt/verification failure leaves NO cache file (the temp is dropped), so a tamper
    //    never poisons the cache and a retry re-downloads cleanly.
    let dec_tmp = tempfile::Builder::new()
        .prefix(FETCH_TEMP_PREFIX)
        .tempfile_in(cache_dir)
        .map_err(|e| SyncError::Transport(format!("audio cache temp: {e}")))?;
    {
        let src = std::fs::File::open(enc_tmp.path())
            .map_err(|e| SyncError::Transport(format!("open fetched blob: {e}")))?;
        let mut w = std::io::BufWriter::new(
            std::fs::File::create(dec_tmp.path())
                .map_err(|e| SyncError::Transport(format!("create cache temp: {e}")))?,
        );
        // CryptoError → SyncError::Crypto (verification/tamper); IO wrapped by open_blob.
        open_blob(vault_key, id, src, &mut w)?;
        use std::io::Write;
        w.flush()
            .map_err(|e| SyncError::Transport(format!("flush cache temp: {e}")))?;
    }
    dec_tmp
        .persist(cache_path)
        .map_err(|e| SyncError::Transport(format!("promote cache file: {e}")))?;
    Ok(FetchResult::Fetched)
}

/// A single-flight fetch slot: the shared [`FetchProgress`] every subscriber polls, plus the
/// terminal outcome once the one download finishes. `Ok(FetchResult)` on a clean terminal;
/// `Err(code)` for the error terminals, where `code` is a stable classifier
/// (`"unreachable"` / `"verification"` / `"error:<detail>"`) the desktop maps to a player
/// state — kept a `String` so this engine type needs no desktop-specific error enum.
#[derive(Debug, Default)]
pub struct FetchSlot {
    pub progress: FetchProgress,
    outcome: Mutex<Option<Result<FetchResult, String>>>,
    /// True while the part sits in the registry's FIFO admission queue (cap reached) —
    /// the observable "waiting" state the player renders as Queued.
    queued: AtomicBool,
}

impl FetchSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    /// Record the terminal outcome (called once by the fetch worker).
    pub fn finish(&self, outcome: Result<FetchResult, String>) {
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
    }
    /// The terminal outcome, or `None` while the fetch is still in flight.
    #[must_use]
    pub fn outcome(&self) -> Option<Result<FetchResult, String>> {
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// Whether the part is admission-queued (submitted but not yet started — cap reached).
    #[must_use]
    pub fn is_queued(&self) -> bool {
        self.queued.load(Ordering::Relaxed)
    }
    fn set_queued(&self, q: bool) {
        self.queued.store(q, Ordering::Relaxed);
    }
}

/// The deferred fetch-start a caller hands to [`FetchRegistry::submit`]. In production it
/// spawns the download worker thread; the registry runs it immediately (a permit was free)
/// or from the FIFO queue when a running fetch finishes. Kept a boxed closure so the
/// registry stays a pure coordination type — execution (thread/runtime/transport) is the
/// caller's.
pub type FetchStarter = Box<dyn FnOnce() + Send + 'static>;

/// Default global fetch concurrency: at most this many downloads run at once; the rest
/// wait in FIFO order (submission order = the player's part_index order).
pub const DEFAULT_FETCH_CONCURRENCY: usize = 2;

/// Single-flight registry + global admission control: at most ONE slot per `part_id`
/// (concurrent player views coalesce onto the same download and share its
/// [`FetchProgress`] — the S3 coalescing requirement), and at most `cap` downloads
/// RUNNING process-wide; further submissions queue FIFO in submission order and start as
/// running fetches call [`task_finished`](Self::task_finished). The desktop owns one
/// process-wide instance; the fetch execution (thread + runtime + transport) is the
/// caller-supplied [`FetchStarter`], keeping this a pure coordination type.
pub struct FetchRegistry {
    cap: usize,
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    slots: HashMap<String, Arc<FetchSlot>>,
    /// FIFO of parts admitted but not yet started (cap reached), with their starters.
    queue: VecDeque<(String, FetchStarter)>,
    /// Started-and-not-yet-finished count (each started task calls `task_finished` once).
    running: usize,
}

impl Default for FetchRegistry {
    fn default() -> Self {
        Self::with_cap(DEFAULT_FETCH_CONCURRENCY)
    }
}

impl FetchRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// A registry with an explicit concurrency cap (tests; `new()` uses
    /// [`DEFAULT_FETCH_CONCURRENCY`]).
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            inner: Mutex::new(RegistryInner::default()),
        }
    }

    /// Submit a fetch for `part_id`. If a slot already exists the starter is DROPPED and the
    /// existing slot returned (single-flight coalescing). Otherwise the slot is created and
    /// the starter either runs immediately (a permit was free) or queues FIFO — the slot
    /// reads [`FetchSlot::is_queued`] until promoted. Every started starter's task MUST
    /// eventually call [`task_finished`](Self::task_finished) exactly once.
    pub fn submit(&self, part_id: &str, starter: FetchStarter) -> Arc<FetchSlot> {
        let (slot, run_now) = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = g.slots.get(part_id) {
                return s.clone();
            }
            let s = FetchSlot::new();
            g.slots.insert(part_id.to_string(), s.clone());
            if g.running < self.cap {
                g.running += 1;
                (s, Some(starter))
            } else {
                s.set_queued(true);
                g.queue.push_back((part_id.to_string(), starter));
                (s, None)
            }
        };
        if let Some(start) = run_now {
            start(); // outside the lock — the starter may finish/re-enter synchronously
        }
        slot
    }

    /// A started task finished (any terminal, including cancel): release its permit and
    /// start the next queued fetch, if any. Called exactly once per started starter.
    pub fn task_finished(&self) {
        let next = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.running = g.running.saturating_sub(1);
            match g.queue.pop_front() {
                Some((id, starter)) => {
                    g.running += 1;
                    if let Some(s) = g.slots.get(&id) {
                        s.set_queued(false);
                    }
                    Some(starter)
                }
                None => None,
            }
        };
        if let Some(start) = next {
            start();
        }
    }

    /// Navigate-away semantics: if `part_id` is still admission-QUEUED, remove it (queue
    /// entry + slot) and return `true` — it never starts. A RUNNING (or terminal) fetch is
    /// left untouched (`false`): the download completes into the cache, bounded by the cap.
    pub fn cancel_if_queued(&self, part_id: &str) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = g.queue.len();
        g.queue.retain(|(id, _)| id != part_id);
        if g.queue.len() == before {
            return false;
        }
        g.slots.remove(part_id);
        true
    }

    /// The current slot for `part_id`, if one exists.
    #[must_use]
    pub fn get(&self, part_id: &str) -> Option<Arc<FetchSlot>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .get(part_id)
            .cloned()
    }

    /// Whether `part_id` has ANY live slot (queued, running, or unobserved terminal) — the
    /// cache-clear safety probe: a part with a live slot is skipped by the clear.
    #[must_use]
    pub fn is_active(&self, part_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .contains_key(part_id)
    }

    /// Drop the slot (cancel/reset). Purges a queued entry too, so a queued part never
    /// starts after removal. A subsequent [`submit`](Self::submit) starts a fresh fetch —
    /// the retry seam after an error terminal and the reset after a user cancel. NOTE: a
    /// RUNNING task's permit is NOT released here — its worker still calls
    /// [`task_finished`](Self::task_finished) when the (possibly cancelled) download ends.
    pub fn remove(&self, part_id: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.slots.remove(part_id);
        g.queue.retain(|(id, _)| id != part_id);
    }
}

// ---- Fetch-cache management (S4 pulled forward) --------------------------------
//
// The keep-until-clear cache is user-visible storage; these primitives back the sync
// panel's "Fetched audio: N MB — Clear" row. They NEVER touch the fetch-tmp dir (in-flight
// encrypted downloads) and the clear additionally skips both in-cache-dir decrypt temps
// (FETCH_TEMP_PREFIX) and any file whose part has a live registry slot — the simplest rule
// that can never corrupt an in-flight fetch.

/// Point-in-time cache footprint: decrypted audio files only (decrypt temps excluded).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub bytes: u64,
    pub files: u64,
}

/// Total bytes + file count of the fetch cache dir. A missing dir is an empty cache (not
/// an error). Skips [`FETCH_TEMP_PREFIX`] decrypt temps and subdirectories; best-effort
/// per entry (an unreadable entry is simply not counted).
#[must_use]
pub fn cache_stats(cache_dir: &Path) -> CacheStats {
    let mut stats = CacheStats::default();
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return stats;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(FETCH_TEMP_PREFIX) {
            continue; // an in-flight decrypt temp is not settled cache content
        }
        match entry.metadata() {
            Ok(m) if m.is_file() => {
                stats.bytes = stats.bytes.saturating_add(m.len());
                stats.files += 1;
            }
            _ => {}
        }
    }
    stats
}

/// Delete the cache dir's settled audio files, skipping (a) [`FETCH_TEMP_PREFIX`] decrypt
/// temps and (b) any file whose stem (the part_id) `is_active` reports live — an in-flight
/// or just-finished-but-unobserved fetch is never yanked out from under the player. Only
/// regular files directly in `cache_dir` are ever touched (no recursion, and the fetch-tmp
/// dir is a sibling this function never sees). Best-effort per file; returns what was
/// reclaimed. A missing dir reclaims nothing.
pub fn cache_clear<F: Fn(&str) -> bool>(cache_dir: &Path, is_active: F) -> CacheStats {
    let mut removed = CacheStats::default();
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(FETCH_TEMP_PREFIX) {
            continue; // never touch an in-flight decrypt temp
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue; // never recurse into / remove a directory
        }
        // Cache files are `<part_id>.<ext>`; the stem is the part identity.
        let stem = std::path::Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name);
        if is_active(stem) {
            continue; // live fetch slot → the part is (or may momentarily be) in use
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed.bytes = removed.bytes.saturating_add(meta.len());
            removed.files += 1;
        }
    }
    removed
}
