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

use std::path::Path;

use rusqlite::Connection;

use crate::transport::SyncTransport;
use crate::SyncError;
use yapstack_crypto::audio_stream::{seal_blob, AudioIdentity};

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

/// Whether the part row still exists in `session_audio_parts` (a deleted part → drop the
/// queue entry silently, D9).
fn part_row_exists(conn: &Connection, part_id: &str) -> Result<bool, SyncError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM session_audio_parts WHERE id=?1",
        [part_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
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

    // Deleted-part entries are dropped SILENTLY (D9).
    if !part_row_exists(conn, &entry.part_id)? {
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
