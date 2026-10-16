// SPDX-License-Identifier: AGPL-3.0-only
//! LIVE BUG regression: `Failed to delete segment: expected 29 values got 27`.
//!
//! Owner-confirmed on Windows: right-click segment edit / soft-delete / hide failed
//! while inserts and sync kept working. Root cause: the
//! device CRRified `segments` at the 13-column shape, then a build that predates the
//! db.ts CRR gate applied `ALTER TABLE segments ADD COLUMN speaker_id INTEGER` as a
//! BARE ALTER — outside the `crsql_begin_alter`/`crsql_commit_alter` dance. cr-sqlite
//! ACCEPTS that (pinned in `remediations::bare_alter_add_column_on_crr_table_is_accepted_not_rejected`)
//! and does not regenerate the AFTER UPDATE trigger, so:
//!
//!   * `x_crsql_after_update` re-derives its expected arity from the LIVE shape:
//!     `1 + pks*2 + non_pks*2` = 1 + 2 + 26 = **29** (vendor `local_writes/after_update.rs:43-56`),
//!   * the frozen trigger text still passes the 13-column list = 1 + 2 + 24 = **27**
//!     (vendor `triggers.rs:39-76`),
//!   * INSERT/DELETE triggers pass PK values only, so they stay correct — exactly the
//!     reported symptom.
//!
//! `apply_out_of_band_alters` cannot repair it (it skips any column that already
//! exists), which is why the R11 coverage — which only exercised the incoming-change
//! APPLY path — never caught it. `heal_stale_crr_triggers` is the repair.

mod support;

use rusqlite::Connection;
use yapstack_sync::change::read_local_changes_since;
use yapstack_sync::quarantine::{merge_changeset, pending_count};
use yapstack_sync::schema::{
    self, apply_out_of_band_alters, crr_tables, crr_triggers_are_stale, heal_stale_crr_triggers,
    rebuild_crr_machinery,
};
use yapstack_sync::CrsqlDb;

/// The 13-column `segments` a device gets from the base migration chain (v1 + v3
/// editing columns) — NO `speaker_id`, which is a frontend runtime patch.
const SEGMENTS_13: &str = "CREATE TABLE segments (\
    id TEXT NOT NULL PRIMARY KEY, \
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
    hidden INTEGER NOT NULL DEFAULT 0);";

/// The 6-column `chat_messages` shape that predates the v14 tool-replay columns. Six
/// of the eight `OUT_OF_BAND_ALTERS` entries target this table, so it carries the same
/// exposure as `segments`.
const CHAT_MESSAGES_6: &str = "CREATE TABLE chat_messages (\
    id TEXT NOT NULL PRIMARY KEY, \
    context_key TEXT NOT NULL DEFAULT '', \
    session_id TEXT, \
    role TEXT NOT NULL DEFAULT 'user', \
    content TEXT NOT NULL DEFAULT '', \
    created_at TEXT NOT NULL DEFAULT (datetime('now')));";

/// Every out-of-band column for a table, in `OUT_OF_BAND_ALTERS` order.
fn oob_cols(table: &str) -> Vec<&'static str> {
    schema::OUT_OF_BAND_ALTERS
        .iter()
        .filter(|(t, _, _)| *t == table)
        .map(|(_, c, _)| *c)
        .collect()
}

fn crrified(ddl: &str, table: &str, seed: &str) -> CrsqlDb {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute_batch(ddl).unwrap();
    conn.query_row(&format!("SELECT crsql_as_crr('{table}')"), [], |_| Ok(()))
        .unwrap();
    conn.execute_batch(seed).unwrap();
    db
}

fn segments_13_crr() -> CrsqlDb {
    crrified(
        SEGMENTS_13,
        "segments",
        "INSERT INTO segments(id,session_id,source,text) VALUES('g1','s','mic','hi'), \
         ('g2','s','mic','there');",
    )
}

fn chat_messages_6_crr() -> CrsqlDb {
    crrified(
        CHAT_MESSAGES_6,
        "chat_messages",
        "INSERT INTO chat_messages(id,context_key,role,content) VALUES('m1','k','user','yo');",
    )
}

/// A device that took the bare-ALTER path: the columns are on the base table but the
/// crsql machinery was never regenerated for them.
fn bare_alter_all(conn: &Connection, table: &str) {
    for (t, _c, coldef) in schema::OUT_OF_BAND_ALTERS {
        if *t == table {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {coldef}"), [])
                .unwrap();
        }
    }
}

