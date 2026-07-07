// SPDX-License-Identifier: AGPL-3.0-only
//! Device-authorization endpoints (CRYPTO_SPEC §7.3/§7.4/§7.5).
//!
//! The relay stays BLIND. It stores the signed roster (`device_list`, `signature`)
//! verbatim and opaquely — it holds no vault key and NEVER reads the roster to learn
//! membership or content. It enforces only:
//!   * §7.4 anti-rollback on the plaintext integer `counter` (strictly-increasing,
//!     under a `FOR UPDATE` row lock so concurrent uploads are compare-and-set safe),
//!   * structural signature PRESENCE (a 64-byte Ed25519 signature must accompany the
//!     roster — full cryptographic verification against the vault-derived roster key is
//!     a CLIENT responsibility, §7.5 step 2/step 4; the relay cannot verify it without
//!     the vault key and does not try),
//!   * `vault_key_epoch` monotonicity (never decreases).
//!
//! `devices.status` (pending|active) is an ADVISORY index the relay maintains from
//! client-supplied plaintext metadata (`active_devices`). It is NOT authoritative:
//! clients trust the signed roster, which they verify themselves.

use axum::extract::State;
use axum::Json;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use uuid::Uuid;
use yapstack_common::auth::{
    DeviceInfo, DevicesResponse, RosterUploadRequest, RosterUploadResponse,
};

use crate::db;
use crate::error::AppError;
use crate::extract::AuthTenant;
use crate::state::AppState;

/// Ed25519 detached signatures are exactly 64 bytes (RFC 8032).
const ED25519_SIG_LEN: usize = 64;

#[derive(sqlx::FromRow)]
struct DeviceRow {
    client_id: Uuid,
    ed25519_pub: Vec<u8>,
    label: String,
    status: String,
    added_at: chrono::DateTime<chrono::Utc>,
}

/// `GET /devices` — the account's device index (pending + active) for the approving
/// device to review (§7.5 step 3). RLS-scoped to the caller's workspace via the
/// validated JWT (`tenant_id` never request-supplied).
pub async fn list(
    State(st): State<AppState>,
    auth: AuthTenant,
) -> Result<Json<DevicesResponse>, AppError> {
    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;
    let rows: Vec<DeviceRow> = sqlx::query_as(
        "SELECT client_id, ed25519_pub, label, status, added_at \
         FROM devices WHERE workspace_id = $1 ORDER BY added_at",
    )
    .bind(auth.tenant_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let devices = rows
        .into_iter()
        .map(|r| DeviceInfo {
            client_id: r.client_id,
            ed25519_pub: B64.encode(&r.ed25519_pub),
            label: r.label,
            status: r.status,
            added_at: r.added_at.to_rfc3339(),
        })
        .collect();
    Ok(Json(DevicesResponse { devices }))
}

/// `PUT /devices/roster` — upload a re-signed roster with §7.4 anti-rollback.
///
/// The counter monotonicity is a compare-and-set on the PLAINTEXT integer `counter`
/// carried alongside the opaque roster — no roster content is read. `SELECT ... FOR
/// UPDATE` locks the tenant's roster row before the compare so two concurrent uploads
/// cannot both pass a stale check (the second blocks, re-reads the advanced counter, and
/// is rejected). This is the C2 bootstrap-rollback fix: an attacker replaying an old
/// signed roster to re-admit an evicted device is rejected because its `counter` is not
/// strictly greater than the stored one.
pub async fn put_roster(
    State(st): State<AppState>,
    auth: AuthTenant,
    Json(req): Json<RosterUploadRequest>,
) -> Result<Json<RosterUploadResponse>, AppError> {
    // Signature PRESENCE / well-formedness (a 64-byte Ed25519 signature must accompany
    // the roster). The relay does NOT verify it against the vault-derived roster key —
    // it holds no vault key; clients do that (§7.5 step 2). Reject an absent/malformed
    // signature so the stored blob is never an unsigned roster.
    let signature = B64
        .decode(&req.signature)
        .map_err(|_| AppError::BadRequest("signature: invalid base64".into()))?;
    if signature.len() != ED25519_SIG_LEN {
        return Err(AppError::BadRequest("signature: expected 64 bytes".into()));
    }

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;

    // Lock the roster row for this tenant, then compare-and-set (§7.4). The lock is held
    // to commit, so concurrent uploads are serialized and the check is race-free.
    let current: Option<(i64, i64)> = sqlx::query_as(
        "SELECT counter, vault_key_epoch FROM device_roster \
         WHERE workspace_id = $1 FOR UPDATE",
    )
    .bind(auth.tenant_id)
    .fetch_optional(&mut *tx)
    .await?;

    // A roster row always exists for an authenticated tenant (created at signup). Its
    // absence would be a corrupt account, not a client-supplied condition.
    let Some((stored_counter, stored_epoch)) = current else {
        return Err(AppError::NotFound);
    };

    // ANTI-ROLLBACK (§7.4): the counter MUST strictly increase. `<=` is rejected — this
    // is what blocks replay of an old roster to re-admit an evicted device.
    if req.counter <= stored_counter {
        return Err(AppError::Conflict(
            "roster counter is not newer than the stored roster".into(),
        ));
    }
    // The vault-key epoch is monotonic non-decreasing (§4.4/§7.4): a roster may keep the
    // same epoch (membership-only change) but never lower it (that would be a rollback
    // past a vault-key rotation).
    if req.vault_key_epoch < stored_epoch {
        return Err(AppError::Conflict(
            "vault_key_epoch is older than the stored roster".into(),
        ));
    }

    // Store the opaque roster + signature + the advanced watermarks verbatim.
    sqlx::query(
        "UPDATE device_roster \
         SET device_list = $2, signature = $3, counter = $4, vault_key_epoch = $5, \
             updated_at = now() \
         WHERE workspace_id = $1",
    )
    .bind(auth.tenant_id)
    .bind(&req.device_list)
    .bind(&signature)
    .bind(req.counter)
    .bind(req.vault_key_epoch)
    .execute(&mut *tx)
    .await?;

    // Advance the ADVISORY device index from the client-supplied active-device metadata
    // (NOT read from the opaque roster). Devices the new roster names as active are
    // promoted pending→active; this is how a pending device becomes a sync peer (§7.5).
    if !req.active_devices.is_empty() {
        sqlx::query(
            "UPDATE devices SET status = 'active' \
             WHERE workspace_id = $1 AND client_id = ANY($2)",
        )
        .bind(auth.tenant_id)
        .bind(&req.active_devices)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(Json(RosterUploadResponse {
        counter: req.counter,
        vault_key_epoch: req.vault_key_epoch,
    }))
}
