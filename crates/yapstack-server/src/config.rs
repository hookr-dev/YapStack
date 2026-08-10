// SPDX-License-Identifier: AGPL-3.0-only
//! `yapstack-relay.toml` loader.
//!
//! Fail-open on entitlements: the `[limits]` section is OPTIONAL. Absent ⇒ the relay
//! wires [`yapstack_entitlements::AllowAll`] and disables the admin API — the
//! self-host default is the max tier. Present ⇒ `StoredLimits` + admin API
//! (ENTITLEMENTS_SEAM.md config seam).
//!
//! The relay makes ZERO outbound calls; nothing here can point it at a remote
//! service. `billing_url` is a passive string echoed to clients in `/sync/info`.

use serde::Deserialize;
use yapstack_entitlements::LimitDefaults;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    pub database_url: String,
    /// HS256 signing secret for access/refresh JWTs. REQUIRED; never defaulted (a
    /// defaulted secret would be a forgery hole).
    pub jwt_secret: String,
    /// Pepper for the login decoy-salt (CRYPTO_SPEC §3.2), keeping account existence
    /// unobservable. REQUIRED.
    pub server_pepper: String,
    #[serde(default)]
    pub sync: SyncInfoConfig,
    /// Absent ⇒ AllowAll + admin API disabled.
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
    /// Object-storage (S3/MinIO) presigning target. Absent ⇒ audio presign disabled
    /// (the relay still stores no bytes; it only signs URLs). Presigning is pure local
    /// HMAC — the relay makes ZERO outbound calls.
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    /// Per-`(workspace_id, ip)` push rate limit (architecture §10). Defaulted.
    #[serde(default)]
    pub ratelimit: RateLimitConfig,
    /// IPs of trusted reverse proxies (e.g. the local Caddy/nginx terminating TLS).
    /// `X-Forwarded-For` is honored ONLY when the immediate wire peer is in this list;
    /// empty (the default) ⇒ fail-closed (XFF ignored, the connecting peer is the
    /// rate-limit key). Tunable per deployment.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Relay blob GC (deletes unreferenced audio objects + rows after a grace period).
    /// Defaulted; env-overridable via [`GcConfig::resolved`].
    #[serde(default)]
    pub gc: GcConfig,
}

/// S3/MinIO presigning parameters. The relay uses these only to compute SigV4
/// presigned URLs locally; it never uploads or downloads bytes itself.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    /// e.g. `https://s3.us-east-1.amazonaws.com` or `http://minio:9000`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Optional client-facing endpoint if it differs from the signing endpoint
    /// (e.g. a public MinIO hostname). Defaults to `endpoint`.
    #[serde(default)]
    pub public_endpoint: Option<String>,
    /// Presigned-URL lifetime in seconds (default 15 min).
    #[serde(default = "default_presign_ttl")]
    pub presign_ttl_secs: u32,
}

fn default_presign_ttl() -> u32 {
    900
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct RateLimitConfig {
    /// Max `POST /sync/push` calls per `(workspace_id, ip)` per rolling minute.
    #[serde(default = "default_push_per_minute")]
    pub push_per_minute: u32,
    /// Max `POST /auth/login/finish` attempts per client IP per rolling minute. Applied
    /// identically to unknown emails (no user-existence oracle). Sane default; tunable.
    #[serde(default = "default_login_per_minute")]
    pub login_per_minute: u32,
    /// Max `POST /auth/signup` attempts per client IP per rolling minute. Sane default;
    /// tunable.
    #[serde(default = "default_signup_per_minute")]
    pub signup_per_minute: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            push_per_minute: default_push_per_minute(),
            login_per_minute: default_login_per_minute(),
            signup_per_minute: default_signup_per_minute(),
        }
    }
}

fn default_push_per_minute() -> u32 {
    120
}

fn default_login_per_minute() -> u32 {
    10
}

fn default_signup_per_minute() -> u32 {
    5
}

/// Relay blob GC tunables (hardening item 5). A background sweep deletes audio blobs whose
/// mapping-count `refcount <= 0` and whose `released_at` (the moment refcount transitioned to
/// <= 0) is older than the grace period — object first, then row (see `gc.rs`). TOML defaults
/// follow the server's config pattern (serde `default` fns); each field is additionally
/// env-overridable at startup via [`GcConfig::resolved`] so an operator can tune it without
/// editing the config file.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct GcConfig {
    /// Master on/off. Default on. Env: `YAPSTACK_GC_ENABLED` (`1`/`true`/`0`/`false`).
    #[serde(default = "default_gc_enabled")]
    pub enabled: bool,
    /// Seconds between sweeps (the interval also fires once shortly after boot). Default 24h.
    /// Env: `YAPSTACK_GC_INTERVAL_SECS`.
    #[serde(default = "default_gc_interval_secs")]
    pub interval_secs: u64,
    /// A blob is only eligible once it has been unreferenced for this many seconds. Default
    /// 7 days. Env: `YAPSTACK_GC_GRACE_SECS`.
    #[serde(default = "default_gc_grace_secs")]
    pub grace_secs: u64,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            enabled: default_gc_enabled(),
            interval_secs: default_gc_interval_secs(),
            grace_secs: default_gc_grace_secs(),
        }
    }
}

