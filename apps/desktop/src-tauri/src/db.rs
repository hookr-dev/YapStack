use std::path::{Path, PathBuf};

/// A single schema migration. Repo-owned (replaces `tauri_plugin_sql::Migration`
/// after the Option A′ backend swap); applied by [`crate::db_service::run_migrations`],
/// which preserves continuity with the plugin's `_sqlx_migrations` bookkeeping.
/// All migrations are `Up`, so there is no `kind` discriminant.
pub struct Migration {
    pub version: i64,
    pub description: &'static str,
    pub sql: &'static str,
}

/// Row to insert into `session_audio_parts`. Mirrors the columns in the v15
/// migration; constructed by both the live finalize path and reconciliation.
pub struct AudioPartRow {
    pub session_id: String,
    pub part_index: u32,
    pub file_path: String,
    pub format: &'static str,
    pub duration_seconds: f32,
    pub sample_rate: u32,
}

/// The `audio_save_locations` DDL, issued verbatim by both
/// [`ensure_runtime_schema`] and [`register_audio_save_location`] so the two
/// sites can never drift apart.
const AUDIO_SAVE_LOCATIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS audio_save_locations (\
    dir TEXT PRIMARY KEY,\
    registered_at TEXT NOT NULL DEFAULT (datetime('now'))\
)";

/// Pre-migration runtime patches. Currently only sweeps stale `recording`
/// sessions left by a prior crash; runtime *schema* patches (segments.speaker_id)
/// live in the frontend's `getDb()` so they run after migrations on fresh installs.
/// `me` = this device's `device_fingerprint` (LIVE_SESSION_STATE.md D7), threaded from the
/// boot call site via `sync::current_device_fingerprint()`. `None` when sync is
/// unconfigured, the identity is unloadable, or the build has no `sync` feature. It flows
/// straight into [`close_orphaned_recordings`], which owner-scopes the sweep.
pub fn ensure_runtime_schema(db_path: &Path, me: Option<&str>) {
    if !db_path.exists() {
        return;
    }

    // Routed through the shared connection helper so it runs on a
    // cr-sqlite-initialized connection under the `sync` feature (A3-ready).
    let managed = match crate::db_service::open_managed(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "ensure_runtime_schema: open({}) failed: {e}; skipping",
                db_path.display()
            );
            return;
        }
    };
    let conn = managed.conn();

    if !table_exists(conn, "segments") {
        return;
    }

    // Out-of-band of the migration list so dev DBs whose `_sqlx_migrations`
    // history is ahead of schema can still pick this up. Idempotent; no-op
    // if already created.
    let _ = conn.execute(AUDIO_SAVE_LOCATIONS_DDL, []);

    close_orphaned_recordings(conn, me);
}

/// Records `dir` in `audio_save_locations`. Called at recording start with
/// the resolved audio-output directory so reconciliation can scan it on
/// next startup even if the run dies before any `session_audio_parts` row
/// is committed. Best-effort: failures are logged and ignored.
pub fn register_audio_save_location(db_path: &Path, dir: &Path) {
    if !db_path.exists() {
        return;
    }
    let Ok(managed) = crate::db_service::open_managed(db_path) else {
        return;
    };
    let conn = managed.conn();
    // Should already exist via ensure_runtime_schema; create if not.
    let _ = conn.execute(AUDIO_SAVE_LOCATIONS_DDL, []);
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let _ = conn.execute(
        "INSERT OR IGNORE INTO audio_save_locations (dir) VALUES (?)",
        rusqlite::params![canon.to_string_lossy().as_ref()],
    );
}

/// Flat liveness threshold (LIVE_SESSION_STATE.md D5 / resolved Q2). A
/// `status='recording'` row is "stale" — treated as a crashed session rather than a
/// live one — once its heartbeat (the max `segments.created_at`, falling back to the
/// session's own `created_at` at zero segments) is older than this. Named constant per
/// Q2 ("flat 3 minutes, a named constant; revisit only if UAT shows flapping").
const RECORDING_STALE_THRESHOLD_MINUTES: i64 = 3;

/// True once the live DB has EVER been CRR-prepared, i.e. cr-sqlite converted the
/// `sessions` table and left its clock shadow table (`sessions__crsql_clock`) behind.
///
/// This mirrors `yapstack_sync::schema::is_crr(conn, "sessions")`
/// (crates/yapstack-sync/src/schema.rs:212) but is evaluated here as a plain
/// `sqlite_master` probe so the boot sweep can decide on a bare rusqlite connection —
/// no crsql extension is needed for a table-existence check — and so it behaves
/// identically in `sync` and `no-sync` builds (a `no-sync` DB never grows this table, so
/// it reads false). It is deliberately NOT `device_fingerprint IS NULL`: a credential
/// clear / fresh keychain over a *retained* CRR DB has a NULL fingerprint yet still holds
/// foreign synced rows, and a fingerprint predicate would let the interim finalize a
/// peer's stale-at-receiver live row and LWW-propagate `'completed'` back after
/// re-enrollment. The DB-level check cannot be fooled that way
/// (LIVE_SESSION_STATE.md "Sweep rules — interim").
fn db_was_crr_prepared(conn: &rusqlite::Connection) -> bool {
    table_exists(conn, "sessions__crsql_clock")
}

/// Production boot-sweep entry (LIVE_SESSION_STATE.md D1-final, "Sweep rules — final",
/// sequencing step 3). Called from [`ensure_runtime_schema`] on the CRR (`open_managed`)
/// connection.
///
/// `me` = this device's `device_fingerprint` (`sync::current_device_fingerprint()`),
/// threaded in from the boot call site now the sweep is owner-aware. It separates an OWN
/// crashed row (finalize) from a FOREIGN live row (never touch, D6):
///
/// - **`me = Some(fp)`** — sync is configured and the identity loaded. Run the full
///   owner-scoped final rule ([`sweep_orphaned_recordings`] with `Some(fp)`) regardless of
///   CRR state: foreign rows are never touched, own crashed rows finalize, own/sync-off
///   empties delete per the delete rule, and NULL-owner rows finalize only when stale. This
///   is the real P0-close path — on a CRR-prepared DB it now performs the deferred
///   own-crash finalization safely because the ownership predicate protects every foreign
///   row.
/// - **`me = None` on a never-CRR-prepared DB** — a pure pre-sync / `no-sync` single-device
///   DB that *cannot* hold another device's rows. `me` is genuinely NULL (sync-off), so run
///   the full final rule with `me = None`: today's delete-empties + stale-finalize
///   semantics, exactly per spec for a NULL column on a sync-off device.
/// - **`me = None` on a CRR-prepared DB** — a synced DB with no loadable identity (sync
///   never configured over a synced DB, or a credential clear / fresh keychain over a
///   retained CRR DB). Ownership is ambiguous: a `status='recording'` row may be a peer's
///   genuinely-live session, and this sweep runs on the CRR connection so any write
///   LWW-propagates. Never guess — TOUCH NOTHING (foreign-safe; own-crash finalization
///   waits until an identity is loadable). This matches the slice-1 interim behavior for
///   synced DBs and keeps P0 fully closed in the ambiguous case.
///
/// The full owner-scoped rule itself is [`sweep_orphaned_recordings`], exercised across
/// the complete spec matrix by the unit tests with synthetic `me` values.
fn close_orphaned_recordings(conn: &rusqlite::Connection, me: Option<&str>) {
    match me {
        // Sync configured + identity loaded: the real owner-scoped final rule. Safe on a
        // CRR-prepared DB — the ownership predicate never touches a foreign row.
        Some(_) => sweep_orphaned_recordings(conn, me),
        None if db_was_crr_prepared(conn) => {
            // Synced DB, no loadable fingerprint → ambiguous ownership. Never guess.
            tracing::info!(
                "close_orphaned_recordings: CRR-prepared DB with no loadable device \
                 fingerprint (sync unconfigured or identity unloadable); touching nothing \
                 (foreign-safe, own-crash finalize deferred). deleted 0, completed 0"
            );
        }
        // Never CRR-prepared: `me` is genuinely NULL (single-device / sync-off semantics).
        None => sweep_orphaned_recordings(conn, None),
    }
}

