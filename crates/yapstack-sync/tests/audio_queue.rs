// SPDX-License-Identifier: AGPL-3.0-only
//! Engine tests for the audio upload queue + background uploader (S1, D4/D9), driven by the
//! `MockRelay` that faithfully models the existence-checked presign (D8) and object store.
//! These cover every §Verification row the queue owns without a live relay: durable
//! enqueue, the idempotent backfill walk, NORMAL-before-LOW draining, one-in-flight,
//! full seal→upload→download→open BYTE-EQUAL round-trip, dedup (refcount), deleted-part
//! drop, and failure surfacing through the distinct lane label.

use rusqlite::Connection;
use yapstack_sync::audio::{self, AudioSealContext, DrainStep, PRIORITY_BACKFILL, PRIORITY_NORMAL};
use yapstack_sync::transport::{MockRelay, SyncTransport};

const SESSION_A: &str = "11111111-1111-4111-8111-111111111111";
const PART_1: &str = "aaaaaaaaaaaa4aaa8aaaaaaaaaaaaaa1"; // simple-format UUIDv4
const PART_2: &str = "bbbbbbbbbbbb4bbb8bbbbbbbbbbbbbb2";
const PART_3: &str = "cccccccccccc4ccc8cccccccccccccc3";

fn ctx() -> AudioSealContext {
    AudioSealContext {
        vault_key: [0x42; 32],
        tenant_id: [0x11; 16],
        epoch: 0,
    }
}

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE session_audio_parts (\
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, part_index INTEGER, \
            file_path TEXT, format TEXT);",
    )
    .unwrap();
    audio::ensure_queue_table(&conn).unwrap();
    conn
}

fn write_part_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

fn insert_part(conn: &Connection, id: &str, session: &str, path: &str) {
    conn.execute(
        "INSERT INTO session_audio_parts (id, session_id, part_index, file_path, format) \
         VALUES (?1, ?2, 0, ?3, 'wav')",
        rusqlite::params![id, session, path],
    )
    .unwrap();
}

/// Drain the whole lane one blob at a time, returning the ordered steps.
async fn drain_all(
    conn: &Connection,
    relay: &MockRelay,
    ctx: &AudioSealContext,
    temp: &std::path::Path,
) -> Vec<DrainStep> {
    let mut steps = Vec::new();
    loop {
        let step = audio::drain_one(conn, relay, ctx, temp).await.unwrap();
        if step == DrainStep::Idle {
            break;
        }
        steps.push(step);
    }
    steps
}

#[tokio::test]
async fn enqueue_is_idempotent_and_durable() {
    let conn = db();
    assert!(audio::enqueue_on_save(&conn, PART_1, "/a.wav", SESSION_A).unwrap());
    // Re-enqueue the same part is a no-op (INSERT-OR-IGNORE by part_id).
    assert!(!audio::enqueue_on_save(&conn, PART_1, "/a.wav", SESSION_A).unwrap());
    let s = audio::lane_status(&conn).unwrap();
    assert_eq!(s.pending, 1);
    assert_eq!(s.lane_label(), "audio-upload");
}

#[tokio::test]
async fn full_roundtrip_seal_upload_download_open_is_byte_equal() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    let ctx = ctx();

    let plaintext = b"RIFFxxxxWAVEfmt some real-ish audio bytes 0123456789".repeat(4000);
    let src = write_part_file(dir.path(), "part1.wav", &plaintext);
    insert_part(&conn, PART_1, SESSION_A, &src);
    audio::enqueue_on_save(&conn, PART_1, &src, SESSION_A).unwrap();

    let steps = drain_all(&conn, &relay, &ctx, dir.path()).await;
    assert_eq!(steps.len(), 1);
    assert!(matches!(
        &steps[0],
        DrainStep::Uploaded {
            already_exists: false,
            ..
        }
    ));
    assert_eq!(audio::lane_status(&conn).unwrap().done, 1);

    // Download the blob and decrypt it → byte-equal to the source WAV.
    let fetched = dir.path().join("fetched.blob");
    assert!(relay.get_audio(PART_1, &fetched).await.unwrap());
    let mut out = Vec::new();
    let id = yapstack_crypto::audio_stream::AudioIdentity {
        tenant_id: ctx.tenant_id,
        session_id: uuid16(SESSION_A),
        part_id: uuid16(PART_1),
        epoch: ctx.epoch,
    };
    let blob = std::fs::File::open(&fetched).unwrap();
    yapstack_crypto::audio_stream::open_blob(&ctx.vault_key, &id, blob, &mut out).unwrap();
    assert_eq!(
        out, plaintext,
        "downloaded+decrypted blob must equal the source"
    );
}

