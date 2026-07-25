// SPDX-License-Identifier: AGPL-3.0-only
//! Relay blob garbage collection (hardening item 5).
//!
//! A periodic background sweep deletes audio blobs that are no longer referenced. A blob is
//! eligible when its mapping-count `refcount <= 0` AND it has been unreferenced for longer
//! than the grace period (`released_at < now() - grace`, default 7 days — see
//! [`crate::config::GcConfig`]). `released_at` is stamped by [`crate::audio`] the moment the
//! refcount transitions to `<= 0`, and cleared when the refcount goes back up.
//!
//! ## Two-phase, storage-first ordering (crash safety)
//! For each eligible blob the sweep deletes the STORAGE OBJECT first, then the `audio_blobs`
//! row, IN THAT ORDER. A crash between the two leaves a row without an object — harmless: the
//! next sweep re-deletes it (S3/MinIO DELETE is idempotent, so a re-delete of an already-gone
//! object still succeeds). The REVERSE order (row first) would drop the only pointer to the
//! object and orphan its bytes in storage forever. So: object, then row.
//!
//! ## RLS-preserving cross-tenant scan
//! `audio_blobs` is FORCE-RLS (tenant-scoped on `app.tenant_id`), and the relay connects as
//! the non-owner `yapstack_app` role that can never bypass RLS. The sweep therefore does NOT
//! try to read across tenants directly: it enumerates workspace ids from the non-RLS
//! `workspaces` registry, then scans + deletes each tenant's blobs under that tenant's guard
//! (`begin_tenant_tx`). This keeps the blind-relay isolation invariant intact.
//!
//! ## Blindness / logging
//! The relay still stores no bytes and sees no plaintext. Presigned DELETE URLs are bearer
//! capabilities and are never logged. The per-sweep summary logs COUNTS and total BYTES only
//! — never object keys, hashes, or URLs.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::StorageConfig;
use crate::db;
use crate::state::AppState;
use crate::storage;

/// Delay before the first sweep after boot ("run once shortly after boot then every N").
/// Long enough that short-lived processes (integration tests that mount the router) exit
/// before it ever fires; short enough that a real relay reclaims space the same day it starts.
const INITIAL_DELAY: Duration = Duration::from_secs(60);

/// Outcome of one sweep (a whole pass over every tenant), for the summary log + tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    /// Blobs whose object + row were both deleted.
    pub deleted: u64,
    /// Bytes reclaimed (sum of `size_bytes` of deleted blobs).
    pub bytes: u64,
    /// Eligible blobs skipped this pass because the storage DELETE did not confirm (kept for
    /// the next sweep — never a row deleted without its object gone).
    pub skipped: u64,
}

/// One eligible blob, read under the tenant guard.
struct Eligible {
    hash: Vec<u8>,
    size: i64,
}

/// Run a single sweep across every tenant. Injectable `now` for deterministic tests.
///
/// For each tenant: select the eligible blobs (refcount <= 0 AND released past the grace
/// window), then for each blob delete the object FIRST and — only if that succeeds — delete
/// the row, re-checking eligibility in the DELETE predicate so a blob referenced again between
/// the scan and the delete is left alone.
///
/// # Errors
/// Returns a `sqlx::Error` only if the tenant enumeration itself fails. Per-blob storage
/// errors are counted as `skipped` (retried next sweep), never propagated — one unreachable
/// object must not abort the whole pass.
pub async fn run_sweep(
    pool: &PgPool,
    storage_cfg: &StorageConfig,
    grace: chrono::Duration,
    now: DateTime<Utc>,
) -> Result<SweepStats, sqlx::Error> {
    let cutoff = now - grace;
    let mut stats = SweepStats::default();

    // Enumerate tenants from the non-RLS registry (yapstack_app has SELECT on workspaces).
    let workspaces: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM workspaces")
        .fetch_all(pool)
        .await?;

    for (workspace_id,) in workspaces {
        // Snapshot the eligible set for this tenant under its RLS guard, then release the tx
        // so we never hold a transaction open across the network DELETE.
        let eligible: Vec<Eligible> = {
            let mut tx = db::begin_tenant_tx(pool, workspace_id).await?;
            let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as(
                "SELECT ciphertext_sha256, size_bytes FROM audio_blobs \
                 WHERE workspace_id = $1 AND refcount <= 0 \
                   AND released_at IS NOT NULL AND released_at < $2",
            )
            .bind(workspace_id)
            .bind(cutoff)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            rows.into_iter()
                .map(|(hash, size)| Eligible { hash, size })
                .collect()
        };

        for blob in eligible {
            let key = storage::object_key(workspace_id, &hex::encode(&blob.hash));

            // PHASE 1: delete the object. On any error keep the row and retry next sweep.
            if let Err(e) = storage::delete_object(storage_cfg, &key).await {
                tracing::warn!(
                    workspace = %workspace_id,
                    error = %e,
                    "gc: storage delete failed; keeping row for next sweep"
                );
                stats.skipped += 1;
                continue;
            }

            // PHASE 2: delete the row — re-asserting eligibility so a blob that was referenced
            // again (refcount back up / released_at cleared) after the scan is NOT removed.
            let mut tx = db::begin_tenant_tx(pool, workspace_id).await?;
            let res = sqlx::query(
                "DELETE FROM audio_blobs \
                 WHERE workspace_id = $1 AND ciphertext_sha256 = $2 \
                   AND refcount <= 0 AND released_at IS NOT NULL AND released_at < $3",
            )
            .bind(workspace_id)
            .bind(&blob.hash)
            .bind(cutoff)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;

            if res.rows_affected() > 0 {
                stats.deleted += 1;
                stats.bytes = stats.bytes.saturating_add(blob.size.max(0) as u64);
            }
            // rows_affected == 0 ⇒ re-referenced between scan and delete; the object we just
            // deleted is recovered by the D8 existence-check re-upload path on next presign.
        }
    }

    Ok(stats)
}

/// Spawn the periodic GC background task (wired from [`crate::build_router`]). No-op unless
/// storage is configured (nothing to delete) and `gc.enabled`. Runs once after
/// [`INITIAL_DELAY`], then every `interval_secs`. Owns clones of the pool + config; exits
/// when the runtime shuts down.
pub fn spawn(state: &AppState) {
    let gc = state.config.gc.resolved();
    if !gc.enabled {
        tracing::info!("relay blob GC disabled (gc.enabled = false)");
        return;
    }
    let Some(storage_cfg) = state.config.storage.clone() else {
        // No object storage ⇒ no blobs to reclaim. Nothing to do.
        return;
    };
    let pool = state.pool.clone();
    let grace = gc.grace();
    let interval = Duration::from_secs(gc.interval_secs.max(1));

    tokio::spawn(async move {
        tracing::info!(
            interval_secs = gc.interval_secs,
            grace_secs = gc.grace_secs,
            "relay blob GC armed"
        );
        tokio::time::sleep(INITIAL_DELAY).await;
        let mut ticker = tokio::time::interval(interval);
        // Skip the immediate first tick that `interval` fires at t=0; we already slept.
        ticker.tick().await;
        loop {
            match run_sweep(&pool, &storage_cfg, grace, Utc::now()).await {
                Ok(stats) => tracing::info!(
                    deleted = stats.deleted,
                    bytes = stats.bytes,
                    skipped = stats.skipped,
                    "relay blob GC sweep complete"
                ),
                Err(e) => tracing::error!(error = %e, "relay blob GC sweep failed"),
            }
            ticker.tick().await;
        }
    });
}
