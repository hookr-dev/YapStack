// SPDX-License-Identifier: AGPL-3.0-only
//! The [`TenantLimitSource`] contract and its two in-repo implementations.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::types::{Limit, TenantLimits, TenantState, WarnThresholds};
use crate::TenantId;

/// Resolves per-tenant operational limits. **Fail-open by contract:** an
/// implementation MUST NOT error and MUST NOT block boot or ingestion — a
/// resolution failure resolves to the deployment default (or unlimited), never a
/// lockout (ENTITLEMENTS_SEAM.md: the Cal.com fail-closed bug class is structurally
/// absent).
#[async_trait]
pub trait TenantLimitSource: Send + Sync {
    async fn limits(&self, tenant: TenantId) -> TenantLimits;
}

/// Self-host default: every dimension unlimited, always `Active`. Wired whenever the
/// `[limits]` config section is absent.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

#[async_trait]
impl TenantLimitSource for AllowAll {
    async fn limits(&self, _tenant: TenantId) -> TenantLimits {
        TenantLimits::unlimited()
    }
}

/// Deployment fallback numbers from `[limits.default]`. A field left `None` resolves
/// to [`Limit::Unlimited`] (self-host omits the section entirely ⇒ all unlimited);
/// hosted sets free-tier numbers so a provisioning gap costs at most free-tier
/// leakage, not an open door.
#[derive(Debug, Clone, Copy, Default)]
pub struct LimitDefaults {
    pub storage_bytes: Option<u64>,
    pub upload_bytes_period: Option<u64>,
    pub share_count: Option<u64>,
    pub device_count: Option<u64>,
}

impl LimitDefaults {
    fn as_limits(&self) -> TenantLimits {
        TenantLimits {
            plan: "default".to_string(),
            storage_bytes: opt_to_limit(self.storage_bytes),
            upload_bytes_period: opt_to_limit(self.upload_bytes_period),
            share_count: opt_to_limit(self.share_count),
            device_count: opt_to_limit(self.device_count),
            period_start: None,
            warn_at: None,
            state: TenantState::Active,
        }
    }
}

fn opt_to_limit(v: Option<u64>) -> Limit {
    v.map_or(Limit::Unlimited, Limit::Max)
}

/// One `tenant_limits` row. A NULL limit column means *explicit unlimited* (an admin
/// PUT with `unlimited: true` stores NULL); a *missing row* means "use the default".
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredRow {
    pub plan: String,
    pub storage_bytes: Option<i64>,
    pub upload_bytes_period: Option<i64>,
    pub share_count: Option<i64>,
    pub device_count: Option<i64>,
    pub period_start: Option<DateTime<Utc>>,
    pub warn_storage: Option<f64>,
    pub warn_upload: Option<f64>,
    pub state: String,
    pub grace_until: Option<DateTime<Utc>>,
}

fn col_to_limit(v: Option<i64>) -> Limit {
    // NULL ⇒ Unlimited; a non-negative bigint ⇒ Max. Negative is coerced to 0 rather
    // than trusted (defense against a corrupt row); there is no `-1` sentinel.
    match v {
        None => Limit::Unlimited,
        Some(n) => Limit::Max(u64::try_from(n).unwrap_or(0)),
    }
}

/// Pure resolution used by [`StoredLimits`]: a present row is honored; a missing row
/// falls open to the deployment default. Kept side-effect-free so the missing-row
/// fallback is unit-testable without a database.
#[must_use]
pub fn resolve(row: Option<StoredRow>, default: &LimitDefaults) -> TenantLimits {
    let Some(row) = row else {
        return default.as_limits();
    };
    let state = match row.state.as_str() {
        "frozen" => TenantState::Frozen,
        "grace" => row
            .grace_until
            .map_or(TenantState::Active, |until| TenantState::Grace { until }),
        _ => TenantState::Active,
    };
    let warn_at = if row.warn_storage.is_some() || row.warn_upload.is_some() {
        Some(WarnThresholds {
            storage_bytes: row.warn_storage,
            upload_bytes_period: row.warn_upload,
        })
    } else {
        None
    };
    TenantLimits {
        plan: row.plan,
        storage_bytes: col_to_limit(row.storage_bytes),
        upload_bytes_period: col_to_limit(row.upload_bytes_period),
        share_count: col_to_limit(row.share_count),
        device_count: col_to_limit(row.device_count),
        period_start: row.period_start,
        warn_at,
        state,
    }
}

