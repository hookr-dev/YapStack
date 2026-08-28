// SPDX-License-Identifier: AGPL-3.0-only
//! R3 CRR schema migration + R7 alter-wrapping.
//!
//! Transforms the real local schema (apps/desktop/src-tauri/src/db.rs + the db.ts
//! out-of-band ALTERs) into a cr-sqlite-compatible one and converts each sync table
//! to a CRR. Per T001/T003 `is_table_compatible` (tableinfo.rs:909-1001) rejects a
//! table with ANY checked FK, ANY non-PK UNIQUE index, or a NOT-NULL-no-default
//! non-PK column, and a single-col TEXT PK must be rebuilt `NOT NULL PRIMARY KEY`.
//! We therefore rebuild every sync table: FK COLUMNS kept but constraints stripped
//! (CASCADE/SET NULL move to app layer — see `cascade`), the 3 non-PK UNIQUEs
//! dropped (enforced in `uniqueness`), NOT-NULL-no-default columns defaulted, TEXT
//! PKs made `NOT NULL PRIMARY KEY`. Column ORDER is identical to the live schema so
//! `INSERT ... SELECT *` round-trips exactly (proven by T003 scenario 0 and the
//! crate's `crr_migration_roundtrip` test on a populated copy).

use rusqlite::Connection;

use crate::SyncError;

/// The tables the architecture syncs (T001 Deliverable 4). Everything else —
/// `*_fts*`, `*_embeddings_*`, `pending_audio_deletions`, `_sqlx_migrations`,
/// `sqlite_sequence` — is local-only and MUST NOT be a CRR.
pub const SYNC_TABLES: &[&str] = &[
    "sessions",
    "segments",
    "notes",
    "note_versions",
    "folders",
    "session_folders",
    "chat_messages",
    "dictation_history",
    "tags",
    "session_tags",
    "session_audio_parts",
    "shares",
];

/// One column's metadata as reported by `PRAGMA table_info`, in declared (`cid`)
/// order. This is the authoritative, drift-proof shape of the LIVE table.
struct ColInfo {
    name: String,
    ty: String,
    notnull: bool,
    dflt: Option<String>,
    /// 0 = not part of the PK; 1..N = position within the PRIMARY KEY.
    pk: i64,
}

/// Read the live column metadata for `table` in declared order.
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<ColInfo>, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT name, type, \"notnull\", dflt_value, pk \
         FROM pragma_table_info(?1) ORDER BY cid",
    )?;
    let cols: Result<Vec<ColInfo>, _> = stmt
        .query_map([table], |r| {
            Ok(ColInfo {
                name: r.get(0)?,
                ty: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                notnull: r.get::<_, i64>(2)? != 0,
                dflt: r.get::<_, Option<String>>(3)?,
                pk: r.get(4)?,
            })
        })?
        .collect();
    Ok(cols?)
}

/// True if a declared column type has TEXT storage affinity (TEXT/CHAR/CLOB).
/// The sync architecture assumes UUID **TEXT** primary keys.
fn type_is_text(ty: &str) -> bool {
    let u = ty.to_ascii_uppercase();
    u.contains("TEXT") || u.contains("CHAR") || u.contains("CLOB")
}

/// A non-null, type-valid synthetic default for a NOT-NULL column the live schema
/// left without one — required so `is_table_compatible` accepts the CRR and so a
/// merge that inserts a partial row does not violate NOT NULL.
fn synthetic_default(ty: &str) -> &'static str {
    let u = ty.to_ascii_uppercase();
    if u.contains("INT")
        || u.contains("REAL")
        || u.contains("FLOA")
        || u.contains("DOUB")
        || u.contains("NUM")
        || u.contains("DEC")
    {
        "0"
    } else {
        "''"
    }
}

/// Re-emit a live default so it is valid inside a fresh `CREATE TABLE`. `PRAGMA
/// table_info` strips the parentheses SQLite requires around an *expression* default
/// (e.g. `datetime('now')`), so a non-literal is re-wrapped in `(...)`; literals
/// (numbers, quoted strings, NULL, CURRENT_* keywords, already-parenthesized exprs)
/// pass through verbatim.
fn emit_default(dflt: &str) -> String {
    let t = dflt.trim();
    let u = t.to_ascii_uppercase();
    let literal = t.starts_with('(')
        || t.starts_with('\'')
        || t.starts_with('"')
        || u == "NULL"
        || u == "TRUE"
        || u == "FALSE"
        || u == "CURRENT_TIME"
        || u == "CURRENT_DATE"
        || u == "CURRENT_TIMESTAMP"
        || t.parse::<f64>().is_ok();
    if literal {
        t.to_string()
    } else {
        format!("({t})")
    }
}

