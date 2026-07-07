// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end auth flow against a LIVE Postgres. These tests are `#[ignore]` because
//! no Postgres is present in the build/CI-check environment (T007 was authored without
//! a live DB; sqlx uses runtime queries, no compile-time `DATABASE_URL` needed).
//!
//! To run locally:
//! ```text
//! createdb yapstack_test
//! DATABASE_URL=postgres://localhost/yapstack_test cargo test -p yapstack-server -- --ignored
//! ```
//! The application should normally connect as the non-owner `yapstack_app` role; these
//! tests connect as the migration owner (which FORCE ROW LEVEL SECURITY still covers).
//!
//! T009 review targets exercised here: signup second-hash storage, login verifier
//! path, refresh ROTATION, and refresh REUSE DETECTION revoking the whole family.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use yapstack_server::{build_router, AppState, Config};

fn client_auth_key(password: &str, salt_enc: &[u8]) -> String {
    let stretch = yapstack_crypto::kdf::client_stretch(password.as_bytes(), salt_enc).unwrap();
    let (auth, _master) = yapstack_crypto::kdf::split_keys(&stretch);
    B64.encode(auth)
}

/// A stand-in for the client-side recovery auth key derived from the 160-bit recovery
/// code (§6). The server only ever second-hashes it; the exact derivation is the
/// client's (T010e). Here we just use the recovery bytes as the second-hash input.
fn client_recovery_auth_key(recovery_bytes: &[u8; 20]) -> String {
    B64.encode(recovery_bytes)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn setup() -> AppState {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for --ignored tests");
    let pool = yapstack_server::db::connect(&url).await.unwrap();
    yapstack_server::db::migrate(&pool).await.unwrap();
    let cfg = Config::from_toml_str(&format!(
        "database_url = \"{url}\"\njwt_secret = \"test-secret\"\nserver_pepper = \"test-pepper\"\n"
    ))
    .unwrap();
    AppState::new(pool, cfg)
}

const RECOVERY_BYTES: [u8; 20] = [0x5au8; 20];

fn signup_body(email: &str, salt_enc: &[u8], auth_key_b64: &str) -> Value {
    signup_body_with_client(email, salt_enc, auth_key_b64, uuid::Uuid::new_v4())
}

fn signup_body_with_client(
    email: &str,
    salt_enc: &[u8],
    auth_key_b64: &str,
    client_id: uuid::Uuid,
) -> Value {
    // Placeholder committing-envelope blobs (the server stores them opaquely).
    let wrapped = B64.encode([0x01u8; 73]);
    let vault_key = [0x33u8; 32];
    let sk = yapstack_crypto::sign::roster_signing_key(&vault_key);
    let roster = json!({ "version": 1, "counter": 0, "vault_key_epoch": 0 });
    let sig = yapstack_crypto::sign::sign_roster(&vault_key, roster.to_string().as_bytes());
    json!({
        "email": email,
        "auth_key": auth_key_b64,
        "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES),
        "salt_enc": B64.encode(salt_enc),
        "wrapped_vault_key_password": wrapped,
        "wrapped_vault_key_recovery": wrapped,
        "device_list": {
            "device_list": roster,
            "signature": B64.encode(sig),
            "counter": 0,
            "vault_key_epoch": 0,
            "client_id": client_id,
            "ed25519_pub": B64.encode(sk.verifying_key().to_bytes()),
            "label": "test-device"
        }
    })
}

fn signed_roster(vault_key: &[u8; 32], counter: i64, epoch: i64) -> (Value, String) {
    let roster = json!({ "version": 1, "counter": counter, "vault_key_epoch": epoch });
    let sig = yapstack_crypto::sign::sign_roster(vault_key, roster.to_string().as_bytes());
    (roster, B64.encode(sig))
}

async fn put_auth(
    app: &axum::Router,
    path: &str,
    access: &str,
    body: Value,
) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(path)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {access}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get_auth(app: &axum::Router, path: &str, access: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header("authorization", format!("Bearer {access}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn post(app: &axum::Router, path: &str, body: Value) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn signup_login_refresh_and_reuse_detection() {
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);

    // signup
    let resp = post(
        &app,
        "/auth/signup",
        signup_body(&email, &salt_enc, &auth_key),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // login begin -> returns salt_enc
    let resp = post(&app, "/auth/login/begin", json!({ "email": email })).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // login finish -> tokens
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({ "email": email, "auth_key": auth_key }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tokens = body_json(resp).await;
    let refresh1 = tokens["refresh_token"].as_str().unwrap().to_string();

    // refresh rotates
    let resp = post(&app, "/auth/refresh", json!({ "refresh_token": refresh1 })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let refresh2 = body_json(resp).await["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // reuse of the OLD refresh token -> reuse detection revokes the family
    let resp = post(&app, "/auth/refresh", json!({ "refresh_token": refresh1 })).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // and now even the (previously valid) rotated token is dead
    let resp = post(&app, "/auth/refresh", json!({ "refresh_token": refresh2 })).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn concurrent_reuse_of_same_fresh_token_revokes_family() {
    // Regression for F3: without `FOR UPDATE` on the refresh_tokens read, two
    // concurrent uses of the SAME still-fresh token both read rotated_at=NULL and both
    // rotate, evading reuse detection. With the row lock, the second use blocks until
    // the first commits, then sees rotated_at set → reuse detected → family revoked.
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);

    let resp = post(
        &app,
        "/auth/signup",
        signup_body(&email, &salt_enc, &auth_key),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = post(
        &app,
        "/auth/login/finish",
        json!({ "email": email, "auth_key": auth_key }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let refresh1 = body_json(resp).await["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Fire two refreshes for the SAME fresh token concurrently.
    let (r_a, r_b) = tokio::join!(
        post(&app, "/auth/refresh", json!({ "refresh_token": refresh1 })),
        post(&app, "/auth/refresh", json!({ "refresh_token": refresh1 })),
    );
    let (sa, sb) = (r_a.status(), r_b.status());

    // Exactly one winner (rotates) and one loser (reuse detected) — never two winners.
    let oks = [sa, sb].iter().filter(|s| **s == StatusCode::OK).count();
    let unauth = [sa, sb]
        .iter()
        .filter(|s| **s == StatusCode::UNAUTHORIZED)
        .count();
    assert_eq!(oks, 1, "exactly one concurrent refresh may win");
    assert_eq!(
        unauth, 1,
        "the second concurrent use must be reuse-detected"
    );

    // The winner minted a child, but the loser's reuse detection revoked the WHOLE
    // family — so that freshly minted child is already dead.
    let winner = if sa == StatusCode::OK { r_a } else { r_b };
    let child = body_json(winner).await["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = post(&app, "/auth/refresh", json!({ "refresh_token": child })).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn login_with_wrong_password_is_unauthorized() {
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let good = client_auth_key("correct horse battery staple", &salt_enc);
    let bad = client_auth_key("wrong password", &salt_enc);

    let resp = post(&app, "/auth/signup", signup_body(&email, &salt_enc, &good)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = post(
        &app,
        "/auth/login/finish",
        json!({ "email": email, "auth_key": bad }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// --------------------------------------------------------------- T010d: recover

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn recover_round_trip_serves_recovery_wrap() {
    // The recovery-wrapped vault key is stored at signup but NEVER served on the login
    // path; only /auth/recover, after a valid recovery-code second-hash check, serves it.
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);

    let resp = post(
        &app,
        "/auth/signup",
        signup_body(&email, &salt_enc, &auth_key),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // login/finish never carries the recovery wrap.
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({ "email": email, "auth_key": auth_key }),
    )
    .await;
    let login = body_json(resp).await;
    assert!(login.get("wrapped_vault_key_recovery").is_none());

    // Correct recovery code -> serves the recovery wrap.
    let good_rec = client_recovery_auth_key(&RECOVERY_BYTES);
    let resp = post(
        &app,
        "/auth/recover",
        json!({ "email": email, "recovery_auth_key": good_rec }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let recovered = body_json(resp).await;
    assert!(
        recovered["wrapped_vault_key_recovery"].as_str().is_some(),
        "recover must serve the recovery-wrapped vault key"
    );

    // Wrong recovery code -> uniform unauthorized (no oracle).
    let bad_rec = client_recovery_auth_key(&[0x00u8; 20]);
    let resp = post(
        &app,
        "/auth/recover",
        json!({ "email": email, "recovery_auth_key": bad_rec }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Unknown email -> same uniform unauthorized.
    let resp = post(
        &app,
        "/auth/recover",
        json!({ "email": "nobody@example.com", "recovery_auth_key": good_rec }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------- T010d: pending enroll + roster

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn unknown_device_enrolls_pending_then_roster_promotes() {
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let first_client = uuid::Uuid::new_v4();

    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&email, &salt_enc, &auth_key, first_client),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // The signup device is ACTIVE and its access token carries first_client — this is
    // the token that may write the roster (only active callers may, §7.5 / T012b).
    let first_access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    // A NEW device logs in presenting its own client_id + Ed25519 pubkey -> enrolled
    // PENDING (not auto-promoted). It also receives the roster signature (§7.5 step 2).
    let new_client = uuid::Uuid::new_v4();
    let new_pub = B64.encode([0x07u8; 32]);
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email,
            "auth_key": auth_key,
            "client_id": new_client,
            "ed25519_pub": new_pub,
            "label": "new-laptop"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let login = body_json(resp).await;
    let access = login["access_token"].as_str().unwrap().to_string();
    assert!(
        login["signature"].as_str().is_some(),
        "login must return the roster signature for §7.5 verification"
    );

    // GET /devices shows the new device as PENDING, the first as ACTIVE.
    let resp = get_auth(&app, "/devices", &access).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let devices = body_json(resp).await;
    let list = devices["devices"].as_array().unwrap();
    let new_dev = list
        .iter()
        .find(|d| d["client_id"] == json!(new_client.to_string()))
        .expect("new device present");
    assert_eq!(new_dev["status"], json!("pending"));
    let first_dev = list
        .iter()
        .find(|d| d["client_id"] == json!(first_client.to_string()))
        .expect("first device present");
    assert_eq!(first_dev["status"], json!("active"));

    // The approving device (the ACTIVE first device) uploads a re-signed roster
    // (counter 0 -> 1) naming the new device active -> promotion pending->active. The
    // write uses first_access: only an active caller may write the roster (T012b).
    let vault_key = [0x33u8; 32];
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &first_access,
        json!({
            "device_list": roster,
            "signature": sig,
            "counter": 1,
            "vault_key_epoch": 0,
            "active_devices": [first_client, new_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = get_auth(&app, "/devices", &access).await;
    let devices = body_json(resp).await;
    let new_dev = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_id"] == json!(new_client.to_string()))
        .unwrap()
        .clone();
    assert_eq!(
        new_dev["status"],
        json!("active"),
        "roster promoted the device"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn roster_counter_anti_rollback_rejects_stale_or_equal() {
    // §7.4 / C2: the relay accepts a roster ONLY if its counter STRICTLY exceeds the
    // stored one — a replayed old/equal counter (re-admitting an evicted device) is
    // rejected, WITHOUT the relay reading roster content.
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);

    let resp = post(
        &app,
        "/auth/signup",
        signup_body(&email, &salt_enc, &auth_key),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    let vault_key = [0x33u8; 32];

    // Advance to counter 1 (stored is 0 from signup).
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({ "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0, "active_devices": [] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Equal counter -> rejected.
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({ "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0, "active_devices": [] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Stale (lower) counter -> rejected.
    let (roster, sig) = signed_roster(&vault_key, 0, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({ "device_list": roster, "signature": sig, "counter": 0, "vault_key_epoch": 0, "active_devices": [] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Lower vault_key_epoch -> rejected even with a higher counter.
    let (roster, sig) = signed_roster(&vault_key, 5, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({ "device_list": roster, "signature": sig, "counter": 5, "vault_key_epoch": -1, "active_devices": [] }),
    )
    .await;
    // vault_key_epoch is i64; a negative is < stored 0 -> rejected.
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // A malformed (short) signature is rejected as a bad request (presence check).
    let (roster, _sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({ "device_list": roster, "signature": B64.encode([0u8; 10]), "counter": 2, "vault_key_epoch": 0, "active_devices": [] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn concurrent_roster_uploads_are_compare_and_set_safe() {
    // Two concurrent uploads with the same next counter: exactly one wins (FOR UPDATE
    // serializes; the loser re-reads the advanced counter and is rejected).
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);

    let resp = post(
        &app,
        "/auth/signup",
        signup_body(&email, &salt_enc, &auth_key),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    let vault_key = [0x33u8; 32];
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let body = json!({ "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0, "active_devices": [] });

    let (r_a, r_b) = tokio::join!(
        put_auth(&app, "/devices/roster", &access, body.clone()),
        put_auth(&app, "/devices/roster", &access, body.clone()),
    );
    let (sa, sb) = (r_a.status(), r_b.status());
    let oks = [sa, sb].iter().filter(|s| **s == StatusCode::OK).count();
    let conflicts = [sa, sb]
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert_eq!(oks, 1, "exactly one concurrent roster upload may win");
    assert_eq!(conflicts, 1, "the second must be rejected by anti-rollback");
}

// --------------------------------------------- T012b: active-caller roster gate

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn pending_caller_put_roster_forbidden_active_accepted() {
    // T012b hardening: PUT /devices/roster is gated to an ALREADY-ACTIVE caller device.
    // The caller is identified by the client_id bound into its access token at login
    // (server-validated, never request-supplied). A PENDING device's write is FORBIDDEN
    // — it cannot overwrite the roster (DoS) nor self-promote via a crafted roster. The
    // ACTIVE signup device's write succeeds.
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let first_client = uuid::Uuid::new_v4();

    // signup: the first device is ACTIVE; its access token carries first_client.
    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&email, &salt_enc, &auth_key, first_client),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let active_access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    // A NEW device logs in -> enrolled PENDING; its access token carries new_client.
    let new_client = uuid::Uuid::new_v4();
    let new_pub = B64.encode([0x07u8; 32]);
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email,
            "auth_key": auth_key,
            "client_id": new_client,
            "ed25519_pub": new_pub,
            "label": "new-laptop"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let pending_access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    let vault_key = [0x33u8; 32];

    // PENDING caller -> FORBIDDEN. It even tries to name ITSELF active (self-promotion);
    // the relay refuses to let a non-active device author the roster at all.
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &pending_access,
        json!({
            "device_list": roster,
            "signature": sig,
            "counter": 1,
            "vault_key_epoch": 0,
            "active_devices": [first_client, new_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // ACTIVE caller (the signup device) with the SAME next counter -> accepted (proving
    // the pending rejection was authz, not anti-rollback: the stored counter is still 0).
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster,
            "signature": sig,
            "counter": 1,
            "vault_key_epoch": 0,
            "active_devices": [first_client, new_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}
