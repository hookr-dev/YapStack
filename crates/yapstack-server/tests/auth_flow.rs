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
    // The auth suites exercise many logins per account inside one minute; the per-IP
    // login throttle is not what they test (client_ip_spoof.rs covers the limiter), so
    // raise it here.
    let cfg = Config::from_toml_str(&format!(
        "database_url = \"{url}\"\njwt_secret = \"test-secret\"\nserver_pepper = \"test-pepper\"\n\n[ratelimit]\nlogin_per_minute = 200\n"
    ))
    .unwrap();
    AppState::new(pool, cfg)
}

const RECOVERY_BYTES: [u8; 20] = [0x5au8; 20];

/// Deterministic per-device Ed25519 seed for tests (derived from the client_id), so a
/// test can present a device identity AND prove possession (A10) without carrying keys.
fn dev_seed(client_id: uuid::Uuid) -> [u8; 32] {
    yapstack_crypto::hkdf::expand(client_id.as_bytes(), b"test-device-seed", 32)
        .try_into()
        .unwrap()
}

fn dev_pub(client_id: uuid::Uuid) -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(&dev_seed(client_id))
        .verifying_key()
        .to_bytes()
}

fn dev_pub_b64(client_id: uuid::Uuid) -> String {
    B64.encode(dev_pub(client_id))
}

/// Fetch a login nonce and sign the A10 proof with `seed` for (`client_id`, its
/// PRESENTED public key `pub_bytes`). Returns `(nonce_b64, proof_b64)`.
async fn pop_with(
    app: &axum::Router,
    email: &str,
    client_id: uuid::Uuid,
    pub_bytes: &[u8; 32],
    seed: &[u8; 32],
) -> (String, String) {
    let resp = post(app, "/auth/login/begin", json!({ "email": email })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let nonce_b64 = body_json(resp).await["nonce"]
        .as_str()
        .expect("A10 relay issues a nonce")
        .to_string();
    let nonce = B64.decode(&nonce_b64).unwrap();
    let msg = yapstack_crypto::sign::login_pop_message(
        &nonce,
        client_id.as_bytes(),
        pub_bytes,
        &email.to_lowercase(),
    );
    let sig = yapstack_crypto::sign::sign_login_pop(seed, &msg);
    (nonce_b64, B64.encode(sig))
}

/// The honest device's proof: its own deterministic key.
async fn pop(app: &axum::Router, email: &str, client_id: uuid::Uuid) -> (String, String) {
    pop_with(
        app,
        email,
        client_id,
        &dev_pub(client_id),
        &dev_seed(client_id),
    )
    .await
}

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
            "ed25519_pub": dev_pub_b64(client_id),
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
    let new_pub = dev_pub_b64(new_client);
    let (nonce, proof) = pop(&app, &email, new_client).await;
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email,
            "auth_key": auth_key,
            "client_id": new_client,
            "ed25519_pub": new_pub,
            "label": "new-laptop",
            "nonce": nonce,
            "proof": proof
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
    let new_pub = dev_pub_b64(new_client);
    let (nonce, proof) = pop(&app, &email, new_client).await;
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email,
            "auth_key": auth_key,
            "client_id": new_client,
            "ed25519_pub": new_pub,
            "label": "new-laptop",
            "nonce": nonce,
            "proof": proof
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

/// SYNC_SPEC §15.B: the write gate covers every device-authored write, not only the
/// roster. A PENDING device's token is refused on `POST /sync/push` and
/// `POST /audio/presign` (403), while the ACTIVE signup device pushes normally. Reads are
/// not gated (a pending device may still pull). The presign check runs BEFORE the
/// object-store lookup, so this holds on a relay with no storage configured (as here).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn pending_caller_cannot_push_or_presign_active_can() {
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x24u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let first_client = uuid::Uuid::new_v4();

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

    let (pending_client, pending_access) = enroll_pending(&app, &email, &auth_key).await;

    let push_body = |client: uuid::Uuid| {
        json!({ "changes": [{
            "client_id": client,
            "client_seq": 1,
            "ciphertext": B64.encode(b"opaque"),
            "schema_version": 1,
            "engine_version": 16003
        }]})
    };

    // PENDING: push refused, presign refused (before any storage lookup).
    let resp = post_auth(
        &app,
        "/sync/push",
        &pending_access,
        push_body(pending_client),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "pending device must not push"
    );
    let presign_path = format!(
        "/audio/presign?sha256={}&size=10&part_id={}&session_id={}",
        "00".repeat(32),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4()
    );
    let resp = post_auth(&app, &presign_path, &pending_access, json!({})).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "pending device must not presign"
    );

    // PENDING may still READ.
    let resp = get_auth(&app, "/sync/pull?since=0", &pending_access).await;
    assert_eq!(resp.status(), StatusCode::OK, "pending device may pull");

    // ACTIVE: push accepted.
    let resp = post_auth(&app, "/sync/push", &active_access, push_body(first_client)).await;
    assert_eq!(resp.status(), StatusCode::OK, "active device pushes");
}

