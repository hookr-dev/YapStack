// SPDX-License-Identifier: AGPL-3.0-only
//! Per-connection initialization of the vendored cr-sqlite v0.16.3 extension (R6).
//!
//! `build.rs` statically links the extension with `-DSQLITE_CORE`
//! (`static,omit_load_extension`), so `sqlite3_crsqlite_init` treats its
//! `sqlite3_api_routines` pointer as a no-op (`SQLITE_EXTENSION_INIT2`).
//!
//! We initialize cr-sqlite **directly on the sync runtime's own connection** and
//! nowhere else. We deliberately do NOT use `sqlite3_auto_extension`: that is a
//! process-global hook SQLite runs on EVERY new connection in the process, which
//! would leak cr-sqlite onto the desktop app's main `tauri-plugin-sql` (sqlx)
//! pool. sqlx never calls `crsql_finalize()` before closing a pooled connection,
//! so cr-sqlite then aborts the whole process on close
//! ("unable to close due to unfinalized statements"). Per-connection init keeps
//! cr-sqlite confined to [`CrsqlDb`] and leaves every plain rusqlite/sqlx
//! connection untouched.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;

use rusqlite::Connection;

use crate::SyncError;

// `SQLITE_OK_LOAD_PERMANENTLY` (not exported by `rusqlite::ffi`): some extension
// entry points return this instead of `SQLITE_OK` to signal a permanent load.
// cr-sqlite's static init returns `SQLITE_OK`, but we accept both like SQLite.
const SQLITE_OK_LOAD_PERMANENTLY: i32 = 256;

// Provided by the statically-linked C shim (`crsqlite.c`, compiled with
// `-DSQLITE_CORE`). Signature is the standard SQLite extension entry point:
// `(sqlite3 *db, char **pzErrMsg, const sqlite3_api_routines *pApi)`.
extern "C" {
    fn sqlite3_crsqlite_init(db: *mut c_void, err: *mut *mut c_char, api: *const c_void) -> i32;
}

/// Retained for API compatibility. cr-sqlite is now initialized **per connection**
/// (see [`init_crsqlite_on`] / [`CrsqlDb::open`]); there is intentionally no
/// process-global registration. Callers holding a plain [`rusqlite::Connection`]
/// that need cr-sqlite must go through [`CrsqlDb`].
///
/// NOTE: retained as a no-op only so pre-existing callers/tests keep compiling; it
/// registers nothing. Do not add new callers.
pub fn register_crsqlite() {
    // No-op: process-global auto-extension registration was removed (T022) to
    // stop cr-sqlite from polluting the desktop app's sqlx connection pool.
}

/// How long a contended writer waits for the lock before giving up with `SQLITE_BUSY`. The
/// sync DB can be held by MORE than one connection at once — the drain thread plus any
/// capture/status path that opens the same file — so a brief overlap must serialize, not
/// error (T023 Judge). Paired with `BEGIN IMMEDIATE` on the write transactions
/// ([`crate::outbox::enqueue_local`]) so the write lock is taken up front rather than being
/// upgraded mid-transaction into a busy/PK-conflict on the `client_seq` counter.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Apply the shared connection pragmas every on-disk sync connection needs. Today that is a
/// generous `busy_timeout` (see [`BUSY_TIMEOUT`]). Crate-internal so the write path can prove
/// the contention behaviour in tests with a plain connection.
pub(crate) fn apply_sync_pragmas(conn: &Connection) -> Result<(), SyncError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

/// Initialize the statically-linked cr-sqlite extension on a single connection.
///
/// Calls `sqlite3_crsqlite_init` directly on `conn`'s underlying `sqlite3*`. Under
/// the `-DSQLITE_CORE` static build the `api` pointer is ignored, so we pass null.
fn init_crsqlite_on(conn: &Connection) -> Result<(), SyncError> {
    let mut errmsg: *mut c_char = ptr::null_mut();
    // SAFETY: `handle()` returns this connection's live `sqlite3*`; we only use it
    // for the duration of the call. `sqlite3_crsqlite_init` has the SQLite
    // extension-entry ABI and, under `-DSQLITE_CORE`, ignores the (null) api arg.
    let rc = unsafe {
        let handle = conn.handle();
        sqlite3_crsqlite_init(handle as *mut c_void, &mut errmsg, ptr::null())
    };

    let msg = if errmsg.is_null() {
        None
    } else {
        // SAFETY: cr-sqlite allocated `errmsg` with sqlite3_malloc; copy then free.
        let owned = unsafe { CStr::from_ptr(errmsg) }
            .to_string_lossy()
            .into_owned();
        unsafe { rusqlite::ffi::sqlite3_free(errmsg as *mut c_void) };
        Some(owned)
    };

    if rc != rusqlite::ffi::SQLITE_OK && rc != SQLITE_OK_LOAD_PERMANENTLY {
        return Err(SyncError::CrrUnavailable(format!(
            "sqlite3_crsqlite_init failed (rc={rc}){}",
            msg.map(|m| format!(": {m}")).unwrap_or_default()
        )));
    }
    Ok(())
}

