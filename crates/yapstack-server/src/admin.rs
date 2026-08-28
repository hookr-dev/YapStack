// SPDX-License-Identifier: AGPL-3.0-only
//! Control-plane admin API (ENTITLEMENTS_SEAM.md wire contract). Enabled ONLY when
//! `[limits].admin_public_key` is configured; absent ⇒ these routes are not mounted.
//!
//! The control plane PUSHES per-tenant limits INBOUND over a published HTTP contract;
//! the relay makes ZERO outbound calls and has zero runtime dependency on the control
//! plane. Each request is Ed25519-signed by the control plane over
//! `timestamp \n nonce \n method \n path \n hex(sha256(body))`, carried in headers:
//!
//! - `X-YapStack-Timestamp`: unix seconds (±300s replay window)
//! - `X-YapStack-Nonce`: unique per request (single-use, enforced by `admin_nonces`)
//! - `X-YapStack-Signature`: base64 of the 64-byte signature
//!
//! The relay holds only the PUBLIC key; it can verify but never forge. The tenant to
//! write is the `{id}` path segment, which is INSIDE the signed path — so an attacker
//! cannot retarget a captured signature at another tenant. RLS context is set to that
//! signed path tenant (the admin is authorized cross-tenant by signature, not by JWT).

use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method};
use axum::Json;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::Utc;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use yapstack_entitlements::wire::{LimitWire, StateWire, TenantLimitsUpdate, UsageResponse};

use crate::db;
use crate::error::AppError;
use crate::state::AppState;

const REPLAY_WINDOW_SECS: i64 = 300;

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Verify the admin request signature, timestamp window, and single-use nonce.
async fn verify_admin(
    st: &AppState,
    method: &Method,
    path: &str,
    body: &[u8],
    headers: &HeaderMap,
) -> Result<(), AppError> {
    let admin_key = st.admin_key.ok_or(AppError::Forbidden)?;

    let ts = header(headers, "x-yapstack-timestamp")
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or(AppError::Unauthorized)?;
    let nonce = header(headers, "x-yapstack-nonce")
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or(AppError::Unauthorized)?;
    let sig_bytes = header(headers, "x-yapstack-signature")
        .and_then(|s| B64.decode(s).ok())
        .ok_or(AppError::Unauthorized)?;
    let sig: [u8; 64] =
        <[u8; 64]>::try_from(sig_bytes.as_slice()).map_err(|_| AppError::Unauthorized)?;

    // Replay window.
    let now = Utc::now().timestamp();
    if (now - ts).abs() > REPLAY_WINDOW_SECS {
        return Err(AppError::Unauthorized);
    }

    // Canonical signed message.
    let body_hash = hex::encode(Sha256::digest(body));
    let message = format!("{ts}\n{nonce}\n{}\n{path}\n{body_hash}", method.as_str());
    // verify_strict (defense-in-depth): rejects non-canonical / small-order points, so a
    // captured admin signature cannot be malleated into an alternate accepted encoding.
    let vk = VerifyingKey::from_bytes(&admin_key).map_err(|_| AppError::Unauthorized)?;
    let signature = Signature::from_bytes(&sig);
    vk.verify_strict(message.as_bytes(), &signature)
        .map_err(|_| AppError::Unauthorized)?;

    // Single-use nonce (replay guard). admin_nonces is non-RLS; use the pool directly.
    // Opportunistic prune of expired nonces keeps the table bounded.
    let _ = sqlx::query("DELETE FROM admin_nonces WHERE seen_at < now() - interval '10 minutes'")
        .execute(&st.pool)
        .await;
    let inserted =
        sqlx::query("INSERT INTO admin_nonces (nonce) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(nonce)
            .execute(&st.pool)
            .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::Unauthorized); // replayed nonce
    }
    Ok(())
}

fn wire_to_col(w: LimitWire) -> Option<i64> {
    if w.unlimited {
        None
    } else {
        Some(i64::try_from(w.limit).unwrap_or(i64::MAX))
    }
}

fn state_str(s: StateWire) -> &'static str {
    match s {
        StateWire::Active => "active",
        StateWire::Grace => "grace",
        StateWire::Frozen => "frozen",
    }
}

/// `PUT /admin/v1/tenants/{id}/limits` — StoredLimits reads exactly what this writes.
pub async fn put_limits(
    State(st): State<AppState>,
    Path(tenant_id): Path<Uuid>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    verify_admin(&st, &method, uri.path(), &body, &headers).await?;

    let update: TenantLimitsUpdate =
        serde_json::from_slice(&body).map_err(|e| AppError::BadRequest(format!("body: {e}")))?;

    let (warn_storage, warn_upload) = update
        .warn_at
        .map_or((None, None), |w| (w.storage_bytes, w.upload_bytes_period));

    // The admin is authorized for THIS signed path tenant; set the RLS context to it.
    let mut tx = db::begin_tenant_tx(&st.pool, tenant_id).await?;
    sqlx::query(
        "INSERT INTO tenant_limits \
         (tenant_id, plan, storage_bytes, upload_bytes_period, share_count, device_count, \
          period_start, warn_storage, warn_upload, state, grace_until, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11, now()) \
         ON CONFLICT (tenant_id) DO UPDATE SET \
           plan = EXCLUDED.plan, storage_bytes = EXCLUDED.storage_bytes, \
           upload_bytes_period = EXCLUDED.upload_bytes_period, share_count = EXCLUDED.share_count, \
           device_count = EXCLUDED.device_count, period_start = EXCLUDED.period_start, \
           warn_storage = EXCLUDED.warn_storage, warn_upload = EXCLUDED.warn_upload, \
           state = EXCLUDED.state, grace_until = EXCLUDED.grace_until, updated_at = now()",
    )
    .bind(tenant_id)
    .bind(&update.plan)
    .bind(wire_to_col(update.storage_bytes))
    .bind(wire_to_col(update.upload_bytes_period))
    .bind(wire_to_col(update.share_count))
    .bind(wire_to_col(update.device_count))
    .bind(update.period_start)
    .bind(warn_storage)
    .bind(warn_upload)
    .bind(state_str(update.state))
    .bind(update.grace_until)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `GET /admin/v1/tenants/{id}/usage` — open metering; self-hosters get it free.
pub async fn get_usage(
    State(st): State<AppState>,
    Path(tenant_id): Path<Uuid>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Json<UsageResponse>, AppError> {
    verify_admin(&st, &method, uri.path(), b"", &headers).await?;

    let mut tx = db::begin_tenant_tx(&st.pool, tenant_id).await?;
    let row: Option<(i64, i64, i64, i64, chrono::DateTime<Utc>)> = sqlx::query_as(
        "SELECT storage_bytes, upload_bytes_period, share_count, device_count, window_start \
         FROM tenant_usage WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    let resp = match row {
        Some((sb, ub, sc, dc, ws)) => UsageResponse {
            storage_bytes: sb.max(0) as u64,
            upload_bytes_period: ub.max(0) as u64,
            share_count: sc.max(0) as u64,
            device_count: dc.max(0) as u64,
            window_start: ws,
        },
        None => UsageResponse {
            storage_bytes: 0,
            upload_bytes_period: 0,
            share_count: 0,
            device_count: 0,
            window_start: Utc::now(),
        },
    };
    Ok(Json(resp))
}
