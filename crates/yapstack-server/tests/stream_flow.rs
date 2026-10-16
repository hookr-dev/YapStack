// SPDX-License-Identifier: AGPL-3.0-only
//! `GET /sync/stream` SSE wakeup integration tests against a LIVE Postgres + a real bound
//! server socket. `#[ignore]`-gated like the sibling suites (the CI-check environment has no
//! Postgres). Run locally:
//!
//! ```text
//! createdb yapstack_test
//! DATABASE_URL=postgres://localhost/yapstack_test cargo test -p yapstack-server \
//!     --test stream_flow -- --ignored
//! ```
//!
//! The SSE stream had ZERO client callers until the desktop SSE lane landed; these tests are
//! the server-side proof of its two load-bearing guarantees:
//!   1. AUTH — the stream requires the same bearer as `/sync/pull` (no bearer → 401);
//!   2. TENANT SCOPING — a push as tenant A wakes ONLY tenant A's subscriber, never B's.
//!
//! The wake is exercised end-to-end: two subscribers on real sockets, a push through a clone
//! of the SAME router (so the shared `SseHub` in `AppState` fans out), and a raw read of the
//! `data: <seq>` line off each socket.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;
use uuid::Uuid;
use yapstack_server::{build_router, AppState, Config};

// --------------------------------------------------------------------- harness

async fn setup() -> AppState {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for --ignored tests");
    let pool = yapstack_server::db::connect(&url).await.unwrap();
    yapstack_server::db::migrate(&pool).await.unwrap();
    let toml = format!(
        "database_url = \"{url}\"\njwt_secret = \"test-secret\"\nserver_pepper = \"test-pepper\"\n"
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

/// Signup and return `(access_token, tenant_id, client_id)`.
async fn signup(app: &axum::Router) -> (String, Uuid, Uuid) {
    let email = format!("t-{}@example.com", Uuid::new_v4());
    let body = signup_body(&email);
    let client_id: Uuid = body["device_list"]["client_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/signup")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    (
        v["access_token"].as_str().unwrap().to_string(),
        v["tenant_id"].as_str().unwrap().parse().unwrap(),
        client_id,
    )
}

/// A minimal push request body: one changeset authored by `client_id` at `client_seq`.
fn push_body(client_id: Uuid, client_seq: i64) -> Value {
    json!({
        "changes": [{
            "client_id": client_id,
            "client_seq": client_seq,
            "ciphertext": B64.encode([0xAB, 0xCD, 0xEF]),
            "schema_version": 1,
            "engine_version": 1,
        }]
    })
}

/// Open a raw SSE subscription to `GET /sync/stream` on `addr` with `bearer`, returning the
/// live socket after the response headers have been read (status asserted 200).
async fn subscribe(addr: std::net::SocketAddr, bearer: &str) -> TcpStream {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /sync/stream HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {bearer}\r\n\r\n"
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    sock.flush().await.unwrap();
    // Read until end-of-headers so we don't consume event bytes; assert a 200 status line.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk))
            .await
            .expect("timed out reading SSE response headers")
            .unwrap();
        assert!(n > 0, "stream closed before headers");
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "stream must open 200 for an authed subscriber, got: {}",
        head.lines().next().unwrap_or("")
    );
    sock
}

/// Read from `sock` until a `data: <seq>` SSE line arrives (returns the seq) or `timeout`
/// elapses (returns `None`). Keep-alive comments (`:`-prefixed) and the `event:` field are
/// skipped — exactly the desktop parser's contract.
async fn read_wake(sock: &mut TcpStream, timeout: Duration) -> Option<i64> {
    let mut acc = Vec::new();
    let mut chunk = [0u8; 512];
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let n = match tokio::time::timeout(remaining, sock.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return None, // timeout or EOF
        };
        acc.extend_from_slice(&chunk[..n]);
        while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = acc.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            let text = text.trim_end_matches('\r');
            if let Some(rest) = text.strip_prefix("data:") {
                if let Ok(seq) = rest.trim().parse::<i64>() {
                    return Some(seq);
                }
            }
        }
    }
}

/// Serve `app` on an ephemeral localhost port; returns the bound address. The server task runs
/// for the test's lifetime (dropped with the runtime).
async fn serve(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

// --------------------------------------------------------------------- tests

/// AUTH: the stream requires a bearer — an unauthenticated subscribe is rejected 401 (same
/// `AuthTenant` gate as `/sync/pull`), never an open anonymous stream.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn stream_requires_auth() {
    let state = setup().await;
    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/sync/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "an unauthenticated /sync/stream must be 401"
    );
}

/// TENANT SCOPING end-to-end: subscribe as A and B on real sockets, push as A through a clone
/// of the same router; ONLY A's stream receives a `data:` wakeup carrying the committed seq,
/// and B's stream stays silent.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn push_wakes_only_the_pushing_tenant() {
    let state = setup().await;
    let app = build_router(state);
    // One router, cloned: one clone is served (real sockets for the subscribers), the other
    // drives the push via oneshot. Both share the same AppState → the same SseHub.
    let push_app = app.clone();
    let addr = serve(app).await;

    let (token_a, _tenant_a, client_a) = signup(&push_app).await;
    let (token_b, _tenant_b, _client_b) = signup(&push_app).await;

    let mut sock_a = subscribe(addr, &token_a).await;
    let mut sock_b = subscribe(addr, &token_b).await;
    // Give both broadcast subscriptions a beat to register before the push fans out.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = push_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sync/push")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token_a}"))
                .body(Body::from(push_body(client_a, 1).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "push must succeed");
    let pushed = body_json(resp).await;
    let max_seq = pushed["max_changeset_seq"].as_i64().unwrap();
    assert_eq!(max_seq, 1, "first commit is seq 1");

    // A wakes with the committed seq...
    let wake_a = read_wake(&mut sock_a, Duration::from_secs(3)).await;
    assert_eq!(
        wake_a,
        Some(1),
        "tenant A's subscriber must receive the wakeup carrying max_changeset_seq"
    );
    // ...B (a different tenant) must NOT — cross-tenant isolation.
    let wake_b = read_wake(&mut sock_b, Duration::from_millis(500)).await;
    assert_eq!(
        wake_b, None,
        "tenant B must NOT receive tenant A's wakeup (cross-tenant leak)"
    );
}