/// Owner-scoped final boot sweep (LIVE_SESSION_STATE.md "Sweep rules — final").
///
/// `me` = this device's `device_fingerprint`, NULL when sync is unconfigured.
/// `foreign` = `recording_device_id IS NOT NULL AND recording_device_id != me`.
/// `empty` = no `segments` AND no `session_audio_parts` rows.
/// `stale` = heartbeat (`max(segments.created_at)`, fallback `session.created_at`) older
/// than [`RECORDING_STALE_THRESHOLD_MINUTES`] (D5).
///
/// - **Foreign rows are NEVER touched** (D6: a non-owner has no finalization authority).
/// - **DELETE** non-foreign, delete-eligible empties:
///   `status='recording' AND empty AND (owner IS NULL OR owner = me) AND (me IS NULL OR owner = me)`.
///   Sync-off (`me IS NULL`) → today's delete-empties (only the DB's own NULL-owner rows;
///   a foreign owner still fails the not-foreign predicate). Sync-on → only `owner = me`.
///   A NULL-owner row on a sync-ON device is never delete-eligible — it may be a peer's
///   legacy row whose DELETE would sync back.
/// - **FINALIZE** the remaining non-foreign rows not deleted above; the NULL-owner branch
///   additionally requires `stale` (a fresh NULL-owner row can be a not-yet-updated live
///   peer during rollout). Owned rows are always crashes at the owner's own boot, so they
///   finalize regardless of staleness.
///
/// The ownership predicate `(recording_device_id IS NULL OR recording_device_id = ?1)`
/// matches own + NULL rows and excludes foreign in BOTH the `me`-set and `me`-NULL cases
/// (`owner = NULL` is SQL-unknown → falsy), so foreign rows are protected even sync-off.
fn sweep_orphaned_recordings(conn: &rusqlite::Connection, me: Option<&str>) {
    // A brand-new DB whose parts table hasn't landed yet: `empty`/duration key off
    // segments alone.
    let has_parts_table = table_exists(conn, "session_audio_parts");
    let stale_modifier = format!("-{RECORDING_STALE_THRESHOLD_MINUTES} minutes");

    let empty_clause = if has_parts_table {
        "NOT EXISTS (SELECT 1 FROM segments WHERE session_id = sessions.id) \
         AND NOT EXISTS (SELECT 1 FROM session_audio_parts WHERE session_id = sessions.id)"
    } else {
        "NOT EXISTS (SELECT 1 FROM segments WHERE session_id = sessions.id)"
    };
    // ?1 = me (may be NULL). Reused across both predicates. The NULL-safe `IS`
    // operator is deliberate: with plain `=`, `recording_device_id = ?1` yields SQL
    // NULL (not FALSE) whenever the column is NULL, and `FALSE OR NULL = NULL` /
    // `TRUE AND NULL = NULL` would silently drop a NULL-owner row from the WHERE.
    // `IS` compares NULL as a value, so every branch resolves to a definite bool.
    let not_foreign = "(recording_device_id IS NULL OR recording_device_id IS ?1)";
    let delete_eligible = "(?1 IS NULL OR recording_device_id IS ?1)";
    // ?2 = the '-N minutes' modifier.
    let stale_clause = "datetime(COALESCE( \
            (SELECT MAX(created_at) FROM segments WHERE session_id = sessions.id), \
            created_at \
        )) <= datetime('now', ?2)";

    let delete_sql = format!(
        "DELETE FROM sessions \
         WHERE status = 'recording' \
           AND ({empty_clause}) \
           AND {not_foreign} \
           AND {delete_eligible}"
    );
    let deleted = conn
        .execute(&delete_sql, rusqlite::params![me])
        .unwrap_or_else(|e| {
            tracing::warn!("sweep_orphaned_recordings: delete failed: {e}");
            0
        });

    let duration_expr = if has_parts_table {
        "COALESCE( \
            (SELECT SUM(duration_seconds) FROM session_audio_parts WHERE session_id = sessions.id), \
            (SELECT MAX(audio_offset_seconds + chunk_duration_seconds) \
             FROM segments WHERE session_id = sessions.id), \
            duration_seconds \
        )"
    } else {
        "COALESCE( \
            (SELECT MAX(audio_offset_seconds + chunk_duration_seconds) \
             FROM segments WHERE session_id = sessions.id), \
            duration_seconds \
        )"
    };
    let update_sql = format!(
        "UPDATE sessions SET \
            status = 'completed', \
            total_segments = (SELECT COUNT(*) FROM segments WHERE session_id = sessions.id), \
            duration_seconds = {duration_expr}, \
            updated_at = datetime('now') \
         WHERE status = 'recording' \
           AND {not_foreign} \
           AND NOT (({empty_clause}) AND {delete_eligible}) \
           AND ((?1 IS NOT NULL AND recording_device_id IS ?1) OR ({stale_clause}))"
    );
    let completed = conn
        .execute(&update_sql, rusqlite::params![me, stale_modifier])
        .unwrap_or_else(|e| {
            tracing::warn!("sweep_orphaned_recordings: finalize failed: {e}");
            0
        });

    tracing::info!(
        sync_off = me.is_none(),
        "sweep_orphaned_recordings: deleted {deleted}, completed {completed}"
    );
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Inserts a `session_audio_parts` row. Uses INSERT OR IGNORE so a partial
/// crash that leaves a row already inserted is recoverable on retry.
pub fn insert_audio_part_row(db_path: &Path, row: &AudioPartRow) -> rusqlite::Result<()> {
    // Cutover guard: `open_managed` opens with CREATE, so a finalize racing the A3 CRR
    // enable-cutover file-swap (live path momentarily absent between renames) would
    // otherwise resurrect a stray empty `yapstack.db`. Precheck existence like the sibling
    // read/reconcile callers and refuse rather than create; the finalize caller already
    // logs the Err and relies on the FE refresh to recover the row.
    if !db_path.exists() {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(format!(
                "insert_audio_part_row: {} does not exist (cutover swap window); \
                 refusing to create a stray DB",
                db_path.display()
            )),
        ));
    }
    let managed = crate::db_service::open_managed(db_path)?;
    insert_audio_part_row_on(managed.conn(), row)
}

/// [`insert_audio_part_row`] on a connection the caller already holds. Each
/// INSERT still runs in its own autocommit transaction, so the cr-sqlite
/// `db_version` grouping is identical either way.
fn insert_audio_part_row_on(
    conn: &rusqlite::Connection,
    row: &AudioPartRow,
) -> rusqlite::Result<()> {
    use rusqlite::params;
    let id = mint_part_id(conn)?;
    let created_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "INSERT OR IGNORE INTO session_audio_parts (
            id, session_id, part_index, file_path, format,
            duration_seconds, sample_rate, created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id,
            row.session_id,
            row.part_index,
            row.file_path,
            row.format,
            row.duration_seconds as f64,
            row.sample_rate,
            created_at,
        ],
    )?;
    Ok(())
}

/// Mint a fresh `session_audio_parts.id` as a **CSPRNG UUIDv4** (audio round-trip D6).
///
/// Blobs are addressed cross-device by this row id (server `audio_objects` key + the
/// crypto wrap AAD's `part_id`), so it MUST be collision-free across independent devices.
/// The prior clock-derived id was NOT: two NTP-synced LAN devices minting at the same
/// nanosecond could collide, and a cross-device PK collision under cr-sqlite would merge
/// two distinct parts. Entropy comes from SQLite's OS-seeded `randomblob` (the same CSPRNG
/// the v15 backfill already uses), formatted as a **32-hex simple-format** UUIDv4 (no
/// dashes: version nibble = 4, variant nibble ∈ {8,9,a,b}) — the format the table already
/// stores and the server's `/audio/part/{part_id}` route accepts.
fn mint_part_id(conn: &rusqlite::Connection) -> rusqlite::Result<String> {
    let raw: String = conn.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
    // A short id would both index out of bounds below and break the collision-free
    // contract, so it is an error, never a fallback: any substitute value this
    // function could invent is by definition NOT collision-free.
    if raw.len() != 32 {
        return Err(mint_part_id_error("randomblob(16) hex must be 32 chars"));
    }
    let mut b = raw.into_bytes();
    b[12] = b'4'; // RFC 4122 version 4
                  // Variant nibble → 10xx (8..b): keep the low 2 random bits, force the top 2 to `10`.
    let v = (b[16] as char).to_digit(16).unwrap_or(0) as u8;
    let v = (v & 0x3) | 0x8;
    b[16] = std::char::from_digit(u32::from(v), 16)
        .map(|c| c as u8)
        .unwrap_or(b'8');
    String::from_utf8(b).map_err(|_| mint_part_id_error("part id not ascii hex"))
}

