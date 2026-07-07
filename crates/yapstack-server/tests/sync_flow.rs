// SPDX-License-Identifier: AGPL-3.0-only
//! Gate 4 (T008) integration tests against a LIVE Postgres. `#[ignore]`-gated because
//! the CI-check environment has no Postgres (sqlx runtime queries, no compile-time
//! `DATABASE_URL`). Run locally:
//!
//! ```text
//! createdb yapstack_test
//! DATABASE_URL=postgres://localhost/yapstack_test cargo test -p yapstack-server -- --ignored
//! ```
//!
//! T009 review targets exercised here: commit-ordered `changeset_seq` monotonicity +
//! gap-freeness UNDER CONCURRENCY, push idempotency, completeness math + per-client
//! tails (A3), the choke-point 429, admin Ed25519 verify + replay rejection, and audio
//! ciphertext-hash dedup.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;
use yapstack_server::{build_router, AppState, Config};

// --------------------------------------------------------------------- harness

fn admin_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

async fn setup(with_admin: bool) -> AppState {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for --ignored tests");
    let pool = yapstack_server::db::connect(&url).await.unwrap();
    yapstack_server::db::migrate(&pool).await.unwrap();
    let mut toml = format!(
        "database_url = \"{url}\"\njwt_secret = \"test-secret\"\nserver_pepper = \"test-pepper\"\n"
    );
    if with_admin {
        let pk = B64.encode(admin_signing_key().verifying_key().to_bytes());
        toml.push_str(&format!(
            "\n[limits]\nadmin_public_key = \"ed25519:{pk}\"\nhelp_url = \"https://example.test/upgrade\"\n"
        ));
    }
    let cfg = Config::from_toml_str(&toml).unwrap();
    AppState::new(pool, cfg)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
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
        // Required by SignupRequest since T010d (recovery-code auth path). This suite
        // never exercises /auth/recover, so any well-formed opaque value suffices; the
        // server only ever second-hashes it. Matches the auth_flow.rs fixture shape.
        "recovery_auth_key": B64.encode([0x5au8; 20]),
        "wrapped_vault_key_password": wrapped, "wrapped_vault_key_recovery": wrapped,
        "device_list": {
            "device_list": roster, "signature": B64.encode(sig),
            "counter": 0, "vault_key_epoch": 0, "client_id": client_id,
            "ed25519_pub": B64.encode(sk.verifying_key().to_bytes()), "label": "d"
        }
    })
}

