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
    let generation = i64::try_from(req.generation).unwrap_or(i64::MAX);

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;

    // A row for this generation may already exist. Read its hash AND the size we metered.
    let existing: Option<(Vec<u8>, i64)> = sqlx::query_as(
        "SELECT ciphertext_sha256, size_bytes FROM snapshots \
         WHERE workspace_id = $1 AND generation = $2",
    )
    .bind(auth.tenant_id)
    .bind(generation)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((existing_hash, existing_size)) = existing {
        // Release the read tx before the network HEAD; any write path below reopens one.
        tx.commit().await?;

        // HEAD object storage to tell a genuinely-published snapshot apart from a row whose
        // direct PUT died. Uncertainty (a HEAD error) errs toward absent → re-upload, never
        // a phantom `done`.
        let existing_key =
            storage::snapshot_object_key(auth.tenant_id, &hex::encode(&existing_hash));
        let existing_present = storage::object_exists(cfg, &existing_key)
            .await
            .unwrap_or_default();

        let existing_size = existing_size.max(0) as u64;

        if existing_hash == hash {
            if existing_present {
                // Idempotent: the exact generation+hash is already published.
                return Ok(Json(SnapshotPresignResponse {
                    already_exists: true,
                    upload_url: None,
                    object_key: key,
                }));
            }
            // Row committed but the PUT died: hand back a fresh upload_url. Pin the SIZE we
            // already metered (mirror `audio::presign`), never the newly-declared one — the
            // hash is unchanged, so accepting a larger declared size would store unmetered
            // bytes under the same content-addressed key.
            let signed =
                storage::presign(cfg, "PUT", &key, Some(existing_size), chrono::Utc::now());
            return Ok(Json(SnapshotPresignResponse {
                already_exists: false,
                upload_url: Some(signed.url),
                object_key: key,
            }));
        }

        // Hash CHANGE for the same generation.
        if existing_present {
            // A genuinely-published snapshot already occupies this generation.
            return Err(AppError::Conflict(
                "snapshot generation already published with a different hash".into(),
            ));
        }

        // Old object absent (dead PUT + fresh-key reseal retry, crypto.rs:53-62): re-point
        // the row at the new hash/size so the seed can finish publishing this generation.
        // Meter only the POSITIVE delta over the size already reserved — the resealed
        // ciphertext can be larger, and re-signing a bigger PUT with no metering is a quota
        // bypass. The UPDATE is a COMPARE-AND-SWAP on the hash we read: if a concurrent
        // retry (or a raced publish) advanced this row between our read and write, we must
        // NOT blind-overwrite it (that could orphan a valid object and point the row at a
        // missing one) — we roll back our metering and report a conflict instead.
        let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;
        let delta = req.size.saturating_sub(existing_size);
        choke::admit(&mut tx, auth.tenant_id, &limits, delta, &help_url).await?;
        let updated = sqlx::query(
            "UPDATE snapshots \
             SET ciphertext_sha256 = $3, size_bytes = $4, baseline_seq = $5 \
             WHERE workspace_id = $1 AND generation = $2 AND ciphertext_sha256 = $6",
        )
        .bind(auth.tenant_id)
        .bind(generation)
        .bind(&hash)
        .bind(i64::try_from(req.size).unwrap_or(i64::MAX))
        .bind(req.baseline_seq)
        .bind(&existing_hash)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            // The row no longer holds the hash we read. Undo the delta reservation and
            // return a conflict; the client re-reads and retries against current state.
            tx.rollback().await?;
            return Err(AppError::Conflict(
                "snapshot generation concurrently updated; retry".into(),
            ));
        }
        tx.commit().await?;

        let signed = storage::presign(cfg, "PUT", &key, Some(req.size), chrono::Utc::now());
        return Ok(Json(SnapshotPresignResponse {
            already_exists: false,
            upload_url: Some(signed.url),
            object_key: key,
        }));
    }

    // Record the new snapshot generation, then meter its bytes at the choke point. If
    // the choke rejects, the whole tx (including this INSERT) rolls back — no phantom.
    sqlx::query(
        "INSERT INTO snapshots \
         (workspace_id, generation, ciphertext_sha256, size_bytes, baseline_seq) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(auth.tenant_id)
    .bind(generation)
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

    // Existence-check before advertising present:true (D8, mirror `audio`). A row whose
    // direct PUT died would otherwise hand a joining device a download_url that 404s,
    // failing bootstrap hard; report present:false so the client degrades to changeset
    // replay. Uncertainty (a HEAD error) errs toward absent for the same reason.
    if !storage::object_exists(cfg, &key).await.unwrap_or_default() {
        return Ok(Json(SnapshotHeadResponse {
            present: false,
            generation: 0,
            baseline_seq: 0,
            download_url: None,
        }));
    }

    let signed = storage::presign(cfg, "GET", &key, None, chrono::Utc::now());
    Ok(Json(SnapshotHeadResponse {
        present: true,
        generation: generation.max(0) as u64,
        baseline_seq,
        download_url: Some(signed.url),
    }))
}
