// SPDX-License-Identifier: AGPL-3.0-only
//! `POST /sync/push` — accept a drained outbox batch (architecture §7a).
//!
//! ## The commit-ordered sequence primitive (LOAD-BEARING, §7a)
//!
//! `changeset_seq` is assigned from a PER-TENANT COUNTER ROW (`tenant_seq`), NEVER a
//! bigserial. Bigserial (or any sequence) assigns numbers at statement time and leaves
//! HOLES when a transaction rolls back — a reader using a snapshot cursor then silently
//! skips a hole that later fills, losing data. The counter row fixes this:
//!
//! 1. Bootstrap the tenant's `tenant_seq` row (idempotent).
//! 2. `SELECT next_seq ... FOR UPDATE` — this ROW LOCK is the per-tenant push
//!    serialization point; it is held until COMMIT, so concurrent pushers for the same
//!    tenant are ordered and the idempotency check below is race-free.
//! 3. Resolve idempotency `(client_id, client_seq)` → existing seq, under the lock.
//! 4. Choke point (bytes) — atomic check-and-reserve.
//! 5. Assign the NEW changes contiguous seqs `[next .. next+m-1]` and INSERT them.
//! 6. `UPDATE tenant_seq SET next_seq = next_seq + m` — the counter is only ADVANCED
//!    (numbers consumed) right before commit; a rollback un-reserves them. Dense,
//!    gap-free, and monotonic with COMMIT order.
//!
//! Because the seqs are dense (start at 1, no holes), a client's gap detection is pure
//! arithmetic and its cursor is loss-free.

use std::collections::HashMap;

use axum::extract::State;
use axum::Json;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use uuid::Uuid;
use yapstack_common::sync::{PushAck, PushRequest, PushResponse, MAX_PUSH_BYTES, MAX_PUSH_CHANGES};

use crate::choke;
use crate::db;
use crate::error::AppError;
use crate::extract::{AuthTenant, ClientIp};
use crate::state::AppState;

