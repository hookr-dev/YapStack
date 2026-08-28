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
    /// base64 of the recovery auth key: a SECOND, independent second-hash input derived
    /// client-side from the 160-bit recovery code (§6). The server second-hashes it into
    /// a `recovery_verifier` (exactly like the password `verifier`, §3.1) and discards
    /// it — the recovery code itself never leaves the device, and no password-equivalent
    /// is ever stored. This is what `POST /auth/recover` authenticates against before it
    /// serves `wrapped_vault_key_recovery`.
    pub recovery_auth_key: String,
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
    /// A NEW (unknown) device bootstrapping via password login (§7.5 step 1) presents
    /// its fresh Ed25519 public key so the relay can enroll it as a PENDING device row
    /// (authenticated, but NOT yet a signed-roster sync peer — no auto-promotion). Absent
    /// for an already-known device; when absent the login never mutates the roster.
    #[serde(default)]
    pub ed25519_pub: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
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
    /// base64 Ed25519 signature over the served roster (§7.5 step 2). A bootstrapping
    /// device verifies `device_list` against this using the vault-derived roster key it
    /// unwraps locally; the relay never authors or verifies it (it holds no vault key).
    pub signature: Option<String>,
}

// --------------------------------------------------------------------- refresh

/// `POST /auth/refresh` body — rotation + reuse detection (architecture §5/§10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

// --------------------------------------------------------------------- recover

/// `POST /auth/recover` body (§6.2). Authenticate with the recovery code so the relay
/// will serve `wrapped_vault_key_recovery`. `recovery_auth_key` is derived client-side
/// from the recovery code and second-hashed server-side against the stored
/// `recovery_verifier` (constant-time), EXACTLY like the password verifier (§3.1) — no
/// account-existence or recovery oracle. The raw recovery code never leaves the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverRequest {
    pub email: String,
    pub recovery_auth_key: String,
}

/// `POST /auth/recover` success (§6.2). Carries the RECOVERY-wrapped vault key — the
/// blob the relay stores at signup but NEVER serves on the login path — so the client
/// can unwrap the vault key with the recovery code and then re-wrap under a new
/// password. `device_list`/`signature` let the recovering device verify the roster
/// (§7.5). Tokens log the recovering device in (client_id bound at issuance).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub tenant_id: Uuid,
    pub salt_enc: String,
    pub wrapped_vault_key_recovery: String,
    pub device_list: Option<Value>,
    pub signature: Option<String>,
}

// --------------------------------------------------------------------- devices

/// One device as seen by the relay's advisory device index (§7.5). `status` is a UI
/// hint the relay maintains from client-supplied metadata; the CRYPTOGRAPHIC source of
/// truth for membership is always the signed roster (`device_list`), which clients
/// verify — the relay never reads it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub client_id: Uuid,
    /// base64 of the device's 32-byte Ed25519 public key.
    pub ed25519_pub: String,
    pub label: String,
    /// `"pending"` (enrolled, awaiting approval) or `"active"` (in an accepted roster).
    pub status: String,
    pub added_at: String,
}

/// `GET /devices` — the account's device index (pending + active) for the approving
/// device to review (§7.5 step 3). RLS-scoped to the caller's workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicesResponse {
    pub devices: Vec<DeviceInfo>,
}

/// `PUT /devices/roster` — upload a re-signed device roster (§7.5 step 3, §7.4). The
/// `device_list`/`signature` are stored VERBATIM and opaquely (the relay holds no vault
/// key and never reads them). `counter` is the plaintext anti-rollback watermark (§7.4):
/// the relay accepts the upload ONLY if `counter` STRICTLY EXCEEDS the stored counter,
/// enforced under a row lock — no roster content is read to do this. `active_devices`
/// is plaintext metadata naming the client_ids the new roster lists as active, used to
/// advance the advisory `devices.status` (pending→active); it is NEVER derived from the
/// opaque roster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterUploadRequest {
    pub device_list: Value,
    pub signature: String,
    pub counter: i64,
    pub vault_key_epoch: i64,
    #[serde(default)]
    pub active_devices: Vec<Uuid>,
}

/// `PUT /devices/roster` success: the accepted anti-rollback watermarks, echoed back so
/// the client can confirm the relay advanced to exactly its `counter`/`vault_key_epoch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterUploadResponse {
    pub counter: i64,
    pub vault_key_epoch: i64,
}

/// `GET /devices/roster` — the stored signed roster (§7.5), served verbatim so ANY
/// authenticated device (including a still-pending one bootstrapping) can re-anchor its
/// anti-rollback `counter` before signing a new roster. `device_list`/`signature` are the
/// opaque blobs the relay stored (it holds no vault key and never authored them); the
/// caller verifies them with the vault-derived roster key. `counter`/`vault_key_epoch`
/// are the authoritative anti-rollback watermarks (§7.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterFetchResponse {
    pub device_list: Value,
    pub signature: String,
    pub counter: i64,
    pub vault_key_epoch: i64,
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
