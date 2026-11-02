// SPDX-License-Identifier: AGPL-3.0-only
//! `GET /sync/pull?since=&limit=` — the SOURCE OF TRUTH for cursor advancement
//! (SYNC_SPEC §6.4, §9.1). Returns opaque ciphertext blobs after the cursor in dense
//! commit order. NEVER gated by entitlements (a read must never be locked out). It IS
//! gated by sync capability (ADR 0006 §5): a client below the workspace floor would wedge
//! on rows for tables it doesn't have, so it is refused with 426 "update required".

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
use yapstack_common::sync::{PullResponse, PulledChange};

use crate::error::AppError;
use crate::extract::AuthTenant;
use crate::state::AppState;
use crate::sync::capability;

const DEFAULT_LIMIT: i64 = 500;
const MAX_LIMIT: i64 = 2000;

#[derive(Debug, Deserialize)]
pub struct PullQuery {
    /// Exclusive cursor: return changesets with `changeset_seq > since`. Default 0.
    #[serde(default)]
    pub since: i64,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// One stored changeset: `(changeset_seq, client_id, client_seq, ciphertext,
/// schema_version, engine_version)`.
pub type ChangesetRow = (i64, Uuid, i64, Vec<u8>, i32, i32);

/// Read up to `limit` changesets after `since`, in commit order, inside the caller's
/// gated transaction.
///
/// # Errors
/// A DB error.
pub async fn read_changes(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    since: i64,
    limit: i64,
) -> Result<Vec<ChangesetRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT changeset_seq, client_id, client_seq, ciphertext, schema_version, engine_version \
         FROM changesets \
         WHERE workspace_id = $1 AND changeset_seq > $2 \
         ORDER BY changeset_seq ASC \
         LIMIT $3",
    )
    .bind(tenant)
    .bind(since)
    .bind(limit)
    .fetch_all(&mut **tx)
    .await
}

pub async fn pull(
    State(st): State<AppState>,
    auth: AuthTenant,
    headers: HeaderMap,
    Query(q): Query<PullQuery>,
) -> Result<Json<PullResponse>, AppError> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    // The floor row stays locked FOR SHARE until commit, so a concurrent raise can't land
    // between the level check and the read below.
    let (mut tx, levels) = capability::begin_gated_read(&st.pool, &auth, &headers).await?;
    // Fetch one extra row to compute has_more without a second COUNT query.
    let rows = read_changes(&mut tx, auth.tenant_id, q.since, limit + 1).await?;
    tx.commit().await?;

    let has_more = rows.len() as i64 > limit;
    let changes: Vec<PulledChange> = rows
        .into_iter()
        .take(limit as usize)
        .map(|(seq, cid, cseq, ct, sv, ev)| PulledChange {
            changeset_seq: seq,
            client_id: cid,
            client_seq: cseq,
            ciphertext: B64.encode(ct),
            schema_version: sv,
            engine_version: ev,
        })
        .collect();

    // next_seq is the last returned seq (dense), or the caller's cursor when empty.
    let next_seq = changes.last().map_or(q.since, |c| c.changeset_seq);

    Ok(Json(PullResponse {
        changes,
        next_seq,
        has_more,
        required_sync_capability: levels.required_sync_capability,
        requested_sync_capability: levels.requested_sync_capability,
    }))
}
