// SPDX-License-Identifier: AGPL-3.0-only
//! Shared application state.

use std::sync::Arc;

use sqlx::PgPool;
use yapstack_entitlements::{AllowAll, StoredLimits, TenantLimitSource};

use crate::config::Config;
use crate::jwt::JwtKeys;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub jwt: Arc<JwtKeys>,
    /// Selected by config ONLY (ENTITLEMENTS_SEAM.md guardrail): `[limits]` present ⇒
    /// [`StoredLimits`], absent ⇒ [`AllowAll`]. No compiled-in private impl.
    pub limits: Arc<dyn TenantLimitSource>,
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
        Self {
            pool,
            config: Arc::new(config),
            jwt,
            limits,
        }
    }
}
