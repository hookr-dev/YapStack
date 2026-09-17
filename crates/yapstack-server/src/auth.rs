// SPDX-License-Identifier: AGPL-3.0-only
//! Auth endpoints per CRYPTO_SPEC §3.
//!
//! The server stores `verifier = Argon2id(auth_key, server_salt)` (§3.1) — never
//! `auth_key`, never the password, never any unwrapped key. Wrapped vault keys and
//! KDF salts are stored opaquely (inert without the password/recovery code) and
//! handed to bootstrapping devices. `tenant_id` is bound into JWTs at issuance and is
//! never request-supplied.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::{Duration, Utc};
use serde_json::Value;
use uuid::Uuid;
use zeroize::Zeroizing;

// The auth request/response DTOs are the ONE shared definition in
// `yapstack-common` (architecture §14): the desktop client and this relay compile
// against the same shapes so the wire contract can never skew. The handler logic —
// second-hash verifier, decoy salt, two-round login, JWT rotation/reuse — is
// unchanged; only the DTO's home moved.
use yapstack_common::auth::{
    ChangePasswordRequest, LoginBeginRequest, LoginBeginResponse, LoginFinishRequest,
    LoginFinishResponse, RecoverRequest, RecoverResponse, RefreshRequest, SignupRequest,
    TokenResponse,
};

use crate::db;
use crate::error::AppError;
use crate::extract::{AuthTenant, ClientIp};
use crate::jwt::REFRESH_TTL_SECS;
use crate::state::AppState;

// --------------------------------------------------------------------- helpers

fn b64_decode(field: &str, s: &str) -> Result<Vec<u8>, AppError> {
    B64.decode(s)
        .map_err(|_| AppError::BadRequest(format!("{field}: invalid base64")))
}

/// Constant-time byte comparison for the verifier check (no early-exit timing
/// oracle). Length mismatch returns false without leaking where.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn gen_salt() -> [u8; 16] {
    let mut s = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut s);
    s
}

// --------------------------------------------------------------------- signup

