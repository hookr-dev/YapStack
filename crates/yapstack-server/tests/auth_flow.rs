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

fn signup_body(email: &str, salt_enc: &[u8], auth_key_b64: &str) -> Value {
    // Placeholder committing-envelope blobs (the server stores them opaquely).
    let wrapped = B64.encode([0x01u8; 73]);
    let vault_key = [0x33u8; 32];
    let client_id = uuid::Uuid::new_v4();
    let sk = yapstack_crypto::sign::roster_signing_key(&vault_key);
    let roster = json!({ "version": 1, "counter": 0, "vault_key_epoch": 0 });
    let sig = yapstack_crypto::sign::sign_roster(&vault_key, roster.to_string().as_bytes());
    json!({
        "email": email,
        "auth_key": auth_key_b64,
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
