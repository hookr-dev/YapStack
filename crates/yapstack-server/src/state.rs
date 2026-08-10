// SPDX-License-Identifier: AGPL-3.0-only
//! Shared application state.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Semaphore;
use yapstack_entitlements::{AllowAll, StoredLimits, TenantLimitSource};
use zeroize::Zeroizing;

use crate::config::{Config, StorageConfig};
use crate::error::AppError;
use crate::extract::TrustedProxies;
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
    /// Per-IP throttle on `POST /auth/login/finish` (nil workspace key).
    pub login_ratelimit: Arc<RateLimiter>,
    /// Per-IP throttle on `POST /auth/signup` (nil workspace key).
    pub signup_ratelimit: Arc<RateLimiter>,
    /// Bounds concurrent Argon2id verifier hashes (sized to the core count) so a
    /// login/signup flood cannot drive the box into CPU exhaustion. See
    /// [`AppState::server_verifier`].
    pub hash_sem: Arc<Semaphore>,
    /// Trusted reverse-proxy IPs, shared into request extensions for [`crate::extract::ClientIp`].
    pub trusted_proxies: TrustedProxies,
    /// Optional signup invite token (env `YAPSTACK_SIGNUP_INVITE`). `Some` ⇒ signup
    /// requires a matching `X-YapStack-Invite` header; `None` ⇒ open (default).
    pub signup_invite: Option<String>,
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
        let login_ratelimit = Arc::new(RateLimiter::new(config.ratelimit.login_per_minute));
        let signup_ratelimit = Arc::new(RateLimiter::new(config.ratelimit.signup_per_minute));
        // Bound concurrent Argon2id hashes to the core count: the verifier is a
        // deliberately expensive CPU hash, so unbounded concurrent hashing under a flood
        // is the DoS. At least one permit even if the count probe fails.
        let cores = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        let hash_sem = Arc::new(Semaphore::new(cores));
        let trusted_proxies = TrustedProxies(Arc::new(config.trusted_proxy_ips()));
        let signup_invite = Config::signup_invite();
        let admin_key = config.admin_public_key_bytes();
        Self {
            pool,
            config: Arc::new(config),
            jwt,
            limits,
            sse: Arc::new(SseHub::new()),
            ratelimit,
            login_ratelimit,
            signup_ratelimit,
            hash_sem,
            trusted_proxies,
            signup_invite,
            admin_key,
        }
    }

    /// Compute the CRYPTO_SPEC §3.1 second-hash verifier off the async runtime. Argon2id
    /// is a deliberately expensive CPU hash; running it inline on a tokio worker lets a
    /// login/signup flood starve every async task. `spawn_blocking` moves it to the
    /// blocking pool and [`AppState::hash_sem`] bounds concurrent hashes so the box can't
    /// be driven into CPU exhaustion. The `auth_key` moves into the blocking closure and
    /// its `Zeroizing` wrapper wipes it there on drop.
    ///
    /// # Errors
    /// [`AppError::Internal`] if the semaphore is closed, the blocking task panics, or
    /// the KDF fails.
    pub async fn server_verifier(
        &self,
        auth_key: Zeroizing<Vec<u8>>,
        salt: Vec<u8>,
    ) -> Result<[u8; 32], AppError> {
        let permit = self
            .hash_sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Internal("hash semaphore closed".into()))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit; // held for the hash's duration, released on drop
            yapstack_crypto::kdf::server_verifier(&auth_key, &salt)
        })
        .await
        .map_err(|e| AppError::Internal(format!("verifier task: {e}")))?
        .map_err(|e| AppError::Internal(format!("verifier: {e}")))
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
