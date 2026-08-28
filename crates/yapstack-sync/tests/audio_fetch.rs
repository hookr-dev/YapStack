// SPDX-License-Identifier: AGPL-3.0-only
//! Engine tests for fetch-on-demand playback (S3, D2/D3): streaming download →
//! streaming decrypt → keep-until-clear cache, single-flight coalescing, the D2 cache
//! resolution short-circuit, honest error terminals (not-on-server / unreachable-mapped /
//! decrypt-verification), cancellation, and the dictation data-model fold-in (dictation
//! audio round-trips through the SAME queue + fetch path as session audio). Driven by the
//! `MockRelay` that faithfully models the object store; no live relay required.

use std::sync::atomic::Ordering;

use rusqlite::Connection;
use yapstack_crypto::audio_stream::AudioIdentity;
use yapstack_sync::audio::{
    self, AudioSealContext, DrainStep, FetchProgress, FetchRegistry, FetchResult, PRIORITY_BACKFILL,
};
use yapstack_sync::transport::MockRelay;
use yapstack_sync::SyncError;

const SESSION_A: &str = "11111111-1111-4111-8111-111111111111";
const PART_1: &str = "aaaaaaaaaaaa4aaa8aaaaaaaaaaaaaa1";
const DICT_1: &str = "dddddddddddd4ddd8dddddddddddddd1";
const VAULT: [u8; 32] = [0x42; 32];
const WRONG_VAULT: [u8; 32] = [0x99; 32];
const TENANT: [u8; 16] = [0x11; 16];

fn ctx() -> AudioSealContext {
    AudioSealContext {
        vault_key: VAULT,
        tenant_id: TENANT,
        epoch: 0,
    }
}

fn uuid16(s: &str) -> [u8; 16] {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    let raw = hex::decode(hex).unwrap();
    let mut out = [0u8; 16];
    out.copy_from_slice(&raw);
    out
}

fn identity(session: &str, part: &str) -> AudioIdentity {
    AudioIdentity {
        tenant_id: TENANT,
        session_id: uuid16(session),
        part_id: uuid16(part),
        epoch: 0,
    }
}

/// A DB with session_audio_parts + dictation_history (as the live schema has them).
fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE session_audio_parts (\
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, part_index INTEGER, \
            file_path TEXT, format TEXT);\
         CREATE TABLE dictation_history (\
            id TEXT PRIMARY KEY, wav_file_path TEXT, session_id TEXT);",
    )
    .unwrap();
    audio::ensure_queue_table(&conn).unwrap();
    conn
}

fn write_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

/// Seal `plaintext` for `(session, part)` and upload it through the real queue+drain so the
/// MockRelay holds the exact blob a peer would fetch.
async fn upload_blob(
    conn: &Connection,
    relay: &MockRelay,
    dir: &std::path::Path,
    part: &str,
    session: &str,
    plaintext: &[u8],
    is_dictation: bool,
) {
    let src = write_file(dir, &format!("{part}.wav"), plaintext);
    if is_dictation {
        conn.execute(
            "INSERT INTO dictation_history (id, wav_file_path) VALUES (?1, ?2)",
            rusqlite::params![part, src],
        )
        .unwrap();
    } else {
        conn.execute(
            "INSERT INTO session_audio_parts (id, session_id, part_index, file_path, format) \
             VALUES (?1, ?2, 0, ?3, 'wav')",
            rusqlite::params![part, session, src],
        )
        .unwrap();
    }
    audio::enqueue_on_save(conn, part, &src, session).unwrap();
    loop {
        let step = audio::drain_one(conn, relay, &ctx(), dir).await.unwrap();
        if step == DrainStep::Idle {
            break;
        }
        assert!(
            matches!(step, DrainStep::Uploaded { .. }),
            "unexpected {step:?}"
        );
    }
}

// ---- fetch round-trip + resolution order ---------------------------------------

