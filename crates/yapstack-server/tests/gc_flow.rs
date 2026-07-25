// SPDX-License-Identifier: AGPL-3.0-only
//! Relay blob GC (hardening item 5) integration tests against a LIVE Postgres. `#[ignore]`-
//! gated like the other server suites (the CI-check environment has no Postgres). Run:
//!
//! ```text
//! DATABASE_URL=postgres://.../yapstack_test cargo test -p yapstack-server --test gc_flow -- --ignored
//! ```
//!
//! The DB half (eligibility selection, released_at transitions, storage-first crash safety)
//! runs with only `DATABASE_URL` — a DEAD storage endpoint is enough, because a failed object
//! DELETE must (by design) leave the row intact. The storage half (object AND row actually
//! deleted) additionally needs a reachable object store via `S3_ENDPOINT`, exactly like the
//! sibling audio test in `sync_flow.rs`; it is skipped (returns early) when unset.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::{Duration as ChronoDuration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;
use yapstack_server::config::StorageConfig;
use yapstack_server::{build_router, gc, AppState, Config};

// --------------------------------------------------------------------- harness

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for --ignored tests");
    let p = yapstack_server::db::connect(&url).await.unwrap();
    yapstack_server::db::migrate(&p).await.unwrap();
    p
}

/// A storage config that points at an unroutable endpoint: every object DELETE/HEAD fails
/// fast (connection refused), which is exactly what the DB-half + safety assertions want.
fn dead_storage() -> StorageConfig {
    StorageConfig {
        endpoint: "http://127.0.0.1:1".to_string(),
        region: "us-east-1".to_string(),
        bucket: "yapstack".to_string(),
        access_key_id: "k".to_string(),
        secret_access_key: "s".to_string(),
        public_endpoint: None,
        presign_ttl_secs: 900,
    }
}