fn db_version(conn: &Connection) -> i64 {
    conn.query_row("SELECT crsql_db_version()", [], |r| r.get(0))
        .unwrap()
}

fn site_id(conn: &Connection) -> Vec<u8> {
    conn.query_row("SELECT crsql_site_id()", [], |r| r.get(0))
        .unwrap()
}

/// (key, col_name, col_version, db_version, seq) of every clock row, sorted.
fn clock_rows(conn: &Connection, table: &str) -> Vec<(i64, String, i64, i64, i64)> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT key, col_name, col_version, db_version, seq \
             FROM \"{table}__crsql_clock\" ORDER BY key, col_name"
        ))
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .map(|x| x.unwrap())
    .collect()
}

// ---------------------------------------------------------------------------
// 1. Reproduction — the exact owner-reported failure, and that the heal fixes it.
// ---------------------------------------------------------------------------

/// THE BUG, verbatim. Bare-ALTER `speaker_id` onto a 13-column CRR `segments`, let
/// `apply_out_of_band_alters` skip it (it already exists), then do what the right-click
/// menu does: a plain `UPDATE segments SET ...`. The error message is asserted
/// character-for-character because it is the owner's evidence string.
#[test]
fn repro_direct_update_fails_with_expected_29_values_got_27() {
    let db = segments_13_crr();
    let conn = db.conn();

    bare_alter_all(conn, "segments");
    // The Rust boot self-heal SKIPS speaker_id: `column_exists` is already true.
    apply_out_of_band_alters(conn).unwrap();
    assert!(schema::column_exists(conn, "segments", "speaker_id").unwrap());

    let err = conn
        .execute("UPDATE segments SET text='edited' WHERE id='g1'", [])
        .expect_err("a direct UPDATE must fail on the stale-arity trigger");
    assert!(
        err.to_string().contains("expected 29 values, got 27"),
        "expected the owner's verbatim arity error, got: {err}"
    );

    // The symptom asymmetry the owner saw: inserts and deletes still work, because
    // their triggers pass PK values only.
    conn.execute(
        "INSERT INTO segments(id,session_id,source,text) VALUES('g3','s','mic','new')",
        [],
    )
    .expect("INSERT is unaffected (PK-only trigger arity)");
    conn.execute("DELETE FROM segments WHERE id='g3'", [])
        .expect("DELETE is unaffected (PK-only trigger arity)");
}