#[tokio::test]
async fn fetch_streams_decrypts_and_caches_byte_equal() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();

    let plaintext = b"RIFFxxxxWAVEfmt peer playback bytes 0123456789".repeat(5000);
    upload_blob(
        &conn,
        &relay,
        dir.path(),
        PART_1,
        SESSION_A,
        &plaintext,
        false,
    )
    .await;

    let cache = dir.path().join("cache").join(format!("{PART_1}.wav"));
    let temp = dir.path().join("fetch-tmp");
    let progress = FetchProgress::default();
    let id = identity(SESSION_A, PART_1);

    let res =
        audio::fetch_blob_to_cache(&relay, &VAULT, &id, PART_1, &temp, &cache, &progress, None)
            .await
            .unwrap();
    assert_eq!(res, FetchResult::Fetched);
    assert_eq!(
        std::fs::read(&cache).unwrap(),
        plaintext,
        "cache == source WAV"
    );

    // Progress advanced and total was declared.
    let (received, total) = progress.snapshot();
    assert!(
        received > 0 && total == received,
        "received={received} total={total}"
    );
}

#[tokio::test]
async fn cache_hit_short_circuits_before_any_network() {
    let dir = tempfile::tempdir().unwrap();
    // Empty relay: if the fetch touched the network it would return NotOnServer. A present
    // cache file (D2 step 1) must win outright.
    let relay = MockRelay::new();
    let cache = dir.path().join("cache").join(format!("{PART_1}.wav"));
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, b"already cached").unwrap();
    assert!(audio::cache_hit(&cache));

    let progress = FetchProgress::default();
    let id = identity(SESSION_A, PART_1);
    let res = audio::fetch_blob_to_cache(
        &relay,
        &VAULT,
        &id,
        PART_1,
        &dir.path().join("tmp"),
        &cache,
        &progress,
        None,
    )
    .await
    .unwrap();
    assert_eq!(res, FetchResult::Fetched);
    assert_eq!(progress.snapshot(), (0, 0), "no bytes moved on a cache hit");
}

#[tokio::test]
async fn missing_server_blob_is_not_on_server() {
    let dir = tempfile::tempdir().unwrap();
    let relay = MockRelay::new(); // nothing uploaded
    let cache = dir.path().join("cache").join(format!("{PART_1}.wav"));
    let progress = FetchProgress::default();
    let id = identity(SESSION_A, PART_1);
    let res = audio::fetch_blob_to_cache(
        &relay,
        &VAULT,
        &id,
        PART_1,
        &dir.path().join("tmp"),
        &cache,
        &progress,
        None,
    )
    .await
    .unwrap();
    assert_eq!(res, FetchResult::NotOnServer);
    assert!(!cache.exists(), "no cache file when the server has no blob");
}

#[tokio::test]
async fn decrypt_verification_failure_leaves_no_cache_entry() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    upload_blob(
        &conn,
        &relay,
        dir.path(),
        PART_1,
        SESSION_A,
        b"secret audio".repeat(500).as_slice(),
        false,
    )
    .await;

    let cache = dir.path().join("cache").join(format!("{PART_1}.wav"));
    let progress = FetchProgress::default();
    let id = identity(SESSION_A, PART_1);
    // Wrong vault key → the identity wrap unwrap fails → CryptoError → SyncError::Crypto.
    let err = audio::fetch_blob_to_cache(
        &relay,
        &WRONG_VAULT,
        &id,
        PART_1,
        &dir.path().join("tmp"),
        &cache,
        &progress,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SyncError::Crypto(_)), "got {err:?}");
    assert!(
        !cache.exists(),
        "a tamper/verification failure must not poison the cache"
    );
}

