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

/// CRR-compatible column definition body for each sync table, matching the live
/// column order (incl. the db.ts out-of-band columns `segments.speaker_id` and the
/// `chat_messages` columns, which the live DB already carries). Ported from the
/// T003 spike, which round-tripped the real 41 MB DB with identical fingerprints.
pub fn rebuild_body(table: &str) -> Result<&'static str, SyncError> {
    Ok(match table {
        "sessions" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             title TEXT NOT NULL DEFAULT '', \
             created_at TEXT NOT NULL DEFAULT (datetime('now')), \
             updated_at TEXT NOT NULL DEFAULT (datetime('now')), \
             source TEXT NOT NULL DEFAULT 'Mixed', \
             status TEXT NOT NULL DEFAULT 'recording', \
             duration_seconds REAL, \
             total_segments INTEGER NOT NULL DEFAULT 0, \
             folder_id TEXT, \
             is_pinned INTEGER NOT NULL DEFAULT 0, \
             pinned_at TEXT, \
             session_type TEXT NOT NULL DEFAULT 'transcription', \
             wav_file_path TEXT, \
             wav_duration_seconds REAL, \
             sort_order INTEGER NOT NULL DEFAULT 0"
        }
        "segments" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             session_id TEXT NOT NULL DEFAULT '', \
             source TEXT NOT NULL DEFAULT '', \
             text TEXT NOT NULL DEFAULT '', \
             audio_offset_seconds REAL NOT NULL DEFAULT 0, \
             chunk_duration_seconds REAL NOT NULL DEFAULT 0, \
             confidence REAL NOT NULL DEFAULT 1.0, \
             created_at TEXT NOT NULL DEFAULT (datetime('now')), \
             chunk_index INTEGER NOT NULL DEFAULT 0, \
             original_text TEXT, \
             edited_at TEXT, \
             deleted_at TEXT, \
             hidden INTEGER NOT NULL DEFAULT 0, \
             speaker_id INTEGER"
        }
        "notes" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             session_id TEXT NOT NULL DEFAULT '', \
             content TEXT NOT NULL DEFAULT '', \
             updated_at TEXT NOT NULL DEFAULT (datetime('now'))"
        }
        "note_versions" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             note_id TEXT NOT NULL DEFAULT '', \
             content TEXT NOT NULL DEFAULT '', \
             created_at TEXT NOT NULL DEFAULT (datetime('now'))"
        }
        "folders" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             name TEXT NOT NULL DEFAULT '', \
             parent_id TEXT, \
             sort_order INTEGER NOT NULL DEFAULT 0, \
             created_at TEXT NOT NULL DEFAULT (datetime('now')), \
             updated_at TEXT NOT NULL DEFAULT (datetime('now')), \
             icon TEXT, \
             color TEXT, \
             description TEXT"
        }
        "session_folders" => {
            "session_id TEXT NOT NULL, \
             folder_id TEXT NOT NULL, \
             created_at TEXT NOT NULL DEFAULT (datetime('now')), \
             PRIMARY KEY (session_id, folder_id)"
        }
        "chat_messages" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             context_key TEXT NOT NULL DEFAULT '', \
             session_id TEXT, \
             role TEXT NOT NULL DEFAULT '', \
             content TEXT NOT NULL DEFAULT '', \
             action TEXT, \
             created_at TEXT NOT NULL DEFAULT (datetime('now')), \
             tool_calls TEXT, \
             send_id TEXT, \
             sequence INTEGER, \
             tool_call_id TEXT, \
             observation TEXT, \
             status TEXT"
        }
        "dictation_history" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             slot_id TEXT NOT NULL DEFAULT '', \
             slot_name TEXT NOT NULL DEFAULT '', \
             input_text TEXT NOT NULL DEFAULT '', \
             output_text TEXT NOT NULL DEFAULT '', \
             ai_enabled INTEGER NOT NULL DEFAULT 0, \
             ai_prompt TEXT, \
             output_action TEXT NOT NULL DEFAULT '', \
             wav_file_path TEXT, \
             wav_duration_seconds REAL, \
             session_id TEXT, \
             created_at TEXT NOT NULL DEFAULT (datetime('now'))"
        }
        "tags" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             name TEXT NOT NULL DEFAULT '', \
             color TEXT, \
             created_at TEXT NOT NULL DEFAULT (datetime('now'))"
        }
        "session_tags" => {
            "session_id TEXT NOT NULL, \
             tag_id TEXT NOT NULL, \
             source TEXT NOT NULL DEFAULT 'manual', \
             confidence REAL, \
             created_at TEXT NOT NULL DEFAULT (datetime('now')), \
             PRIMARY KEY (session_id, tag_id)"
        }
        "session_audio_parts" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             session_id TEXT NOT NULL DEFAULT '', \
             part_index INTEGER NOT NULL DEFAULT 0, \
             file_path TEXT NOT NULL DEFAULT '', \
             format TEXT NOT NULL DEFAULT 'wav' CHECK (format IN ('wav','mp3')), \
             duration_seconds REAL NOT NULL DEFAULT 0, \
             sample_rate INTEGER NOT NULL DEFAULT 0, \
             created_at TEXT NOT NULL DEFAULT (datetime('now'))"
        }
        "shares" => {
            "id TEXT NOT NULL PRIMARY KEY, \
             folder_id TEXT NOT NULL DEFAULT '', \
             shared_with_email TEXT, \
             permission TEXT NOT NULL DEFAULT 'viewer', \
             created_at TEXT NOT NULL DEFAULT (datetime('now')), \
             expires_at TEXT"
        }
        other => return Err(SyncError::UnknownTable(other.to_string())),
    })
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
    let body = rebuild_body(table)?;
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
