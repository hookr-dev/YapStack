// SPDX-License-Identifier: AGPL-3.0-only
//! `GET /sync/pull?since=&limit=` — the SOURCE OF TRUTH for cursor advancement
//! (architecture §7a). Returns opaque ciphertext blobs after the cursor in dense
//! commit order. NEVER gated by entitlements (a read must never be locked out).

use axum::extract::{Query, State};
use axum::Json;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use uuid::Uuid;
use yapstack_common::sync::{PullResponse, PulledChange};

use crate::db;
use crate::error::AppError;
use crate::extract::AuthTenant;
use crate::state::AppState;

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

pub async fn pull(
    State(st): State<AppState>,
    auth: AuthTenant,
    Query(q): Query<PullQuery>,
) -> Result<Json<PullResponse>, AppError> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;
    // Fetch one extra row to compute has_more without a second COUNT query.
    let rows: Vec<(i64, Uuid, i64, Vec<u8>, i32, i32)> = sqlx::query_as(
        "SELECT changeset_seq, client_id, client_seq, ciphertext, schema_version, engine_version \
         FROM changesets \
         WHERE workspace_id = $1 AND changeset_seq > $2 \
         ORDER BY changeset_seq ASC \
         LIMIT $3",
    )
    .bind(auth.tenant_id)
    .bind(q.since)
    .bind(limit + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let has_more = rows.len() as i64 > limit;
    let mut changes: Vec<PulledChange> = rows
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
    // Truncate defensively in case take() kept the +1 (it won't, but be explicit).
    changes.truncate(limit as usize);

    Ok(Json(PullResponse {
        changes,
        next_seq,
        has_more,
    }))
}