#[tokio::test]
async fn wrong_part_identity_fails_verification() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    upload_blob(
        &conn,
        &relay,
        dir.path(),
        PART_1,
        SESSION_A,
        b"bound to PART_1".repeat(300).as_slice(),
        false,
    )
    .await;

    let cache = dir.path().join("cache").join(format!("{PART_1}.wav"));
    let progress = FetchProgress::default();
    // Correct vault, but claim a DIFFERENT part_id in the AAD → unwrap must reject.
    let id = identity(SESSION_A, DICT_1);
    let err = audio::fetch_blob_to_cache(
        &relay,
        &VAULT,
        &id,
        PART_1,
        &dir.path().join("tmp"),
        &cache,
        &progress,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SyncError::Crypto(_)), "got {err:?}");
}

#[tokio::test]
async fn cancel_before_download_yields_cancelled_no_cache() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    upload_blob(
        &conn,
        &relay,
        dir.path(),
        PART_1,
        SESSION_A,
        b"cancellable".repeat(200).as_slice(),
        false,
    )
    .await;

    let cache = dir.path().join("cache").join(format!("{PART_1}.wav"));
    let progress = FetchProgress::default();
    progress.request_cancel();
    let id = identity(SESSION_A, PART_1);
    let res = audio::fetch_blob_to_cache(
        &relay,
        &VAULT,
        &id,
        PART_1,
        &dir.path().join("tmp"),
        &cache,
        &progress,
        None,
    )
    .await
    .unwrap();
    assert_eq!(res, FetchResult::Cancelled);
    assert!(!cache.exists());
}

// ---- single-flight coalescing + global cap / FIFO admission ---------------------

use std::sync::{Arc, Mutex};

/// A submit whose starter just records the part id — the registry runs starters inline
/// (production starters spawn the worker thread; tests observe start order directly) and
/// `task_finished()` stands in for a worker completing.
fn recording_starter(
    log: &Arc<Mutex<Vec<&'static str>>>,
    id: &'static str,
) -> Box<dyn FnOnce() + Send + 'static> {
    let log = log.clone();
    Box::new(move || log.lock().unwrap().push(id))
}

#[test]
fn registry_coalesces_one_slot_per_part() {
    let reg = FetchRegistry::new();
    let started = Arc::new(Mutex::new(Vec::new()));
    let a = reg.submit(PART_1, recording_starter(&started, "p1"));
    assert_eq!(*started.lock().unwrap(), vec!["p1"], "first submit starts");
    let b = reg.submit(PART_1, recording_starter(&started, "p1-again"));
    assert_eq!(
        *started.lock().unwrap(),
        vec!["p1"],
        "second submit coalesces — its starter is dropped"
    );
    assert!(Arc::ptr_eq(&a, &b), "same shared slot");

    // Different part → its own slot.
    let c = reg.submit(DICT_1, recording_starter(&started, "d1"));
    assert!(!Arc::ptr_eq(&a, &c));

    // Shared progress: a write through one handle is visible through the other.
    a.progress.received.store(4096, Ordering::Relaxed);
    assert_eq!(b.progress.snapshot().0, 4096);

    // Terminal outcome is visible to every subscriber; remove resets for a retry.
    a.finish(Ok(FetchResult::Fetched));
    assert_eq!(b.outcome(), Some(Ok(FetchResult::Fetched)));
    reg.remove(PART_1);
    reg.task_finished(); // the worker's permit release
    let started_before = started.lock().unwrap().len();
    let _d = reg.submit(PART_1, recording_starter(&started, "p1-retry"));
    assert_eq!(
        started.lock().unwrap().len(),
        started_before + 1,
        "after remove, the next submit starts a fresh fetch"
    );
}

#[test]
fn cap_runs_two_queues_third_then_starts_it() {
    let reg = FetchRegistry::with_cap(2);
    let started = Arc::new(Mutex::new(Vec::new()));
    let s1 = reg.submit("part-a", recording_starter(&started, "a"));
    let s2 = reg.submit("part-b", recording_starter(&started, "b"));
    let s3 = reg.submit("part-c", recording_starter(&started, "c"));
    assert_eq!(
        *started.lock().unwrap(),
        vec!["a", "b"],
        "only cap=2 downloads start"
    );
    assert!(!s1.is_queued() && !s2.is_queued());
    assert!(s3.is_queued(), "third part waits in the admission queue");

    // One running fetch finishes → the queued part starts and stops reading queued.
    reg.task_finished();
    assert_eq!(*started.lock().unwrap(), vec!["a", "b", "c"]);
    assert!(!s3.is_queued());
}