/// `POST /auth/signup`
pub async fn signup(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Json(req): Json<SignupRequest>,
) -> Result<Json<TokenResponse>, AppError> {
    // Per-IP throttle keyed on the trustworthy client IP (nil workspace), applied before
    // any DB work so a signup flood can't churn the box.
    if !st.signup_ratelimit.check(Uuid::nil(), &ip) {
        return Err(AppError::RateLimited);
    }
    // Optional invite gate: when `YAPSTACK_SIGNUP_INVITE` is set, require a matching
    // `X-YapStack-Invite` header (constant-time compare). Unset ⇒ open (default).
    if let Some(expected) = st.signup_invite.as_deref() {
        let provided = headers
            .get("x-yapstack-invite")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !ct_eq(provided.as_bytes(), expected.as_bytes()) {
            return Err(AppError::Unauthorized);
        }
    }

    // auth_key is secret key material (§2.3); hold it in memory that zeroizes on drop.
    let auth_key = Zeroizing::new(b64_decode("auth_key", &req.auth_key)?);
    // recovery_auth_key is likewise a second-hash INPUT (§6.2), not a stored secret;
    // zeroize it too and discard after computing the recovery verifier below.
    let recovery_auth_key =
        Zeroizing::new(b64_decode("recovery_auth_key", &req.recovery_auth_key)?);
    let salt_enc = b64_decode("salt_enc", &req.salt_enc)?;
    // Pin the client salt_enc length: real accounts store exactly 16 bytes, matching the
    // login/begin decoy salt, so a decoy can never be length-distinguished from a real one.
    if salt_enc.len() != 16 {
        return Err(AppError::BadRequest("salt_enc: expected 16 bytes".into()));
    }
    let wrapped_pw = b64_decode(
        "wrapped_vault_key_password",
        &req.wrapped_vault_key_password,
    )?;
    let wrapped_rec = b64_decode(
        "wrapped_vault_key_recovery",
        &req.wrapped_vault_key_recovery,
    )?;
    let signature = b64_decode("device_list.signature", &req.device_list.signature)?;
    let ed_pub = b64_decode("device_list.ed25519_pub", &req.device_list.ed25519_pub)?;

    // Server picks a fresh per-account server_salt and stores the SECOND hash (§3.1).
    // The Argon2id hash runs on the blocking pool under the concurrency semaphore
    // (see `AppState::server_verifier`); the spent `auth_key` moves in and zeroizes there.
    let server_salt = gen_salt();
    let verifier = st.server_verifier(auth_key, server_salt.to_vec()).await?;
    // The recovery code authenticates the SAME way (§6.2): an independent per-account
    // salt + second hash of the client-derived recovery auth key. Stored, then discarded
    // — the relay can verify a presented recovery code but never recover it or the code.
    let recovery_salt = gen_salt();
    let recovery_verifier = st
        .server_verifier(recovery_auth_key, recovery_salt.to_vec())
        .await?;

    let user_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();

    // One RLS guard primitive for the whole crate. Setting it up front is a no-op for the
    // non-RLS `users`/`workspaces` inserts below and arms the tenant-scoped ones after them.
    let mut tx = db::begin_tenant_tx(&st.pool, workspace_id).await?;

    // Identity bootstrap tables (non-RLS). Unique(lower(email)) enforces one account.
    let inserted = sqlx::query(
        "INSERT INTO users (id, email, verifier, server_salt, salt_enc, \
         wrapped_vault_key_password, wrapped_vault_key_recovery, \
         recovery_salt, recovery_verifier) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT ((lower(email))) DO NOTHING",
    )
    .bind(user_id)
    .bind(&req.email)
    .bind(verifier.as_slice())
    .bind(server_salt.as_slice())
    .bind(&salt_enc)
    .bind(&wrapped_pw)
    .bind(&wrapped_rec)
    .bind(recovery_salt.as_slice())
    .bind(recovery_verifier.as_slice())
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::Conflict("email already registered".into()));
    }

    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, $2)")
        .bind(workspace_id)
        .bind("")
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO devices (client_id, workspace_id, user_id, ed25519_pub, label, platform) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(req.device_list.client_id)
    .bind(workspace_id)
    .bind(user_id)
    .bind(&ed_pub)
    .bind(&req.device_list.label)
    .bind(&req.device_list.platform)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO device_roster (workspace_id, device_list, signature, counter, vault_key_epoch) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(workspace_id)
    .bind(&req.device_list.device_list)
    .bind(&signature)
    .bind(req.device_list.counter)
    .bind(req.device_list.vault_key_epoch)
    .execute(&mut *tx)
    .await?;

    let tokens = issue_pair(
        &st,
        &mut tx,
        user_id,
        workspace_id,
        Some(req.device_list.client_id),
    )
    .await?;

    tx.commit().await?;
    Ok(Json(tokens))
}

// ----------------------------------------------------------------- login begin

/// `POST /auth/login/begin` — round 1 of §3.2.
pub async fn login_begin(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(req): Json<LoginBeginRequest>,
) -> Result<Json<LoginBeginResponse>, AppError> {
    // Bounded on the general per-IP bucket (it now writes a nonce row per call), and the
    // stored email is length-capped (RFC 5321 maximum) so an unauthenticated caller cannot
    // grow `login_nonces` with megabyte strings. Applied before any lookup — no oracle.
    if !st.ratelimit.check(Uuid::nil(), &ip) {
        return Err(AppError::RateLimited);
    }
    if req.email.len() > 254 {
        return Err(AppError::BadRequest("email: too long".into()));
    }
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT salt_enc FROM users WHERE lower(email) = lower($1)")
            .bind(&req.email)
            .fetch_optional(&st.pool)
            .await?;

    let salt = match row {
        Some((salt_enc,)) => salt_enc,
        None => {
            // Decoy salt = HKDF(server_pepper, email) — deterministic per email, so a
            // probe cannot distinguish "unknown account" from "real salt".
            yapstack_crypto::hkdf::expand(
                st.config.server_pepper.as_bytes(),
                req.email.to_lowercase().as_bytes(),
                16,
            )
        }
    };
    // A10 proof-of-possession nonce: issued for known AND unknown emails alike (no
    // oracle), single-use, 5-minute validity, pruned after 10.
    let mut nonce = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let _ = sqlx::query("DELETE FROM login_nonces WHERE issued_at < now() - interval '10 minutes'")
        .execute(&st.pool)
        .await;
    sqlx::query("INSERT INTO login_nonces (nonce, email_lower) VALUES ($1, $2)")
        .bind(nonce.as_slice())
        .bind(req.email.to_lowercase())
        .execute(&st.pool)
        .await?;
    Ok(Json(LoginBeginResponse {
        salt_enc: B64.encode(salt),
        nonce: Some(B64.encode(nonce)),
    }))
}