/// MinIO connection from env → a `StorageConfig` and the endpoint/bucket, or `None` to skip.
fn minio_env() -> Option<(StorageConfig, String, String)> {
    let endpoint = std::env::var("S3_ENDPOINT").ok()?;
    let bucket = std::env::var("S3_BUCKET").unwrap_or_else(|_| "yapstack".into());
    let ak = std::env::var("S3_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
    let sk = std::env::var("S3_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into());
    let region = std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    let cfg = StorageConfig {
        endpoint: endpoint.clone(),
        region,
        bucket: bucket.clone(),
        access_key_id: ak,
        secret_access_key: sk,
        public_endpoint: None,
        presign_ttl_secs: 900,
    };
    Some((cfg, endpoint, bucket))
}

/// Insert a bare workspace (FK target for audio_blobs; not RLS-scoped).
async fn seed_workspace(pool: &sqlx::PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces (id) VALUES ($1)")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// Insert an audio_blobs row directly (under the tenant RLS guard) to reach a precise state.
async fn seed_blob(
    pool: &sqlx::PgPool,
    ws: Uuid,
    hash: &[u8],
    size: i64,
    refcount: i64,
    released_at: Option<chrono::DateTime<Utc>>,
) {
    let mut tx = yapstack_server::db::begin_tenant_tx(pool, ws)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO audio_blobs (workspace_id, ciphertext_sha256, size_bytes, refcount, released_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(ws)
    .bind(hash)
    .bind(size)
    .bind(refcount)
    .bind(released_at)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn blob_state(
    pool: &sqlx::PgPool,
    ws: Uuid,
    hash: &[u8],
) -> Option<(i64, Option<chrono::DateTime<Utc>>)> {
    let mut tx = yapstack_server::db::begin_tenant_tx(pool, ws)
        .await
        .unwrap();
    let row: Option<(i64, Option<chrono::DateTime<Utc>>)> = sqlx::query_as(
        "SELECT refcount, released_at FROM audio_blobs WHERE workspace_id = $1 AND ciphertext_sha256 = $2",
    )
    .bind(ws)
    .bind(hash)
    .fetch_optional(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    row
}

fn hash_of(seed: &str) -> Vec<u8> {
    Sha256::digest(seed.as_bytes()).to_vec()
}

/// `run_sweep` is GLOBAL (all tenants), so two overlapping sweeps in this binary would race
/// over each other's rows. Serialize the GC tests through one async lock. Each test still uses
/// its own fresh workspace and asserts on its OWN rows, so leftover rows from a prior test
/// (which persist in the shared DB) never invalidate an assertion.
fn gc_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

// ------------------------------------------------------ DB half (no MinIO needed)

/// Eligibility selection + storage-first crash safety, proven with a DEAD storage endpoint:
/// the ONE eligible blob is scanned but — because its object DELETE fails — its row is KEPT
/// (skipped), and the ineligible blobs are never touched. This is the crash-safety invariant:
/// a row is never deleted while its object is (or might still be) present.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn eligibility_and_storage_first_safety() {
    let _guard = gc_lock().lock().await;
    let pool = pool().await;
    let ws = seed_workspace(&pool).await;
    let old = Utc::now() - ChronoDuration::days(8); // past the 7d grace
    let recent = Utc::now() - ChronoDuration::days(1); // within grace

    let eligible = hash_of("eligible");
    let referenced = hash_of("referenced");
    let within_grace = hash_of("within-grace");
    let never_released = hash_of("never-released");

    seed_blob(&pool, ws, &eligible, 111, 0, Some(old)).await;
    seed_blob(&pool, ws, &referenced, 222, 2, None).await; // refcount > 0
    seed_blob(&pool, ws, &within_grace, 333, 0, Some(recent)).await; // grace not elapsed
    seed_blob(&pool, ws, &never_released, 444, 0, None).await; // refcount 0 but released_at NULL

    let stats = gc::run_sweep(&pool, &dead_storage(), ChronoDuration::days(7), Utc::now())
        .await
        .unwrap();

    // With a dead endpoint EVERY object delete fails, so NOTHING is deleted this pass — a
    // global guarantee regardless of other workspaces' rows. At least our one eligible blob was
    // scanned and skipped (other tenants may add to the count; we assert only the floor).
    assert_eq!(
        stats.deleted, 0,
        "no rows deleted when storage delete fails (crash-safety: object-first)"
    );
    assert_eq!(stats.bytes, 0);
    assert!(
        stats.skipped >= 1,
        "the eligible blob was scanned then skipped, not deleted"
    );

    // Every OWN row survives (safety) — including the eligible one whose object couldn't be
    // deleted: a row is never removed while its object might still be present.
    for h in [&eligible, &referenced, &within_grace, &never_released] {
        assert!(
            blob_state(&pool, ws, h).await.is_some(),
            "row must survive a failed sweep"
        );
    }
}

/// released_at maintenance driven through the REAL `/audio/presign` decrement/increment sites:
/// a blob that dips to refcount 0 gets stamped; repointing a part back onto it clears the
/// stamp; and it is then never GC-eligible. Uses a dead storage endpoint (the existence-check
/// HEAD on the dedup path is allowed to fail — it does not touch the committed DB state).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn released_at_transitions_and_repoint_clears() {
    let _guard = gc_lock().lock().await;
    let base = pool().await;
    let url = std::env::var("DATABASE_URL").unwrap();
    let cfg = Config::from_toml_str(&format!(
        "database_url = \"{url}\"\njwt_secret = \"s\"\nserver_pepper = \"p\"\n\
         [storage]\nendpoint = \"http://127.0.0.1:1\"\nregion = \"us-east-1\"\n\
         bucket = \"yapstack\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\n"
    ))
    .unwrap();
    let app = build_router(AppState::new(base.clone(), cfg));
    let (tok, tenant) = signup(&app).await;

    let part = Uuid::new_v4();
    let sess = Uuid::new_v4();
    let a = hash_of("blob-A");
    let b = hash_of("blob-B");
    let a_hex = hex::encode(&a);
    let b_hex = hex::encode(&b);

    // 1) point P → A : A refcount 1, released_at NULL.
    presign(&app, &tok, &a_hex, part, sess).await;
    let (rc, rel) = blob_state(&base, tenant, &a).await.unwrap();
    assert_eq!(rc, 1);
    assert!(rel.is_none(), "referenced blob keeps released_at NULL");

    // 2) repoint P → B : A drops to 0 and is STAMPED; B is 1.
    presign(&app, &tok, &b_hex, part, sess).await;
    let (rc_a, rel_a) = blob_state(&base, tenant, &a).await.unwrap();
    assert_eq!(rc_a, 0);
    assert!(
        rel_a.is_some(),
        "transition to refcount 0 stamps released_at"
    );
    let (rc_b, rel_b) = blob_state(&base, tenant, &b).await.unwrap();
    assert_eq!(rc_b, 1);
    assert!(rel_b.is_none());

    // 3) repoint P → A again : A goes back UP to 1 and released_at is CLEARED; B drops to 0.
    presign(&app, &tok, &a_hex, part, sess).await;
    let (rc_a2, rel_a2) = blob_state(&base, tenant, &a).await.unwrap();
    assert_eq!(rc_a2, 1);
    assert!(
        rel_a2.is_none(),
        "refcount going back up clears released_at"
    );

    // 4) even with an ANCIENT stamp forced on A, a re-referenced (refcount>0) blob is NEVER
    //    eligible — the predicate requires refcount <= 0. Dead storage ⇒ deleted == 0 globally.
    force_released_at(&base, tenant, &a, Utc::now() - ChronoDuration::days(30)).await;
    let stats = gc::run_sweep(&base, &dead_storage(), ChronoDuration::days(7), Utc::now())
        .await
        .unwrap();
    assert_eq!(stats.deleted, 0, "dead storage never deletes a row");
    let (rc_a3, _) = blob_state(&base, tenant, &a).await.unwrap();
    assert_eq!(rc_a3, 1, "referenced blob survives with refcount intact");
    // B (refcount 0) still has a fresh stamp (within grace) → untouched this pass.
    let (rc_b3, _) = blob_state(&base, tenant, &b).await.unwrap();
    assert_eq!(rc_b3, 0);
}

/// Force a released_at value (test-only) to exercise grace boundaries deterministically.
async fn force_released_at(pool: &sqlx::PgPool, ws: Uuid, hash: &[u8], at: chrono::DateTime<Utc>) {
    let mut tx = yapstack_server::db::begin_tenant_tx(pool, ws)
        .await
        .unwrap();
    sqlx::query("UPDATE audio_blobs SET released_at = $3 WHERE workspace_id = $1 AND ciphertext_sha256 = $2")
        .bind(ws)
        .bind(hash)
        .bind(at)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

// -------------------------------------------------- storage half (needs MinIO)

/// Full lifecycle against a reachable object store: an eligible blob's OBJECT and ROW are both
/// deleted (bytes reclaimed); a referenced blob and an in-grace blob are untouched; and a row
/// whose object is ALREADY gone (crash-order simulation) is still swept clean, because the
/// idempotent DELETE succeeds on a missing object.
#[tokio::test]
#[ignore = "requires a live Postgres AND a reachable MinIO (S3_ENDPOINT)"]
async fn gc_deletes_object_and_row_live() {
    let Some((store, _endpoint, _bucket)) = minio_env() else {
        eprintln!("SKIP: S3_ENDPOINT unset — no reachable object store");
        return;
    };
    let _guard = gc_lock().lock().await;
    let pool = pool().await;
    let ws = seed_workspace(&pool).await;
    let client = reqwest::Client::new();
    let old = Utc::now() - ChronoDuration::days(8);
    let recent = Utc::now() - ChronoDuration::days(1);

    // Eligible blob WITH a real object present (key = hash of the actual bytes).
    let eligible_bytes = b"opaque-ciphertext-eligible".to_vec();
    let eligible = Sha256::digest(&eligible_bytes).to_vec();
    let ekey = yapstack_server::storage::object_key(ws, &hex::encode(&eligible));
    put_object(&client, &store, &ekey, &eligible_bytes).await;
    seed_blob(
        &pool,
        ws,
        &eligible,
        eligible_bytes.len() as i64,
        0,
        Some(old),
    )
    .await;

    // Referenced blob (object present) — must NOT be deleted.
    let referenced = Sha256::digest(b"opaque-referenced".as_slice()).to_vec();
    let rkey = yapstack_server::storage::object_key(ws, &hex::encode(&referenced));
    put_object(&client, &store, &rkey, b"opaque-referenced").await;
    seed_blob(&pool, ws, &referenced, 17, 3, None).await;

    // In-grace blob (object present) — must NOT be deleted yet.
    let ingrace = Sha256::digest(b"opaque-ingrace".as_slice()).to_vec();
    let gkey = yapstack_server::storage::object_key(ws, &hex::encode(&ingrace));
    put_object(&client, &store, &gkey, b"opaque-ingrace").await;
    seed_blob(&pool, ws, &ingrace, 17, 0, Some(recent)).await;

    // Crash-order: eligible row whose OBJECT was never/already deleted → still swept clean.
    let orphan = Sha256::digest(b"opaque-orphan-row".as_slice()).to_vec();
    seed_blob(&pool, ws, &orphan, 99, 0, Some(old)).await; // no object put

    let stats = gc::run_sweep(&pool, &store, ChronoDuration::days(7), Utc::now())
        .await
        .unwrap();

    // Both OWN eligible blobs (present-object + orphan-row) were deleted; other tenants' rows
    // may add to the global counters, so assert the floor + prove our own rows precisely below.
    assert!(stats.deleted >= 2, "eligible + orphan-row both deleted");
    assert!(
        stats.bytes >= eligible_bytes.len() as u64 + 99,
        "bytes reclaimed counted"
    );

    // Rows gone for the eligible + orphan; kept for referenced + in-grace.
    assert!(
        blob_state(&pool, ws, &eligible).await.is_none(),
        "eligible row deleted"
    );
    assert!(
        blob_state(&pool, ws, &orphan).await.is_none(),
        "orphan-row deleted"
    );
    assert!(
        blob_state(&pool, ws, &referenced).await.is_some(),
        "referenced row kept"
    );
    assert!(
        blob_state(&pool, ws, &ingrace).await.is_some(),
        "in-grace row kept"
    );

    // The eligible OBJECT is gone from storage; the referenced object survives.
    assert!(
        !object_present(&client, &store, &ekey).await,
        "eligible object deleted"
    );
    assert!(
        object_present(&client, &store, &rkey).await,
        "referenced object survives"
    );
}

async fn put_object(client: &reqwest::Client, store: &StorageConfig, key: &str, bytes: &[u8]) {
    let signed =
        yapstack_server::storage::presign(store, "PUT", key, Some(bytes.len() as u64), Utc::now());
    let resp = client
        .put(&signed.url)
        .header("content-length", bytes.len())
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "seed PUT to MinIO failed: {}",
        resp.status()
    );
}

/// Presigned HEAD (same signer the relay uses) → object present?
async fn object_present(client: &reqwest::Client, store: &StorageConfig, key: &str) -> bool {
    let signed = yapstack_server::storage::presign(store, "HEAD", key, None, Utc::now());
    let resp = client.head(&signed.url).send().await.unwrap();
    resp.status().is_success()
}

// ------------------------------------------------------ auth/presign helpers

async fn signup(app: &axum::Router) -> (String, Uuid) {
    let email = format!("gc-{}@example.com", Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = {
        let stretch = yapstack_crypto::kdf::client_stretch(b"pw", &salt_enc).unwrap();
        let (auth, _m) = yapstack_crypto::kdf::split_keys(&stretch);
        B64.encode(auth)
    };
    let wrapped = B64.encode([0x01u8; 73]);
    let vault_key = [0x33u8; 32];
    let client_id = Uuid::new_v4();
    let sk = yapstack_crypto::sign::roster_signing_key(&vault_key);
    let roster = json!({ "version": 1, "counter": 0, "vault_key_epoch": 0 });
    let sig = yapstack_crypto::sign::sign_roster(&vault_key, roster.to_string().as_bytes());
    let body = json!({
        "email": email, "auth_key": auth_key, "salt_enc": B64.encode(salt_enc),
        "recovery_auth_key": B64.encode([0x5au8; 20]),
        "wrapped_vault_key_password": wrapped, "wrapped_vault_key_recovery": wrapped,
        "device_list": {
            "device_list": roster, "signature": B64.encode(sig),
            "counter": 0, "vault_key_epoch": 0, "client_id": client_id,
            "ed25519_pub": B64.encode(sk.verifying_key().to_bytes()), "label": "d"
        }
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/signup")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    (
        v["access_token"].as_str().unwrap().to_string(),
        v["tenant_id"].as_str().unwrap().parse().unwrap(),
    )
}

async fn presign(app: &axum::Router, tok: &str, sha_hex: &str, part: Uuid, sess: Uuid) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/audio/presign?sha256={sha_hex}&size=100&part_id={part}&session_id={sess}"
                ))
                .header("authorization", format!("Bearer {tok}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "presign should succeed");
}