/// Bearer POST helper for the write-gate test.
async fn post_auth(
    app: &axum::Router,
    path: &str,
    access: &str,
    body: Value,
) -> axum::response::Response {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {access}"))
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

/// CRYPTO_SPEC §7.5 amendment A7 (SYNC_SPEC §15.A): a device that authenticates with the
/// recovery code and presents its identity is enrolled PENDING with a one-shot recovery
/// grant, its tokens carry its client_id, and it may upload the next roster ITSELF — the
/// lost-only-device path. The grant is strict (exactly counter+1, same epoch, names itself
/// active) and single-use; a plain pending device (password login) still cannot.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn recovery_enrols_device_and_lets_it_rebootstrap_roster_once() {
    let st = setup().await;
    let pool = st.pool.clone();
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x25u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let lost_client = uuid::Uuid::new_v4();
    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&email, &salt_enc, &auth_key, lost_client),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The only device is lost. A NEW device recovers with the code and its identity.
    let new_client = uuid::Uuid::new_v4();
    let (nonce, proof) = pop(&app, &email, new_client).await;
    let resp = post(
        &app,
        "/auth/recover",
        json!({
            "email": email,
            "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES),
            "client_id": new_client,
            "ed25519_pub": dev_pub_b64(new_client),
            "label": "replacement",
            "platform": "macos",
            "nonce": nonce,
            "proof": proof
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Enrolled pending: visible in the index, not yet a writer (push refused).
    let resp = get_auth(&app, "/devices", &access).await;
    let devices = body_json(resp).await;
    let me = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_id"] == json!(new_client))
        .expect("recovering device enrolled");
    assert_eq!(me["status"], "pending");
    let vault_key = [0x33u8; 32];

    // Wrong shape for a grant: skipping ahead (counter 2 when stored is 0) → 409; a roster
    // that does not name the caller active → 403; a changed epoch → 403; promoting a
    // THIRD pending device alongside itself → 403. None consumes the grant.
    let (third, _third_access) = enroll_pending(&app, &email, &auth_key).await;
    let (roster, sig) = signed_roster(&vault_key, 1, 1);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({
            "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 1,
            "active_devices": [lost_client, new_client]
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "recovery cannot change the epoch"
    );
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({
            "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0,
            "active_devices": [lost_client, new_client, third]
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "recovery cannot promote a third party"
    );
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [lost_client, new_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({
            "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0,
            "active_devices": [lost_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The re-bootstrap itself: exactly counter+1, same epoch, names itself → accepted.
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({
            "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0,
            "active_devices": [lost_client, new_client]
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "recovery re-bootstrap accepted"
    );
    // One-shot, proven at the column: the grant is consumed and the row is active. The
    // third party was NOT promoted.
    let (ws,): (uuid::Uuid,) = sqlx::query_as("SELECT workspace_id FROM yapstack_lookup_login($1)")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut tx = yapstack_server::db::begin_tenant_tx(&pool, ws)
        .await
        .unwrap();
    let rows: Vec<(uuid::Uuid, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT client_id, status, recovery_grant_at FROM devices WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let row = |id: uuid::Uuid| rows.iter().find(|r| r.0 == id).unwrap();
    assert_eq!(row(new_client).1, "active");
    assert!(row(new_client).2.is_none(), "grant consumed");
    assert_eq!(row(third).1, "pending", "third party untouched");

    // Now ACTIVE: pushes are accepted and the ordinary write path applies (counter 2 ok).
    let resp = post_auth(
        &app,
        "/sync/push",
        &access,
        json!({ "changes": [{
        "client_id": new_client, "client_seq": 1, "ciphertext": B64.encode(b"x"),
        "schema_version": 1, "engine_version": 16003 }]}),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "recovered device is a writer"
    );
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [lost_client, new_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A PASSWORD login of yet another new device gets no grant: still forbidden.
    let (_other, other_access) = enroll_pending(&app, &email, &auth_key).await;
    let (roster, sig) = signed_roster(&vault_key, 3, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &other_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 3, "vault_key_epoch": 0,
            "active_devices": [lost_client, new_client, _other]
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "no grant without the recovery code"
    );

    // An OLDER client recovering without an identity: tokens carry no device, nothing is
    // enrolled, and a roster write is refused (unchanged behaviour).
    let resp = post(
        &app,
        "/auth/recover",
        json!({ "email": email, "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES) }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bare_access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (roster, sig) = signed_roster(&vault_key, 3, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &bare_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 3, "vault_key_epoch": 0,
            "active_devices": [lost_client, new_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// A7 cross-account hardening: `client_id` is a GLOBAL primary key, so a recovery for
/// account T that presents a `client_id` already enrolled under account V must NOT arm a
/// grant on V's row (a stolen pending token for V plus a throwaway account T would
/// otherwise promote itself into V). The relay refuses and V's row is untouched — by the
/// tenant-scoped conflict arm itself, independent of RLS (an RLS violation also counts).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn recovery_cannot_arm_a_grant_on_another_accounts_device() {
    let st = setup().await;
    let pool = st.pool.clone();
    let app = build_router(st);
    let salt_enc = [0x26u8; 16];
    // Victim V with a pending device.
    let v_email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let v_auth = client_auth_key("victim horse battery staple", &salt_enc);
    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&v_email, &salt_enc, &v_auth, uuid::Uuid::new_v4()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (victim_pending, _) = enroll_pending(&app, &v_email, &v_auth).await;
    // Attacker T recovers its OWN account presenting V's pending client_id.
    let t_email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let t_auth = client_auth_key("attacker horse battery staple", &salt_enc);
    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&t_email, &salt_enc, &t_auth, uuid::Uuid::new_v4()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // The attacker does not hold the victim device's key: it presents its own key and
    // signs with it. Under the owner role the victim row is visible, so the proof is
    // checked against the STORED key → 401 (A10); under a non-owner role the row is
    // invisible and the tenant-scoped conflict arm refuses → 409. Either way: not armed.
    let attacker_seed = [0x5Cu8; 32];
    let attacker_pub = ed25519_dalek::SigningKey::from_bytes(&attacker_seed)
        .verifying_key()
        .to_bytes();
    let (nonce, proof) = pop_with(
        &app,
        &t_email,
        victim_pending,
        &attacker_pub,
        &attacker_seed,
    )
    .await;
    let resp = post(
        &app,
        "/auth/recover",
        json!({
            "email": t_email,
            "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES),
            "client_id": victim_pending,
            "ed25519_pub": B64.encode(attacker_pub),
            "label": "collide",
            "nonce": nonce,
            "proof": proof
        }),
    )
    .await;
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED
            || resp.status() == StatusCode::CONFLICT
            || resp.status().is_server_error(),
        "colliding client_id must not be armed (got {})",
        resp.status()
    );
    // V's row: still pending, no grant.
    let (ws,): (uuid::Uuid,) = sqlx::query_as("SELECT workspace_id FROM yapstack_lookup_login($1)")
        .bind(&v_email)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut tx = yapstack_server::db::begin_tenant_tx(&pool, ws)
        .await
        .unwrap();
    let (status, grant): (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT status, recovery_grant_at FROM devices WHERE workspace_id = $1 AND client_id = $2",
    )
    .bind(ws)
    .bind(victim_pending)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(status, "pending");
    assert!(grant.is_none(), "victim row must not be armed");
}

/// CRYPTO_SPEC §4.3 / A9: `PUT /auth/password` replaces the verifier (fresh server salt)
/// and the password wrap while RETAINING `salt_enc`; it requires the current auth key
/// unless the caller recovered with the code in the last 15 minutes; a pending device
/// is refused.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn change_password_requires_reauth_or_fresh_recovery_and_keeps_salt() {
    let st = setup().await;
    let pool = st.pool.clone();
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x27u8; 16];
    let old_auth = client_auth_key("old horse battery staple", &salt_enc);
    let new_auth = client_auth_key("new horse battery staple", &salt_enc);
    let first_client = uuid::Uuid::new_v4();
    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&email, &salt_enc, &old_auth, first_client),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let signup = body_json(resp).await;
    let active_access = signup["access_token"].as_str().unwrap().to_string();
    let first_refresh = signup["refresh_token"].as_str().unwrap().to_string();
    let new_wrap = B64.encode([0x02u8; 105]);
    let body = |current: Option<&str>| {
        json!({
            "current_auth_key": current,
            "auth_key": new_auth,
            "wrapped_vault_key_password": new_wrap
        })
    };

    // No current key, no recent recovery → 401. Wrong current key → 401.
    let resp = put_auth(&app, "/auth/password", &active_access, body(None)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let wrong = client_auth_key("wrong", &salt_enc);
    let resp = put_auth(&app, "/auth/password", &active_access, body(Some(&wrong))).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Malformed material → 400 (never brick password login with a truncated wrap).
    let resp = put_auth(
        &app,
        "/auth/password",
        &active_access,
        json!({
            "current_auth_key": old_auth,
            "auth_key": new_auth,
            "wrapped_vault_key_password": B64.encode([0u8; 20])
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A PENDING device (password login, unapproved) → 403 even with the right key.
    let (_p, pending_access) = enroll_pending(&app, &email, &old_auth).await;
    let resp = put_auth(
        &app,
        "/auth/password",
        &pending_access,
        body(Some(&old_auth)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // A second ACTIVE session holds a refresh family that the change will revoke.
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({ "email": email, "auth_key": old_auth }),
    )
    .await;
    let other_refresh = body_json(resp).await["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (old_server_salt,): (Vec<u8>,) = sqlx::query_as(
        "SELECT u.server_salt FROM yapstack_lookup_login($1) l JOIN users u ON u.id = l.user_id",
    )
    .bind(&email)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Correct current key → OK. Old login fails, new login works and serves the NEW wrap
    // under the SAME salt_enc.
    let resp = put_auth(
        &app,
        "/auth/password",
        &active_access,
        body(Some(&old_auth)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({ "email": email, "auth_key": old_auth }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "old password dead");
    let resp = post(&app, "/auth/login/begin", json!({ "email": email })).await;
    assert_eq!(
        body_json(resp).await["salt_enc"],
        json!(B64.encode(salt_enc)),
        "salt_enc retained (A9)"
    );
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({ "email": email, "auth_key": new_auth }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "new password works");
    assert_eq!(
        body_json(resp).await["wrapped_vault_key_password"],
        json!(new_wrap)
    );
    // server_salt rotated; the OTHER session's refresh family is revoked; the caller's
    // own family survives.
    let (new_server_salt,): (Vec<u8>,) = sqlx::query_as(
        "SELECT u.server_salt FROM yapstack_lookup_login($1) l JOIN users u ON u.id = l.user_id",
    )
    .bind(&email)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_ne!(old_server_salt, new_server_salt, "server_salt rotated");
    let resp = post(
        &app,
        "/auth/refresh",
        json!({ "refresh_token": other_refresh }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "other device's family revoked"
    );
    let resp = post(
        &app,
        "/auth/refresh",
        json!({ "refresh_token": first_refresh }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "caller's own family kept");

    // Recovery on a fresh device, then a change WITHOUT the current key.
    let rec_client = uuid::Uuid::new_v4();
    let (nonce, proof) = pop(&app, &email, rec_client).await;
    let resp = post(
        &app,
        "/auth/recover",
        json!({
            "email": email,
            "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES),
            "client_id": rec_client,
            "ed25519_pub": dev_pub_b64(rec_client),
            "label": "replacement",
            "nonce": nonce,
            "proof": proof
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rec_access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let newer_auth = client_auth_key("newer horse battery staple", &salt_enc);
    let body2 = json!({ "auth_key": newer_auth, "wrapped_vault_key_password": new_wrap });
    // Still PENDING (before the A7 roster re-bootstrap) → 403 even with the fresh waiver.
    let resp = put_auth(&app, "/auth/password", &rec_access, body2.clone()).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "pending recovering device"
    );
    // The recovering device re-bootstraps the roster (A7) and becomes active.
    let (roster, sig) = signed_roster(&[0x33u8; 32], 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &rec_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0,
            "active_devices": [first_client, rec_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // Fresh waiver + a WRONG current key → 401 (a presented key is always checked).
    let resp = put_auth(
        &app,
        "/auth/password",
        &rec_access,
        json!({
            "current_auth_key": wrong,
            "auth_key": newer_auth,
            "wrapped_vault_key_password": new_wrap
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // The waiver is PER DEVICE: the first device's active token cannot use it. (Its
    // access token is still valid — only refresh families were revoked.)
    let resp = put_auth(&app, "/auth/password", &active_access, body2.clone()).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "waiver is per device"
    );
    // An EXPIRED waiver (backdated) → 401; a fresh one → OK once; then single-use → 401.
    let (ws,): (uuid::Uuid,) = sqlx::query_as("SELECT workspace_id FROM yapstack_lookup_login($1)")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    async fn backdate(pool: &sqlx::PgPool, ws: uuid::Uuid, client: uuid::Uuid, when: &str) {
        let mut tx = yapstack_server::db::begin_tenant_tx(pool, ws)
            .await
            .unwrap();
        sqlx::query(&format!(
            "UPDATE devices SET recovered_at = now() - interval '{when}' \
             WHERE workspace_id = $1 AND client_id = $2"
        ))
        .bind(ws)
        .bind(client)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    backdate(&pool, ws, rec_client, "16 minutes").await;
    let resp = put_auth(&app, "/auth/password", &rec_access, body2.clone()).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "expired waiver");
    backdate(&pool, ws, rec_client, "1 minute").await;
    let resp = put_auth(&app, "/auth/password", &rec_access, body2.clone()).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "recent recovery waives the current key"
    );
    let resp = put_auth(&app, "/auth/password", &rec_access, body2).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "waiver is single-use"
    );
}

/// SYNC_SPEC §15.D device removal: an ACTIVE caller re-signs the roster without a member and
/// names it in `revoked_devices`. The relay marks it revoked, revokes its refresh family,
/// refuses its writes, and re-enrols it PENDING if it signs in again. A caller cannot
/// remove itself, a recovery-bootstrap caller cannot remove anyone, and an id cannot be
/// both revoked and active.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn device_removal_revokes_access_and_requires_reapproval() {
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x28u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let first_client = uuid::Uuid::new_v4();
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
    let vault_key = [0x33u8; 32];

    // Approve a second device the normal way.
    let (second, second_access) = enroll_pending(&app, &email, &auth_key).await;
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0,
            "active_devices": [first_client, second]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // Its refresh family (from a fresh login as that device).
    let (second_login_access, _) = login_finish_as(&app, &email, &auth_key, second).await;
    let _ = second_login_access;

    // Invalid removals: self → 403; revoked-and-active → 400.
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [second], "revoked_devices": [first_client]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "cannot remove self");
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [first_client, second], "revoked_devices": [second]
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "revoked and active at once"
    );

    // The removal (re-authenticated with the current auth key).
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [first_client], "revoked_devices": [second],
            "current_auth_key": auth_key
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Index shows it revoked; its writes are refused; reads still work on the live token.
    let resp = get_auth(&app, "/devices", &active_access).await;
    let devices = body_json(resp).await;
    let row = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_id"] == json!(second))
        .unwrap();
    assert_eq!(row["status"], "revoked");
    let resp = post_auth(
        &app,
        "/sync/push",
        &second_access,
        json!({ "changes": [{
        "client_id": second, "client_seq": 1, "ciphertext": B64.encode(b"x"),
        "schema_version": 1, "engine_version": 16003 }]}),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "removed device cannot write"
    );
    let resp = get_auth(&app, "/sync/pull?since=0", &second_access).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "reads run out with the access token"
    );
    // Its refresh family is dead: a fresh login as that device re-enrols it PENDING.
    let (_, _) = login_finish_as(&app, &email, &auth_key, second).await;
    let resp = get_auth(&app, "/devices", &active_access).await;
    let devices = body_json(resp).await;
    let row = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_id"] == json!(second))
        .unwrap();
    assert_eq!(
        row["status"], "pending",
        "re-login re-enrols a removed device as pending"
    );

    // A recovery-bootstrap caller may not remove anyone.
    let rec_client = uuid::Uuid::new_v4();
    let (nonce, proof) = pop(&app, &email, rec_client).await;
    let resp = post(
        &app,
        "/auth/recover",
        json!({
            "email": email, "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES),
            "client_id": rec_client, "ed25519_pub": dev_pub_b64(rec_client), "label": "rec",
            "nonce": nonce, "proof": proof
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rec_access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (roster, sig) = signed_roster(&vault_key, 3, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &rec_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 3, "vault_key_epoch": 0,
            "active_devices": [rec_client], "revoked_devices": [first_client]
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "recovery bootstrap cannot remove"
    );
}

/// Device-removal edges the review asked for: a removed device's refresh family really
/// is dead; removal needs the caller's current auth key (401 without / wrong); a removed
/// device that recovers with the code is re-enrolled pending WITHOUT a grant (its
/// self-admit is refused); a pending caller naming `revoked_devices` is refused; and the
/// snapshot write route is gated like every other device-authored write.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn device_removal_reauth_and_revoked_row_edges() {
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x29u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let first_client = uuid::Uuid::new_v4();
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
    let vault_key = [0x33u8; 32];

    // Approve a second device and capture ITS refresh token.
    let (second, _) = enroll_pending(&app, &email, &auth_key).await;
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 1, "vault_key_epoch": 0,
            "active_devices": [first_client, second]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (nonce, proof) = pop(&app, &email, second).await;
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email, "auth_key": auth_key, "client_id": second,
            "ed25519_pub": dev_pub_b64(second), "label": "device",
            "nonce": nonce, "proof": proof
        }),
    )
    .await;
    let second_tokens = body_json(resp).await;
    let second_refresh = second_tokens["refresh_token"].as_str().unwrap().to_string();
    let second_access = second_tokens["access_token"].as_str().unwrap().to_string();

    // A pending caller naming revoked_devices → 403 (before any re-auth question).
    let (_third, third_access) = enroll_pending(&app, &email, &auth_key).await;
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &third_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [first_client], "revoked_devices": [second],
            "current_auth_key": auth_key
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Removal without the current key → 401; with a wrong key → 401; with it → OK.
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [first_client], "revoked_devices": [second]
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "removal needs re-auth"
    );
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [first_client], "revoked_devices": [second],
            "current_auth_key": client_auth_key("wrong", &salt_enc)
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "wrong key");
    let (roster, sig) = signed_roster(&vault_key, 2, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &active_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 2, "vault_key_epoch": 0,
            "active_devices": [first_client], "revoked_devices": [second],
            "current_auth_key": auth_key
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The removed device's refresh family is dead, and its snapshot write is refused.
    let resp = post(
        &app,
        "/auth/refresh",
        json!({ "refresh_token": second_refresh }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "family revoked");
    let resp = post_auth(
        &app,
        "/snapshot/presign",
        &second_access,
        json!({ "sha256": "00".repeat(32), "size": 10, "generation": 1, "baseline_seq": 0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "snapshot write gated");

    // The removed device recovers with the code: re-enrolled PENDING, but NO grant — its
    // self-admission is refused; it needs approval like any new device.
    let (nonce, proof) = pop(&app, &email, second).await;
    let resp = post(
        &app,
        "/auth/recover",
        json!({
            "email": email,
            "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES),
            "client_id": second,
            "ed25519_pub": dev_pub_b64(second),
            "label": "device",
            "nonce": nonce,
            "proof": proof
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rec_access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = get_auth(&app, "/devices", &active_access).await;
    let devices = body_json(resp).await;
    let row = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_id"] == json!(second))
        .unwrap();
    assert_eq!(row["status"], "pending", "re-enrolled pending");
    let (roster, sig) = signed_roster(&vault_key, 3, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &rec_access,
        json!({
            "device_list": roster, "signature": sig, "counter": 3, "vault_key_epoch": 0,
            "active_devices": [first_client, second]
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a removed device gets no recovery grant"
    );
}

/// CRYPTO_SPEC A10: knowing the password is no longer enough to act as another device.
/// A login that names an existing `client_id` must prove possession of THAT device's key;
/// a request with no proof binds no device (read-only); a nonce is single-use and bound to
/// its email.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn password_holder_cannot_impersonate_another_device() {
    let st = setup().await;
    let pool = st.pool.clone();
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x2Au8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let first_client = uuid::Uuid::new_v4();
    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&email, &salt_enc, &auth_key, first_client),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // An honest device enrols (pending) with its proof, establishing its stored key.
    let (honest, _) = enroll_pending(&app, &email, &auth_key).await;

    // Impersonation: the password holder presents `honest`'s client_id with a key it
    // controls and a valid signature by THAT key → the relay checks against the STORED key
    // → 401. Same with the signup device's id.
    let evil_seed = [0x5Du8; 32];
    let evil_pub = ed25519_dalek::SigningKey::from_bytes(&evil_seed)
        .verifying_key()
        .to_bytes();
    for target in [honest, first_client] {
        let (nonce, proof) = pop_with(&app, &email, target, &evil_pub, &evil_seed).await;
        let resp = post(
            &app,
            "/auth/login/finish",
            json!({
                "email": email, "auth_key": auth_key, "client_id": target,
                "ed25519_pub": B64.encode(evil_pub), "label": "evil",
                "nonce": nonce, "proof": proof
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "stored key wins");
    }

    // No proof at all (older client): 200, but the token binds no device — writes refused.
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email, "auth_key": auth_key, "client_id": first_client,
            "ed25519_pub": dev_pub_b64(first_client), "label": "old-client"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bare = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = post_auth(
        &app,
        "/sync/push",
        &bare,
        json!({ "changes": [{
        "client_id": first_client, "client_seq": 1, "ciphertext": B64.encode(b"x"),
        "schema_version": 1, "engine_version": 16003 }]}),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "device-less token cannot write"
    );

    // A nonce is single-use, and bound to its email.
    let (nonce, proof) = pop(&app, &email, honest).await;
    let body = json!({
        "email": email, "auth_key": auth_key, "client_id": honest,
        "ed25519_pub": dev_pub_b64(honest), "label": "device",
        "nonce": nonce, "proof": proof
    });
    let resp = post(&app, "/auth/login/finish", body.clone()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = post(&app, "/auth/login/finish", body).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "nonce replay");
    let other_email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let resp = post(&app, "/auth/login/begin", json!({ "email": other_email })).await;
    let foreign_nonce = body_json(resp).await["nonce"].as_str().unwrap().to_string();
    let msg = yapstack_crypto::sign::login_pop_message(
        &B64.decode(&foreign_nonce).unwrap(),
        honest.as_bytes(),
        &dev_pub(honest),
        &email.to_lowercase(),
    );
    let sig = yapstack_crypto::sign::sign_login_pop(&dev_seed(honest), &msg);
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email, "auth_key": auth_key, "client_id": honest,
            "ed25519_pub": dev_pub_b64(honest), "label": "device",
            "nonce": foreign_nonce, "proof": B64.encode(sig)
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "nonce bound to its email"
    );

    // Same on the RECOVERY path: presenting an existing device's id with a controlled key
    // and the recovery code fails against the stored key (the grant is not armed).
    let (nonce, proof) = pop_with(&app, &email, honest, &evil_pub, &evil_seed).await;
    let resp = post(
        &app,
        "/auth/recover",
        json!({
            "email": email, "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES),
            "client_id": honest, "ed25519_pub": B64.encode(evil_pub), "label": "evil",
            "nonce": nonce, "proof": proof
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "recover: stored key wins"
    );

    // A failed proof BURNS the nonce: the honest device cannot reuse it afterwards.
    let (nonce, _bad) = pop(&app, &email, honest).await;
    let junk = B64.encode([0u8; 64]);
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email, "auth_key": auth_key, "client_id": honest,
            "ed25519_pub": dev_pub_b64(honest), "label": "device",
            "nonce": nonce, "proof": junk
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let nonce_bytes = B64.decode(&nonce).unwrap();
    let msg = yapstack_crypto::sign::login_pop_message(
        &nonce_bytes,
        honest.as_bytes(),
        &dev_pub(honest),
        &email.to_lowercase(),
    );
    let good = B64.encode(yapstack_crypto::sign::sign_login_pop(
        &dev_seed(honest),
        &msg,
    ));
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email, "auth_key": auth_key, "client_id": honest,
            "ed25519_pub": dev_pub_b64(honest), "label": "device",
            "nonce": nonce, "proof": good
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "burned nonce");

    // An EXPIRED nonce (backdated) is refused.
    let (nonce, proof) = pop(&app, &email, honest).await;
    sqlx::query(
        "UPDATE login_nonces SET issued_at = now() - interval '6 minutes' WHERE nonce = $1",
    )
    .bind(B64.decode(&nonce).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({
            "email": email, "auth_key": auth_key, "client_id": honest,
            "ed25519_pub": dev_pub_b64(honest), "label": "device",
            "nonce": nonce, "proof": proof
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "expired nonce");
}

// =====================================================================================
// PROVING TESTS for cluster `roster-approval` (SYNC_REVIEW_FINDINGS.md):
//   #1  "Approval uses the cached roster counter, permanently 409-locking devices out"
//   #2  "Signed roster membership is authored from the relay's unverified device index"
// These are INVESTIGATOR proving tests — do NOT ship as the fix. They pin the current
// buggy mechanism against a live relay so the remediation has a red/green harness.
// =====================================================================================

/// Enroll a NEW device as PENDING via login/finish; return (client_id, access_token).
/// Faithful to the client bootstrap: a fresh device presents its own client_id +
/// Ed25519 pub and is enrolled pending (not auto-promoted).
async fn enroll_pending(app: &axum::Router, email: &str, auth_key: &str) -> (uuid::Uuid, String) {
    let client_id = uuid::Uuid::new_v4();
    let access = login_finish_as(app, email, auth_key, client_id).await.0;
    (client_id, access)
}

/// login/finish for an EXPLICIT client_id. Enrolls it pending on first call; on a later
/// call for an already-active device it is a no-op promotion (ON CONFLICT DO NOTHING)
/// that still returns fresh tokens + the served roster — the ONLY way a client
/// re-anchors its counter cache (`verify_and_inspect_roster` reads the served counter).
/// Returns (access_token, served_roster_counter).
async fn login_finish_as(
    app: &axum::Router,
    email: &str,
    auth_key: &str,
    client_id: uuid::Uuid,
) -> (String, i64) {
    let pub_b64 = dev_pub_b64(client_id);
    let (nonce, proof) = pop(app, email, client_id).await;
    let resp = post(
        app,
        "/auth/login/finish",
        json!({
            "email": email,
            "auth_key": auth_key,
            "client_id": client_id,
            "ed25519_pub": pub_b64,
            "label": "device",
            "nonce": nonce,
            "proof": proof
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let access = body["access_token"].as_str().unwrap().to_string();
    // The served roster's counter is the value the client would re-anchor its cache to.
    let counter = body["device_list"]["counter"].as_i64().unwrap_or(0);
    (access, counter)
}

/// FINDING #1 — the cached-counter approval deadlock, end to end against a live relay.
///
/// This test models the client's `sync_approve_device` counter algorithm EXACTLY
/// (apps/desktop/src-tauri/src/sync.rs:5550 `new_counter = session.roster_counter + 1`,
/// write-back only on success at :5579; the cache is re-anchored ONLY by
/// signup/login/recover/successful-approve — never by `sync_status`/`GET /devices`,
/// which carry no counter). A device whose cache lags by one — a perfectly ordinary
/// state the moment ANOTHER active device performs an approval — computes `stale + 1`,
/// which equals the stored counter, and `put_roster` returns 409. The write-back never
/// runs, so the cache stays stale and EVERY retry sends the same value: a permanent lock.
///
/// NOTE ON RED/GREEN: the 409 here is the relay behaving CORRECTLY (anti-rollback is by
/// design, pinned by `roster_counter_anti_rollback_rejects_stale_or_equal`). The defect
/// is client-side (stale cache + no re-anchor). So this test DEMONSTRATES the lockout
/// mechanism and STAYS GREEN; the genuinely-red gate for the fix is the companion test
/// `finding1_no_get_roster_route_to_reanchor_counter` below.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn finding1_cached_counter_permanently_409_locks_lagging_approver() {
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let vault_key = [0x33u8; 32];

    // D1 signs up: server counter = 0, D1's cached counter = 0.
    let d1 = uuid::Uuid::new_v4();
    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&email, &salt_enc, &auth_key, d1),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let d1_access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut d1_cache: i64 = 0;

    // D1 approves D2 -> server counter 1. D1 write-back advances its cache to 1.
    // D2 enrolled pending at counter 0 (its login cached 0, before this approval).
    let (d2, _d2_enroll_access) = enroll_pending(&app, &email, &auth_key).await;
    let (roster, sig) = signed_roster(&vault_key, d1_cache + 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &d1_access,
        json!({ "device_list": roster, "signature": sig, "counter": d1_cache + 1,
                "vault_key_epoch": 0, "active_devices": [d1, d2] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    d1_cache = body_json(resp).await["counter"].as_i64().unwrap(); // -> 1

    // D2 is now ACTIVE. To become a working approver it re-anchors its counter cache the
    // only way a client can — a full login (verify_and_inspect_roster reads the served
    // roster's counter). Same client_id d2, now active; the login returns counter = 1.
    // This IS the sign-out/sign-in escape hatch the finding names (and the one that wipes
    // the TOFU baselines in the sibling crypto finding).
    let (d2_access, d2_served_counter) = login_finish_as(&app, &email, &auth_key, d2).await;
    let mut d2_cache: i64 = d2_served_counter;
    assert_eq!(
        d2_cache, 1,
        "D2 re-anchored to the current counter via login"
    );

    // D2 approves D3 -> server counter 2. D1's cache is STILL 1 (nothing told it).
    let (d3, _d3_access) = enroll_pending(&app, &email, &auth_key).await;
    let (roster, sig) = signed_roster(&vault_key, d2_cache + 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &d2_access,
        json!({ "device_list": roster, "signature": sig, "counter": d2_cache + 1,
                "vault_key_epoch": 0, "active_devices": [d2, d3] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    d2_cache = body_json(resp).await["counter"].as_i64().unwrap(); // -> 2
    assert_eq!(d2_cache, 2);

    // Now D1 tries to approve D4 with its stale cache (1): it sends counter = 1 + 1 = 2,
    // which is NOT strictly greater than the stored 2 -> 409.
    let (d4, _d4_access) = enroll_pending(&app, &email, &auth_key).await;
    let (roster, sig) = signed_roster(&vault_key, d1_cache + 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &d1_access,
        json!({ "device_list": roster, "signature": sig, "counter": d1_cache + 1,
                "vault_key_epoch": 0, "active_devices": [d1, d4] }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "lagging approver sending stale+1 is rejected by anti-rollback"
    );
    // The client write-back (sync.rs:5579) only runs on a 2xx; the cache stays 1.
    // d1_cache is UNCHANGED here, modelling that the stale value is never advanced.
    assert_eq!(d1_cache, 1, "stale cache is never advanced after a 409");

    // Retry: the client recomputes the SAME stale+1 (there is no 409 handling in
    // sync.rs — grep 409/Conflict yields nothing) -> 409 again. Permanent lock.
    let (roster, sig) = signed_roster(&vault_key, d1_cache + 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &d1_access,
        json!({ "device_list": roster, "signature": sig, "counter": d1_cache + 1,
                "vault_key_epoch": 0, "active_devices": [d1, d4] }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "the retry re-sends the same stale counter and is locked out permanently"
    );
}

/// FINDING #1 (RED GATE) — the relay exposes NO route to re-anchor the roster counter,
/// which is why the lockout above is permanent. The finding's FIX is to "expose the
/// current roster (counter + device_list + signature) on a GET" so the client can
/// re-anchor before signing. This test asserts that endpoint exists and returns the
/// current counter. It FAILS on the reviewed tree (no such route: `/devices/roster`
/// has only a PUT handler, lib.rs:67) and PASSES once the re-anchor GET is added.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn finding1_no_get_roster_route_to_reanchor_counter() {
    let st = setup().await;
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);
    let d1 = uuid::Uuid::new_v4();

    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&email, &salt_enc, &auth_key, d1),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let access = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Advance the stored counter to 1 so a re-anchor GET would have something to report.
    let vault_key = [0x33u8; 32];
    let (roster, sig) = signed_roster(&vault_key, 1, 0);
    let resp = put_auth(
        &app,
        "/devices/roster",
        &access,
        json!({ "device_list": roster, "signature": sig, "counter": 1,
                "vault_key_epoch": 0, "active_devices": [] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The FIX must let a client learn the authoritative counter without a full re-login.
    let resp = get_auth(&app, "/devices/roster", &access).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a GET /devices/roster re-anchor route must exist (finding #1 fix); \
         on the reviewed tree there is none, so this is the RED gate"
    );
    let body = body_json(resp).await;
    assert_eq!(
        body["counter"].as_i64(),
        Some(1),
        "the re-anchor route must report the current authoritative counter"
    );
}

/// FINDING #2 — the ONLY membership source `sync_approve_device` consults is
/// `GET /devices` (sync.rs:5517), and it folds EVERY row whose relay-supplied
/// `status == "active"` into the vault-signed roster (sync.rs:5536-5548, :5574),
/// never consulting the previously-verified roster (`verify_and_inspect_roster`,
/// sync.rs:4069-4102, extracts self-membership ONLY). This test proves the crux:
/// that `status` field is a RELAY-CONTROLLED, cryptographically-UNBOUND value — a
/// compromised relay can flip a pending attacker device to "active" with a plain
/// UPDATE, and `GET /devices` reports it verbatim. An honest approver folding on that
/// value would therefore sign a membership attestation the vault-key holder never
/// intended. (§0 threat model: the relay is honest-but-curious, potentially compromised.)
///
/// The client-side FOLDING itself is inline in the async command and not unit-reachable,
/// so this is a STRUCTURAL demonstration of the untrusted input + a code citation; it
/// stays green (it pins current relay behavior). The fix's red gate belongs on the
/// client: build the roster from the last VERIFIED roster, using `GET /devices` only to
/// discover pending candidates.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn finding2_get_devices_status_is_relay_mutable_unsigned_membership_source() {
    let st = setup().await;
    let pool = st.pool.clone();
    let app = build_router(st);
    let email = format!("kat-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);

    // Owner's active device.
    let d1 = uuid::Uuid::new_v4();
    let resp = post(
        &app,
        "/auth/signup",
        signup_body_with_client(&email, &salt_enc, &auth_key, d1),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let signup = body_json(resp).await;
    let d1_access = signup["access_token"].as_str().unwrap().to_string();
    let tenant: uuid::Uuid = signup["tenant_id"].as_str().unwrap().parse().unwrap();

    // An attacker who holds the password (the §7.5 adversary) enrolls a device -> PENDING.
    let (attacker, _attacker_access) = enroll_pending(&app, &email, &auth_key).await;

    // Sanity: the relay reports the attacker as pending.
    let resp = get_auth(&app, "/devices", &d1_access).await;
    let list = body_json(resp).await;
    let att = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_id"] == json!(attacker.to_string()))
        .expect("attacker device present")
        .clone();
    assert_eq!(att["status"], json!("pending"));

    // COMPROMISED RELAY: flip the attacker's advisory status to "active" with a plain
    // UPDATE. `devices.status` carries NO signature and NO vault-key binding — it is pure
    // relay-owned metadata, exactly the value the client trusts as its membership source.
    let mut tx = yapstack_server::db::begin_tenant_tx(&pool, tenant)
        .await
        .unwrap();
    let rows = sqlx::query(
        "UPDATE devices SET status = 'active' WHERE workspace_id = $1 AND client_id = $2",
    )
    .bind(tenant)
    .bind(attacker)
    .execute(&mut *tx)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(
        rows, 1,
        "relay flipped one row pending->active with no crypto"
    );
    tx.commit().await.unwrap();

    // The honest approver's SOLE membership source now reports the attacker as active.
    // sync_approve_device (sync.rs:5536-5538) includes precisely these rows in the
    // vault-signed roster + `active_devices`, with no cross-check against the last
    // verified roster and no human review (the out-of-band fingerprint check covers only
    // the single `find`-matched pending TARGET, sync.rs:5520-5531).
    let resp = get_auth(&app, "/devices", &d1_access).await;
    let list = body_json(resp).await;
    let att = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_id"] == json!(attacker.to_string()))
        .expect("attacker device present")
        .clone();
    assert_eq!(
        att["status"],
        json!("active"),
        "GET /devices — the client's only membership source — reports relay-chosen, \
         cryptographically-unbound membership; an honest approver folds it into a \
         vault-signed roster (finding #2)"
    );
}
