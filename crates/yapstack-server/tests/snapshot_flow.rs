// SPDX-License-Identifier: AGPL-3.0-only
//! RED integration tests for the snapshot-presign D8 finding:
//! "Snapshot presign advertises a generation before bytes exist; retry 409s it
//! permanently" (snapshot.rs:66-107, 126-152).
//!
//! `#[ignore]`-gated like the sibling suites (the CI-check environment has no Postgres).
//! Run locally against a live Postgres:
//!
//! ```text
//! DATABASE_URL=postgres://postgres:postgres@localhost:55432/yapstack_test \
//!   cargo test -p yapstack-server --test snapshot_flow -- --ignored
//! ```
//!
//! These need NO MinIO: `storage::presign` is a pure HMAC string-builder (no network),
//! and the current `snapshot::presign` / `snapshot::head` never call `object_exists`.
//! A well-formed but unreachable `[storage]` endpoint is enough for the routes to build
//! presigned URLs. The bug lives entirely in the DB/response logic:
//!
//!   * `snapshot::presign` COMMITs the `snapshots` row + choke metering BEFORE any bytes
//!     land. If the direct PUT to object storage then dies, the generation is recorded
//!     but no object exists — and NOTHING ever deletes a `snapshots` row.
//!   * A retry re-seals with a fresh random data key (crypto.rs:53-62), so its ciphertext
//!     hash differs → `409 "snapshot generation already published with a different hash"`
//!     — permanently, with no way to recover that generation.
//!   * `snapshot::head` reports `present: true` + a `download_url` straight from the row
//!     with no existence check, so a joining device 404s on the download and fails
//!     bootstrap hard instead of degrading to changeset replay.
//!
//! This is the exact D8 failure that `audio::presign` already fixes with a metadata-only
//! HEAD (audio.rs:17-26, 214-224); the structurally identical snapshot path never got it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;
use yapstack_server::{build_router, AppState, Config};

// --------------------------------------------------------------------- harness

/// A well-formed but deliberately-unreachable object-store endpoint. `storage::presign`
/// only signs a URL string from these fields — it never connects — so no MinIO is needed.
const FAKE_STORAGE_TOML: &str = "\n[storage]\nendpoint = \"http://127.0.0.1:1\"\n\
     region = \"us-east-1\"\nbucket = \"yapstack\"\n\
     access_key_id = \"minioadmin\"\nsecret_access_key = \"minioadmin\"\n";

