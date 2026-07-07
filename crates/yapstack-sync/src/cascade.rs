// SPDX-License-Identifier: AGPL-3.0-only
//! R4: app-layer cascade / tombstone GC.
//!
//! The CRR migration strips every checked FK (cr-sqlite forbids them), so the
//! `ON DELETE CASCADE` / `ON DELETE SET NULL` semantics of the original schema
//! (T001 Deliverable 4) no longer fire. A hard FK cascade would also be wrong under
//! a CRDT: a delete and a concurrent child-insert must converge, not race. Instead
//! we run a GC pass that, on the CONVERGED state, deletes orphaned children (through
//! the CRDT, so the delete propagates as a tombstone) and nulls dangling SET-NULL
//! references. Because it keys off synced column values, every device computes the
//! same orphan set and converges. Idempotent; run after each merge.

use rusqlite::Connection;

use crate::SyncError;

/// Per-relationship counts from one GC pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CascadeStats {
    pub deleted: usize,
    pub nulled: usize,
}

/// Child tables whose rows are deleted when their parent PK is gone (the stripped
/// `ON DELETE CASCADE` sites). `(child, fk_col, parent, parent_pk, fk_nullable)`.
const CASCADE_DELETE: &[(&str, &str, &str, &str, bool)] = &[
    ("segments", "session_id", "sessions", "id", false),
    ("notes", "session_id", "sessions", "id", false),
    ("note_versions", "note_id", "notes", "id", false),
    ("chat_messages", "session_id", "sessions", "id", true),
    ("session_audio_parts", "session_id", "sessions", "id", false),
    ("shares", "folder_id", "folders", "id", false),
];

/// `(child, (fk_a, parent_a), (fk_b, parent_b))` for a two-parent junction table.
type Junction = (
    &'static str,
    (&'static str, &'static str),
    (&'static str, &'static str),
);

/// Junction tables cascaded on EITHER parent (`session_folders`, `session_tags`).
const CASCADE_JUNCTION: &[Junction] = &[
    (
        "session_folders",
        ("session_id", "sessions"),
        ("folder_id", "folders"),
    ),
    (
        "session_tags",
        ("session_id", "sessions"),
        ("tag_id", "tags"),
    ),
];

/// SET-NULL sites: null the FK column when its referent is gone. `(table, fk_col,
/// parent, parent_pk)`.
const SET_NULL: &[(&str, &str, &str, &str)] = &[
    ("folders", "parent_id", "folders", "id"),
    ("sessions", "folder_id", "folders", "id"),
];

/// Run the full cascade/tombstone GC pass. Deletes/updates go through cr-sqlite's
/// CRR triggers, so they become synced tombstones. Runs in one transaction.
pub fn cascade_gc(conn: &Connection) -> Result<CascadeStats, SyncError> {
    conn.execute_batch("BEGIN;")?;
    let mut stats = CascadeStats::default();
    let mut inner = || -> Result<(), SyncError> {
        for (child, fk, parent, ppk, nullable) in CASCADE_DELETE {
            let null_guard = if *nullable {
                format!("\"{fk}\" IS NOT NULL AND ")
            } else {
                String::new()
            };
            let sql = format!(
                "DELETE FROM \"{child}\" \
                 WHERE {null_guard}\"{fk}\" NOT IN (SELECT \"{ppk}\" FROM \"{parent}\")"
            );
            stats.deleted += conn.execute(&sql, [])?;
        }
        for (child, (fka, pa), (fkb, pb)) in CASCADE_JUNCTION {
            let sql = format!(
                "DELETE FROM \"{child}\" WHERE \
                 \"{fka}\" NOT IN (SELECT id FROM \"{pa}\") OR \
                 \"{fkb}\" NOT IN (SELECT id FROM \"{pb}\")"
            );
            stats.deleted += conn.execute(&sql, [])?;
        }
        for (table, fk, parent, ppk) in SET_NULL {
            let sql = format!(
                "UPDATE \"{table}\" SET \"{fk}\" = NULL \
                 WHERE \"{fk}\" IS NOT NULL AND \
                 \"{fk}\" NOT IN (SELECT \"{ppk}\" FROM \"{parent}\")"
            );
            stats.nulled += conn.execute(&sql, [])?;
        }
        Ok(())
    };
    if let Err(e) = inner() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(e);
    }
    conn.execute_batch("COMMIT;")?;
    Ok(stats)
}
