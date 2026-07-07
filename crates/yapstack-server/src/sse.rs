// SPDX-License-Identifier: AGPL-3.0-only
//! In-process SSE wakeup hub (architecture §7a "Live push").
//!
//! **Wakeup-only, by contract.** Events carry ONLY the tenant's latest `changeset_seq`
//! — never ciphertext, never a value a client could treat as authoritative. SSE MUST
//! NOT be usable to advance a client's cursor: it is a hint to *pull*, and `GET
//! /sync/pull` remains the sole source of truth. A malicious or lossy stream can at
//! worst fail to wake a client (which the completeness endpoint / periodic pull covers),
//! never corrupt its cursor.
//!
//! Single-process only: a `tokio::sync::broadcast` channel per tenant. At horizontal
//! scale, cross-app-server fan-out needs Postgres `LISTEN/NOTIFY` (or Redis) — noted
//! here, not wired in v1.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;
use uuid::Uuid;

const CHANNEL_CAPACITY: usize = 64;

/// Per-tenant broadcast of the latest committed `changeset_seq`.
pub struct SseHub {
    channels: Mutex<HashMap<Uuid, broadcast::Sender<i64>>>,
}

impl Default for SseHub {
    fn default() -> Self {
        Self::new()
    }
}

impl SseHub {
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
        }
    }

    fn sender(&self, tenant: Uuid) -> broadcast::Sender<i64> {
        let mut map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(tenant)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// Subscribe a streaming client to a tenant's wakeups.
    #[must_use]
    pub fn subscribe(&self, tenant: Uuid) -> broadcast::Receiver<i64> {
        self.sender(tenant).subscribe()
    }

    /// Publish a wakeup after a successful push commit. Best-effort: if no subscribers,
    /// the send is a no-op. NEVER carries content.
    pub fn wake(&self, tenant: Uuid, changeset_seq: i64) {
        let _ = self.sender(tenant).send(changeset_seq);
    }
}
