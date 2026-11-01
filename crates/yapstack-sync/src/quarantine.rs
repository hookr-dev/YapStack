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
//! exactly as if the change had arrived after the migration (SYNC_SPEC §6.4, §12 I14).
//!
//! This is the load-bearing case the T003 spike deferred; the crate test
//! `quarantine_replay_across_schema_gap` exercises A-has-column / B-lacks-column →
//! B quarantines, migrates, replays, converges.
//!
//! The same holds one level up (ADR 0006 §4, SYNC_SPEC §12 I-NT1): a peer on a newer build
//! can send rows for a whole synced TABLE this device doesn't have yet, or has but hasn't
//! converted to a CRR. cr-sqlite rejects any row, sentinels included, for a table it has no
//! CRR schema for, which used to fail the whole changeset and stall the pull watermark
//! forever. Those rows are stashed the same way and replay once the table is a CRR here.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::change::{apply_change, ChangeRow, Changeset, SENTINEL_CID};
use crate::schema::{column_exists, is_crr};
use crate::SyncError;
use yapstack_common::write_turnstile::WriteTurnstile;

/// Create the local quarantine table. Prefixed `_yapstack_` and never CRRified, so
/// it never syncs.
fn ensure_pending_table(conn: &Connection) -> Result<(), SyncError> {
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

/// `schema::is_crr` per table, cached for one merge or one replay pass so a changeset of many
/// rows checks `sqlite_master` once per table.
#[derive(Default)]
struct CrrCache(HashMap<String, bool>);

impl CrrCache {
    fn is_crr(&mut self, conn: &Connection, table: &str) -> Result<bool, SyncError> {
        if let Some(&known) = self.0.get(table) {
            return Ok(known);
        }
        let known = is_crr(conn, table)?;
        self.0.insert(table.to_string(), known);
        Ok(known)
    }
}

/// True if this row can't be applied here YET: its table isn't a CRR locally (absent, or
/// present but not converted), which covers its sentinel rows too, or its column doesn't exist
/// locally. Such a row waits in the stash for the app update that adds the table or column.
fn is_deferrable(conn: &Connection, crr: &mut CrrCache, r: &ChangeRow) -> Result<bool, SyncError> {
    if !crr.is_crr(conn, &r.table)? {
        return Ok(true);
    }
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

/// Apply a change, or quarantine it if its table or column isn't ready locally. Returns true
/// if the change was quarantined (deferred), false if applied.
fn apply_or_quarantine(
    conn: &Connection,
    crr: &mut CrrCache,
    r: &ChangeRow,
) -> Result<bool, SyncError> {
    if is_deferrable(conn, crr, r)? {
        stash(conn, r)?;
        Ok(true)
    } else {
        apply_change(conn, r)?;
        Ok(false)
    }
}

/// Apply a whole changeset (pull path), quarantining any rows whose table or column isn't
/// ready locally. Runs in one transaction. Returns (applied, quarantined) counts. A row that
/// is ready but still fails to apply fails the whole changeset: that's a real fault, not a
/// schema gap, so it surfaces as the pull's error rather than being skipped.
///
/// `BEGIN IMMEDIATE`, not a deferred `BEGIN`. The body READS (`is_deferrable` inspects
/// the local schema) before it WRITES, so a deferred transaction has to upgrade read→write;
/// in WAL a lost upgrade race returns `SQLITE_BUSY_SNAPSHOT`, which the busy handler does
/// NOT retry — it fails outright regardless of `busy_timeout` (same rationale as
/// `schema::alter_dance` and `outbox::enqueue_local`). Taking the write lock up front lets
/// `busy_timeout` serialize this merge against the app's writer instead of failing the whole
/// changeset under boot-time contention.
///
/// The merge takes the process [`WriteTurnstile`] turn around its transaction, so a catch-up
/// running merges back to back hands the lock to any in-process writer already waiting
/// instead of out-racing it in SQLite's busy handler. Must not be called inside an open
/// transaction or while holding a turn.
pub fn merge_changeset(conn: &Connection, cs: &Changeset) -> Result<(usize, usize), SyncError> {
    ensure_pending_table(conn)?;
    let _turn = WriteTurnstile::process().acquire();
    // `new_unchecked` because we hold only a `&Connection` here, not `&mut`.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let mut crr = CrrCache::default();
    let mut applied = 0usize;
    let mut quarantined = 0usize;
    for r in &cs.rows {
        if apply_or_quarantine(&tx, &mut crr, r)? {
            quarantined += 1;
        } else {
            applied += 1;
        }
    }
    tx.commit()?;
    Ok((applied, quarantined))
}

/// What one [`replay_pending`] pass did with the stash.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReplayReport {
    /// Stashed rows applied (and removed from the stash) this pass.
    pub replayed: usize,
    /// Rows still waiting because their table or column isn't ready locally yet, i.e. they
    /// wait for an app update.
    pub waiting: usize,
    /// Rows whose table and column are ready but whose apply failed. They stay stashed and
    /// are retried next pass.
    pub failed: usize,
    /// Rows skipped this pass because an earlier row for the same `(table, pk)` failed, so a
    /// later change can never land over a failed one (a column row over a failed delete
    /// sentinel would resurrect the row). They stay stashed.
    pub blocked: usize,
}

/// Replay every stashed change whose table and column are now ready locally, deleting the
/// ones that apply. Idempotent; safe to call after any schema migration.
///
/// One row per write transaction, in stash (`id`, i.e. arrival) order, each under its own
/// [`WriteTurnstile`] turn taken before `BEGIN IMMEDIATE`. A row that fails rolls back alone
/// and stays stashed, so one bad row never strands the rest; every later row for the same
/// `(table, pk)` is skipped for the rest of the pass. `BEGIN IMMEDIATE` for the same reason
/// as [`merge_changeset`]: the apply reads before it writes, and a deferred transaction's
/// lost read→write upgrade surfaces as `SQLITE_BUSY_SNAPSHOT`, which `busy_timeout` cannot
/// retry. Only a local DB fault (opening a transaction, deleting a replayed row) is an `Err`.
/// Must not be called inside an open transaction or while holding a turn.
pub fn replay_pending(conn: &Connection) -> Result<ReplayReport, SyncError> {
    ensure_pending_table(conn)?;
    let candidates: Vec<(i64, Vec<u8>)> = {
        let mut stmt =
            conn.prepare("SELECT id, row_blob FROM _yapstack_pending_changes ORDER BY id")?;
        let v = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        v
    };

    let mut report = ReplayReport::default();
    let mut crr = CrrCache::default();
    let mut failed_keys: HashSet<(String, Vec<u8>)> = HashSet::new();
    for (id, blob) in &candidates {
        let Some(r) = Changeset::decode(blob)?.rows.into_iter().next() else {
            // malformed/empty — drop it, do not wedge the queue.
            let _turn = WriteTurnstile::process().acquire();
            conn.execute("DELETE FROM _yapstack_pending_changes WHERE id=?1", [id])?;
            continue;
        };
        let key = (r.table.clone(), r.pk.clone());
        if failed_keys.contains(&key) {
            report.blocked += 1;
            continue;
        }
        // A plain read, so a stash of rows still waiting for an update costs no write lock.
        if is_deferrable(conn, &mut crr, &r)? {
            report.waiting += 1;
            continue;
        }
        let _turn = WriteTurnstile::process().acquire();
        // `new_unchecked` because we hold only a `&Connection` here, not `&mut`.
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        if let Err(e) = apply_change(&tx, &r) {
            // Dropping `tx` rolls back this row only. Table name and error, never values; the
            // pass's totals reach the drain's own log line and lane status.
            tracing::debug!(
                target: "yapstack::sync",
                table = %r.table,
                error = %e,
                "stashed change failed to apply; kept for the next pass"
            );
            report.failed += 1;
            failed_keys.insert(key);
            continue;
        }
        tx.execute("DELETE FROM _yapstack_pending_changes WHERE id=?1", [id])?;
        tx.commit()?;
        report.replayed += 1;
    }
    Ok(report)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::read_local_changes_since;
    use crate::CrsqlDb;

    const T_NEW: &str =
        "CREATE TABLE t_new(id TEXT NOT NULL PRIMARY KEY, v TEXT NOT NULL DEFAULT '');";

    fn crr(db: &CrsqlDb, create: &str, table: &str) {
        db.conn().execute_batch(create).unwrap();
        db.conn()
            .query_row(&format!("SELECT crsql_as_crr('{table}')"), [], |_| Ok(()))
            .unwrap();
    }

    /// A peer that has `t_new` as a CRR and has written to it: an insert that stays, and an
    /// insert then delete, so the changeset carries a delete sentinel.
    fn peer_with_t_new_rows() -> CrsqlDb {
        let a = CrsqlDb::open_in_memory().unwrap();
        crr(&a, T_NEW, "t_new");
        a.conn()
            .execute_batch(
                "INSERT INTO t_new(id,v) VALUES('n1','kept'); \
                 INSERT INTO t_new(id,v) VALUES('n2','gone'); DELETE FROM t_new WHERE id='n2';",
            )
            .unwrap();
        a
    }

    fn t_new_rows(conn: &Connection) -> Vec<(String, String)> {
        let mut st = conn.prepare("SELECT id, v FROM t_new ORDER BY id").unwrap();
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        v
    }

    fn kept_only() -> Vec<(String, String)> {
        vec![("n1".to_string(), "kept".to_string())]
    }

    /// A row cr-sqlite rejects even though its table and column exist: a site id longer
    /// than cr-sqlite allows.
    fn unappliable(mut r: ChangeRow) -> ChangeRow {
        r.site_id = Some(vec![7u8; 40]);
        r
    }

    #[test]
    fn non_crr_table_column_row_quarantines() {
        // The table exists here but isn't converted yet (a crash between CREATE and
        // CRR-ify). Its column rows used to pass the column check and fail the changeset.
        let a = peer_with_t_new_rows();
        let b = CrsqlDb::open_in_memory().unwrap();
        b.conn().execute_batch(T_NEW).unwrap();
        let cs = read_local_changes_since(a.conn(), 0).unwrap();
        let (applied, quarantined) = merge_changeset(b.conn(), &cs).unwrap();
        assert_eq!((applied, quarantined), (0, cs.rows.len()));

        b.conn()
            .query_row("SELECT crsql_as_crr('t_new')", [], |_| Ok(()))
            .unwrap();
        let report = replay_pending(b.conn()).unwrap();
        assert_eq!(report.replayed, cs.rows.len());
        assert_eq!(pending_count(b.conn()).unwrap(), 0);
        assert_eq!(t_new_rows(b.conn()), kept_only());
    }

    #[test]
    fn replay_after_table_becomes_crr() {
        // B lacks the table entirely. Every row, sentinels included, waits; replay passes
        // before the update keep waiting; after the update the replay converges with A,
        // and the deleted row stays deleted.
        let a = peer_with_t_new_rows();
        let b = CrsqlDb::open_in_memory().unwrap();
        let cs = read_local_changes_since(a.conn(), 0).unwrap();
        assert!(
            cs.rows.iter().any(|r| r.cid == SENTINEL_CID),
            "precondition: the changeset carries sentinel rows"
        );
        let (applied, quarantined) = merge_changeset(b.conn(), &cs).unwrap();
        assert_eq!((applied, quarantined), (0, cs.rows.len()));
        let waiting = replay_pending(b.conn()).unwrap();
        assert_eq!(
            waiting,
            ReplayReport {
                waiting: cs.rows.len(),
                ..Default::default()
            }
        );

        crr(&b, T_NEW, "t_new");
        let report = replay_pending(b.conn()).unwrap();
        assert_eq!(report.replayed, cs.rows.len());
        assert_eq!(pending_count(b.conn()).unwrap(), 0);
        assert_eq!(t_new_rows(b.conn()), kept_only());
        assert_eq!(t_new_rows(a.conn()), kept_only());
    }

    #[test]
    fn merge_still_fails_on_a_ready_row_that_cannot_apply() {
        // Only "not ready here yet" is deferred. A row whose table and column exist but that
        // cr-sqlite rejects is a real fault and fails the changeset (it surfaces as the
        // pull's error), never skipped.
        let a = CrsqlDb::open_in_memory().unwrap();
        crr(&a, T_NEW, "t_new");
        a.conn()
            .execute("INSERT INTO t_new(id,v) VALUES('n1','x')", [])
            .unwrap();
        let b = CrsqlDb::open_in_memory().unwrap();
        crr(&b, T_NEW, "t_new");
        let mut cs = read_local_changes_since(a.conn(), 0).unwrap();
        cs.rows[0] = unappliable(cs.rows[0].clone());
        assert!(merge_changeset(b.conn(), &cs).is_err());
        assert_eq!(pending_count(b.conn()).unwrap(), 0);
    }

    #[test]
    fn replay_one_bad_row_keeps_the_rest() {
        // One stashed row fails to apply; before FP1 that aborted the whole replay
        // transaction and every other stashed row stayed stranded with it.
        let a = peer_with_t_new_rows();
        let b = CrsqlDb::open_in_memory().unwrap();
        crr(&b, T_NEW, "t_new");
        let cs = read_local_changes_since(a.conn(), 0).unwrap();
        // The bad row is for a pk of its own, so nothing is blocked behind it.
        let other = CrsqlDb::open_in_memory().unwrap();
        crr(&other, T_NEW, "t_new");
        other
            .conn()
            .execute("INSERT INTO t_new(id,v) VALUES('zz','bad')", [])
            .unwrap();
        let bad = unappliable(read_local_changes_since(other.conn(), 0).unwrap().rows[0].clone());
        ensure_pending_table(b.conn()).unwrap();
        stash(b.conn(), &bad).unwrap();
        for r in &cs.rows {
            stash(b.conn(), r).unwrap();
        }

        let report = replay_pending(b.conn()).expect("a failing row must not fail the pass");
        assert_eq!(report.failed, 1);
        assert_eq!(report.replayed, cs.rows.len());
        assert_eq!(
            pending_count(b.conn()).unwrap(),
            1,
            "the bad row stays stashed"
        );
        assert_eq!(t_new_rows(b.conn()), kept_only());
    }

    #[test]
    fn replay_failed_delete_sentinel_blocks_later_rows_for_that_pk() {
        // A delete sentinel for n1 fails; the column rows for n1 stashed after it must not
        // apply over it (that would resurrect the row), while rows for other pks still do.
        let a = CrsqlDb::open_in_memory().unwrap();
        crr(&a, T_NEW, "t_new");
        a.conn()
            .execute_batch("INSERT INTO t_new(id,v) VALUES('n1','one'), ('n2','two');")
            .unwrap();
        let cs = read_local_changes_since(a.conn(), 0).unwrap();
        let n1_pk = cs
            .rows
            .iter()
            .find(|r| r.cid == "v" && r.val == rusqlite::types::Value::Text("one".into()))
            .unwrap()
            .pk
            .clone();
        let mut delete = cs.rows.iter().find(|r| r.pk == n1_pk).unwrap().clone();
        delete.cid = SENTINEL_CID.into();
        delete.val = rusqlite::types::Value::Null;
        delete.cl = 2;
        delete.col_version = 2;

        let b = CrsqlDb::open_in_memory().unwrap();
        crr(&b, T_NEW, "t_new");
        ensure_pending_table(b.conn()).unwrap();
        stash(b.conn(), &unappliable(delete)).unwrap();
        for r in &cs.rows {
            stash(b.conn(), r).unwrap();
        }
        let n1_rows = cs.rows.iter().filter(|r| r.pk == n1_pk).count();

        let report = replay_pending(b.conn()).unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(report.blocked, n1_rows);
        assert_eq!(report.replayed, cs.rows.len() - n1_rows);
        assert_eq!(
            t_new_rows(b.conn()),
            vec![("n2".to_string(), "two".to_string())],
            "n1 is held behind its failed delete; n2 applied"
        );
        assert_eq!(pending_count(b.conn()).unwrap(), (1 + n1_rows) as i64);
    }

    #[test]
    fn cross_connection_replay_sees_a_table_converted_elsewhere() {
        // The drain's long-lived connection stashes rows for t_new, then a different
        // connection (the app's migration runner) creates and converts the table. The drain
        // connection's replay must see the new CRR (cr-sqlite caches table info per
        // connection) and land the rows.
        let a = peer_with_t_new_rows();
        let dir = std::env::temp_dir().join(format!("yapstack-fp1-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.db");
        let drain = CrsqlDb::open(&path).unwrap();
        drain
            .conn()
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
        let cs = read_local_changes_since(a.conn(), 0).unwrap();
        merge_changeset(drain.conn(), &cs).unwrap();
        assert_eq!(replay_pending(drain.conn()).unwrap().waiting, cs.rows.len());

        {
            let migration = CrsqlDb::open(&path).unwrap();
            crr(&migration, T_NEW, "t_new");
        }

        let report = replay_pending(drain.conn()).unwrap();
        assert_eq!(report.replayed, cs.rows.len());
        assert_eq!(t_new_rows(drain.conn()), kept_only());
        drop(drain);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
