// SPDX-License-Identifier: AGPL-3.0-only
//! Auth endpoints per CRYPTO_SPEC §3.
//!
//! The server stores `verifier = Argon2id(auth_key, server_salt)` (§3.1) — never
//! `auth_key`, never the password, never any unwrapped key. Wrapped vault keys and
//! KDF salts are stored opaquely (inert without the password/recovery code) and
//! handed to bootstrapping devices. `tenant_id` is bound into JWTs at issuance and is
//! never request-supplied.

use axum::extract::State;
use axum::Json;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::db;
use crate::error::AppError;
use crate::jwt::REFRESH_TTL_SECS;
use crate::state::AppState;

// --------------------------------------------------------------------- helpers

fn b64_decode(field: &str, s: &str) -> Result<Vec<u8>, AppError> {
    B64.decode(s)
        .map_err(|_| AppError::BadRequest(format!("{field}: invalid base64")))
}

/// Constant-time byte comparison for the verifier check (no early-exit timing
/// oracle). Length mismatch returns false without leaking where.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
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

#[derive(Debug, Deserialize)]
pub struct SignupRequest {
    pub email: String,
    /// base64 of the 32-byte `auth_key` (§2.3). Discarded after computing `verifier`.
    pub auth_key: String,
    pub salt_enc: String,
    pub wrapped_vault_key_password: String,
    pub wrapped_vault_key_recovery: String,
    /// First-device self-enrolled signed roster (§3.2 C2): counter 0, epoch 0.
    pub device_list: RosterEnvelope,
}

#[derive(Debug, Deserialize)]
pub struct RosterEnvelope {
    /// Opaque signed roster body (§7.3). Stored verbatim; the server never authors it.
    pub device_list: Value,
    pub signature: String, // base64 Ed25519 signature
    pub counter: i64,
    pub vault_key_epoch: i64,
    /// The enrolling device (§7.1 fresh UUIDv4) and its Ed25519 public key.
    pub client_id: Uuid,
    pub ed25519_pub: String, // base64
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub tenant_id: Uuid,
}

/// `POST /auth/signup`
pub async fn signup(
    State(st): State<AppState>,
    Json(req): Json<SignupRequest>,
) -> Result<Json<TokenResponse>, AppError> {
    // auth_key is secret key material (§2.3); hold it in memory that zeroizes on drop.
    let auth_key = Zeroizing::new(b64_decode("auth_key", &req.auth_key)?);
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
    let server_salt = gen_salt();
    let verifier = yapstack_crypto::kdf::server_verifier(&auth_key, &server_salt)
        .map_err(|e| AppError::Internal(format!("verifier: {e}")))?;
    // auth_key is now spent; drop it (Zeroizing wipes the bytes, never persisted §3.2).
    drop(auth_key);

    let user_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();

    let mut tx = st.pool.begin().await?;

    // Identity bootstrap tables (non-RLS). Unique(lower(email)) enforces one account.
    let inserted = sqlx::query(
        "INSERT INTO users (id, email, verifier, server_salt, salt_enc, \
         wrapped_vault_key_password, wrapped_vault_key_recovery) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT ((lower(email))) DO NOTHING",
    )
    .bind(user_id)
    .bind(&req.email)
    .bind(verifier.as_slice())
    .bind(server_salt.as_slice())
    .bind(&salt_enc)
    .bind(&wrapped_pw)
    .bind(&wrapped_rec)
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

    // Enter the tenant RLS context for the tenant-scoped inserts below.
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(workspace_id.to_string())
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
        "INSERT INTO devices (client_id, workspace_id, user_id, ed25519_pub, label) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(req.device_list.client_id)
    .bind(workspace_id)
    .bind(user_id)
    .bind(&ed_pub)
    .bind(&req.device_list.label)
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

#[derive(Debug, Deserialize)]
pub struct LoginBeginRequest {
    pub email: String,
}

#[derive(Debug, Serialize)]
pub struct LoginBeginResponse {
    /// base64 of the account's `salt_enc`, OR a deterministic DECOY salt for an
    /// unknown email (no account-existence oracle, §3.2).
    pub salt_enc: String,
}

/// `POST /auth/login/begin` — round 1 of §3.2.
pub async fn login_begin(
    State(st): State<AppState>,
    Json(req): Json<LoginBeginRequest>,
) -> Result<Json<LoginBeginResponse>, AppError> {
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
    Ok(Json(LoginBeginResponse {
        salt_enc: B64.encode(salt),
    }))
}

// ---------------------------------------------------------------- login finish

#[derive(Debug, Deserialize)]
pub struct LoginFinishRequest {
    pub email: String,
    pub auth_key: String,
    #[serde(default)]
    pub client_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct LoginFinishResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub tenant_id: Uuid,
    pub salt_enc: String,
    pub wrapped_vault_key_password: String,
    pub device_list: Option<Value>,
}

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
    Json(req): Json<LoginFinishRequest>,
) -> Result<Json<LoginFinishResponse>, AppError> {
    // Secret key material (§2.3): zeroize on drop rather than leaving it in a plain Vec.
    let auth_key = Zeroizing::new(b64_decode("auth_key", &req.auth_key)?);

    let row: Option<AuthRow> = sqlx::query_as(
        "SELECT u.id, m.workspace_id, u.verifier, u.server_salt, u.salt_enc, \
         u.wrapped_vault_key_password \
         FROM users u JOIN workspace_members m ON m.user_id = u.id \
         WHERE lower(u.email) = lower($1)",
    )
    .bind(&req.email)
    .fetch_optional(&st.pool)
    .await?;

    // Uniform failure for unknown user AND bad verifier (no oracle). We still compute
    // a verifier against a dummy salt on the unknown path to reduce timing skew.
    let Some(row) = row else {
        let _ = yapstack_crypto::kdf::server_verifier(&auth_key, &gen_salt());
        return Err(AppError::Unauthorized);
    };

    let computed = yapstack_crypto::kdf::server_verifier(&auth_key, &row.server_salt)
        .map_err(|e| AppError::Internal(format!("verifier: {e}")))?;
    if !ct_eq(&computed, &row.verifier) {
        return Err(AppError::Unauthorized);
    }

    // Fetch the signed roster for bootstrap (§7.5).
    let roster: Option<(Value,)> = db_fetch_roster(&st, row.workspace_id).await?;

    let mut tx = st.pool.begin().await?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(row.workspace_id.to_string())
        .execute(&mut *tx)
        .await?;
    let tokens = issue_pair(&st, &mut tx, row.id, row.workspace_id, req.client_id).await?;
    tx.commit().await?;

    Ok(Json(LoginFinishResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        tenant_id: row.workspace_id,
        salt_enc: B64.encode(&row.salt_enc),
        wrapped_vault_key_password: B64.encode(&row.wrapped_vault_key_password),
        device_list: roster.map(|(v,)| v),
    }))
}

async fn db_fetch_roster(st: &AppState, workspace_id: Uuid) -> Result<Option<(Value,)>, AppError> {
    let mut tx = db::begin_tenant_tx(&st.pool, workspace_id).await?;
    let roster: Option<(Value,)> =
        sqlx::query_as("SELECT device_list FROM device_roster WHERE workspace_id = $1")
            .bind(workspace_id)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(roster)
}

// --------------------------------------------------------------------- refresh

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

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
        .issue_access(claims.sub, claims.tenant)
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
        .issue_access(user_id, workspace_id)
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
