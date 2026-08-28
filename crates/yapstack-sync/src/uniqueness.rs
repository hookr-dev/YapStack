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
    //
    // `BEGIN IMMEDIATE`, not a deferred `BEGIN`: each dedup pass READS (the proven-column
    // clock joins, and `dedup_tags` first SELECTs its loser→winner pairs) before it WRITES, so
    // a deferred transaction has to upgrade read→write, and in WAL a lost upgrade race returns
    // `SQLITE_BUSY_SNAPSHOT` — which the busy handler does NOT retry, so it fails outright
    // regardless of `busy_timeout` (same rationale as `quarantine::merge_changeset`).
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
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
///
/// Only rows whose `session_id` (the uniqueness key) AND `updated_at` (the LWW winner column)
/// are BOTH PROVEN merged values participate: a mid-row pull can materialize `notes.session_id`
/// at its CRR-rebuild synthetic default (making distinct notes collide spuriously) OR
/// materialize `updated_at` at its live `datetime('now')` default (making a partial note
/// out-rank and tombstone a genuine, complete note that carries an older real timestamp). The
/// destructive comparison keys off `updated_at`, so it too must be proven. Same proven-column
/// gate as [`dedup_audio_parts`], extended to the winner column.
fn dedup_notes(conn: &Connection) -> Result<usize, SyncError> {
    let proven = "SELECT pk.id FROM \"notes__crsql_pks\" pk \
        JOIN \"notes__crsql_clock\" cs ON cs.key = pk.__crsql_key AND cs.col_name = 'session_id' \
        JOIN \"notes__crsql_clock\" cu ON cu.key = pk.__crsql_key AND cu.col_name = 'updated_at'";
    // A note is a loser if, within its session_id group, another note sorts higher
    // by (updated_at DESC, id ASC).
    let sql = format!(
        "DELETE FROM notes WHERE id IN (\
        SELECT n.id FROM notes n \
        JOIN notes w ON w.session_id = n.session_id AND w.id <> n.id \
        WHERE ((w.updated_at > n.updated_at) \
           OR (w.updated_at = n.updated_at AND w.id < n.id)) \
          AND n.id IN ({proven}) AND w.id IN ({proven}))"
    );
    Ok(conn.execute(&sql, [])?)
}

/// Tag names unique case-insensitively. Winner per NOCASE name = lowest `id`.
/// Repoint `session_tags.tag_id` from losers to the winner (dedup associations),
/// drop now-duplicate junction rows, then delete loser tags.
///
/// Only tags whose `name` (the uniqueness key) is a PROVEN merged value participate: a
/// mid-row pull can materialize `tags.name` at its CRR-rebuild synthetic default, making
/// distinct tags collide spuriously and merge away a real tag permanently. The LWW winner
/// here is the lowest `id` — the PRIMARY KEY, which every row carries (it can never sit at a
/// synthetic default), so the `name` proof already covers the winner column with nothing more
/// to gate. Same proven-column gate as [`dedup_audio_parts`].
fn dedup_tags(conn: &Connection) -> Result<usize, SyncError> {
    let proven = "SELECT pk.id FROM \"tags__crsql_pks\" pk \
        JOIN \"tags__crsql_clock\" cs ON cs.key = pk.__crsql_key AND cs.col_name = 'name'";
    // Map each loser tag id -> winner id.
    let pairs: Vec<(String, String)> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT t.id AS loser, \
                    (SELECT m.id FROM tags m \
                     WHERE m.name = t.name COLLATE NOCASE AND m.id IN ({proven}) \
                     ORDER BY m.id ASC LIMIT 1) AS winner \
             FROM tags t WHERE t.id IN ({proven})",
        ))?;
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
///
/// Only rows whose BOTH key columns are PROVEN merged values participate: a mid-row pull
/// can materialize `session_audio_parts` with `part_index` at its CRR-rebuild synthetic
/// default `0` (its column change simply has not arrived yet), which would collide
/// spuriously with a genuine part 0 and tombstone it permanently. A column is proven when
/// `{table}__crsql_clock` carries an entry for it (present for every real insert/merge, see
/// tableinfo.rs; absent for a synthetic default) — the dedup analog of cascade's
/// proven-delete gate.
fn dedup_audio_parts(conn: &Connection) -> Result<usize, SyncError> {
    let proven = "SELECT pk.id FROM \"session_audio_parts__crsql_pks\" pk \
        JOIN \"session_audio_parts__crsql_clock\" cs ON cs.key = pk.__crsql_key AND cs.col_name = 'session_id' \
        JOIN \"session_audio_parts__crsql_clock\" cp ON cp.key = pk.__crsql_key AND cp.col_name = 'part_index'";
    let sql = format!(
        "DELETE FROM session_audio_parts WHERE id IN (\
         SELECT a.id FROM session_audio_parts a \
         JOIN session_audio_parts w \
           ON w.session_id = a.session_id AND w.part_index = a.part_index AND w.id < a.id \
         WHERE a.id IN ({proven}) AND w.id IN ({proven}))"
    );
    Ok(conn.execute(&sql, [])?)
}
