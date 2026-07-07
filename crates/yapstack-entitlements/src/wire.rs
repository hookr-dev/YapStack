// SPDX-License-Identifier: AGPL-3.0-only
//! Published admin/usage wire contract (ENTITLEMENTS_SEAM.md "Wire contract").
//!
//! These are the JSON shapes for `PUT /admin/v1/tenants/{id}/limits` and
//! `GET /admin/v1/tenants/{id}/usage`. Explicit `unlimited: bool` — no `-1`
//! sentinels. Tier→numbers mapping (and softness headroom) lives only in the
//! private `yapstack-cloud` control plane, never here.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{Limit, TenantState, WarnThresholds};

/// One limit dimension on the wire: `{ "limit": u64, "unlimited": bool }`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LimitWire {
    pub limit: u64,
    pub unlimited: bool,
}

impl From<Limit> for LimitWire {
    fn from(l: Limit) -> Self {
        match l {
            Limit::Unlimited => LimitWire {
                limit: 0,
                unlimited: true,
            },
            Limit::Max(n) => LimitWire {
                limit: n,
                unlimited: false,
            },
        }
    }
}

impl From<LimitWire> for Limit {
    fn from(w: LimitWire) -> Self {
        if w.unlimited {
            Limit::Unlimited
        } else {
            Limit::Max(w.limit)
        }
    }
}

/// Wire form of [`crate::TenantState`]: `state` + optional `grace_until`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StateWire {
    Active,
    Grace,
    Frozen,
}

/// Warn thresholds on the wire (fractions), matching [`WarnThresholds`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct WarnAtWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_bytes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_bytes_period: Option<f64>,
}

impl From<WarnAtWire> for WarnThresholds {
    fn from(w: WarnAtWire) -> Self {
        WarnThresholds {
            storage_bytes: w.storage_bytes,
            upload_bytes_period: w.upload_bytes_period,
        }
    }
}

/// `PUT /admin/v1/tenants/{tenant_id}/limits` body. The control plane pushes this;
/// the request is Ed25519-signed over `(body, timestamp, monotonic nonce)` at the
/// transport layer (verified by the relay against the configured `admin_public_key`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TenantLimitsUpdate {
    pub plan: String,
    pub storage_bytes: LimitWire,
    pub upload_bytes_period: LimitWire,
    pub share_count: LimitWire,
    pub device_count: LimitWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_start: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_at: Option<WarnAtWire>,
    pub state: StateWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_until: Option<DateTime<Utc>>,
    /// Free-text audit field (e.g. `"stripe:sub_1AbC…"`). Never interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TenantLimitsUpdate {
    /// Resolve the wire body's declared [`TenantState`], validating the
    /// `grace`/`grace_until` pairing.
    #[must_use]
    pub fn tenant_state(&self) -> TenantState {
        match self.state {
            StateWire::Active => TenantState::Active,
            StateWire::Frozen => TenantState::Frozen,
            StateWire::Grace => self
                .grace_until
                .map_or(TenantState::Active, |until| TenantState::Grace { until }),
        }
    }
}

/// `GET /admin/v1/tenants/{tenant_id}/usage` — open metering; self-hosters get it
/// free (decision 4, ENTITLEMENTS_SEAM.md "Metering in the open").
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageResponse {
    pub storage_bytes: u64,
    pub upload_bytes_period: u64,
    pub share_count: u64,
    pub device_count: u64,
    pub window_start: DateTime<Utc>,
}

/// Machine-readable quota error (`HTTP 429`), upsell copy supplied by deployment
/// config — never hardcoded here (avoids the Standard Notes `TRUSTED_FEATURE_HOSTS`
/// anti-pattern).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaExceeded {
    pub code: String,
    pub resource: String,
    pub used: u64,
    pub limit: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// From deployment config; empty on self-host.
    pub help_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_wire_roundtrips() {
        for l in [Limit::Unlimited, Limit::Max(7_516_192_768)] {
            let w = LimitWire::from(l);
            assert_eq!(Limit::from(w), l);
        }
    }

    #[test]
    fn unlimited_wire_has_no_sentinel() {
        let w = LimitWire::from(Limit::Unlimited);
        assert!(w.unlimited);
        assert_eq!(w.limit, 0);
        let json = serde_json::to_string(&w).unwrap();
        assert!(json.contains("\"unlimited\":true"));
    }

    #[test]
    fn update_body_deserializes_seam_example() {
        let body = r#"{
            "plan": "pro",
            "storage_bytes": { "limit": 107374182400, "unlimited": false },
            "upload_bytes_period": { "limit": 7516192768, "unlimited": false },
            "share_count": { "limit": 50, "unlimited": false },
            "device_count": { "limit": 10, "unlimited": false },
            "period_start": "2026-07-01T00:00:00Z",
            "warn_at": { "storage_bytes": 0.8, "upload_bytes_period": 0.8 },
            "state": "active", "grace_until": null,
            "reason": "stripe:sub_1AbC"
        }"#;
        let update: TenantLimitsUpdate = serde_json::from_str(body).unwrap();
        assert_eq!(update.plan, "pro");
        assert_eq!(Limit::from(update.share_count), Limit::Max(50));
        assert_eq!(update.tenant_state(), TenantState::Active);
    }
}
