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
    /// PUT is verified against this key (T008 wires verification).
    pub admin_public_key: String,
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
