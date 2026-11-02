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
/// Connection-scoped session marker: the relay enforces the capability gate (O2, see
/// [`capability_gate_proven`]).
const K_SESSION_CAPABILITY_GATE_PROVEN: &str = "capability_gate_proven";
/// Connection-scoped session marker: the unacked outbox levels were re-checked (O1, see
/// [`outbox_levels_checked`]).
const K_SESSION_OUTBOX_LEVELS_CHECKED: &str = "outbox_levels_checked";

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
    session_marker(conn, K_SESSION_SEQ_RECONCILED)
}

/// Record that this connection has reconciled its counter against the relay tail this spawn
/// (G1). Set after a successful completeness comparison (whether or not a regression was
/// repaired). Connection-scoped / in-memory only (see [`seq_reconciled`]).
pub fn mark_seq_reconciled(conn: &Connection) -> Result<(), SyncError> {
    set_session_marker(conn, K_SESSION_SEQ_RECONCILED)
}

fn session_marker(conn: &Connection, key: &str) -> Result<bool, SyncError> {
    ensure_session_table(conn)?;
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM temp._yapstack_sync_session WHERE key=?1",
        [key],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn set_session_marker(conn: &Connection, key: &str) -> Result<(), SyncError> {
    ensure_session_table(conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO _yapstack_sync_session(key) VALUES(?1)",
        [key],
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

/// True once THIS drain connection has seen proof that the relay enforces the sync capability
/// gate (ADR 0006 §5.4, O2): a raise that left the floor at or above the asked level, a raise
/// refused naming blockers, a 409 `sync_capability_floor`, or a 426. A relay that predates the
/// gate ignores a batch's declared level, so no batch above level 0 is sent until this holds.
///
/// Connection-scoped like [`seq_reconciled`], so every drain spawn (app launch, re-login, an
/// account switch) proves it again against the relay it is talking to now. It is also dropped
/// when the relay was unreachable or its event stream ended ([`forget_capability_gate`]).
pub fn capability_gate_proven(conn: &Connection) -> Result<bool, SyncError> {
    session_marker(conn, K_SESSION_CAPABILITY_GATE_PROVEN)
}

/// Record that this connection has seen the relay enforce the capability gate.
pub fn mark_capability_gate_proven(conn: &Connection) -> Result<(), SyncError> {
    set_session_marker(conn, K_SESSION_CAPABILITY_GATE_PROVEN)
}

/// True once THIS connection has re-checked the levels stored on the unacked outbox entries
/// against the current table levels (O1, `outbox::restamp_unacked_levels`). Connection-scoped
/// like [`seq_reconciled`], so an upgrade or a restored outbox is re-checked at the next spawn.
pub fn outbox_levels_checked(conn: &Connection) -> Result<bool, SyncError> {
    session_marker(conn, K_SESSION_OUTBOX_LEVELS_CHECKED)
}

/// Record that this connection has re-checked the outbox levels.
pub fn mark_outbox_levels_checked(conn: &Connection) -> Result<(), SyncError> {
    set_session_marker(conn, K_SESSION_OUTBOX_LEVELS_CHECKED)
}

/// The first wait after a refused floor raise, doubled per further refusal up to
/// [`RAISE_BACKOFF_MAX_MS`] (O3).
pub const RAISE_BACKOFF_INITIAL_MS: i64 = 60_000;
/// The longest wait between floor raises while they keep being refused.
pub const RAISE_BACKOFF_MAX_MS: i64 = 15 * 60_000;

/// Why a floor raise was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RaiseRefusal {
    /// The relay named devices below the level (409 `sync_capability_blocked`).
    Blocked(Vec<yapstack_common::sync::CapabilityBlocker>),
    /// The relay has no raise route: it predates the gate (404/405).
    GateMissing,
}

/// A refused floor raise this connection is still waiting out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedRaise {
    pub level: i32,
    pub refusal: RaiseRefusal,
}

impl RefusedRaise {
    /// The push error this refusal holds a push with.
    #[must_use]
    pub fn into_error(self) -> SyncError {
        match self.refusal {
            RaiseRefusal::Blocked(blockers) => SyncError::CapabilityBlocked {
                level: self.level,
                blockers,
            },
            RaiseRefusal::GateMissing => SyncError::CapabilityGateMissing { level: self.level },
        }
    }

    fn kind(&self) -> &'static str {
        match self.refusal {
            RaiseRefusal::Blocked(_) => "blocked",
            RaiseRefusal::GateMissing => "gate_missing",
        }
    }
}

