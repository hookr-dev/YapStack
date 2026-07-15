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
    if t.is_empty() {
        return "''".to_string();
    }
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
        let ty = c.ty.trim();
        let mut d = if ty.is_empty() {
            format!("\"{}\"", c.name)
        } else {
            format!("\"{}\" {ty}", c.name)
        };
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
fn table_exists(conn: &Connection, table: &str) -> Result<bool, SyncError> {
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
    conn.execute_batch("BEGIN;")?;
    let build = || -> Result<(), SyncError> {
        conn.execute_batch(&format!("CREATE TABLE \"{tmp}\" ({body});"))?;
        conn.execute_batch(&format!("INSERT INTO \"{tmp}\" SELECT * FROM \"{table}\";"))?;
        conn.execute_batch(&format!("DROP TABLE \"{table}\";"))?;
        conn.execute_batch(&format!("ALTER TABLE \"{tmp}\" RENAME TO \"{table}\";"))?;
        for t in &triggers {
            conn.execute_batch(&format!("{t};"))?;
        }
        Ok(())
    };
    if let Err(e) = build() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(e);
    }
    conn.execute_batch("COMMIT;")?;

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

/// R7: apply a schema ALTER to a CRR table wrapped in `crsql_begin_alter` /
/// `crsql_commit_alter`, so cr-sqlite rebuilds the clock/pks shadow tables and
/// backfills the new column at `col_version=1` WITHOUT desyncing existing clocks
/// (T001: alter.rs:36-146). `alter_sql` is a full statement, e.g.
/// `ALTER TABLE segments ADD COLUMN speaker_id INTEGER`.
pub fn crsql_alter(conn: &Connection, table: &str, alter_sql: &str) -> Result<(), SyncError> {
    conn.execute_batch("BEGIN;")?;
    let inner = || -> Result<(), SyncError> {
        conn.query_row(&format!("SELECT crsql_begin_alter('{table}')"), [], |_| {
            Ok(())
        })?;
        conn.execute_batch(&format!("{alter_sql};"))?;
        conn.query_row(&format!("SELECT crsql_commit_alter('{table}')"), [], |_| {
            Ok(())
        })?;
        Ok(())
    };
    if let Err(e) = inner() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(e);
    }
    conn.execute_batch("COMMIT;")?;
    Ok(())
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

    #[test]
    fn derive_rejects_missing_table() {
        let conn = Connection::open_in_memory().unwrap();
        match derive_rebuild_body(&conn, "nope").unwrap_err() {
            SyncError::UnknownTable(t) => assert_eq!(t, "nope"),
            other => panic!("expected UnknownTable, got {other:?}"),
        }
    }
}
