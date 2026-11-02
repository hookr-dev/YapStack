// SPDX-License-Identifier: AGPL-3.0-only
//! RED harness for the `server-rls` finding cluster: the relay's auth + boot paths
//! against a NON-OWNER serving connection (the documented "hardened split", and every
//! managed Postgres where the table owner is not a superuser).
//!
//! The existing `auth_flow.rs` tests connect as the migration OWNER, which in practice
//! is the superuser `postgres`. Superusers carry `rolbypassrls`, so FORCE ROW LEVEL
//! SECURITY never executes on that connection and the login JOIN against the RLS-forced
//! `workspace_members` returns rows. That is precisely WHY those tests are green while
//! any hardened deployment is a permanent, silent, total auth outage.
//!
//! These tests reproduce the non-owner serving connection faithfully by building the
//! AppState pool with an `after_connect` hook that runs `SET ROLE yapstack_app`. From a
//! superuser session, `SET ROLE` to a non-superuser role drops `rolbypassrls` for all
//! privilege/RLS checks (verified inline in `serving_role_is_rls_subject`), so the pool
//! behaves exactly like a connection authenticated as `yapstack_app` — the role the
//! schema comment and docs/self-hosting.md say the app MUST use.
//!
//! Run against a throwaway Postgres:
//! ```text
//! DATABASE_URL=postgres://postgres:prove@127.0.0.1:55500/yapstack_test \
//!   cargo test -p yapstack-server --test rls_nonowner_flow -- --ignored --nocapture
//! ```
//! Each test asserts the POST-FIX (correct) behavior and therefore FAILS on the current
//! tree — this is the red half of the red/green harness for the real fix.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tower::ServiceExt;
use yapstack_server::{build_router, AppState, Config};

// ----------------------------------------------------------------- client helpers
// (mirrors auth_flow.rs; kept self-contained so this file is a standalone harness.)

fn client_auth_key(password: &str, salt_enc: &[u8]) -> String {
    let stretch = yapstack_crypto::kdf::client_stretch(password.as_bytes(), salt_enc).unwrap();
    let (auth, _master) = yapstack_crypto::kdf::split_keys(&stretch);
    B64.encode(auth)
}

const RECOVERY_BYTES: [u8; 20] = [0x5au8; 20];

fn client_recovery_auth_key(recovery_bytes: &[u8; 20]) -> String {
    B64.encode(recovery_bytes)
}

fn signup_body(email: &str, salt_enc: &[u8], auth_key_b64: &str) -> Value {
    let wrapped = B64.encode([0x01u8; 73]);
    let vault_key = [0x33u8; 32];
    let sk = yapstack_crypto::sign::roster_signing_key(&vault_key);
    let roster = json!({ "version": 1, "counter": 0, "vault_key_epoch": 0 });
    let sig = yapstack_crypto::sign::sign_roster(&vault_key, roster.to_string().as_bytes());
    json!({
        "email": email,
        "auth_key": auth_key_b64,
        "recovery_auth_key": client_recovery_auth_key(&RECOVERY_BYTES),
        "salt_enc": B64.encode(salt_enc),
        "wrapped_vault_key_password": wrapped,
        "wrapped_vault_key_recovery": wrapped,
        "device_list": {
            "device_list": roster,
            "signature": B64.encode(sig),
            "counter": 0,
            "vault_key_epoch": 0,
            "client_id": uuid::Uuid::new_v4(),
            "ed25519_pub": B64.encode(sk.verifying_key().to_bytes()),
            "label": "test-device"
        }
    })
}

async fn post(app: &axum::Router, path: &str, body: Value) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

// ----------------------------------------------------------------- pool builders

fn database_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL for --ignored tests")
}

/// Owner pool (the migration owner / superuser). Used only to run migrations, exactly
/// as `main.rs` would were the owner and the app the same role.
async fn owner_pool() -> PgPool {
    let pool = yapstack_server::db::connect(&database_url()).await.unwrap();
    yapstack_server::db::migrate(&pool).await.unwrap();
    pool
}

