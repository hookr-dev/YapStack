// SPDX-License-Identifier: AGPL-3.0-only
//! R6 (static-link CRR registration) + R3 (CRR migration round-trip).

mod support;

use yapstack_sync::schema::{self, SYNC_TABLES};
use yapstack_sync::CrsqlDb;

/// R6: the statically-linked, auto-registered cr-sqlite extension makes
/// `crsql_as_crr` / `crsql_changes` callable on a plain rusqlite connection.
#[test]
fn r6_static_link_registers_crr() {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    assert_eq!(db.site_id().unwrap().len(), 16);
    conn.execute_batch("CREATE TABLE t(id TEXT NOT NULL PRIMARY KEY, v TEXT NOT NULL DEFAULT '');")
        .unwrap();
    conn.query_row("SELECT crsql_as_crr('t')", [], |_| Ok(()))
        .expect("crsql_as_crr must be callable via the static link");
    conn.execute("INSERT INTO t(id,v) VALUES('a','hello')", [])
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM crsql_changes", [], |r| r.get(0))
        .unwrap();
    assert!(n >= 1, "crsql_changes vtab must expose the local write");
    assert!(schema::is_crr(conn, "t").unwrap());
}

/// R3: the migration transforms the real (synthetic) populated schema and every sync
/// table round-trips with an IDENTICAL content fingerprint.
#[test]
fn crr_migration_roundtrip_synthetic() {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    support::create_original_schema(conn);
    support::populate(conn);

    // Pre-migration fingerprints on the untouched copy.
    let pre: Vec<(&str, (i64, u64))> = SYNC_TABLES
        .iter()
        .map(|t| (*t, support::fingerprint(conn, t)))
        .collect();

    schema::crr_migrate(conn).expect("R3 migration must succeed on the real schema");

    for (t, (pn, ph)) in &pre {
        let (n, h) = support::fingerprint(conn, t);
        assert_eq!(n, *pn, "{t}: row count drifted {pn}->{n}");
        assert_eq!(
            h, *ph,
            "{t}: content fingerprint drifted (round-trip lost data)"
        );
        assert!(
            schema::is_crr(conn, t).unwrap(),
            "{t}: not converted to a CRR"
        );
    }
    assert!(
        *pre.iter()
            .find(|(t, _)| *t == "segments")
            .map(|(_, f)| &f.0)
            .unwrap()
            > 100
    );
}

/// R8 fresh-install regression: the full db_service migration chain produces a
/// 13-column `segments` (speaker_id is a frontend runtime patch, not a migration).
/// The hardcoded rebuild assumed 14 columns and crashed cutover with
/// `segments__crr_rebuild has 14 columns but 13 values were supplied`. The derived
/// rebuild must follow the live 13-column shape and still round-trip + CRRify.
#[test]
fn crr_migration_roundtrip_fresh_chain_13col_segments() {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    support::create_original_schema(conn);
    // Drop speaker_id to reproduce the pristine migration-chain shape (13 columns).
    conn.execute_batch("ALTER TABLE segments DROP COLUMN speaker_id;")
        .unwrap();
    let ncols: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('segments')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ncols, 13,
        "precondition: fresh-chain segments has 13 columns"
    );
    support::populate(conn);

    let pre: Vec<(&str, (i64, u64))> = SYNC_TABLES
        .iter()
        .map(|t| (*t, support::fingerprint(conn, t)))
        .collect();

    schema::crr_migrate(conn).expect("crr_migrate must adapt to the 13-column live shape");

    for (t, (pn, ph)) in &pre {
        let (n, h) = support::fingerprint(conn, t);
        assert_eq!(n, *pn, "{t}: row count drifted {pn}->{n}");
        assert_eq!(h, *ph, "{t}: content fingerprint drifted");
        assert!(
            schema::is_crr(conn, t).unwrap(),
            "{t}: not converted to a CRR"
        );
    }
    // A post-migration write to the 13-column CRR captures to crsql_changes.
    conn.execute(
        "INSERT INTO segments(id,session_id,source,text,audio_offset_seconds,chunk_duration_seconds) \
         VALUES ('post','s0','mic','after',9,1)",
        [],
    )
    .unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM crsql_changes WHERE \"table\"='segments' AND pk IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(n > 0, "post-cutover write must capture to crsql_changes");
}

/// R3 against a COPY of the REAL populated DB. Opt-in: set `YAPSTACK_SPIKE_DB` to a
/// `.backup` copy path (NEVER the live DB — T001 copy procedure). Ignored by default.
#[test]
#[ignore = "requires YAPSTACK_SPIKE_DB pointing at a .backup COPY of the live DB"]
fn crr_migration_roundtrip_real_db() {
    let path = std::env::var("YAPSTACK_SPIKE_DB").expect("set YAPSTACK_SPIKE_DB");
    // cr-sqlite lives on the [`CrsqlDb`] connection only; a plain connection has no
    // `crsql_*` functions, so the migration must run through it (drop finalizes).
    let db = CrsqlDb::open(&path).unwrap();
    let conn = db.conn();
    let pre: Vec<(&str, (i64, u64))> = SYNC_TABLES
        .iter()
        .map(|t| (*t, support::fingerprint(conn, t)))
        .collect();
    schema::crr_migrate(conn).expect("migration on real-DB copy");
    for (t, (pn, ph)) in &pre {
        let (n, h) = support::fingerprint(conn, t);
        assert_eq!(n, *pn, "{t}: row count drifted");
        assert_eq!(h, *ph, "{t}: fingerprint drifted");
    }
}

/// I-NT2 (cutover half): the CRR rebuild keeps every non-UNIQUE secondary index.
/// Before the fix `DROP TABLE` took the indexes with it and only triggers were
/// replayed, so a synced DB lost every one of them (segments-by-session and
/// sessions-by-day turned into full scans). A UNIQUE index is not kept: cr-sqlite
/// refuses a CRR with a non-PK UNIQUE index, and uniqueness is app-layer.
#[test]
fn crr_migration_preserves_secondary_indexes() {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    support::create_original_schema(conn);
    conn.execute_batch(support::SECONDARY_INDEXES).unwrap();
    // A partial index, to prove the replay is the captured SQL verbatim.
    conn.execute_batch(
        "CREATE INDEX idx_segments_live ON segments(session_id) WHERE deleted_at IS NULL;",
    )
    .unwrap();
    support::populate(conn);
    let before = support::index_set(conn, SYNC_TABLES);
    assert_eq!(before.len(), 20, "precondition: fixture indexes present");
    // An explicit UNIQUE index is the one shape the rebuild must NOT carry over.
    conn.execute_batch("CREATE UNIQUE INDEX idx_tags_color_u ON tags(color, name);")
        .unwrap();

    schema::crr_migrate(conn).expect("cutover");

    assert_eq!(
        support::index_set(conn, SYNC_TABLES),
        before,
        "every non-UNIQUE secondary index survives cutover, shape for shape"
    );
    for t in SYNC_TABLES {
        assert!(schema::is_crr(conn, t).unwrap(), "{t}: not a CRR");
    }
    // The planner actually uses the restored indexes.
    let mut stmt = conn
        .prepare("EXPLAIN QUERY PLAN SELECT * FROM segments WHERE session_id = 's0'")
        .unwrap();
    let detail: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .map(|x| x.unwrap())
        .collect();
    assert!(
        detail.iter().any(|d| d.contains("USING INDEX")),
        "segments-by-session must use an index after cutover: {detail:?}"
    );
}