#[test]
fn queue_starts_in_fifo_submission_order() {
    let reg = FetchRegistry::with_cap(1);
    let started = Arc::new(Mutex::new(Vec::new()));
    for (id, tag) in [
        ("part-a", "a"),
        ("part-b", "b"),
        ("part-c", "c"),
        ("part-d", "d"),
    ] {
        reg.submit(id, recording_starter(&started, tag));
    }
    assert_eq!(*started.lock().unwrap(), vec!["a"]);
    reg.task_finished();
    reg.task_finished();
    reg.task_finished();
    assert_eq!(
        *started.lock().unwrap(),
        vec!["a", "b", "c", "d"],
        "queued parts start in the order they were submitted"
    );
}

#[test]
fn cancel_if_queued_removes_queued_but_leaves_in_flight() {
    let reg = FetchRegistry::with_cap(1);
    let started = Arc::new(Mutex::new(Vec::new()));
    reg.submit("part-a", recording_starter(&started, "a")); // running
    reg.submit("part-b", recording_starter(&started, "b")); // queued

    // In-flight → left alone (the download completes into the cache).
    assert!(!reg.cancel_if_queued("part-a"));
    assert!(reg.get("part-a").is_some());

    // Queued → removed before it ever starts; its slot is gone.
    assert!(reg.cancel_if_queued("part-b"));
    assert!(reg.get("part-b").is_none());
    reg.task_finished(); // part-a's worker ends
    assert_eq!(
        *started.lock().unwrap(),
        vec!["a"],
        "a cancelled-while-queued part never starts"
    );
}

#[test]
fn remove_purges_a_queued_entry_too() {
    let reg = FetchRegistry::with_cap(1);
    let started = Arc::new(Mutex::new(Vec::new()));
    reg.submit("part-a", recording_starter(&started, "a"));
    reg.submit("part-b", recording_starter(&started, "b")); // queued
    reg.remove("part-b"); // e.g. user X on a queued part
    reg.task_finished();
    assert_eq!(
        *started.lock().unwrap(),
        vec!["a"],
        "a removed queued part must not start later"
    );
    assert!(reg.get("part-b").is_none());
}

/// FETCH POLISH item 5: high-class submissions join the FRONT segment of the admission
/// queue (FIFO within the class), so the session the user is looking at starts before
/// background dictation prefetches that queued earlier — without reordering the session's
/// own part sequence.
#[test]
fn high_priority_submissions_jump_queued_normal_prefetches() {
    let reg = FetchRegistry::with_cap(1);
    let started = Arc::new(Mutex::new(Vec::new()));
    reg.submit("running-x", recording_starter(&started, "x")); // occupies the one permit
    reg.submit("dict-a", recording_starter(&started, "dA")); // normal prefetches queue first
    reg.submit("dict-b", recording_starter(&started, "dB"));
    // The session view submits its parts high, in part_index order.
    reg.submit_with_priority("sess-p0", recording_starter(&started, "p0"), true);
    reg.submit_with_priority("sess-p1", recording_starter(&started, "p1"), true);
    for _ in 0..4 {
        reg.task_finished();
    }
    assert_eq!(
        *started.lock().unwrap(),
        vec!["x", "p0", "p1", "dA", "dB"],
        "session parts start ahead of earlier-queued prefetches, in their own order"
    );
}

