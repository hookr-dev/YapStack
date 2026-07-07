// SPDX-License-Identifier: AGPL-3.0-only
//! R1/R2 tests: seed→join snapshot bootstrap + reconciliation of TWO DIVERGED
//! populated DBs (the owner's real day-one path), with the no-silent-loss guarantee.
//!
//! These use the statically-linked cr-sqlite engine (they build a real CRR DB, produce
//! a snapshot, re-site it, and converge through the MockRelay). They run in-process with
//! no relay/network. The live-relay + two-real-machine path is T012 (owner UAT).

mod support;

use std::path::Path;

use rusqlite::Connection;
use uuid::Uuid;

use yapstack_sync::crypto::{ChangesetCipher, SnapshotCipher};
use yapstack_sync::outbox::drain_once;
use yapstack_sync::reconcile::{
    reconcile_local_rows, reset_join_local_state, resite_as_fresh_peer, CollisionKind,
};
use yapstack_sync::schema::{crr_migrate, SYNC_TABLES};
use yapstack_sync::snapshot::{produce_snapshot_bytes, write_snapshot_file, SnapshotMeta};
use yapstack_sync::state;
use yapstack_sync::transport::{MockRelay, SyncTransport};
use yapstack_sync::{cascade, uniqueness, CrsqlDb, CRSQLITE_ENGINE_VERSION, SYNC_SCHEMA_VERSION};

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
    // NOTE: the cr-sqlite auto-extension is process-global once any CrsqlDb has opened,
    // so a plain connection holds internal statements and strict `close()` errors.
    // Dropping is correct here (and matches how the runtime treats plain handles).
    let _ = conn.close();
}

