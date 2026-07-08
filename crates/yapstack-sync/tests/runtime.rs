// SPDX-License-Identifier: AGPL-3.0-only
//! Encrypted-outbox crypto round-trip, drain enqueue/idempotency, and end-to-end
//! two-device convergence through the blind relay (MockRelay).

mod support;

use rusqlite::Connection;
use std::sync::Arc;
use uuid::Uuid;
use yapstack_sync::change::{ChangeRow, Changeset};
use yapstack_sync::crypto::ChangesetCipher;
use yapstack_sync::outbox::drain_once;
use yapstack_sync::state;
use yapstack_sync::transport::{MockRelay, SyncTransport};
use yapstack_sync::{CrsqlDb, CRSQLITE_ENGINE_VERSION, SYNC_SCHEMA_VERSION};

const TENANT: Uuid = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
const VAULT_KEY: [u8; 32] = [7u8; 32];

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

fn sample_changeset() -> Changeset {
    Changeset {
        rows: vec![
            ChangeRow {
                table: "kv".into(),
                pk: vec![1, 2, 3],
                cid: "v".into(),
                val: rusqlite::types::Value::Text("héllo 世界".into()),
                col_version: 4,
                db_version: 9,
                site_id: Some(vec![9u8; 16]),
                cl: 1,
                seq: 0,
            },
            ChangeRow {
                table: "kv".into(),
                pk: vec![4, 5],
                cid: "-1".into(),
                val: rusqlite::types::Value::Null,
                col_version: 1,
                db_version: 9,
                site_id: None,
                cl: 1,
                seq: 1,
            },
        ],
    }
}

#[test]
fn changeset_codec_roundtrips() {
    let cs = sample_changeset();
    let bytes = cs.encode();
    let back = Changeset::decode(&bytes).unwrap();
    assert_eq!(cs, back);
}

