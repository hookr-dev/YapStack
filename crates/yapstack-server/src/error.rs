// SPDX-License-Identifier: AGPL-3.0-only
//! Uniform HTTP error type. Internal/DB errors NEVER leak details to the client.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("not found")]
    NotFound,
    #[error("database error")]
    Db(#[from] sqlx::Error),
    #[error("internal error")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            // Constant-time-ish uniform message: never distinguish "unknown user" from
            // "bad verifier" (account-existence oracle avoidance, CRYPTO_SPEC §3.2).
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "invalid credentials".into(),
            ),
            AppError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m.clone()),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found".into()),
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