/// A10 proof of possession. Consumes the nonce (single-use, 5 min, bound to this email)
/// and verifies the device's signature over `login_pop_message` against the STORED public
/// key when a row for `client_id` exists in this tenant (identity substitution fails even
/// if the request presents a different key) or the presented key when the device is new.
/// Returns the decoded public key on success. Any failure is a uniform 401.
/// The A10 proof-of-possession fields a login/recover request carries (all base64).
struct LoginPop<'a> {
    client_id: Uuid,
    presented_pub_b64: &'a str,
    nonce_b64: &'a str,
    proof_b64: &'a str,
}

async fn verify_login_pop(
    pool: &sqlx::PgPool,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    email: &str,
    pop: LoginPop<'_>,
) -> Result<Vec<u8>, AppError> {
    let LoginPop {
        client_id,
        presented_pub_b64,
        nonce_b64,
        proof_b64,
    } = pop;
    let presented = b64_decode("ed25519_pub", presented_pub_b64)?;
    if presented.len() != 32 {
        return Err(AppError::BadRequest(
            "ed25519_pub: expected 32 bytes".into(),
        ));
    }
    let nonce = B64.decode(nonce_b64).map_err(|_| AppError::Unauthorized)?;
    let proof: [u8; 64] = B64
        .decode(proof_b64)
        .map_err(|_| AppError::Unauthorized)?
        .try_into()
        .map_err(|_| AppError::Unauthorized)?;
    // Consume-as-check: exactly one fresh row for this nonce AND this email. Runs in its
    // own autocommit statement on the pool — NOT inside the caller's transaction — so a
    // proof that then fails still burns the nonce (single attempt, not single success).
    let consumed = sqlx::query(
        "DELETE FROM login_nonces WHERE nonce = $1 AND email_lower = $2 \
           AND issued_at > now() - interval '5 minutes'",
    )
    .bind(&nonce)
    .bind(email.to_lowercase())
    .execute(pool)
    .await?;
    if consumed.rows_affected() != 1 {
        return Err(AppError::Unauthorized);
    }
    let stored: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT ed25519_pub FROM devices WHERE workspace_id = $1 AND client_id = $2",
    )
    .bind(workspace_id)
    .bind(client_id)
    .fetch_optional(&mut **tx)
    .await?;
    let key_bytes = stored.map_or(presented.clone(), |(k,)| k);
    let key: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| AppError::Unauthorized)?;
    let presented_arr: [u8; 32] = presented.as_slice().try_into().unwrap_or([0u8; 32]);
    let message = yapstack_crypto::sign::login_pop_message(
        &nonce,
        client_id.as_bytes(),
        &presented_arr,
        &email.to_lowercase(),
    );
    yapstack_crypto::sign::verify_login_pop(&key, &message, &proof)
        .map_err(|_| AppError::Unauthorized)?;
    Ok(key.to_vec())
}

// ---------------------------------------------------------------- login finish

#[derive(sqlx::FromRow)]
struct AuthRow {
    id: Uuid,
    workspace_id: Uuid,
    verifier: Vec<u8>,
    server_salt: Vec<u8>,
    salt_enc: Vec<u8>,
    wrapped_vault_key_password: Vec<u8>,
}

