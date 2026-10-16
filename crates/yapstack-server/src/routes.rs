// SPDX-License-Identifier: AGPL-3.0-only
//! Unauthenticated informational endpoints.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

/// Build digest stub. The hosted relay exposes the running public-tag digest here
/// (ENTITLEMENTS_SEAM.md decision 1/8 binary transparency). Wired from the build via
/// the `YAPSTACK_BUILD_DIGEST` env at compile time; unknown in dev.
const BUILD_DIGEST: Option<&str> = option_env!("YAPSTACK_BUILD_DIGEST");

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

/// `GET /health`
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[derive(Serialize)]
pub struct VersionResponse {
    pub name: &'static str,
    pub version: &'static str,
    pub build_digest: Option<&'static str>,
}

/// `GET /version`
pub async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        build_digest: BUILD_DIGEST,
    })
}

#[derive(Serialize)]
pub struct SyncInfoResponse {
    pub protocol_version: u32,
    pub min_client_version: String,
    pub engine_version: String,
    /// Present only if deployment config set it (hosted advertises the control plane;
    /// self-host omits it and the client shows no upgrade UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing_url: Option<String>,
}

/// `GET /sync/info`
pub async fn sync_info(State(st): State<AppState>) -> Json<SyncInfoResponse> {
    let s = &st.config.sync;
    Json(SyncInfoResponse {
        protocol_version: s.protocol_version,
        min_client_version: s.min_client_version.clone(),
        engine_version: s.engine_version.clone(),
        billing_url: s.billing_url.clone(),
    })
}