/// `promote` moves an already-QUEUED normal entry into the high class in place; running,
/// absent, and already-high entries are untouched.
#[test]
fn promote_moves_queued_entry_ahead_but_never_touches_running() {
    let reg = FetchRegistry::with_cap(1);
    let started = Arc::new(Mutex::new(Vec::new()));
    reg.submit("running-x", recording_starter(&started, "x"));
    reg.submit("dict-a", recording_starter(&started, "dA"));
    reg.submit("sess-p0", recording_starter(&started, "p0")); // queued NORMAL first…

    assert!(!reg.promote("running-x"), "running → untouched");
    assert!(!reg.promote("ghost"), "absent → untouched");
    assert!(reg.promote("sess-p0"), "queued normal → promoted");
    assert!(!reg.promote("sess-p0"), "already high → no-op");

    reg.task_finished();
    reg.task_finished();
    assert_eq!(
        *started.lock().unwrap(),
        vec!["x", "p0", "dA"],
        "the promoted part starts ahead of the earlier-queued normal one"
    );
    // Promotion order is preserved among promoted entries.
    let reg2 = FetchRegistry::with_cap(1);
    let started2 = Arc::new(Mutex::new(Vec::new()));
    reg2.submit("running-x", recording_starter(&started2, "x"));
    reg2.submit("q1", recording_starter(&started2, "q1"));
    reg2.submit("q2", recording_starter(&started2, "q2"));
    assert!(reg2.promote("q1"));
    assert!(reg2.promote("q2")); // joins BEHIND q1 in the high segment
    reg2.task_finished();
    reg2.task_finished();
    assert_eq!(*started2.lock().unwrap(), vec!["x", "q1", "q2"]);
}

/// FETCH POLISH item 4 (fetched-slot cleanup): the worker removes a `Fetched` slot right
/// after `finish` — once inactive, `cache_clear` can finally reclaim that file (previously
/// a lingering Fetched slot made the clear skip it forever), and a fresh submit after
/// removal is a genuinely new attempt.
#[test]
fn fetched_slot_removal_lets_cache_clear_reclaim_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("done-part.wav"), vec![0u8; 40]).unwrap();

    let reg = FetchRegistry::with_cap(2);
    let slot = reg.submit("done-part", Box::new(|| {}));
    slot.finish(Ok(FetchResult::Fetched));
    // Slot still live (terminal unobserved) → the clear must skip the file.
    let removed = audio::cache_clear(&cache, |p| reg.is_active(p));
    assert_eq!(removed.files, 0, "live Fetched slot protects the file");

    // The worker's cleanup convention: remove after finish, then release the permit.
    reg.remove("done-part");
    reg.task_finished();
    assert!(!reg.is_active("done-part"));
    let removed = audio::cache_clear(&cache, |p| reg.is_active(p));
    assert_eq!(removed.files, 1, "inactive part is reclaimable");
    assert!(!cache.join("done-part.wav").exists());
}

/// The still-uploading re-probe seam: after a NotOnServer terminal the desktop CLEARS the
/// slot (it is no longer a stable terminal), so a later re-probe submit starts a genuinely
/// fresh attempt — this is what lets the fetch begin automatically once the source device's
/// upload lands.
#[test]
fn not_on_server_clear_then_resubmit_reattempts() {
    let reg = FetchRegistry::with_cap(2);
    let started = Arc::new(Mutex::new(Vec::new()));
    let slot = reg.submit(PART_1, recording_starter(&started, "probe-1"));
    slot.finish(Ok(FetchResult::NotOnServer));
    reg.task_finished(); // worker ended
    reg.remove(PART_1); // the desktop's NotOnServer slot-clear
    assert!(!reg.is_active(PART_1));

    let slot2 = reg.submit(PART_1, recording_starter(&started, "probe-2"));
    assert_eq!(
        *started.lock().unwrap(),
        vec!["probe-1", "probe-2"],
        "the re-probe starts a fresh fetch attempt"
    );
    assert_eq!(slot2.outcome(), None, "fresh slot, no stale terminal");
}

// ---- disk precheck (NoSpace terminal) ------------------------------------------

