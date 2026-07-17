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

/// Connection-scoped session marker: `seq_reconciled` (G1). This lives in a SQLite TEMP
/// table so it is held in memory for the lifetime of the open connection ONLY and is never
/// written to the DB file (see [`seq_reconciled`]).
const K_SESSION_SEQ_RECONCILED: &str = "seq_reconciled";

pub fn ensure_meta_table(conn: &Connection) -> Result<(), SyncError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _yapstack_sync_meta (\
            key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    Ok(())
}

/// Read an arbitrary meta value (ensures the table first). Public seam for sibling modules
/// (e.g. the audio backfill-walk completion marker).
///
/// # Errors
/// Propagates any sqlite error.
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, SyncError> {
    ensure_meta_table(conn)?;
    get_str(conn, key)
}

/// Write an arbitrary meta value (ensures the table first).
///
/// # Errors
/// Propagates any sqlite error.
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), SyncError> {
    ensure_meta_table(conn)?;
    set_str(conn, key, value)
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

/// Force the `client_seq` high-water mark to `v` (last-assigned). Used ONLY by the G1
/// reseed path after a DB restore is detected: advancing the counter to the relay's max
/// for this device means the next [`next_client_seq`] mints `v + 1`, past every seq the
/// relay already holds — so we stop re-minting the colliding pairs the relay would dedup
/// (the silent-loss bug). Never used to REWIND the counter; the reseed caller only ever
/// raises it to the server tail.
pub fn set_client_seq(conn: &Connection, v: i64) -> Result<(), SyncError> {
    set_i64(conn, K_CLIENT_SEQ, v)
}

/// Allocate the next contiguous `client_seq` (advisory A3: contiguity is guaranteed
/// by this monotonic +1 allocation; a gap in what the server reports for our
/// `client_id` then proves a dropped tail).
pub fn next_client_seq(conn: &Connection) -> Result<i64, SyncError> {
    let next = client_seq(conn)? + 1;
    set_i64(conn, K_CLIENT_SEQ, next)?;
    Ok(next)
}

/// The connection-scoped TEMP table holding per-spawn drain markers. A `TEMP` table lives
/// only in the connection's private temp storage: it is created fresh on first use and
/// dropped when the connection closes, and it is NEVER written to the main DB file.
fn ensure_session_table(conn: &Connection) -> Result<(), SyncError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _yapstack_sync_session (key TEXT PRIMARY KEY);",
    )?;
    Ok(())
}

/// True once THIS drain connection has reconciled its `client_seq` counter against the relay
/// tail at least once this spawn (G1).
///
/// The marker is stored ONLY in the connection-scoped TEMP session table, deliberately NOT in
/// the persisted meta table. That is the whole point: a persisted flag survives a file-level
/// DB restore alongside the (now stale) counter it guards, so the drain would skip reconcile
/// in exactly the scenario reconcile exists for. A TEMP marker cannot be restored from a
/// backup — a fresh drain spawn (app launch / enable / re-login) opens a NEW connection with
/// no marker, so reconcile runs exactly ONCE per spawn and every restore self-heals. One extra
/// `GET /sync/completeness` per app session is the whole cost.
pub fn seq_reconciled(conn: &Connection) -> Result<bool, SyncError> {
    ensure_session_table(conn)?;
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM temp._yapstack_sync_session WHERE key=?1",
        [K_SESSION_SEQ_RECONCILED],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Record that this connection has reconciled its counter against the relay tail this spawn
/// (G1). Set after a successful completeness comparison (whether or not a regression was
/// repaired). Connection-scoped / in-memory only (see [`seq_reconciled`]).
pub fn mark_seq_reconciled(conn: &Connection) -> Result<(), SyncError> {
    ensure_session_table(conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO _yapstack_sync_session(key) VALUES(?1)",
        [K_SESSION_SEQ_RECONCILED],
    )?;
    Ok(())
}

/// Clear the per-spawn reconciled marker (G3): a relay 409 conflict proves this connection's
/// counter collided with the relay tail, so the next push must re-reconcile / reseed before it
/// can trust the counter again. Connection-scoped / in-memory only.
pub fn clear_seq_reconciled(conn: &Connection) -> Result<(), SyncError> {
    ensure_session_table(conn)?;
    conn.execute(
        "DELETE FROM _yapstack_sync_session WHERE key=?1",
        [K_SESSION_SEQ_RECONCILED],
    )?;
    Ok(())
}
