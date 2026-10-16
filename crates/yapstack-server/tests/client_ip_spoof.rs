// SPDX-License-Identifier: AGPL-3.0-only
//! RED test for finding `server-dos` #5: client-supplied `X-Forwarded-For` is trusted,
//! so one header bypasses both rate limiters (push §10 guardrail + recovery throttle).
//!
//! `ClientIp` (crates/yapstack-server/src/extract.rs:60-78) takes `.split(',').next()`
//! — the LEFT-most, fully client-supplied element — as the limiter key, with no
//! trusted-proxy allowlist, no hop count, and no `IpAddr` validation. Behind the
//! documented Caddy proxy (docs/self-hosting.md), Caddy APPENDS the real peer to any
//! inbound XFF (like nginx `$proxy_add_x_forwarded_for`), so the attacker's own value
//! stays first. Rotating the header lands every request in a fresh
//! `HashMap<(Uuid, String), Bucket>` bucket (ratelimit.rs:20,46).
//!
//! These tests assert the SECURE invariant a real client cannot influence the limiter
//! key. On the current tree they FAIL because the extractor returns the spoofed value.
//! This file is the red half of the red-green harness; it is NOT the fix.

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::Request;
use yapstack_server::extract::ClientIp;

/// Build request `Parts` carrying the given raw `X-Forwarded-For` header value.
fn parts_with_xff(xff: &str) -> axum::http::request::Parts {
    Request::builder()
        .header("x-forwarded-for", xff)
        .body(Body::empty())
        .unwrap()
        .into_parts()
        .0
}

async fn client_ip(xff: &str) -> String {
    let mut parts = parts_with_xff(xff);
    // `ClientIp: FromRequestParts<S>` for any `S: Send + Sync`; `()` needs no AppState/DB.
    let ClientIp(ip) = ClientIp::from_request_parts(&mut parts, &())
        .await
        .expect("ClientIp extractor is Infallible");
    ip
}

/// Two requests that differ ONLY in the attacker-controlled `X-Forwarded-For` header
/// must map to the SAME limiter key — otherwise one client rotates into unlimited fresh
/// rate-limit buckets. On the current tree the keys differ (each spoofed value is used
/// verbatim), so this assertion fails: that IS the bypass.
#[tokio::test]
async fn spoofed_xff_cannot_rotate_the_rate_limit_key() {
    let key_a = client_ip("203.0.113.7").await;
    let key_b = client_ip("198.51.100.42").await;
    assert_eq!(
        key_a, key_b,
        "rate-limit key is attacker-controllable: two spoofed X-Forwarded-For values \
         ({key_a:?} vs {key_b:?}) produced different limiter buckets, so a single client \
         can rotate the header to escape both rate limiters",
    );
}

/// Caddy/nginx APPEND the peer to any inbound XFF, so a spoofed value stays LEFT-most:
/// `X-Forwarded-For: <attacker>, <real-peer>`. The trusted peer here is `10.0.0.1`;
/// the limiter must never key on the attacker's `203.0.113.7`. On the current tree the
/// extractor returns exactly `203.0.113.7`, so this fails: that IS the spoof.
#[tokio::test]
async fn appended_xff_does_not_key_on_the_attacker_prefix() {
    let attacker = "203.0.113.7";
    let real_peer = "10.0.0.1";
    let key = client_ip(&format!("{attacker}, {real_peer}")).await;
    assert_ne!(
        key, attacker,
        "limiter keyed on the attacker-supplied left-most X-Forwarded-For element \
         instead of the trusted appended peer {real_peer:?}",
    );
}

/// A limiter key that is never validated as an `IpAddr` lets arbitrary attacker strings
/// become map keys, growing `HashMap<(Uuid, String), Bucket>` (cleanup only above 10k,
/// ratelimit.rs:43). The extractor must reject non-IP junk rather than key on it. On the
/// current tree the raw junk string is returned, so this fails.
#[tokio::test]
async fn non_ip_xff_is_not_used_as_a_limiter_key() {
    let junk = "not-an-ip-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let key = client_ip(junk).await;
    assert_ne!(
        key, junk,
        "arbitrary non-IP X-Forwarded-For string became the limiter key verbatim",
    );
}
