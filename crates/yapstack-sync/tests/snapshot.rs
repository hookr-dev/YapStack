// SPDX-License-Identifier: AGPL-3.0-only
//! Snapshot seam tests: a compact CRR snapshot round-trips through its cipher and
//! reopens as an identical CRR base, and the snapshot / changeset AAD domains never
//! cross-open (CRYPTO_SPEC §4.2/§5.2). The seam has no desktop caller today; it is
//! retained for relay history compaction (docs/SYNC_SPEC.md §15.F).

mod support;

use std::path::Path;

use rusqlite::Connection;
use uuid::Uuid;

use yapstack_sync::crypto::{ChangesetCipher, SnapshotCipher};
use yapstack_sync::schema::{crr_migrate, SYNC_TABLES};
use yapstack_sync::snapshot::{produce_snapshot_bytes, write_snapshot_file};
use yapstack_sync::state;
use yapstack_sync::{change, CrsqlDb, CRSQLITE_ENGINE_VERSION, SYNC_SCHEMA_VERSION};

const TENANT: Uuid = Uuid::from_u128(0xABCD_0000_0000_0000_0000_0000_0000_0001);
const VAULT_KEY: [u8; 32] = [42u8; 32];

fn changeset_cipher() -> ChangesetCipher {
    ChangesetCipher::new(
        VAULT_KEY,
        0,
        TENANT,
        SYNC_SCHEMA_VERSION,
        CRSQLITE_ENGINE_VERSION,
    )
}

/// Build a plain (non-CRR) populated library at `path` — the shape of a `.backup` of
/// the live app DB.
fn build_populated_plain(path: &Path) {
    let conn = Connection::open(path).unwrap();
    support::create_original_schema(&conn);
    support::populate(&conn);
    // The cr-sqlite auto-extension is process-global once any CrsqlDb has opened, so a
    // plain connection holds internal statements and strict `close()` errors. Dropping
    // is correct here (and matches how the runtime treats plain handles).
    let _ = conn.close();
}

/// CRRify a COPY of a populated plain DB into a fresh CRR DB at `crr_path`.
fn build_seed_crr(plain: &Path, crr_path: &Path) {
    {
        let src = Connection::open(plain).unwrap();
        src.execute("VACUUM INTO ?1", [crr_path.to_str().unwrap()])
            .unwrap();
    }
    let db = CrsqlDb::open(crr_path).unwrap();
    crr_migrate(db.conn()).unwrap();
    let _ = state::client_id(db.conn()).unwrap();
    let max_dbv = change::max_local_db_version(db.conn()).unwrap();
    state::set_push_watermark(db.conn(), max_dbv).unwrap();
}

#[test]
fn snapshot_roundtrip_reopens_as_identical_crr_base() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("seed_live.db");
    let seed_crr = dir.path().join("seed.sync.db");
    build_populated_plain(&plain);
    build_seed_crr(&plain, &seed_crr);

    let seed = CrsqlDb::open(&seed_crr).unwrap();
    let want: Vec<_> = SYNC_TABLES
        .iter()
        .map(|t| (*t, support::fingerprint(seed.conn(), t)))
        .collect();

    // Produce → encrypt → decrypt → rewrite → reopen.
    let bytes = produce_snapshot_bytes(&seed_crr, dir.path()).unwrap();
    let cipher = SnapshotCipher::new(VAULT_KEY, 0, TENANT);
    let blob = cipher.encrypt(1, &bytes).unwrap();
    // Opaque to anyone without the vault key: wrong generation / wrong key both fail.
    assert!(cipher.decrypt(2, &blob).is_err());
    assert!(SnapshotCipher::new([0u8; 32], 0, TENANT)
        .decrypt(1, &blob)
        .is_err());

    let round = cipher.decrypt(1, &blob).unwrap();
    assert_eq!(round, bytes, "snapshot decrypt round-trips");

    let join_base = dir.path().join("join.sync.db");
    write_snapshot_file(&join_base, &round).unwrap();
    let join = CrsqlDb::open(&join_base).unwrap();
    for (t, fp) in &want {
        assert_eq!(
            *fp,
            support::fingerprint(join.conn(), t),
            "table {t} content preserved"
        );
    }
}

/// AAD domain separation (§4.2/§5.2): the two ciphers hold the SAME vault key, epoch
/// and tenant, so only their distinct domains keep a changeset blob from opening as a
/// snapshot — and the reverse. Both directions must fail with everything else matched.
#[test]
fn changeset_and_snapshot_blobs_do_not_cross_open() {
    let client = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0007);
    let changeset = changeset_cipher();
    let snapshot = SnapshotCipher::new(VAULT_KEY, 0, TENANT);

    let cs_blob = changeset.encrypt(client, 1, b"payload").unwrap();
    assert!(
        snapshot.decrypt(1, &cs_blob).is_err(),
        "a changeset blob must not open under the snapshot domain"
    );

    let snap_blob = snapshot.encrypt(1, b"payload").unwrap();
    assert!(
        changeset.decrypt(client, 1, &snap_blob).is_err(),
        "a snapshot blob must not open under the changeset domain"
    );
}
