// SPDX-License-Identifier: AGPL-3.0-only
//! R1: two-populated-devices reconciliation — the owner's real day-one path.
//!
//! ## The trap this module exists to avoid
//! Two devices each hold an independently-populated, independently-evolved copy of the
//! library (the owner's Mac and Windows). The NAIVE path — CRRify BOTH databases
//! locally and merge their `crsql_changes` — is silently lossy: each device stamps
//! every one of its rows with `col_version = 1` and its OWN `site_id`, so for a row
//! that exists on both sides the LWW tiebreak is decided by `site_id` ordering, NOT by
//! which edit is newer. One side's genuine edits vanish with no record. Rows that were
//! assigned different PKs for the same logical entity duplicate. This is unacceptable —
//! it is the owner's real data (T004 R1).
//!
//! ## The device-role model (owner decision: MAC SEEDS)
//! Exactly one device is the [`DeviceRole::Seed`]: it holds the primary DB, CRRifies it
//! on a copy ([`crate::schema::crr_migrate`]), and publishes it as a snapshot
//! ([`crate::snapshot`]). Every other device is a [`DeviceRole::Join`]: it does NOT
//! CRRify-and-merge its own DB. It **re-bootstraps** from the seed's snapshot into a
//! fresh CRR base, then RECONCILES its own local-only rows into that base at the app
//! layer with dedup — matching on stable identity, inserting genuinely-local rows as
//! new writes, and **surfacing** any ambiguous collision rather than silently dropping.
//!
//! ## Why re-siting is required
//! The snapshot is a byte copy of the seed's CRR DB, so its `crsql_site_id` (ordinal 0,
//! "local") is the SEED's. If the join wrote into it as-is, its writes would be
//! attributed to the seed's `site_id` and collide with the seed's clock. So the join
//! [`resite_as_fresh_peer`]s the base first: the snapshot's local rows (ordinal 0) are
//! demoted to a peer ordinal, and the join mints a fresh random `site_id` at ordinal 0.
//! Local writes (the reconciled rows) then carry the join's own identity and flow to the
//! seed as genuine foreign changes; the seed's snapshot rows are never re-pushed (they
//! are a peer's history now). Confirmed against the pinned engine: local writes store
//! ordinal 0 in `{tbl}__crsql_clock` (tableinfo.rs get_mark_locally_created_stmt), and
//! `db_version` is `MAX(db_version)+1` from storage (db_version.rs), so demoting rows to
//! a peer ordinal leaves the version bookkeeping intact.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use uuid::Uuid;

use crate::schema::SYNC_TABLES;
use crate::SyncError;

/// A device's role in a two-populated-device reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRole {
    /// Holds the primary DB; CRRifies a copy and seeds the relay with a snapshot.
    Seed,
    /// Re-bootstraps from the seed's snapshot and reconciles its own local-only rows.
    Join,
}

/// Why a local row could not be cleanly placed — SURFACED for owner review, never a
/// silent drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollisionKind {
    /// The same PK exists in the bootstrapped base with DIFFERENT column values. The
    /// seed's value is authoritative for a shared row; the local edit is surfaced so
    /// the owner can re-apply it deliberately (never auto-overwritten, never dropped
    /// without a record).
    ContentDiverged,
    /// A DIFFERENT PK in the base shares this row's app-level unique identity (e.g. a
    /// second note for one session, a same-name tag, a duplicate audio part). Inserting
    /// it would create a logical duplicate, so it is held back and surfaced.
    LogicalDuplicate,
}

/// One surfaced collision. `pk` is a human-readable rendering of the primary key so the
/// owner (and the T012 UAT) can locate the row; no content is included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub table: String,
    pub pk: String,
    pub kind: CollisionKind,
}