#[tokio::test]
async fn single_upload_stores_object_with_mapping_refcount_one() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    let ctx = ctx();

    // (Relay-side content-addressed dedup + the D8 mapping-count refcount are proven against
    // real Postgres in the server suite; here we prove the CLIENT lane content-addresses the
    // blob and registers exactly one mapping.)
    let content = b"client-side-content-address".repeat(1000);
    let src = write_part_file(dir.path(), "d.wav", &content);
    insert_part(&conn, PART_1, SESSION_A, &src);
    audio::enqueue_on_save(&conn, PART_1, &src, SESSION_A).unwrap();

    let steps = drain_all(&conn, &relay, &ctx, dir.path()).await;
    assert!(matches!(
        &steps[0],
        DrainStep::Uploaded {
            already_exists: false,
            ..
        }
    ));

    // Recover the uploaded hash by fetching + hashing the stored object.
    let fetched = dir.path().join("f.blob");
    assert!(relay.get_audio(PART_1, &fetched).await.unwrap());
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(
        std::fs::read(&fetched).unwrap(),
    ));
    assert!(relay.audio_object_present(&sha));
    assert_eq!(
        relay.audio_refcount(&sha),
        1,
        "one part → one mapping → refcount 1"
    );
    assert_eq!(audio::lane_status(&conn).unwrap().failed, 0);
}

#[tokio::test]
async fn backfill_walk_is_idempotent_and_low_priority_drains_after_normal() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    let ctx = ctx();

    // Two historical parts on disk + one whose file is missing.
    let p1 = write_part_file(dir.path(), "h1.wav", b"hist-one".repeat(500).as_slice());
    let p2 = write_part_file(dir.path(), "h2.wav", b"hist-two".repeat(500).as_slice());
    insert_part(&conn, PART_1, SESSION_A, &p1);
    insert_part(&conn, PART_2, SESSION_A, &p2);
    insert_part(&conn, PART_3, SESSION_A, "/does/not/exist.wav");

    let exists = |path: &str| std::path::Path::new(path).exists();
    let report = audio::backfill_walk(&conn, exists).unwrap();
    assert_eq!(report.examined, 3);
    assert_eq!(report.enqueued, 2, "only existing files enqueue");
    assert_eq!(report.missing_file, 1);
    assert!(audio::backfill_walk_completed(&conn).unwrap());

    // Re-run: idempotent (already-queued parts are not re-enqueued).
    let again = audio::backfill_walk(&conn, exists).unwrap();
    assert_eq!(again.enqueued, 0);

    // A NEW recording enqueues at NORMAL and must jump ahead of the LOW backfill lane.
    let np = write_part_file(dir.path(), "new.wav", b"fresh".repeat(500).as_slice());
    // Use a distinct part id for the new recording (valid 32-hex simple UUIDv4).
    let new_part = "dddddddddddd4ddd8ddddddddddddddd";
    insert_part(&conn, new_part, SESSION_A, &np);
    audio::enqueue_on_save(&conn, new_part, &np, SESSION_A).unwrap();

    let steps = drain_all(&conn, &relay, &ctx, dir.path()).await;
    // First uploaded must be the NORMAL-priority new recording.
    match &steps[0] {
        DrainStep::Uploaded { part_id, .. } => {
            assert_eq!(
                part_id, new_part,
                "NORMAL lane drains before the backfill LOW lane"
            )
        }
        other => panic!("unexpected {other:?}"),
    }
    // All three parts uploaded, none failed.
    assert_eq!(audio::lane_status(&conn).unwrap().failed, 0);
    assert_eq!(audio::lane_status(&conn).unwrap().done, 3);
}

#[tokio::test]
async fn deleted_part_entry_is_dropped_silently() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    let ctx = ctx();

    // Enqueue a part that has NO row in session_audio_parts (deleted before upload).
    audio::enqueue_on_save(&conn, PART_1, "/gone.wav", SESSION_A).unwrap();
    let steps = drain_all(&conn, &relay, &ctx, dir.path()).await;
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], DrainStep::DroppedDeleted { .. }));
    // Dropped, not surfaced as a failure.
    let s = audio::lane_status(&conn).unwrap();
    assert_eq!(s.failed, 0);
    assert_eq!(s.pending, 0);
    assert_eq!(s.done, 0);
}

#[tokio::test]
async fn missing_source_file_fails_and_is_surfaced_then_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    let ctx = ctx();

    // Part row exists but the source file is gone → a real failure (not a silent drop).
    insert_part(&conn, PART_1, SESSION_A, "/no/such/file.wav");
    audio::enqueue_on_save(&conn, PART_1, "/no/such/file.wav", SESSION_A).unwrap();

    let steps = drain_all(&conn, &relay, &ctx, dir.path()).await;
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], DrainStep::Failed { .. }));
    let s = audio::lane_status(&conn).unwrap();
    assert_eq!(
        s.failed, 1,
        "failure surfaced under the distinct lane label — never silent"
    );

    // Manual/app-start retry re-arms it.
    assert_eq!(audio::retry_failed(&conn).unwrap(), 1);
    assert_eq!(audio::lane_status(&conn).unwrap().pending, 1);
}

