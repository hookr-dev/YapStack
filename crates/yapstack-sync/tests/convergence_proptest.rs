// SPDX-License-Identifier: AGPL-3.0-only
//! §15 row 1 — RANDOMIZED CRDT convergence proptest (Gap 1 / audit R1).
//!
//! The deterministic tests (`runtime::two_devices_converge_through_relay`,
//! `merge_semantics`, `reconcile`) pin specific hand-authored schedules. This harness
//! complements them with a property test: across N∈[2,5] simulated cr-sqlite peers on one
//! CRR schema, a RANDOM program of inserts/updates/deletes attributed to random peers is
//! interleaved with RANDOM sync-exchange points. Because each peer pulls from the shared
//! relay-sim only when its own `Sync` op fires, exchange ORDER is reordered/partial and some
//! peers LAG many rounds (partitions). After the random program a final full exchange runs to
//! quiescence, and the load-bearing assertion is:
//!
//!   > all peers reach an identical table fingerprint (CRDT convergence)
//!
//! regardless of the operation schedule or the (reordered, partitioned) delivery timing.
//!
//! Case count is CI-sane by default (64 cases) so `pnpm check` stays fast; a deeper soak runs
//! via `PROPTEST_CASES=4096 cargo test -p yapstack-sync convergence_proptest` (proptest reads
//! `PROPTEST_CASES` from the environment and overrides the in-file default). Failure
//! persistence is disabled so a failing case never writes a stray regressions file into the
//! source tree.

mod support;

use proptest::prelude::*;
use uuid::Uuid;

use yapstack_sync::crypto::ChangesetCipher;
use yapstack_sync::outbox::drain_once;
use yapstack_sync::state;
use yapstack_sync::transport::MockRelay;
use yapstack_sync::{CrsqlDb, CRSQLITE_ENGINE_VERSION, SYNC_SCHEMA_VERSION};

const TENANT: Uuid = Uuid::from_u128(0x9999_AAAA_BBBB_CCCC_DDDD_EEEE_FFFF_0001);
const VAULT_KEY: [u8; 32] = [23u8; 32];

/// Shared small key space, so inserts/updates/deletes from independent peers COLLIDE on the
/// same PKs and actually exercise cr-sqlite's LWW/delete merge (not a trivial disjoint union).
const KEYS: usize = 6;

fn cipher() -> ChangesetCipher {
    ChangesetCipher::new(
        VAULT_KEY,
        0,
        TENANT,
        SYNC_SCHEMA_VERSION,
        CRSQLITE_ENGINE_VERSION,
    )
}

fn make_kv(db: &CrsqlDb) {
    db.conn()
        .execute_batch("CREATE TABLE kv(id TEXT NOT NULL PRIMARY KEY, v TEXT NOT NULL DEFAULT '');")
        .unwrap();
    db.conn()
        .query_row("SELECT crsql_as_crr('kv')", [], |_| Ok(()))
        .unwrap();
}

