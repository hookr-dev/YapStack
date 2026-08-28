// SPDX-License-Identifier: AGPL-3.0-only
//! Request extractors: the authenticated tenant context and the client IP.
//!
//! `AuthTenant` is the ONLY source of `tenant_id` for tenant-scoped handlers — it comes
//! from a validated access token, NEVER from the request body or a path/query param.
//! This closes the confused-deputy / IDOR path and is what the RLS guard binds to.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

/// The validated `(user_id, tenant_id, client_id)` from a
/// `Authorization: Bearer <access>` header.
#[derive(Debug, Clone, Copy)]
pub struct AuthTenant {
    pub user_id: Uuid,
    pub tenant_id: Uuid,
    /// The calling DEVICE bound into the access token at login (never request-supplied).
    /// `None` on the recovery path (no specific enrolled device). Handlers that must
    /// know which device is calling (e.g. `PUT /devices/roster`, §7.5) read this.
    pub client_id: Option<Uuid>,
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
            client_id: claims.client_id,
        })
    }
}

/// The IPs of trusted reverse proxies (e.g. the local Caddy/nginx). Inserted into the
/// request extensions by [`crate::build_router`] so the generic [`ClientIp`] extractor
/// can read it without a state bound. Empty ⇒ fail-closed: `X-Forwarded-For` is ignored
/// entirely and the wire peer is the rate-limit key.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies(pub Arc<Vec<IpAddr>>);

/// Trustworthy client IP for rate-limiting keys. The ground truth is the wire peer from
/// `ConnectInfo<SocketAddr>` (requires `into_make_service_with_connect_info`, wired in
/// `main`). `X-Forwarded-For` is honored ONLY when that peer is a configured trusted
/// proxy ([`TrustedProxies`]) — otherwise a client can spoof the header to rotate into
/// unlimited fresh rate-limit buckets. The chosen value must parse as an `IpAddr`. Falls
/// back to `0.0.0.0` when the peer is unknown (a single shared bucket rather than an
/// extractor failure — the limiter must never 500).
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

impl ClientIp {
    /// The real client from `X-Forwarded-For` when the immediate peer is trusted. A
    /// trusted proxy (Caddy/nginx, like `$proxy_add_x_forwarded_for`) APPENDS the peer
    /// it observed to the right of any inbound value, so walk right-to-left and return
    /// the first element that parses as an `IpAddr` and is not itself a trusted proxy —
    /// the real client. Attacker-supplied values stay left of the appended peer and are
    /// skipped; non-IP junk never becomes a key.
    fn from_xff(parts: &Parts, trusted: &TrustedProxies) -> Option<IpAddr> {
        parts
            .headers
            .get("x-forwarded-for")?
            .to_str()
            .ok()?
            .split(',')
            .rev()
            .filter_map(|s| s.trim().parse::<IpAddr>().ok())
            .find(|ip| !trusted.0.contains(ip))
    }
}

#[axum::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip());

        let ip = match peer {
            // The peer is a configured trusted proxy ⇒ the real client is behind it in
            // X-Forwarded-For. Fall back to the peer if XFF is absent/unusable.
            Some(peer)
                if parts
                    .extensions
                    .get::<TrustedProxies>()
                    .is_some_and(|tp| tp.0.contains(&peer)) =>
            {
                let tp = parts.extensions.get::<TrustedProxies>().unwrap();
                Self::from_xff(parts, tp).unwrap_or(peer).to_string()
            }
            // Direct (or untrusted) peer ⇒ ground truth, XFF ignored (fail-closed).
            Some(peer) => peer.to_string(),
            // No connect-info (e.g. unit tests) ⇒ a single shared bucket.
            None => "0.0.0.0".to_string(),
        };
        Ok(ClientIp(ip))
    }
}
