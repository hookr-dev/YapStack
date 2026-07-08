// SPDX-License-Identifier: AGPL-3.0-only
//! Repo-owned SQLite command backend for the app's live `yapstack.db` (Option A′
//! stage 2). Replaces `tauri-plugin-sql`'s sqlx backend with connections *we*
//! control, so the entire DB path runs through cr-sqlite-initialized connections
//! (per-connection init + finalize-on-drop). For A2 the served database is the
//! same plain (non-CRR) `yapstack.db` — behaviour is identical to the plugin.
//! cr-sqlite init on a plain DB is harmless and dormant until CRR tables exist
//! (A3), which is why we do it from day one: A3 becomes a file swap only.
//!
//! ## Pool (A1 spike verdict)
//! ONE writer connection behind a mutex (all writes serialize; writer-writer
//! `SQLITE_BUSY` impossible) plus a small ring of reader connections
//! round-robined for `select`. WAL + `busy_timeout` on every connection; every
//! SQLite call runs on a blocking thread (`spawn_blocking`).
//!
//! ## Read-your-writes
//! `execute` always uses the writer and fully commits (autocommit per command)
//! before its promise resolves; each `select` starts a fresh read transaction on
//! a reader, which under WAL observes the latest committed snapshot. Because a
//! command's write is committed before the next command runs, a read that follows
//! a write across commands sees it.
//!
//! ## Feature switch (no-sync build)
//! cr-sqlite lives in `yapstack-sync`, which is only a dependency under the
//! `sync` feature. When `sync` is OFF we open plain `rusqlite` connections —
//! behaviour is byte-identical, because the extension is only ever needed for
//! CRR tables, which do not exist until the A3 cutover (and cutover requires
//! sync). See [`ManagedConn`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{Connection, ToSql};
use serde_json::Value as JsonValue;
use tauri::State;

/// How long a contended connection waits before giving up with `SQLITE_BUSY`.
/// Matches the sync runtime's generous timeout so a brief writer/reader overlap
/// serializes rather than erroring.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// Number of reader connections. A small ring is plenty for a single-window
/// desktop app; the writer is separate so reads never block behind a write lock.
const READER_COUNT: usize = 4;

/// A managed on-disk SQLite connection.
///
/// Under the `sync` feature this is a [`yapstack_sync::CrsqlDb`] — cr-sqlite is
/// initialized on this connection only, and `crsql_finalize()` runs on drop.
/// Without the feature it is a plain [`rusqlite::Connection`]. Both apply the
/// same pragmas (WAL, foreign keys ON, busy_timeout) so behaviour is identical.
pub struct ManagedConn {
    #[cfg(feature = "sync")]
    inner: yapstack_sync::CrsqlDb,
    #[cfg(not(feature = "sync"))]
    inner: Connection,
}

impl ManagedConn {
    /// Open `path`, initialize cr-sqlite (sync feature only), and apply the
    /// shared pragmas.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        #[cfg(feature = "sync")]
        {
            // CrsqlDb::open already applies busy_timeout + inits cr-sqlite +
            // confirms CRR is callable. We additionally set WAL + foreign keys
            // to match the plugin's sqlx defaults.
            let db = yapstack_sync::CrsqlDb::open(path).map_err(|e| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                    Some(format!("cr-sqlite init on {}: {e}", path.display())),
                )
            })?;
            apply_pragmas(db.conn())?;
            Ok(Self { inner: db })
        }
        #[cfg(not(feature = "sync"))]
        {
            let conn = Connection::open(path)?;
            apply_pragmas(&conn)?;
            Ok(Self { inner: conn })
        }
    }

    /// Borrow the underlying rusqlite connection.
    pub fn conn(&self) -> &Connection {
        #[cfg(feature = "sync")]
        {
            self.inner.conn()
        }
        #[cfg(not(feature = "sync"))]
        {
            &self.inner
        }
    }
}