/// A cr-sqlite-enabled connection. On drop it runs `crsql_finalize()` (required by
/// cr-sqlite to release its per-connection prepared statements before close).
pub struct CrsqlDb {
    conn: Connection,
}

impl CrsqlDb {
    /// Open an on-disk database and initialize cr-sqlite on it (only this
    /// connection gets the extension).
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self, SyncError> {
        let conn = Connection::open(path)?;
        apply_sync_pragmas(&conn)?;
        init_crsqlite_on(&conn)?;
        Self::confirm_crr(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory database (tests, ephemeral verification) and initialize
    /// cr-sqlite on it.
    pub fn open_in_memory() -> Result<Self, SyncError> {
        let conn = Connection::open_in_memory()?;
        init_crsqlite_on(&conn)?;
        Self::confirm_crr(&conn)?;
        Ok(Self { conn })
    }

    /// Prove the extension initialized on this connection: the `crsql_site_id()`
    /// scalar (16-byte site id) must be callable. Fails closed.
    fn confirm_crr(conn: &Connection) -> Result<(), SyncError> {
        let site: Vec<u8> = conn
            .query_row("SELECT crsql_site_id()", [], |r| r.get(0))
            .map_err(|e| SyncError::CrrUnavailable(e.to_string()))?;
        if site.len() != 16 {
            return Err(SyncError::CrrUnavailable(format!(
                "crsql_site_id returned {} bytes, expected 16",
                site.len()
            )));
        }
        Ok(())
    }

    /// This connection's 16-byte cr-sqlite site id.
    pub fn site_id(&self) -> Result<Vec<u8>, SyncError> {
        Ok(self
            .conn
            .query_row("SELECT crsql_site_id()", [], |r| r.get(0))?)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

impl Drop for CrsqlDb {
    fn drop(&mut self) {
        // Best-effort: cr-sqlite requires finalize before close.
        let _ = self.conn.execute_batch("SELECT crsql_finalize();");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolation proof (T022): a [`CrsqlDb`] connection has `crsql_as_crr`, but a
    /// PLAIN `rusqlite::Connection` opened in the SAME process does NOT — proving
    /// per-connection init leaks nothing globally (no `sqlite3_auto_extension`).
    #[test]
    fn crsqlite_is_per_connection_not_global() {
        // Sync connection: cr-sqlite must be present.
        let db = CrsqlDb::open_in_memory().expect("sync connection opens");
        db.conn()
            .execute_batch(
                "CREATE TABLE t(id TEXT NOT NULL PRIMARY KEY, v TEXT NOT NULL DEFAULT '');",
            )
            .unwrap();
        db.conn()
            .query_row("SELECT crsql_as_crr('t')", [], |_| Ok(()))
            .expect("crsql_as_crr available on the sync connection");

        // Plain connection in the same process: cr-sqlite must be ABSENT.
        let plain = Connection::open_in_memory().expect("plain connection opens");
        let err = plain
            .query_row("SELECT crsql_as_crr('t')", [], |_| Ok(()))
            .unwrap_err();
        assert!(
            err.to_string().contains("crsql_as_crr"),
            "expected 'no such function: crsql_as_crr' on a plain connection, got: {err}"
        );
        // And crsql_site_id must likewise be unavailable on the plain connection.
        assert!(
            plain
                .query_row("SELECT crsql_site_id()", [], |r| r.get::<_, Vec<u8>>(0))
                .is_err(),
            "crsql_site_id must NOT be callable on a plain connection"
        );
    }
}