/// `POST /auth/login/finish` — round 2 of §3.2.
pub async fn login_finish(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(req): Json<LoginFinishRequest>,
) -> Result<Json<LoginFinishResponse>, AppError> {
    // Per-IP throttle keyed on the trustworthy client IP (nil workspace), applied BEFORE
    // the user lookup so it is identical for known and unknown emails (no user-existence
    // oracle) and caps online verifier-guessing.
    if !st.login_ratelimit.check(Uuid::nil(), &ip) {
        return Err(AppError::RateLimited);
    }

    // Secret key material (§2.3): zeroize on drop rather than leaving it in a plain Vec.
    let auth_key = Zeroizing::new(b64_decode("auth_key", &req.auth_key)?);

    // Login is PRE-tenant: no app.tenant_id context exists yet, so the FORCE-RLS
    // `workspace_members` cannot be read directly under the non-owner serving role.
    // Resolve the (user, workspace) mapping via the SECURITY DEFINER lookup, then read
    // the non-RLS `users` columns. `yapstack_lookup_login` returns a row set (v1: one
    // workspace per user), so `fetch_optional` takes the single expected row.
    let row: Option<AuthRow> = sqlx::query_as(
        "SELECT u.id, l.workspace_id, u.verifier, u.server_salt, u.salt_enc, \
         u.wrapped_vault_key_password \
         FROM yapstack_lookup_login($1) l JOIN users u ON u.id = l.user_id",
    )
    .bind(&req.email)
    .fetch_optional(&st.pool)
    .await?;

    // Uniform failure for unknown user AND bad verifier (no oracle). We still compute
    // a verifier against a dummy salt on the unknown path to reduce timing skew.
    let Some(row) = row else {
        let _ = st.server_verifier(auth_key, gen_salt().to_vec()).await;
        return Err(AppError::Unauthorized);
    };

    let computed = st
        .server_verifier(auth_key, row.server_salt.clone())
        .await?;
    if !ct_eq(&computed, &row.verifier) {
        return Err(AppError::Unauthorized);
    }

    let mut tx = db::begin_tenant_tx(&st.pool, row.workspace_id).await?;

    // Fetch the signed roster + its signature for bootstrap (§7.5 step 2).
    let roster: Option<(Value, Vec<u8>)> = fetch_roster(&mut tx, row.workspace_id).await?;

    // Enroll-as-PENDING (§7.5 step 1): a NEW device that presents an Ed25519 pubkey and
    // is not already in the devices table is inserted PENDING — authenticated to the
    // account but NOT yet a signed-roster sync peer. No auto-promotion; ON CONFLICT DO
    // NOTHING never demotes an already-active/pending device. The roster (the crypto
    // source of truth) is untouched here — promotion happens only via PUT /devices/roster.
    // A10: a device identity is honoured ONLY with a valid proof of possession. A request
    // that names a device without one (an older client) is treated as device-less: no
    // enrolment, tokens without `client_id`, refused by every write gate.
    let mut bound_client: Option<Uuid> = None;
    if let (Some(cid), Some(pub_b64), Some(nonce), Some(proof)) = (
        req.client_id,
        req.ed25519_pub.as_deref(),
        req.nonce.as_deref(),
        req.proof.as_deref(),
    ) {
        let ed_pub = verify_login_pop(
            &st.pool,
            &mut tx,
            row.workspace_id,
            &req.email,
            LoginPop {
                client_id: cid,
                presented_pub_b64: pub_b64,
                nonce_b64: nonce,
                proof_b64: proof,
            },
        )
        .await?;
        bound_client = Some(cid);
        // A REVOKED device (removed by another device, §15.D) that signs in again with the
        // password re-enrols PENDING under the same identity and must be approved anew;
        // every other existing row (pending/active) is left exactly as it is.
        let touched = sqlx::query(
            "INSERT INTO devices (client_id, workspace_id, user_id, ed25519_pub, label, platform, status) \
             VALUES ($1, $2, $3, $4, $5, $6, 'pending') \
             ON CONFLICT (client_id) DO UPDATE \
             SET status = CASE WHEN devices.status = 'revoked' THEN 'pending' \
                               ELSE devices.status END \
             WHERE devices.workspace_id = EXCLUDED.workspace_id \
               AND devices.user_id = EXCLUDED.user_id",
        )
        .bind(cid)
        .bind(row.workspace_id)
        .bind(row.id)
        .bind(&ed_pub)
        .bind(req.label.clone().unwrap_or_default())
        .bind(req.platform.clone().unwrap_or_default())
        .execute(&mut *tx)
        .await?;
        // `client_id` is a global PK: a row under ANOTHER account is refused, never bound.
        if touched.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "client_id is already enrolled under a different account".into(),
            ));
        }
    }

    let tokens = issue_pair(&st, &mut tx, row.id, row.workspace_id, bound_client).await?;
    tx.commit().await?;

    let (device_list, signature) = match roster {
        Some((v, sig)) => (Some(v), Some(B64.encode(sig))),
        None => (None, None),
    };
    Ok(Json(LoginFinishResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        tenant_id: row.workspace_id,
        salt_enc: B64.encode(&row.salt_enc),
        wrapped_vault_key_password: B64.encode(&row.wrapped_vault_key_password),
        device_list,
        signature,
    }))
}