/// Derive the CRR-compatible rebuild body for `table` from its LIVE shape, rather
/// than a hardcoded column list. This is the fresh-install cutover fix (R8): a DB
/// built by the full A2 migration runner legitimately differs — by the runtime-patched
/// `segments.speaker_id` column — from a long-lived dev DB that already carries it, so
/// a hardcoded shape races the migration history and `INSERT ... SELECT *` mismatches
/// the column count. Deriving from `PRAGMA table_info` follows the live schema exactly.
///
/// Transformations applied (the invariants the rebuild exists to enforce, per the
/// module doc): the single-column TEXT PK becomes `NOT NULL PRIMARY KEY`; a composite
/// PK is re-declared as a trailing `PRIMARY KEY (...)` clause with its members forced
/// NOT NULL; every NOT-NULL non-PK column WITHOUT a default gains a type-appropriate
/// synthetic default. FK constraints, non-PK UNIQUE indexes and CHECK constraints are
/// deliberately NOT reconstructed — cascade/uniqueness move to the app layer (see
/// `cascade`/`uniqueness`), and a surviving CHECK would be a merge hazard (a peer's
/// merged value that violated it would fail to apply and desync). Column names and
/// order are copied verbatim, so the existing `INSERT ... SELECT *` round-trips.
///
/// CRYPTO/architecture invariant: sync assumes a UUID TEXT primary key. Every PK column
/// is asserted to have TEXT affinity; a table whose PK is anything else fails loudly.
pub fn derive_rebuild_body(conn: &Connection, table: &str) -> Result<String, SyncError> {
    let cols = table_columns(conn, table)?;
    if cols.is_empty() {
        return Err(SyncError::UnknownTable(table.to_string()));
    }
    let mut pk_cols: Vec<&ColInfo> = cols.iter().filter(|c| c.pk > 0).collect();
    pk_cols.sort_by_key(|c| c.pk);
    if pk_cols.is_empty() {
        return Err(SyncError::Migration(format!(
            "sync table `{table}` has no PRIMARY KEY; sync requires a UUID TEXT PK"
        )));
    }
    for c in &pk_cols {
        if !type_is_text(&c.ty) {
            return Err(SyncError::Migration(format!(
                "sync table `{table}` PK column `{}` has type `{}`, not TEXT; the sync \
                 architecture (and per-row crypto AAD) assumes a UUID TEXT primary key",
                c.name, c.ty
            )));
        }
    }
    let single_pk = pk_cols.len() == 1;

    let mut defs: Vec<String> = Vec::with_capacity(cols.len() + 1);
    for c in &cols {
        let mut d = format!("\"{}\" {}", c.name, c.ty.trim());
        if c.pk > 0 {
            // PK columns are always NOT NULL; a single TEXT PK is declared inline.
            d.push_str(" NOT NULL");
            if single_pk {
                d.push_str(" PRIMARY KEY");
            }
        } else if c.notnull {
            d.push_str(" NOT NULL DEFAULT ");
            match &c.dflt {
                Some(v) => d.push_str(&emit_default(v)),
                None => d.push_str(synthetic_default(&c.ty)),
            }
        } else if let Some(v) = &c.dflt {
            d.push_str(" DEFAULT ");
            d.push_str(&emit_default(v));
        }
        defs.push(d);
    }
    if !single_pk {
        let names = pk_cols
            .iter()
            .map(|c| format!("\"{}\"", c.name))
            .collect::<Vec<_>>()
            .join(", ");
        defs.push(format!("PRIMARY KEY ({names})"));
    }
    Ok(defs.join(", "))
}

/// True if `table` currently exists in the schema.
pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool, SyncError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(n == 1)
}

/// True once `table` has been converted (its clock shadow table exists).
pub fn is_crr(conn: &Connection, table: &str) -> Result<bool, SyncError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [format!("{table}__crsql_clock")],
        |r| r.get(0),
    )?;
    Ok(n == 1)
}

