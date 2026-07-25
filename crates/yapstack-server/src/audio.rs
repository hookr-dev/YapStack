// SPDX-License-Identifier: AGPL-3.0-only
//! Audio E2E blobs (architecture §9, audio round-trip tranche D6/D8). The relay presigns
//! direct-to-object-storage uploads/downloads and tracks a dedup REFCOUNT; it never sees
//! plaintext, never re-hashes, and no blob bytes flow through it. Presigned URLs are bearer
//! capabilities and are NEVER logged.
//!
//! Dedup is WITHIN a tenant on the CIPHERTEXT hash. Blobs are addressed by the
//! `session_audio_parts` row UUID (`part_id`, D6), because the CRR rebuild strips
//! `UNIQUE(session_id, part_index)` — so `(session_id, part_index)` can collide across
//! devices, but a CSPRNG UUIDv4 row id cannot. `session_id` is kept as blob metadata.
//!
//! ## Refcount invariant (D8, MAPPING-COUNT)
//! `refcount` = the number of `audio_objects` mappings referencing a hash. It is
//! incremented/decremented **exactly on mapping create/repoint**, INDEPENDENT of whether
//! the object bytes exist in storage. A same-part idempotent retry never double-counts.
//!
//! ## Existence-checked presign (D8)
//! The presign row + choke metering are committed BEFORE any bytes land. If the direct PUT
//! then dies, a naive retry would dedup-hit and report `already_exists` with no
//! `upload_url` — the client marks the upload done and the object never exists (silent
//! loss). We therefore add a **metadata-only HEAD** against object storage: it gates ONLY
//! the `already_exists` vs. fresh-`upload_url` decision. Object present → `already_exists`;
//! object absent (row committed but PUT died) → a fresh `upload_url`. Uncertainty (a HEAD
//! error) errs toward re-upload — never a phantom `done`. The HEAD is metadata-only; the
//! relay still never sees blob bytes or plaintext. Choke metering stays FIRST-PRESIGN-ONLY:
//! a D8 re-presign never re-meters (the bytes were reserved when the blob row was created).

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
    /// SHA-256 of the CIPHERTEXT (the whole `audio_blob`), 64 lowercase hex chars.
    pub sha256: String,
    /// Client-declared ciphertext length. PINNED in the presigned policy, so a mismatched
    /// upload is rejected by object storage — the relay never trusts it.
    pub size: u64,
    /// The `session_audio_parts` row UUID this blob belongs to (blob ADDRESS, D6).
    /// Simple-format (32-hex, no dashes) is accepted.
    pub part_id: Uuid,
    /// The session this part belongs to — stored as metadata for future per-session
    /// listing; NOT part of the address.
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

fn upload_response(key: String, url: String, content_length: u64) -> Json<PresignResponse> {
    Json(PresignResponse {
        already_exists: false,
        upload_url: Some(url),
        object_key: key,
        content_length,
    })
}