/// The SERVING pool as a hardened deployment runs it: a non-owner `yapstack_app`
/// connection. `SET ROLE yapstack_app` from the superuser session drops the RLS bypass,
/// so this pool is subject to FORCE ROW LEVEL SECURITY just like a real
/// `postgres://yapstack_app:...@/db` connection would be.
async fn app_role_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(8)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET ROLE yapstack_app").execute(conn).await?;
                Ok(())
            })
        })
        .connect(&database_url())
        .await
        .unwrap()
}

fn test_config() -> Config {
    Config::from_toml_str(&format!(
        "database_url = \"{}\"\njwt_secret = \"test-secret\"\nserver_pepper = \"test-pepper\"\n",
        database_url()
    ))
    .unwrap()
}

// ============================================================================
// Finding #1 — login/finish + recover read FORCE-RLS workspace_members outside a
// tenant tx: EVERY login 401s under the non-owner serving role.
// ============================================================================

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn login_finish_succeeds_under_app_role() {
    // Migrate as owner; serve as the non-owner app role (the hardened / managed-PG case).
    let _owner = owner_pool().await;
    let app_pool = app_role_pool().await;
    let st = AppState::new(app_pool, test_config());
    let app = build_router(st);

    let email = format!("nonowner-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x22u8; 16];
    let auth_key = client_auth_key("correct horse battery staple", &salt_enc);

    // signup runs INSIDE begin_tenant_tx, so app.tenant_id IS set and RLS passes here.
    let resp = post(
        &app,
        "/auth/signup",
        signup_body(&email, &salt_enc, &auth_key),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "signup must succeed as the app role (it is inside a tenant tx)"
    );

    // login/begin is pre-tenant but touches only the non-RLS `users` table — fine.
    let resp = post(&app, "/auth/login/begin", json!({ "email": email })).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "login/begin (users only) should be OK"
    );

    // login/finish: the `users JOIN workspace_members` runs in AUTOCOMMIT (no tenant tx),
    // so app.tenant_id is unset -> RLS predicate is NULL -> 0 rows -> the handler returns
    // 401 for a CORRECT password. THIS ASSERTION IS THE RED TEST: it currently fails 401.
    let resp = post(
        &app,
        "/auth/login/finish",
        json!({ "email": email, "auth_key": auth_key }),
    )
    .await;
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "login/finish with the CORRECT password must succeed as the non-owner serving \
         role; it currently 401s because the login JOIN reads FORCE-RLS \
         workspace_members outside a tenant tx. body={body:?}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn recover_succeeds_under_app_role() {
    let _owner = owner_pool().await;
    let app_pool = app_role_pool().await;
    let st = AppState::new(app_pool, test_config());
    let app = build_router(st);

    let email = format!("recover-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x24u8; 16];
    let auth_key = client_auth_key("hunter2 hunter2 hunter2", &salt_enc);

    let resp = post(
        &app,
        "/auth/signup",
        signup_body(&email, &salt_enc, &auth_key),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "signup must succeed as the app role"
    );

    // /auth/recover: same `users JOIN workspace_members` in autocommit -> 0 rows -> 401.
    // The recovery path is the ONLY route a locked-out user has; it is permanently dead.
    let recovery_auth_key = client_recovery_auth_key(&RECOVERY_BYTES);
    let resp = post(
        &app,
        "/auth/recover",
        json!({ "email": email, "recovery_auth_key": recovery_auth_key }),
    )
    .await;
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "recover with the CORRECT recovery code must succeed as the non-owner serving \
         role; it currently 401s for the same RLS-outside-tenant-tx reason. body={body:?}"
    );
}

