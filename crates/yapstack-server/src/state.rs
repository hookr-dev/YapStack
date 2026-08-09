// SPDX-License-Identifier: AGPL-3.0-only
//! Shared application state.

use std::sync::Arc;

use sqlx::PgPool;
use yapstack_entitlements::{AllowAll, StoredLimits, TenantLimitSource};

use crate::config::{Config, StorageConfig};
use crate::error::AppError;
use crate::jwt::JwtKeys;
use crate::ratelimit::RateLimiter;
use crate::sse::SseHub;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub jwt: Arc<JwtKeys>,
    /// Selected by config ONLY (ENTITLEMENTS_SEAM.md guardrail): `[limits]` present ⇒
    /// [`StoredLimits`], absent ⇒ [`AllowAll`]. No compiled-in private impl.
    pub limits: Arc<dyn TenantLimitSource>,
    /// In-process SSE wakeup fan-out (wakeup-only; pull is source of truth).
    pub sse: Arc<SseHub>,
    /// Per-(tenant, ip) push rate limiter (architecture §10).
    pub ratelimit: Arc<RateLimiter>,
    /// Parsed Ed25519 admin public key. `Some` ⇒ the admin API is mounted; `None` ⇒
    /// the control-plane endpoints are disabled entirely.
    pub admin_key: Option<[u8; 32]>,
}

impl AppState {
    #[must_use]
    pub fn new(pool: PgPool, config: Config) -> Self {
        let jwt = Arc::new(JwtKeys::new(config.jwt_secret.as_bytes()));
        let limits: Arc<dyn TenantLimitSource> = match &config.limits {
            Some(l) => Arc::new(StoredLimits::new(
                pool.clone(),
                l.default.to_limit_defaults(),
            )),
            None => Arc::new(AllowAll),
        };
        let ratelimit = Arc::new(RateLimiter::new(config.ratelimit.push_per_minute));
        let admin_key = config.admin_public_key_bytes();
        Self {
            pool,
            config: Arc::new(config),
            jwt,
            limits,
            sse: Arc::new(SseHub::new()),
            ratelimit,
            admin_key,
        }
    }

    /// Is the control-plane admin API enabled (a valid `admin_public_key` was parsed)?
    #[must_use]
    pub fn admin_enabled(&self) -> bool {
        self.admin_key.is_some()
    }
}

/// The configured object-storage backend, or a 503 if the deployment has none. Shared by
/// every presign path (audio, snapshot) so they answer with one message.
///
/// # Errors
/// [`AppError::Unavailable`] when `[storage]` is absent from the config.
pub(crate) fn storage_cfg(st: &AppState) -> Result<&StorageConfig, AppError> {
    st.config
        .storage
        .as_ref()
        .ok_or_else(|| AppError::Unavailable("object storage not configured".into()))
}