/// Rebuild one table into its CRR-compatible shape (preserving app triggers such as
/// the FTS sync triggers) then `crsql_as_crr` it. Idempotent: a table already
/// converted is skipped.
pub fn transform_and_crrify(conn: &Connection, table: &str) -> Result<(), SyncError> {
    if is_crr(conn, table)? {
        return Ok(());
    }
    if !table_exists(conn, table)? {
        return Err(SyncError::UnknownTable(table.to_string()));
    }
    let triggers: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT sql FROM sqlite_master \
             WHERE type='trigger' AND tbl_name=?1 AND sql IS NOT NULL",
        )?;
        let v: Result<Vec<String>, _> = stmt
            .query_map([table], |r| r.get::<_, String>(0))?
            .collect();
        v?
    };

    conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
    let tmp = format!("{table}__crr_rebuild");
    // Derive the rebuild shape from the LIVE table (not a hardcoded list) so the
    // column count/order always matches `SELECT *`, regardless of migration drift
    // such as the runtime-patched `segments.speaker_id` (R8 fresh-install fix).
    let body = derive_rebuild_body(conn, table)?;
    // `new_unchecked` because we hold only a `&Connection` here, not `&mut`; the
    // `DropBehavior::Rollback` default is what unwinds a half-done rebuild.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;
    tx.execute_batch(&format!("CREATE TABLE \"{tmp}\" ({body});"))?;
    tx.execute_batch(&format!("INSERT INTO \"{tmp}\" SELECT * FROM \"{table}\";"))?;
    tx.execute_batch(&format!("DROP TABLE \"{table}\";"))?;
    tx.execute_batch(&format!("ALTER TABLE \"{tmp}\" RENAME TO \"{table}\";"))?;
    for t in &triggers {
        tx.execute_batch(&format!("{t};"))?;
    }
    tx.commit()?;

    // AFTER the commit: `crsql_as_crr` must see the rebuilt table as committed schema.
    conn.query_row(&format!("SELECT crsql_as_crr('{table}')"), [], |_| Ok(()))
        .map_err(|e| SyncError::Migration(format!("crsql_as_crr('{table}'): {e}")))?;
    Ok(())
}

/// R3 entry point: transform + CRRify every sync table on a (COPY of a) local DB.
/// NEVER call against the live DB — operate on a `.backup` copy (T001 procedure).
pub fn crr_migrate(conn: &Connection) -> Result<(), SyncError> {
    for t in SYNC_TABLES {
        transform_and_crrify(conn, t)?;
    }
    Ok(())
}

/// The `crsql_begin_alter` / `crsql_commit_alter` dance around an OPTIONAL schema
/// statement.
///
/// `crsql_begin_alter` DROPS the three crsql triggers (vendor
/// `core/rs/core/src/lib.rs:645` -> `teardown.rs:21-55`); `crsql_commit_alter`
/// (`lib.rs:654-720`) then runs `crsql_compact_post_alter` (`alter.rs:29-149`) and
/// `crsql_create_crr` (`create_crr.rs:14-49`), which re-pulls the table shape from
/// `pragma_table_info` and RE-CREATES all three triggers at the table's CURRENT arity
/// (`triggers.rs:12-20`). The trigger recreation is why `alter_sql` may be `None`:
/// the dance with no schema change is a pure "regenerate the CRR machinery for the
/// shape the table has right now" operation (see `rebuild_crr_machinery`).
///
/// INVARIANT — the dance must NOT reset sync state, and does not:
///   * `create_clock_table` is `CREATE TABLE IF NOT EXISTS` for both `__crsql_clock`
///     and `__crsql_pks` plus `CREATE [UNIQUE] INDEX IF NOT EXISTS`
///     (`bootstrap.rs:195-235`) — existing clock rows and lookaside keys survive.
///   * the clock table is only DROPPED when the PRIMARY KEY set changed
///     (`alter.rs:65-71`, `pk_diff > 0`); with no schema change `pk_diff` is 0 and the
///     `else` branch only COMPACTS (deletes clock rows for columns/rows that no longer
///     exist, keeping delete sentinels) — a no-op on a consistent table.
///   * the backfill runs with `is_commit_alter = true`, which stamps any newly created
///     clock row with `crsql_db_version()` — the CURRENT version, NOT
///     `crsql_next_db_version()` (`backfill.rs:107-117`, and the comment at
///     `backfill.rs:100-104`) — so the dance never bumps `db_version` and never
///     manufactures a re-push storm. Rows that already have clock entries are skipped
///     by `INSERT OR IGNORE`.
///   * `crsql_site_id` is never touched by any of these paths.
///
/// BEGIN IMMEDIATE, not a deferred BEGIN. `crsql_begin_alter` writes immediately (it
/// DROPs triggers), so a DEFERRED transaction that first READ anything would have to
/// upgrade read->write; in WAL a lost upgrade race returns `SQLITE_BUSY_SNAPSHOT`, which
/// the busy handler does NOT retry — it fails outright regardless of `busy_timeout`.
/// Taking the write lock up front lets `busy_timeout`
/// (`crsqlite::apply_sync_pragmas`) serialize the drain against the app's writer
/// instead. Same reasoning and precedent as `outbox::enqueue_local` (T023 Judge). This
/// matters more for the heal than for a migration: detection runs ONCE per drain spawn,
/// so one lost race would postpone the repair until the next app restart.
fn alter_dance(conn: &Connection, table: &str, alter_sql: Option<&str>) -> Result<(), SyncError> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.query_row(&format!("SELECT crsql_begin_alter('{table}')"), [], |_| {
        Ok(())
    })?;
    if let Some(sql) = alter_sql {
        tx.execute_batch(&format!("{sql};"))?;
    }
    tx.query_row(&format!("SELECT crsql_commit_alter('{table}')"), [], |_| {
        Ok(())
    })?;
    tx.commit()?;
    Ok(())
}