async fn setup_with_storage() -> AppState {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for --ignored tests");
    let pool = yapstack_server::db::connect(&url).await.unwrap();
    yapstack_server::db::migrate(&pool).await.unwrap();
    let toml = format!(
        "database_url = \"{url}\"\njwt_secret = \"test-secret\"\n\
         server_pepper = \"test-pepper\"\n{FAKE_STORAGE_TOML}"
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    AppState::new(pool, cfg)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn signup_body(email: &str) -> Value {
    let salt_enc = [0x22u8; 16];
    let auth_key = {
        let stretch = yapstack_crypto::kdf::client_stretch(b"pw", &salt_enc).unwrap();
        let (auth, _m) = yapstack_crypto::kdf::split_keys(&stretch);
        B64.encode(auth)
    };
    let wrapped = B64.encode([0x01u8; 73]);
    let vault_key = [0x33u8; 32];
    let client_id = Uuid::new_v4();
    let sk = yapstack_crypto::sign::roster_signing_key(&vault_key);
    let roster = json!({ "version": 1, "counter": 0, "vault_key_epoch": 0 });
    let sig = yapstack_crypto::sign::sign_roster(&vault_key, roster.to_string().as_bytes());
    json!({
        "email": email, "auth_key": auth_key, "salt_enc": B64.encode(salt_enc),
        "recovery_auth_key": B64.encode([0x5au8; 20]),
        "wrapped_vault_key_password": wrapped, "wrapped_vault_key_recovery": wrapped,
        "device_list": {
            "device_list": roster, "signature": B64.encode(sig),
            "counter": 0, "vault_key_epoch": 0, "client_id": client_id,
            "ed25519_pub": B64.encode(sk.verifying_key().to_bytes()), "label": "d"
        }
    })
}

async fn signup(app: &axum::Router) -> String {
    let email = format!("snap-{}@example.com", Uuid::new_v4());
    let resp = post(app, "/auth/signup", None, signup_body(&email)).await;
    assert_eq!(resp.status(), StatusCode::OK, "signup should succeed");
    let v = body_json(resp).await;
    v["access_token"].as_str().unwrap().to_string()
}

async fn post(
    app: &axum::Router,
    path: &str,
    bearer: Option<&str>,
    body: Value,
) -> axum::response::Response {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn get(app: &axum::Router, path: &str, bearer: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

fn presign_body(sha256_hex: &str, generation: u64) -> Value {
    json!({
        "sha256": sha256_hex,
        "size": 1024,
        "generation": generation,
        "baseline_seq": 0,
    })
}

// ------------------------------------------------------------------ tests

/// PRIMARY RED GATE (needs only Postgres — no MinIO).
///
/// A seed presigns generation 5 (hash H1), the direct PUT dies (we never upload), then
/// the seed retries. Because the retry re-seals with a fresh random key its hash is
/// H2 != H1, so the CURRENT server returns `409 Conflict` — and nothing ever deletes the
/// `snapshots` row, so generation 5 is poisoned FOREVER.
///
/// A device that cannot re-publish a generation whose bytes never landed is stuck.
/// The D8 fix (mirror `audio::presign`: HEAD the object; when absent, allow a fresh
/// upload_url / a hash overwrite) makes the retry recoverable. This asserts the
/// RECOVERABLE outcome, so it FAILS on the current tree (409) and PASSES after the fix —
/// and it needs no live object store because the fix treats an unreachable HEAD as
/// "object absent".
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn snapshot_dead_put_must_be_retryable_not_409_forever() {
    let app = build_router(setup_with_storage().await);
    let tok = signup(&app).await;

    let h1 = "a".repeat(64); // 64 lowercase hex → parses as a sha256
    let h2 = "b".repeat(64); // fresh-key reseal → different ciphertext hash

    // Presign generation 5. Server records the row + meters bytes, hands back upload_url.
    let resp = post(&app, "/snapshot/presign", Some(&tok), presign_body(&h1, 5)).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "first presign should succeed"
    );
    let v = body_json(resp).await;
    assert_eq!(v["already_exists"], false);
    assert!(
        v["upload_url"].is_string(),
        "first presign returns upload_url"
    );
    // --- The direct PUT to object storage now DIES: we deliberately never upload. ---

    // The seed retries. Fresh-key reseal → different hash H2 for the SAME generation 5.
    let resp = post(&app, "/snapshot/presign", Some(&tok), presign_body(&h2, 5)).await;
    let status = resp.status();
    let v = body_json(resp).await;

    // RED: the retry after a dead PUT must be able to re-publish generation 5.
    assert_eq!(
        status,
        StatusCode::OK,
        "a dead-PUT retry must NOT be permanently 409-poisoned for that generation \
         (got {status}, body {v})"
    );
    assert!(
        v["upload_url"].is_string(),
        "the retry must hand back a fresh upload_url so the seed can finish publishing \
         generation 5; body {v}"
    );
}

/// SECONDARY RED GATE (needs only Postgres — no MinIO).
///
/// After a dead PUT, `GET /snapshot` advertises `present: true` + a `download_url` for an
/// object that was never uploaded. A joining device does `error_for_status()?` on the
/// resulting 404 and fails bootstrap HARD instead of falling back to changeset replay.
/// The D8 fix has `snapshot::head` existence-check before advertising `present: true`
/// (mirroring `audio`'s `object_exists(...).unwrap_or_default()`), so with the object
/// absent it must report `present: false`. FAILS on the current tree (present: true).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn snapshot_head_must_not_advertise_present_without_object() {
    let app = build_router(setup_with_storage().await);
    let tok = signup(&app).await;

    let h1 = "c".repeat(64);
    let resp = post(&app, "/snapshot/presign", Some(&tok), presign_body(&h1, 7)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    // --- direct PUT dies: never uploaded. ---

    let resp = get(&app, "/snapshot", &tok).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;

    // RED: head must not claim a snapshot is present when its object was never uploaded.
    assert_eq!(
        v["present"], false,
        "GET /snapshot must not advertise present:true for a generation whose bytes never \
         landed — a joining device 404s on the download_url and fails bootstrap; body {v}"
    );
}

/// The identical-hash retry after a dead PUT (needs only Postgres — no MinIO).
///
/// The original red harness pinned the CURRENT misbehaviour here: an identical-hash retry
/// phantom-published (`already_exists: true, upload_url: null`) even though the object was
/// never uploaded, so the client's `put_snapshot` returned Ok and marked a never-uploaded
/// snapshot as done. That docstring explicitly said "if a future fix flips this
/// (existence-checked idempotency), update this test to assert a fresh upload_url instead."
///
/// The D8 fix does exactly that: `snapshot::presign` HEADs the object, and because the
/// object is absent (the fake endpoint is unreachable → treated as absent), the
/// identical-hash retry now hands back a fresh `upload_url` so the seed can finish
/// publishing generation 9 instead of being told a phantom "already published".
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn snapshot_identical_retry_recovers_with_fresh_upload_url() {
    let app = build_router(setup_with_storage().await);
    let tok = signup(&app).await;

    let h1 = "d".repeat(64);
    let resp = post(&app, "/snapshot/presign", Some(&tok), presign_body(&h1, 9)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    // --- direct PUT dies: never uploaded. ---

    // Identical-hash retry: the object is absent, so the fix re-offers an upload_url.
    let resp = post(&app, "/snapshot/presign", Some(&tok), presign_body(&h1, 9)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(
        v["already_exists"], false,
        "existence-checked idempotency: an identical-hash retry whose object never landed \
         must NOT report already_exists (phantom-published); body {v}"
    );
    assert!(
        v["upload_url"].is_string(),
        "the identical-hash retry must hand back a fresh upload_url so the seed can finish \
         publishing generation 9; body {v}"
    );
}