/// Reads `tenant_limits` rows from the relay's own Postgres (control plane *pushes*
/// them via the admin endpoint; the relay makes zero outbound calls — decision 3).
#[derive(Clone)]
pub struct StoredLimits {
    pool: PgPool,
    default: LimitDefaults,
}

impl StoredLimits {
    #[must_use]
    pub fn new(pool: PgPool, default: LimitDefaults) -> Self {
        Self { pool, default }
    }

    async fn fetch(&self, tenant: TenantId) -> Result<Option<StoredRow>, sqlx::Error> {
        // `tenant_limits` is FORCE-RLS (fail-closed on an unset guard). The read MUST run
        // inside a transaction that sets the transaction-local `app.tenant_id`, or the
        // row is invisible and every tenant silently resolves to the default. This is
        // the same pooling guard the relay's tenant-scoped handlers use.
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant.to_string())
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query_as::<_, StoredRow>(
            "SELECT plan, storage_bytes, upload_bytes_period, share_count, device_count, \
             period_start, warn_storage, warn_upload, state, grace_until \
             FROM tenant_limits WHERE tenant_id = $1",
        )
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row)
    }
}

#[async_trait]
impl TenantLimitSource for StoredLimits {
    async fn limits(&self, tenant: TenantId) -> TenantLimits {
        match self.fetch(tenant).await {
            Ok(row) => resolve(row, &self.default),
            Err(e) => {
                // Fail OPEN: a DB hiccup must never lock a tenant out. Resolve to the
                // deployment default and carry on.
                tracing::warn!(%tenant, error = %e, "tenant_limits lookup failed; resolving to default (fail-open)");
                resolve(None, &self.default)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Limit;

    #[tokio::test]
    async fn allow_all_is_unlimited() {
        let limits = AllowAll.limits(TenantId::new_v4()).await;
        assert_eq!(limits.storage_bytes, Limit::Unlimited);
        assert_eq!(limits.upload_bytes_period, Limit::Unlimited);
        assert_eq!(limits.share_count, Limit::Unlimited);
        assert_eq!(limits.device_count, Limit::Unlimited);
        assert_eq!(limits.state, TenantState::Active);
    }

    #[test]
    fn stored_missing_row_falls_back_to_default() {
        let default = LimitDefaults {
            storage_bytes: Some(3_221_225_472),
            upload_bytes_period: Some(1_073_741_824),
            share_count: Some(3),
            device_count: Some(5),
        };
        let resolved = resolve(None, &default);
        assert_eq!(resolved.storage_bytes, Limit::Max(3_221_225_472));
        assert_eq!(resolved.share_count, Limit::Max(3));
        assert_eq!(resolved.state, TenantState::Active);
    }

    #[test]
    fn stored_missing_row_with_empty_default_is_unlimited() {
        // Self-host: no [limits.default] ⇒ missing row resolves to Unlimited.
        let resolved = resolve(None, &LimitDefaults::default());
        assert_eq!(resolved.storage_bytes, Limit::Unlimited);
        assert_eq!(resolved.device_count, Limit::Unlimited);
    }

    #[test]
    fn stored_null_column_is_explicit_unlimited() {
        let row = StoredRow {
            plan: "pro".to_string(),
            storage_bytes: None, // explicit unlimited
            upload_bytes_period: Some(1024),
            share_count: Some(50),
            device_count: None,
            period_start: None,
            warn_storage: Some(0.8),
            warn_upload: None,
            state: "active".to_string(),
            grace_until: None,
        };
        let resolved = resolve(Some(row), &LimitDefaults::default());
        assert_eq!(resolved.storage_bytes, Limit::Unlimited);
        assert_eq!(resolved.upload_bytes_period, Limit::Max(1024));
        assert_eq!(resolved.plan, "pro");
        assert!(resolved.warn_at.is_some());
    }
}