/// The one definition of the roster read. Borrows the CALLER's tenant transaction — login
/// and recover each already open one, so the roster costs no extra pool checkout.
async fn fetch_roster(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
) -> Result<Option<(Value, Vec<u8>)>, AppError> {
    let roster: Option<(Value, Vec<u8>)> =
        sqlx::query_as("SELECT device_list, signature FROM device_roster WHERE workspace_id = $1")
            .bind(workspace_id)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(roster)
}

// --------------------------------------------------------------------- recover

#[derive(sqlx::FromRow)]
struct RecoverRow {
    id: Uuid,
    workspace_id: Uuid,
    recovery_salt: Option<Vec<u8>>,
    recovery_verifier: Option<Vec<u8>>,
    salt_enc: Vec<u8>,
    wrapped_vault_key_recovery: Vec<u8>,
}

/// `POST /auth/recover` — authenticate with the recovery code (§6.2) and serve
/// `wrapped_vault_key_recovery` (the blob the relay stores at signup but NEVER serves on
/// the login path). Recovery auth is the SAME second-hash/constant-time check as the
/// password verifier (§3.1): no recovery/account-existence oracle. Per-IP rate limited.
pub async fn recover(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(req): Json<RecoverRequest>,
) -> Result<Json<RecoverResponse>, AppError> {
    // Defense-in-depth throttle on recovery attempts (recovery codes are 160-bit and not
    // brute-forceable, but rate-limit anyway). A stable per-IP bucket (nil workspace).
    if !st.ratelimit.check(Uuid::nil(), &ip) {
        return Err(AppError::RateLimited);
    }

    let recovery_auth_key =
        Zeroizing::new(b64_decode("recovery_auth_key", &req.recovery_auth_key)?);

    // Same pre-tenant resolution as login/finish: the SECURITY DEFINER lookup bridges
    // the FORCE-RLS `workspace_members` before a tenant context exists (row set; v1
    // single workspace per user), then the non-RLS `users` columns are read.
    let row: Option<RecoverRow> = sqlx::query_as(
        "SELECT u.id, l.workspace_id, u.recovery_salt, u.recovery_verifier, u.salt_enc, \
         u.wrapped_vault_key_recovery \
         FROM yapstack_lookup_login($1) l JOIN users u ON u.id = l.user_id",
    )
    .bind(&req.email)
    .fetch_optional(&st.pool)
    .await?;

    // Uniform failure for unknown user AND bad recovery code (no oracle), computing a
    // dummy hash on every reject path to flatten timing.
    let Some(row) = row else {
        let _ = st
            .server_verifier(recovery_auth_key, gen_salt().to_vec())
            .await;
        return Err(AppError::Unauthorized);
    };
    let (Some(rec_salt), Some(rec_verifier)) = (row.recovery_salt, row.recovery_verifier) else {
        let _ = st
            .server_verifier(recovery_auth_key, gen_salt().to_vec())
            .await;
        return Err(AppError::Unauthorized);
    };

    let computed = st.server_verifier(recovery_auth_key, rec_salt).await?;
    if !ct_eq(&computed, &rec_verifier) {
        return Err(AppError::Unauthorized);
    }

    let mut tx = db::begin_tenant_tx(&st.pool, row.workspace_id).await?;

    // Authenticated. Serve the recovery-wrapped vault key + roster for the client to
    // unwrap locally and re-establish. The relay never unwraps it (no vault key).
    let roster: Option<(Value, Vec<u8>)> = fetch_roster(&mut tx, row.workspace_id).await?;

    // A7 recovery re-bootstrap: a recovering device that presents its identity is enrolled
    // like a password login (PENDING, never demoting an existing row) AND receives a
    // one-shot, time-boxed `recovery_grant_at` — the recovery code proved it holds the
    // vault key, so it may author the next roster itself even when no active device is
    // left to approve it (`devices::put_roster`). An already-active device keeps its
    // status; a device that was pending (password login, never approved) gains the grant
    // and can self-approve. A REVOKED device (removed by another device, §15.D) is
    // re-enrolled PENDING but gets NO grant: the account demonstrably has an approver,
    // so the lost-only-device rationale does not apply and re-admission goes through
    // approval. `client_id` is a GLOBAL primary key, so the conflict arm is scoped to
    // this account (RLS is the second line, not the only one): a `client_id` enrolled
    // under another account is refused rather than armed.
    let mut bound_client: Option<Uuid> = None;
    if let (Some(cid), Some(pub_b64), Some(nonce), Some(proof)) = (
        req.client_id,
        req.ed25519_pub.as_deref(),
        req.nonce.as_deref(),
        req.proof.as_deref(),
    ) {
        // A10: the recovering device proves it holds its key (against the stored key if
        // it is already enrolled), else it is treated as device-less like an old client.
        let ed_pub = verify_login_pop(
            &st.pool,
            &mut tx,
            row.workspace_id,
            &req.email,
            LoginPop {
                client_id: cid,
                presented_pub_b64: pub_b64,
                nonce_b64: nonce,
                proof_b64: proof,
            },
        )
        .await?;
        bound_client = Some(cid);
        let armed = sqlx::query(
            "INSERT INTO devices \
             (client_id, workspace_id, user_id, ed25519_pub, label, platform, status, \
              recovery_grant_at, recovered_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 'pending', now(), now()) \
             ON CONFLICT (client_id) DO UPDATE \
             SET status = CASE WHEN devices.status = 'revoked' THEN 'pending' \
                               ELSE devices.status END, \
                 recovery_grant_at = CASE WHEN devices.status = 'revoked' THEN NULL \
                                          ELSE now() END, \
                 recovered_at = now() \
             WHERE devices.workspace_id = EXCLUDED.workspace_id \
               AND devices.user_id = EXCLUDED.user_id",
        )
        .bind(cid)
        .bind(row.workspace_id)
        .bind(row.id)
        .bind(&ed_pub)
        .bind(req.label.clone().unwrap_or_default())
        .bind(req.platform.clone().unwrap_or_default())
        .execute(&mut *tx)
        .await?;
        if armed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "client_id is already enrolled under a different account".into(),
            ));
        }
    }

    let tokens = issue_pair(&st, &mut tx, row.id, row.workspace_id, bound_client).await?;
    tx.commit().await?;

    let (device_list, signature) = match roster {
        Some((v, sig)) => (Some(v), Some(B64.encode(sig))),
        None => (None, None),
    };
    Ok(Json(RecoverResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        tenant_id: row.workspace_id,
        salt_enc: B64.encode(&row.salt_enc),
        wrapped_vault_key_recovery: B64.encode(&row.wrapped_vault_key_recovery),
        device_list,
        signature,
    }))
}