/// CRRify a COPY of a populated plain DB into a fresh CRR DB at `crr_path` (the seed's
/// prepared sync copy).
fn build_seed_crr(plain: &Path, crr_path: &Path) {
    // VACUUM INTO makes the working CRR copy without touching the source (safety).
    {
        let src = Connection::open(plain).unwrap();
        src.execute("VACUUM INTO ?1", [crr_path.to_str().unwrap()])
            .unwrap();
    }
    let db = CrsqlDb::open(crr_path).unwrap();
    crr_migrate(db.conn()).unwrap();
    // Give the seed a stable client_id + suppress re-pushing its whole history as
    // changesets (R2 seed side): the snapshot carries it, so the push watermark starts
    // at the current max local db_version.
    let _ = state::client_id(db.conn()).unwrap();
    let max_dbv: i64 = db
        .conn()
        .query_row(
            "SELECT coalesce(max(db_version),0) FROM crsql_changes WHERE site_id = crsql_site_id()",
            [],
            |r| r.get(0),
        )
        .unwrap();
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

#[test]
fn resite_gives_join_fresh_site_and_demotes_snapshot_rows() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("seed_live.db");
    let seed_crr = dir.path().join("seed.sync.db");
    build_populated_plain(&plain);
    build_seed_crr(&plain, &seed_crr);
    let seed = CrsqlDb::open(&seed_crr).unwrap();
    let seed_sid = seed.site_id().unwrap();

    let bytes = produce_snapshot_bytes(&seed_crr, dir.path()).unwrap();
    let join_base = dir.path().join("join.sync.db");
    write_snapshot_file(&join_base, &bytes).unwrap();

    {
        let join = CrsqlDb::open(&join_base).unwrap();
        // Before re-site, the join inherits the SEED's site id (that is the hazard).
        assert_eq!(join.site_id().unwrap(), seed_sid);
        resite_as_fresh_peer(join.conn()).unwrap();
    }
    // Reopen so cr-sqlite re-reads the fresh ordinal-0 site id.
    let join = CrsqlDb::open(&join_base).unwrap();
    let join_sid = join.site_id().unwrap();
    assert_ne!(join_sid, seed_sid, "join minted a fresh site id");

    // None of the snapshot rows are attributed to the join's local site now, so a
    // local-change read returns NOTHING — the join will not re-push the seed's history.
    let local_rows: i64 = join
        .conn()
        .query_row(
            "SELECT count(*) FROM crsql_changes WHERE site_id = crsql_site_id()",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        local_rows, 0,
        "snapshot rows demoted to a peer; nothing to re-push"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn seed_and_join_converge_two_diverged_dbs_without_silent_loss() {
    let dir = tempfile::tempdir().unwrap();

    // --- SEED (Mac): populate, CRRify a copy, produce+publish a snapshot. ---
    let seed_plain = dir.path().join("seed_live.db");
    let seed_crr = dir.path().join("seed.sync.db");
    build_populated_plain(&seed_plain);
    build_seed_crr(&seed_plain, &seed_crr);
    let seed = CrsqlDb::open(&seed_crr).unwrap();
    let seed_client = state::client_id(seed.conn()).unwrap();

    let relay = MockRelay::new();
    let snap_cipher = SnapshotCipher::new(VAULT_KEY, 0, TENANT);
    let snap_bytes = produce_snapshot_bytes(&seed_crr, dir.path()).unwrap();
    let meta = SnapshotMeta {
        generation: 1,
        baseline_seq: 0,
    };
    let blob = snap_cipher.encrypt(meta.generation, &snap_bytes).unwrap();
    relay.put_snapshot(meta, &blob).await.unwrap();

    // --- JOIN (Windows): an independently-diverged populated library. ---
    let join_live = dir.path().join("join_live.db");
    build_populated_plain(&join_live); // same deterministic base => SHARED PKs with seed
    {
        let jl = Connection::open(&join_live).unwrap();
        jl.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        // Windows-only session + its children (genuinely local — MUST survive).
        jl.execute(
            "INSERT INTO sessions(id,title) VALUES('win1','Windows only session')",
            [],
        )
        .unwrap();
        jl.execute(
            "INSERT INTO segments(id,session_id,source,text,audio_offset_seconds,chunk_duration_seconds) \
             VALUES('wseg1','win1','mic','windows recorded text',0,1.0)",
            [],
        ).unwrap();
        jl.execute(
            "INSERT INTO notes(id,session_id,content) VALUES('wn1','win1','windows note')",
            [],
        )
        .unwrap();
        // A Windows-only tag (new name).
        jl.execute("INSERT INTO tags(id,name) VALUES('wt1','windows-tag')", [])
            .unwrap();
        // An EDIT to a SHARED row (same PK s0) — an ambiguous collision to SURFACE.
        jl.execute(
            "UPDATE sessions SET title='EDITED ON WINDOWS' WHERE id='s0'",
            [],
        )
        .unwrap();
    }
    let local_total: i64 = {
        let jl = Connection::open(&join_live).unwrap();
        SYNC_TABLES
            .iter()
            .map(|t| {
                jl.query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
            })
            .sum()
    };

    // Bootstrap the join from the snapshot: decrypt → write → re-site → reset local state.
    let join_base = dir.path().join("join.sync.db");
    let got = snap_cipher.decrypt(meta.generation, &blob).unwrap();
    write_snapshot_file(&join_base, &got).unwrap();
    {
        let jb = CrsqlDb::open(&join_base).unwrap();
        resite_as_fresh_peer(jb.conn()).unwrap();
        reset_join_local_state(jb.conn()).unwrap();
    }
    let join = CrsqlDb::open(&join_base).unwrap();
    let join_client = state::client_id(join.conn()).unwrap();
    assert_ne!(
        join_client, seed_client,
        "join minted a fresh client_id (not the seed's)"
    );
    state::set_pull_watermark(join.conn(), meta.baseline_seq).unwrap();

    // RECONCILE the join's own rows into the bootstrapped base.
    let report = {
        let jl = Connection::open(&join_live).unwrap();
        reconcile_local_rows(&jl, join.conn()).unwrap()
    };

    // No silent loss: every local row is accounted for.
    assert_eq!(
        report.accounted() as i64,
        local_total,
        "every local row accounted for (no silent loss)"
    );
    assert!(
        report.inserted_local_only >= 4,
        "windows-only rows preserved (session+seg+note+tag)"
    );
    // The shared-row edit to s0 is SURFACED, never silently applied or dropped.
    let s0 = report
        .collisions
        .iter()
        .find(|c| c.table == "sessions" && c.pk == "s0");
    assert!(s0.is_some(), "edited shared row surfaced as a collision");
    assert_eq!(s0.unwrap().kind, CollisionKind::ContentDiverged);

    // Windows-only rows are present in the bootstrapped base.
    for (t, pk) in [
        ("sessions", "win1"),
        ("segments", "wseg1"),
        ("notes", "wn1"),
        ("tags", "wt1"),
    ] {
        let n: i64 = join
            .conn()
            .query_row(
                &format!("SELECT count(*) FROM \"{t}\" WHERE id=?1"),
                [pk],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "windows-only {t}/{pk} preserved in base");
    }

    cascade::cascade_gc(join.conn()).unwrap();
    uniqueness::enforce_uniqueness(join.conn()).unwrap();

    // --- Convergence through the blind relay. ---
    let cc = changeset_cipher();
    let (sv, ev) = (SYNC_SCHEMA_VERSION as i32, CRSQLITE_ENGINE_VERSION as i32);

    // Join drains: pushes ONLY its local-only reconciled rows (R2 — NOT the seed's
    // ~hundreds of snapshot rows, which are peer history now).
    let jr = drain_once(join.conn(), &cc, &relay, join_client, sv, ev)
        .await
        .unwrap();
    assert!(jr.pushed >= 1, "join pushed its windows-only writes");
    let comp = relay.completeness().await.unwrap();
    assert!(
        comp.count < 100,
        "join did not re-push the seed's whole history (R2): {}",
        comp.count
    );

    // Seed drains: pulls + applies the join's windows-only rows.
    let sr = drain_once(seed.conn(), &cc, &relay, seed_client, sv, ev)
        .await
        .unwrap();
    assert!(
        sr.applied > 0,
        "seed applied the join's windows-only changes"
    );
    cascade::cascade_gc(seed.conn()).unwrap();
    uniqueness::enforce_uniqueness(seed.conn()).unwrap();

    // The Windows-only data reached the seed (Mac) — nothing lost.
    for (t, pk) in [
        ("sessions", "win1"),
        ("segments", "wseg1"),
        ("notes", "wn1"),
        ("tags", "wt1"),
    ] {
        let n: i64 = seed
            .conn()
            .query_row(
                &format!("SELECT count(*) FROM \"{t}\" WHERE id=?1"),
                [pk],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "windows-only {t}/{pk} converged onto the seed");
    }
    // The surfaced collision was NOT silently applied: the seed keeps its snapshot title.
    let seed_s0: String = seed
        .conn()
        .query_row("SELECT title FROM sessions WHERE id='s0'", [], |r| r.get(0))
        .unwrap();
    assert_ne!(
        seed_s0, "EDITED ON WINDOWS",
        "collision surfaced, not silently merged onto the seed"
    );
}
