// SPDX-License-Identifier: AGPL-3.0-only
#![forbid(unsafe_code)]
//! # yapstack-server
//!
//! The YapStack blind relay (architecture §5, World B). Gate 3 scope: auth
//! (CRYPTO_SPEC §3), RLS metadata isolation with the standard pooling guards, the
//! entitlements seam wiring, and the informational endpoints. It NEVER runs the CRDT
//! engine, NEVER decrypts content, and makes ZERO outbound calls.
//!
//! Gate 4 (T008) adds the changeset relay (push/pull/completeness/SSE), audio presign,
//! the single ingestion choke point, per-tenant usage metering, the admin entitlements
//! endpoints, and push rate limiting — all still blind (opaque ciphertext, zero
//! outbound calls, never decrypts).

pub mod admin;
pub mod audio;
pub mod auth;
pub mod choke;
pub mod config;
pub mod db;
pub mod devices;
pub mod error;
pub mod extract;
pub mod gc;
pub mod jwt;
pub mod ratelimit;
pub mod routes;
pub mod snapshot;
pub mod sse;
pub mod state;
pub mod storage;
pub mod sync;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use axum::{Extension, Router};
use yapstack_common::sync::MAX_PUSH_BYTES;

pub use config::Config;
pub use state::AppState;

/// Request-body limit for `POST /sync/push`. axum's default body limit is 2 MiB, which
/// a legal maximal push (up to [`MAX_PUSH_BYTES`] = 5 MiB of RAW ciphertext) blows
/// through as `413 Payload Too Large` LONG before the application-level 5 MiB check in
/// `sync::push` is even reached — the live "413 forever" symptom. The body is base64
/// (≈ 4/3 expansion of the raw ciphertext) plus a JSON envelope, so the limit is sized
/// to base64(MAX_PUSH_BYTES) + 1 MiB of envelope headroom. That makes the raw-byte
/// check in the handler the ACTUAL gate while still bounding memory. The client keeps
/// `PUSH_WIRE_BUDGET` (1.5 MB) conservative, well under this.
const PUSH_BODY_LIMIT: usize = (MAX_PUSH_BYTES / 3) * 4 + 1024 * 1024;

/// Assemble the full router. Kept separate from `main` so integration tests can mount
/// it against a test pool. The admin routes are mounted ONLY when a valid
/// `admin_public_key` is configured (self-host: absent ⇒ not mounted at all).
pub fn build_router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/health", get(routes::health))
        .route("/version", get(routes::version))
        .route("/sync/info", get(routes::sync_info))
        .route("/auth/signup", post(auth::signup))
        .route("/auth/login/begin", post(auth::login_begin))
        .route("/auth/login/finish", post(auth::login_finish))
        .route("/auth/refresh", post(auth::refresh))
        .route("/auth/recover", post(auth::recover))
        // --- device authorization (§7.3/§7.4/§7.5) ---
        .route("/devices", get(devices::list))
        .route(
            "/devices/roster",
            get(devices::get_roster).put(devices::put_roster),
        )
        // --- changeset relay (§7) ---
        // The push route raises the default 2 MiB body limit to admit a maximal legal
        // batch (base64 of MAX_PUSH_BYTES + envelope) so the handler's own byte cap is
        // the real gate, not the transport limit. Applied per-route so the small-body
        // endpoints (auth, presign) keep the tight default.
        .route(
            "/sync/push",
            post(sync::push::push).layer(DefaultBodyLimit::max(PUSH_BODY_LIMIT)),
        )
        .route("/sync/pull", get(sync::pull::pull))
        .route("/sync/completeness", get(sync::completeness::completeness))
        // SSE wakeup stream (§7a). Client: yapstack_sync::transport::stream_wakeups, driven
        // by the desktop drain's SSE subscriber lane. Hint-only — pull remains the source of
        // truth.
        .route("/sync/stream", get(sync::stream::stream))
        // --- audio E2E blobs (§9, audio round-trip D6/D8) ---
        // Blobs are addressed by the session_audio_parts row UUID (part_id); presign gains
        // the D8 existence check. Client wiring lands with the S1 upload queue.
        .route("/audio/presign", post(audio::presign))
        .route("/audio/part/:part_id", get(audio::get_by_part))
        // --- R2 snapshot bootstrap (blind opaque blob, §9-style) ---
        .route("/snapshot/presign", post(snapshot::presign))
        .route("/snapshot", get(snapshot::head));

    if state.admin_enabled() {
        router = router
            .route("/admin/v1/tenants/:id/limits", put(admin::put_limits))
            .route("/admin/v1/tenants/:id/usage", get(admin::get_usage));
    }

    // Relay blob GC (hardening item 5): a background sweep, NOT an admin endpoint (the admin
    // API is unmounted in self-host). No-op unless object storage is configured and
    // gc.enabled. Wired here — main() only calls build_router — so the same startup path
    // arms it. Integration tests that mount the router without storage never spawn it, and
    // the ones that do exit long before the post-boot initial delay elapses.
    gc::spawn(&state);

    // Share the trusted-proxy allowlist into request extensions so the generic `ClientIp`
    // extractor can read it without a state bound. `ConnectInfo<SocketAddr>` (the wire
    // peer) is supplied by `main`'s `into_make_service_with_connect_info`.
    let trusted_proxies = state.trusted_proxies.clone();
    router.layer(Extension(trusted_proxies)).with_state(state)
}
