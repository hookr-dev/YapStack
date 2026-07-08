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

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

/// The command backend: one writer + a ring of readers over `yapstack.db`.
pub struct DbService {
    writer: Mutex<ManagedConn>,
    readers: Vec<Mutex<ManagedConn>>,
    next_reader: AtomicUsize,
}

/// Managed Tauri state handle.
pub type DbServiceState = Arc<DbService>;

impl DbService {
    /// Open the pool for `path`, run pending migrations on the writer BEFORE any
    /// reader is opened or any command can be served, then open the readers.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
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

    /// Run a write (INSERT/UPDATE/DELETE/DDL) on the serialized writer.
    pub fn execute(&self, query: &str, values: &[JsonValue]) -> rusqlite::Result<DbExecuteResult> {
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

    /// Run a read on the next reader connection (round-robin).
    pub fn select(&self, query: &str, values: &[JsonValue]) -> rusqlite::Result<Vec<DbRow>> {
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
pub fn run_migrations(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
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
    for m in crate::db::migrations() {
        if applied.contains(&m.version) {
            continue;
        }
        let start = std::time::Instant::now();
        conn.execute_batch("BEGIN;")?;
        match conn.execute_batch(m.sql) {
            Ok(()) => {
                let elapsed = start.elapsed().as_nanos() as i64;
                // Empty checksum: the runner never validates it (sqlx is gone),
                // and dev DBs have divergent history we must not re-check.
                let checksum: Vec<u8> = Vec::new();
                conn.execute(
                    "INSERT INTO _sqlx_migrations
                       (version, description, success, checksum, execution_time)
                     VALUES (?, ?, 1, ?, ?)",
                    rusqlite::params![m.version, m.description, checksum, elapsed],
                )?;
                conn.execute_batch("COMMIT;")?;
                newly_applied.push(m.version);
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
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
}