/// Apply the pragmas the plugin's sqlx pool applied implicitly:
/// - WAL journal mode (concurrent readers + a single writer),
/// - `foreign_keys = ON` (sqlx default; the app relies on `ON DELETE CASCADE`),
/// - a generous `busy_timeout`.
fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // journal_mode returns a row, so use execute_batch which ignores it.
    // foreign_keys must be set outside a transaction (no-op inside one).
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    Ok(())
}

/// Result of a `db_execute`, matching tauri-plugin-sql's `QueryResult` shape.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DbExecuteResult {
    pub rows_affected: u64,
    pub last_insert_id: i64,
}

/// One selected row as an ordered-by-name JSON object, matching the plugin.
pub type DbRow = serde_json::Map<String, JsonValue>;

/// One writer connection + a ring of reader connections, all open on the same
/// on-disk file. Held inside [`DbService`] behind an `RwLock<Option<Pool>>` so the
/// A3 CRR cutover can drop **every** connection (`None`) for the file swap and
/// reopen a fresh pool afterwards.
struct Pool {
    writer: Mutex<ManagedConn>,
    readers: Vec<Mutex<ManagedConn>>,
    next_reader: AtomicUsize,
}

impl Pool {
    /// Open the writer, run pending migrations on it BEFORE any reader is opened,
    /// then open the reader ring.
    fn open(path: &Path) -> rusqlite::Result<Self> {
        let writer = ManagedConn::open(path)?;
        // Migrations run first, on the writer, so readers open on the migrated
        // schema and the first command observes a fully-migrated DB.
        run_migrations(writer.conn())?;
        let mut readers = Vec::with_capacity(READER_COUNT);
        for _ in 0..READER_COUNT {
            readers.push(Mutex::new(ManagedConn::open(path)?));
        }
        Ok(Self {
            writer: Mutex::new(writer),
            readers,
            next_reader: AtomicUsize::new(0),
        })
    }

    fn execute(&self, query: &str, values: &[JsonValue]) -> rusqlite::Result<DbExecuteResult> {
        let guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn();
        let rows_affected =
            conn.execute(query, rusqlite::params_from_iter(values.iter().map(Bind)))?;
        let last_insert_id = conn.last_insert_rowid();
        Ok(DbExecuteResult {
            rows_affected: rows_affected as u64,
            last_insert_id,
        })
    }

    fn select(&self, query: &str, values: &[JsonValue]) -> rusqlite::Result<Vec<DbRow>> {
        let idx = self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        let guard = self.readers[idx].lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn();
        let mut stmt = conn.prepare(query)?;
        let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let col_count = col_names.len();
        let mut rows = stmt.query(rusqlite::params_from_iter(values.iter().map(Bind)))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut map = serde_json::Map::with_capacity(col_count);
            for (i, name) in col_names.iter().enumerate() {
                map.insert(name.clone(), value_ref_to_json(row.get_ref(i)?));
            }
            out.push(map);
        }
        Ok(out)
    }
}

/// The command backend: one writer + a ring of readers over `yapstack.db`.
///
/// ## Cutover-time quiesce (A3)
/// The pool is `RwLock<Option<Pool>>` plus a `swapping` fast-path flag. During the
/// enable-time CRR cutover the sync layer calls [`DbService::close_for_swap`]: it
/// raises `swapping` (so new `execute`/`select` fail-fast, never blocking), waits
/// for in-flight commands to finish (the write lock), checkpoints the writer, and
/// drops every connection so `crsql_finalize()` runs and the file can be renamed.
/// [`DbService::reopen`] rebuilds the pool on the swapped-in file. During the
/// (sub-second) window, `db_execute`/`db_select` return a retryable `SQLITE_BUSY`
/// (`DB_SWAP_IN_PROGRESS`); the frontend's Enable flow shows a "Preparing…" state
/// and the app's polling queries retry — we deliberately **fail-fast rather than
/// queue** (queueing across the swap risks holding connections we must drop).
pub struct DbService {
    /// The served file — read only by [`DbService::reopen`] (sync-only cutover), so
    /// it is otherwise dead in the no-sync build.
    #[cfg_attr(not(feature = "sync"), allow(dead_code))]
    path: PathBuf,
    /// Fast-path gate: set true for the swap window so callers fail-fast before ever
    /// contending on the pool lock (prevents reader-preferring `RwLock` starvation of
    /// the swap writer).
    swapping: AtomicBool,
    /// `None` only during the swap window (between `close_for_swap` and `reopen`).
    pool: RwLock<Option<Pool>>,
}