/// R7: apply a schema ALTER to a CRR table wrapped in `crsql_begin_alter` /
/// `crsql_commit_alter`, so cr-sqlite rebuilds the clock/pks shadow tables and
/// backfills the new column at `col_version=1` WITHOUT desyncing existing clocks
/// (T001: alter.rs:36-146). `alter_sql` is a full statement, e.g.
/// `ALTER TABLE segments ADD COLUMN speaker_id INTEGER`.
pub fn crsql_alter(conn: &Connection, table: &str, alter_sql: &str) -> Result<(), SyncError> {
    alter_dance(conn, table, Some(alter_sql))
}

/// Re-run the alter dance with NO schema change, forcing cr-sqlite to regenerate the
/// insert/update/delete triggers (and any missing clock rows) for the table's CURRENT
/// column shape. This is the repair for a table whose trigger TEXT drifted away from
/// its column list — see `crr_triggers_are_stale` for how that happens.
///
/// Safe and idempotent on a healthy table: see the `alter_dance` invariant block for
/// the vendor citations proving clock rows, lookaside keys, `db_version` and site id
/// all survive.
pub fn rebuild_crr_machinery(conn: &Connection, table: &str) -> Result<(), SyncError> {
    alter_dance(conn, table, None)
}

/// Every CRR-tracked table in this database, discovered from its clock shadow table.
/// Deliberately NOT `SYNC_TABLES`: the trigger-arity hazard applies to ANY CRR table,
/// including ones a future release adds, so the heal must be table-generic.
pub fn crr_tables(conn: &Connection) -> Result<Vec<String>, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT substr(name, 1, length(name) - length('__crsql_clock')) \
         FROM sqlite_master \
         WHERE type='table' AND name LIKE '%\\_\\_crsql\\_clock' ESCAPE '\\' \
         ORDER BY name",
    )?;
    let names: Result<Vec<String>, _> = stmt.query_map([], |r| r.get::<_, String>(0))?.collect();
    let names = names?;
    // Keep only the ones whose base table actually exists (a stray clock table with no
    // base table is a teardown artifact, not something we can or should rebuild).
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        if !n.is_empty() && table_exists(conn, &n)? {
            out.push(n);
        }
    }
    Ok(out)
}

/// Of the three triggers cr-sqlite generates per CRR table (`triggers.rs:12-20`), the two
/// whose existence nothing else proves. ALL three must still exist: `is_crr` here probes
/// the clock table, and cr-sqlite's own `is_crr` probes only `__crsql_itrig`
/// (`is_crr.rs:10-26`), so neither alone notices a table that lost one of them.
/// `__crsql_utrig` is omitted only because the slot comparison below already has to fetch
/// its SQL, and reports stale when it is absent.
const CRR_TRIGGERS_NOT_COVERED_BY_SLOT_COMPARE: [&str; 2] = ["__crsql_itrig", "__crsql_dtrig"];

/// The SQL text of trigger `name`, or `None` if it does not exist.
fn trigger_sql(conn: &Connection, name: &str) -> Result<Option<String>, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1 AND sql IS NOT NULL",
    )?;
    let mut rows = stmt.query([name])?;
    match rows.next()? {
        Some(r) => Ok(Some(r.get(0)?)),
        None => Ok(None),
    }
}

/// The ordered `NEW."col"` / `OLD."col"` value slots a trigger body passes, in the exact
/// textual order they appear, with SQL identifier quoting undone (`""` -> `"`).
///
/// Order matters: comparing an ORDERED slot list against the expected one catches a
/// RENAME COLUMN and a column REORDER, which a per-column "is it mentioned anywhere"
/// check cannot — those keep both the slot count and the name set intact while the
/// positional mapping to `crsql_after_update`'s pks/non-pks partition
/// (`local_writes/after_update.rs:43-63`) silently shifts, so values would be attributed
/// to the wrong columns. Scanning is byte-wise and only ever splits on the ASCII `"`
/// delimiter, so multi-byte identifiers survive intact.
fn trigger_value_slots(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 5 <= b.len() {
        let prefix = if b[i..].starts_with(b"NEW.\"") {
            "NEW"
        } else if b[i..].starts_with(b"OLD.\"") {
            "OLD"
        } else {
            i += 1;
            continue;
        };
        let mut j = i + 5;
        let mut name: Vec<u8> = Vec::new();
        let mut closed = false;
        while j < b.len() {
            if b[j] == b'"' {
                if b.get(j + 1) == Some(&b'"') {
                    name.push(b'"'); // escaped quote inside the identifier
                    j += 2;
                } else {
                    j += 1;
                    closed = true;
                    break;
                }
            } else {
                name.push(b[j]);
                j += 1;
            }
        }
        if !closed {
            break; // unterminated identifier: malformed tail, stop scanning
        }
        out.push(format!("{prefix}.{}", String::from_utf8_lossy(&name)));
        i = j;
    }
    out
}