#[tokio::test]
async fn insufficient_space_yields_no_space_terminal_and_no_cache() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    let plaintext = b"big audio payload".repeat(1000);
    upload_blob(
        &conn,
        &relay,
        dir.path(),
        PART_1,
        SESSION_A,
        &plaintext,
        false,
    )
    .await;

    let cache = dir.path().join("cache").join(format!("{PART_1}.wav"));
    let progress = FetchProgress::default();
    let id = identity(SESSION_A, PART_1);
    // The probe positively reports a nearly-full volume.
    let probe: &audio::FreeSpaceFn = &|_p: &std::path::Path| Some(1024);
    let res = audio::fetch_blob_to_cache(
        &relay,
        &VAULT,
        &id,
        PART_1,
        &dir.path().join("tmp"),
        &cache,
        &progress,
        Some(probe),
    )
    .await
    .unwrap();
    match res {
        FetchResult::NoSpace { needed } => {
            // Budget: blob*2 + 256MB headroom (from the true encrypted length, ≥ plaintext).
            assert!(
                needed >= plaintext.len() as u64 * 2 + audio::NO_SPACE_HEADROOM_BYTES,
                "needed={needed}"
            );
        }
        other => panic!("expected NoSpace, got {other:?}"),
    }
    assert!(!cache.exists(), "no cache entry on a NoSpace terminal");
}

#[tokio::test]
async fn indeterminate_probe_fails_open_and_fetches() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    let plaintext = b"fits fine".repeat(500);
    upload_blob(
        &conn,
        &relay,
        dir.path(),
        PART_1,
        SESSION_A,
        &plaintext,
        false,
    )
    .await;

    let cache = dir.path().join("cache").join(format!("{PART_1}.wav"));
    let id = identity(SESSION_A, PART_1);
    // Probe can't determine → NEVER fabricate NoSpace; the fetch proceeds.
    let none_probe: &audio::FreeSpaceFn = &|_p: &std::path::Path| None;
    let res = audio::fetch_blob_to_cache(
        &relay,
        &VAULT,
        &id,
        PART_1,
        &dir.path().join("tmp"),
        &cache,
        &FetchProgress::default(),
        Some(none_probe),
    )
    .await
    .unwrap();
    assert_eq!(res, FetchResult::Fetched);
    assert_eq!(std::fs::read(&cache).unwrap(), plaintext);

    // And a roomy volume also fetches (the check passes, not merely skips).
    let cache2 = dir.path().join("cache").join("second-copy.wav");
    std::fs::remove_file(&cache).unwrap();
    let roomy: &audio::FreeSpaceFn = &|_p: &std::path::Path| Some(u64::MAX);
    let res2 = audio::fetch_blob_to_cache(
        &relay,
        &VAULT,
        &id,
        PART_1,
        &dir.path().join("tmp"),
        &cache2,
        &FetchProgress::default(),
        Some(roomy),
    )
    .await
    .unwrap();
    assert_eq!(res2, FetchResult::Fetched);
}

// ---- cache stats / clear safety -------------------------------------------------

#[test]
fn cache_stats_counts_settled_files_only() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("aaa.wav"), vec![0u8; 100]).unwrap();
    std::fs::write(cache.join("bbb.mp3"), vec![0u8; 50]).unwrap();
    // An in-flight decrypt temp must not count as settled cache content.
    std::fs::write(
        cache.join(format!("{}partial", audio::FETCH_TEMP_PREFIX)),
        vec![0u8; 999],
    )
    .unwrap();
    let stats = audio::cache_stats(&cache);
    assert_eq!(stats.files, 2);
    assert_eq!(stats.bytes, 150);

    // Missing dir = empty cache, not an error.
    assert_eq!(
        audio::cache_stats(&dir.path().join("nope")),
        audio::CacheStats::default()
    );
}