// ============================================================================
// Finding #3 — Hardened non-owner deployment cannot boot: sqlx migrate needs CREATE
// on schema public. `db::migrate` (main.rs runs it unconditionally every boot) calls
// sqlx `ensure_migrations_table` = `CREATE TABLE IF NOT EXISTS _sqlx_migrations (...)`,
// which PG rejects for `yapstack_app` (only USAGE on public) BEFORE the IF-NOT-EXISTS
// shortcut. => ExitCode::FAILURE on every boot => restart loop.
// ============================================================================

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn migrate_boots_under_app_role() {
    // Owner applies the schema first (creates the yapstack_app role + grants).
    let _owner = owner_pool().await;
    // Now the server boots pointed at the non-owner role (docs/self-hosting.md step 2)
    // and re-runs migrate on startup, exactly as main.rs does.
    let app_pool = app_role_pool().await;

    let res = yapstack_server::db::migrate(&app_pool).await;
    assert!(
        res.is_ok(),
        "db::migrate must succeed as the non-owner serving role so the relay can boot in \
         the documented hardened split; it currently fails with `permission denied for \
         schema public` at ensure_migrations_table (CREATE TABLE _sqlx_migrations). \
         err={res:?}"
    );
}

// ============================================================================
// Finding #2 — Relay serves as a Postgres superuser, so every FORCE-RLS tenant policy
// is INERT. The shipped compose default connects as POSTGRES_USER (a BYPASSRLS
// superuser). docs/self-hosting.md claims "tenant isolation holds regardless of which
// role the server connects as" — false: the ONLY isolation is the hand-written
// `workspace_id = $1` predicate. This test demonstrates the bypass empirically and,
// as a paired positive control, that the SAME query IS filtered under the app role.
// ============================================================================

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn serving_role_is_rls_subject() {
    // Seed one account/workspace as owner.
    let owner = owner_pool().await;
    let ws_a = uuid::Uuid::new_v4();
    let user_a = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, '')")
        .bind(ws_a)
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, email, verifier, server_salt, salt_enc, \
         wrapped_vault_key_password, wrapped_vault_key_recovery) \
         VALUES ($1, $2, '\\x01', '\\x02', '\\x03', '\\x04', '\\x05')",
    )
    .bind(user_a)
    .bind(format!("tenant-a-{user_a}@example.com"))
    .execute(&owner)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_members (workspace_id, user_id) VALUES ($1, $2)")
        .bind(ws_a)
        .bind(user_a)
        .execute(&owner)
        .await
        .unwrap();

    // The property the fix must hold is about the SERVING role, and it is genuinely
    // checkable from a single DSN: after `SET ROLE yapstack_app`, the connection must be
    // an RLS subject (no rolbypassrls) and a cross-tenant read must be filtered to 0.
    // (Asserting anything about the owner superuser's own rolbypassrls is meaningless —
    // from one DSN the migration owner IS the superuser regardless of the role split.)
    let ws_b = uuid::Uuid::new_v4();
    let app_pool = app_role_pool().await;
    let mut ac = app_pool.acquire().await.unwrap();
    let app_bypass: bool =
        sqlx::query("SELECT rolbypassrls FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&mut *ac)
            .await
            .unwrap()
            .get(0);
    sqlx::query("SELECT set_config('app.tenant_id', $1, false)")
        .bind(ws_b.to_string())
        .execute(&mut *ac)
        .await
        .unwrap();
    let app_visible: i64 =
        sqlx::query("SELECT count(*) FROM workspace_members WHERE workspace_id = $1")
            .bind(ws_a)
            .fetch_one(&mut *ac)
            .await
            .unwrap()
            .get(0);
    drop(ac);

    eprintln!("app role: rolbypassrls={app_bypass} cross_tenant_rows={app_visible}");
    assert!(
        !app_bypass,
        "the serving role must not carry rolbypassrls; otherwise FORCE-RLS tenant \
         isolation is inert and the ONLY isolation is the hand-written workspace_id \
         predicate"
    );
    assert_eq!(
        app_visible, 0,
        "FORCE RLS must hide tenant A's rows from a tenant-B app-role context; the serving \
         connection leaked another tenant's workspace_members rows despite app.tenant_id \
         pointing at a different workspace"
    );
}

// ============================================================================
// ADR 0006 §5.5 — the sync capability floor row works as the non-owner role, and one
// tenant can neither read nor raise another's floor.
// ============================================================================

