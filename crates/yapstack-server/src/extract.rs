// SPDX-License-Identifier: AGPL-3.0-only
//! Request extractors: the authenticated tenant context and the client IP.
//!
//! `AuthTenant` is the ONLY source of `tenant_id` for tenant-scoped handlers — it comes
//! from a validated access token, NEVER from the request body or a path/query param.
//! This closes the confused-deputy / IDOR path and is what the RLS guard binds to.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

/// The validated `(user_id, tenant_id)` from a `Authorization: Bearer <access>` header.
#[derive(Debug, Clone, Copy)]
pub struct AuthTenant {
    pub user_id: Uuid,
    pub tenant_id: Uuid,
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthTenant {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(AppError::Unauthorized)?;
        let claims = st
            .jwt
            .verify_access(token)
            .map_err(|_| AppError::Unauthorized)?;
        Ok(AuthTenant {
            user_id: claims.sub,
            tenant_id: claims.tenant,
        })
    }
}

/// Best-effort client IP for rate-limiting keys. Reads `X-Forwarded-For` (first hop)
/// then `X-Real-IP`; falls back to `0.0.0.0` when absent (a single shared bucket rather
/// than an extractor failure — the limiter must never 500).
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

#[axum::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let ip = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                parts
                    .headers
                    .get("x-real-ip")
                    .and_then(|v| v.to_str().ok())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("0.0.0.0")
            .to_string();
        Ok(ClientIp(ip))
    }
}
