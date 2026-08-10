// SPDX-License-Identifier: AGPL-3.0-only
//! RED proving tests for the `data-integrity` cluster (SYNC_REVIEW_FINDINGS.md).
//!
//! Both tests assert the CONVERGED invariant that the design guarantees and that a
//! correct fix must restore. They FAIL on the reviewed tree (HEAD 70e33d3) because
//! the post-merge maintenance passes (`enforce_uniqueness` / `cascade_gc`) run on a
//! NON-converged pull and make PERMANENT, GLOBALLY-propagating CRDT deletions.
//!
//! These are proving harnesses, not the fix. A correct fix (row-atomic chunking in
//! `plan_capture_chunks`, and/or gating the maintenance passes on a fully-drained
//! pull, and/or cascading only on a proven parent delete) turns them green.

mod support;

use rusqlite::Connection;
use yapstack_sync::cascade::cascade_gc;
use yapstack_sync::change::{read_local_changes_since, Changeset};
use yapstack_sync::quarantine::merge_changeset;
use yapstack_sync::schema;
use yapstack_sync::uniqueness::enforce_uniqueness;
use yapstack_sync::CrsqlDb;

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// A device at the real migrated (CRR) schema. `populate` seeds the faithful
/// inter-referential library from the shared fixture; an unpopulated device is an
/// independent site with NO shared ancestry (the realistic "fresh peer" pull path).
fn migrated(populate: bool) -> CrsqlDb {
    let db = CrsqlDb::open_in_memory().unwrap();
    support::create_original_schema(db.conn());
    if populate {
        support::populate(db.conn());
    }
    schema::crr_migrate(db.conn()).unwrap();
    db
}

/// FINDING: `enforce_uniqueness` deletes partially-merged audio-part rows; the
/// tombstone is permanent (uniqueness.rs:29-38, 91-97).
///
/// A pull page can stop at a chunk boundary that falls INSIDE one logical row
/// (`plan_capture_chunks`, outbox.rs:343-360, has no (table,pk) awareness). A
/// `session_audio_parts` row split right after `session_id` materializes via
/// cr-sqlite's per-column merge_insert with `part_index` at its CRR-rebuild synthetic
/// default `0` (schema.rs synthetic_default). That collides with the session's
/// genuine part 0, and the drain runs `enforce_uniqueness` unconditionally after any
/// applied>0 merge (sync.rs:3549-3557) — deleting one of the two through the CRR
/// delete trigger (tombstone at causal length 2). When the split row's real columns
/// arrive later (cl 1) they are dropped against the cl-2 tombstone, so a genuine,
/// distinct audio part is destroyed permanently on every device.
#[test]
fn dedup_must_not_destroy_a_partially_merged_audio_part() {
    // Device A authors session s0 with its genuine part `ap0` (part_index 0) plus a
    // second, DISTINCT part `ap-zzz` (part_index 5). `ap-zzz` < `ap0` lexically, so it
    // is the dedup "winner" — which is exactly what makes the GENUINE part 0 the loser.
    let a = migrated(true);
    a.conn()
        .execute(
            "INSERT INTO session_audio_parts(id,session_id,part_index,file_path,format,duration_seconds,sample_rate) \
             VALUES ('ap-zzz','s0',5,'/audio/extra.wav','wav',2.0,16000)",
            [],
        )
        .unwrap();

    let cs = read_local_changes_since(a.conn(), 0).unwrap();

    // Split `ap-zzz` at the boundary right after `session_id`: its remaining cells
    // (part_index, file_path, ...) belong to a LATER push request / pull page.
    let mut page1 = Changeset::default();
    let mut page2 = Changeset::default();
    for r in &cs.rows {
        let pk = String::from_utf8_lossy(&r.pk).to_string();
        let is_split_row = r.table == "session_audio_parts" && pk.contains("ap-zzz");
        let is_head = r.cid == "-1" || r.cid == "session_id";
        if is_split_row && !is_head {
            page2.rows.push(r.clone());
        } else {
            page1.rows.push(r.clone());
        }
    }

    // Device B — a fresh peer with no shared ancestry — drains pull page 1 to the tip.
    let b = migrated(false);
    let (applied, _q) = merge_changeset(b.conn(), &page1).unwrap();
    assert!(
        applied > 0,
        "page 1 must apply changes so the drain runs maintenance"
    );

    // The truncated `ap-zzz` materialized with part_index defaulted to 0 — a spurious
    // collision with the genuine `ap0` that exists ONLY because the pull is mid-row.
    assert_eq!(
        count(
            b.conn(),
            "SELECT count(*) FROM session_audio_parts WHERE session_id='s0' AND part_index=0"
        ),
        2,
        "precondition: partial merge yields a transient (session_id, part_index=0) collision"
    );

    // The drain runs `enforce_uniqueness` after any applied>0 merge — on this
    // NON-converged, mid-row state (sync.rs:3549-3557).
    enforce_uniqueness(b.conn()).unwrap();

    // Pull page 2 delivers `ap-zzz`'s real columns (part_index=5, file_path, ...).
    merge_changeset(b.conn(), &page2).unwrap();

    // CONVERGED INVARIANT: session s0 has BOTH parts — the genuine part 0 and part 5.
    // On the reviewed tree the genuine `ap0` was tombstoned at cl 2 and never comes
    // back, so only one part survives. THIS ASSERTION FAILS ON THE BUGGY TREE.
    assert_eq!(
        count(
            b.conn(),
            "SELECT count(*) FROM session_audio_parts WHERE session_id='s0'"
        ),
        2,
        "converged state must keep both distinct audio parts (0 and 5) — the drain \
         must not permanently delete a genuine part it saw only mid-row"
    );
    assert_eq!(
        count(
            b.conn(),
            "SELECT count(*) FROM session_audio_parts \
             WHERE session_id='s0' AND part_index=0 AND file_path='/audio/0.wav'"
        ),
        1,
        "the genuine part 0 (with its real file_path) must survive"
    );

    // Global permanence: the dedup tombstone propagates back to the AUTHORING device,
    // destroying the genuine part there too.
    support::direct_merge(b.conn(), a.conn(), 0);
    assert_eq!(
        count(
            a.conn(),
            "SELECT count(*) FROM session_audio_parts WHERE session_id='s0'"
        ),
        2,
        "the author device must not lose its genuine audio part to the peer's tombstone"
    );
}

