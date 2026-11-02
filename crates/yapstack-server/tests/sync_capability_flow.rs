// SPDX-License-Identifier: AGPL-3.0-only
//! The sync capability gate (ADR 0006 §5) against a LIVE Postgres. `#[ignore]`-gated like
//! the other relay integration suites; run with
//!
//! ```text
//! DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/yapstack_test \
//!   cargo test -p yapstack-server --test sync_capability_flow -- --ignored
//! ```
//!
//! Covers: the pull header is recorded per device (throttled, NULL stamp always written),
//! the 426 refusal below the floor (which still stamps), the raise's atomic eligibility
//! check (30-day window, revoked and ghost rows, the caller's own header), the push batch
//! gate including after a relay restore drops the floor, the snapshot route gate, and the
//! raise-vs-pull interleaving.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;
use yapstack_server::sync::{capability, pull};
use yapstack_server::{build_router, AppState, Config};

// --------------------------------------------------------------------- harness

async fn setup() -> AppState {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for --ignored tests");
    let pool = yapstack_server::db::connect(&url).await.unwrap();
    yapstack_server::db::migrate(&pool).await.unwrap();
    let cfg = Config::from_toml_str(&format!(
        "database_url = \"{url}\"\njwt_secret = \"test-secret\"\nserver_pepper = \"test-pepper\"\n"
    ))
    .unwrap();
    AppState::new(pool, cfg)
}

struct Tenant {
    tenant: Uuid,
    user: Uuid,
    /// The signup device's id and access token (an active device).
    owner_device: Uuid,
    owner_token: String,
}

async fn signup(st: &AppState, app: &axum::Router) -> Tenant {
    let email = format!("cap-{}@example.com", Uuid::new_v4());
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
            "ed25519_pub": B64.encode(sk.verifying_key().to_bytes()), "label": "owner"
        }
    });
    let resp = send(app, "POST", "/auth/signup", None, &[], Some(body)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let (user,): (Uuid,) = sqlx::query_as("SELECT user_id FROM devices WHERE client_id = $1")
        .bind(client_id)
        .fetch_one(&st.pool)
        .await
        .unwrap();
    Tenant {
        tenant: v["tenant_id"].as_str().unwrap().parse().unwrap(),
        user,
        owner_device: client_id,
        owner_token: v["access_token"].as_str().unwrap().to_string(),
    }
}