/// `POST /audio/presign?sha256=&size=&part_id=&session_id=` — content-addressed dedup with
/// the D8 mapping-count refcount and existence-checked `already_exists`. New bytes pass THE
/// choke point exactly once (on first creation of the blob row); a dedup hit or a D8
/// re-presign reserves nothing.
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
        "SELECT ciphertext_sha256 FROM audio_objects WHERE workspace_id = $1 AND part_id = $2",
    )
    .bind(auth.tenant_id)
    .bind(q.part_id)
    .fetch_optional(&mut *tx)
    .await?;
    let old_hash = old_mapping.map(|(h,)| h);
    let same_part_same_blob = old_hash.as_deref() == Some(hash.as_slice());

    // This part is being repointed AWAY from a different blob: drop that blob's ref
    // (mapping-count invariant — decrement on repoint, independent of object existence).
    if let Some(oh) = &old_hash {
        if oh != &hash {
            // released_at (GC): stamp the "became unreferenced at" time exactly on the
            // TRANSITION to refcount <= 0 (`refcount - 1 <= 0` against the OLD value, only
            // when not already released). A blob that stays referenced (refcount still > 0)
            // keeps released_at NULL and is never GC-eligible.
            sqlx::query(
                "UPDATE audio_blobs \
                 SET refcount = GREATEST(0, refcount - 1), \
                     released_at = CASE WHEN refcount - 1 <= 0 AND released_at IS NULL \
                                        THEN now() ELSE released_at END \
                 WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
            )
            .bind(auth.tenant_id)
            .bind(oh)
            .execute(&mut *tx)
            .await?;
        }
    }

    // Point the part at this blob (insert or repoint), recording session_id metadata.
    sqlx::query(
        "INSERT INTO audio_objects (workspace_id, part_id, session_id, ciphertext_sha256) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (workspace_id, part_id) \
         DO UPDATE SET ciphertext_sha256 = EXCLUDED.ciphertext_sha256, \
                       session_id = EXCLUDED.session_id, created_at = now()",
    )
    .bind(auth.tenant_id)
    .bind(q.part_id)
    .bind(q.session_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;

    // `stored_size` is the authoritative pinned length for any (re-)upload. Defaults to the
    // freshly-declared size; overwritten with the metered size on a dedup path.
    let mut stored_size = q.size;
    let mut new_object = false; // we created the blob row → the object cannot exist yet

    if let Some((size,)) = blob {
        stored_size = size.max(0) as u64;
        // A NEW mapping to an existing blob (a different part, or a repoint onto this hash)
        // increments the refcount — even if the OBJECT is still absent. Only the fully
        // idempotent same-part→same-blob retry does not (it never double-counts).
        if !same_part_same_blob {
            // refcount goes UP → the blob is referenced again: clear released_at so a blob
            // that had dipped to <= 0 is no longer GC-eligible (D8 GC invariant).
            sqlx::query(
                "UPDATE audio_blobs SET refcount = refcount + 1, released_at = NULL \
                 WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
            )
            .bind(auth.tenant_id)
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
        }
    } else {
        // NEW blob as this tx saw it. Insert FIRST so two concurrent identical new-blob
        // presigns serialize on the PK: exactly one INSERT wins; the loser degrades to a
        // dedup hit instead of a spurious 500.
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
            // Lost the race: the winner committed this blob. Behave as a dedup hit — add a
            // reference for our mapping, meter NOTHING (no reservation leak).
            let stored: Option<(i64,)> = sqlx::query_as(
                "SELECT size_bytes FROM audio_blobs \
                 WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
            )
            .bind(auth.tenant_id)
            .bind(&hash)
            .fetch_optional(&mut *tx)
            .await?;
            stored_size = stored.map_or(q.size, |(s,)| s.max(0) as u64);
            if !same_part_same_blob {
                // refcount goes UP → clear released_at (see the sibling increment above).
                sqlx::query(
                    "UPDATE audio_blobs SET refcount = refcount + 1, released_at = NULL \
                     WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
                )
                .bind(auth.tenant_id)
                .bind(&hash)
                .execute(&mut *tx)
                .await?;
            }
        } else {
            // We created the blob: meter the declared size once at the choke point. If it
            // rejects, the whole tx (incl. the INSERT) rolls back — no phantom blob, no leak.
            choke::admit(&mut tx, auth.tenant_id, &limits, q.size, &help_url).await?;
            new_object = true;
        }
    }
    tx.commit().await?;

    // Freshly-created object: it cannot exist in storage yet — return an upload URL with the
    // declared size pinned. No HEAD needed.
    if new_object {
        let signed = storage::presign(cfg, "PUT", &key, Some(q.size), chrono::Utc::now());
        return Ok(upload_response(key, signed.url, q.size));
    }

    // Dedup / idempotent / lost-race: the blob ROW exists, but the OBJECT may not (a prior
    // PUT could have died — D8). HEAD object storage to decide. Absent → fresh upload_url
    // (no re-meter); present → already_exists. A HEAD error is treated as absent so we never
    // report a phantom `done`.
    let present = storage::object_exists(cfg, &key).await.unwrap_or_default();
    if present {
        Ok(Json(PresignResponse {
            already_exists: true,
            upload_url: None,
            object_key: key,
            content_length: stored_size,
        }))
    } else {
        let signed = storage::presign(cfg, "PUT", &key, Some(stored_size), chrono::Utc::now());
        Ok(upload_response(key, signed.url, stored_size))
    }
}

/// `GET /audio/part/{part_id}` → 302 to a presigned GET (D6). A READ — never gated. The
/// `part_id` is the `session_audio_parts` row UUID; simple-format (32-hex) is accepted.
pub async fn get_by_part(
    State(st): State<AppState>,
    auth: AuthTenant,
    Path(part_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let cfg = storage_cfg(&st)?;

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT ciphertext_sha256 FROM audio_objects WHERE workspace_id = $1 AND part_id = $2",
    )
    .bind(auth.tenant_id)
    .bind(part_id)
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
