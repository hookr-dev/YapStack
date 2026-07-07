// SPDX-License-Identifier: AGPL-3.0-only
//! Auth-ceremony wire DTOs shared by the desktop client and the blind relay
//! (architecture §14, CRYPTO_SPEC §3/§4/§6/§7).
//!
//! ONE definition, both stacks: the relay (`yapstack-server`) and the desktop
//! sync client compile against these exact shapes so the JSON contract can never
//! skew between producer and consumer. Every type derives BOTH `Serialize` and
//! `Deserialize` because each side needs the opposite direction of the same
//! struct (the client serializes requests / deserializes responses; the server
//! does the reverse).
//!
//! These carry only what the spec permits the relay to see: the `auth_key`
//! (a second-hash verifier input the server hashes-then-discards, §3.1), KDF
//! salts, and OPAQUE wrapped-key / signed-roster blobs. They NEVER carry the
//! password or the `master_key`/`vault_key` in the clear (§3.2). `wrapped_*`
//! and `device_list` fields are base64 / opaque JSON the relay stores verbatim
//! and cannot decrypt or forge.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// --------------------------------------------------------------------- signup

/// `POST /auth/signup` body (§3.2). The client derives `auth_key` client-side and
/// hands it over ONCE; the server second-hashes it into a `verifier` and discards
/// it. `wrapped_vault_key_*` are committing envelopes (§4.2/§6.2) inert without the
/// password / recovery code. `device_list` is the first-device self-enrolled signed
/// roster (§3.2 C2: counter 0, epoch 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// A signed device-roster envelope (§7.3). The server stores `device_list` (opaque
/// JSON) and `signature` verbatim and never authors or forges it (it holds no vault
/// key). `counter`/`vault_key_epoch` are the anti-rollback watermarks (§7.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Access+refresh token pair returned by signup / refresh. `tenant_id` is bound into
/// the JWTs at issuance and is never request-supplied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub tenant_id: Uuid,
}

// ----------------------------------------------------------------- login begin

/// `POST /auth/login/begin` — round 1 of §3.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginBeginRequest {
    pub email: String,
}

/// Round-1 response: the account's `salt_enc`, OR a deterministic DECOY salt for an
/// unknown email (no account-existence oracle, §3.2). The client caches its own
/// `salt_enc` and MUST alert on mismatch for a known device (§3.2 C3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginBeginResponse {
    /// base64 of the served `salt_enc`.
    pub salt_enc: String,
}

// ---------------------------------------------------------------- login finish

/// `POST /auth/login/finish` — round 2 of §3.2. `auth_key` is derived client-side
/// from the password + the round-1 `salt_enc`; the password itself never leaves the
/// device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginFinishRequest {
    pub email: String,
    pub auth_key: String,
    #[serde(default)]
    pub client_id: Option<Uuid>,
}

/// Round-2 success response: the token pair plus the bootstrap material — `salt_enc`,
/// the password-wrapped vault key (unwrapped locally with `master_key`), and the
/// signed roster JSON (§7.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginFinishResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub tenant_id: Uuid,
    pub salt_enc: String,
    pub wrapped_vault_key_password: String,
    pub device_list: Option<Value>,
}

// --------------------------------------------------------------------- refresh

/// `POST /auth/refresh` body — rotation + reuse detection (architecture §5/§10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_finish_client_id_defaults_when_absent() {
        // A first (signup) device may omit client_id; it must deserialize to None,
        // preserving the server's existing #[serde(default)] behavior.
        let req: LoginFinishRequest =
            serde_json::from_str(r#"{"email":"a@b.c","auth_key":"AAAA"}"#).unwrap();
        assert_eq!(req.client_id, None);
    }

    #[test]
    fn token_response_roundtrips_both_directions() {
        // The shared DTO must serialize (server) AND deserialize (client) the same shape.
        let t = TokenResponse {
            access_token: "a".into(),
            refresh_token: "r".into(),
            tenant_id: Uuid::nil(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: TokenResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.access_token, "a");
        assert_eq!(back.tenant_id, Uuid::nil());
    }
}
