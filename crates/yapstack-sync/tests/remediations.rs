// SPDX-License-Identifier: AGPL-3.0-only
//! R4 (cascade/tombstone GC), R5 (app-side uniqueness), R7 (cross-schema-gap
//! quarantine + replay).

mod support;

use rusqlite::Connection;
use yapstack_sync::cascade::cascade_gc;
use yapstack_sync::change::read_local_changes_since;
use yapstack_sync::quarantine::{merge_changeset, pending_count, replay_pending};
use yapstack_sync::schema::{self, apply_out_of_band_alters};
use yapstack_sync::uniqueness::enforce_uniqueness;
use yapstack_sync::CrsqlDb;

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn migrated_db() -> CrsqlDb {
    let db = CrsqlDb::open_in_memory().unwrap();
    support::create_original_schema(db.conn());
    support::populate(db.conn());
    schema::crr_migrate(db.conn()).unwrap();
    db
}

/// R4: deleting a parent leaves orphans (no FK cascade); the GC pass tombstones them
/// through the CRDT, and nulls dangling SET-NULL references.
#[test]
fn r4_cascade_gc_tombstones_orphans() {
    let db = migrated_db();
    let conn = db.conn();

    // Baseline: s0 has children.
    assert!(count(conn, "SELECT count(*) FROM segments WHERE session_id='s0'") > 0);
    assert_eq!(
        count(conn, "SELECT count(*) FROM notes WHERE session_id='s0'"),
        1
    );

    // Delete a session and a folder (no cascade fires — FKs were stripped).
    conn.execute("DELETE FROM sessions WHERE id='s0'", [])
        .unwrap();
    conn.execute("DELETE FROM folders WHERE id='f1'", [])
        .unwrap();
    assert!(
        count(conn, "SELECT count(*) FROM segments WHERE session_id='s0'") > 0,
        "orphans must survive until the GC pass (proves no hard cascade)"
    );

    let stats = cascade_gc(conn).unwrap();
    assert!(stats.deleted > 0);

    // Session children gone.
    assert_eq!(
        count(conn, "SELECT count(*) FROM segments WHERE session_id='s0'"),
        0
    );
    assert_eq!(
        count(conn, "SELECT count(*) FROM notes WHERE session_id='s0'"),
        0
    );
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM note_versions WHERE note_id='n0'"
        ),
        0
    );
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM session_audio_parts WHERE session_id='s0'"
        ),
        0
    );
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM session_tags WHERE session_id='s0'"
        ),
        0
    );
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM chat_messages WHERE session_id='s0'"
        ),
        0
    );
    // Folder children: shares cascaded, dangling folder refs nulled.
    assert_eq!(
        count(conn, "SELECT count(*) FROM shares WHERE folder_id='f1'"),
        0
    );
    assert_eq!(
        count(conn, "SELECT count(*) FROM sessions WHERE folder_id='f1'"),
        0,
        "sessions.folder_id must be SET NULL when folder deleted"
    );
    assert_eq!(
        count(conn, "SELECT count(*) FROM folders WHERE parent_id='f1'"),
        0,
        "folders.parent_id must be SET NULL when parent deleted"
    );

    // Idempotent: a second pass changes nothing.
    let again = cascade_gc(conn).unwrap();
    assert_eq!(again.deleted, 0);
    assert_eq!(again.nulled, 0);
}

