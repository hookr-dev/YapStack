// SPDX-License-Identifier: AGPL-3.0-only
//! A first-come-first-served turnstile for in-process SQLite write transactions.
//!
//! SQLite's busy handler is not fair. A connection waiting for the write lock sleeps and
//! re-polls with backoff (steps of up to ~100 ms) and only wins if a poll happens to land
//! while the lock is free. A writer that commits and immediately begins again re-takes the
//! lock within microseconds, so a loop of back-to-back write transactions can keep another
//! connection in the same process waiting until its `busy_timeout` expires.
//!
//! [`WriteTurnstile`] is a ticket lock: each writer draws a ticket and is served strictly in
//! ticket order. A writer that wants its next transaction after a commit draws a NEW ticket,
//! which is behind every writer already waiting, so each waiter is guaranteed a turn.
//!
//! Rules for callers:
//! - Take the turn BEFORE `BEGIN` and release it after `COMMIT`/rollback. Never take it while
//!   already inside a transaction (the SQLite lock would then be held while waiting here).
//! - Never take it twice on one thread; it is not reentrant.
//! - Readers do not take it.
//! - It is a blocking lock: don't hold a [`WriteTurn`] across an `.await`.

use std::sync::{Condvar, Mutex};

/// FIFO ticket lock that serializes write transactions in arrival order.
#[derive(Debug, Default)]
pub struct WriteTurnstile {
    tickets: Mutex<Tickets>,
    served: Condvar,
}

#[derive(Debug, Default)]
struct Tickets {
    /// Next ticket to hand out.
    next: u64,
    /// Ticket whose holder currently has the turn (or is next to take it).
    serving: u64,
}

/// Proof of holding the turn. Dropping it hands the turn to the next ticket.
#[derive(Debug)]
#[must_use = "the turn is released as soon as this guard is dropped"]
pub struct WriteTurn<'a> {
    turnstile: &'a WriteTurnstile,
}

static PROCESS: WriteTurnstile = WriteTurnstile::new();

impl WriteTurnstile {
    pub const fn new() -> Self {
        Self {
            tickets: Mutex::new(Tickets {
                next: 0,
                serving: 0,
            }),
            served: Condvar::new(),
        }
    }

    /// The process-wide turnstile shared by every writer of the app's database file.
    pub fn process() -> &'static WriteTurnstile {
        &PROCESS
    }

    /// Draw a ticket and block until it is served.
    pub fn acquire(&self) -> WriteTurn<'_> {
        let mut t = self.lock();
        let mine = t.next;
        t.next += 1;
        while t.serving != mine {
            t = self.served.wait(t).unwrap_or_else(|e| e.into_inner());
        }
        WriteTurn { turnstile: self }
    }

    /// Tickets drawn but not yet served, i.e. writers waiting behind the current holder.
    pub fn waiting(&self) -> u64 {
        let t = self.lock();
        let unreleased = t.next - t.serving;
        unreleased.saturating_sub(1)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tickets> {
        // The counters are always consistent (each update is a single step), so a panic
        // elsewhere while the mutex was held cannot leave them torn.
        self.tickets.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for WriteTurn<'_> {
    fn drop(&mut self) {
        let mut t = self.turnstile.lock();
        t.serving += 1;
        drop(t);
        self.turnstile.served.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::Arc;

    fn wait_for_waiting(ts: &WriteTurnstile, n: u64) {
        while ts.waiting() < n {
            std::thread::yield_now();
        }
    }

    #[test]
    fn waiters_are_served_in_arrival_order() {
        let ts = Arc::new(WriteTurnstile::new());
        let holder = ts.acquire();
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for id in 0..4 {
            let (worker_ts, tx) = (ts.clone(), tx.clone());
            handles.push(std::thread::spawn(move || {
                let _turn = worker_ts.acquire();
                tx.send(id).unwrap();
            }));
            wait_for_waiting(&ts, id + 1);
        }
        drop(holder);
        for h in handles {
            h.join().unwrap();
        }
        let order: Vec<u64> = rx.try_iter().collect();
        assert_eq!(order, vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_releasing_holder_cannot_cut_in_front_of_a_waiter() {
        // The holder releases and immediately asks again, as a loop of back-to-back write
        // transactions does. The writer already waiting must go first.
        let ts = Arc::new(WriteTurnstile::new());
        let (tx, rx) = mpsc::channel();
        let first = ts.acquire();
        let waiter = {
            let (worker_ts, tx) = (ts.clone(), tx.clone());
            std::thread::spawn(move || {
                let _turn = worker_ts.acquire();
                tx.send("waiter").unwrap();
            })
        };
        wait_for_waiting(&ts, 1);
        drop(first);
        {
            let _again = ts.acquire();
            tx.send("holder again").unwrap();
        }
        waiter.join().unwrap();
        let order: Vec<&str> = rx.try_iter().collect();
        assert_eq!(order, vec!["waiter", "holder again"]);
    }

    #[test]
    fn a_turn_dropped_during_a_panic_still_passes_on() {
        let ts = Arc::new(WriteTurnstile::new());
        let panicker = {
            let worker_ts = ts.clone();
            std::thread::spawn(move || {
                let _turn = worker_ts.acquire();
                panic!("writer failed mid-transaction");
            })
        };
        assert!(panicker.join().is_err());
        let _turn = ts.acquire();
        assert_eq!(ts.waiting(), 0);
    }
}
