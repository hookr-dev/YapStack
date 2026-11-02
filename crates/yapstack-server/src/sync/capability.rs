// SPDX-License-Identifier: AGPL-3.0-only
//! The sync capability gate (ADR 0006 §5): peer evidence that every recently seen device
//! can handle a new synced table before the relay accepts rows for it.
//!
//! Each tenant has one floor row (`tenant_sync_capability`). Locking it is the
//! serialization point between a floor raise and the routes that hand out or accept
//! content:
//!
//! * readers (`GET /sync/pull`, `GET /snapshot`, `POST /snapshot/presign`) take it
//!   `FOR SHARE`, record the caller's reported level, check it against the floor and read,
//!   all in one transaction;
//! * `POST /sync/push` takes it `FOR SHARE` before `tenant_seq FOR UPDATE` and refuses a
//!   batch whose declared level is above the floor;
//! * `PUT /sync/capability` takes it `FOR UPDATE`, then reads `devices`.
//!
//! So a pull that passed the old check has either finished its read before a raise
//! commits, or waits and re-checks after it. Lock order is always this row first, then
//! `tenant_seq` (push only), so there is no cycle.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
use yapstack_common::sync::{
    CapabilityBlocker, RaiseSyncCapabilityRequest, SyncCapabilityBlocked, SyncCapabilityLevels,
    CAPABILITY_WINDOW_DAYS, SYNC_CAPABILITY_HEADER,
};

use crate::db;
use crate::error::AppError;
use crate::extract::AuthTenant;
use crate::state::AppState;

/// Row-lock strength on the floor row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorLock {
    /// Readers and push: many may hold it at once; it blocks a raise.
    Share,
    /// The raise: exclusive against readers and pushes.
    Update,
}

/// Parse a capability header. Absent means level 0 (a build that predates the gate).
///
/// # Errors
/// [`AppError::BadRequest`] for a value that isn't a non-negative integer.
pub fn header_level(headers: &HeaderMap, name: &str) -> Result<i32, AppError> {
    let Some(v) = headers.get(name) else {
        return Ok(0);
    };
    v.to_str()
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|l| *l >= 0)
        .ok_or_else(|| AppError::BadRequest(format!("{name}: expected a non-negative integer")))
}

/// Bootstrap the tenant's floor row (idempotent) and lock it. Must be the first lock a
/// transaction takes on sync state.
///
/// # Errors
/// A DB error.
pub async fn lock_floor(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    lock: FloorLock,
) -> Result<SyncCapabilityLevels, AppError> {
    sqlx::query(
        "INSERT INTO tenant_sync_capability (workspace_id) VALUES ($1) ON CONFLICT DO NOTHING",
    )
    .bind(tenant)
    .execute(&mut **tx)
    .await?;
    let sql = match lock {
        FloorLock::Share => {
            "SELECT required_sync_capability, requested_sync_capability \
             FROM tenant_sync_capability WHERE workspace_id = $1 FOR SHARE"
        }
        FloorLock::Update => {
            "SELECT required_sync_capability, requested_sync_capability \
             FROM tenant_sync_capability WHERE workspace_id = $1 FOR UPDATE"
        }
    };
    let (required, requested): (i32, i32) = sqlx::query_as(sql)
        .bind(tenant)
        .fetch_one(&mut **tx)
        .await?;
    Ok(SyncCapabilityLevels {
        required_sync_capability: required,
        requested_sync_capability: requested,
    })
}

/// Record the caller's reported level on its own `devices` row. Throttled: it writes only
/// when the level changed, the stamp is over an hour old, or there is no stamp yet (so an
/// existing device's first pull, headerless or not, always counts in the 30-day window).
/// A token without a device identity has no row to stamp.
///
/// # Errors
/// A DB error.
pub async fn record_caller_level(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    client_id: Option<Uuid>,
    level: i32,
) -> Result<(), AppError> {
    let Some(client_id) = client_id else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE devices SET sync_capability = $3, capability_seen_at = now() \
         WHERE workspace_id = $1 AND client_id = $2 \
           AND (sync_capability <> $3 \
                OR capability_seen_at IS NULL \
                OR capability_seen_at < now() - interval '1 hour')",
    )
    .bind(tenant)
    .bind(client_id)
    .bind(level)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The read gate shared by pull and the snapshot routes. Opens the caller's tenant
