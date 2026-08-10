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

/// SQL scalar subquery yielding the `ppk` values of parent rows that are KNOWN-DELETED
/// (their `{parent}__crsql_clock` delete sentinel `'-1'` exists at even causal length —
/// cr-sqlite stamps a live row's sentinel odd and a deleted row's sentinel even, see
/// tableinfo.rs mark_locally_created/deleted). A never-merged / never-seen parent has NO
/// clock entry and is therefore ABSENT from this set, so its children are retained
/// (orphans of a not-yet-merged parent are correct CRDT state, not garbage).
fn proven_deleted_pks(parent: &str, ppk: &str) -> String {
    format!(
        "SELECT p.\"{ppk}\" FROM \"{parent}__crsql_pks\" p \
         JOIN \"{parent}__crsql_clock\" c ON c.key = p.__crsql_key \
         WHERE c.col_name = '-1' AND c.col_version % 2 = 0"
    )
}

/// Run the full cascade/tombstone GC pass. Deletes/updates go through cr-sqlite's
/// CRR triggers, so they become synced tombstones. Runs in one transaction.
pub fn cascade_gc(conn: &Connection) -> Result<CascadeStats, SyncError> {
    // `new_unchecked` because we hold only a `&Connection` here, not `&mut`.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;
    let mut stats = CascadeStats::default();
    for (child, fk, parent, ppk, _nullable) in CASCADE_DELETE {
        // Cascade a delete only when the parent is PROVEN deleted — never on mere
        // absence, which a quarantine-and-advanced or DB-restored device reaches with
        // the parent still un-merged. A `NULL` fk never matches an `IN` set, so the
        // nullable case needs no separate guard.
        let sql = format!(
            "DELETE FROM \"{child}\" WHERE \"{fk}\" IN ({})",
            proven_deleted_pks(parent, ppk)
        );
        stats.deleted += tx.execute(&sql, [])?;
    }
    for (child, (fka, pa), (fkb, pb)) in CASCADE_JUNCTION {
        // A junction row dies when EITHER parent is proven-deleted.
        let sql = format!(
            "DELETE FROM \"{child}\" WHERE \"{fka}\" IN ({}) OR \"{fkb}\" IN ({})",
            proven_deleted_pks(pa, "id"),
            proven_deleted_pks(pb, "id")
        );
        stats.deleted += tx.execute(&sql, [])?;
    }
    for (table, fk, parent, ppk) in SET_NULL {
        // SET-NULL is non-destructive, so nulling on mere absence is acceptable here
        // (a resurrected parent can simply be re-pointed) — no proven-delete gate.
        let sql = format!(
            "UPDATE \"{table}\" SET \"{fk}\" = NULL \
             WHERE \"{fk}\" IS NOT NULL AND \
             \"{fk}\" NOT IN (SELECT \"{ppk}\" FROM \"{parent}\")"
        );
        stats.nulled += tx.execute(&sql, [])?;
    }
    tx.commit()?;
    Ok(stats)
}
