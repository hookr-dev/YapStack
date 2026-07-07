// SPDX-License-Identifier: AGPL-3.0-only
//! THE single ingestion choke point (ENTITLEMENTS_SEAM.md "One choke point").
//!
//! BOTH `POST /sync/push` and `POST /audio/presign` pass through here before any bytes
//! are accepted. Reads (pull, completeness, audio GET), export, and sync-DOWN are NEVER
//! routed through this function — under E2E, a quota lapse must never lock a user away
//! from their own ciphertext.
//!
//! Enforcement is metered on what the relay can verify: `storage_bytes` (total
//! ciphertext at rest) and `upload_bytes_period` (ingress this billing window). A
//! `Frozen` tenant is blocked from NEW uploads only. On breach the caller returns the
//! published `HTTP 429 quota_exceeded` body.
//!
//! Correctness under concurrency: the metering is a SINGLE atomic conditional `UPDATE`
//! that both checks the limit and reserves the bytes under the row lock, so two
//! concurrent uploads cannot both slip past a shared cap.

use sqlx::{Postgres, Transaction};
use uuid::Uuid;
use yapstack_entitlements::wire::QuotaExceeded;
use yapstack_entitlements::{Limit, TenantLimits, TenantState};

use crate::error::AppError;

fn limit_bound(l: Limit) -> Option<i64> {
    match l {
        Limit::Unlimited => None,
        // A cap beyond i64::MAX is treated as effectively unbounded for the SQL guard.
        Limit::Max(n) => Some(i64::try_from(n).unwrap_or(i64::MAX)),
    }
}

fn quota(resource: &str, used: u64, limit: u64, limits: &TenantLimits, help_url: &str) -> AppError {
    AppError::Quota(Box::new(QuotaExceeded {
        code: "quota_exceeded".to_string(),
        resource: resource.to_string(),
        used,
        limit,
        resets_at: limits.period_start,
        help_url: help_url.to_string(),
    }))
}

/// Admit `added_bytes` of new ingress for `tenant`, metering usage atomically. Returns
/// `Ok(())` and increments the counters on success; returns `AppError::Quota` (429) on
/// a `Frozen` state or a storage/upload cap breach, leaving the counters unchanged so
/// the caller can roll the whole transaction back.
///
/// `added_bytes == 0` (e.g. an audio dedup hit) still validates state but reserves
/// nothing.
///
/// # Errors
/// [`AppError::Quota`] on breach/frozen; [`AppError::Db`] on a database failure.
pub async fn admit(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    limits: &TenantLimits,
    added_bytes: u64,
    help_url: &str,
) -> Result<(), AppError> {
    // Ensure the usage row exists (idempotent).
    sqlx::query("INSERT INTO tenant_usage (tenant_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(tenant)
        .execute(&mut **tx)
        .await?;

    // Roll the ingress window forward when the control plane advanced period_start.
    if let Some(period_start) = limits.period_start {
        sqlx::query(
            "UPDATE tenant_usage SET upload_bytes_period = 0, window_start = $2 \
             WHERE tenant_id = $1 AND window_start < $2",
        )
        .bind(tenant)
        .bind(period_start)
        .execute(&mut **tx)
        .await?;
    }

    // Frozen blocks NEW uploads only (never reads/export/deletes).
    if matches!(limits.state, TenantState::Frozen) {
        return Err(quota("frozen", 0, 0, limits, help_url));
    }

    let added = i64::try_from(added_bytes).unwrap_or(i64::MAX);
    let storage_bound = limit_bound(limits.storage_bytes);
    let upload_bound = limit_bound(limits.upload_bytes_period);

    // Atomic check-and-reserve: the row lock makes the read+write indivisible, so
    // concurrent uploads cannot both pass a shared cap.
    let updated: Option<(i64, i64)> = sqlx::query_as(
        "UPDATE tenant_usage \
         SET storage_bytes = storage_bytes + $2, \
             upload_bytes_period = upload_bytes_period + $2 \
         WHERE tenant_id = $1 \
           AND ($3::bigint IS NULL OR storage_bytes + $2 <= $3) \
           AND ($4::bigint IS NULL OR upload_bytes_period + $2 <= $4) \
         RETURNING storage_bytes, upload_bytes_period",
    )
    .bind(tenant)
    .bind(added)
    .bind(storage_bound)
    .bind(upload_bound)
    .fetch_optional(&mut **tx)
    .await?;

    if updated.is_some() {
        return Ok(());
    }

    // Breach: read the current counters to build a precise 429 body.
    let (cur_storage, cur_upload): (i64, i64) = sqlx::query_as(
        "SELECT storage_bytes, upload_bytes_period FROM tenant_usage WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_one(&mut **tx)
    .await?;

    let would_storage = cur_storage.saturating_add(added);
    if let Some(bound) = storage_bound {
        if would_storage > bound {
            return Err(quota(
                "storage_bytes",
                cur_storage.max(0) as u64,
                bound.max(0) as u64,
                limits,
                help_url,
            ));
        }
    }
    let would_upload = cur_upload.saturating_add(added);
    if let Some(bound) = upload_bound {
        if would_upload > bound {
            return Err(quota(
                "upload_bytes_period",
                cur_upload.max(0) as u64,
                bound.max(0) as u64,
                limits,
                help_url,
            ));
        }
    }
    // Should be unreachable (the UPDATE only fails a bound); default to a generic 429.
    Err(quota(
        "upload_bytes_period",
        cur_upload.max(0) as u64,
        0,
        limits,
        help_url,
    ))
}

/// Release `bytes` of storage previously metered (e.g. on rollback compensation or a
/// soft-delete). Never drives a counter negative. Not wired into a GC job in v1.
///
/// # Errors
/// [`AppError::Db`] on a database failure.
pub async fn release_storage(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    bytes: u64,
) -> Result<(), AppError> {
    let b = i64::try_from(bytes).unwrap_or(i64::MAX);
    sqlx::query(
        "UPDATE tenant_usage SET storage_bytes = GREATEST(0, storage_bytes - $2) \
         WHERE tenant_id = $1",
    )
    .bind(tenant)
    .bind(b)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