/// Managed Tauri state handle.
pub type DbServiceState = Arc<DbService>;

/// Retryable error surfaced to `db_execute`/`db_select` callers during the cutover
/// swap window. `SQLITE_BUSY` marks it retryable; the message is a stable sentinel
/// the frontend can recognize.
fn swap_in_progress_err() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some(
            "DB_SWAP_IN_PROGRESS: the database is briefly unavailable while sync is being \
             enabled; retry shortly"
                .to_string(),
        ),
    )
}

impl DbService {
    /// Open the pool for `path`, running migrations on the writer before serving.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let pool = Pool::open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            swapping: AtomicBool::new(false),
            pool: RwLock::new(Some(pool)),
        })
    }

    /// Run a write (INSERT/UPDATE/DELETE/DDL) on the serialized writer.
    pub fn execute(&self, query: &str, values: &[JsonValue]) -> rusqlite::Result<DbExecuteResult> {
        if self.swapping.load(Ordering::Acquire) {
            return Err(swap_in_progress_err());
        }
        let guard = self.pool.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(pool) => pool.execute(query, values),
            None => Err(swap_in_progress_err()),
        }
    }

    /// Run a read on the next reader connection (round-robin).
    pub fn select(&self, query: &str, values: &[JsonValue]) -> rusqlite::Result<Vec<DbRow>> {
        if self.swapping.load(Ordering::Acquire) {
            return Err(swap_in_progress_err());
        }
        let guard = self.pool.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(pool) => pool.select(query, values),
            None => Err(swap_in_progress_err()),
        }
    }

    /// Quiesce the pool for the A3 file swap: raise the fast-path gate, wait for
    /// in-flight commands (write lock), checkpoint the writer so the `-wal` merges
    /// into the main file, then drop every connection (`crsql_finalize()` runs on
    /// drop). After this returns, the on-disk file has no live handles and can be
    /// renamed. Pairs with [`DbService::reopen`].
    ///
    /// Only exists under the `sync` feature — the cutover it serves is sync-only.
    /// The short-lived per-call connections in [`crate::db`] (`open_managed`) are
    /// opened+dropped within a single function and hold no persistent handle across
    /// the window; they are not tracked here (a rare concurrent open during the
    /// sub-second window is best-effort and self-heals on retry).
    #[cfg(feature = "sync")]
    pub fn close_for_swap(&self) -> rusqlite::Result<()> {
        self.swapping.store(true, Ordering::Release);
        let mut guard = self.pool.write().unwrap_or_else(|e| e.into_inner());
        if let Some(pool) = guard.as_ref() {
            let w = pool.writer.lock().unwrap_or_else(|e| e.into_inner());
            // Best-effort: sidecars are also removed by the cutover after this returns.
            let _ = w.conn().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
        *guard = None; // drop Pool -> drop every ManagedConn -> crsql_finalize.
        Ok(())
    }

    /// Rebuild the pool on the (possibly swapped) file and lower the fast-path gate.
    /// Runs migrations again; on a CRR-prepared live DB every migration is already
    /// recorded so this is a no-op. Pairs with [`DbService::close_for_swap`].
    #[cfg(feature = "sync")]
    pub fn reopen(&self) -> rusqlite::Result<()> {
        let pool = Pool::open(&self.path)?;
        {
            let mut guard = self.pool.write().unwrap_or_else(|e| e.into_inner());
            *guard = Some(pool);
        }
        self.swapping.store(false, Ordering::Release);
        Ok(())
    }
}

