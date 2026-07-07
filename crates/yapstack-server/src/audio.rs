// SPDX-License-Identifier: AGPL-3.0-only
//! Audio E2E blobs (architecture §9). The relay presigns direct-to-object-storage
//! uploads/downloads and tracks a dedup REFCOUNT; it never sees plaintext, never
//! re-hashes, and no bytes flow through it. Presigned URLs are bearer capabilities and
//! are NEVER logged.
//!
//! Dedup is WITHIN a tenant on the CIPHERTEXT hash. A refcount (`audio_blobs.refcount`)
//! counts how many sessions reference a blob so soft-delete GC (a T010+ stub) won't
//! orphan a blob another session still shares.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;
use yapstack_common::sync::PresignResponse;

use crate::choke;
use crate::config::StorageConfig;
use crate::db;
use crate::error::AppError;
use crate::extract::AuthTenant;
use crate::state::AppState;
use crate::storage;

#[derive(Debug, Deserialize)]
pub struct PresignQuery {
    /// SHA-256 of the CIPHERTEXT, 64 lowercase hex chars.
    pub sha256: String,
    /// Client-declared ciphertext length. PINNED in the presigned policy, so a
    /// mismatched upload is rejected by object storage — the relay never trusts it.
    pub size: u64,
    /// The session this blob belongs to (maps session → blob for GET).
    pub session_id: Uuid,
}

fn parse_sha256(s: &str) -> Result<(Vec<u8>, String), AppError> {
    let lower = s.to_ascii_lowercase();
    if lower.len() != 64 || !lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest("sha256: expected 64 hex chars".into()));
    }
    let bytes =
        hex::decode(&lower).map_err(|_| AppError::BadRequest("sha256: invalid hex".into()))?;
    Ok((bytes, lower))
}

fn storage_cfg(st: &AppState) -> Result<&StorageConfig, AppError> {
    st.config
        .storage
        .as_ref()
        .ok_or_else(|| AppError::Unavailable("audio storage not configured".into()))
}

/// `POST /audio/presign?sha256=&size=&session_id=` — passes through THE choke point
/// before accepting any new bytes; a dedup hit reserves nothing (bytes already stored).
pub async fn presign(
    State(st): State<AppState>,
    auth: AuthTenant,
    Query(q): Query<PresignQuery>,
) -> Result<Json<PresignResponse>, AppError> {
    let cfg = storage_cfg(&st)?;
    let (hash, hash_hex) = parse_sha256(&q.sha256)?;
    let key = storage::object_key(auth.tenant_id, &hash_hex);

    let limits = st.limits.limits(auth.tenant_id).await;
    let help_url = st.config.help_url();

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;

    let blob: Option<(i64,)> = sqlx::query_as(
        "SELECT size_bytes FROM audio_blobs WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
    )
    .bind(auth.tenant_id)
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;

    let old_mapping: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT ciphertext_sha256 FROM audio_objects WHERE workspace_id = $1 AND session_id = $2",
    )
    .bind(auth.tenant_id)
    .bind(q.session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let old_hash = old_mapping.map(|(h,)| h);

    // Fully idempotent: this session already points at this exact blob.
    if let (Some((stored_size,)), Some(oh)) = (&blob, &old_hash) {
        if *oh == hash {
            tx.commit().await?;
            return Ok(Json(PresignResponse {
                already_exists: true,
                upload_url: None,
                object_key: key,
                content_length: (*stored_size).max(0) as u64,
            }));
        }
    }

    // The session is being (re)pointed away from a different blob: drop that ref.
    if let Some(oh) = &old_hash {
        if oh != &hash {
            sqlx::query(
                "UPDATE audio_blobs SET refcount = GREATEST(0, refcount - 1) \
                 WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
            )
            .bind(auth.tenant_id)
            .bind(oh)
            .execute(&mut *tx)
            .await?;
        }
    }

    // Point the session at the new blob (insert or repoint).
    sqlx::query(
        "INSERT INTO audio_objects (workspace_id, session_id, ciphertext_sha256) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id, session_id) \
         DO UPDATE SET ciphertext_sha256 = EXCLUDED.ciphertext_sha256, created_at = now()",
    )
    .bind(auth.tenant_id)
    .bind(q.session_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;

    if let Some((stored_size,)) = blob {
        // DEDUP HIT: the ciphertext is already stored. Add a reference; meter NOTHING.
        sqlx::query(
            "UPDATE audio_blobs SET refcount = refcount + 1 \
             WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
        )
        .bind(auth.tenant_id)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(Json(PresignResponse {
            already_exists: true,
            upload_url: None,
            object_key: key,
            content_length: stored_size.max(0) as u64,
        }));
    }

    // NEW blob (as this tx saw it). Insert FIRST so two concurrent identical new-blob
    // presigns serialize on the PK: exactly one INSERT wins; the loser sees the
    // conflict and degrades to a dedup hit instead of a spurious 500.
    let inserted = sqlx::query(
        "INSERT INTO audio_blobs (workspace_id, ciphertext_sha256, size_bytes, refcount) \
         VALUES ($1, $2, $3, 1) \
         ON CONFLICT (workspace_id, ciphertext_sha256) DO NOTHING",
    )
    .bind(auth.tenant_id)
    .bind(&hash)
    .bind(i64::try_from(q.size).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;

    if inserted.rows_affected() == 0 {
        // Lost the race: the winning tx committed this blob. Behave as a DEDUP HIT —
        // add a reference, meter NOTHING (no choke → no reservation leak), and point
        // the client at the existing object.
        let stored: Option<(i64,)> = sqlx::query_as(
            "SELECT size_bytes FROM audio_blobs \
             WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
        )
        .bind(auth.tenant_id)
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE audio_blobs SET refcount = refcount + 1 \
             WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
        )
        .bind(auth.tenant_id)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(Json(PresignResponse {
            already_exists: true,
            upload_url: None,
            object_key: key,
            content_length: stored.map_or(0, |(s,)| s.max(0) as u64),
        }));
    }

    // We created the blob: meter the declared size at the choke point. If it rejects,
    // the whole tx — including the INSERT above — rolls back (no phantom blob, no leak).
    choke::admit(&mut tx, auth.tenant_id, &limits, q.size, &help_url).await?;
    tx.commit().await?;

    // Presigned PUT with content-length PINNED to the declared size.
    let signed = storage::presign(cfg, "PUT", &key, Some(q.size), chrono::Utc::now());
    Ok(Json(PresignResponse {
        already_exists: false,
        upload_url: Some(signed.url),
        object_key: key,
        content_length: q.size,
    }))
}

/// `GET /audio/{session_id}` → 302 to a presigned GET. A READ — never gated.
pub async fn get(
    State(st): State<AppState>,
    auth: AuthTenant,
    Path(session_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let cfg = storage_cfg(&st)?;

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT ciphertext_sha256 FROM audio_objects WHERE workspace_id = $1 AND session_id = $2",
    )
    .bind(auth.tenant_id)
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    let Some((hash,)) = row else {
        return Err(AppError::NotFound);
    };
    let key = storage::object_key(auth.tenant_id, &hex::encode(hash));
    let signed = storage::presign(cfg, "GET", &key, None, chrono::Utc::now());

    // 302 redirect. The Location is a bearer capability — do NOT log it.
    Ok((StatusCode::FOUND, [(header::LOCATION, signed.url)], "").into_response())
}