// ------------------------------------------------------------- change password

/// `PUT /auth/password` — CRYPTO_SPEC §4.3 as amended by A9: the account's `salt_enc` is
/// RETAINED; the relay atomically replaces the password verifier (fresh `server_salt`) and
/// the password wrap of the vault key. Nothing else changes: the recovery wrap, the
/// roster, other devices' sessions. Authorization: an ACTIVE caller device that either
/// re-authenticates with its current `auth_key` or recovered with the recovery code in the
/// last 15 minutes (`devices.recovered_at`). Throttled per (tenant, IP) on the
/// authenticated bucket (it runs Argon2, but the caller already holds a valid token, so
/// the unauthenticated login bucket's tighter budget would only punish legitimate use).
pub async fn change_password(
    State(st): State<AppState>,
    auth: AuthTenant,
    ClientIp(ip): ClientIp,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !st.ratelimit.check(auth.tenant_id, &ip) {
        return Err(AppError::RateLimited);
    }
    let new_auth_key = Zeroizing::new(b64_decode("auth_key", &req.auth_key)?);
    let new_wrap = b64_decode(
        "wrapped_vault_key_password",
        &req.wrapped_vault_key_password,
    )?;
    // Shape checks so a buggy client cannot brick password login with truncated material:
    // `auth_key` is a 32-byte HKDF output (§2.3); the wrap is a committing envelope of a
    // 32-byte key = 1 + 32 + 24 + 32 + 16 bytes (§1.3/§4.2).
    if new_auth_key.len() != 32 {
        return Err(AppError::BadRequest("auth_key: expected 32 bytes".into()));
    }
    if new_wrap.len() != 105 {
        return Err(AppError::BadRequest(
            "wrapped_vault_key_password: expected a 105-byte committing envelope".into(),
        ));
    }
    let current = req
        .current_auth_key
        .as_deref()
        .map(|s| b64_decode("current_auth_key", s).map(Zeroizing::new))
        .transpose()?;

    let mut tx = db::begin_tenant_tx(&st.pool, auth.tenant_id).await?;
    crate::devices::require_active_caller(&mut tx, auth.tenant_id, auth.client_id).await?;

    // The user row is looked up by the token's `sub` (server-validated), never by email,
    // and LOCKED so two concurrent changes serialise on the verifier they check against.
    let row: Option<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT verifier, server_salt FROM users WHERE id = $1 FOR UPDATE")
            .bind(auth.user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((verifier, server_salt)) = row else {
        return Err(AppError::Unauthorized);
    };

    match current {
        // Re-authentication: the presented current key must verify (constant time).
        Some(cur) => {
            let computed = st.server_verifier(cur, server_salt).await?;
            if !ct_eq(&computed, &verifier) {
                return Err(AppError::Unauthorized);
            }
        }
        // Forgotten password: only a device that recovered with the code just now may
        // skip re-authentication. The consume IS the check — one UPDATE that clears the
        // stamp only while it is fresh — so the waiver is single-use even under two
        // concurrent keyless calls (the second finds it already cleared).
        None => {
            let consumed = sqlx::query(
                "UPDATE devices SET recovered_at = NULL \
                 WHERE workspace_id = $1 AND client_id = $2 \
                   AND recovered_at IS NOT NULL \
                   AND recovered_at > now() - interval '15 minutes'",
            )
            .bind(auth.tenant_id)
            .bind(auth.client_id)
            .execute(&mut *tx)
            .await?;
            if consumed.rows_affected() != 1 {
                return Err(AppError::Unauthorized);
            }
        }
    }

    // New second hash under a FRESH server salt; the new auth key is discarded after.
    let new_server_salt = gen_salt();
    let new_verifier = st
        .server_verifier(new_auth_key, new_server_salt.to_vec())
        .await?;
    sqlx::query(
        "UPDATE users SET verifier = $2, server_salt = $3, wrapped_vault_key_password = $4 \
         WHERE id = $1",
    )
    .bind(auth.user_id)
    .bind(new_verifier.as_slice())
    .bind(new_server_salt.as_slice())
    .bind(&new_wrap)
    .execute(&mut *tx)
    .await?;
    // A password change is the one lever the owner has against a device enrolled with
    // the OLD password (device removal and vault-key rotation are later slices): revoke
    // every OTHER device's refresh families so they must sign in again — with the new
    // password. Devices that already unwrapped the vault key keep what they hold (E2E
    // cannot take that back); what they lose is continued relay access. The caller's own
    // family is kept so the change does not sign this device out. A re-authenticated
    // caller's own waiver is also cleared (a stale stamp never outlives a change).
    sqlx::query(
        "UPDATE refresh_tokens SET revoked = true \
         WHERE workspace_id = $1 AND client_id IS DISTINCT FROM $2 AND NOT revoked",
    )
    .bind(auth.tenant_id)
    .bind(auth.client_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE devices SET recovered_at = NULL \
         WHERE workspace_id = $1 AND client_id = $2 AND recovered_at IS NOT NULL",
    )
    .bind(auth.tenant_id)
    .bind(auth.client_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// --------------------------------------------------------------------- refresh

/// `POST /auth/refresh` — rotation + reuse detection, per-device families
/// (architecture §5/§10). `tenant` comes from the validated token, never the request.
pub async fn refresh(
    State(st): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<TokenResponse>, AppError> {
    let claims = st
        .jwt
        .verify_refresh(&req.refresh_token)
        .map_err(|_| AppError::Unauthorized)?;

    let mut tx = db::begin_tenant_tx(&st.pool, claims.tenant).await?;

    // FOR UPDATE serializes concurrent uses of the SAME token within their txs: the
    // row lock is taken here and held to commit alongside the rotate/revoke below.
    // Two concurrent uses of a still-fresh token can no longer both read
    // rotated_at=NULL — the second blocks until the first commits, then (READ
    // COMMITTED re-reads the locked row) sees rotated_at set → reuse detected →
    // whole family revoked. Prevents a stolen fresh token forking an undetected chain.
    let existing: Option<(Option<chrono::DateTime<Utc>>, bool, Uuid)> = sqlx::query_as(
        "SELECT rotated_at, revoked, family_id FROM refresh_tokens \
         WHERE id = $1 AND workspace_id = $2 FOR UPDATE",
    )
    .bind(claims.jti)
    .bind(claims.tenant)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((rotated_at, revoked, family_id)) = existing else {
        return Err(AppError::Unauthorized);
    };

    // REUSE DETECTION: a token that was already rotated (or a revoked one) being
    // presented again means the family is compromised — revoke the entire family.
    if revoked || rotated_at.is_some() {
        sqlx::query("UPDATE refresh_tokens SET revoked = true WHERE family_id = $1")
            .bind(family_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(AppError::Unauthorized);
    }

    let new_jti = Uuid::new_v4();
    // Rotate: mark the presented token spent, chained to its replacement.
    sqlx::query("UPDATE refresh_tokens SET rotated_at = now(), replaced_by = $1 WHERE id = $2")
        .bind(new_jti)
        .bind(claims.jti)
        .execute(&mut *tx)
        .await?;

    let expires_at = Utc::now() + Duration::seconds(REFRESH_TTL_SECS);
    sqlx::query(
        "INSERT INTO refresh_tokens (id, workspace_id, user_id, client_id, family_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(new_jti)
    .bind(claims.tenant)
    .bind(claims.sub)
    .bind(claims.client_id)
    .bind(family_id)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;

    let access = st
        .jwt
        .issue_access(claims.sub, claims.tenant, claims.client_id)
        .map_err(|e| AppError::Internal(format!("jwt: {e}")))?;
    let refresh_token = st
        .jwt
        .issue_refresh(
            claims.sub,
            claims.tenant,
            claims.client_id,
            family_id,
            new_jti,
        )
        .map_err(|e| AppError::Internal(format!("jwt: {e}")))?;

    tx.commit().await?;
    Ok(Json(TokenResponse {
        access_token: access,
        refresh_token,
        tenant_id: claims.tenant,
    }))
}

/// Issue an access+refresh pair and persist the refresh row (new family per login).
async fn issue_pair(
    st: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    workspace_id: Uuid,
    client_id: Option<Uuid>,
) -> Result<TokenResponse, AppError> {
    let family_id = Uuid::new_v4();
    let jti = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::seconds(REFRESH_TTL_SECS);

    sqlx::query(
        "INSERT INTO refresh_tokens (id, workspace_id, user_id, client_id, family_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(jti)
    .bind(workspace_id)
    .bind(user_id)
    .bind(client_id)
    .bind(family_id)
    .bind(expires_at)
    .execute(&mut **tx)
    .await?;

    let access = st
        .jwt
        .issue_access(user_id, workspace_id, client_id)
        .map_err(|e| AppError::Internal(format!("jwt: {e}")))?;
    let refresh_token = st
        .jwt
        .issue_refresh(user_id, workspace_id, client_id, family_id, jti)
        .map_err(|e| AppError::Internal(format!("jwt: {e}")))?;

    Ok(TokenResponse {
        access_token: access,
        refresh_token,
        tenant_id: workspace_id,
    })
}

#[cfg(test)]
mod tests {
    use super::ct_eq;

    #[test]
    fn ct_eq_matches_semantics() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }
}