/// Binds a JSON value as a SQLite parameter, matching tauri-plugin-sql's binding
/// exactly: null → NULL, string → TEXT, **any number → f64/REAL** (SQLite column
/// affinity converts to INTEGER for integer columns, as it did under the plugin).
/// Booleans and composite values are only defensive — db.ts never binds them.
struct Bind<'a>(&'a JsonValue);

impl ToSql for Bind<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let v = match self.0 {
            JsonValue::Null => SqlValue::Null,
            JsonValue::Bool(b) => SqlValue::Integer(i64::from(*b)),
            JsonValue::Number(n) => SqlValue::Real(n.as_f64().unwrap_or_default()),
            JsonValue::String(s) => SqlValue::Text(s.clone()),
            other @ (JsonValue::Array(_) | JsonValue::Object(_)) => {
                SqlValue::Text(serde_json::to_string(other).unwrap_or_default())
            }
        };
        Ok(ToSqlOutput::Owned(v))
    }
}

/// Maps a SQLite value to JSON by storage class, matching the plugin's decoder:
/// INTEGER → number(i64), REAL → number(f64), TEXT → string, BLOB → array of
/// byte-numbers, NULL → null.
fn value_ref_to_json(v: ValueRef<'_>) -> JsonValue {
    match v {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(i) => JsonValue::Number(i.into()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        ValueRef::Text(bytes) => JsonValue::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => JsonValue::Array(
            bytes
                .iter()
                .map(|b| JsonValue::Number((*b).into()))
                .collect(),
        ),
    }
}

/// Repo-owned migration runner. Preserves continuity with tauri-plugin-sql's
/// sqlx bookkeeping: the plugin recorded each applied migration in the
/// `_sqlx_migrations` table. We ensure that table exists, treat every version
/// already recorded there as applied (regardless of checksum), and run only the
/// missing ones — so a user's existing DB (the owner's) NEVER re-runs an old
/// migration. Newly-applied migrations are recorded in the same table shape so
/// the bookkeeping stays coherent. A migration that fails on a divergent dev DB
/// is logged and skipped rather than aborting startup (matching the app's
/// existing idempotent runtime-schema safety net); on a fresh DB all succeed.
///
/// Returns the list of versions applied by this call (empty when already
/// up-to-date).
///
/// ## Note for future migration authors (A3 CRR ownership)
/// After the enable-time cutover the live `yapstack.db` **is** the cr-sqlite CRR
/// database. A schema change to a synced table (see
/// [`yapstack_sync::schema::SYNC_TABLES`]) must NOT be a bare `ALTER TABLE`: that
/// desyncs the per-row clock/pks shadow tables cr-sqlite maintains. This runner
/// detects a CRR-prepared DB and automatically routes a **single-statement
/// `ALTER TABLE <sync_table> …`** migration through
/// [`yapstack_sync::schema::crsql_alter`] (`crsql_begin_alter`/`crsql_commit_alter`),
/// which rebuilds the shadow tables and backfills the new column at
/// `col_version=1`. What you must know:
///   * Keep a sync-table schema migration to ONE `ALTER TABLE` statement so the
///     auto-wrap applies; split multi-step changes across migration versions.
///   * `CREATE INDEX`/trigger changes and any migration touching a NON-sync table
///     are applied plainly (unchanged) — only sync-table `ALTER`s are wrapped.
///   * On a non-CRR DB (fresh install before cutover, or the `no-sync` build) every
///     migration is applied plainly, exactly as before.
pub fn run_migrations(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    run_migrations_list(conn, &crate::db::migrations())
}

/// Migration runner over an explicit list (so tests can drive the CRR-wrap path
/// with a synthetic migration). See [`run_migrations`].
pub fn run_migrations_list(
    conn: &Connection,
    migrations: &[crate::db::Migration],
) -> rusqlite::Result<Vec<i64>> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        );",
    )?;

    let applied: std::collections::HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT version FROM _sqlx_migrations")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let mut newly_applied = Vec::new();
    for m in migrations {
        if applied.contains(&m.version) {
            continue;
        }
        match apply_migration(conn, m) {
            Ok(()) => newly_applied.push(m.version),
            Err(e) => {
                tracing::error!(
                    "migration v{} ({}) failed; skipping (runtime schema patches will \
                     backfill if applicable): {e}",
                    m.version,
                    m.description
                );
            }
        }
    }
    Ok(newly_applied)
}