/// Signup and return `(access_token, tenant_id)`.
async fn signup(app: &axum::Router) -> (String, Uuid) {
    let email = format!("t-{}@example.com", Uuid::new_v4());
    let resp = post(app, "/auth/signup", None, signup_body(&email)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    (
        v["access_token"].as_str().unwrap().to_string(),
        v["tenant_id"].as_str().unwrap().parse().unwrap(),
    )
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

fn change(client_id: Uuid, client_seq: i64, payload: &[u8]) -> Value {
    json!({
        "client_id": client_id, "client_seq": client_seq,
        "ciphertext": B64.encode(payload), "schema_version": 22, "engine_version": 16003
    })
}

// --------------------------------------------------------------------- tests

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn push_assigns_dense_seqs_and_is_idempotent() {
    let app = build_router(setup(false).await);
    let (tok, _tenant) = signup(&app).await;
    let dev = Uuid::new_v4();

    let batch = json!({ "changes": [change(dev, 1, b"a"), change(dev, 2, b"bb")] });
    let resp = post(&app, "/sync/push", Some(&tok), batch.clone()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["acks"][0]["changeset_seq"], 1);
    assert_eq!(v["acks"][1]["changeset_seq"], 2);
    assert_eq!(v["max_changeset_seq"], 2);

    // Re-push the SAME (client_id, client_seq) pairs → original seqs, deduplicated.
    let resp = post(&app, "/sync/push", Some(&tok), batch).await;
    let v = body_json(resp).await;
    assert_eq!(v["acks"][0]["changeset_seq"], 1);
    assert_eq!(v["acks"][0]["deduplicated"], true);
    assert_eq!(
        v["max_changeset_seq"], 2,
        "no new seqs consumed on a dup re-push"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn concurrent_pushes_are_gap_free_and_monotonic() {
    let app = build_router(setup(false).await);
    let (tok, _tenant) = signup(&app).await;

    // 20 devices each push one change concurrently.
    let mut handles = Vec::new();
    for i in 0..20u32 {
        let app = app.clone();
        let tok = tok.clone();
        handles.push(tokio::spawn(async move {
            let dev = Uuid::new_v4();
            let batch = json!({ "changes": [change(dev, 1, &i.to_le_bytes())] });
            let resp = post(&app, "/sync/push", Some(&tok), batch).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let resp = get(&app, "/sync/completeness", &tok).await;
    let v = body_json(resp).await;
    assert_eq!(v["max_changeset_seq"], 20);
    assert_eq!(v["count"], 20);
    assert_eq!(v["contiguous"], true, "dense, gap-free under concurrency");
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn completeness_reports_contiguous_and_client_tails() {
    let app = build_router(setup(false).await);
    let (tok, _t) = signup(&app).await;
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    post(
        &app,
        "/sync/push",
        Some(&tok),
        json!({ "changes": [change(a, 1, b"x"), change(a, 2, b"y")] }),
    )
    .await;
    post(
        &app,
        "/sync/push",
        Some(&tok),
        json!({ "changes": [change(b, 1, b"z")] }),
    )
    .await;

    let v = body_json(get(&app, "/sync/completeness", &tok).await).await;
    assert_eq!(v["max_changeset_seq"], 3);
    assert_eq!(v["contiguous"], true);
    // A3: per-client tails let a device detect a silently-dropped stream tail.
    let tails = v["per_client"].as_array().unwrap();
    let tail_a = tails
        .iter()
        .find(|t| t["client_id"] == a.to_string())
        .unwrap();
    assert_eq!(tail_a["max_client_seq"], 2);

    // Pull returns dense ciphertext in commit order.
    let v = body_json(get(&app, "/sync/pull?since=0&limit=10", &tok).await).await;
    assert_eq!(v["changes"].as_array().unwrap().len(), 3);
    assert_eq!(v["changes"][0]["changeset_seq"], 1);
    assert_eq!(v["has_more"], false);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn choke_point_returns_429_quota_exceeded() {
    let app = build_router(setup(true).await);
    let (tok, tenant) = signup(&app).await;

    // Push a tiny upload cap via the signed admin endpoint.
    admin_put_limits(
        &app,
        tenant,
        json!({
            "plan": "test",
            "storage_bytes": { "limit": 0, "unlimited": true },
            "upload_bytes_period": { "limit": 4, "unlimited": false },
            "share_count": { "limit": 0, "unlimited": true },
            "device_count": { "limit": 0, "unlimited": true },
            "state": "active"
        }),
    )
    .await;

    // 8 bytes > 4-byte cap → 429 with the published quota_exceeded shape.
    let resp = post(
        &app,
        "/sync/push",
        Some(&tok),
        json!({ "changes": [change(Uuid::new_v4(), 1, b"12345678")] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let v = body_json(resp).await;
    assert_eq!(v["code"], "quota_exceeded");
    assert_eq!(v["resource"], "upload_bytes_period");
    assert_eq!(v["limit"], 4);
    assert_eq!(v["help_url"], "https://example.test/upgrade");
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn admin_signature_and_replay_are_enforced() {
    let app = build_router(setup(true).await);
    let (_tok, tenant) = signup(&app).await;
    let body = json!({
        "plan": "pro",
        "storage_bytes": { "limit": 1000, "unlimited": false },
        "upload_bytes_period": { "limit": 1000, "unlimited": false },
        "share_count": { "limit": 5, "unlimited": false },
        "device_count": { "limit": 5, "unlimited": false },
        "state": "active"
    });

    // A bad signature is rejected.
    let path = format!("/admin/v1/tenants/{tenant}/limits");
    let resp = admin_request(
        &app,
        "PUT",
        &path,
        &body.to_string(),
        Some("bad-nonce"),
        Some(&[0u8; 64]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // A valid signature is accepted...
    let resp = admin_put_limits(&app, tenant, body.clone()).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // ...and StoredLimits now reads what admin wrote (usage GET works, signed).
    let usage_path = format!("/admin/v1/tenants/{tenant}/usage");
    let resp = admin_request(&app, "GET", &usage_path, "", None, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn admin_nonce_replay_is_rejected() {
    let app = build_router(setup(true).await);
    let (_tok, tenant) = signup(&app).await;
    let body = json!({
        "plan": "pro",
        "storage_bytes": { "limit": 1, "unlimited": true },
        "upload_bytes_period": { "limit": 1, "unlimited": true },
        "share_count": { "limit": 1, "unlimited": true },
        "device_count": { "limit": 1, "unlimited": true },
        "state": "active"
    })
    .to_string();
    let path = format!("/admin/v1/tenants/{tenant}/limits");
    let nonce = format!("nonce-{}", Uuid::new_v4());

    let (ts, sig) = sign_admin("PUT", &path, &body, &nonce);
    let first = admin_request_raw(&app, "PUT", &path, &body, &nonce, ts, &sig).await;
    assert_eq!(first.status(), StatusCode::OK);
    // Exact same signed request replayed → rejected on the single-use nonce.
    let replay = admin_request_raw(&app, "PUT", &path, &body, &nonce, ts, &sig).await;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn audio_presign_dedups_on_ciphertext_hash() {
    // Storage configured so presign returns a URL. Point at a dummy MinIO.
    let base = setup(false).await;
    // Re-load config WITH storage. Simplest: build a fresh state with a storage section.
    let url = std::env::var("DATABASE_URL").unwrap();
    let cfg = Config::from_toml_str(&format!(
        "database_url = \"{url}\"\njwt_secret = \"s\"\nserver_pepper = \"p\"\n\n\
         [storage]\nendpoint = \"http://minio:9000\"\nregion = \"us-east-1\"\n\
         bucket = \"yap\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\n"
    ))
    .unwrap();
    let app = build_router(AppState::new(base.pool.clone(), cfg));
    let (tok, _t) = signup(&app).await;

    let sha = "a".repeat(64);
    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();

    // First presign for a novel hash → a real upload URL, not a dedup.
    let v = body_json(
        post(
            &app,
            &format!("/audio/presign?sha256={sha}&size=1024&session_id={s1}"),
            Some(&tok),
            json!({}),
        )
        .await,
    )
    .await;
    assert_eq!(v["already_exists"], false);
    assert!(v["upload_url"].is_string());

    // Second session, SAME ciphertext hash → dedup hit, no upload URL.
    let v = body_json(
        post(
            &app,
            &format!("/audio/presign?sha256={sha}&size=1024&session_id={s2}"),
            Some(&tok),
            json!({}),
        )
        .await,
    )
    .await;
    assert_eq!(v["already_exists"], true);
    assert!(v["upload_url"].is_null());
}

// ----------------------------------------------------------------- admin helpers

fn sign_admin(method: &str, path: &str, body: &str, nonce: &str) -> (i64, [u8; 64]) {
    let ts = chrono::Utc::now().timestamp();
    let body_hash = hex::encode(Sha256::digest(body.as_bytes()));
    let msg = format!("{ts}\n{nonce}\n{method}\n{path}\n{body_hash}");
    let sig = admin_signing_key().sign(msg.as_bytes()).to_bytes();
    (ts, sig)
}

async fn admin_put_limits(
    app: &axum::Router,
    tenant: Uuid,
    body: Value,
) -> axum::response::Response {
    let path = format!("/admin/v1/tenants/{tenant}/limits");
    admin_request(app, "PUT", &path, &body.to_string(), None, None).await
}

/// Sign correctly unless an explicit override nonce/sig is supplied (for negative tests).
async fn admin_request(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: &str,
    override_nonce: Option<&str>,
    override_sig: Option<&[u8; 64]>,
) -> axum::response::Response {
    let nonce = override_nonce
        .map(str::to_string)
        .unwrap_or_else(|| format!("nonce-{}", Uuid::new_v4()));
    let (ts, good_sig) = sign_admin(method, path, body, &nonce);
    let sig = override_sig.copied().unwrap_or(good_sig);
    admin_request_raw(app, method, path, body, &nonce, ts, &sig).await
}

async fn admin_request_raw(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: &str,
    nonce: &str,
    ts: i64,
    sig: &[u8; 64],
) -> axum::response::Response {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-yapstack-timestamp", ts.to_string())
        .header("x-yapstack-nonce", nonce)
        .header("x-yapstack-signature", B64.encode(sig))
        .body(Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}