async fn authed(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> axum::response::Response {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"));
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
    app.clone().oneshot(b.body(body).unwrap()).await.unwrap()
}

/// Sign up a fresh account as the app role; returns `(access_token, tenant_id, client_id)`.
async fn signup_tenant(app: &axum::Router) -> (String, uuid::Uuid, uuid::Uuid) {
    let email = format!("cap-{}@example.com", uuid::Uuid::new_v4());
    let salt_enc = [0x26u8; 16];
    let auth_key = client_auth_key("capability floor pw", &salt_enc);
    let body = signup_body(&email, &salt_enc, &auth_key);
    let client_id: uuid::Uuid = body["device_list"]["client_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let resp = post(app, "/auth/signup", body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    (
        v["access_token"].as_str().unwrap().to_string(),
        v["tenant_id"].as_str().unwrap().parse().unwrap(),
        client_id,
    )
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL"]
async fn sync_capability_floor_works_and_isolates_under_app_role() {
    let owner = owner_pool().await;
    let app_pool = app_role_pool().await;
    let app = build_router(AppState::new(app_pool.clone(), test_config()));

    let (tok_a, ws_a, dev_a) = signup_tenant(&app).await;
    let (tok_b, ws_b, _) = signup_tenant(&app).await;
    let level1 = [("x-yapstack-sync-capability", "1")];

    // Bootstrap + FOR SHARE (pull), then FOR UPDATE + GREATEST (raise), then FOR SHARE ahead
    // of tenant_seq (gated push): all as yapstack_app.
    let resp = authed(&app, "GET", "/sync/pull?since=0", &tok_a, &level1, None).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "pull bootstraps + locks the floor row"
    );
    let resp = authed(
        &app,
        "PUT",
        "/sync/capability",
        &tok_a,
        &level1,
        Some(json!({ "required_sync_capability": 1 })),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the raise works as the app role"
    );
    assert_eq!(body_json(resp).await["required_sync_capability"], 1);
    let change = json!({ "changes": [{
        "client_id": dev_a, "client_seq": 1, "ciphertext": B64.encode(b"gated"),
        "schema_version": 22, "engine_version": 16003
    }]});
    let resp = authed(
        &app,
        "POST",
        "/sync/push",
        &tok_a,
        &[("x-yapstack-batch-capability", "1")],
        Some(change),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a gated push at the floor is accepted"
    );

    // Tenant B is untouched by A's raise.
    let resp = authed(&app, "GET", "/sync/pull?since=0", &tok_b, &[], None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["required_sync_capability"], 0);

    // Direct SQL as the app role in tenant A's context: B's floor row is invisible, can't be
    // updated, and can't be inserted.
    let mut tx = yapstack_server::db::begin_tenant_tx(&app_pool, ws_a)
        .await
        .unwrap();
    let seen: i64 =
        sqlx::query("SELECT count(*) FROM tenant_sync_capability WHERE workspace_id = $1")
            .bind(ws_b)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
            .get(0);
    assert_eq!(seen, 0, "tenant A must not read tenant B's floor");
    let updated = sqlx::query(
        "UPDATE tenant_sync_capability SET required_sync_capability = 9 WHERE workspace_id = $1",
    )
    .bind(ws_b)
    .execute(&mut *tx)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(updated, 0, "tenant A must not raise tenant B's floor");
    tx.commit().await.unwrap();
    let mut tx = yapstack_server::db::begin_tenant_tx(&app_pool, ws_a)
        .await
        .unwrap();
    let insert = sqlx::query(
        "INSERT INTO tenant_sync_capability (workspace_id, required_sync_capability) \
         VALUES ($1, 9) ON CONFLICT DO NOTHING",
    )
    .bind(ws_b)
    .execute(&mut *tx)
    .await;
    assert!(
        insert.is_err(),
        "RLS WITH CHECK refuses another tenant's row"
    );
    drop(tx);

    let floor_b: i32 = sqlx::query(
        "SELECT required_sync_capability FROM tenant_sync_capability WHERE workspace_id = $1",
    )
    .bind(ws_b)
    .fetch_one(&owner)
    .await
    .unwrap()
    .get(0);
    assert_eq!(floor_b, 0);
}