/// R5: distinct-PK rows that collide on a dropped UNIQUE (post-merge) are resolved to
/// a deterministic winner.
#[test]
fn r5_uniqueness_deterministic_resolution() {
    let db = migrated_db();
    let conn = db.conn();

    // Two notes for the SAME (fresh) session (as after a concurrent-create merge).
    conn.execute(
        "INSERT INTO notes(id,session_id,content,updated_at) VALUES ('nx','snew','older','2024-01-01')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO notes(id,session_id,content,updated_at) VALUES ('ny','snew','newer','2024-02-01')",
        [],
    )
    .unwrap();
    // Two tags with the same NOCASE name; associate the loser.
    conn.execute("INSERT INTO tags(id,name) VALUES ('t9','URGENT')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO session_tags(session_id,tag_id) VALUES ('s2','t9')",
        [],
    )
    .unwrap();
    // Two audio parts colliding on (session_id, part_index).
    conn.execute(
        "INSERT INTO session_audio_parts(id,session_id,part_index,file_path,format,duration_seconds,sample_rate) \
         VALUES ('apx','s1',0,'/dup.wav','wav',1.0,16000)",
        [],
    )
    .unwrap();

    let stats = enforce_uniqueness(conn).unwrap();
    assert!(stats.notes_removed >= 1 && stats.tags_merged >= 1 && stats.audio_parts_removed >= 1);

    // notes: exactly one for the session, and it is the freshest (updated_at) winner.
    assert_eq!(
        count(conn, "SELECT count(*) FROM notes WHERE session_id='snew'"),
        1
    );
    let kept: String = conn
        .query_row(
            "SELECT content FROM notes WHERE session_id='snew'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, "newer", "freshest-updated_at note must win");

    // tags: one 'urgent' (NOCASE), association repointed to the surviving winner.
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM tags WHERE name='urgent' COLLATE NOCASE"
        ),
        1
    );
    let winner: String = conn
        .query_row(
            "SELECT id FROM tags WHERE name='urgent' COLLATE NOCASE",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count(
            conn,
            &format!(
                "SELECT count(*) FROM session_tags WHERE session_id='s2' AND tag_id='{winner}'"
            )
        ),
        1,
        "loser tag's association must survive on the winner"
    );

    // audio parts: one row per (session_id, part_index).
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM session_audio_parts WHERE session_id='s1' AND part_index=0"
        ),
        1
    );

    // Idempotent.
    let again = enforce_uniqueness(conn).unwrap();
    assert_eq!(again.notes_removed, 0);
    assert_eq!(again.tags_merged, 0);
    assert_eq!(again.audio_parts_removed, 0);
}