#[tokio::test]
async fn reset_in_flight_rearms_crashed_entries() {
    let conn = db();
    audio::enqueue_on_save(&conn, PART_1, "/a.wav", SESSION_A).unwrap();
    // Simulate a crash mid-seal.
    conn.execute(
        "UPDATE _yapstack_audio_upload_queue SET state='sealing' WHERE part_id=?1",
        [PART_1],
    )
    .unwrap();
    assert_eq!(audio::lane_status(&conn).unwrap().sealing, 1);
    assert_eq!(audio::reset_in_flight(&conn).unwrap(), 1);
    assert_eq!(audio::lane_status(&conn).unwrap().pending, 1);
    let _ = (PRIORITY_NORMAL, PRIORITY_BACKFILL);
}

#[test]
fn sweep_orphan_temps_removes_only_prefixed_files() {
    // A3: a crash mid-seal can strand a `SEAL_TEMP_PREFIX` temp; the startup sweep must
    // reclaim exactly those and NEVER touch anything else in the dir.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Two orphaned seal temps (ours).
    let orphan_a = root.join(format!("{}abcd123", audio::SEAL_TEMP_PREFIX));
    let orphan_b = root.join(format!("{}ef56789", audio::SEAL_TEMP_PREFIX));
    std::fs::write(&orphan_a, b"garbage").unwrap();
    std::fs::write(&orphan_b, b"garbage").unwrap();
    // Files we must NEVER touch: an unrelated file, and a dir that happens to carry the
    // prefix (must not be recursed into / removed).
    let keeper = root.join("real-recording.wav");
    std::fs::write(&keeper, b"keep me").unwrap();
    let prefixed_dir = root.join(format!("{}not-a-file", audio::SEAL_TEMP_PREFIX));
    std::fs::create_dir(&prefixed_dir).unwrap();

    let removed = audio::sweep_orphan_temps(root).unwrap();
    assert_eq!(
        removed, 2,
        "only the two prefixed regular files are reclaimed"
    );
    assert!(!orphan_a.exists());
    assert!(!orphan_b.exists());
    assert!(keeper.exists(), "unrelated file untouched");
    assert!(prefixed_dir.is_dir(), "prefixed directory untouched");

    // Idempotent + missing-dir tolerant.
    assert_eq!(audio::sweep_orphan_temps(root).unwrap(), 0);
    assert_eq!(
        audio::sweep_orphan_temps(&root.join("does-not-exist")).unwrap(),
        0
    );
}

fn uuid16(s: &str) -> [u8; 16] {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    let raw = hex::decode(hex).unwrap();
    let mut out = [0u8; 16];
    out.copy_from_slice(&raw);
    out
}

// ----- Boot-time lock contention (background-lane hardening) --------------------------------
//
// The audio lane starts at the most contended moment there is: alongside the changeset
// drain's catch-up merge and capture, the CRR self-heal, and the app's own first writes. On
// the owner's boot log the walk's enqueue lost that race and returned "database is locked" —
// which, with a bare `?`, abandoned the ENTIRE walk for the session (it could only re-run at
// the next app start). A transient lock must never be a user-visible failure, so every
// background-lane write is retried, and a part that still cannot be queued is stepped over
// while the pass stays honestly incomplete.

/// An on-disk DB (in-memory DBs cannot be contended from a second connection) with the
/// app-side tables the walk reads, plus a second connection standing in for the sync drain.
fn contended_db(dir: &std::path::Path) -> (Connection, Connection) {
    let path = dir.join("contend.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; \
         CREATE TABLE session_audio_parts (\
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, part_index INTEGER, \
            file_path TEXT, format TEXT);",
    )
    .unwrap();
    audio::ensure_queue_table(&conn).unwrap();
    // Short patience on the walk's own connection so the SQLite busy handler gives up fast and
    // the engine-level retry is what has to carry it.
    conn.busy_timeout(std::time::Duration::from_millis(20))
        .unwrap();
    let writer = Connection::open(&path).unwrap();
    writer
        .busy_timeout(std::time::Duration::from_millis(20))
        .unwrap();
    (conn, writer)
}