/// `rusqlite::Error` for a `mint_part_id` invariant breach. Returned rather than
/// panicked: release builds are `panic=abort` and this sits on the live recording
/// finalize path, where the caller logs and degrades gracefully.
fn mint_part_id_error(msg: &str) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
        Some(msg.to_string()),
    )
}

/// Returns canonicalized parent directories of every `session_audio_parts.file_path`,
/// always including `$APP_DATA_DIR/audio` as the seed. Used to bootstrap the
/// trusted-audio-dirs set at startup.
pub fn list_audio_part_directories(db_path: &Path, app_audio_dir: &Path) -> Vec<PathBuf> {
    use std::collections::HashSet;
    let mut out: HashSet<PathBuf> = HashSet::new();
    if let Ok(canon) = std::fs::canonicalize(app_audio_dir) {
        out.insert(canon);
    } else {
        out.insert(app_audio_dir.to_path_buf());
    }
    if !db_path.exists() {
        return out.into_iter().collect();
    }
    let Ok(managed) = crate::db_service::open_managed(db_path) else {
        return out.into_iter().collect();
    };
    let conn = managed.conn();
    if let Ok(mut stmt) = conn
        .prepare("SELECT DISTINCT file_path FROM session_audio_parts WHERE file_path IS NOT NULL")
    {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            for path in rows.flatten() {
                if let Some(parent) = Path::new(&path).parent() {
                    let canon =
                        std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
                    out.insert(canon);
                }
            }
        }
    }
    // `audio_save_locations` is registered at recording start and persists
    // even when no part row landed (crash between finalize and INSERT, or
    // a brand-new custom dir whose first run has not yet committed). It
    // closes the gap where reconciliation would otherwise never visit a
    // newly-chosen audioSaveLocation.
    if let Ok(mut stmt) = conn.prepare("SELECT dir FROM audio_save_locations") {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            for dir in rows.flatten() {
                let p = Path::new(&dir);
                let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                out.insert(canon);
            }
        }
    }
    out.into_iter().collect()
}

/// Walks each trusted dir for filenames matching `{session_id}.{part_index}.{wav|mp3}`
/// whose `(session_id, part_index)` row is missing. INSERT OR IGNORE recovers
/// the row using duration read from file metadata. Rare orphan recovery —
/// nominal flow has Rust insert the row inline at finalize time.
pub fn reconcile_audio_parts(db_path: &Path, dirs: &[PathBuf]) {
    if !db_path.exists() {
        return;
    }
    let Ok(managed) = crate::db_service::open_managed(db_path) else {
        return;
    };
    let conn = managed.conn();
    if !table_exists(conn, "session_audio_parts") || !table_exists(conn, "sessions") {
        return;
    }

    let mut recovered = 0u32;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some((session_id, part_index, format)) = parse_part_filename(&name) else {
                continue;
            };
            // Skip if (session_id, part_index) already exists.
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM session_audio_parts WHERE session_id = ? AND part_index = ?",
                    rusqlite::params![session_id, part_index],
                    |_| Ok(()),
                )
                .is_ok();
            if exists {
                continue;
            }
            // Skip if the session row doesn't exist (file is unrelated).
            let session_exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sessions WHERE id = ?",
                    rusqlite::params![session_id],
                    |_| Ok(()),
                )
                .is_ok();
            if !session_exists {
                continue;
            }
            let abs = entry.path();
            let (duration, sample_rate) =
                read_audio_metadata(&abs, format).unwrap_or((0.0, 48_000));
            let row = AudioPartRow {
                session_id: session_id.to_string(),
                part_index,
                file_path: abs.to_string_lossy().into_owned(),
                format,
                duration_seconds: duration,
                sample_rate,
            };
            if insert_audio_part_row_on(conn, &row).is_ok() {
                recovered += 1;
            }
        }
    }
    if recovered > 0 {
        tracing::info!("reconcile_audio_parts: recovered {recovered} orphan rows");
    }
}

/// Returns `(session_id, part_index, format)` for a filename like
/// `{uuid-or-hex}.{n}.{wav|mp3}`. Session id is a non-strict shape — accepts
/// any token without dots — but rejects dictation-style `{id}.wav` (no
/// part-index segment).
fn parse_part_filename(name: &str) -> Option<(&str, u32, &'static str)> {
    let (stem, ext) = name.rsplit_once('.')?;
    let format = match ext.to_ascii_lowercase().as_str() {
        "wav" => "wav",
        "mp3" => "mp3",
        _ => return None,
    };
    let (session_id, idx_str) = stem.rsplit_once('.')?;
    let part_index: u32 = idx_str.parse().ok()?;
    if session_id.is_empty() || session_id.contains('.') {
        return None;
    }
    Some((session_id, part_index, format))
}

/// Returns `(duration_seconds, sample_rate)` for a recovered audio file.
/// WAV uses `hound`; MP3 parses the first MPEG audio frame header. Returns
/// `(0.0, 48000)` for unparseable MP3 — playback still works, but resume is
/// blocked until the part is overwritten by a fresh finalize.
fn read_audio_metadata(path: &Path, format: &'static str) -> Option<(f32, u32)> {
    match format {
        "wav" => {
            let reader = hound::WavReader::open(path).ok()?;
            let spec = reader.spec();
            let sample_rate = spec.sample_rate;
            let frames = reader.duration() as f32;
            if sample_rate == 0 {
                return None;
            }
            Some((frames / sample_rate as f32, sample_rate))
        }
        "mp3" => {
            // Read the first 64 KiB; that's enough to skip ID3v2 + locate
            // a sync. CBR is the only thing the app's encoder produces.
            let mut buf = [0u8; 64 * 1024];
            let n = read_prefix(path, &mut buf)?;
            let (bitrate_bps, sample_rate, id3_size) = mp3_first_frame(&buf[..n])?;
            let file_len = std::fs::metadata(path).ok()?.len() as i64;
            let body_bits = ((file_len - id3_size as i64).max(0) as f64) * 8.0;
            let duration = (body_bits / bitrate_bps as f64) as f32;
            Some((duration, sample_rate))
        }
        _ => None,
    }
}

fn read_prefix(path: &Path, buf: &mut [u8]) -> Option<usize> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(buf).ok()?;
    Some(n)
}

/// Parses the first MPEG audio frame header to extract bitrate (bps) and
/// sample rate. Skips an ID3v2 tag if present and returns its byte length so
/// callers can compute body size for CBR duration math.
fn mp3_first_frame(data: &[u8]) -> Option<(u32, u32, usize)> {
    let id3_size = if data.len() >= 10 && &data[0..3] == b"ID3" {
        // 28-bit synchsafe int across data[6..10]
        let s = ((data[6] as usize) << 21)
            | ((data[7] as usize) << 14)
            | ((data[8] as usize) << 7)
            | (data[9] as usize);
        s + 10
    } else {
        0
    };
    let h = data.get(id3_size..id3_size + 4)?;
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version_id = (h[1] >> 3) & 0x03; // 0=2.5, 2=2, 3=1
    let layer = (h[1] >> 1) & 0x03; // 1=L3, 2=L2, 3=L1
    if layer == 0 || version_id == 1 {
        return None;
    }
    let bitrate_idx = ((h[2] >> 4) & 0x0F) as usize;
    let sample_rate_idx = ((h[2] >> 2) & 0x03) as usize;
    if bitrate_idx == 0 || bitrate_idx == 0xF || sample_rate_idx == 3 {
        return None;
    }
    let bitrates_v1_l3 = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    let bitrates_v2_l3 = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let is_v1 = version_id == 3;
    let kbps = if is_v1 {
        bitrates_v1_l3[bitrate_idx]
    } else {
        bitrates_v2_l3[bitrate_idx]
    };
    let rates_v1 = [44_100u32, 48_000, 32_000];
    let rates_v2 = [22_050u32, 24_000, 16_000];
    let rates_v25 = [11_025u32, 12_000, 8_000];
    let sample_rate = match version_id {
        3 => rates_v1[sample_rate_idx],
        2 => rates_v2[sample_rate_idx],
        _ => rates_v25[sample_rate_idx],
    };
    Some((kbps as u32 * 1000, sample_rate, id3_size))
}

