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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    /// TENANT SCOPING (the isolation guarantee `/sync/stream` binds to via `AuthTenant`):
    /// a wake published for tenant A is delivered ONLY to tenant A's subscriber; tenant B's
    /// subscriber sees nothing. A cross-tenant leak here would let a client observe another
    /// tenant's write cadence — the exact confused-deputy path the extractor closes.
    #[test]
    fn wake_is_tenant_scoped() {
        let hub = SseHub::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut ra = hub.subscribe(a);
        let mut rb = hub.subscribe(b);

        hub.wake(a, 7);

        assert_eq!(
            ra.try_recv().unwrap(),
            7,
            "A's subscriber receives A's wake"
        );
        assert!(
            matches!(rb.try_recv(), Err(TryRecvError::Empty)),
            "B's subscriber must NOT receive A's wake"
        );
    }

    /// The payload is exactly the committed `changeset_seq` (a pull HINT), and only the
    /// latest is meaningful: two wakes deliver both values in commit order.
    #[test]
    fn wake_carries_the_changeset_seq() {
        let hub = SseHub::new();
        let t = Uuid::new_v4();
        let mut rx = hub.subscribe(t);
        hub.wake(t, 41);
        hub.wake(t, 42);
        assert_eq!(rx.try_recv().unwrap(), 41);
        assert_eq!(rx.try_recv().unwrap(), 42);
    }

    /// A wake with no subscriber is a no-op (best-effort), never a panic — the relay must
    /// not depend on any client being connected.
    #[test]
    fn wake_without_subscriber_is_noop() {
        let hub = SseHub::new();
        hub.wake(Uuid::new_v4(), 1);
    }
}