#[test]
fn busy_retry_rides_out_transient_contention_and_gives_up_on_real_errors() {
    // The retry is CLASS-SCOPED: only SQLite's busy/locked errors are retried, because only
    // they mean "try again in a moment". A malformed statement must fail on the first attempt,
    // undelayed, exactly as before.
    let busy = || {
        yapstack_sync::SyncError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5), // SQLITE_BUSY
            Some("database is locked".into()),
        ))
    };
    assert!(audio::is_busy_error(&busy()));
    assert!(!audio::is_busy_error(&yapstack_sync::SyncError::Codec(
        "nope".into()
    )));

    // Fails twice with BUSY, then succeeds → the caller never sees an error.
    let mut attempts = 0;
    let out = audio::with_busy_retry("test-op", || {
        attempts += 1;
        if attempts < 3 {
            Err(busy())
        } else {
            Ok(attempts)
        }
    })
    .unwrap();
    assert_eq!(out, 3, "succeeded on the third attempt");

    // A non-busy error is returned immediately — no retries, no sleeping.
    let mut calls = 0;
    let err = audio::with_busy_retry("test-op", || -> Result<(), yapstack_sync::SyncError> {
        calls += 1;
        Err(yapstack_sync::SyncError::Codec("bad".into()))
    })
    .unwrap_err();
    assert_eq!(calls, 1, "a real error is not retried");
    assert!(matches!(err, yapstack_sync::SyncError::Codec(_)));
}

#[test]
fn backfill_walk_completes_once_contention_clears() {
    // The owner's exact scenario: the walk starts while another writer holds the lock. It must
    // wait it out and finish the pass — not surface a failure, not abandon the library.
    let dir = tempfile::tempdir().unwrap();
    let (conn, writer) = contended_db(dir.path());
    let p1 = write_part_file(dir.path(), "h1.wav", b"one".repeat(50).as_slice());
    let p2 = write_part_file(dir.path(), "h2.wav", b"two".repeat(50).as_slice());
    insert_part(&conn, PART_1, SESSION_A, &p1);
    insert_part(&conn, PART_2, SESSION_A, &p2);

    // A competing writer holds the write lock for longer than the walk connection's own
    // busy_timeout, then releases it.
    let holding = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let release = holding.clone();
    let holder = std::thread::spawn(move || {
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        writer
            .execute(
                "INSERT INTO session_audio_parts(id, session_id) VALUES('z','z')",
                [],
            )
            .unwrap();
        while release.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        writer.execute_batch("COMMIT;").unwrap();
    });
    // Let the holder actually take the lock, then release it while the walk is retrying.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let releaser = holding.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        releaser.store(false, std::sync::atomic::Ordering::SeqCst);
    });

    let report = audio::backfill_walk(&conn, |p| std::path::Path::new(p).exists()).unwrap();
    holder.join().unwrap();
    assert_eq!(report.skipped, 0, "contention alone never skips a part");
    assert_eq!(
        report.enqueued, 2,
        "both parts queued once the lock cleared"
    );
    assert!(report.is_complete());
    assert!(
        audio::backfill_walk_completed(&conn).unwrap(),
        "a fully-covered pass is marked complete"
    );
}

#[test]
fn backfill_walk_skips_an_unqueueable_part_and_stays_incomplete() {
    // Whatever the per-part failure is, ONE part must not abandon the pass: the rest of the
    // library still gets queued, the pass is NOT marked complete, and the next start retries
    // exactly the straggler. Modelled with a trigger that refuses one part_id — the same
    // control flow a still-locked row takes after its retry budget is spent.
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let p1 = write_part_file(dir.path(), "h1.wav", b"one".repeat(50).as_slice());
    let p2 = write_part_file(dir.path(), "h2.wav", b"two".repeat(50).as_slice());
    insert_part(&conn, PART_1, SESSION_A, &p1);
    insert_part(&conn, PART_2, SESSION_A, &p2);
    conn.execute_batch(&format!(
        "CREATE TRIGGER refuse_one BEFORE INSERT ON _yapstack_audio_upload_queue \
         WHEN NEW.part_id = '{PART_2}' \
         BEGIN SELECT RAISE(ABORT, 'simulated per-part failure'); END;"
    ))
    .unwrap();

    let report = audio::backfill_walk(&conn, |p| std::path::Path::new(p).exists()).unwrap();
    assert_eq!(report.examined, 2);
    assert_eq!(report.enqueued, 1, "the healthy part is still queued");
    assert_eq!(
        report.skipped, 1,
        "the failing part is stepped over, counted"
    );
    assert!(!report.is_complete());
    assert!(
        !audio::backfill_walk_completed(&conn).unwrap(),
        "a pass that skipped work must NOT claim completion — the next start retries it"
    );

    // Next start: the obstruction is gone, the straggler is picked up, and the pass completes.
    conn.execute_batch("DROP TRIGGER refuse_one;").unwrap();
    let again = audio::backfill_walk(&conn, |p| std::path::Path::new(p).exists()).unwrap();
    assert_eq!(again.enqueued, 1, "only the straggler was left to queue");
    assert_eq!(again.skipped, 0);
    assert!(audio::backfill_walk_completed(&conn).unwrap());
    assert_eq!(audio::lane_status(&conn).unwrap().pending, 2);
}