/// R7: device A adds a column and writes it; device B (still at the base schema)
/// quarantines the unknown-column changes, migrates with the wrapped ALTER, replays,
/// and converges with NO data loss (architecture §6.3/§15).
#[test]
fn r7_quarantine_replay_across_schema_gap() {
    // Two independent devices at the SAME base schema `doc(id, a)`.
    let a = CrsqlDb::open_in_memory().unwrap();
    let b = CrsqlDb::open_in_memory().unwrap();
    for db in [&a, &b] {
        db.conn()
            .execute_batch(
                "CREATE TABLE doc(id TEXT NOT NULL PRIMARY KEY, a TEXT NOT NULL DEFAULT '');",
            )
            .unwrap();
        db.conn()
            .query_row("SELECT crsql_as_crr('doc')", [], |_| Ok(()))
            .unwrap();
    }
    // A pre-gap write, then A migrates to add column `b` (wrapped) and writes it.
    a.conn()
        .execute("INSERT INTO doc(id,a) VALUES('d1','a1')", [])
        .unwrap();
    schema::crsql_alter(a.conn(), "doc", "ALTER TABLE doc ADD COLUMN b TEXT").unwrap();
    a.conn()
        .execute("UPDATE doc SET b='bee' WHERE id='d1'", [])
        .unwrap();
    a.conn()
        .execute("INSERT INTO doc(id,a,b) VALUES('d2','a2','bee2')", [])
        .unwrap();

    // Ship ALL of A's changes to B (B lacks column `b`).
    let cs = read_local_changes_since(a.conn(), 0).unwrap();
    let (applied, quarantined) = merge_changeset(b.conn(), &cs).unwrap();
    assert!(applied > 0);
    assert!(
        quarantined >= 2,
        "the two `b` writes must be quarantined, got {quarantined}"
    );
    assert_eq!(pending_count(b.conn()).unwrap(), quarantined as i64);

    // B has the rows but not the gapped column yet.
    assert_eq!(count(b.conn(), "SELECT count(*) FROM doc"), 2);
    assert!(!schema::column_exists(b.conn(), "doc", "b").unwrap());

    // B runs the SAME wrapped migration, then replays the quarantine.
    schema::crsql_alter(b.conn(), "doc", "ALTER TABLE doc ADD COLUMN b TEXT").unwrap();
    let replayed = replay_pending(b.conn()).unwrap();
    assert_eq!(
        replayed, quarantined,
        "every quarantined change must replay"
    );
    assert_eq!(pending_count(b.conn()).unwrap(), 0);

    // Converged with no data loss.
    let b_d1: Option<String> = b
        .conn()
        .query_row("SELECT b FROM doc WHERE id='d1'", [], |r| r.get(0))
        .unwrap();
    let b_d2: Option<String> = b
        .conn()
        .query_row("SELECT b FROM doc WHERE id='d2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(b_d1.as_deref(), Some("bee"));
    assert_eq!(b_d2.as_deref(), Some("bee2"));
    assert_eq!(
        support::fingerprint(a.conn(), "doc"),
        support::fingerprint(b.conn(), "doc")
    );
}

/// (Finding 3 pin) cr-sqlite's ACTUAL behavior for a BARE `ALTER TABLE ... ADD COLUMN` on a
/// CRR table — the assumption the frontend gate must not rely on. On the pinned cr-sqlite
/// version a bare `ADD COLUMN` is ACCEPTED, NOT rejected: the column lands on the base table
/// OUTSIDE the `crsql_begin_alter`/`crsql_commit_alter` dance. That is precisely why the db.ts
/// gate (`addColumnIfMissing` skips CRR tables) is required: it CANNOT depend on cr-sqlite
/// refusing the statement. Two facts make either possible cr-sqlite behavior safe once the gate
/// is in place:
///   1. If cr-sqlite REJECTED the bare ALTER, the frontend simply no-ops and the Rust boot
///      self-heal adds the column via `crsql_alter`.
///   2. If cr-sqlite ACCEPTS it (as here), the frontend gate means the frontend never issues it
///      on a CRR DB — so the un-dance'd column is never created by the frontend. Crucially,
///      `apply_out_of_band_alters` SKIPS any column that already `column_exists`, so had the
///      frontend bare-ALTERed it first, the Rust dance would be skipped and the column could be
///      left improperly clock-tracked. The gate closes exactly that race.
#[test]
fn bare_alter_add_column_on_crr_table_is_accepted_not_rejected() {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute_batch(
        "CREATE TABLE doc(id TEXT NOT NULL PRIMARY KEY, a TEXT NOT NULL DEFAULT '');",
    )
    .unwrap();
    conn.query_row("SELECT crsql_as_crr('doc')", [], |_| Ok(()))
        .unwrap();
    assert!(schema::is_crr(conn, "doc").unwrap(), "doc is CRR-tracked");

    // The load-bearing fact: cr-sqlite does NOT reject a bare ADD COLUMN on a CRR table.
    let result = conn.execute("ALTER TABLE doc ADD COLUMN b TEXT", []);
    assert!(
        result.is_ok(),
        "PINNED cr-sqlite ACCEPTS a bare ADD COLUMN on a CRR table (got {result:?}); the db.ts \
         gate must therefore proactively skip CRR tables rather than rely on a rejection"
    );
    assert!(
        schema::column_exists(conn, "doc", "b").unwrap(),
        "the bare ALTER did land the column on the base table (outside the crsql_alter dance)"
    );
    // And `apply_out_of_band_alters` would now SKIP `b` (it already exists), never running the
    // proper crsql_alter dance for it — the quarantine-bug resurrection the frontend gate averts.
    apply_out_of_band_alters(conn).unwrap();
}

/// R7: the named db.ts out-of-band ALTERs are applied through begin/commit_alter,
/// preserving existing rows and their clocks.
#[test]
fn r7_out_of_band_alters_are_wrapped() {
    let db = CrsqlDb::open_in_memory().unwrap();
    let conn = db.conn();
    // Base `segments` WITHOUT the db.ts `speaker_id` column.
    conn.execute_batch(
        "CREATE TABLE segments(id TEXT NOT NULL PRIMARY KEY, session_id TEXT NOT NULL DEFAULT '', \
         text TEXT NOT NULL DEFAULT '');",
    )
    .unwrap();
    conn.query_row("SELECT crsql_as_crr('segments')", [], |_| Ok(()))
        .unwrap();
    conn.execute(
        "INSERT INTO segments(id,session_id,text) VALUES('g1','s','hi')",
        [],
    )
    .unwrap();
    let clocks_before: i64 = count(conn, "SELECT count(*) FROM crsql_changes");

    apply_out_of_band_alters(conn).unwrap();

    assert!(schema::column_exists(conn, "segments", "speaker_id").unwrap());
    // Existing row + its clock survive the wrapped alter.
    assert_eq!(count(conn, "SELECT count(*) FROM segments"), 1);
    assert!(count(conn, "SELECT count(*) FROM crsql_changes") >= clocks_before);
    // Idempotent second application.
    apply_out_of_band_alters(conn).unwrap();
}

/// (Bug fix) End-to-end proof of the two-device UAT failure and its fix. A fresh
/// device CRRifies `segments` at the BASE shape (no `speaker_id`, because the
/// frontend's runtime ALTER had not run before cutover). WITHOUT the out-of-band
/// alter pass it quarantines every incoming `speaker_id` change forever
/// (`is_unknown_column`, the 26k-quarantined-changes symptom); WITH it — as the
/// fixed cutover / boot self-heal now runs — the column is CRR-tracked and the
/// identical incoming change APPLIES directly, converging with no data loss.
#[test]
fn out_of_band_alters_let_fresh_device_apply_speaker_id_without_quarantine() {
    // Device A already carries a CRR-tracked speaker_id and writes it.
    let a = CrsqlDb::open_in_memory().unwrap();
    a.conn()
        .execute_batch(
            "CREATE TABLE segments(id TEXT NOT NULL PRIMARY KEY, \
             session_id TEXT NOT NULL DEFAULT '', text TEXT NOT NULL DEFAULT '', \
             speaker_id INTEGER);",
        )
        .unwrap();
    a.conn()
        .query_row("SELECT crsql_as_crr('segments')", [], |_| Ok(()))
        .unwrap();
    a.conn()
        .execute(
            "INSERT INTO segments(id,session_id,text,speaker_id) VALUES('g1','s','hi',7)",
            [],
        )
        .unwrap();
    let cs = read_local_changes_since(a.conn(), 0).unwrap();

    // A fresh device B at the BASE (no-speaker_id) shape, CRRified.
    let fresh_base = || {
        let b = CrsqlDb::open_in_memory().unwrap();
        b.conn()
            .execute_batch(
                "CREATE TABLE segments(id TEXT NOT NULL PRIMARY KEY, \
                 session_id TEXT NOT NULL DEFAULT '', text TEXT NOT NULL DEFAULT '');",
            )
            .unwrap();
        b.conn()
            .query_row("SELECT crsql_as_crr('segments')", [], |_| Ok(()))
            .unwrap();
        b
    };

    // Control (pre-fix): no out-of-band alters → the speaker_id change quarantines.
    let raw = fresh_base();
    let (_applied, quarantined) = merge_changeset(raw.conn(), &cs).unwrap();
    assert!(
        quarantined >= 1,
        "without the alter the speaker_id change must quarantine, got {quarantined}"
    );
    assert!(!schema::column_exists(raw.conn(), "segments", "speaker_id").unwrap());

    // Fixed path: run apply_out_of_band_alters first (as cutover / boot self-heal now
    // does), then the SAME changeset applies with zero quarantine.
    let healed = fresh_base();
    apply_out_of_band_alters(healed.conn()).unwrap();
    assert!(schema::column_exists(healed.conn(), "segments", "speaker_id").unwrap());
    let (applied, quarantined) = merge_changeset(healed.conn(), &cs).unwrap();
    assert!(applied >= 1, "the changeset must apply, got {applied}");
    assert_eq!(
        quarantined, 0,
        "with speaker_id CRR-tracked, nothing quarantines"
    );
    assert_eq!(pending_count(healed.conn()).unwrap(), 0);

    // Converged: B carries A's speaker_id with no data loss.
    let sid: Option<i64> = healed
        .conn()
        .query_row("SELECT speaker_id FROM segments WHERE id='g1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sid, Some(7));
}

/// LIVE_SESSION_STATE.md Schema & sync-evolution: `sessions.recording_device_id` is
/// added the R11-proven way (OUT_OF_BAND_ALTERS). A device that CRRified `sessions` at
/// the base schema version must pick the column up via `apply_out_of_band_alters` and
/// then apply a peer's incoming `recording_device_id` change WITHOUT quarantining it —
/// the value converges. Mirrors the speaker_id proof for the new attribution column.
#[test]
fn out_of_band_alter_lets_fresh_device_apply_recording_device_id_without_quarantine() {
    // Device A already carries a CRR-tracked recording_device_id and writes it.
    let a = CrsqlDb::open_in_memory().unwrap();
    a.conn()
        .execute_batch(
            "CREATE TABLE sessions(id TEXT NOT NULL PRIMARY KEY, \
             title TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'recording', \
             recording_device_id TEXT);",
        )
        .unwrap();
    a.conn()
        .query_row("SELECT crsql_as_crr('sessions')", [], |_| Ok(()))
        .unwrap();
    a.conn()
        .execute(
            "INSERT INTO sessions(id,title,status,recording_device_id) \
             VALUES('sx','t','recording','AAAABBBBCCCCDDDD')",
            [],
        )
        .unwrap();
    let cs = read_local_changes_since(a.conn(), 0).unwrap();

    // A fresh device B CRRified `sessions` at the BASE (no recording_device_id) shape.
    let fresh_base = || {
        let b = CrsqlDb::open_in_memory().unwrap();
        b.conn()
            .execute_batch(
                "CREATE TABLE sessions(id TEXT NOT NULL PRIMARY KEY, \
                 title TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'recording');",
            )
            .unwrap();
        b.conn()
            .query_row("SELECT crsql_as_crr('sessions')", [], |_| Ok(()))
            .unwrap();
        b
    };

    // Control (pre-heal): the recording_device_id change quarantines as an unknown column.
    let raw = fresh_base();
    let (_applied, quarantined) = merge_changeset(raw.conn(), &cs).unwrap();
    assert!(
        quarantined >= 1,
        "without the out-of-band alter the recording_device_id change must quarantine, got \
         {quarantined}"
    );
    assert!(!schema::column_exists(raw.conn(), "sessions", "recording_device_id").unwrap());

    // Healed path: apply_out_of_band_alters adds the CRR-tracked column, then the SAME
    // changeset applies with zero quarantine and the value converges.
    let healed = fresh_base();
    apply_out_of_band_alters(healed.conn()).unwrap();
    assert!(schema::column_exists(healed.conn(), "sessions", "recording_device_id").unwrap());
    let (applied, quarantined) = merge_changeset(healed.conn(), &cs).unwrap();
    assert!(applied >= 1, "the changeset must apply, got {applied}");
    assert_eq!(
        quarantined, 0,
        "with the column CRR-tracked, nothing quarantines"
    );
    assert_eq!(pending_count(healed.conn()).unwrap(), 0);

    let owner: Option<String> = healed
        .conn()
        .query_row(
            "SELECT recording_device_id FROM sessions WHERE id='sx'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owner.as_deref(), Some("AAAABBBBCCCCDDDD"));
}

/// The pull-side merge and the app's own writer share one file. `merge_changeset` and
/// `replay_pending` both READ the local schema before they WRITE, so a DEFERRED `BEGIN` has
/// to upgrade read→write mid-transaction — and in WAL a lost upgrade race returns
/// `SQLITE_BUSY_SNAPSHOT`, which SQLite's busy handler does NOT retry: the merge would fail
/// outright no matter how generous `busy_timeout` is. Taking the write lock UP FRONT
/// (`BEGIN IMMEDIATE`) makes the two writers serialize instead, so a merge that starts while
/// the app is writing WAITS and then succeeds.
#[test]
fn merge_waits_out_a_concurrent_writer_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("merge-contend.db");

    // Source device authors a change.
    let src = CrsqlDb::open_in_memory().unwrap();
    src.conn()
        .execute_batch("CREATE TABLE kv(id TEXT NOT NULL PRIMARY KEY, v TEXT NOT NULL DEFAULT '');")
        .unwrap();
    src.conn()
        .query_row("SELECT crsql_as_crr('kv')", [], |_| Ok(()))
        .unwrap();
    src.conn()
        .execute("INSERT INTO kv(id,v) VALUES('k1','v1')", [])
        .unwrap();
    let cs = read_local_changes_since(src.conn(), 0).unwrap();

    // Target on disk so a SECOND connection can hold its write lock.
    let dst = CrsqlDb::open(&path).unwrap();
    dst.conn()
        .execute_batch("PRAGMA journal_mode=WAL;")
        .unwrap();
    dst.conn()
        .execute_batch("CREATE TABLE kv(id TEXT NOT NULL PRIMARY KEY, v TEXT NOT NULL DEFAULT '');")
        .unwrap();
    dst.conn()
        .query_row("SELECT crsql_as_crr('kv')", [], |_| Ok(()))
        .unwrap();

    // The app's writer holds the write lock for a beat, then commits.
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let w = Connection::open(&writer_path).unwrap();
        w.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
        w.execute_batch("CREATE TABLE app_t(id INTEGER PRIMARY KEY);")
            .unwrap();
        w.execute_batch("BEGIN IMMEDIATE;").unwrap();
        w.execute("INSERT INTO app_t(id) VALUES(1)", []).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        w.execute_batch("COMMIT;").unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(50));

    // The merge must WAIT for the writer and then apply — never fail on contention.
    let (applied, quarantined) = merge_changeset(dst.conn(), &cs).unwrap();
    writer.join().unwrap();
    assert!(
        applied > 0,
        "the merge applied after waiting out the writer"
    );
    assert_eq!(quarantined, 0);
    assert_eq!(count(dst.conn(), "SELECT count(*) FROM kv"), 1);
    // Both writers' work survives: serialized, not lost.
    assert_eq!(count(dst.conn(), "SELECT count(*) FROM app_t"), 1);

    // The replay path takes the same lock discipline; with an empty queue it is a clean no-op.
    assert_eq!(replay_pending(dst.conn()).unwrap(), 0);
}