fn default_gc_enabled() -> bool {
    true
}

fn default_gc_interval_secs() -> u64 {
    86_400 // 24h
}

fn default_gc_grace_secs() -> u64 {
    604_800 // 7d
}

impl GcConfig {
    /// Layer environment overrides on top of the TOML/defaults. A malformed env value is
    /// ignored (the TOML/default value stands) rather than crashing the relay at boot.
    #[must_use]
    pub fn resolved(self) -> Self {
        fn env_u64(key: &str) -> Option<u64> {
            std::env::var(key).ok()?.trim().parse().ok()
        }
        fn env_bool(key: &str) -> Option<bool> {
            match std::env::var(key)
                .ok()?
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            }
        }
        Self {
            enabled: env_bool("YAPSTACK_GC_ENABLED").unwrap_or(self.enabled),
            interval_secs: env_u64("YAPSTACK_GC_INTERVAL_SECS").unwrap_or(self.interval_secs),
            grace_secs: env_u64("YAPSTACK_GC_GRACE_SECS").unwrap_or(self.grace_secs),
        }
    }

    /// The configured grace period as a `chrono::Duration` for the sweep predicate.
    #[must_use]
    pub fn grace(self) -> chrono::Duration {
        chrono::Duration::seconds(i64::try_from(self.grace_secs).unwrap_or(i64::MAX))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncInfoConfig {
    pub protocol_version: u32,
    pub min_client_version: String,
    pub engine_version: String,
    /// Optional; hosted advertises the control plane, self-host omits it and the
    /// client renders no upgrade UI. No hardcoded commercial endpoint in this repo.
    #[serde(default)]
    pub billing_url: Option<String>,
}

impl Default for SyncInfoConfig {
    fn default() -> Self {
        Self {
            protocol_version: 1,
            min_client_version: "1.0.0".to_string(),
            engine_version: "0.16.3".to_string(),
            billing_url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    /// `ed25519:<base64>` — presence enables StoredLimits + the admin API. The admin
    /// PUT/usage requests are verified against this key.
    pub admin_public_key: String,
    /// Upsell/help URL echoed in the `HTTP 429 quota_exceeded` body. Empty/absent on
    /// self-host (no hardcoded commercial endpoint in this AGPL repo).
    #[serde(default)]
    pub help_url: Option<String>,
    #[serde(default)]
    pub default: LimitsDefaultConfig,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct LimitsDefaultConfig {
    #[serde(default)]
    pub storage_bytes: Option<u64>,
    #[serde(default)]
    pub upload_bytes_period: Option<u64>,
    #[serde(default)]
    pub share_count: Option<u64>,
    #[serde(default)]
    pub device_count: Option<u64>,
}

impl LimitsDefaultConfig {
    #[must_use]
    pub fn to_limit_defaults(self) -> LimitDefaults {
        LimitDefaults {
            storage_bytes: self.storage_bytes,
            upload_bytes_period: self.upload_bytes_period,
            share_count: self.share_count,
            device_count: self.device_count,
        }
    }
}

fn default_bind_addr() -> String {
    "0.0.0.0:8080".to_string()
}

impl Config {
    /// Parse a TOML config string.
    ///
    /// # Errors
    /// Returns the underlying [`toml::de::Error`] on malformed input.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Load from a file path.
    ///
    /// # Errors
    /// Returns a boxed error on read or parse failure.
    pub fn from_path(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let s = std::fs::read_to_string(path)?;
        Ok(Self::from_toml_str(&s)?)
    }

    /// Is the entitlements admin API enabled (i.e. `[limits]` present)?
    #[must_use]
    pub fn limits_enabled(&self) -> bool {
        self.limits.is_some()
    }

    /// Parse `trusted_proxies` into `IpAddr`s, silently dropping malformed entries
    /// (a bad allowlist entry must not weaken the default fail-closed posture).
    #[must_use]
    pub fn trusted_proxy_ips(&self) -> Vec<std::net::IpAddr> {
        self.trusted_proxies
            .iter()
            .filter_map(|s| s.trim().parse().ok())
            .collect()
    }

    /// The optional signup invite token, read from `YAPSTACK_SIGNUP_INVITE`. When set
    /// (non-empty), `POST /auth/signup` requires a matching `X-YapStack-Invite` header;
    /// unset (the default) leaves signup open. Env-configured so operators can gate a
    /// self-host relay without editing the config file.
    #[must_use]
    pub fn signup_invite() -> Option<String> {
        std::env::var("YAPSTACK_SIGNUP_INVITE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// The 32-byte Ed25519 admin public key, parsed from `ed25519:<base64>`. `None`
    /// when `[limits]` is absent (admin API disabled) or the value is malformed (which
    /// also disables the admin API — fail-closed for the admin surface specifically,
    /// while the tenant limit path stays fail-OPEN).
    #[must_use]
    pub fn admin_public_key_bytes(&self) -> Option<[u8; 32]> {
        let raw = self
            .limits
            .as_ref()?
            .admin_public_key
            .strip_prefix("ed25519:")?;
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, raw).ok()?;
        <[u8; 32]>::try_from(bytes.as_slice()).ok()
    }

    /// Upsell `help_url` for the quota-exceeded body. Empty string on self-host.
    #[must_use]
    pub fn help_url(&self) -> String {
        self.limits
            .as_ref()
            .and_then(|l| l.help_url.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_host_config_has_no_limits_section() {
        let toml = r#"
            database_url = "postgres://localhost/yapstack"
            jwt_secret = "dev-secret"
            server_pepper = "dev-pepper"
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert!(!cfg.limits_enabled());
        assert_eq!(cfg.sync.protocol_version, 1);
        assert!(cfg.sync.billing_url.is_none());
        // GC section absent ⇒ defaults: on, 24h interval, 7d grace.
        assert!(cfg.gc.enabled);
        assert_eq!(cfg.gc.interval_secs, 86_400);
        assert_eq!(cfg.gc.grace_secs, 604_800);
        assert_eq!(cfg.gc.grace().num_days(), 7);
    }

    #[test]
    fn ratelimit_defaults_and_trusted_proxies_parse() {
        let toml = r#"
            database_url = "postgres://localhost/yapstack"
            jwt_secret = "s"
            server_pepper = "p"
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        // Sane auth throttle defaults (tunable via [ratelimit]).
        assert_eq!(cfg.ratelimit.login_per_minute, 10);
        assert_eq!(cfg.ratelimit.signup_per_minute, 5);
        assert_eq!(cfg.ratelimit.push_per_minute, 120);
        // Absent trusted_proxies => fail-closed (empty allowlist).
        assert!(cfg.trusted_proxy_ips().is_empty());
    }

    #[test]
    fn trusted_proxies_parse_drops_junk() {
        let toml = r#"
            database_url = "postgres://localhost/yapstack"
            jwt_secret = "s"
            server_pepper = "p"
            trusted_proxies = ["10.0.0.1", "not-an-ip", "::1"]

            [ratelimit]
            login_per_minute = 3
            signup_per_minute = 2
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        let ips = cfg.trusted_proxy_ips();
        assert_eq!(ips.len(), 2); // junk entry dropped
        assert!(ips.contains(&"10.0.0.1".parse().unwrap()));
        assert!(ips.contains(&"::1".parse().unwrap()));
        assert_eq!(cfg.ratelimit.login_per_minute, 3);
        assert_eq!(cfg.ratelimit.signup_per_minute, 2);
    }

    #[test]
    fn gc_section_overrides_defaults() {
        let toml = r#"
            database_url = "postgres://localhost/yapstack"
            jwt_secret = "s"
            server_pepper = "p"

            [gc]
            enabled = false
            interval_secs = 3600
            grace_secs = 60
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert!(!cfg.gc.enabled);
        assert_eq!(cfg.gc.interval_secs, 3600);
        assert_eq!(cfg.gc.grace_secs, 60);
    }

    #[test]
    fn hosted_config_enables_limits_and_defaults() {
        let toml = r#"
            database_url = "postgres://localhost/yapstack"
            jwt_secret = "s"
            server_pepper = "p"

            [sync]
            protocol_version = 1
            min_client_version = "1.2.0"
            engine_version = "0.16.3"
            billing_url = "https://yapstack.cloud/billing"

            [limits]
            admin_public_key = "ed25519:AAAA"

            [limits.default]
            storage_bytes = 3221225472
            share_count = 3
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert!(cfg.limits_enabled());
        let limits = cfg.limits.unwrap();
        let d = limits.default.to_limit_defaults();
        assert_eq!(d.storage_bytes, Some(3_221_225_472));
        assert_eq!(d.share_count, Some(3));
        assert_eq!(d.upload_bytes_period, None);
        assert_eq!(
            cfg.sync.billing_url.as_deref(),
            Some("https://yapstack.cloud/billing")
        );
    }
}