/// The same reproduction, flipped GREEN by the boot self-heal — zero manual steps,
/// no schema change, and the previously un-tracked column becomes CRR-tracked.
#[test]
fn heal_fixes_the_repro_direct_update_succeeds() {
    let db = segments_13_crr();
    let conn = db.conn();
    bare_alter_all(conn, "segments");
    apply_out_of_band_alters(conn).unwrap();
    assert!(conn
        .execute("UPDATE segments SET text='edited' WHERE id='g1'", [])
        .is_err());

    assert!(crr_triggers_are_stale(conn, "segments").unwrap());
    let healed = heal_stale_crr_triggers(conn).unwrap();
    assert_eq!(healed, vec!["segments".to_string()]);
    assert!(!crr_triggers_are_stale(conn, "segments").unwrap());

    // Edit, soft-delete and hide — the three right-click actions — all work now.
    assert_eq!(
        conn.execute("UPDATE segments SET text='edited' WHERE id='g1'", [])
            .unwrap(),
        1
    );
    conn.execute(
        "UPDATE segments SET deleted_at=datetime('now') WHERE id='g1'",
        [],
    )
    .unwrap();
    conn.execute("UPDATE segments SET hidden=1 WHERE id='g2'", [])
        .unwrap();
    let text: String = conn
        .query_row("SELECT text FROM segments WHERE id='g1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(text, "edited");

    // The rebuild's backfill deliberately creates NO clock rows for a column whose
    // value equals its default — `fill_column` adds `AND t1."col" IS NOT NULL` for a
    // nullable, defaultless column (vendor `backfill.rs:205-231` + `util.rs:31-33`), so
    // an all-NULL `speaker_id` costs nothing and manufactures no changes to push.
    assert!(
        !clock_rows(conn, "segments")
            .iter()
            .any(|(_, col, _, _, _)| col == "speaker_id"),
        "an all-NULL nullable column needs no clock rows (default-valued)"
    );
    // And a local write to it is now a real, pushable local change.
    let before = db_version(conn);
    conn.execute("UPDATE segments SET speaker_id=7 WHERE id='g1'", [])
        .unwrap();
    let cs = read_local_changes_since(conn, before).unwrap();
    assert!(
        cs.rows.iter().any(|r| r.cid == "speaker_id"),
        "a speaker_id write must now produce a local change row"
    );
}

/// Clock entries that DO exist for the bare-ALTERed column — INSERT keeps recording
/// every column correctly, because `x_crsql_after_insert` reads the live shape rather
/// than a frozen argument list — survive the rebuild untouched.
#[test]
fn heal_preserves_clock_entries_recorded_for_the_bare_altered_column() {
    let db = segments_13_crr();
    let conn = db.conn();
    bare_alter_all(conn, "segments");
    // The insert path still works and DOES track speaker_id.
    conn.execute(
        "INSERT INTO segments(id,session_id,source,text,speaker_id) \
         VALUES('g4','s','mic','post-alter',5)",
        [],
    )
    .unwrap();
    let tracked: Vec<_> = clock_rows(conn, "segments")
        .into_iter()
        .filter(|(_, col, _, _, _)| col == "speaker_id")
        .collect();
    assert!(
        !tracked.is_empty(),
        "the insert path records the new column even while UPDATE is broken"
    );
    let version_before = db_version(conn);

    heal_stale_crr_triggers(conn).unwrap();

    let after: Vec<_> = clock_rows(conn, "segments")
        .into_iter()
        .filter(|(_, col, _, _, _)| col == "speaker_id")
        .collect();
    assert_eq!(after, tracked, "existing clock entries survive the rebuild");
    assert_eq!(db_version(conn), version_before, "db_version is not bumped");
    let sid: Option<i64> = conn
        .query_row("SELECT speaker_id FROM segments WHERE id='g4'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sid, Some(5));
}

/// Table-generic: `chat_messages` carries six out-of-band columns and the identical
/// exposure. Its stale-arity error is `expected 25 values, got 13`
/// (1 + 2*1 + 2*11 vs 1 + 2*1 + 2*5), and the same heal fixes it.
#[test]
fn repro_and_heal_chat_messages_six_out_of_band_columns() {
    assert_eq!(oob_cols("chat_messages").len(), 6);
    let db = chat_messages_6_crr();
    let conn = db.conn();
    bare_alter_all(conn, "chat_messages");
    apply_out_of_band_alters(conn).unwrap();

    let err = conn
        .execute("UPDATE chat_messages SET content='x' WHERE id='m1'", [])
        .expect_err("chat_messages has the same stale-arity exposure");
    assert!(
        err.to_string().contains("expected 25 values, got 13"),
        "unexpected arity error for chat_messages: {err}"
    );

    assert!(crr_triggers_are_stale(conn, "chat_messages").unwrap());
    assert_eq!(
        heal_stale_crr_triggers(conn).unwrap(),
        vec!["chat_messages".to_string()]
    );
    conn.execute("UPDATE chat_messages SET content='x' WHERE id='m1'", [])
        .expect("UPDATE works after the heal");
    for col in oob_cols("chat_messages") {
        conn.execute(
            &format!("UPDATE chat_messages SET \"{col}\"=NULL WHERE id='m1'"),
            [],
        )
        .unwrap_or_else(|e| panic!("UPDATE of healed column {col} failed: {e}"));
    }
}

/// Several broken tables in one database are all healed in one pass.
#[test]
fn heal_repairs_every_stale_crr_table_in_one_pass() {
    let db = segments_13_crr();
    let conn = db.conn();
    conn.execute_batch(CHAT_MESSAGES_6).unwrap();
    conn.query_row("SELECT crsql_as_crr('chat_messages')", [], |_| Ok(()))
        .unwrap();
    conn.execute(
        "INSERT INTO chat_messages(id,context_key,role,content) VALUES('m1','k','user','yo')",
        [],
    )
    .unwrap();
    bare_alter_all(conn, "segments");
    bare_alter_all(conn, "chat_messages");

    let mut healed = heal_stale_crr_triggers(conn).unwrap();
    healed.sort();
    assert_eq!(healed, vec!["chat_messages", "segments"]);
    conn.execute("UPDATE segments SET text='a' WHERE id='g1'", [])
        .unwrap();
    conn.execute("UPDATE chat_messages SET content='b' WHERE id='m1'", [])
        .unwrap();
}

// ---------------------------------------------------------------------------
// 2. Detection.
// ---------------------------------------------------------------------------

/// A table that took the CORRECT (wrapped) path is never reported stale — proof that
/// `crsql_commit_alter` regenerates the update trigger, i.e. the vendor is NOT at
/// fault here and needed no patch.
#[test]
fn wrapped_alter_leaves_triggers_consistent() {
    let db = segments_13_crr();
    let conn = db.conn();
    assert!(!crr_triggers_are_stale(conn, "segments").unwrap());
    apply_out_of_band_alters(conn).unwrap();
    assert!(schema::column_exists(conn, "segments", "speaker_id").unwrap());
    assert!(
        !crr_triggers_are_stale(conn, "segments").unwrap(),
        "the crsql_alter dance regenerates the update trigger at the new arity"
    );
    assert!(heal_stale_crr_triggers(conn).unwrap().is_empty());
    conn.execute("UPDATE segments SET speaker_id=3 WHERE id='g1'", [])
        .unwrap();
}

/// A CRR table whose update trigger vanished entirely (a crashed alter dance) is
/// stale, and is rebuilt.
#[test]
fn missing_update_trigger_is_stale_and_rebuilt() {
    let db = segments_13_crr();
    let conn = db.conn();
    conn.execute_batch("DROP TRIGGER \"segments__crsql_utrig\";")
        .unwrap();
    assert!(crr_triggers_are_stale(conn, "segments").unwrap());
    assert_eq!(
        heal_stale_crr_triggers(conn).unwrap(),
        vec!["segments".to_string()]
    );
    conn.execute("UPDATE segments SET text='a' WHERE id='g1'", [])
        .unwrap();
}

/// A missing INSERT or DELETE trigger is stale too, even when the update trigger is
/// perfectly consistent. This failure is SILENT — an uncaptured insert/delete raises no
/// error, it just never syncs — and neither our clock-table `is_crr` nor cr-sqlite's
/// itrig-based one would flag it (`is_crr.rs:10-26` only probes the insert trigger, so a
/// missing DELETE trigger is invisible to it).
#[test]
fn missing_insert_or_delete_trigger_is_stale_and_rebuilt() {
    for victim in ["segments__crsql_dtrig", "segments__crsql_itrig"] {
        let db = segments_13_crr();
        let conn = db.conn();
        assert!(!crr_triggers_are_stale(conn, "segments").unwrap());

        conn.execute_batch(&format!("DROP TRIGGER \"{victim}\";"))
            .unwrap();
        // The update trigger is untouched and still correct, so an arity-only check
        // would call this table healthy.
        conn.execute("UPDATE segments SET text='fine' WHERE id='g1'", [])
            .expect("the update trigger is still consistent");
        assert!(
            crr_triggers_are_stale(conn, "segments").unwrap(),
            "{victim} missing must be reported stale"
        );

        assert_eq!(
            heal_stale_crr_triggers(conn).unwrap(),
            vec!["segments".to_string()]
        );
        for suffix in ["__crsql_itrig", "__crsql_utrig", "__crsql_dtrig"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name=?1",
                    [format!("segments{suffix}")],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "segments{suffix} must exist after the rebuild");
        }
        // Writes of every shape are captured again.
        let before = db_version(conn);
        conn.execute(
            "INSERT INTO segments(id,session_id,source,text) VALUES('g7','s','mic','ins')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM segments WHERE id='g7'", [])
            .unwrap();
        assert!(
            !read_local_changes_since(conn, before)
                .unwrap()
                .rows
                .is_empty(),
            "post-rebuild insert/delete must be captured as local changes"
        );
    }
}

/// The ORDER check, exercised directly: a trigger whose slots carry the right NAMES in
/// the WRONG ORDER passes a slot-count check and a per-column "is it mentioned" check,
/// yet mis-attributes every value when `partition_values` slices the arguments back
/// apart. It must be reported stale and rebuilt.
///
/// (A bare `ALTER TABLE … RENAME COLUMN` does NOT reach this state: SQLite rewrites
/// column references inside dependent triggers, so the crsql trigger stays consistent —
/// verified against the pinned build. This check therefore guards hand-edited or
/// migration-rebuilt trigger text, not a rename.)
#[test]
fn slot_order_drift_is_stale_and_rebuilt() {
    let db = segments_13_crr();
    let conn = db.conn();
    let good: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='segments__crsql_utrig'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // Swap two adjacent NEW slots: same names, same count, wrong positions.
    let permuted = good.replace("NEW.\"source\",NEW.\"text\"", "NEW.\"text\",NEW.\"source\"");
    assert_ne!(
        permuted, good,
        "the fixture must actually permute two slots"
    );
    conn.execute_batch(&format!(
        "DROP TRIGGER \"segments__crsql_utrig\"; {permuted};"
    ))
    .unwrap();

    assert!(
        crr_triggers_are_stale(conn, "segments").unwrap(),
        "reordered slots must be detected even though the name set and count match"
    );
    assert_eq!(
        heal_stale_crr_triggers(conn).unwrap(),
        vec!["segments".to_string()]
    );
    let rebuilt: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='segments__crsql_utrig'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rebuilt, good,
        "the rebuild restores the canonical slot order"
    );

    // And a write now records the value against the RIGHT column.
    let before = db_version(conn);
    conn.execute("UPDATE segments SET text='right-column' WHERE id='g1'", [])
        .unwrap();
    let cs = read_local_changes_since(conn, before).unwrap();
    assert!(cs.rows.iter().any(|r| r.cid == "text"));
    assert!(!cs.rows.iter().any(|r| r.cid == "source"));
}