/// transaction, locks the floor row `FOR SHARE`, records the caller's level and checks it.
/// On success the transaction is returned with the lock still held, so the caller's read
/// happens under it. A refused caller's stamp is committed before the 426, so a device that
/// keeps getting refused still counts as seen.
///
/// # Errors
/// [`AppError::UpdateRequired`] below the floor; a DB error otherwise.
pub async fn begin_gated_read<'a>(
    pool: &'a sqlx::PgPool,
    auth: &AuthTenant,
    headers: &HeaderMap,
) -> Result<(Transaction<'a, Postgres>, SyncCapabilityLevels), AppError> {
    let level = header_level(headers, SYNC_CAPABILITY_HEADER)?;
    let mut tx = db::begin_tenant_tx(pool, auth.tenant_id).await?;
    let levels = gate_read(&mut tx, auth.tenant_id, auth.client_id, level).await?;
    if level < levels.required_sync_capability {
        tx.commit().await?;
        return Err(AppError::UpdateRequired(levels));
    }
    Ok((tx, levels))
}

/// Lock, record and return the levels, without deciding. Split from [`begin_gated_read`]
/// so the interleaving test can hold a reader between its check and its read.
///
/// # Errors
/// A DB error.
pub async fn gate_read(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    client_id: Option<Uuid>,
    level: i32,
) -> Result<SyncCapabilityLevels, AppError> {
    let levels = lock_floor(tx, tenant, FloorLock::Share).await?;
    record_caller_level(tx, tenant, client_id, level).await?;
    Ok(levels)
}

#[derive(sqlx::FromRow)]
struct BlockerRow {
    client_id: Uuid,
    label: String,
    platform: String,
    sync_capability: i32,
    capability_seen_at: chrono::DateTime<chrono::Utc>,
}

/// `PUT /sync/capability` — raise the workspace floor to at least `L`.
///
/// Locks the floor row `FOR UPDATE`, records the caller's own header (so a device raising
/// at cutover never blocks itself), then re-checks eligibility in the same transaction:
/// every non-revoked device seen within [`CAPABILITY_WINDOW_DAYS`] must report at least `L`.
/// If so the floor becomes `GREATEST(required, L)`. Otherwise `requested` becomes
/// `GREATEST(requested, L)` (so stragglers are nudged) and the 409 names the blockers. The
/// client never decides eligibility from a roster it fetched earlier. A raise at or below
/// the current floor is a no-op.
pub async fn raise(
    State(st): State<AppState>,
    auth: AuthTenant,
    headers: HeaderMap,
    Json(req): Json<RaiseSyncCapabilityRequest>,
) -> Result<Json<SyncCapabilityLevels>, AppError> {
    let target = req.required_sync_capability;
    if target < 0 {
        return Err(AppError::BadRequest(
            "required_sync_capability must be non-negative".into(),
        ));
    }
    let caller_level = header_level(&headers, SYNC_CAPABILITY_HEADER)?;

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;
    crate::devices::require_active_caller(&mut tx, auth.tenant_id, auth.client_id).await?;
    let levels = lock_floor(&mut tx, auth.tenant_id, FloorLock::Update).await?;
    record_caller_level(&mut tx, auth.tenant_id, auth.client_id, caller_level).await?;

    if target <= levels.required_sync_capability {
        tx.commit().await?;
        return Ok(Json(levels));
    }

    let blockers: Vec<BlockerRow> = sqlx::query_as(
        "SELECT client_id, label, platform, sync_capability, capability_seen_at \
         FROM devices \
         WHERE workspace_id = $1 AND status <> 'revoked' \
           AND capability_seen_at > now() - make_interval(days => $3) \
           AND sync_capability < $2 \
         ORDER BY capability_seen_at DESC",
    )
    .bind(auth.tenant_id)
    .bind(target)
    .bind(CAPABILITY_WINDOW_DAYS)
    .fetch_all(&mut *tx)
    .await?;

    if blockers.is_empty() {
        let (required, requested): (i32, i32) = sqlx::query_as(
            "UPDATE tenant_sync_capability \
             SET required_sync_capability = GREATEST(required_sync_capability, $2) \
             WHERE workspace_id = $1 \
             RETURNING required_sync_capability, requested_sync_capability",
        )
        .bind(auth.tenant_id)
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(Json(SyncCapabilityLevels {
            required_sync_capability: required,
            requested_sync_capability: requested,
        }));
    }

    let (required, requested): (i32, i32) = sqlx::query_as(
        "UPDATE tenant_sync_capability \
         SET requested_sync_capability = GREATEST(requested_sync_capability, $2) \
         WHERE workspace_id = $1 \
         RETURNING required_sync_capability, requested_sync_capability",
    )
    .bind(auth.tenant_id)
    .bind(target)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Err(AppError::CapabilityBlocked(Box::new(
        SyncCapabilityBlocked {
            code: "sync_capability_blocked".into(),
            message: "other devices need to update YapStack first".into(),
            required_sync_capability: required,
            requested_sync_capability: requested,
            blockers: blockers
                .into_iter()
                .map(|b| CapabilityBlocker {
                    client_id: b.client_id,
                    label: b.label,
                    platform: b.platform,
                    sync_capability: b.sync_capability,
                    capability_seen_at: b.capability_seen_at.to_rfc3339(),
                })
                .collect(),
        },
    )))
}