/// Outcome of reconciling a join device's local rows into the bootstrapped base.
///
/// The invariant that makes this loss-free: EVERY local row is accounted for — it is
/// either already present identically (`matched_identical`), inserted as a preserved
/// local-only row (`inserted_local_only`), or recorded in `collisions`. Nothing is
/// dropped without a record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub inserted_local_only: usize,
    pub matched_identical: usize,
    pub collisions: Vec<Collision>,
}

impl ReconcileReport {
    /// Total local rows examined (the sum must equal the source row count — asserted in
    /// tests as the no-silent-loss guarantee).
    #[must_use]
    pub fn accounted(&self) -> usize {
        self.inserted_local_only + self.matched_identical + self.collisions.len()
    }
}

/// List this database's cr-sqlite per-table clock tables (`*__crsql_clock`).
fn clock_tables(conn: &Connection) -> Result<Vec<String>, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master \
         WHERE type='table' AND name LIKE '%\\_\\_crsql\\_clock' ESCAPE '\\'",
    )?;
    let v = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(v)
}

/// Re-site a database freshly opened from a SEED snapshot so the join device becomes an
/// independent peer (see module docs). Demotes the snapshot's local rows (ordinal 0) to
/// a new peer ordinal and mints a fresh random 16-byte `site_id` at ordinal 0.
///
/// MUST be called immediately after opening the snapshot copy and BEFORE any local
/// write; the [`crate::CrsqlDb`] MUST then be reopened so cr-sqlite re-reads the fresh
/// ordinal-0 site id (it caches the site id per connection on first use). Idempotent
/// guard: if ordinal 0 already holds a site id with no rows attributed to a demoted
/// peer, re-running would mint another fresh id — callers gate this behind a one-shot
/// bootstrap flag.
pub fn resite_as_fresh_peer(conn: &Connection) -> Result<(), SyncError> {
    let seed_sid: Vec<u8> = conn.query_row(
        "SELECT site_id FROM crsql_site_id WHERE ordinal = 0",
        [],
        |r| r.get(0),
    )?;
    let next_ord: i64 = conn.query_row(
        "SELECT coalesce(max(ordinal), 0) + 1 FROM crsql_site_id",
        [],
        |r| r.get(0),
    )?;
    let fresh = *Uuid::new_v4().as_bytes();
    let clocks = clock_tables(conn)?;

    conn.execute_batch("BEGIN;")?;
    let inner = || -> Result<(), SyncError> {
        // Mint the join's fresh local identity at ordinal 0 (frees the seed's blob).
        conn.execute(
            "UPDATE crsql_site_id SET site_id = ?1 WHERE ordinal = 0",
            rusqlite::params![fresh.to_vec()],
        )?;
        // Register the seed as a peer at a new ordinal.
        conn.execute(
            "INSERT INTO crsql_site_id (site_id, ordinal) VALUES (?1, ?2)",
            rusqlite::params![seed_sid, next_ord],
        )?;
        // Re-attribute the snapshot's local rows (ordinal 0) to the seed's peer ordinal.
        for clock in &clocks {
            conn.execute(
                &format!("UPDATE \"{clock}\" SET site_id = ?1 WHERE site_id = 0"),
                rusqlite::params![next_ord],
            )?;
        }
        Ok(())
    };
    if let Err(e) = inner() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(e);
    }
    conn.execute_batch("COMMIT;")?;
    Ok(())
}

/// Clear the seed's LOCAL bookkeeping that a byte snapshot inevitably carries over, so
/// the join does not inherit the seed's `client_id`, watermarks, staged outbox, or
/// quarantine. These are plain (non-CRR) tables, so clearing them touches no CRDT clock.
/// After this, [`crate::state::client_id`] mints a FRESH per-install id for the join and
/// the watermarks reset to 0 (the seed's snapshot rows are a peer's history now, so a
/// 0 push watermark still re-emits only the join's own reconciled writes).
///
/// Call once, on the freshly-opened+re-sited join base, before the first drain.
pub fn reset_join_local_state(conn: &Connection) -> Result<(), SyncError> {
    for t in [
        "_yapstack_sync_meta",
        "_yapstack_outbox",
        "_yapstack_pending_changes",
        "_yapstack_sync_prep",
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [t],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n == 1)?;
        if exists {
            conn.execute(&format!("DELETE FROM \"{t}\""), [])?;
        }
    }
    Ok(())
}