pub fn migrations() -> Vec<Migration> {
    vec![
        Migration {
            version: 1,
            description: "create sessions and segments tables",
            sql: r#"
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                source TEXT NOT NULL DEFAULT 'Mixed',
                status TEXT NOT NULL DEFAULT 'recording',
                duration_seconds REAL,
                total_segments INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE segments (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                source TEXT NOT NULL,
                text TEXT NOT NULL,
                audio_offset_seconds REAL NOT NULL,
                chunk_duration_seconds REAL NOT NULL,
                confidence REAL NOT NULL DEFAULT 1.0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                chunk_index INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

            CREATE INDEX idx_segments_session_id ON segments(session_id);
            CREATE INDEX idx_segments_offset ON segments(session_id, audio_offset_seconds);
            CREATE INDEX idx_sessions_created ON sessions(created_at DESC);
        "#,
        },
        Migration {
            version: 2,
            description: "add folders, pins, and folder_id to sessions",
            sql: r#"
            CREATE TABLE folders (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                parent_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX idx_folders_parent ON folders(parent_id);

            ALTER TABLE sessions ADD COLUMN folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL;
            ALTER TABLE sessions ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE sessions ADD COLUMN pinned_at TEXT;
            CREATE INDEX idx_sessions_folder ON sessions(folder_id);
            CREATE INDEX idx_sessions_pinned ON sessions(is_pinned, pinned_at DESC);
        "#,
        },
        Migration {
            version: 3,
            description: "add segment editing columns",
            sql: r#"
            ALTER TABLE segments ADD COLUMN original_text TEXT;
            ALTER TABLE segments ADD COLUMN edited_at TEXT;
            ALTER TABLE segments ADD COLUMN deleted_at TEXT;
            ALTER TABLE segments ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
        "#,
        },
        Migration {
            version: 4,
            description: "add notes, note_versions, and session_type",
            sql: r#"
            CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL UNIQUE,
                content TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

            CREATE TABLE note_versions (
                id TEXT PRIMARY KEY,
                note_id TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
            );
            CREATE INDEX idx_note_versions_note ON note_versions(note_id, created_at DESC);

            ALTER TABLE sessions ADD COLUMN session_type TEXT NOT NULL DEFAULT 'transcription';
        "#,
        },
        Migration {
            version: 5,
            description: "add wav file path and duration to sessions",
            sql: r#"
            ALTER TABLE sessions ADD COLUMN wav_file_path TEXT;
            ALTER TABLE sessions ADD COLUMN wav_duration_seconds REAL;
        "#,
        },
        Migration {
            version: 6,
            description: "add sort_order to sessions and shares table",
            sql: r#"
            ALTER TABLE sessions ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;

            CREATE TABLE shares (
                id TEXT PRIMARY KEY,
                folder_id TEXT NOT NULL,
                shared_with_email TEXT,
                permission TEXT NOT NULL DEFAULT 'viewer',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                expires_at TEXT,
                FOREIGN KEY (folder_id) REFERENCES folders(id) ON DELETE CASCADE
            );
            CREATE INDEX idx_shares_folder ON shares(folder_id);
        "#,
        },
        Migration {
            version: 7,
            description: "add chat_messages table for AI chat persistence",
            sql: r#"
            CREATE TABLE chat_messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                action TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );
            CREATE INDEX idx_chat_messages_session ON chat_messages(session_id, created_at ASC);
        "#,
        },
        Migration {
            version: 8,
            description: "add context_key to chat_messages, make session_id nullable",
            sql: r#"
            CREATE TABLE chat_messages_new (
                id TEXT PRIMARY KEY,
                context_key TEXT NOT NULL,
                session_id TEXT,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                action TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );
            INSERT INTO chat_messages_new (id, context_key, session_id, role, content, action, created_at)
            SELECT id, session_id, session_id, role, content, action, created_at FROM chat_messages;
            DROP TABLE chat_messages;
            ALTER TABLE chat_messages_new RENAME TO chat_messages;
            CREATE INDEX idx_chat_messages_context ON chat_messages(context_key, created_at ASC);
            CREATE INDEX idx_chat_messages_session ON chat_messages(session_id);
        "#,
        },
        Migration {
            version: 9,
            description: "add folder metadata and session_folders junction table",
            sql: r#"
            ALTER TABLE folders ADD COLUMN icon TEXT;
            ALTER TABLE folders ADD COLUMN color TEXT;
            ALTER TABLE folders ADD COLUMN description TEXT;

            CREATE TABLE session_folders (
                session_id TEXT NOT NULL,
                folder_id TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (session_id, folder_id),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
                FOREIGN KEY (folder_id) REFERENCES folders(id) ON DELETE CASCADE
            );
            CREATE INDEX idx_session_folders_folder ON session_folders(folder_id);
            CREATE INDEX idx_session_folders_session ON session_folders(session_id);

            INSERT INTO session_folders (session_id, folder_id)
            SELECT id, folder_id FROM sessions WHERE folder_id IS NOT NULL;
        "#,
        },
        Migration {
            version: 10,
            description: "add dictation_history table",
            sql: r#"
            CREATE TABLE dictation_history (
                id TEXT PRIMARY KEY,
                slot_id TEXT NOT NULL,
                slot_name TEXT NOT NULL,
                input_text TEXT NOT NULL,
                output_text TEXT NOT NULL,
                ai_enabled INTEGER NOT NULL DEFAULT 0,
                ai_prompt TEXT,
                output_action TEXT NOT NULL,
                wav_file_path TEXT,
                wav_duration_seconds REAL,
                session_id TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX idx_dictation_history_created ON dictation_history(created_at DESC);
            CREATE INDEX idx_dictation_history_slot ON dictation_history(slot_id);
        "#,
        },
        Migration {
            version: 11,
            description: "add tags and session_tags tables",
            sql: r#"
            CREATE TABLE tags (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                color TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX idx_tags_name ON tags(name);

            CREATE TABLE session_tags (
                session_id TEXT NOT NULL,
                tag_id TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT 'manual',
                confidence REAL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (session_id, tag_id),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
                FOREIGN KEY (tag_id) REFERENCES tags(id) ON DELETE CASCADE
            );
            CREATE INDEX idx_session_tags_tag ON session_tags(tag_id);
            CREATE INDEX idx_session_tags_session ON session_tags(session_id);
        "#,
        },
        Migration {
            version: 12,
            description: "add FTS5 search tables for segments, notes, sessions, dictation_history",
            sql: r#"
            -- Contentless FTS5 tables that own their searchable text plus the
            -- source-row TEXT primary key as UNINDEXED columns. Search queries
            -- return the PK directly; no rowid mapping needed for our UUID PKs.

            CREATE VIRTUAL TABLE segments_fts USING fts5(
                segment_id UNINDEXED,
                session_id UNINDEXED,
                text,
                tokenize = 'porter unicode61 remove_diacritics 2'
            );

            CREATE VIRTUAL TABLE notes_fts USING fts5(
                note_id UNINDEXED,
                session_id UNINDEXED,
                content,
                tokenize = 'porter unicode61 remove_diacritics 2'
            );

            CREATE VIRTUAL TABLE sessions_fts USING fts5(
                session_id UNINDEXED,
                title,
                tokenize = 'porter unicode61 remove_diacritics 2'
            );

            CREATE VIRTUAL TABLE dictations_fts USING fts5(
                dictation_id UNINDEXED,
                output_text,
                input_text,
                tokenize = 'porter unicode61 remove_diacritics 2'
            );

            -- Backfill from existing rows. segments_fts skips soft-deleted rows.
            INSERT INTO segments_fts (segment_id, session_id, text)
                SELECT id, session_id, text FROM segments WHERE deleted_at IS NULL;

            INSERT INTO notes_fts (note_id, session_id, content)
                SELECT id, session_id, content FROM notes;

            INSERT INTO sessions_fts (session_id, title)
                SELECT id, title FROM sessions;

            INSERT INTO dictations_fts (dictation_id, output_text, input_text)
                SELECT id, output_text, input_text FROM dictation_history;

            -- Triggers: keep FTS in sync with source tables.

            CREATE TRIGGER segments_ai AFTER INSERT ON segments
            WHEN new.deleted_at IS NULL
            BEGIN
                INSERT INTO segments_fts (segment_id, session_id, text)
                    VALUES (new.id, new.session_id, new.text);
            END;

            CREATE TRIGGER segments_ad AFTER DELETE ON segments BEGIN
                DELETE FROM segments_fts WHERE segment_id = old.id;
            END;

            CREATE TRIGGER segments_au AFTER UPDATE ON segments BEGIN
                DELETE FROM segments_fts WHERE segment_id = old.id;
                INSERT INTO segments_fts (segment_id, session_id, text)
                    SELECT new.id, new.session_id, new.text WHERE new.deleted_at IS NULL;
            END;

            CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
                INSERT INTO notes_fts (note_id, session_id, content)
                    VALUES (new.id, new.session_id, new.content);
            END;

            CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN
                DELETE FROM notes_fts WHERE note_id = old.id;
            END;

            CREATE TRIGGER notes_au AFTER UPDATE ON notes BEGIN
                DELETE FROM notes_fts WHERE note_id = old.id;
                INSERT INTO notes_fts (note_id, session_id, content)
                    VALUES (new.id, new.session_id, new.content);
            END;

            CREATE TRIGGER sessions_ai AFTER INSERT ON sessions BEGIN
                INSERT INTO sessions_fts (session_id, title)
                    VALUES (new.id, new.title);
            END;

            CREATE TRIGGER sessions_ad AFTER DELETE ON sessions BEGIN
                DELETE FROM sessions_fts WHERE session_id = old.id;
            END;

            CREATE TRIGGER sessions_au AFTER UPDATE OF title ON sessions BEGIN
                DELETE FROM sessions_fts WHERE session_id = old.id;
                INSERT INTO sessions_fts (session_id, title)
                    VALUES (new.id, new.title);
            END;

            CREATE TRIGGER dictations_ai AFTER INSERT ON dictation_history BEGIN
                INSERT INTO dictations_fts (dictation_id, output_text, input_text)
                    VALUES (new.id, new.output_text, new.input_text);
            END;

            CREATE TRIGGER dictations_ad AFTER DELETE ON dictation_history BEGIN
                DELETE FROM dictations_fts WHERE dictation_id = old.id;
            END;

            CREATE TRIGGER dictations_au AFTER UPDATE ON dictation_history BEGIN
                DELETE FROM dictations_fts WHERE dictation_id = old.id;
                INSERT INTO dictations_fts (dictation_id, output_text, input_text)
                    VALUES (new.id, new.output_text, new.input_text);
            END;
        "#,
        },
        Migration {
            version: 13,
            description: "add tool_calls JSON column to chat_messages",
            sql: r#"
            ALTER TABLE chat_messages ADD COLUMN tool_calls TEXT;
        "#,
        },
        Migration {
            version: 14,
            description: "per-LLM-response chat_messages rows for tool replay",
            // New columns:
            //   send_id      groups all rows derived from one user send
            //   sequence     ordering within send_id (0=user, 1+ = subsequent)
            //   tool_call_id required on role='tool', references the parent
            //                assistant row's tool_calls[].id
            //   observation  structured ToolObservation JSON, role='tool' only
            //   status       'done' | 'error', role='tool' only
            //
            // SQLite can't relax the implicit role check in-place, but the
            // CHECK was never declared explicitly — `role` is just TEXT, so
            // 'tool' is already accepted by the column type. No CHECK changes
            // needed.
            //
            // `content` was NOT NULL in the original CREATE TABLE. SQLite
            // can't drop NOT NULL via ALTER, so we keep writes valid by
            // storing an empty string for assistant rows that only emit
            // tool_calls. The TS layer treats `content === ""` as "no prose".
            //
            // Backfill: every existing row is its own send (one-row sends),
            // so set send_id = id, sequence = 0. Pre-v14 tool memory is left
            // dormant in the legacy `tool_calls` JSON column for read-side
            // soft fallback.
            sql: r#"
            ALTER TABLE chat_messages ADD COLUMN send_id TEXT;
            ALTER TABLE chat_messages ADD COLUMN sequence INTEGER;
            ALTER TABLE chat_messages ADD COLUMN tool_call_id TEXT;
            ALTER TABLE chat_messages ADD COLUMN observation TEXT;
            ALTER TABLE chat_messages ADD COLUMN status TEXT;
            UPDATE chat_messages SET send_id = id WHERE send_id IS NULL;
            UPDATE chat_messages SET sequence = 0 WHERE sequence IS NULL;
            CREATE INDEX IF NOT EXISTS idx_chat_messages_send
                ON chat_messages(context_key, send_id, sequence);
        "#,
        },
        Migration {
            version: 15,
            description: "session_audio_parts table + backfill from sessions.wav_file_path",
            // Each recording run produces one part. A session's audio is the
            // ordered concatenation of its parts. Resume = append a new part,
            // never mutate prior parts.
            //
            // Backfill: every existing session with a wav_file_path becomes
            // part_index=0 in the new table. Sample rate isn't stored on the
            // legacy row; default to 48000 (only used for diagnostics).
            //
            // We deliberately do NOT drop the legacy `wav_file_path` /
            // `wav_duration_seconds` columns from `sessions` here — combining
            // ALTER TABLE DROP COLUMN with the backfill INSERT in the same
            // tauri-plugin-sql migration transaction has been seen to fail
            // on some SQLite builds, leaving the DB in a dead state. The
            // columns are unused at the TypeScript boundary; they stay
            // dormant until a future cleanup migration drops them.
            //
            // The migration is idempotent so it can re-run cleanly:
            //   - CREATE TABLE IF NOT EXISTS
            //   - INSERT OR IGNORE (the (session_id, part_index) UNIQUE
            //     constraint short-circuits duplicates).
            sql: r#"
            CREATE TABLE IF NOT EXISTS session_audio_parts (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                part_index INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                format TEXT NOT NULL CHECK (format IN ('wav','mp3')),
                duration_seconds REAL NOT NULL,
                sample_rate INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE (session_id, part_index)
            );
            CREATE INDEX IF NOT EXISTS idx_audio_parts_session
                ON session_audio_parts(session_id, part_index);

            INSERT OR IGNORE INTO session_audio_parts (
                id, session_id, part_index, file_path, format,
                duration_seconds, sample_rate, created_at
            )
            SELECT
                lower(hex(randomblob(16))),
                id,
                0,
                wav_file_path,
                CASE WHEN wav_file_path LIKE '%.mp3' THEN 'mp3' ELSE 'wav' END,
                COALESCE(wav_duration_seconds, 0),
                48000,
                COALESCE(updated_at, created_at)
            FROM sessions
            WHERE wav_file_path IS NOT NULL;
        "#,
        },
        Migration {
            version: 16,
            description: "sessions.recording_device_id attribution column (LIVE_SESSION_STATE D2)",
            // The recorder's `device_fingerprint`; NULL = legacy/pre-attribution OR
            // sync-not-configured. ONE `ALTER TABLE sessions …` statement so
            // `db_service::run_migrations` auto-wraps it through `crsql_alter`
            // (crsql_begin_alter/commit_alter, backfilling col_version=1) on a
            // CRR-prepared DB, and applies it plainly on a fresh / no-sync DB. A
            // device that CRRified at the base schema version picks the column up via
            // OUT_OF_BAND_ALTERS (yapstack-sync schema.rs) at cutover / boot self-heal.
            sql: r#"
            ALTER TABLE sessions ADD COLUMN recording_device_id TEXT;
        "#,
        },
        Migration {
            version: 17,
            description: "chat_context_settings table (per-chat-context Profile override)",
            // Folds the former frontend-only runtime CREATE (lib/db.ts) into the
            // migration list so a fresh DB gets the table from the ordered migrations
            // rather than the idempotent getDb() bootstrap. `profile_id` NULL means
            // "use the live default Chat Assignment"; a non-null row is an explicit
            // per-context override. CREATE TABLE IF NOT EXISTS keeps it idempotent and
            // harmless on any DB that already has the table (dev DBs whose
            // `_sqlx_migrations` history is misaligned still carry the db.ts copy — the
            // db.ts CREATE stays as a belt-and-suspenders no-op for exactly that case).
            sql: r#"
            CREATE TABLE IF NOT EXISTS chat_context_settings (
                context_key TEXT PRIMARY KEY,
                profile_id  TEXT NULL,
                updated_at  TEXT NOT NULL
            );
        "#,
        },
        // segments.speaker_id is added by the frontend's `getDb()` after
        // migrations run — kept out of the migration list because some
        // local dev DBs picked up a "ghost" v11 entry from another branch
        // that makes sqlx silently refuse later migrations.
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrations_versions_strictly_increase_from_one() {
        let versions: Vec<i64> = migrations().iter().map(|x| x.version).collect();
        assert_eq!(versions.first(), Some(&1), "the chain must start at v1");
        // Strictly increasing, not merely non-decreasing: `run_migrations_list`
        // treats every version already in `_sqlx_migrations` as applied, so a
        // REPEATED version number silently skips a migration on every existing DB —
        // the same shape as the "ghost" v11 entry noted on the migration list.
        assert!(
            versions.windows(2).all(|w| w[1] > w[0]),
            "migration versions must strictly increase: {versions:?}"
        );
    }

    #[test]
    fn test_migration_v16_adds_recording_device_id_column() {
        let m = migrations();
        let v16 = &m[15];
        assert_eq!(v16.version, 16);
        // Apply the whole chain and confirm the column materializes and defaults NULL.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for mig in migrations() {
            conn.execute_batch(mig.sql)
                .unwrap_or_else(|e| panic!("migration v{} failed: {}", mig.version, e));
        }
        conn.execute(
            "INSERT INTO sessions (id, title, source, status) \
             VALUES ('s16', 't', 'MicOnly', 'recording')",
            [],
        )
        .unwrap();
        let owner: Option<String> = conn
            .query_row(
                "SELECT recording_device_id FROM sessions WHERE id = 's16'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, None, "new column defaults NULL (pre-attribution)");
    }

    #[test]
    fn test_migration_v17_creates_chat_context_settings() {
        let m = migrations();
        let v17 = &m[16];
        assert_eq!(v17.version, 17);
        // Apply the whole chain and confirm the table materializes with the right shape.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for mig in migrations() {
            conn.execute_batch(mig.sql)
                .unwrap_or_else(|e| panic!("migration v{} failed: {}", mig.version, e));
        }
        conn.execute(
            "INSERT INTO chat_context_settings (context_key, profile_id, updated_at) \
             VALUES ('ctx-a', NULL, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let pid: Option<String> = conn
            .query_row(
                "SELECT profile_id FROM chat_context_settings WHERE context_key = 'ctx-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pid, None, "NULL profile_id means 'use the live default'");
        // Idempotent: re-running the v17 CREATE IF NOT EXISTS is a clean no-op.
        conn.execute_batch(v17.sql).unwrap();
    }

    #[test]
    fn parse_part_filename_accepts_session_index_format() {
        let (sid, idx, fmt) = parse_part_filename("abc-def-123.0.wav").unwrap();
        assert_eq!(sid, "abc-def-123");
        assert_eq!(idx, 0);
        assert_eq!(fmt, "wav");

        let (sid, idx, fmt) = parse_part_filename("uuid.7.mp3").unwrap();
        assert_eq!(sid, "uuid");
        assert_eq!(idx, 7);
        assert_eq!(fmt, "mp3");
    }

    #[test]
    fn parse_part_filename_rejects_legacy_dictation_shape() {
        // `{id}.wav` (no part-index segment) — must not match.
        assert!(parse_part_filename("abc.wav").is_none());
        assert!(parse_part_filename("notaudio.txt").is_none());
        assert!(parse_part_filename("noext").is_none());
    }

    #[test]
    fn insert_and_reconcile_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        // Apply all migrations to the test DB.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for m in migrations() {
            conn.execute_batch(m.sql).unwrap();
        }
        conn.execute(
            "INSERT INTO sessions (id, title, source, status) \
             VALUES ('sess-x', 't', 'MicOnly', 'completed')",
            [],
        )
        .unwrap();
        drop(conn);

        // Write a fake WAV file matching the parts naming scheme into a
        // standalone audio dir so reconciliation can find it.
        let audio_dir = dir.path().join("audio");
        std::fs::create_dir_all(&audio_dir).unwrap();
        let wav_path = audio_dir.join("sess-x.0.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav_path, spec).unwrap();
        for _ in 0..16_000 {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();

        // No row yet — reconciliation should insert one.
        reconcile_audio_parts(&db_path, std::slice::from_ref(&audio_dir));
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (count, dur, sr): (i64, f64, i64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(duration_seconds), MAX(sample_rate) \
                 FROM session_audio_parts WHERE session_id = 'sess-x'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(sr, 16_000);
        assert!((dur - 1.0).abs() < 0.01, "expected ~1s, got {dur}");

        // Running reconciliation again must be a no-op (INSERT OR IGNORE).
        reconcile_audio_parts(&db_path, std::slice::from_ref(&audio_dir));
        let count2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_audio_parts WHERE session_id = 'sess-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count2, 1, "reconciliation must be idempotent");
    }

    #[test]
    fn reconcile_seeds_from_audio_save_locations_even_without_part_rows() {
        // Reproduces the "first run to a brand-new custom dir crashes
        // before INSERT" case: the dir has a part file but no part row,
        // and no other row points at it. Reconciliation must still find
        // it via the registered `audio_save_locations` row.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for m in migrations() {
            conn.execute_batch(m.sql).unwrap();
        }
        // ensure_runtime_schema would normally create this; do it
        // explicitly for the test.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS audio_save_locations (\
                dir TEXT PRIMARY KEY,\
                registered_at TEXT NOT NULL DEFAULT (datetime('now'))\
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, title, source, status) \
             VALUES ('orphan', 't', 'MicOnly', 'completed')",
            [],
        )
        .unwrap();
        drop(conn);

        let custom_dir = dir.path().join("custom-A");
        std::fs::create_dir_all(&custom_dir).unwrap();
        register_audio_save_location(&db_path, &custom_dir);

        // Simulated orphan: file exists, no part row.
        let wav_path = custom_dir.join("orphan.0.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav_path, spec).unwrap();
        writer.write_sample(0i16).unwrap();
        writer.finalize().unwrap();

        // Drive reconciliation through the same pipeline lib.rs uses:
        // list_audio_part_directories must include the registered dir
        // even though no parts row references it yet.
        let app_audio_dir = dir.path().join("audio");
        std::fs::create_dir_all(&app_audio_dir).unwrap();
        let dirs = list_audio_part_directories(&db_path, &app_audio_dir);
        assert!(
            dirs.iter()
                .any(|d| d == &std::fs::canonicalize(&custom_dir).unwrap()),
            "registered audio_save_location must be returned for reconciliation: {dirs:?}"
        );

        reconcile_audio_parts(&db_path, &dirs);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_audio_parts WHERE session_id = 'orphan'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "orphan in registered custom dir must be recovered"
        );
    }

    #[test]
    fn reconcile_skips_files_for_unknown_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for m in migrations() {
            conn.execute_batch(m.sql).unwrap();
        }
        drop(conn);

        let audio_dir = dir.path().join("audio");
        std::fs::create_dir_all(&audio_dir).unwrap();
        let wav_path = audio_dir.join("ghost.0.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav_path, spec).unwrap();
        writer.write_sample(0i16).unwrap();
        writer.finalize().unwrap();

        reconcile_audio_parts(&db_path, &[audio_dir]);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_audio_parts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "no row should be inserted for unknown sessions");
    }

    #[test]
    fn test_migration_v15_is_idempotent() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for m in migrations() {
            conn.execute_batch(m.sql)
                .unwrap_or_else(|e| panic!("migration v{} failed: {}", m.version, e));
        }
        conn.execute(
            "INSERT INTO sessions (id, title, source, status, wav_file_path, wav_duration_seconds) \
             VALUES ('s1', 't', 'MicOnly', 'completed', '/tmp/s1.mp3', 12.5)",
            [],
        )
        .unwrap();
        let v15 = &migrations()[14];
        assert_eq!(v15.version, 15, "index 14 must still be the v15 migration");
        let v15_sql = v15.sql;
        // Re-applying the migration must not error or double-insert.
        conn.execute_batch(v15_sql).unwrap();
        conn.execute_batch(v15_sql).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_audio_parts WHERE session_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "re-running v15 must not double-insert parts");
    }

    #[test]
    fn test_migration_v13_adds_tool_calls_column() {
        let m = migrations();
        let v13 = &m[12];
        assert_eq!(v13.version, 13);

        // Shape contract, not decoration: `run_migrations` only routes a migration
        // through `crsql_alter` when it is ONE `ALTER TABLE <sync_table>` statement.
        // Split v13 into a batch and a CRR DB desyncs its shadow tables (or, since
        // F3, fails the migration loudly).
        let stmt = v13.sql.trim().trim_end_matches(';').trim();
        assert!(
            !stmt.contains(';'),
            "v13 must stay a single statement so the crsql alter-wrap applies"
        );
        let target = stmt
            .strip_prefix("ALTER TABLE ")
            .and_then(|rest| rest.split_whitespace().next())
            .expect("v13 must be a single ALTER TABLE");
        assert_eq!(target, "chat_messages");
        #[cfg(feature = "sync")]
        assert!(
            yapstack_sync::schema::SYNC_TABLES.contains(&target),
            "chat_messages must stay a synced table or the alter-wrap does not apply"
        );

        // And the column actually lands on the migrated chain.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for mig in migrations() {
            conn.execute_batch(mig.sql)
                .unwrap_or_else(|e| panic!("migration v{} failed: {}", mig.version, e));
        }
        let has_col: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('chat_messages') WHERE name = 'tool_calls'",
                [],
                |_| Ok(()),
            )
            .is_ok();
        assert!(has_col, "v13 must add chat_messages.tool_calls");
    }

    #[test]
    fn test_all_migrations_execute_against_sqlite() {
        // Run the migration SQL against an in-memory rusqlite to catch
        // syntax errors (e.g. malformed FTS5 declarations) at test time
        // instead of waiting for a user to launch a fresh DB.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for m in migrations() {
            conn.execute_batch(m.sql)
                .unwrap_or_else(|e| panic!("migration v{} failed: {}", m.version, e));
        }
        // Sanity-check that the FTS5 tables are queryable post-migration.
        for fts_table in [
            "segments_fts",
            "notes_fts",
            "sessions_fts",
            "dictations_fts",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {fts_table}"), [], |row| {
                    row.get(0)
                })
                .unwrap_or_else(|e| panic!("query against {fts_table} failed: {e}"));
            assert_eq!(count, 0, "{fts_table} should be empty after migration");
        }
    }

    // --- Boot-sweep interim ownership gate (LIVE_SESSION_STATE.md slice 1) ---------

    /// Migrated in-memory DB (never CRR-prepared).
    fn migrated_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for m in migrations() {
            conn.execute_batch(m.sql)
                .unwrap_or_else(|e| panic!("migration v{} failed: {}", m.version, e));
        }
        conn
    }

    /// Simulate a DB that has been CRR-prepared by planting the `sessions` clock
    /// shadow table cr-sqlite leaves behind — no real crsql extension needed, matching
    /// the DB-level `is_crr(conn, "sessions")` probe the sweep uses.
    fn mark_crr_prepared(conn: &rusqlite::Connection) {
        conn.execute_batch("CREATE TABLE sessions__crsql_clock (x INTEGER);")
            .unwrap();
    }

    /// Insert a `status='recording'` session with a NULL `recording_device_id` whose
    /// `created_at` is the given SQL expression (e.g. `datetime('now')` or
    /// `datetime('now', '-1 hour')`).
    fn insert_recording(conn: &rusqlite::Connection, id: &str, created_at_sql: &str) {
        insert_recording_owned(conn, id, created_at_sql, None);
    }

    /// Insert a `status='recording'` session stamped with `owner` (`None` → NULL
    /// `recording_device_id`), used to drive the owner-scoped sweep matrix.
    fn insert_recording_owned(
        conn: &rusqlite::Connection,
        id: &str,
        created_at_sql: &str,
        owner: Option<&str>,
    ) {
        conn.execute(
            &format!(
                "INSERT INTO sessions \
                    (id, title, source, status, created_at, updated_at, recording_device_id) \
                 VALUES ('{id}', 't', 'MicOnly', 'recording', {created_at_sql}, {created_at_sql}, ?1)"
            ),
            rusqlite::params![owner],
        )
        .unwrap();
    }

    fn insert_segment(
        conn: &rusqlite::Connection,
        seg_id: &str,
        session_id: &str,
        created_at_sql: &str,
    ) {
        conn.execute(
            &format!(
                "INSERT INTO segments \
                    (id, session_id, source, text, audio_offset_seconds, chunk_duration_seconds, created_at) \
                 VALUES ('{seg_id}', '{session_id}', 'mic', 'hi', 0.0, 1.0, {created_at_sql})"
            ),
            [],
        )
        .unwrap();
    }

    fn session_status(conn: &rusqlite::Connection, id: &str) -> String {
        conn.query_row(
            "SELECT status FROM sessions WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn session_exists(conn: &rusqlite::Connection, id: &str) -> bool {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        n == 1
    }

    #[test]
    fn db_was_crr_prepared_detects_clock_shadow_table() {
        let conn = migrated_conn();
        assert!(
            !db_was_crr_prepared(&conn),
            "a freshly-migrated DB has never been CRR-prepared"
        );
        mark_crr_prepared(&conn);
        assert!(
            db_was_crr_prepared(&conn),
            "presence of sessions__crsql_clock means the DB was CRR-prepared"
        );
    }

    // --- Owner-scoped FINAL sweep rule matrix (LIVE_SESSION_STATE.md "Sweep rules —
    // final"): {owned, foreign, NULL} × {empty, has-segments} × {fresh, stale} ×
    // {sync-on, sync-off}. `me` = "A".

    const ME: &str = "AAAABBBBCCCCDDDD";
    const PEER: &str = "EEEEFFFFGGGGHHHH";

    #[test]
    fn final_foreign_rows_are_never_touched_sync_on() {
        // The P0 fix (D6): a peer's synced row — in EVERY shape — must never be
        // deleted or finalized, so no 'completed'/DELETE LWW-propagates onto the
        // recorder. Includes the stale-at-receiver-but-live-at-source case.
        let conn = migrated_conn();
        insert_recording_owned(&conn, "f-empty-fresh", "datetime('now')", Some(PEER));
        insert_recording_owned(
            &conn,
            "f-empty-stale",
            "datetime('now', '-1 hour')",
            Some(PEER),
        );
        insert_recording_owned(&conn, "f-full-fresh", "datetime('now')", Some(PEER));
        insert_segment(&conn, "sf1", "f-full-fresh", "datetime('now')");
        insert_recording_owned(
            &conn,
            "f-full-stale",
            "datetime('now', '-1 hour')",
            Some(PEER),
        );
        insert_segment(&conn, "sf2", "f-full-stale", "datetime('now', '-1 hour')");

        sweep_orphaned_recordings(&conn, Some(ME));

        for id in [
            "f-empty-fresh",
            "f-empty-stale",
            "f-full-fresh",
            "f-full-stale",
        ] {
            assert!(
                session_exists(&conn, id),
                "{id}: foreign row must not be deleted"
            );
            assert_eq!(
                session_status(&conn, id),
                "recording",
                "{id}: foreign row must not be finalized"
            );
        }
    }

    #[test]
    fn final_foreign_rows_are_never_touched_even_sync_off() {
        // A credential-clear over a retained CRR DB leaves `me = NULL` while foreign
        // synced rows remain: the not-foreign predicate still excludes them (`owner =
        // NULL` is SQL-unknown), so a stale-at-receiver peer row is never finalized —
        // "including when B's fingerprint is NULL" (spec Verification matrix).
        let conn = migrated_conn();
        insert_recording_owned(&conn, "peer-live", "datetime('now', '-1 hour')", Some(PEER));
        insert_segment(&conn, "sp", "peer-live", "datetime('now', '-1 hour')");

        sweep_orphaned_recordings(&conn, None);

        assert!(session_exists(&conn, "peer-live"));
        assert_eq!(session_status(&conn, "peer-live"), "recording");
    }

    #[test]
    fn final_owned_rows_finalize_regardless_of_stale_and_empties_delete() {
        let conn = migrated_conn();
        // Owned non-empty: finalize whether fresh (own crash at own boot) or stale.
        insert_recording_owned(&conn, "own-full-fresh", "datetime('now')", Some(ME));
        insert_segment(&conn, "of1", "own-full-fresh", "datetime('now')");
        insert_recording_owned(
            &conn,
            "own-full-stale",
            "datetime('now', '-1 hour')",
            Some(ME),
        );
        insert_segment(&conn, "of2", "own-full-stale", "datetime('now', '-1 hour')");
        // Owned empty: deleted (own empties, sync-on).
        insert_recording_owned(&conn, "own-empty", "datetime('now')", Some(ME));

        sweep_orphaned_recordings(&conn, Some(ME));

        assert_eq!(session_status(&conn, "own-full-fresh"), "completed");
        assert_eq!(session_status(&conn, "own-full-stale"), "completed");
        assert!(
            !session_exists(&conn, "own-empty"),
            "an owned empty recording row is delete-eligible on a sync-on device"
        );
    }

    #[test]
    fn final_null_owner_sync_on_never_deleted_finalized_only_when_stale() {
        let conn = migrated_conn();
        // NULL-owner empties on a SYNC-ON device are NEVER deleted (could be a peer's
        // legacy row whose DELETE would sync back) — but ARE finalized when stale.
        insert_recording(&conn, "null-empty-stale", "datetime('now', '-1 hour')");
        insert_recording(&conn, "null-empty-fresh", "datetime('now')");
        // NULL-owner non-empty: finalize only when stale (fresh could be a rollout
        // peer still recording with a NULL owner).
        insert_recording(&conn, "null-full-stale", "datetime('now', '-1 hour')");
        insert_segment(
            &conn,
            "nfs",
            "null-full-stale",
            "datetime('now', '-1 hour')",
        );
        insert_recording(&conn, "null-full-fresh", "datetime('now')");
        insert_segment(&conn, "nff", "null-full-fresh", "datetime('now')");

        sweep_orphaned_recordings(&conn, Some(ME));

        assert!(
            session_exists(&conn, "null-empty-stale"),
            "NULL-owner row on sync-on must never be DELETEd"
        );
        assert!(session_exists(&conn, "null-empty-fresh"));
        assert_eq!(session_status(&conn, "null-empty-stale"), "completed");
        assert_eq!(session_status(&conn, "null-empty-fresh"), "recording");
        assert_eq!(session_status(&conn, "null-full-stale"), "completed");
        assert_eq!(
            session_status(&conn, "null-full-fresh"),
            "recording",
            "a fresh NULL-owner row may be a not-yet-updated live peer"
        );
    }

    #[test]
    fn final_sync_off_deletes_empties_and_finalizes_stale() {
        // me = NULL: today's single-device semantics — delete empties, finalize stale
        // non-empties, leave fresh non-empties (a genuine in-flight recording).
        let conn = migrated_conn();
        insert_recording(&conn, "off-empty-stale", "datetime('now', '-1 hour')");
        insert_recording(&conn, "off-empty-fresh", "datetime('now')");
        insert_recording(&conn, "off-full-stale", "datetime('now', '-1 hour')");
        insert_segment(&conn, "ofs", "off-full-stale", "datetime('now', '-1 hour')");
        insert_recording(&conn, "off-full-fresh", "datetime('now')");
        insert_segment(&conn, "off", "off-full-fresh", "datetime('now')");

        sweep_orphaned_recordings(&conn, None);

        assert!(
            !session_exists(&conn, "off-empty-stale"),
            "sync-off empties delete"
        );
        assert!(
            !session_exists(&conn, "off-empty-fresh"),
            "sync-off empties delete"
        );
        assert_eq!(session_status(&conn, "off-full-stale"), "completed");
        assert_eq!(session_status(&conn, "off-full-fresh"), "recording");
    }

    #[test]
    fn close_orphaned_recordings_never_crr_runs_sync_off_rule() {
        // Production entry on a never-CRR DB: me is genuinely NULL → full sync-off rule.
        let conn = migrated_conn();
        insert_recording(&conn, "e", "datetime('now', '-1 hour')");
        insert_recording(&conn, "s", "datetime('now', '-1 hour')");
        insert_segment(&conn, "sseg", "s", "datetime('now', '-1 hour')");

        close_orphaned_recordings(&conn, None);

        assert!(
            !session_exists(&conn, "e"),
            "sync-off empty deleted at boot"
        );
        assert_eq!(session_status(&conn, "s"), "completed");
    }

    #[test]
    fn close_orphaned_recordings_crr_prepared_no_fingerprint_touches_nothing() {
        // Production entry on a CRR-prepared DB with `me = None` (sync unconfigured /
        // identity unloadable): ownership is ambiguous, so it must touch NOTHING
        // (foreign-safe; own-crash finalize deferred). Every shape stays untouched.
        let conn = migrated_conn();
        mark_crr_prepared(&conn);
        insert_recording_owned(&conn, "own-stale", "datetime('now', '-1 hour')", Some(ME));
        insert_segment(&conn, "os", "own-stale", "datetime('now', '-1 hour')");
        insert_recording_owned(
            &conn,
            "peer-stale",
            "datetime('now', '-1 hour')",
            Some(PEER),
        );
        insert_recording(&conn, "null-empty-stale", "datetime('now', '-1 hour')");

        close_orphaned_recordings(&conn, None);

        for id in ["own-stale", "peer-stale", "null-empty-stale"] {
            assert!(session_exists(&conn, id), "{id} must not be deleted");
            assert_eq!(
                session_status(&conn, id),
                "recording",
                "{id} must stay 'recording' on a CRR-prepared DB with no fingerprint"
            );
        }
    }

    #[test]
    fn close_orphaned_recordings_crr_prepared_with_fingerprint_runs_owner_scoped_rule() {
        // Production entry on a CRR-prepared DB with `me = Some(fp)`: the real owner-scoped
        // final rule now runs. Foreign rows stay untouched (D6); the device's OWN crashed
        // row finalizes; an owned empty deletes; a NULL-owner row finalizes only when stale.
        let conn = migrated_conn();
        mark_crr_prepared(&conn);
        // Own crashed non-empty → finalize.
        insert_recording_owned(&conn, "own-stale", "datetime('now', '-1 hour')", Some(ME));
        insert_segment(&conn, "os", "own-stale", "datetime('now', '-1 hour')");
        // Own empty → delete (sync-on own empties are delete-eligible).
        insert_recording_owned(&conn, "own-empty", "datetime('now')", Some(ME));
        // Peer's live-at-source row → never touched.
        insert_recording_owned(
            &conn,
            "peer-stale",
            "datetime('now', '-1 hour')",
            Some(PEER),
        );
        insert_segment(&conn, "ps", "peer-stale", "datetime('now', '-1 hour')");
        // NULL-owner stale → finalize; NULL-owner fresh → left (possible live rollout peer).
        insert_recording(&conn, "null-stale", "datetime('now', '-1 hour')");
        insert_segment(&conn, "ns", "null-stale", "datetime('now', '-1 hour')");
        insert_recording(&conn, "null-fresh", "datetime('now')");
        insert_segment(&conn, "nf", "null-fresh", "datetime('now')");

        close_orphaned_recordings(&conn, Some(ME));

        assert_eq!(
            session_status(&conn, "own-stale"),
            "completed",
            "own crashed row must finalize"
        );
        assert!(
            !session_exists(&conn, "own-empty"),
            "own empty recording row must delete on a sync-on device"
        );
        assert!(
            session_exists(&conn, "peer-stale"),
            "foreign row must never be deleted"
        );
        assert_eq!(
            session_status(&conn, "peer-stale"),
            "recording",
            "foreign row must never be finalized (D6)"
        );
        assert_eq!(session_status(&conn, "null-stale"), "completed");
        assert_eq!(
            session_status(&conn, "null-fresh"),
            "recording",
            "a fresh NULL-owner row may be a not-yet-updated live peer"
        );
    }

    #[test]
    fn test_migration_v12_creates_fts_tables() {
        assert_eq!(migrations()[11].version, 12);
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for m in migrations() {
            conn.execute_batch(m.sql)
                .unwrap_or_else(|e| panic!("migration v{} failed: {}", m.version, e));
        }
        // Rows written AFTER the chain only reach the mirrors through v12's
        // triggers, which `test_all_migrations_execute_against_sqlite` cannot see
        // (it only counts the empty tables) and which the CRR rebuild re-creates
        // from whatever triggers it finds.
        conn.execute_batch(
            "INSERT INTO sessions (id, title, source, status) \
                 VALUES ('s12', 'kickoff call', 'MicOnly', 'completed'); \
             INSERT INTO segments (id, session_id, source, text, audio_offset_seconds, chunk_duration_seconds) \
                 VALUES ('seg12', 's12', 'Mic', 'quarterly revenue', 0.0, 1.0); \
             INSERT INTO notes (id, session_id, content) VALUES ('n12', 's12', 'follow up thursday'); \
             INSERT INTO dictation_history (id, slot_id, slot_name, input_text, output_text, output_action) \
                 VALUES ('d12', 'slot', 'Slot', 'raw words', 'polished words', 'Clipboard');",
        )
        .unwrap();

        for (table, id_col, id, term) in [
            ("sessions_fts", "session_id", "s12", "kickoff"),
            ("segments_fts", "segment_id", "seg12", "revenue"),
            ("notes_fts", "note_id", "n12", "thursday"),
            ("dictations_fts", "dictation_id", "d12", "polished"),
        ] {
            let hit: String = conn
                .query_row(
                    &format!("SELECT {id_col} FROM {table} WHERE {table} MATCH ?"),
                    [term],
                    |r| r.get(0),
                )
                .unwrap_or_else(|e| panic!("{table} did not index the inserted row: {e}"));
            assert_eq!(hit, id, "{table} indexed the wrong row");
        }
    }
}
