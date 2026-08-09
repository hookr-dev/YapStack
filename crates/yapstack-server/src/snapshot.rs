// SPDX-License-Identifier: AGPL-3.0-only
//! R2 snapshot bootstrap (blind). A SEED device publishes its whole library ONCE as an
//! encrypted, compact DB snapshot so a JOIN device can bootstrap from it instead of
//! replaying ~434k per-cell changesets. The relay treats the snapshot EXACTLY like an
//! audio blob (§9): it is content-addressed by its CIPHERTEXT SHA-256, presigned
//! directly to object storage, and the relay never sees the bytes, never decrypts, and
//! never re-hashes. This module stores only opaque metadata (which generation exists,
//! its hash/size, and the `changeset_seq` baseline the join resumes incremental pull
//! from). It is the clean reuse of the audio blob-store mechanics; no bytes flow
//! through the relay.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::choke;
use crate::db;
use crate::error::AppError;
use crate::extract::AuthTenant;
use crate::state::{storage_cfg, AppState};
use crate::storage;

#[derive(Debug, Deserialize)]
pub struct SnapshotPresignRequest {
    /// SHA-256 of the CIPHERTEXT, 64 lowercase hex chars.
    pub sha256: String,
    /// Client-declared ciphertext length; PINNED in the presigned policy.
    pub size: u64,
    /// Monotonic snapshot generation (opaque routing metadata).
    pub generation: u64,
    /// The `changeset_seq` the seed had synced through when it produced this snapshot.
    pub baseline_seq: i64,
}

#[derive(Debug, Serialize)]
pub struct SnapshotPresignResponse {
    pub already_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_url: Option<String>,
    pub object_key: String,
}

/// `POST /snapshot/presign` — record the snapshot generation and hand back a presigned
/// PUT so the seed uploads the ciphertext directly to object storage. Passes THE choke
/// point before accepting new bytes (self-host `AllowAll` never gates; hosted meters
/// storage like any other upload). A repeated identical hash+generation is idempotent.
pub async fn presign(
    State(st): State<AppState>,
    auth: AuthTenant,
    Json(req): Json<SnapshotPresignRequest>,
) -> Result<Json<SnapshotPresignResponse>, AppError> {
    let cfg = storage_cfg(&st)?;
    let (hash, hash_hex) = storage::parse_sha256(&req.sha256)?;
    let key = storage::snapshot_object_key(auth.tenant_id, &hash_hex);

    let limits = st.limits.limits(auth.tenant_id).await;
    let help_url = st.config.help_url();

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;

    // Idempotent: this exact generation+hash was already recorded.
    let existing: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT ciphertext_sha256 FROM snapshots \
         WHERE workspace_id = $1 AND generation = $2",
    )
    .bind(auth.tenant_id)
    .bind(i64::try_from(req.generation).unwrap_or(i64::MAX))
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((existing_hash,)) = existing {
        if existing_hash == hash {
            tx.commit().await?;
            return Ok(Json(SnapshotPresignResponse {
                already_exists: true,
                upload_url: None,
                object_key: key,
            }));
        }
        return Err(AppError::Conflict(
            "snapshot generation already published with a different hash".into(),
        ));
    }

    // Record the new snapshot generation, then meter its bytes at the choke point. If
    // the choke rejects, the whole tx (including this INSERT) rolls back — no phantom.
    sqlx::query(
        "INSERT INTO snapshots \
         (workspace_id, generation, ciphertext_sha256, size_bytes, baseline_seq) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(auth.tenant_id)
    .bind(i64::try_from(req.generation).unwrap_or(i64::MAX))
    .bind(&hash)
    .bind(i64::try_from(req.size).unwrap_or(i64::MAX))
    .bind(req.baseline_seq)
    .execute(&mut *tx)
    .await?;

    choke::admit(&mut tx, auth.tenant_id, &limits, req.size, &help_url).await?;
    tx.commit().await?;

    let signed = storage::presign(cfg, "PUT", &key, Some(req.size), chrono::Utc::now());
    Ok(Json(SnapshotPresignResponse {
        already_exists: false,
        upload_url: Some(signed.url),
        object_key: key,
    }))
}

#[derive(Debug, Serialize)]
pub struct SnapshotHeadResponse {
    pub present: bool,
    pub generation: u64,
    pub baseline_seq: i64,
    /// Presigned GET for the latest snapshot ciphertext (a bearer capability — NEVER
    /// logged). `None` when no snapshot exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
}

/// `GET /snapshot` — return the latest snapshot's metadata + a presigned GET. A READ:
/// never gated (a bootstrapping device must never be locked out of its own data).
pub async fn head(
    State(st): State<AppState>,
    auth: AuthTenant,
) -> Result<Json<SnapshotHeadResponse>, AppError> {
    let cfg = storage_cfg(&st)?;

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;
    let row: Option<(i64, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT generation, ciphertext_sha256, baseline_seq FROM snapshots \
         WHERE workspace_id = $1 ORDER BY generation DESC LIMIT 1",
    )
    .bind(auth.tenant_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    let Some((generation, hash, baseline_seq)) = row else {
        return Ok(Json(SnapshotHeadResponse {
            present: false,
            generation: 0,
            baseline_seq: 0,
            download_url: None,
        }));
    };
    let key = storage::snapshot_object_key(auth.tenant_id, &hex::encode(hash));
    let signed = storage::presign(cfg, "GET", &key, None, chrono::Utc::now());
    Ok(Json(SnapshotHeadResponse {
        present: true,
        generation: generation.max(0) as u64,
        baseline_seq,
        download_url: Some(signed.url),
    }))
}