/// TRUE when `table`'s cr-sqlite AFTER UPDATE trigger no longer matches the table's
/// current column list — the "expected 29 values, got 27" corruption.
///
/// HOW THE DRIFT HAPPENS. `x_crsql_after_update` derives the argument count it expects
/// from the LIVE table shape it re-reads at call time —
/// `1 + pks*2 + non_pks*2` (vendor `local_writes/after_update.rs:43-56`) — while the
/// argument list actually passed comes from the trigger TEXT frozen in `sqlite_master`
/// when the trigger was generated (`triggers.rs:39-76`). A bare
/// `ALTER TABLE t ADD COLUMN c` OUTSIDE the `crsql_begin_alter`/`crsql_commit_alter`
/// dance is ACCEPTED by cr-sqlite (pinned in the `remediations` suite) and changes the
/// live shape WITHOUT regenerating the trigger, so every subsequent direct UPDATE fails
/// at trigger-fire time. INSERT and DELETE keep working, because their triggers pass
/// only primary-key values (`triggers.rs:22-37` / `:78-95`) — which is exactly the
/// reported symptom: sync and inserts fine, segment edit/soft-delete/hide broken.
/// `apply_out_of_band_alters` cannot repair it on its own: it skips any column that
/// already `column_exists`, which a bare ALTER has already created.
///
/// DETECTION, two independent checks.
///
/// 1. ALL THREE triggers must exist. A missing `__crsql_itrig` / `__crsql_dtrig` is just
///    as broken as a stale `__crsql_utrig` — it is silently WORSE, because an
///    uncaptured INSERT or DELETE raises no error at all, it simply never syncs. The
///    transactional dance cannot leave that state, but a future table-rebuild migration
///    or manual DDL can (SQLite drops a table's triggers with the table), and neither
///    our clock-table `is_crr` nor cr-sqlite's itrig-based one would notice.
/// 2. The update trigger's ORDERED value slots must equal
///    `[NEW.pk…, OLD.pk…, NEW.non_pk…, OLD.non_pk…]` for the table's live shape, which is
///    exactly what the generator emits (`triggers.rs:39-76`) and exactly how
///    `partition_values` slices them back apart (`local_writes/after_update.rs:43-63`).
///    Comparing the ordered list subsumes the arity check that catches ADD/DROP COLUMN
///    and additionally catches RENAME COLUMN and column reordering.
pub fn crr_triggers_are_stale(conn: &Connection, table: &str) -> Result<bool, SyncError> {
    if !is_crr(conn, table)? {
        return Ok(false);
    }
    for suffix in CRR_TRIGGERS_NOT_COVERED_BY_SLOT_COMPARE {
        if trigger_sql(conn, &format!("{table}{suffix}"))?.is_none() {
            return Ok(true);
        }
    }
    let cols = table_columns(conn, table)?;
    let mut pks: Vec<&ColInfo> = cols.iter().filter(|c| c.pk > 0).collect();
    pks.sort_by_key(|c| c.pk);
    // A CRR table always has a primary key (cr-sqlite's `is_table_compatible` enforces
    // it). Without one we cannot derive the expected slot order, so report healthy
    // rather than fire a rebuild we could not verify.
    if pks.is_empty() {
        return Ok(false);
    }
    let non_pks: Vec<&ColInfo> = cols.iter().filter(|c| c.pk == 0).collect();
    let mut expected = Vec::with_capacity(cols.len() * 2);
    expected.extend(pks.iter().map(|c| format!("NEW.{}", c.name)));
    expected.extend(pks.iter().map(|c| format!("OLD.{}", c.name)));
    expected.extend(non_pks.iter().map(|c| format!("NEW.{}", c.name)));
    expected.extend(non_pks.iter().map(|c| format!("OLD.{}", c.name)));

    // Doubles as the `__crsql_utrig` existence leg of check 1 (it is the one suffix the
    // loop above leaves out), so the `None` arm is load-bearing, not defensive.
    let utrig = match trigger_sql(conn, &format!("{table}__crsql_utrig"))? {
        Some(s) => s,
        None => return Ok(true),
    };
    Ok(trigger_value_slots(&utrig) != expected)
}

