// SPDX-License-Identifier: AGPL-3.0-only
//! `GET /sync/completeness` — anti-entropy on opaque blobs (architecture §7b).
//!
//! The relay verifies COMPLETENESS without reading content: per tenant it exposes
//! `(max_changeset_seq, count, contiguous?)`. Because the cursor is dense and starts at
//! 1, `contiguous == (max == count)` — pure arithmetic; a client nightly checks it
//! holds every seq up to `max` and re-pulls any gap.
//!
//! Advisory A3: the response ALSO returns, per `client_id`, the max `client_seq` the
//! server holds. A client compares this against the `client_seq` it last pushed from
//! that device; a shortfall means a (malicious) relay silently DROPPED that device's
//! stream tail — which the tenant-wide `changeset_seq` contiguity report alone cannot
//! reveal (dropping the newest tail keeps the remaining prefix contiguous). NEVER gated.

use axum::extract::State;
use axum::Json;
use uuid::Uuid;
use yapstack_common::sync::{ClientTail, CompletenessResponse};

use crate::db;
use crate::error::AppError;
use crate::extract::AuthTenant;
use crate::state::AppState;

pub async fn completeness(
    State(st): State<AppState>,
    auth: AuthTenant,
) -> Result<Json<CompletenessResponse>, AppError> {
    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;

    let (max_seq, count): (Option<i64>, i64) = sqlx::query_as(
        "SELECT MAX(changeset_seq), COUNT(*) FROM changesets WHERE workspace_id = $1",
    )
    .bind(auth.tenant_id)
    .fetch_one(&mut *tx)
    .await?;

    let per_client_rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT client_id, MAX(client_seq) FROM changesets \
         WHERE workspace_id = $1 GROUP BY client_id ORDER BY client_id",
    )
    .bind(auth.tenant_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let max_changeset_seq = max_seq.unwrap_or(0);
    // Dense from 1 ⇒ contiguous iff the highest seq equals the row count.
    let contiguous = max_changeset_seq == count;

    let per_client = per_client_rows
        .into_iter()
        .map(|(client_id, max_client_seq)| ClientTail {
            client_id,
            max_client_seq,
        })
        .collect();

    Ok(Json(CompletenessResponse {
        max_changeset_seq,
        count,
        contiguous,
        per_client,
    }))
}
