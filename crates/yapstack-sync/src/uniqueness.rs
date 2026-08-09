// SPDX-License-Identifier: AGPL-3.0-only
//! R5: app-side enforcement of the three dropped non-PK UNIQUE constraints.
//!
//! `is_table_compatible` forbids non-PK UNIQUE indices, so the migration drops
//! three: `notes.session_id` (one note per session), `tags.name` (COLLATE NOCASE),
//! and `session_audio_parts(session_id, part_index)`.
//!
//! After a merge two devices can hold distinct-PK rows that collide on these keys
//! (T003 scenario 2). We resolve each collision to a DETERMINISTIC winner using only
//! synced column values, so every device picks the SAME winner and converges. Losers
//! are deleted through the CRDT (tombstone); for tags, references are first
//! repointed to the winner so associations survive the dedup. Idempotent; run after
//! each merge.

use rusqlite::Connection;

use crate::SyncError;

/// Counts from one uniqueness pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UniquenessStats {
    pub notes_removed: usize,
    pub tags_merged: usize,
    pub audio_parts_removed: usize,
}

/// Enforce all three dropped uniqueness constraints. One transaction.
pub fn enforce_uniqueness(conn: &Connection) -> Result<UniquenessStats, SyncError> {
    // `new_unchecked` because we hold only a `&Connection` here, not `&mut`.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;
    let stats = UniquenessStats {
        notes_removed: dedup_notes(&tx)?,
        tags_merged: dedup_tags(&tx)?,
        audio_parts_removed: dedup_audio_parts(&tx)?,
    };
    tx.commit()?;
    Ok(stats)
}

/// One note per session. Winner = freshest `updated_at`, tie-break lowest `id`
/// (both synced → deterministic). Losing notes are deleted.
fn dedup_notes(conn: &Connection) -> Result<usize, SyncError> {
    // A note is a loser if, within its session_id group, another note sorts higher
    // by (updated_at DESC, id ASC).
    let sql = "DELETE FROM notes WHERE id IN (\
        SELECT n.id FROM notes n \
        JOIN notes w ON w.session_id = n.session_id AND w.id <> n.id \
        WHERE (w.updated_at > n.updated_at) \
           OR (w.updated_at = n.updated_at AND w.id < n.id))";
    Ok(conn.execute(sql, [])?)
}

/// Tag names unique case-insensitively. Winner per NOCASE name = lowest `id`.
/// Repoint `session_tags.tag_id` from losers to the winner (dedup associations),
/// drop now-duplicate junction rows, then delete loser tags.
fn dedup_tags(conn: &Connection) -> Result<usize, SyncError> {
    // Map each loser tag id -> winner id.
    let pairs: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT t.id AS loser, \
                    (SELECT m.id FROM tags m \
                     WHERE m.name = t.name COLLATE NOCASE \
                     ORDER BY m.id ASC LIMIT 1) AS winner \
             FROM tags t",
        )?;
        let all = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        all.into_iter().filter(|(l, w)| l != w).collect()
    };
    let mut merged = 0usize;
    for (loser, winner) in &pairs {
        // `tag_id` is part of the session_tags PRIMARY KEY, and cr-sqlite forbids
        // UPDATE of a PK column on a CRR — so repoint by DELETE + INSERT: recreate
        // the winner association for every session tagged by the loser that the
        // winner does not already tag, then drop all loser associations.
        conn.execute(
            "INSERT INTO session_tags(session_id, tag_id, source, confidence, created_at) \
             SELECT session_id, ?1, source, confidence, created_at FROM session_tags \
             WHERE tag_id = ?2 \
               AND session_id NOT IN (SELECT session_id FROM session_tags WHERE tag_id = ?1)",
            rusqlite::params![winner, loser],
        )?;
        conn.execute("DELETE FROM session_tags WHERE tag_id = ?1", [loser])?;
        merged += conn.execute("DELETE FROM tags WHERE id = ?1", [loser])?;
    }
    Ok(merged)
}

/// `(session_id, part_index)` unique. Winner = lowest `id`; losers deleted.
fn dedup_audio_parts(conn: &Connection) -> Result<usize, SyncError> {
    let sql = "DELETE FROM session_audio_parts WHERE id IN (\
        SELECT a.id FROM session_audio_parts a \
        JOIN session_audio_parts w \
          ON w.session_id = a.session_id AND w.part_index = a.part_index AND w.id < a.id)";
    Ok(conn.execute(sql, [])?)
}
