// SPDX-License-Identifier: AGPL-3.0-only
//! Sync-protocol wire DTOs shared by the desktop client and the blind relay
//! (architecture §7a/§7b, §9, §14).
//!
//! These types are the JSON contract for changeset push/pull, the completeness
//! (anti-entropy) endpoint, and audio presign. They carry **only ciphertext and
//! opaque metadata** — the relay never decrypts. `ciphertext` fields are base64 of
//! the opaque committing/standard envelope bytes (`bytea` at rest, an opaque string
//! on the wire); `schema_version`/`engine_version` are the server's UNTRUSTED plaintext
//! hints (CRYPTO_SPEC §5.4) used only for pre-decrypt routing, never a trust decision.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Max changes accepted in one `POST /sync/push` (architecture §10 guardrail).
pub const MAX_PUSH_CHANGES: usize = 1000;
/// Max total ciphertext bytes accepted in one `POST /sync/push` (architecture §10).
pub const MAX_PUSH_BYTES: usize = 5 * 1024 * 1024;

// ------------------------------------------------------------------ push

/// One outbox entry a client uploads. `(client_id, client_seq)` is the idempotency
/// key: re-pushing the same pair returns the ORIGINAL `changeset_seq` and never
/// re-inserts. `client_seq` is the client's own per-device monotonic counter (bound
/// into the AEAD AAD per CRYPTO_SPEC §5.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushChange {
    pub client_id: Uuid,
    pub client_seq: i64,
    /// base64 of the opaque envelope bytes. Stored as `bytea`; never decrypted.
    pub ciphertext: String,
    /// Untrusted plaintext hint (§5.4); the authority is the AAD-bound version.
    pub schema_version: i32,
    pub engine_version: i32,
}

/// `POST /sync/push` body: a drained outbox batch.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PushRequest {
    pub changes: Vec<PushChange>,
}

/// Server acknowledgement of one pushed change: the commit-ordered `changeset_seq`
/// the server assigned (or the ORIGINAL seq on an idempotent re-push).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushAck {
    pub client_id: Uuid,
    pub client_seq: i64,
    pub changeset_seq: i64,
    /// True when this pair was already present and the seq is the original (no
    /// re-insert happened) — lets the client prove idempotency end to end.
    pub deduplicated: bool,
}

/// `POST /sync/push` response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PushResponse {
    pub acks: Vec<PushAck>,
    /// The tenant's max `changeset_seq` after this push (a client can wake peers).
    pub max_changeset_seq: i64,
}

// ------------------------------------------------------------------ pull

/// One changeset returned by pull, in dense commit order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PulledChange {
    pub changeset_seq: i64,
    pub client_id: Uuid,
    pub client_seq: i64,
    /// base64 of the opaque envelope bytes.
    pub ciphertext: String,
    pub schema_version: i32,
    pub engine_version: i32,
}

/// `GET /sync/pull?since=&limit=` response. `next_seq` is the cursor to pass as the
/// next `since`; `has_more` says another page exists. The cursor is a DENSE
/// commit-ordered sequence so gap detection is arithmetic (§7a/§7b).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PullResponse {
    pub changes: Vec<PulledChange>,
    pub next_seq: i64,
    pub has_more: bool,
}

// -------------------------------------------------------- completeness (§7b, A3)

/// The max `client_seq` the server holds for one `client_id`. Lets a client detect a
/// dropped TAIL of its OWN device stream (advisory A3): a malicious relay that
/// silently drops a device's newest changesets is caught because the client knows the
/// `client_seq` it last pushed and can compare.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientTail {
    pub client_id: Uuid,
    pub max_client_seq: i64,
}

/// `GET /sync/completeness` (architecture §7b): verifiable without reading content.
/// `contiguous` is arithmetic over the dense cursor (`max == count`, seqs start at 1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompletenessResponse {
    pub max_changeset_seq: i64,
    pub count: i64,
    pub contiguous: bool,
    /// Per-device tail watermarks (A3). A client asserts its own `client_seq` high
    /// water mark is present here; a shortfall = the relay dropped its tail.
    pub per_client: Vec<ClientTail>,
}

// ------------------------------------------------------------------ audio (§9)

/// `POST /audio/presign` response. Either the blob already exists within the tenant
/// (dedup on the CIPHERTEXT hash → no upload, no metering) or a presigned PUT is
/// returned. The client uploads directly to object storage; bytes never flow through
/// the relay, which never sees plaintext and never re-hashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresignResponse {
    /// Within-tenant dedup hit: the identical ciphertext blob is already stored.
    pub already_exists: bool,
    /// Presigned `PUT` URL. `None` when `already_exists`. The content-length is PINNED
    /// in the signed policy, so the client-declared `size` is enforced by object
    /// storage, not trusted by the relay. NEVER logged (it is a bearer capability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_url: Option<String>,
    /// The object key `tenants/{workspace_id}/blobs/{ciphertext_sha256}`.
    pub object_key: String,
    /// Exact byte length the presigned PUT is pinned to (echo of the declared `size`).
    pub content_length: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_change_roundtrips_json() {
        let c = PushChange {
            client_id: Uuid::nil(),
            client_seq: 42,
            ciphertext: "AQID".to_string(),
            schema_version: 22,
            engine_version: 16003,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: PushChange = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn batch_guardrails_are_the_locked_numbers() {
        assert_eq!(MAX_PUSH_CHANGES, 1000);
        assert_eq!(MAX_PUSH_BYTES, 5 * 1024 * 1024);
    }
}