#[test]
fn changeset_encrypt_decrypt_roundtrip_and_aad_binding() {
    let c = cipher();
    let client = Uuid::from_u128(42);
    let pt = sample_changeset().encode();

    let blob = c.encrypt(client, 7, &pt).unwrap();
    // No plaintext markers leak (the distinctive unicode token must not appear).
    assert!(!blob
        .windows(3)
        .any(|w| w == "世".as_bytes()[..3.min("世".len())].to_vec().as_slice()));

    let out = c.decrypt(client, 7, &blob).unwrap();
    assert_eq!(out, pt);

    // AAD binds client_seq and client_id: a mismatch fails the tag.
    assert!(c.decrypt(client, 8, &blob).is_err());
    assert!(c.decrypt(Uuid::from_u128(43), 7, &blob).is_err());

    // Tamper anywhere -> open fails (never a panic).
    let mut bad = blob.clone();
    let n = bad.len();
    bad[n - 1] ^= 0x01;
    assert!(c.decrypt(client, 7, &bad).is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn drain_enqueue_push_idempotency() {
    let db = CrsqlDb::open_in_memory().unwrap();
    make_kv(&db);
    let conn = db.conn();
    let client = state::client_id(conn).unwrap();
    let relay = MockRelay::new();
    let c = cipher();
    let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

    conn.execute("INSERT INTO kv(id,v) VALUES('k1','v1')", [])
        .unwrap();
    let r1 = drain_once(conn, &c, &relay, client, sv, ev).await.unwrap();
    assert_eq!(r1.pushed, 1, "one outbox entry pushed");
    assert_eq!(r1.applied, 0, "no foreign changes to apply");

    // Re-drain with no new writes: nothing to enqueue or push, and our own echo is
    // skipped (idempotent).
    let r2 = drain_once(conn, &c, &relay, client, sv, ev).await.unwrap();
    assert_eq!(r2.pushed, 0);
    assert_eq!(r2.applied, 0);

    // Server holds exactly one changeset; our client tail matches (A3).
    let comp = relay.completeness().await.unwrap();
    assert_eq!(comp.count, 1);
    let tail = comp
        .per_client
        .iter()
        .find(|t| t.client_id == client)
        .unwrap();
    assert_eq!(tail.max_client_seq, state::client_seq(conn).unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn two_devices_converge_through_relay() {
    let a = CrsqlDb::open_in_memory().unwrap();
    let b = CrsqlDb::open_in_memory().unwrap();
    make_kv(&a);
    make_kv(&b);
    let ca = state::client_id(a.conn()).unwrap();
    let cb = state::client_id(b.conn()).unwrap();
    assert_ne!(ca, cb, "fresh client_id per install");

    let relay = Arc::new(MockRelay::new());
    let c = cipher();
    let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

    // A writes, drains (push).
    a.conn()
        .execute("INSERT INTO kv(id,v) VALUES('k1','A-val')", [])
        .unwrap();
    drain_once(a.conn(), &c, relay.as_ref(), ca, sv, ev)
        .await
        .unwrap();

    // B drains: pulls + applies A's change.
    let rb = drain_once(b.conn(), &c, relay.as_ref(), cb, sv, ev)
        .await
        .unwrap();
    assert!(rb.applied > 0);
    let bk1: String = b
        .conn()
        .query_row("SELECT v FROM kv WHERE id='k1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bk1, "A-val");

    // B edits k1 and adds k2, drains.
    b.conn()
        .execute("UPDATE kv SET v='B-val' WHERE id='k1'", [])
        .unwrap();
    b.conn()
        .execute("INSERT INTO kv(id,v) VALUES('k2','B2')", [])
        .unwrap();
    drain_once(b.conn(), &c, relay.as_ref(), cb, sv, ev)
        .await
        .unwrap();

    // A drains: pulls B's changes.
    drain_once(a.conn(), &c, relay.as_ref(), ca, sv, ev)
        .await
        .unwrap();

    // Converged: identical kv content on both devices.
    assert_eq!(
        support::fingerprint(a.conn(), "kv"),
        support::fingerprint(b.conn(), "kv"),
        "devices must converge to identical kv content"
    );
    let ak1: String = a
        .conn()
        .query_row("SELECT v FROM kv WHERE id='k1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ak1, "B-val", "LWW winner (B's later edit) present on A");
    assert_eq!(count(a.conn(), "SELECT count(*) FROM kv"), 2);

    // A final no-op drain on both is idempotent (no divergence, no re-push).
    let ra = drain_once(a.conn(), &c, relay.as_ref(), ca, sv, ev)
        .await
        .unwrap();
    assert_eq!(ra.pushed, 0);
    assert_eq!(
        support::fingerprint(a.conn(), "kv"),
        support::fingerprint(b.conn(), "kv")
    );
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn large_initial_capture_splits_into_multiple_bounded_outbox_entries() {
    let db = CrsqlDb::open_in_memory().unwrap();
    make_kv(&db);
    let conn = db.conn();
    let client = state::client_id(conn).unwrap();
    let relay = MockRelay::new();
    let c = cipher();
    let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

    // ~1.3 MiB of local writes: enough to overrun the per-chunk plaintext budget several
    // times, forcing capture to split into multiple bounded outbox entries (the fix for
    // the 413 that a single all-history push triggered).
    let big = "x".repeat(32 * 1024);
    for i in 0..40 {
        conn.execute(
            "INSERT INTO kv(id,v) VALUES(?1,?2)",
            rusqlite::params![format!("k{i}"), big.as_str()],
        )
        .unwrap();
    }

    let r = drain_once(conn, &c, &relay, client, sv, ev).await.unwrap();

    let entries = count(conn, "SELECT count(*) FROM _yapstack_outbox");
    let acked = count(conn, "SELECT count(*) FROM _yapstack_outbox WHERE acked=1");
    assert!(
        entries >= 2,
        "capture must split a large history into multiple entries, got {entries}"
    );
    assert_eq!(acked, entries, "every captured entry is pushed and acked");
    assert_eq!(r.pushed as i64, entries, "drain reports all entries pushed");

    // The relay holds exactly the captured entries — all pushed within ONE drain cycle
    // (batched back-to-back, not one batch per cycle).
    let comp = relay.completeness().await.unwrap();
    assert_eq!(comp.count, entries);

    // Re-drain is a no-op: nothing new to capture or push (watermark advanced correctly).
    let r2 = drain_once(conn, &c, &relay, client, sv, ev).await.unwrap();
    assert_eq!(r2.pushed, 0);
}