/// A table's column names and its primary-key column names (ordered).
fn table_shape(conn: &Connection, table: &str) -> Result<(Vec<String>, Vec<String>), SyncError> {
    let cols: Vec<String> = {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
        let v = stmt
            .query_map([table], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        v
    };
    let pk: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk>0 ORDER BY pk")?;
        let v = stmt
            .query_map([table], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        v
    };
    Ok((cols, pk))
}

fn value_from_ref(r: rusqlite::types::ValueRef<'_>) -> Value {
    match r {
        rusqlite::types::ValueRef::Null => Value::Null,
        rusqlite::types::ValueRef::Integer(i) => Value::Integer(i),
        rusqlite::types::ValueRef::Real(f) => Value::Real(f),
        rusqlite::types::ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        rusqlite::types::ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// Render a PK tuple for the surfaced collision report (no content, just identity).
fn pk_repr(pk_cols: &[String], values: &[Value]) -> String {
    pk_cols
        .iter()
        .map(|c| match values_of(c, pk_cols, values) {
            Some(Value::Text(s)) => s.clone(),
            Some(Value::Integer(i)) => i.to_string(),
            Some(Value::Real(f)) => f.to_string(),
            Some(Value::Blob(_)) => "<blob>".to_string(),
            _ => "NULL".to_string(),
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn values_of<'a>(col: &str, cols: &[String], values: &'a [Value]) -> Option<&'a Value> {
    cols.iter()
        .position(|c| c == col)
        .and_then(|i| values.get(i))
}

/// The app-level unique identity check for a table (the constraints R5 drops): returns
/// a `(where_sql, params)` that finds a base row sharing this row's logical identity
/// but (necessarily) a different PK. `None` = the table has no secondary identity.
fn logical_identity_query(
    table: &str,
    cols: &[String],
    values: &[Value],
) -> Option<(String, Vec<Value>)> {
    match table {
        // One note per session.
        "notes" => values_of("session_id", cols, values)
            .cloned()
            .map(|sid| ("session_id = ?1".to_string(), vec![sid])),
        // Tag names unique, case-insensitive.
        "tags" => values_of("name", cols, values)
            .cloned()
            .map(|name| ("name = ?1 COLLATE NOCASE".to_string(), vec![name])),
        // One row per (session, part index).
        "session_audio_parts" => {
            match (
                values_of("session_id", cols, values).cloned(),
                values_of("part_index", cols, values).cloned(),
            ) {
                (Some(sid), Some(idx)) => Some((
                    "session_id = ?1 AND part_index = ?2".to_string(),
                    vec![sid, idx],
                )),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Reconcile the join device's own rows (read from its ORIGINAL, non-CRR live DB —
/// opened read-only) into the bootstrapped, re-sited CRR base. See module docs.
///
/// - `local`: a read-only connection to the join's original library (never written).
/// - `base`: the re-sited CRR base opened from the seed snapshot (written here; the
///   inserts become the join's own local changes that drain to the seed).
///
/// Runs in one transaction on `base`. Never drops a local row without recording it.
pub fn reconcile_local_rows(
    local: &Connection,
    base: &Connection,
) -> Result<ReconcileReport, SyncError> {
    let mut report = ReconcileReport::default();
    base.execute_batch("BEGIN;")?;
    let mut run = || -> Result<(), SyncError> {
        for &table in SYNC_TABLES {
            reconcile_table(local, base, table, &mut report)?;
        }
        Ok(())
    };
    if let Err(e) = run() {
        let _ = base.execute_batch("ROLLBACK;");
        return Err(e);
    }
    base.execute_batch("COMMIT;")?;
    Ok(report)
}

fn reconcile_table(
    local: &Connection,
    base: &Connection,
    table: &str,
    report: &mut ReconcileReport,
) -> Result<(), SyncError> {
    // The local (source) DB may predate a column the CRR base added; drive off the
    // base's shape (the CRR schema) and read only columns present in the source.
    let (base_cols, pk_cols) = table_shape(base, table)?;
    let (local_cols, _) = table_shape(local, table)?;
    // Columns to copy = base columns that also exist in the source. Missing-in-source
    // columns take the base table's DEFAULT on insert.
    let copy_cols: Vec<String> = base_cols
        .iter()
        .filter(|c| local_cols.iter().any(|lc| lc == *c))
        .cloned()
        .collect();
    if pk_cols.is_empty() {
        return Err(SyncError::Migration(format!("{table}: no primary key")));
    }

    let sel = format!(
        "SELECT {} FROM \"{table}\"",
        copy_cols
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut stmt = local.prepare(&sel)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut values: Vec<Value> = Vec::with_capacity(copy_cols.len());
        for i in 0..copy_cols.len() {
            values.push(value_from_ref(row.get_ref(i)?));
        }

        // 1. PK identity match against the base.
        let pk_where = pk_cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("\"{c}\" = ?{}", i + 1))
            .collect::<Vec<_>>()
            .join(" AND ");
        let pk_vals: Vec<Value> = pk_cols
            .iter()
            .filter_map(|c| values_of(c, &copy_cols, &values).cloned())
            .collect();
        if pk_vals.len() != pk_cols.len() {
            // A NULL/absent PK component — cannot place safely; surface it.
            report.collisions.push(Collision {
                table: table.to_string(),
                pk: pk_repr(&pk_cols, &values),
                kind: CollisionKind::ContentDiverged,
            });
            continue;
        }

        let base_row = base_row_values(base, table, &copy_cols, &pk_where, &pk_vals)?;
        if let Some(base_values) = base_row {
            if base_values == values {
                report.matched_identical += 1;
            } else {
                report.collisions.push(Collision {
                    table: table.to_string(),
                    pk: pk_repr(&pk_cols, &values),
                    kind: CollisionKind::ContentDiverged,
                });
            }
            continue;
        }

        // 2. App-level logical-identity collision (a different PK, same identity).
        if let Some((where_sql, lparams)) = logical_identity_query(table, &copy_cols, &values) {
            let exists: bool = base.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM \"{table}\" WHERE {where_sql})"),
                params_from_iter(lparams.iter()),
                |r| r.get(0),
            )?;
            if exists {
                report.collisions.push(Collision {
                    table: table.to_string(),
                    pk: pk_repr(&pk_cols, &values),
                    kind: CollisionKind::LogicalDuplicate,
                });
                continue;
            }
        }

        // 3. Genuinely local-only: insert as a new local write (preserved, drains out).
        let placeholders = (1..=copy_cols.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let ins = format!(
            "INSERT INTO \"{table}\" ({}) VALUES ({placeholders})",
            copy_cols
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        base.execute(&ins, params_from_iter(values.iter()))?;
        report.inserted_local_only += 1;
    }
    Ok(())
}

fn base_row_values(
    base: &Connection,
    table: &str,
    cols: &[String],
    pk_where: &str,
    pk_vals: &[Value],
) -> Result<Option<Vec<Value>>, SyncError> {
    let sql = format!(
        "SELECT {} FROM \"{table}\" WHERE {pk_where}",
        cols.iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut stmt = base.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(pk_vals.iter()))?;
    if let Some(row) = rows.next()? {
        let mut v = Vec::with_capacity(cols.len());
        for i in 0..cols.len() {
            v.push(value_from_ref(row.get_ref(i)?));
        }
        Ok(Some(v))
    } else {
        Ok(None)
    }
}