/// A non-CRR table is never "stale" (nothing to heal), and is not discovered as a CRR
/// table. Guards against the heal touching local-only tables.
#[test]
fn non_crr_tables_are_ignored() {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute_batch(SEGMENTS_13).unwrap();
    conn.execute_batch("CREATE TABLE pending_audio_deletions(id TEXT PRIMARY KEY);")
        .unwrap();
    assert!(!crr_triggers_are_stale(conn, "segments").unwrap());
    assert!(crr_tables(conn).unwrap().is_empty());
    assert!(heal_stale_crr_triggers(conn).unwrap().is_empty());
}

/// `crr_tables` discovers CRR tables by clock shadow table, not from `SYNC_TABLES`, so
/// a table a future release adds is covered automatically.
#[test]
fn crr_tables_discovers_tables_outside_sync_tables() {
    let db = segments_13_crr();
    let conn = db.conn();
    conn.execute_batch("CREATE TABLE future_table(id TEXT NOT NULL PRIMARY KEY, v TEXT);")
        .unwrap();
    conn.query_row("SELECT crsql_as_crr('future_table')", [], |_| Ok(()))
        .unwrap();
    conn.execute("INSERT INTO future_table(id,v) VALUES('a','1')", [])
        .unwrap();
    let tables = crr_tables(conn).unwrap();
    assert!(tables.contains(&"future_table".to_string()));
    assert!(!schema::SYNC_TABLES.contains(&"future_table"));

    conn.execute("ALTER TABLE future_table ADD COLUMN w TEXT", [])
        .unwrap();
    assert!(crr_triggers_are_stale(conn, "future_table").unwrap());
    assert_eq!(
        heal_stale_crr_triggers(conn).unwrap(),
        vec!["future_table".to_string()]
    );
    conn.execute("UPDATE future_table SET w='2' WHERE id='a'", [])
        .unwrap();
}

