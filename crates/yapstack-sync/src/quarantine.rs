// SPDX-License-Identifier: AGPL-3.0-only
//! R7: cross-schema-gap quarantine + replay.
//!
//! When a peer at schema version N+1 sends a change for a column this device (still
//! at version N) does not yet have, we CANNOT apply it (`INSERT INTO crsql_changes`
//! for an unknown `cid` would fail or silently drop). Instead we STASH the change —
//! with its `col_version`/`site_id`/`seq` clock intact — in a local, non-synced
//! `_yapstack_pending_changes` table. After the device runs the matching schema
//! migration (`schema::crsql_alter`, which adds the column and rebuilds the clock),
//! we REPLAY the stashed change. Because the clock is byte-preserved, LWW converges
//! exactly as if the change had arrived after the migration (architecture §6.3/§15).
//!
//! This is the load-bearing case the T003 spike deferred; the crate test
//! `quarantine_replay_across_schema_gap` exercises A-has-column / B-lacks-column →
//! B quarantines, migrates, replays, converges.

use rusqlite::Connection;

use crate::change::{apply_change, ChangeRow, Changeset, SENTINEL_CID};
use crate::schema::column_exists;
use crate::SyncError;

/// Create the local quarantine table. Prefixed `_yapstack_` and never CRRified, so
/// it never syncs.
pub fn ensure_pending_table(conn: &Connection) -> Result<(), SyncError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _yapstack_pending_changes (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            tbl TEXT NOT NULL, \
            cid TEXT NOT NULL, \
            row_blob BLOB NOT NULL, \
            created_at TEXT NOT NULL DEFAULT (datetime('now'))\
         );\
         CREATE INDEX IF NOT EXISTS _yapstack_pending_tbl_cid \
            ON _yapstack_pending_changes(tbl, cid);",
    )?;
    Ok(())
}

/// True if this row targets a column that does not exist locally yet (and is not the
/// row-level sentinel, which always applies).
fn is_unknown_column(conn: &Connection, r: &ChangeRow) -> Result<bool, SyncError> {
    if r.cid == SENTINEL_CID {
        return Ok(false);
    }
    Ok(!column_exists(conn, &r.table, &r.cid)?)
}

/// Stash one change row (clock preserved) for later replay.
fn stash(conn: &Connection, r: &ChangeRow) -> Result<(), SyncError> {
    let blob = Changeset {
        rows: vec![r.clone()],
    }
    .encode();
    conn.execute(
        "INSERT INTO _yapstack_pending_changes (tbl, cid, row_blob) VALUES (?1,?2,?3)",
        rusqlite::params![r.table, r.cid, blob],
    )?;
    Ok(())
}

/// Apply a change, or quarantine it if its column is unknown locally. Returns true
/// if the change was quarantined (deferred), false if applied.
pub fn apply_or_quarantine(conn: &Connection, r: &ChangeRow) -> Result<bool, SyncError> {
    if is_unknown_column(conn, r)? {
        stash(conn, r)?;
        Ok(true)
    } else {
        apply_change(conn, r)?;
        Ok(false)
    }
}

/// Apply a whole changeset (pull path), quarantining any rows for unknown columns.
/// Runs in one transaction. Returns (applied, quarantined) counts.
pub fn merge_changeset(conn: &Connection, cs: &Changeset) -> Result<(usize, usize), SyncError> {
    ensure_pending_table(conn)?;
    conn.execute_batch("BEGIN;")?;
    let mut applied = 0usize;
    let mut quarantined = 0usize;
    let mut inner = || -> Result<(), SyncError> {
        for r in &cs.rows {
            if apply_or_quarantine(conn, r)? {
                quarantined += 1;
            } else {
                applied += 1;
            }
        }
        Ok(())
    };
    if let Err(e) = inner() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(e);
    }
    conn.execute_batch("COMMIT;")?;
    Ok((applied, quarantined))
}

/// Replay every stashed change whose column now exists locally, deleting the ones
/// that apply. Idempotent; safe to call after any schema migration. Returns the
/// number of changes replayed.
pub fn replay_pending(conn: &Connection) -> Result<usize, SyncError> {
    ensure_pending_table(conn)?;
    let candidates: Vec<(i64, Vec<u8>)> = {
        let mut stmt =
            conn.prepare("SELECT id, row_blob FROM _yapstack_pending_changes ORDER BY id")?;
        let v = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        v
    };

    conn.execute_batch("BEGIN;")?;
    let mut replayed = 0usize;
    let mut inner = || -> Result<(), SyncError> {
        for (id, blob) in &candidates {
            let cs = Changeset::decode(blob)?;
            let Some(r) = cs.rows.first() else {
                // malformed/empty — drop it, do not wedge the queue.
                conn.execute("DELETE FROM _yapstack_pending_changes WHERE id=?1", [id])?;
                continue;
            };
            if is_unknown_column(conn, r)? {
                continue; // still gapped; keep for a later migration.
            }
            apply_change(conn, r)?;
            conn.execute("DELETE FROM _yapstack_pending_changes WHERE id=?1", [id])?;
            replayed += 1;
        }
        Ok(())
    };
    if let Err(e) = inner() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(e);
    }
    conn.execute_batch("COMMIT;")?;
    Ok(replayed)
}

/// Count of changes currently quarantined (diagnostics / completeness).
pub fn pending_count(conn: &Connection) -> Result<i64, SyncError> {
    ensure_pending_table(conn)?;
    Ok(
        conn.query_row("SELECT count(*) FROM _yapstack_pending_changes", [], |r| {
            r.get(0)
        })?,
    )
}