/// Boot-time self-heal (defence in depth): regenerate the CRR machinery of every CRR
/// table whose triggers drifted from its column shape. Returns the tables it healed.
///
/// DETECT-THEN-HEAL, not unconditional. The dance is not free: `crsql_commit_alter`
/// scans and compacts the whole clock table (`alter.rs:77-141`) and the backfill runs
/// one full LEFT JOIN over the base table PER non-PK column (`backfill.rs:195-238`) —
/// on a real library that is a multi-second, WAL-dirtying pass over every table on
/// EVERY boot, and it would re-run any compaction risk each time. Detection is one
/// `sqlite_master` string read plus one `pragma_table_info` per table. So we pay the
/// rebuild only where it is needed, which for a healthy device is never.
///
/// Failures are per-table and non-fatal to the rest: a table that cannot be rebuilt is
/// returned as an error only after the others have been attempted, so one bad table
/// cannot block healing the rest.
///
/// TWO ACCEPTED CONSEQUENCES of repairing late rather than never (both inherent to the
/// vendor's post-alter behaviour, neither introduced here):
///   * Clock rows the backfill creates for a newly tracked column are stamped AT the
///     current `db_version`, not a new one (`backfill.rs:107-117`) — deliberately, since
///     upstream assumes every peer applies the same migration. So on a device whose push
///     watermark has already caught up, PRE-EXISTING values in that column may never
///     push; only future edits to it propagate. Convergence is unaffected either way
///     (the outcome is deterministic LWW), and a column that is entirely default/NULL —
///     the common case, e.g. `speaker_id` — backfills nothing at all.
///   * If a table ever ran trigger-less AND rows were hard-deleted during that window,
///     `crsql_commit_alter`'s compaction deletes the now-orphaned clock rows for those
///     rows (`alter.rs:87-131`, which keeps only delete SENTINELS). With no sentinel to
///     carry, the deletion never propagates and a peer may resurrect the row. The
///     rebuild neither causes nor worsens this — the missing trigger did — but it does
///     make the loss permanent, so it is recorded rather than silently absorbed.
pub fn heal_stale_crr_triggers(conn: &Connection) -> Result<Vec<String>, SyncError> {
    let mut healed = Vec::new();
    let mut first_err: Option<SyncError> = None;
    for table in crr_tables(conn)? {
        match crr_triggers_are_stale(conn, &table) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => {
                first_err.get_or_insert(e);
                continue;
            }
        }
        if let Err(e) = rebuild_crr_machinery(conn, &table) {
            first_err.get_or_insert(SyncError::Migration(format!(
                "rebuild_crr_machinery('{table}'): {e}"
            )));
            continue;
        }
        // Post-condition: the rebuild must actually have fixed the arity. If it did
        // not, fail loudly rather than reporting a heal that did nothing.
        match crr_triggers_are_stale(conn, &table) {
            Ok(true) => first_err.get_or_insert(SyncError::Migration(format!(
                "crsql trigger arity for `{table}` is STILL stale after rebuild"
            ))),
            Ok(false) => {
                healed.push(table);
                continue;
            }
            Err(e) => first_err.get_or_insert(e),
        };
    }
    match first_err {
        // Name what DID get healed inside the error, so a partial pass is never invisible
        // to the caller (which only sees Ok(healed) or Err).
        Some(e) if healed.is_empty() => Err(e),
        Some(e) => Err(SyncError::Migration(format!(
            "{e} (healed before failing: {})",
            healed.join(", ")
        ))),
        None => Ok(healed),
    }
}

/// True if `table` has column `col`.
pub fn column_exists(conn: &Connection, table: &str, col: &str) -> Result<bool, SyncError> {
    let mut stmt = conn.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name=?2")?;
    let found = stmt.exists(rusqlite::params![table, col])?;
    Ok(found)
}

/// The exact db.ts startup ALTERs (T001 Deliverable, db.ts:191-210): `(table, col,
/// coldef)`. Each is applied through `crsql_alter` (R7) so a device that CRRified at
/// the base schema version migrates without clock desync.
pub const OUT_OF_BAND_ALTERS: &[(&str, &str, &str)] = &[
    ("segments", "speaker_id", "speaker_id INTEGER"),
    (
        "sessions",
        "recording_device_id",
        "recording_device_id TEXT",
    ),
    ("chat_messages", "tool_calls", "tool_calls TEXT"),
    ("chat_messages", "send_id", "send_id TEXT"),
    ("chat_messages", "sequence", "sequence INTEGER"),
    ("chat_messages", "tool_call_id", "tool_call_id TEXT"),
    ("chat_messages", "observation", "observation TEXT"),
    ("chat_messages", "status", "status TEXT"),
];

