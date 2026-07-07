// SPDX-License-Identifier: AGPL-3.0-only
//! In-process rolling-window rate limiter per `(workspace_id, ip)` for push
//! (architecture §10). Single-process only; horizontal scale needs a shared store
//! (Redis) — noted, not a v1 blocker. This gates INGESTION only, never reads.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use uuid::Uuid;

/// Fixed number of allowed hits per rolling 60s window.
struct Bucket {
    window_start: Instant,
    count: u32,
}

pub struct RateLimiter {
    per_minute: u32,
    buckets: Mutex<HashMap<(Uuid, String), Bucket>>,
}

impl RateLimiter {
    #[must_use]
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Record a hit; returns `true` if allowed, `false` if the window is exhausted.
    /// A `per_minute` of 0 disables limiting (always allowed).
    #[must_use]
    pub fn check(&self, workspace_id: Uuid, ip: &str) -> bool {
        if self.per_minute == 0 {
            return true;
        }
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        // Opportunistic cleanup so the map can't grow unbounded across many IPs.
        if map.len() > 10_000 {
            map.retain(|_, b| now.duration_since(b.window_start) < window);
        }
        let key = (workspace_id, ip.to_string());
        let bucket = map.entry(key).or_insert(Bucket {
            window_start: now,
            count: 0,
        });
        if now.duration_since(bucket.window_start) >= window {
            bucket.window_start = now;
            bucket.count = 0;
        }
        if bucket.count >= self.per_minute {
            return false;
        }
        bucket.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_limit_then_blocks() {
        let rl = RateLimiter::new(3);
        let ws = Uuid::new_v4();
        assert!(rl.check(ws, "1.1.1.1"));
        assert!(rl.check(ws, "1.1.1.1"));
        assert!(rl.check(ws, "1.1.1.1"));
        assert!(!rl.check(ws, "1.1.1.1")); // 4th in-window is blocked
    }

    #[test]
    fn separate_keys_have_separate_budgets() {
        let rl = RateLimiter::new(1);
        let ws = Uuid::new_v4();
        assert!(rl.check(ws, "1.1.1.1"));
        assert!(!rl.check(ws, "1.1.1.1"));
        assert!(rl.check(ws, "2.2.2.2")); // different ip, own budget
        assert!(rl.check(Uuid::new_v4(), "1.1.1.1")); // different tenant, own budget
    }

    #[test]
    fn zero_disables_limiting() {
        let rl = RateLimiter::new(0);
        let ws = Uuid::new_v4();
        for _ in 0..100 {
            assert!(rl.check(ws, "9.9.9.9"));
        }
    }
}
