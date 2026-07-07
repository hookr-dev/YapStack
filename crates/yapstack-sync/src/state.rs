// SPDX-License-Identifier: AGPL-3.0-only
//! Local, non-synced sync state: `client_id`, watermarks, and the `client_seq`
//! counter/contiguity tracking (advisory A3).
//!
//! Stored in `_yapstack_sync_meta` (never CRRified). `client_id` is a fresh random
//! UUID v4 generated ONCE per install and persisted (architecture: fresh client_id
//! per install). The push watermark is this device's last-pushed local `db_version`;
//! the pull watermark is the last-seen server `changeset_seq`.

use rusqlite::Connection;
use uuid::Uuid;

use crate::SyncError;

const K_CLIENT_ID: &str = "client_id";
const K_PULL_WM: &str = "pull_watermark"; // last-seen changeset_seq
const K_PUSH_WM: &str = "push_watermark"; // last-pushed local db_version
const K_CLIENT_SEQ: &str = "client_seq"; // our monotonic outgoing counter

pub fn ensure_meta_table(conn: &Connection) -> Result<(), SyncError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _yapstack_sync_meta (\
            key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    Ok(())
}

fn get_str(conn: &Connection, key: &str) -> Result<Option<String>, SyncError> {
    let v = conn
        .query_row(
            "SELECT value FROM _yapstack_sync_meta WHERE key=?1",
            [key],
            |r| r.get::<_, String>(0),
        )
        .ok();
    Ok(v)
}

fn set_str(conn: &Connection, key: &str, value: &str) -> Result<(), SyncError> {
    conn.execute(
        "INSERT INTO _yapstack_sync_meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

fn get_i64(conn: &Connection, key: &str) -> Result<i64, SyncError> {
    Ok(get_str(conn, key)?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0))
}

fn set_i64(conn: &Connection, key: &str, value: i64) -> Result<(), SyncError> {
    set_str(conn, key, &value.to_string())
}

/// This install's `client_id`, generating and persisting a fresh UUID v4 on first
/// call.
pub fn client_id(conn: &Connection) -> Result<Uuid, SyncError> {
    ensure_meta_table(conn)?;
    if let Some(s) = get_str(conn, K_CLIENT_ID)? {
        if let Ok(id) = Uuid::parse_str(&s) {
            return Ok(id);
        }
    }
    let id = Uuid::new_v4();
    set_str(conn, K_CLIENT_ID, &id.to_string())?;
    Ok(id)
}

pub fn pull_watermark(conn: &Connection) -> Result<i64, SyncError> {
    get_i64(conn, K_PULL_WM)
}
pub fn set_pull_watermark(conn: &Connection, v: i64) -> Result<(), SyncError> {
    set_i64(conn, K_PULL_WM, v)
}
pub fn push_watermark(conn: &Connection) -> Result<i64, SyncError> {
    get_i64(conn, K_PUSH_WM)
}
pub fn set_push_watermark(conn: &Connection, v: i64) -> Result<(), SyncError> {
    set_i64(conn, K_PUSH_WM, v)
}

/// Our current outgoing `client_seq` high-water mark (last assigned).
pub fn client_seq(conn: &Connection) -> Result<i64, SyncError> {
    get_i64(conn, K_CLIENT_SEQ)
}

/// Allocate the next contiguous `client_seq` (advisory A3: contiguity is guaranteed
/// by this monotonic +1 allocation; a gap in what the server reports for our
/// `client_id` then proves a dropped tail).
pub fn next_client_seq(conn: &Connection) -> Result<i64, SyncError> {
    let next = client_seq(conn)? + 1;
    set_i64(conn, K_CLIENT_SEQ, next)?;
    Ok(next)
}
