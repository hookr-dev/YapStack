// SPDX-License-Identifier: AGPL-3.0-only
//! Uniform HTTP error type. Internal/DB errors NEVER leak details to the client.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use yapstack_entitlements::wire::QuotaExceeded;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("not found")]
    NotFound,
    /// The single-choke-point quota breach (HTTP 429). Carries the machine-readable
    /// `quota_exceeded` shape (ENTITLEMENTS_SEAM.md) verbatim.
    #[error("quota exceeded")]
    Quota(Box<QuotaExceeded>),
    /// Push rate limit (architecture §10) — distinct from a quota breach.
    #[error("rate limited")]
    RateLimited,
    /// A required subsystem (e.g. object storage) is not configured. NEVER used to
    /// gate reads.
    #[error("service unavailable: {0}")]
    Unavailable(String),
    #[error("database error")]
    Db(#[from] sqlx::Error),
    #[error("internal error")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // The quota error has a bespoke, published body shape — emit it directly.
        if let AppError::Quota(q) = &self {
            return (StatusCode::TOO_MANY_REQUESTS, Json(q.as_ref())).into_response();
        }
        let (status, code, message) = match &self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            // Constant-time-ish uniform message: never distinguish "unknown user" from
            // "bad verifier" (account-existence oracle avoidance, CRYPTO_SPEC §3.2).
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "invalid credentials".into(),
            ),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden", "forbidden".into()),
            AppError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m.clone()),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found".into()),
            AppError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many requests".into(),
            ),
            AppError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable", m.clone()),
            AppError::Quota(_) => unreachable!("handled above"),
            AppError::Db(e) => {
                tracing::error!(error = %e, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal error".into(),
                )
            }
            AppError::Internal(m) => {
                tracing::error!(detail = %m, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal error".into(),
                )
            }
        };
        (status, Json(json!({ "code": code, "message": message }))).into_response()
    }
}