#[test]
fn cache_clear_skips_in_flight_parts_and_decrypt_temps() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("active.wav"), vec![0u8; 10]).unwrap();
    std::fs::write(cache.join("settled.wav"), vec![0u8; 20]).unwrap();
    let temp_name = format!("{}dl", audio::FETCH_TEMP_PREFIX);
    std::fs::write(cache.join(&temp_name), vec![0u8; 30]).unwrap();

    // "active" has a live registry slot (its fetch is in flight) → must be skipped.
    let reg = FetchRegistry::with_cap(2);
    reg.submit("active", Box::new(|| {}));
    let removed = audio::cache_clear(&cache, |part| reg.is_active(part));

    assert_eq!(removed.files, 1, "only the settled file is removed");
    assert_eq!(removed.bytes, 20);
    assert!(cache.join("active.wav").exists(), "in-flight part survives");
    assert!(cache.join(&temp_name).exists(), "decrypt temp survives");
    assert!(!cache.join("settled.wav").exists());
}

// ---- dictation data-model fold-in ---------------------------------------------

#[tokio::test]
async fn dictation_audio_backfills_and_round_trips_like_session_audio() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();

    // A dictation WAV on disk with a dictation_history row but NO session_audio_parts row.
    let plaintext = b"dictated words captured to WAV".repeat(400);
    let src = write_file(dir.path(), "dict.wav", &plaintext);
    conn.execute(
        "INSERT INTO dictation_history (id, wav_file_path) VALUES (?1, ?2)",
        rusqlite::params![DICT_1, src],
    )
    .unwrap();

    // Backfill walk includes dictation rows (session_id == part_id, self-referential).
    let report = audio::backfill_walk(&conn, |p| std::path::Path::new(p).exists()).unwrap();
    assert!(report.enqueued >= 1);
    let s = audio::lane_status(&conn).unwrap();
    assert_eq!(
        s.pending, 1,
        "the dictation part was enqueued at backfill priority"
    );

    // The dictation part drains through the SAME uploader (not dropped as a deleted part —
    // part_source_exists finds it in dictation_history).
    let step = audio::drain_one(&conn, &relay, &ctx(), dir.path())
        .await
        .unwrap();
    assert!(matches!(step, DrainStep::Uploaded { .. }), "got {step:?}");
    assert_eq!(audio::lane_status(&conn).unwrap().done, 1);

    // And it fetches back byte-equal with the dictation identity (session_id == part_id).
    let cache = dir.path().join("cache").join(format!("{DICT_1}.wav"));
    let progress = FetchProgress::default();
    let id = identity(DICT_1, DICT_1);
    let res = audio::fetch_blob_to_cache(
        &relay,
        &VAULT,
        &id,
        DICT_1,
        &dir.path().join("tmp"),
        &cache,
        &progress,
        None,
    )
    .await
    .unwrap();
    assert_eq!(res, FetchResult::Fetched);
    assert_eq!(std::fs::read(&cache).unwrap(), plaintext);
}

#[tokio::test]
async fn deleted_dictation_source_drops_queue_entry_silently() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db();
    let relay = MockRelay::new();
    let src = write_file(dir.path(), "gone.wav", b"soon-deleted");
    // Enqueue a dictation part whose dictation_history row does NOT exist (deleted).
    audio::enqueue_on_save(&conn, DICT_1, &src, DICT_1).unwrap();
    assert_eq!(audio::lane_status(&conn).unwrap().pending, 1);

    let step = audio::drain_one(&conn, &relay, &ctx(), dir.path())
        .await
        .unwrap();
    assert!(
        matches!(step, DrainStep::DroppedDeleted { .. }),
        "a deleted source row → silent drop, got {step:?}"
    );
    let s = audio::lane_status(&conn).unwrap();
    assert_eq!(s.pending + s.done + s.failed, 0, "entry removed");
}

// Silence an unused-const warning when only some consts are referenced per build.
#[allow(dead_code)]
const _USES: i64 = PRIORITY_BACKFILL;