/// The connection-scoped TEMP table holding the raise backoff (at most one row). TEMP for the
/// same reason as [`seq_reconciled`]: a respawn starts over and asks the relay again.
/// `required`/`requested` are the relay's levels from the first pull after the refusal.
fn ensure_raise_backoff_table(conn: &Connection) -> Result<(), SyncError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _yapstack_sync_raise_backoff (\
            id INTEGER PRIMARY KEY CHECK (id = 0), \
            level INTEGER NOT NULL, \
            kind TEXT NOT NULL, \
            blockers TEXT NOT NULL, \
            retry_at_ms INTEGER NOT NULL, \
            delay_ms INTEGER NOT NULL, \
            required INTEGER, \
            requested INTEGER);",
    )?;
    Ok(())
}

/// The refused raise to report instead of asking the relay again, while `now_ms` is inside its
/// backoff. `None` once the wait is over (or never started). A clock that jumped back by more
/// than the wait ends it early rather than stretching it.
pub fn raise_backoff(conn: &Connection, now_ms: i64) -> Result<Option<RefusedRaise>, SyncError> {
    ensure_raise_backoff_table(conn)?;
    let row = conn.query_row(
        "SELECT level, kind, blockers, retry_at_ms, delay_ms \
         FROM temp._yapstack_sync_raise_backoff",
        [],
        |r| {
            Ok((
                r.get::<_, i32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        },
    );
    let (level, kind, blockers, retry_at, delay) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if now_ms >= retry_at || retry_at - now_ms > delay {
        return Ok(None);
    }
    let refusal = if kind == "gate_missing" {
        RaiseRefusal::GateMissing
    } else {
        match serde_json::from_str(&blockers) {
            Ok(b) => RaiseRefusal::Blocked(b),
            Err(_) => return Ok(None),
        }
    };
    Ok(Some(RefusedRaise { level, refusal }))
}

/// Start (or lengthen) the backoff after the relay refused a raise: 60 s after the first
/// refusal, doubling per refusal of the same kind at the same level, at most 15 min.
/// `now_ms` is the moment the refusal arrived.
pub fn record_refused_raise(
    conn: &Connection,
    refused: &RefusedRaise,
    now_ms: i64,
) -> Result<(), SyncError> {
    ensure_raise_backoff_table(conn)?;
    let previous: Option<i64> = conn
        .query_row(
            "SELECT delay_ms FROM temp._yapstack_sync_raise_backoff \
             WHERE level = ?1 AND kind = ?2",
            rusqlite::params![refused.level, refused.kind()],
            |r| r.get(0),
        )
        .ok();
    let delay = previous.map_or(RAISE_BACKOFF_INITIAL_MS, |d| {
        (d * 2).min(RAISE_BACKOFF_MAX_MS)
    });
    let blockers = match &refused.refusal {
        RaiseRefusal::Blocked(b) => serde_json::to_string(b)
            .map_err(|e| SyncError::Codec(format!("capability blockers: {e}")))?,
        RaiseRefusal::GateMissing => "[]".to_string(),
    };
    conn.execute(
        "INSERT INTO temp._yapstack_sync_raise_backoff \
         (id, level, kind, blockers, retry_at_ms, delay_ms) VALUES (0, ?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(id) DO UPDATE SET level = excluded.level, kind = excluded.kind, \
         blockers = excluded.blockers, retry_at_ms = excluded.retry_at_ms, \
         delay_ms = excluded.delay_ms",
        rusqlite::params![
            refused.level,
            refused.kind(),
            blockers,
            now_ms + delay,
            delay
        ],
    )?;
    Ok(())
}

/// Forget the raise backoff, so the next gated batch asks the relay again: after a raise
/// succeeds, and on the events that may have unblocked it (every blocker updated or gone from
/// the roster, a device removed here).
pub fn clear_raise_backoff(conn: &Connection) -> Result<(), SyncError> {
    ensure_raise_backoff_table(conn)?;
    conn.execute("DELETE FROM temp._yapstack_sync_raise_backoff", [])?;
    Ok(())
}

/// Feed the relay's levels from a pull into the backoff. The first levels seen during a
/// backoff are remembered; the backoff ends when either later differs (another device raised
/// or asked for a level, so the blockers may have changed) or the floor already reaches the
/// refused level. Ending it only lets the next gated batch ask again; the relay still decides.
pub fn note_relay_capability(
    conn: &Connection,
    levels: yapstack_common::sync::SyncCapabilityLevels,
) -> Result<(), SyncError> {
    ensure_raise_backoff_table(conn)?;
    let (required, requested) = (
        levels.required_sync_capability,
        levels.requested_sync_capability,
    );
    conn.execute(
        "DELETE FROM temp._yapstack_sync_raise_backoff \
         WHERE ?1 >= level \
            OR (required IS NOT NULL AND required <> ?1) \
            OR (requested IS NOT NULL AND requested <> ?2)",
        [required, requested],
    )?;
    conn.execute(
        "UPDATE temp._yapstack_sync_raise_backoff SET required = ?1, requested = ?2 \
         WHERE required IS NULL",
        [required, requested],
    )?;
    Ok(())
}

/// Forget what this connection learned about the relay's gate: the proof, and a cached "no
/// gate" refusal. For when the relay may have been replaced at the same address (an
/// unreachable relay, a dropped event stream): the next gated batch proves the gate again.
/// A cached blocker refusal is kept; its own signals end it.
pub fn forget_capability_gate(conn: &Connection) -> Result<(), SyncError> {
    ensure_session_table(conn)?;
    conn.execute(
        "DELETE FROM _yapstack_sync_session WHERE key=?1",
        [K_SESSION_CAPABILITY_GATE_PROVEN],
    )?;
    ensure_raise_backoff_table(conn)?;
    conn.execute(
        "DELETE FROM temp._yapstack_sync_raise_backoff WHERE kind = 'gate_missing'",
        [],
    )?;
    Ok(())
}

fn relay_capability_key(workspace: Uuid) -> String {
    format!("relay_capability:{workspace}")
}

fn relay_min_client_version_key(workspace: Uuid) -> String {
    format!("relay_min_client_version:{workspace}")
}

/// The last sync capability levels the relay returned for `workspace` (ADR 0006 §5.4,
/// §5.6), or `None` before the first pull. Keyed by workspace id, so an account switch
/// starts again from nothing. UX and feature gating only: it NEVER authorizes a push, which
/// the relay checks against the floor it holds right then.
///
/// # Errors
/// Propagates any sqlite error.
pub fn relay_capability(
    conn: &Connection,
    workspace: Uuid,
) -> Result<Option<yapstack_common::sync::SyncCapabilityLevels>, SyncError> {
    Ok(
        get_meta(conn, &relay_capability_key(workspace))?.and_then(|v| {
            let (required, requested) = v.split_once(',')?;
            Some(yapstack_common::sync::SyncCapabilityLevels {
                required_sync_capability: required.parse().ok()?,
                requested_sync_capability: requested.parse().ok()?,
            })
        }),
    )
}

/// Persist the levels a pull returned, writing only when they changed. Returns whether it
/// wrote. A lower value (a restored relay) is recorded as-is.
///
/// # Errors
/// Propagates any sqlite error.
pub fn record_relay_capability(
    conn: &Connection,
    workspace: Uuid,
    levels: yapstack_common::sync::SyncCapabilityLevels,
) -> Result<bool, SyncError> {
    if relay_capability(conn, workspace)? == Some(levels) {
        return Ok(false);
    }
    set_meta(
        conn,
        &relay_capability_key(workspace),
        &format!(
            "{},{}",
            levels.required_sync_capability, levels.requested_sync_capability
        ),
    )?;
    Ok(true)
}

/// The relay's `min_client_version` as last seen for `workspace` (ADR 0006 §5.9), so the
/// advisory survives a restart while the relay is unreachable.
///
/// # Errors
/// Propagates any sqlite error.
pub fn relay_min_client_version(
    conn: &Connection,
    workspace: Uuid,
) -> Result<Option<String>, SyncError> {
    get_meta(conn, &relay_min_client_version_key(workspace))
}

/// Forget the relay's `min_client_version` for `workspace` (the relay stopped publishing
/// one), so a restart doesn't resurrect a stale advisory.
///
/// # Errors
/// Propagates any sqlite error.
pub fn clear_relay_min_client_version(conn: &Connection, workspace: Uuid) -> Result<(), SyncError> {
    ensure_meta_table(conn)?;
    conn.execute(
        "DELETE FROM _yapstack_sync_meta WHERE key=?1",
        [relay_min_client_version_key(workspace)],
    )?;
    Ok(())
}

/// Persist the relay's `min_client_version` for `workspace`.
///
/// # Errors
/// Propagates any sqlite error.
pub fn set_relay_min_client_version(
    conn: &Connection,
    workspace: Uuid,
    min: &str,
) -> Result<(), SyncError> {
    set_meta(conn, &relay_min_client_version_key(workspace), min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yapstack_common::sync::SyncCapabilityLevels;

    #[test]
    fn relay_capability_is_persisted_per_workspace_and_written_only_on_change() {
        let conn = Connection::open_in_memory().unwrap();
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        assert_eq!(relay_capability(&conn, a).unwrap(), None);
        let l = SyncCapabilityLevels {
            required_sync_capability: 1,
            requested_sync_capability: 2,
        };
        assert!(record_relay_capability(&conn, a, l).unwrap(), "first write");
        assert!(
            !record_relay_capability(&conn, a, l).unwrap(),
            "unchanged: no write"
        );
        assert_eq!(relay_capability(&conn, a).unwrap(), Some(l));
        assert_eq!(
            relay_capability(&conn, b).unwrap(),
            None,
            "another workspace (an account switch) starts from nothing"
        );
        // A relay restored below the old floor is recorded as-is: this copy is UX only.
        let lower = SyncCapabilityLevels {
            required_sync_capability: 0,
            requested_sync_capability: 0,
        };
        assert!(record_relay_capability(&conn, a, lower).unwrap());
        assert_eq!(relay_capability(&conn, a).unwrap(), Some(lower));
    }

    #[test]
    fn relay_min_client_version_is_persisted_per_workspace() {
        let conn = Connection::open_in_memory().unwrap();
        let a = Uuid::from_u128(1);
        assert_eq!(relay_min_client_version(&conn, a).unwrap(), None);
        set_relay_min_client_version(&conn, a, "1.0.0-alpha.12").unwrap();
        assert_eq!(
            relay_min_client_version(&conn, a).unwrap().as_deref(),
            Some("1.0.0-alpha.12")
        );
        assert_eq!(
            relay_min_client_version(&conn, Uuid::from_u128(2)).unwrap(),
            None
        );
        clear_relay_min_client_version(&conn, a).unwrap();
        assert_eq!(relay_min_client_version(&conn, a).unwrap(), None);
    }
}
