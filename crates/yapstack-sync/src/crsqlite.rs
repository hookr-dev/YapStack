// SPDX-License-Identifier: AGPL-3.0-only
//! Static registration of the vendored cr-sqlite v0.16.3 extension (R6).
//!
//! `build.rs` statically links the extension. Here we register it with SQLite via
//! `sqlite3_auto_extension` — a process-global hook SQLite runs on EVERY new
//! connection. This is the load-extension-free path the desktop stack requires
//! (`tauri-plugin-sql` / `rusqlite` disable `load_extension`).

use std::ffi::c_void;
use std::sync::Once;

use rusqlite::Connection;

use crate::SyncError;

// Provided by the statically-linked C shim (`crsqlite.c`, compiled with
// `-DSQLITE_CORE`) and `libsqlite3-sys` (bundled SQLite) respectively.
extern "C" {
    fn sqlite3_crsqlite_init(db: *mut c_void, err: *mut *mut i8, api: *const c_void) -> i32;
    fn sqlite3_auto_extension(entry: Option<unsafe extern "C" fn()>) -> i32;
}

static REGISTER: Once = Once::new();

/// Register cr-sqlite as an auto-extension exactly once for this process. Idempotent
/// and thread-safe. After this call every `rusqlite::Connection::open*` has the
/// `crsql_*` functions and the `crsql_changes` virtual table available.
pub fn register_crsqlite() {
    REGISTER.call_once(|| {
        // SAFETY: `sqlite3_crsqlite_init` has the SQLite extension-entry ABI;
        // `sqlite3_auto_extension` takes an untyped fn pointer. cr-sqlite's own
        // static builds register it exactly this way (`core_init.c`).
        unsafe {
            let entry: unsafe extern "C" fn() =
                std::mem::transmute(sqlite3_crsqlite_init as *const ());
            let rc = sqlite3_auto_extension(Some(entry));
            debug_assert_eq!(rc, 0, "sqlite3_auto_extension failed");
        }
    });
}

/// A cr-sqlite-enabled connection. On drop it runs `crsql_finalize()` (required by
/// cr-sqlite to release its per-connection prepared statements before close).
pub struct CrsqlDb {
    conn: Connection,
}

impl CrsqlDb {
    /// Open (registering the extension first) an on-disk database.
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self, SyncError> {
        register_crsqlite();
        let conn = Connection::open(path)?;
        Self::confirm_crr(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory database (tests, ephemeral verification).
    pub fn open_in_memory() -> Result<Self, SyncError> {
        register_crsqlite();
        let conn = Connection::open_in_memory()?;
        Self::confirm_crr(&conn)?;
        Ok(Self { conn })
    }

    /// Prove the statically-linked extension registered on this connection: the
    /// `crsql_site_id()` scalar (16-byte site id) must be callable. Fails closed.
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