/// FINDING: `cascade_gc` deletes live children after the crypto quarantine advances
/// past a parent (cascade.rs:26-97).
///
/// `pull_direction` deliberately ADVANCES past any changeset it cannot decrypt/decode
/// (outbox.rs:940-962, quarantine-and-advance), so a device can hold a session's
/// children while the session's own insert sits in `_yapstack_crypto_quarantine`.
/// The drain then runs `cascade_gc` on any applied>0 merge (sync.rs:3549-3554,
/// `crypto_quarantined` never consulted), and `DELETE FROM child WHERE fk NOT IN
/// (SELECT pk FROM parent)` (cascade.rs:69-88) treats "parent never merged" as
/// "parent deleted". The children are tombstoned at cl 2. A later
/// `retry_crypto_quarantine` (sync.rs:4701) restores the parent, but the children's
/// cl-1 inserts are dropped against the cl-2 tombstone — permanent, global loss.
///
/// This test constructs the exact DB state the quarantine-and-advance produces
/// (children present, parent absent) by withholding the parent's changeset from the
/// merge; it does not depend on the crypto layer, only on the ordering it breaks.
#[test]
fn cascade_gc_must_not_destroy_children_of_a_not_yet_merged_parent() {
    // Device C authors a complete session S with children.
    let c = migrated(false);
    c.conn()
        .execute(
            "INSERT INTO sessions(id,title) VALUES('S','My Session')",
            [],
        )
        .unwrap();
    for g in 0..3 {
        c.conn()
            .execute(
                "INSERT INTO segments(id,session_id,source,text,audio_offset_seconds,chunk_duration_seconds) \
                 VALUES (?1,'S','mic',?2,?3,1.0)",
                rusqlite::params![format!("seg-{g}"), format!("text {g}"), g as f64],
            )
            .unwrap();
    }
    c.conn()
        .execute(
            "INSERT INTO notes(id,session_id,content) VALUES('nS','S','note')",
            [],
        )
        .unwrap();

    let cs = read_local_changes_since(c.conn(), 0).unwrap();

    // Split off session S's OWN row (the parent) — it is quarantined on B, so
    // pull_direction advances past it and merges the children without it.
    let mut without_parent = Changeset::default();
    let mut parent_only = Changeset::default();
    for r in &cs.rows {
        let pk = String::from_utf8_lossy(&r.pk).to_string();
        if r.table == "sessions" && pk.contains('S') {
            parent_only.rows.push(r.clone());
        } else {
            without_parent.rows.push(r.clone());
        }
    }

    // Device B merges the children; the parent stays quarantined (absent).
    let b = migrated(false);
    let (applied, _q) = merge_changeset(b.conn(), &without_parent).unwrap();
    assert!(
        applied > 0,
        "children must apply so the drain runs cascade_gc"
    );
    assert_eq!(
        count(
            b.conn(),
            "SELECT count(*) FROM segments WHERE session_id='S'"
        ),
        3,
        "precondition: children present"
    );
    assert_eq!(
        count(b.conn(), "SELECT count(*) FROM sessions WHERE id='S'"),
        0,
        "precondition: parent absent (quarantined)"
    );

    // The drain runs `cascade_gc` after any applied>0 merge, ignoring the quarantine.
    cascade_gc(b.conn()).unwrap();

    // Later: `retry_crypto_quarantine` restores the parent.
    merge_changeset(b.conn(), &parent_only).unwrap();
    assert_eq!(
        count(b.conn(), "SELECT count(*) FROM sessions WHERE id='S'"),
        1,
        "parent restored"
    );

    // CONVERGED INVARIANT: parent present ⇒ its children present. On the reviewed tree
    // the children were tombstoned at cl 2 and the restore cannot bring them back.
    // THESE ASSERTIONS FAIL ON THE BUGGY TREE.
    assert_eq!(
        count(
            b.conn(),
            "SELECT count(*) FROM segments WHERE session_id='S'"
        ),
        3,
        "children must survive a parent that was merely not-yet-merged"
    );
    assert_eq!(
        count(b.conn(), "SELECT count(*) FROM notes WHERE session_id='S'"),
        1,
        "the note child must survive too"
    );

    // Global permanence: the cascade tombstones propagate back to author C.
    support::direct_merge(b.conn(), c.conn(), 0);
    assert_eq!(
        count(
            c.conn(),
            "SELECT count(*) FROM segments WHERE session_id='S'"
        ),
        3,
        "the author device must not lose its children to the peer's cascade tombstone"
    );
}