pub async fn push(
    State(st): State<AppState>,
    auth: AuthTenant,
    ClientIp(ip): ClientIp,
    Json(req): Json<PushRequest>,
) -> Result<Json<PushResponse>, AppError> {
    // Guardrail: per-(tenant, ip) rate limit (architecture §10).
    if !st.ratelimit.check(auth.tenant_id, &ip) {
        return Err(AppError::RateLimited);
    }

    // Guardrail: max batch (architecture §10).
    if req.changes.len() > MAX_PUSH_CHANGES {
        return Err(AppError::BadRequest(format!(
            "batch exceeds {MAX_PUSH_CHANGES} changes"
        )));
    }
    if req.changes.is_empty() {
        return Ok(Json(PushResponse::default()));
    }

    // Decode ciphertext up front (opaque bytea; never inspected). Enforce the byte cap.
    let mut decoded: Vec<Vec<u8>> = Vec::with_capacity(req.changes.len());
    let mut total_bytes: usize = 0;
    for (i, c) in req.changes.iter().enumerate() {
        let bytes = B64
            .decode(&c.ciphertext)
            .map_err(|_| AppError::BadRequest(format!("change[{i}].ciphertext: invalid base64")))?;
        total_bytes = total_bytes.saturating_add(bytes.len());
        decoded.push(bytes);
    }
    if total_bytes > MAX_PUSH_BYTES {
        return Err(AppError::BadRequest(format!(
            "batch exceeds {MAX_PUSH_BYTES} bytes"
        )));
    }

    let limits = st.limits.limits(auth.tenant_id).await;
    let help_url = st.config.help_url();

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;

    // (1) Bootstrap + (2) acquire the per-tenant push lock (held until commit).
    sqlx::query("INSERT INTO tenant_seq (workspace_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(auth.tenant_id)
        .execute(&mut *tx)
        .await?;
    let (mut next_seq,): (i64,) =
        sqlx::query_as("SELECT next_seq FROM tenant_seq WHERE workspace_id = $1 FOR UPDATE")
            .bind(auth.tenant_id)
            .fetch_one(&mut *tx)
            .await?;

    // (3) Idempotency: which (client_id, client_seq) pairs already exist, and with what
    // stored ciphertext? Race-free under the lock. One query via parallel arrays. We fetch
    // the stored ciphertext too so the dedup branch can tell a benign retry (identical bytes)
    // from a counter REGRESSION (a DB restore re-mints a reused seq with DIFFERENT content —
    // G3): the latter must be rejected loudly, never silently deduplicated.
    let client_ids: Vec<Uuid> = req.changes.iter().map(|c| c.client_id).collect();
    let client_seqs: Vec<i64> = req.changes.iter().map(|c| c.client_seq).collect();
    let existing_rows: Vec<(Uuid, i64, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT client_id, client_seq, changeset_seq, ciphertext FROM changesets \
         WHERE workspace_id = $1 \
           AND (client_id, client_seq) IN \
               (SELECT * FROM unnest($2::uuid[], $3::bigint[]))",
    )
    .bind(auth.tenant_id)
    .bind(&client_ids)
    .bind(&client_seqs)
    .fetch_all(&mut *tx)
    .await?;
    let mut existing: HashMap<(Uuid, i64), (i64, Vec<u8>)> = existing_rows
        .into_iter()
        .map(|(cid, cseq, seq, ct)| ((cid, cseq), (seq, ct)))
        .collect();

    // (5-prep) Walk the batch in order, assigning seqs to genuinely-new pairs. Also
    // dedup WITHIN the batch (a repeated pair gets the same ack).
    let mut acks: Vec<PushAck> = Vec::with_capacity(req.changes.len());
    let mut new_rows: Vec<(i64, Uuid, i64, Vec<u8>, i32, i32)> = Vec::new();
    let mut added_bytes: u64 = 0;
    for (c, bytes) in req.changes.iter().zip(decoded) {
        let key = (c.client_id, c.client_seq);
        if let Some((seq, stored_ct)) = existing.get(&key) {
            // G3 — the pair already exists. IDENTICAL ciphertext is a benign idempotent retry
            // (dedup ack, unchanged behavior). DIFFERENT ciphertext can NEVER be a retry: the
            // same (client_id, client_seq) is carrying new content, which only happens when a
            // client's counter REGRESSED (a DB restore) and re-minted a reused seq over fresh
            // data. Silently deduplicating it would swallow that write (the confirmed bug), so
            // reject the whole batch with a 409 and let the client fail loudly and reseed.
            // Never log or echo ciphertext.
            if stored_ct.as_slice() != bytes.as_slice() {
                return Err(AppError::Conflict(format!(
                    "client_seq regression: (client_id, client_seq)=({}, {}) already stored \
                     with different content; the client counter regressed and must reseed",
                    c.client_id, c.client_seq
                )));
            }
            acks.push(PushAck {
                client_id: c.client_id,
                client_seq: c.client_seq,
                changeset_seq: *seq,
                deduplicated: true,
            });
            continue;
        }
        let seq = next_seq;
        next_seq += 1;
        added_bytes = added_bytes.saturating_add(bytes.len() as u64);
        new_rows.push((
            seq,
            c.client_id,
            c.client_seq,
            bytes.clone(),
            c.schema_version,
            c.engine_version,
        ));
        existing.insert(key, (seq, bytes)); // catch intra-batch repeats (with content)
        acks.push(PushAck {
            client_id: c.client_id,
            client_seq: c.client_seq,
            changeset_seq: seq,
            deduplicated: false,
        });
    }

    let m = new_rows.len() as i64;

    // (4) THE CHOKE POINT — meter only genuinely-new bytes. On breach this errors and
    // the whole transaction (including the seq reservation) rolls back: no gap.
    if m > 0 {
        choke::admit(&mut tx, auth.tenant_id, &limits, added_bytes, &help_url).await?;
    }

    // (5) Insert the new changesets with their assigned dense seqs.
    for (seq, cid, cseq, ct, sv, ev) in &new_rows {
        sqlx::query(
            "INSERT INTO changesets \
             (workspace_id, changeset_seq, client_id, client_seq, ciphertext, schema_version, engine_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(auth.tenant_id)
        .bind(seq)
        .bind(cid)
        .bind(cseq)
        .bind(ct)
        .bind(sv)
        .bind(ev)
        .execute(&mut *tx)
        .await?;
    }

    // (6) Advance the counter LAST (numbers consumed at commit).
    if m > 0 {
        sqlx::query("UPDATE tenant_seq SET next_seq = next_seq + $2 WHERE workspace_id = $1")
            .bind(auth.tenant_id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    // next_seq now points one past the last assigned seq.
    let max_changeset_seq = next_seq - 1;
    if m > 0 {
        st.sse.wake(auth.tenant_id, max_changeset_seq);
    }

    Ok(Json(PushResponse {
        acks,
        max_changeset_seq,
    }))
}
