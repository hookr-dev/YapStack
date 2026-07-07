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
pub mod jwt;
pub mod ratelimit;
pub mod routes;
pub mod sse;
pub mod state;
pub mod storage;
pub mod sync;

use axum::routing::{get, post, put};
use axum::Router;

pub use config::Config;
pub use state::AppState;

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
        .route("/devices/roster", put(devices::put_roster))
        // --- changeset relay (§7) ---
        .route("/sync/push", post(sync::push::push))
        .route("/sync/pull", get(sync::pull::pull))
        .route("/sync/completeness", get(sync::completeness::completeness))
        .route("/sync/stream", get(sync::stream::stream))
        // --- audio E2E blobs (§9) ---
        .route("/audio/presign", post(audio::presign))
        .route("/audio/:session_id", get(audio::get));

    if state.admin_enabled() {
        router = router
            .route("/admin/v1/tenants/:id/limits", put(admin::put_limits))
            .route("/admin/v1/tenants/:id/usage", get(admin::get_usage));
    }

    router.with_state(state)
}