/// Apply every out-of-band ALTER through `crsql_alter`, skipping any column that
/// already exists (the live DB already carries them; a base-version device does
/// not). Idempotent.
///
/// LIMIT OF THIS PASS (why `heal_stale_crr_triggers` must run beside it): the
/// `column_exists` skip cannot distinguish "column added through the alter dance" from
/// "column added by a bare ALTER outside the dance". A build that predates the db.ts
/// CRR gate bare-ALTERed these very columns onto a CRR table; this pass then skips them
/// forever, leaving the crsql triggers frozen at the OLD arity. Callers must follow this
/// with `heal_stale_crr_triggers`, which repairs that state.
pub fn apply_out_of_band_alters(conn: &Connection) -> Result<(), SyncError> {
    for (table, col, coldef) in OUT_OF_BAND_ALTERS {
        if !is_crr(conn, table)? {
            continue;
        }
        if column_exists(conn, table, col)? {
            continue;
        }
        crsql_alter(
            conn,
            table,
            &format!("ALTER TABLE {table} ADD COLUMN {coldef}"),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Column names of `table` in declared order.
    fn col_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .unwrap();
        stmt.query_map([table], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect()
    }

    /// Build the derived body, materialize it as a real table, and return that
    /// table's column names — proving the derived DDL is valid SQL and preserves the
    /// live column set/order.
    fn derived_cols(conn: &Connection, src: &str) -> Vec<String> {
        let body = derive_rebuild_body(conn, src).unwrap();
        conn.execute_batch(&format!("CREATE TABLE _derived ({body});"))
            .unwrap();
        // INSERT ... SELECT * must line up column-for-column with the source.
        conn.execute_batch(&format!("INSERT INTO _derived SELECT * FROM \"{src}\";"))
            .unwrap();
        let out = col_names(conn, "_derived");
        conn.execute_batch("DROP TABLE _derived;").unwrap();
        out
    }

    // The exact 13-column `segments` the full db_service migration chain produces
    // (v1 base + v3 editing columns) — NO speaker_id, which is a frontend runtime
    // patch, not a migration. This is the fresh-install shape that crashed cutover.
    const SEGMENTS_13: &str = "CREATE TABLE segments (\
        id TEXT PRIMARY KEY, \
        session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, \
        source TEXT NOT NULL, \
        text TEXT NOT NULL, \
        audio_offset_seconds REAL NOT NULL, \
        chunk_duration_seconds REAL NOT NULL, \
        confidence REAL NOT NULL DEFAULT 1.0, \
        created_at TEXT NOT NULL DEFAULT (datetime('now')), \
        chunk_index INTEGER NOT NULL DEFAULT 0, \
        original_text TEXT, \
        edited_at TEXT, \
        deleted_at TEXT, \
        hidden INTEGER NOT NULL DEFAULT 0);";

    #[test]
    fn derive_segments_fresh_chain_13_columns_no_speaker_id() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(SEGMENTS_13).unwrap();
        conn.execute(
            "INSERT INTO segments(id,session_id,source,text,audio_offset_seconds,chunk_duration_seconds) \
             VALUES ('g1','s1','mic','hi',0,1)",
            [],
        )
        .unwrap();
        let cols = derived_cols(&conn, "segments");
        assert_eq!(cols.len(), 13, "fresh-chain segments derives 13 columns");
        assert!(!cols.iter().any(|c| c == "speaker_id"));
        assert_eq!(cols[0], "id");
        // Single TEXT PK is rebuilt NOT NULL PRIMARY KEY.
        let body = derive_rebuild_body(&conn, "segments").unwrap();
        assert!(body.contains("\"id\" TEXT NOT NULL PRIMARY KEY"));
        // NOT-NULL-no-default columns get synthetic defaults; expression default kept.
        assert!(body.contains("\"session_id\" TEXT NOT NULL DEFAULT ''"));
        assert!(body.contains("\"audio_offset_seconds\" REAL NOT NULL DEFAULT 0"));
        assert!(body.contains("\"created_at\" TEXT NOT NULL DEFAULT (datetime('now'))"));
        assert!(body.contains("\"confidence\" REAL NOT NULL DEFAULT 1.0"));
    }

    #[test]
    fn derive_segments_dev_db_14_columns_with_speaker_id() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(SEGMENTS_13).unwrap();
        // The Mac-shaped divergence: frontend getDb() patched in speaker_id.
        conn.execute_batch("ALTER TABLE segments ADD COLUMN speaker_id INTEGER;")
            .unwrap();
        let cols = derived_cols(&conn, "segments");
        assert_eq!(cols.len(), 14, "dev-DB segments derives 14 columns");
        assert_eq!(cols.last().unwrap(), "speaker_id");
    }

    #[test]
    fn derive_composite_pk_emits_trailing_primary_key_clause() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(
            "CREATE TABLE session_folders (\
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, \
             folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE, \
             created_at TEXT NOT NULL DEFAULT (datetime('now')), \
             PRIMARY KEY (session_id, folder_id));",
        )
        .unwrap();
        let body = derive_rebuild_body(&conn, "session_folders").unwrap();
        assert!(body.contains("PRIMARY KEY (\"session_id\", \"folder_id\")"));
        // Composite-PK members are NOT NULL but carry no synthetic default.
        assert!(body.contains("\"session_id\" TEXT NOT NULL,"));
        assert!(!body.contains("\"session_id\" TEXT NOT NULL DEFAULT"));
        // The materialized table still round-trips SELECT *.
        let cols = derived_cols(&conn, "session_folders");
        assert_eq!(cols, vec!["session_id", "folder_id", "created_at"]);
    }

    #[test]
    fn derive_drops_non_pk_unique_and_check_constraints() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_audio_parts (\
             id TEXT PRIMARY KEY, \
             session_id TEXT NOT NULL, \
             part_index INTEGER NOT NULL, \
             file_path TEXT NOT NULL, \
             format TEXT NOT NULL CHECK (format IN ('wav','mp3')), \
             duration_seconds REAL NOT NULL, \
             sample_rate INTEGER NOT NULL, \
             created_at TEXT NOT NULL, \
             UNIQUE (session_id, part_index));",
        )
        .unwrap();
        let body = derive_rebuild_body(&conn, "session_audio_parts").unwrap();
        // CHECK and UNIQUE are stripped (merge hazards; enforced app-side instead).
        assert!(!body.to_uppercase().contains("CHECK"));
        assert!(!body.to_uppercase().contains("UNIQUE"));
        // format keeps a valid NOT NULL synthetic default.
        assert!(body.contains("\"format\" TEXT NOT NULL DEFAULT ''"));
    }

    #[test]
    fn derive_rejects_non_text_primary_key() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE bad (id INTEGER PRIMARY KEY, v TEXT);")
            .unwrap();
        let err = derive_rebuild_body(&conn, "bad").unwrap_err();
        match err {
            SyncError::Migration(m) => {
                assert!(m.contains("not TEXT"), "loud PK-type failure: {m}");
            }
            other => panic!("expected Migration error, got {other:?}"),
        }
    }

    /// The slot scanner is load-bearing for detection, so pin its contract directly:
    /// textual ORDER preserved, `""` unescaped to `"`, and non-slot text (the table-name
    /// literal, the `WHEN` clause) ignored.
    #[test]
    fn trigger_value_slots_preserves_order_and_unescapes_quotes() {
        let sql = "CREATE TRIGGER \"t__crsql_utrig\" AFTER UPDATE ON \"t\" \
                   WHEN crsql_internal_sync_bit() = 0 BEGIN \
                   VALUES (crsql_after_update('t', NEW.\"id\", OLD.\"id\", \
                   NEW.\"a\",NEW.\"we\"\"ird\", OLD.\"a\",OLD.\"we\"\"ird\")); END";
        assert_eq!(
            trigger_value_slots(sql),
            vec![
                "NEW.id",
                "OLD.id",
                "NEW.a",
                "NEW.we\"ird",
                "OLD.a",
                "OLD.we\"ird"
            ]
        );
        // Order is compared, not just membership: a permutation is a different list.
        let permuted = sql.replace("NEW.\"a\",NEW.\"we\"\"ird\"", "NEW.\"we\"\"ird\",NEW.\"a\"");
        assert_ne!(trigger_value_slots(&permuted), trigger_value_slots(sql));
        // No slots, and an unterminated identifier, both degrade without panicking.
        assert!(trigger_value_slots("SELECT 1").is_empty());
        assert!(trigger_value_slots("NEW.\"unterminated").is_empty());
    }

    #[test]
    fn derive_rejects_missing_table() {
        let conn = Connection::open_in_memory().unwrap();
        match derive_rebuild_body(&conn, "nope").unwrap_err() {
            SyncError::UnknownTable(t) => assert_eq!(t, "nope"),
            other => panic!("expected UnknownTable, got {other:?}"),
        }
    }
}
