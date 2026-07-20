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

    let res = audio::fetch_blob_to_cache(&relay, &VAULT, &id, PART_1, &temp, &cache, &progress)
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
    )
    .await
    .unwrap();
    assert_eq!(res, FetchResult::Cancelled);
    assert!(!cache.exists());
}

// ---- single-flight coalescing --------------------------------------------------

#[test]
fn registry_coalesces_one_slot_per_part() {
    let reg = FetchRegistry::new();
    let (a, a_new) = reg.get_or_create(PART_1);
    assert!(a_new, "first caller starts the fetch");
    let (b, b_new) = reg.get_or_create(PART_1);
    assert!(!b_new, "second caller subscribes to the same slot");
    assert!(std::sync::Arc::ptr_eq(&a, &b), "same shared slot");

    // Different part → its own slot.
    let (c, c_new) = reg.get_or_create(DICT_1);
    assert!(c_new);
    assert!(!std::sync::Arc::ptr_eq(&a, &c));

    // Shared progress: a write through one handle is visible through the other.
    a.progress.received.store(4096, Ordering::Relaxed);
    assert_eq!(b.progress.snapshot().0, 4096);

    // Terminal outcome is visible to every subscriber; remove resets for a retry.
    a.finish(Ok(FetchResult::Fetched));
    assert_eq!(b.outcome(), Some(Ok(FetchResult::Fetched)));
    reg.remove(PART_1);
    let (_d, d_new) = reg.get_or_create(PART_1);
    assert!(d_new, "after remove, the next caller starts a fresh fetch");
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
