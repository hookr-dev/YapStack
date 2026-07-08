// SPDX-License-Identifier: AGPL-3.0-only
//! Pinned merge semantics (SYNC_REMEDIATION.md §1).
//!
//! Two facts about cr-sqlite's LWW merge are load-bearing for the v1 sync posture, so they
//! are pinned here as assertions rather than folklore:
//!
//!  1. **Independent-UUID data is a lossless UNION.** Two devices that only ever write rows
//!     with DISTINCT primary keys (the reality — every synced table uses TEXT UUID PKs)
//!     cannot collide, so a bidirectional merge keeps every row from both sides.
//!
//!  2. **Shared-ancestry merge is silently LOSSY.** If a library is manually copied between
//!     machines (same row UUIDs) and then the SAME row's SAME column is edited on both, the
//!     equal-`col_version` tiebreak decides by VALUE (then site_id), NOT by causal recency —
//!     so the temporally-newer edit can be dropped with no conflict surfaced. This is the
//!     documented v1 limitation ("don't copy libraries between machines — sign in and sync
//!     instead"). The lossiness test below EXPECTS the lossy outcome: if cr-sqlite ever
//!     changes this behavior the test fails and we revisit the posture.

mod support;

use rusqlite::Connection;

use yapstack_sync::reconcile::resite_as_fresh_peer;
use yapstack_sync::snapshot::{produce_snapshot_bytes, write_snapshot_file};
use yapstack_sync::CrsqlDb;

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn make_kv_crr(db: &CrsqlDb) {
    db.conn()
        .execute_batch("CREATE TABLE kv(id TEXT NOT NULL PRIMARY KEY, v TEXT NOT NULL DEFAULT '');")
        .unwrap();
    db.conn()
        .query_row("SELECT crsql_as_crr('kv')", [], |_| Ok(()))
        .unwrap();
}

fn kv(conn: &Connection, id: &str) -> String {
    conn.query_row("SELECT v FROM kv WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

/// Positive companion to the lossiness pin: two INDEPENDENT sites (fresh site ids) writing
/// DISTINCT UUID rows merge to a clean union — every row from both survives, no conflict.
#[test]
fn independent_uuid_writes_merge_to_a_lossless_union() {
    let a = CrsqlDb::open_in_memory().unwrap();
    let b = CrsqlDb::open_in_memory().unwrap();
    make_kv_crr(&a);
    make_kv_crr(&b);
    assert_ne!(
        a.site_id().unwrap(),
        b.site_id().unwrap(),
        "independent installs mint distinct site ids"
    );

    // Each device writes a row with its OWN unique UUID PK — no collision is possible.
    a.conn()
        .execute("INSERT INTO kv(id,v) VALUES('uuid-A','A-only')", [])
        .unwrap();
    b.conn()
        .execute("INSERT INTO kv(id,v) VALUES('uuid-B','B-only')", [])
        .unwrap();

    // Bidirectional merge (mirrors the runtime pull path both ways).
    support::direct_merge(a.conn(), b.conn(), 0);
    support::direct_merge(b.conn(), a.conn(), 0);

    // Lossless union: BOTH rows present on BOTH sites, unchanged.
    for db in [&a, &b] {
        assert_eq!(
            count(db.conn(), "SELECT count(*) FROM kv"),
            2,
            "union of both rows"
        );
        assert_eq!(kv(db.conn(), "uuid-A"), "A-only");
        assert_eq!(kv(db.conn(), "uuid-B"), "B-only");
    }
}

/// PINNED KNOWN LIMITATION (SYNC_REMEDIATION.md §1): shared-ancestry merge is silently
/// lossy. One CRR database is copied to two sites (identical row UUIDs), each edits the SAME
/// row's SAME column, and — because the equal-`col_version` tiebreak keeps the GREATER value,
/// not the causally-newer write — the temporally-newer edit is DROPPED with no conflict
/// surfaced. This test intentionally EXPECTS that loss; if cr-sqlite's tiebreak ever changes,
/// it fails and we revisit the "don't copy libraries between machines" posture.
#[test]
fn shared_ancestry_merge_silently_drops_the_newer_edit() {
    let dir = tempfile::tempdir().unwrap();

    // Build the shared base library: one CRR row at col_version=1.
    let base = dir.path().join("base.sync.db");
    {
        let db = CrsqlDb::open(&base).unwrap();
        make_kv_crr(&db);
        db.conn()
            .execute("INSERT INTO kv(id,v) VALUES('k1','base')", [])
            .unwrap();
    }

    // Copy the base CRR db to two sites (rows + clocks + identical UUIDs) — the manual
    // "copied the library file between machines" scenario.
    let a_path = dir.path().join("a.sync.db");
    let b_path = dir.path().join("b.sync.db");
    let bytes = produce_snapshot_bytes(&base, dir.path()).unwrap();
    write_snapshot_file(&a_path, &bytes).unwrap();
    write_snapshot_file(&b_path, &bytes).unwrap();

    // Re-site each copy so they are independent peers (distinct site ids) that nonetheless
    // SHARE the copied row's PK — the essence of shared ancestry. Re-site demands a reopen so
    // cr-sqlite re-reads the fresh ordinal-0 site id.
    for p in [&a_path, &b_path] {
        let db = CrsqlDb::open(p).unwrap();
        resite_as_fresh_peer(db.conn()).unwrap();
    }
    let a = CrsqlDb::open(&a_path).unwrap();
    let b = CrsqlDb::open(&b_path).unwrap();
    assert_ne!(a.site_id().unwrap(), b.site_id().unwrap());

    // The winner is decided by VALUE, not by time: cr-sqlite keeps the GREATER value on an
    // equal-col_version tie. We deliberately give the OLDER (first) edit the greater value so
    // the NEWER (second) edit is the one that loses.
    let winning_older = "zzz-site-A-edited-FIRST";
    let losing_newer = "aaa-site-B-edited-SECOND";
    assert!(
        winning_older > losing_newer,
        "test fixture: the older edit must sort greater so it wins the value tiebreak"
    );

    // Site A edits k1 FIRST (older in wall-clock); site B edits the SAME column SECOND.
    a.conn()
        .execute("UPDATE kv SET v=?1 WHERE id='k1'", [winning_older])
        .unwrap();
    b.conn()
        .execute("UPDATE kv SET v=?1 WHERE id='k1'", [losing_newer])
        .unwrap();

    // Merge both directions.
    support::direct_merge(a.conn(), b.conn(), 0);
    support::direct_merge(b.conn(), a.conn(), 0);

    // Both sites converge (no divergence) — but to the OLDER edit. The newer edit is gone,
    // silently. THIS IS THE PINNED LOSSY OUTCOME.
    assert_eq!(kv(a.conn(), "k1"), winning_older);
    assert_eq!(
        kv(b.conn(), "k1"),
        winning_older,
        "both converge to the value-tiebreak winner (the OLDER edit)"
    );
    assert_ne!(
        kv(b.conn(), "k1"),
        losing_newer,
        "the causally-newer edit was silently dropped — shared-ancestry lossiness (§1)"
    );
}