/// Insert another device row for the tenant (as the owner connection) and mint it a token.
async fn add_device(
    st: &AppState,
    t: &Tenant,
    label: &str,
    status: &str,
    seen_days_ago: Option<i32>,
    level: i32,
) -> (Uuid, String) {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (client_id, workspace_id, user_id, ed25519_pub, label, platform, \
                              status, sync_capability, capability_seen_at) \
         VALUES ($1, $2, $3, $4, $5, 'windows', $6, $7, \
                 CASE WHEN $8::int IS NULL THEN NULL \
                      ELSE now() - make_interval(days => $8::int) END)",
    )
    .bind(id)
    .bind(t.tenant)
    .bind(t.user)
    .bind(vec![7u8; 32])
    .bind(label)
    .bind(status)
    .bind(level)
    .bind(seen_days_ago)
    .execute(&st.pool)
    .await
    .unwrap();
    let tok = st.jwt.issue_access(t.user, t.tenant, Some(id)).unwrap();
    (id, tok)
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> axum::response::Response {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
    app.clone().oneshot(b.body(body).unwrap()).await.unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn pull_at(app: &axum::Router, tok: &str, level: Option<&str>) -> axum::response::Response {
    let h: Vec<(&str, &str)> = level
        .map(|l| vec![("x-yapstack-sync-capability", l)])
        .unwrap_or_default();
    send(app, "GET", "/sync/pull?since=0", Some(tok), &h, None).await
}

async fn raise(
    app: &axum::Router,
    tok: &str,
    own_level: &str,
    to: i32,
) -> axum::response::Response {
    send(
        app,
        "PUT",
        "/sync/capability",
        Some(tok),
        &[("x-yapstack-sync-capability", own_level)],
        Some(json!({ "required_sync_capability": to })),
    )
    .await
}

fn change(client_id: Uuid, client_seq: i64, payload: &[u8]) -> Value {
    json!({
        "client_id": client_id, "client_seq": client_seq,
        "ciphertext": B64.encode(payload), "schema_version": 22, "engine_version": 16003
    })
}

async fn push_at(
    app: &axum::Router,
    tok: &str,
    batch_level: Option<&str>,
    changes: Vec<Value>,
) -> axum::response::Response {
    let h: Vec<(&str, &str)> = batch_level
        .map(|l| vec![("x-yapstack-batch-capability", l)])
        .unwrap_or_default();
    send(
        app,
        "POST",
        "/sync/push",
        Some(tok),
        &h,
        Some(json!({ "changes": changes })),
    )
    .await
}

async fn device_stamp(pool: &PgPool, id: Uuid) -> (i32, Option<chrono::DateTime<chrono::Utc>>) {
    sqlx::query_as("SELECT sync_capability, capability_seen_at FROM devices WHERE client_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// --------------------------------------------------------------------- tests

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn pull_records_level_and_returns_workspace_levels() {
    let st = setup().await;
    let app = build_router(st.clone());
    let t = signup(&st, &app).await;

    let resp = pull_at(&app, &t.owner_token, Some("1")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["required_sync_capability"], 0);
    assert_eq!(v["requested_sync_capability"], 0);

    let (level, seen) = device_stamp(&st.pool, t.owner_device).await;
    assert_eq!(level, 1, "the pull header is recorded on the caller's row");
    assert!(seen.is_some(), "capability_seen_at is stamped");

    // GET /devices exposes both fields.
    let v = body_json(send(&app, "GET", "/devices", Some(&t.owner_token), &[], None).await).await;
    let me = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_id"] == t.owner_device.to_string())
        .unwrap();
    assert_eq!(me["sync_capability"], 1);
    assert!(me["capability_seen_at"].is_string());

    // A malformed header is a 400, not a silent level 0.
    let resp = pull_at(&app, &t.owner_token, Some("one")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn stamp_is_throttled_but_always_written_when_null_or_level_changes() {
    let st = setup().await;
    let app = build_router(st.clone());
    let t = signup(&st, &app).await;
    let (dev, tok) = add_device(&st, &t, "laptop", "active", None, 0).await;

    // An existing device's first headerless pull is stamped (the NULL arm of the throttle).
    assert_eq!(pull_at(&app, &tok, None).await.status(), StatusCode::OK);
    let (level, seen) = device_stamp(&st.pool, dev).await;
    assert_eq!(level, 0);
    let seen = seen.expect("a headerless first pull must stamp capability_seen_at");

    // Within the hour at the same level: no write.
    sqlx::query("UPDATE devices SET capability_seen_at = $2 WHERE client_id = $1")
        .bind(dev)
        .bind(seen - chrono::Duration::minutes(10))
        .execute(&st.pool)
        .await
        .unwrap();
    assert_eq!(pull_at(&app, &tok, None).await.status(), StatusCode::OK);
    let (_, after) = device_stamp(&st.pool, dev).await;
    assert_eq!(
        after,
        Some(seen - chrono::Duration::minutes(10)),
        "a pull within 1 h at the same level writes nothing to devices"
    );

    // A level change is recorded at once, inside the hour.
    assert_eq!(
        pull_at(&app, &tok, Some("1")).await.status(),
        StatusCode::OK
    );
    let (level, after) = device_stamp(&st.pool, dev).await;
    assert_eq!(level, 1);
    assert!(after.unwrap() > seen - chrono::Duration::minutes(10));

    // An over-an-hour-old stamp is refreshed.
    let old = chrono::Utc::now() - chrono::Duration::hours(2);
    sqlx::query("UPDATE devices SET capability_seen_at = $2 WHERE client_id = $1")
        .bind(dev)
        .bind(old)
        .execute(&st.pool)
        .await
        .unwrap();
    assert_eq!(
        pull_at(&app, &tok, Some("1")).await.status(),
        StatusCode::OK
    );
    assert!(device_stamp(&st.pool, dev).await.1.unwrap() > old);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn raise_rechecks_eligibility_and_names_blockers() {
    let st = setup().await;
    let app = build_router(st.clone());
    let t = signup(&st, &app).await;

    // A level-0 device that has never reported: its first headerless pull stamps it, and from
    // then on it blocks a raise.
    let (laggard, laggard_tok) = add_device(&st, &t, "old laptop", "active", None, 0).await;
    // Out of the window, revoked, and pending-but-current rows.
    add_device(&st, &t, "gone 31d", "active", Some(31), 0).await;
    add_device(&st, &t, "revoked", "revoked", Some(1), 0).await;
    add_device(&st, &t, "pending ok", "pending", Some(1), 1).await;
    // A ghost: a reinstall's old row, seen 5 days ago at level 0.
    let (ghost, _) = add_device(&st, &t, "ghost", "active", Some(5), 0).await;

    assert_eq!(
        pull_at(&app, &laggard_tok, None).await.status(),
        StatusCode::OK
    );

    // The raiser has never pulled; its own header is recorded first, so it doesn't block itself.
    let resp = raise(&app, &t.owner_token, "1", 1).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let v = body_json(resp).await;
    assert_eq!(v["code"], "sync_capability_blocked");
    assert_eq!(v["required_sync_capability"], 0);
    assert_eq!(v["requested_sync_capability"], 1);
    let mut ids: Vec<String> = v["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["client_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    let mut want = vec![laggard.to_string(), ghost.to_string()];
    want.sort();
    assert_eq!(
        ids, want,
        "only in-window, non-revoked, below-level devices block (not the caller, the 31-day \
         row, the revoked row or the level-1 pending row)"
    );
    let ghost_row = v["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["client_id"] == ghost.to_string())
        .unwrap();
    assert_eq!(ghost_row["label"], "ghost");
    assert_eq!(ghost_row["platform"], "windows");
    assert!(ghost_row["capability_seen_at"].is_string());
    assert_eq!(device_stamp(&st.pool, t.owner_device).await.0, 1);

    // The refused raise left `requested` behind, so the straggler's pull nudges it.
    let v = body_json(pull_at(&app, &laggard_tok, None).await).await;
    assert_eq!(v["required_sync_capability"], 0);
    assert_eq!(v["requested_sync_capability"], 1);

    // The laggard updates; the ghost is revoked. Now the raise succeeds.
    assert_eq!(
        pull_at(&app, &laggard_tok, Some("1")).await.status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE devices SET status = 'revoked' WHERE client_id = $1")
        .bind(ghost)
        .execute(&st.pool)
        .await
        .unwrap();
    let resp = raise(&app, &t.owner_token, "1", 1).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["required_sync_capability"], 1);

    // A raise at or below the floor is a no-op that still answers 200.
    let v = body_json(raise(&app, &t.owner_token, "1", 0).await).await;
    assert_eq!(
        v["required_sync_capability"], 1,
        "the floor never goes down"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn pull_below_floor_is_refused_and_still_stamped() {
    let st = setup().await;
    let app = build_router(st.clone());
    let t = signup(&st, &app).await;
    let (dev, tok) = add_device(&st, &t, "laptop", "active", Some(40), 0).await;
    assert_eq!(
        raise(&app, &t.owner_token, "1", 1).await.status(),
        StatusCode::OK
    );

    let before = device_stamp(&st.pool, dev).await.1.unwrap();
    let resp = pull_at(&app, &tok, None).await;
    assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    let v = body_json(resp).await;
    assert_eq!(v["code"], "update_required");
    assert_eq!(v["message"], "Update YapStack to keep syncing");
    assert_eq!(v["required_sync_capability"], 1);
    assert!(
        device_stamp(&st.pool, dev).await.1.unwrap() > before,
        "a refused pull still stamps capability_seen_at"
    );

    // At the floor it's admitted.
    assert_eq!(
        pull_at(&app, &tok, Some("1")).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn snapshot_routes_are_refused_below_floor() {
    let st = setup().await;
    let app = build_router(st.clone());
    let t = signup(&st, &app).await;
    assert_eq!(
        raise(&app, &t.owner_token, "1", 1).await.status(),
        StatusCode::OK
    );

    let resp = send(&app, "GET", "/snapshot", Some(&t.owner_token), &[], None).await;
    assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    let presign =
        json!({ "sha256": "00".repeat(32), "size": 1, "generation": 1, "baseline_seq": 0 });
    let resp = send(
        &app,
        "POST",
        "/snapshot/presign",
        Some(&t.owner_token),
        &[],
        Some(presign.clone()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);

    // At the floor the gate passes; this test relay has no object storage, so the route
    // then reports 503 (the next check), not 426.
    let h = [("x-yapstack-sync-capability", "1")];
    let resp = send(&app, "GET", "/snapshot", Some(&t.owner_token), &h, None).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let resp = send(
        &app,
        "POST",
        "/snapshot/presign",
        Some(&t.owner_token),
        &h,
        Some(presign),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn push_batch_above_floor_is_refused_including_after_relay_restore() {
    let st = setup().await;
    let app = build_router(st.clone());
    let t = signup(&st, &app).await;
    let dev = t.owner_device;

    // No header = level 0: always accepted.
    let resp = push_at(&app, &t.owner_token, None, vec![change(dev, 1, b"a")]).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A level-1 batch at floor 0 → 409 sync_capability_floor, nothing stored.
    let resp = push_at(
        &app,
        &t.owner_token,
        Some("1"),
        vec![change(dev, 2, b"gated")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let v = body_json(resp).await;
    assert_eq!(v["code"], "sync_capability_floor");
    assert_eq!(v["required_sync_capability"], 0);
    let v = body_json(
        send(
            &app,
            "GET",
            "/sync/completeness",
            Some(&t.owner_token),
            &[],
            None,
        )
        .await,
    )
    .await;
    assert_eq!(v["count"], 1);

    // Raise, then the same batch is accepted.
    assert_eq!(
        raise(&app, &t.owner_token, "1", 1).await.status(),
        StatusCode::OK
    );
    let resp = push_at(
        &app,
        &t.owner_token,
        Some("1"),
        vec![change(dev, 2, b"gated")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Relay restore: the floor column is back at 0. The client's cached floor (1) must not
    // authorize anything; the relay refuses on what it holds now.
    sqlx::query(
        "UPDATE tenant_sync_capability SET required_sync_capability = 0 WHERE workspace_id = $1",
    )
    .bind(t.tenant)
    .execute(&st.pool)
    .await
    .unwrap();
    let resp = push_at(
        &app,
        &t.owner_token,
        Some("1"),
        vec![change(dev, 3, b"gated2")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Re-provisioned: the row is gone entirely. Bootstrap recreates it at 0 and refuses.
    sqlx::query("DELETE FROM tenant_sync_capability WHERE workspace_id = $1")
        .bind(t.tenant)
        .execute(&st.pool)
        .await
        .unwrap();
    let resp = push_at(
        &app,
        &t.owner_token,
        Some("1"),
        vec![change(dev, 3, b"gated2")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(resp).await["required_sync_capability"], 0);
}

/// Every backend transitively waiting on `root` for a lock.
async fn lock_waiters(pool: &PgPool, root: i32) -> Vec<i32> {
    let rows: Vec<(i32, Vec<i32>)> = sqlx::query_as(
        "SELECT pid, pg_blocking_pids(pid) FROM pg_stat_activity \
         WHERE wait_event_type = 'Lock'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut tree = vec![root];
    loop {
        let before = tree.len();
        for (pid, blockers) in &rows {
            if !tree.contains(pid) && blockers.iter().any(|b| tree.contains(b)) {
                tree.push(*pid);
            }
        }
        if tree.len() == before {
            break;
        }
    }
    tree.retain(|p| *p != root);
    tree
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn raise_cannot_slip_between_a_pulls_check_and_its_read() {
    // The B1 race: a level-0 pull passes its check at floor 0, a raise and a gated push run,
    // then the pull reads. Under the floor-row lock the pull must never return the gated
    // changeset: the raise waits for the pull to commit, and the gated push can't land until
    // the raise has.
    let st = setup().await;
    let app = build_router(st.clone());
    let t = signup(&st, &app).await;
    let dev = t.owner_device;
    assert_eq!(
        push_at(&app, &t.owner_token, None, vec![change(dev, 1, b"plain")])
            .await
            .status(),
        StatusCode::OK
    );

    // The paused pull: a level-0 reader with no device row (so it never enters the 30-day
    // window and can't block the raise itself). Hold it between its check and its read.
    let mut reader = yapstack_server::db::begin_tenant_tx(&st.pool, t.tenant)
        .await
        .unwrap();
    let (reader_pid,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *reader)
        .await
        .unwrap();
    let levels = capability::gate_read(&mut reader, t.tenant, None, 0)
        .await
        .unwrap();
    assert_eq!(
        levels.required_sync_capability, 0,
        "the pull passes its check at floor 0"
    );

    // The raise blocks on the reader's FOR SHARE.
    let raise_task = {
        let app = app.clone();
        let tok = t.owner_token.clone();
        tokio::spawn(async move { raise(&app, &tok, "1", 1).await.status() })
    };
    while lock_waiters(&st.pool, reader_pid).await.is_empty() {
        tokio::task::yield_now().await;
    }
    assert!(!raise_task.is_finished(), "the raise waits for the reader");

    // The gated push now either waits behind the lock or is refused at floor 0; it never
    // commits while the reader is open.
    let push_task = {
        let app = app.clone();
        let tok = t.owner_token.clone();
        tokio::spawn(async move {
            push_at(&app, &tok, Some("1"), vec![change(dev, 2, b"gated")])
                .await
                .status()
        })
    };
    while !push_task.is_finished() && lock_waiters(&st.pool, reader_pid).await.len() < 2 {
        tokio::task::yield_now().await;
    }

    // The pull's read.
    let rows = pull::read_changes(&mut reader, t.tenant, 0, 100)
        .await
        .unwrap();
    reader.commit().await.unwrap();
    let seqs: Vec<i64> = rows.iter().map(|r| r.2).collect();
    assert_eq!(
        seqs,
        vec![1],
        "the paused pull must never see the gated changeset"
    );

    assert_eq!(raise_task.await.unwrap(), StatusCode::OK);
    let push_status = push_task.await.unwrap();
    if push_status == StatusCode::CONFLICT {
        // Refused at the old floor; the client raises and retries once.
        let resp = push_at(
            &app,
            &t.owner_token,
            Some("1"),
            vec![change(dev, 2, b"gated")],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    } else {
        assert_eq!(push_status, StatusCode::OK);
    }

    // And the same level-0 client pulling now is refused rather than handed the gated row.
    let no_device = st.jwt.issue_access(t.user, t.tenant, None).unwrap();
    assert_eq!(
        pull_at(&app, &no_device, None).await.status(),
        StatusCode::UPGRADE_REQUIRED
    );
}