/// One step of the random program. `peer`/`key` are raw indices reduced modulo the actual
/// peer/key counts at execution time (proptest can't size a range against another generated
/// value cheaply, so we generate wide and fold).
#[derive(Debug, Clone)]
enum Op {
    Insert {
        peer: usize,
        key: usize,
        val: u16,
    },
    Update {
        peer: usize,
        key: usize,
        val: u16,
    },
    Delete {
        peer: usize,
        key: usize,
    },
    /// The attributed peer drains once: PUSH its captured local writes, then PULL + apply
    /// whatever the relay holds that it has not yet seen. Peers that rarely get a `Sync` lag.
    Sync {
        peer: usize,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let peer = any::<u8>().prop_map(|b| b as usize);
    let key = any::<u8>().prop_map(|b| b as usize);
    let val = any::<u16>();
    prop_oneof![
        // Weight `Sync` so exchanges happen often enough to converge in a bounded final loop,
        // while still leaving long partitioned stretches for some peers.
        3 => peer.clone().prop_map(|peer| Op::Sync { peer }),
        2 => (peer.clone(), key.clone(), val).prop_map(|(peer, key, val)| Op::Insert { peer, key, val }),
        2 => (peer.clone(), key.clone(), val).prop_map(|(peer, key, val)| Op::Update { peer, key, val }),
        1 => (peer, key).prop_map(|(peer, key)| Op::Delete { peer, key }),
    ]
}

fn program_strategy() -> impl Strategy<Value = (usize, Vec<Op>)> {
    // N ∈ [2,5] peers, 3..40 operations.
    (2usize..=5, prop::collection::vec(op_strategy(), 3..40))
}

/// Run the generated program against `n` real cr-sqlite peers sharing one MockRelay, then a
/// final full exchange to quiescence. Returns the per-peer `kv` fingerprints (count, hash).
async fn run_program(n: usize, ops: &[Op]) -> Vec<(i64, u64)> {
    let peers: Vec<CrsqlDb> = (0..n).map(|_| CrsqlDb::open_in_memory().unwrap()).collect();
    for p in &peers {
        make_kv(p);
    }
    let clients: Vec<Uuid> = peers
        .iter()
        .map(|p| state::client_id(p.conn()).unwrap())
        .collect();
    // Fresh client_id per install (a property the runtime depends on).
    for i in 0..n {
        for j in (i + 1)..n {
            assert_ne!(
                clients[i], clients[j],
                "each peer mints a distinct client_id"
            );
        }
    }

    let relay = MockRelay::new();
    let c = cipher();
    let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

    for op in ops {
        match *op {
            Op::Insert { peer, key, val } => {
                let p = peer % n;
                let _ = peers[p].conn().execute(
                    "INSERT INTO kv(id,v) VALUES(?1,?2) ON CONFLICT(id) DO NOTHING",
                    rusqlite::params![format!("k{}", key % KEYS), format!("v{val}")],
                );
            }
            Op::Update { peer, key, val } => {
                let p = peer % n;
                let _ = peers[p].conn().execute(
                    "UPDATE kv SET v=?2 WHERE id=?1",
                    rusqlite::params![format!("k{}", key % KEYS), format!("v{val}")],
                );
            }
            Op::Delete { peer, key } => {
                let p = peer % n;
                let _ = peers[p].conn().execute(
                    "DELETE FROM kv WHERE id=?1",
                    rusqlite::params![format!("k{}", key % KEYS)],
                );
            }
            Op::Sync { peer } => {
                let p = peer % n;
                // Randomized, possibly-partial delivery: only THIS peer exchanges now, so
                // other peers may lag arbitrarily many rounds behind the relay log.
                let r = drain_once(peers[p].conn(), &c, &relay, clients[p], sv, ev)
                    .await
                    .expect("drain must not hard-error (crypto-quarantine is non-fatal)");
                assert_eq!(
                    r.crypto_skipped, 0,
                    "all peers share one vault key; nothing should fail to decrypt"
                );
            }
        }
    }

    // FINAL FULL EXCHANGE: drain every peer repeatedly until a whole pass neither pushes nor
    // applies anything (quiescent). A generous bound also asserts TERMINATION — divergence or
    // a non-converging schedule would blow the bound and fail the test.
    let max_rounds = 4 * n + 8;
    let mut quiescent = false;
    for _ in 0..max_rounds {
        let mut moved = false;
        for p in 0..n {
            let r = drain_once(peers[p].conn(), &c, &relay, clients[p], sv, ev)
                .await
                .expect("final-exchange drain must not hard-error");
            assert_eq!(r.crypto_skipped, 0, "no cross-peer decrypt failures");
            if r.pushed > 0 || r.applied > 0 {
                moved = true;
            }
        }
        if !moved {
            quiescent = true;
            break;
        }
    }
    assert!(
        quiescent,
        "peers did not reach quiescence within {max_rounds} full rounds (non-convergence)"
    );

    peers
        .iter()
        .map(|p| support::fingerprint(p.conn(), "kv"))
        .collect()
}

proptest! {
    // 64 cases keeps `pnpm check` fast; PROPTEST_CASES overrides for a deeper soak.
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// CRDT convergence: after any random operation schedule with reordered/partitioned
    /// delivery followed by a final full exchange, ALL peers hold an identical `kv`
    /// fingerprint (same count AND same content hash).
    #[test]
    fn peers_converge_under_random_schedules((n, ops) in program_strategy()) {
        // A fresh single-threaded runtime per case: rusqlite Connections are not Send, and a
        // current-thread runtime keeps every drain on this thread.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fps = rt.block_on(run_program(n, &ops));

        let first = fps[0];
        for (i, fp) in fps.iter().enumerate() {
            prop_assert_eq!(
                *fp, first,
                "peer {} diverged from peer 0: {:?} vs {:?} (n={}, ops={:?})",
                i, fp, first, n, ops
            );
        }
    }
}