/// Insert the sqlx-compatible bookkeeping row for an applied migration. Empty
/// checksum: the runner never validates it (sqlx is gone) and dev DBs have
/// divergent history we must not re-check.
fn record_migration(
    conn: &Connection,
    m: &crate::db::Migration,
    elapsed_nanos: i64,
) -> rusqlite::Result<()> {
    let checksum: Vec<u8> = Vec::new();
    conn.execute(
        "INSERT INTO _sqlx_migrations
           (version, description, success, checksum, execution_time)
         VALUES (?, ?, 1, ?, ?)",
        rusqlite::params![m.version, m.description, checksum, elapsed_nanos],
    )?;
    Ok(())
}

/// Apply one migration and record its bookkeeping. On a CRR-prepared DB a single
/// `ALTER TABLE <sync_table>` is routed through cr-sqlite's alter-wrapping; every
/// other migration is applied plainly in a transaction with its bookkeeping row.
fn apply_migration(conn: &Connection, m: &crate::db::Migration) -> rusqlite::Result<()> {
    let start = std::time::Instant::now();

    #[cfg(feature = "sync")]
    {
        if let Some(table) = crr_alter_target(conn, m.sql)? {
            // CRR-prepared DB + a single ALTER on a CRR sync table: wrap in
            // crsql_begin_alter/crsql_commit_alter so the clock/pks shadow tables are
            // rebuilt and the new column backfills at col_version=1. crsql_alter runs
            // its own transaction, so the bookkeeping row is recorded immediately
            // after (autocommit). The only non-atomic window — the alter commits but
            // the (local, single-row) bookkeeping insert fails — is vanishingly
            // unlikely; on a re-run the ALTER would fail loudly (duplicate column)
            // rather than corrupt, and the column is already present.
            yapstack_sync::schema::crsql_alter(conn, &table, m.sql).map_err(sync_to_rusqlite)?;
            return record_migration(conn, m, start.elapsed().as_nanos() as i64);
        }
    }

    conn.execute_batch("BEGIN;")?;
    let res = conn
        .execute_batch(m.sql)
        .and_then(|()| record_migration(conn, m, start.elapsed().as_nanos() as i64));
    match res {
        Ok(()) => conn.execute_batch("COMMIT;"),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

/// If `conn` is a CRR-prepared DB and `sql` is a single `ALTER TABLE <t>` on a
/// synced table, return that table (routing it through `crsql_alter`). Otherwise
/// `None` (apply plainly).
#[cfg(feature = "sync")]
fn crr_alter_target(conn: &Connection, sql: &str) -> rusqlite::Result<Option<String>> {
    // "sessions" is CRRified together with every other sync table by crr_migrate,
    // so its clock table existing proves the DB is CRR-prepared.
    if !yapstack_sync::schema::is_crr(conn, "sessions").map_err(sync_to_rusqlite)? {
        return Ok(None);
    }
    Ok(parse_single_alter_table(sql)
        .filter(|t| yapstack_sync::schema::SYNC_TABLES.contains(&t.as_str())))
}

/// Extract the table name from a single `ALTER TABLE <name> …` statement, or
/// `None` if `sql` is not exactly one ALTER TABLE statement.
#[cfg(feature = "sync")]
fn parse_single_alter_table(sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.contains(';') {
        return None; // multi-statement batch: apply plainly
    }
    let mut it = trimmed.split_whitespace();
    if !it.next()?.eq_ignore_ascii_case("ALTER") {
        return None;
    }
    if !it.next()?.eq_ignore_ascii_case("TABLE") {
        return None;
    }
    let name = it
        .next()?
        .trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']' || c == '\'');
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Convert a `yapstack_sync::SyncError` into a `rusqlite::Error` so the CRR-wrapped
/// migration path fits the runner's `rusqlite::Result`.
#[cfg(feature = "sync")]
fn sync_to_rusqlite(e: yapstack_sync::SyncError) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
        Some(e.to_string()),
    )
}