// ---------------------------------------------------------------------------
// 3. Idempotency + sync-state preservation on a HEALTHY (Mac-shaped) device.
// ---------------------------------------------------------------------------

/// The Mac-shaped case: CRRified at the FULL 14-column arity, never altered. The heal
/// is a no-op (nothing detected), and even a FORCED rebuild preserves every piece of
/// sync state — site id, `db_version`, and the clock rows byte-for-byte — so a healthy
/// device that runs the repair path cannot be pushed into a re-sync storm.
#[test]
fn heal_is_a_noop_and_preserves_sync_state_on_a_healthy_table() {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    support::create_original_schema(conn);
    support::populate(conn);
    schema::crr_migrate(conn).unwrap();
    apply_out_of_band_alters(conn).unwrap();

    for t in schema::SYNC_TABLES {
        assert!(
            !crr_triggers_are_stale(conn, t).unwrap(),
            "{t}: a correctly migrated table must not look stale"
        );
    }
    assert!(
        heal_stale_crr_triggers(conn).unwrap().is_empty(),
        "the heal must not fire on a healthy device"
    );

    // Now FORCE the rebuild anyway and prove it is state-preserving.
    let site_before = site_id(conn);
    let version_before = db_version(conn);
    let clocks_before: Vec<_> = schema::SYNC_TABLES
        .iter()
        .map(|t| (*t, clock_rows(conn, t)))
        .collect();
    let fps_before: Vec<_> = schema::SYNC_TABLES
        .iter()
        .map(|t| (*t, support::fingerprint(conn, t)))
        .collect();

    for t in schema::SYNC_TABLES {
        rebuild_crr_machinery(conn, t).unwrap();
    }

    assert_eq!(
        site_id(conn),
        site_before,
        "site id must survive the rebuild"
    );
    assert_eq!(
        db_version(conn),
        version_before,
        "the rebuild must NOT bump db_version (backfill uses crsql_db_version(), \
         vendor backfill.rs:107-117)"
    );
    for (t, before) in &clocks_before {
        assert_eq!(
            &clock_rows(conn, t),
            before,
            "{t}: clock rows must be untouched by the rebuild"
        );
    }
    for (t, before) in &fps_before {
        assert_eq!(
            &support::fingerprint(conn, t),
            before,
            "{t}: row data must be untouched by the rebuild"
        );
    }
    // No spurious local changes => nothing new to push after a rebuild.
    assert!(
        read_local_changes_since(conn, version_before)
            .unwrap()
            .rows
            .is_empty(),
        "a rebuild on a healthy table must not manufacture local changes"
    );

    // Second and third passes are equally inert.
    for _ in 0..2 {
        assert!(heal_stale_crr_triggers(conn).unwrap().is_empty());
    }
    conn.execute(
        "UPDATE segments SET text='still writable' WHERE id IS NOT NULL",
        [],
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// 4. No regression to the R11 incoming-change APPLY path.
// ---------------------------------------------------------------------------

/// After a heal, a peer's incoming changeset — including one carrying the previously
/// un-tracked column — still APPLIES with zero quarantine and converges. This is the
/// R11 path the original coverage exercised; the heal must not regress it.
#[test]
fn incoming_changes_still_apply_after_heal() {
    // Peer A: healthy, full-shape device that writes speaker_id.
    let a = crrified(
        "CREATE TABLE segments(id TEXT NOT NULL PRIMARY KEY, \
         session_id TEXT NOT NULL DEFAULT '', text TEXT NOT NULL DEFAULT '', \
         speaker_id INTEGER);",
        "segments",
        "INSERT INTO segments(id,session_id,text,speaker_id) VALUES('g1','s','from-a',7);",
    );
    let cs = read_local_changes_since(a.conn(), 0).unwrap();

    // Device B: the broken Windows shape — CRRified narrow, then bare-ALTERed.
    let b = crrified(
        "CREATE TABLE segments(id TEXT NOT NULL PRIMARY KEY, \
         session_id TEXT NOT NULL DEFAULT '', text TEXT NOT NULL DEFAULT '');",
        "segments",
        "INSERT INTO segments(id,session_id,text) VALUES('g9','s','local');",
    );
    let conn = b.conn();
    conn.execute("ALTER TABLE segments ADD COLUMN speaker_id INTEGER", [])
        .unwrap();
    apply_out_of_band_alters(conn).unwrap();
    assert!(crr_triggers_are_stale(conn, "segments").unwrap());

    assert_eq!(
        heal_stale_crr_triggers(conn).unwrap(),
        vec!["segments".to_string()]
    );

    let (applied, quarantined) = merge_changeset(conn, &cs).unwrap();
    assert!(applied >= 1, "the peer changeset must apply, got {applied}");
    assert_eq!(quarantined, 0, "nothing may quarantine after the heal");
    assert_eq!(pending_count(conn).unwrap(), 0);
    let (txt, sid): (String, Option<i64>) = conn
        .query_row(
            "SELECT text, speaker_id FROM segments WHERE id='g1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(txt, "from-a");
    assert_eq!(sid, Some(7), "the incoming speaker_id converged");

    // And the local row is still directly editable.
    conn.execute("UPDATE segments SET text='local-edit' WHERE id='g9'", [])
        .unwrap();
    assert!(!read_local_changes_since(conn, 0).unwrap().rows.is_empty());
}
