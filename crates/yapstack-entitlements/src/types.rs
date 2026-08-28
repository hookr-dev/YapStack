// SPDX-License-Identifier: AGPL-3.0-only
//! The quantitative-only limit vocabulary and the resolved per-tenant limit set.

use chrono::{DateTime, Utc};

/// A single quota dimension. **Quantitative only, by design** — there is no
/// flag/allowlist variant, so a feature can never be gated through this type
/// (ENTITLEMENTS_SEAM.md decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Unlimited,
    Max(u64),
}

impl Limit {
    /// Does adding `additional` to `current` stay within this limit?
    /// `Unlimited` always allows. This is the primitive the T008 choke point calls;
    /// reads/export never consult it (ENTITLEMENTS_SEAM.md enforcement semantics).
    #[must_use]
    pub fn admits(self, current: u64, additional: u64) -> bool {
        match self {
            Limit::Unlimited => true,
            Limit::Max(max) => current.saturating_add(additional) <= max,
        }
    }

    /// Remaining headroom, or `None` when unlimited.
    #[must_use]
    pub fn remaining(self, current: u64) -> Option<u64> {
        match self {
            Limit::Unlimited => None,
            Limit::Max(max) => Some(max.saturating_sub(current)),
        }
    }
}

/// Control-plane-supplied warn thresholds (fractions of the limit, e.g. `0.8`),
/// surfaced in-band by the client. Softness is control-plane policy, not relay code
/// (decision 7) — the relay only carries these numbers, it does not invent them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WarnThresholds {
    pub storage_bytes: Option<f64>,
    pub upload_bytes_period: Option<f64>,
}

/// Tenant lifecycle. `Frozen` blocks *new uploads only* — never deletion, never a
/// read/export lockout (enforcement semantics: under E2E a lapse must never lock a
/// user away from their own ciphertext).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantState {
    Active,
    Grace { until: DateTime<Utc> },
    Frozen,
}

/// The resolved operational limits for one tenant, as returned by a
/// [`crate::TenantLimitSource`].
#[derive(Debug, Clone, PartialEq)]
pub struct TenantLimits {
    /// Opaque plan label; the relay NEVER interprets it (decision 6/7).
    pub plan: String,
    pub storage_bytes: Limit,
    pub upload_bytes_period: Limit,
    pub share_count: Limit,
    pub device_count: Limit,
    pub period_start: Option<DateTime<Utc>>,
    pub warn_at: Option<WarnThresholds>,
    pub state: TenantState,
}

impl TenantLimits {
    /// Everything unlimited, `Active` — the self-host / `AllowAll` shape.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            plan: "self-host".to_string(),
            storage_bytes: Limit::Unlimited,
            upload_bytes_period: Limit::Unlimited,
            share_count: Limit::Unlimited,
            device_count: Limit::Unlimited,
            period_start: None,
            warn_at: None,
            state: TenantState::Active,
        }
    }
}