/// Open a single managed connection for the ad-hoc Rust-side writers in
/// [`crate::db`] (audio-part inserts, runtime-schema patches, reconciliation).
/// Routing them through here NOW means they open cr-sqlite-initialized
/// connections under the `sync` feature, so the A3 CRR cutover does not have to
/// chase them: they will already be able to write CRR tables (and they finalize
/// cleanly on drop).
pub fn open_managed(path: &Path) -> rusqlite::Result<ManagedConn> {
    ManagedConn::open(path)
}

/// `db.execute(sql, params)` — write path. Rejects on SQL error so the
/// frontend's `.catch()` on idempotent runtime patches keeps working.
#[tauri::command]
#[specta::specta]
pub async fn db_execute(
    service: State<'_, DbServiceState>,
    query: String,
    values: Vec<JsonValue>,
) -> Result<DbExecuteResult, String> {
    let svc = service.inner().clone();
    tokio::task::spawn_blocking(move || svc.execute(&query, &values))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// `db.select(sql, params)` — read path. Returns rows as JSON objects.
#[tauri::command]
#[specta::specta]
pub async fn db_select(
    service: State<'_, DbServiceState>,
    query: String,
    values: Vec<JsonValue>,
) -> Result<Vec<DbRow>, String> {
    let svc = service.inner().clone();
    tokio::task::spawn_blocking(move || svc.select(&query, &values))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        (dir, path)
    }

    fn service_with_schema() -> (tempfile::TempDir, DbService) {
        let (dir, path) = temp_db();
        let svc = DbService::open(&path).expect("open service");
        (dir, svc)
    }

    #[test]
    fn migrations_apply_all_on_fresh_db() {
        let (_dir, path) = temp_db();
        let conn = Connection::open(&path).unwrap();
        let applied = run_migrations(&conn).unwrap();
        assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
        // Bookkeeping recorded.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _sqlx_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 15);
        // Core tables exist.
        for t in ["sessions", "segments", "notes", "chat_messages", "tags"] {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                    [t],
                    |_| Ok(()),
                )
                .is_ok();
            assert!(exists, "table {t} should exist after migration");
        }
    }

    #[test]
    fn migrations_are_idempotent_second_run_is_noop() {
        let (_dir, path) = temp_db();
        let conn = Connection::open(&path).unwrap();
        run_migrations(&conn).unwrap();
        let applied2 = run_migrations(&conn).unwrap();
        assert!(applied2.is_empty(), "second run must apply nothing");
    }

    #[test]
    fn migrations_respect_existing_sqlx_bookkeeping() {
        // Simulate a DB migrated by the OLD tauri-plugin-sql path: the sqlx
        // bookkeeping table records every version as applied, but we do NOT
        // create the actual tables. The runner must SKIP all of them (respecting
        // recorded state), proving a user's DB never re-runs old migrations.
        let (_dir, path) = temp_db();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            );",
        )
        .unwrap();
        for v in 1..=15i64 {
            conn.execute(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
                 VALUES (?, 'preexisting', 1, x'', 0)",
                [v],
            )
            .unwrap();
        }
        let applied = run_migrations(&conn).unwrap();
        assert!(applied.is_empty(), "recorded versions must not re-run");
        // Proof it truly skipped: sessions table was never created.
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions'",
                [],
                |_| Ok(()),
            )
            .is_ok();
        assert!(!exists, "skipped migrations must not have created tables");
    }

    #[test]
    fn migrations_apply_only_missing_versions() {
        let (_dir, path) = temp_db();
        let conn = Connection::open(&path).unwrap();
        // First run applies everything.
        run_migrations(&conn).unwrap();
        // Delete the record for v15 to simulate a partially-migrated DB; the
        // table stays (v15 is idempotent) so re-applying is safe.
        conn.execute("DELETE FROM _sqlx_migrations WHERE version = 15", [])
            .unwrap();
        let applied = run_migrations(&conn).unwrap();
        assert_eq!(applied, vec![15], "only the missing version re-runs");
    }

    #[test]
    fn execute_and_select_round_trip_with_param_binding() {
        let (_dir, svc) = service_with_schema();
        let res = svc
            .execute(
                "INSERT INTO sessions (id, title, source) VALUES ($1, $2, $3)",
                &[json!("s1"), json!("Title"), json!("Mic")],
            )
            .unwrap();
        assert_eq!(res.rows_affected, 1);

        let rows = svc
            .select(
                "SELECT id, title, total_segments FROM sessions WHERE id = $1",
                &[json!("s1")],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], json!("s1"));
        assert_eq!(rows[0]["title"], json!("Title"));
        // total_segments is INTEGER DEFAULT 0 → number, not float.
        assert_eq!(rows[0]["total_segments"], json!(0));
    }

    #[test]
    fn read_your_writes_across_writer_and_reader() {
        let (_dir, svc) = service_with_schema();
        svc.execute(
            "INSERT INTO sessions (id, source) VALUES ($1, $2)",
            &[json!("ryw"), json!("Mic")],
        )
        .unwrap();
        // Immediately readable from a reader connection (WAL + committed write).
        let rows = svc
            .select("SELECT id FROM sessions WHERE id = $1", &[json!("ryw")])
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn value_mapping_int_float_text_blob_null() {
        let (_dir, path) = temp_db();
        let svc = DbService::open(&path).unwrap();
        svc.execute(
            "CREATE TABLE vals (i INTEGER, f REAL, t TEXT, b BLOB, n TEXT)",
            &[],
        )
        .unwrap();
        // Blob inserted via literal (db.ts never binds blobs); NULL via literal.
        svc.execute(
            "INSERT INTO vals (i, f, t, b, n) VALUES (42, 3.5, 'hi', x'01020304', NULL)",
            &[],
        )
        .unwrap();
        let rows = svc.select("SELECT i, f, t, b, n FROM vals", &[]).unwrap();
        let r = &rows[0];
        assert_eq!(r["i"], json!(42));
        assert_eq!(r["f"], json!(3.5));
        assert_eq!(r["t"], json!("hi"));
        assert_eq!(r["b"], json!([1, 2, 3, 4]));
        assert_eq!(r["n"], JsonValue::Null);
    }

    #[test]
    fn number_param_binds_like_plugin_and_reads_back_by_affinity() {
        let (_dir, path) = temp_db();
        let svc = DbService::open(&path).unwrap();
        svc.execute("CREATE TABLE t (i INTEGER, r REAL)", &[])
            .unwrap();
        // Bound as f64 (plugin semantics); INTEGER affinity stores 7 as integer.
        svc.execute(
            "INSERT INTO t (i, r) VALUES ($1, $2)",
            &[json!(7), json!(2.5)],
        )
        .unwrap();
        let rows = svc.select("SELECT i, r FROM t", &[]).unwrap();
        assert_eq!(rows[0]["i"], json!(7)); // integer, not 7.0
        assert_eq!(rows[0]["r"], json!(2.5));
    }

    #[test]
    fn concurrent_smoke_no_busy_escape() {
        let (_dir, path) = temp_db();
        let svc = Arc::new(DbService::open(&path).unwrap());
        svc.execute(
            "INSERT INTO sessions (id, source) VALUES ('base', 'Mic')",
            &[],
        )
        .unwrap();
        let mut handles = Vec::new();
        for n in 0..8 {
            let svc = svc.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..50 {
                    let id = format!("c{n}-{i}");
                    svc.execute(
                        "INSERT INTO sessions (id, source) VALUES ($1, $2)",
                        &[json!(id), json!("Mic")],
                    )
                    .expect("write must not BUSY");
                    let _ = svc
                        .select("SELECT COUNT(*) AS c FROM sessions", &[])
                        .expect("read must not BUSY");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let rows = svc
            .select("SELECT COUNT(*) AS c FROM sessions", &[])
            .unwrap();
        assert_eq!(rows[0]["c"], json!(1 + 8 * 50));
    }

    /// A3 migration ownership: on a CRR-prepared DB, a single `ALTER TABLE
    /// <sync_table>` migration is routed through cr-sqlite's begin/commit_alter, so
    /// the alter succeeds, is recorded, AND the CRR clock shadow table survives (a
    /// bare ALTER would desync it). A non-sync-table migration is applied plainly.
    #[cfg(feature = "sync")]
    #[test]
    fn crr_prepared_alter_on_sync_table_routes_through_crsql_alter() {
        let (_dir, path) = temp_db();
        // Fresh CRR-capable connection; apply the real schema, then CRRify it.
        let db = yapstack_sync::CrsqlDb::open(&path).unwrap();
        let conn = db.conn();
        run_migrations(conn).unwrap();
        // Mirror the frontend db.ts out-of-band ALTERs (the columns rebuild_body
        // expects), skipping any already present in the migrated schema.
        for (table, col, coldef) in yapstack_sync::schema::OUT_OF_BAND_ALTERS {
            if !yapstack_sync::schema::column_exists(conn, table, col).unwrap() {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {coldef};"))
                    .unwrap();
            }
        }
        yapstack_sync::schema::crr_migrate(conn).unwrap();
        assert!(yapstack_sync::schema::is_crr(conn, "segments").unwrap());

        // A synthetic v900 migration: a single ALTER on a synced table.
        let alter = crate::db::Migration {
            version: 900,
            description: "test: add column to a CRR sync table",
            sql: "ALTER TABLE segments ADD COLUMN a3_test_col TEXT",
        };
        // A synthetic v901 migration: touches a NON-sync table (applied plainly).
        let plain = crate::db::Migration {
            version: 901,
            description: "test: create a local (non-sync) table",
            sql: "CREATE TABLE a3_local (id TEXT PRIMARY KEY)",
        };
        let applied = run_migrations_list(conn, &[alter, plain]).unwrap();
        assert_eq!(applied, vec![900, 901]);

        // The column landed and the CRR clock shadow table still exists (proof the
        // ALTER went through crsql_alter, not a bare ALTER that would drop the clock).
        assert!(yapstack_sync::schema::column_exists(conn, "segments", "a3_test_col").unwrap());
        assert!(
            yapstack_sync::schema::is_crr(conn, "segments").unwrap(),
            "CRR clock must survive the wrapped ALTER"
        );
        // A write to the altered CRR table still captures to crsql_changes.
        conn.execute(
            "INSERT INTO segments (id, session_id, a3_test_col) VALUES ('x', 's', 'v')",
            [],
        )
        .unwrap();
        let changes: i64 = conn
            .query_row("SELECT count(*) FROM crsql_changes", [], |r| r.get(0))
            .unwrap();
        assert!(changes > 0, "CRR clock must record the post-alter write");
        // The plain migration created the local table.
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='a3_local'",
                [],
                |_| Ok(()),
            )
            .is_ok();
        assert!(exists);
    }

    /// Post-swap re-entry: `reopen` rebuilds a working pool on the same file and
    /// clears the swap gate (queries fail-fast while the pool is closed).
    #[cfg(feature = "sync")]
    #[test]
    fn close_for_swap_then_reopen_restores_service() {
        let (_dir, svc) = service_with_schema();
        svc.execute("INSERT INTO sessions (id, source) VALUES ('a', 'Mic')", &[])
            .unwrap();
        svc.close_for_swap().unwrap();
        // Closed window: commands fail-fast with the retryable sentinel.
        let err = svc
            .select("SELECT 1", &[])
            .expect_err("closed pool must reject");
        assert!(err.to_string().contains("DB_SWAP_IN_PROGRESS"));
        svc.reopen().unwrap();
        let rows = svc.select("SELECT id FROM sessions", &[]).unwrap();
        assert_eq!(rows.len(), 1);
    }
}
