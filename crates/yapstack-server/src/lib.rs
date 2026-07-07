// SPDX-License-Identifier: AGPL-3.0-only
#![forbid(unsafe_code)]
//! # yapstack-server
//!
//! The YapStack blind relay (architecture §5, World B). Gate 3 scope: auth
//! (CRYPTO_SPEC §3), RLS metadata isolation with the standard pooling guards, the
//! entitlements seam wiring, and the informational endpoints. It NEVER runs the CRDT
//! engine, NEVER decrypts content, and makes ZERO outbound calls.
//!
//! Changeset push/pull, audio presign, the ingestion choke point, and the admin
//! entitlements endpoints are T008 (Gate 4) — deliberately absent here.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod jwt;
pub mod routes;
pub mod state;

use axum::routing::{get, post};
use axum::Router;

pub use config::Config;
pub use state::AppState;

/// Assemble the full router. Kept separate from `main` so integration tests can mount
/// it against a test pool.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(routes::health))
        .route("/version", get(routes::version))
        .route("/sync/info", get(routes::sync_info))
        .route("/auth/signup", post(auth::signup))
        .route("/auth/login/begin", post(auth::login_begin))
        .route("/auth/login/finish", post(auth::login_finish))
        .route("/auth/refresh", post(auth::refresh))
        .with_state(state)
}
