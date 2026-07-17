// SPDX-License-Identifier: AGPL-3.0-only
//! # yapstack-sync
//!
//! On-device sync runtime for World B (blind-relay E2E sync). It statically links
//! the pinned cr-sqlite v0.16.3 CRR engine (R6, `build.rs` + [`crsqlite`]), migrates
//! the local schema to CRR-compatible form (R3, [`schema`]), reinstates the stripped
//! FK cascade/SET-NULL semantics as CRDT tombstone GC (R4, [`cascade`]) and the
//! dropped UNIQUE constraints as deterministic merge-time conflict resolution (R5,
//! [`uniqueness`]), quarantines and replays changes across a schema-version gap (R7,
//! [`quarantine`]), and runs an encrypted [`outbox`] push/pull drain over a blind
//! [`transport`]. All content is encrypted client-side (CRYPTO_SPEC §4/§5,
//! [`crypto`]) via `yapstack-crypto`; the relay never sees plaintext.
//!
//! ## Scope
//! This crate is the runtime only. Desktop wiring (key-management UI, device
//! ceremony, starting the drain task) is T010b; two-populated-device bootstrap
//! reconciliation is T011.

pub mod audio;
pub mod cascade;
pub mod change;
pub mod crsqlite;
pub mod crypto;
pub mod outbox;
pub mod quarantine;
pub mod reconcile;
pub mod schema;
pub mod snapshot;
pub mod state;
pub mod transport;
pub mod uniqueness;

pub use crsqlite::{register_crsqlite, CrsqlDb};

use yapstack_crypto::CryptoError;

/// cr-sqlite `engine_version` bound into changeset AAD (CRYPTO_SPEC §5.1:
/// `major*1_000_000 + minor*1_000 + patch`; `0.16.3` → `16003`).
pub const CRSQLITE_ENGINE_VERSION: u32 = 16_003;

/// The current CRR schema version bound into changeset AAD (§5.4). Bump when the
/// synced schema changes; drives the §6 quarantine/handshake gate.
pub const SYNC_SCHEMA_VERSION: u32 = 1;

/// Every fallible operation's error. All crypto/codec variants are quarantine/deny
/// paths (never a panic on attacker-supplied bytes, CRYPTO_SPEC §11.3).
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    #[error("changeset codec error: {0}")]
    Codec(String),
    #[error("migration error: {0}")]
    Migration(String),
    #[error("unknown/unsupported sync table: {0}")]
    UnknownTable(String),
    #[error("cr-sqlite CRR engine unavailable (static-link/registration failed): {0}")]
    CrrUnavailable(String),
    #[error("transport error: {0}")]
    Transport(String),
    /// The relay rejected the access token (HTTP 401). Distinct from a generic
    /// [`SyncError::Http`] so the drain can refresh-and-retry ONCE instead of
    /// hot-looping on a dead token (Bug A). Carries no token material.
    #[error("relay rejected the access token (401 unauthorized)")]
    Unauthorized,
    /// A single unacked outbox entry is larger than the per-request wire budget and
    /// can never be pushed as-is (Bug B). Distinct from a transient network error so
    /// the drain surfaces it ONCE instead of retrying a guaranteed 413 every cycle.
    /// `size` is the base64 wire size (what the relay body limit measures).
    #[error("outbox entry client_seq={client_seq} is {size} base64 bytes on the wire, exceeding the per-request push budget")]
    Oversized { client_seq: i64, size: usize },
    /// A pulled changeset could not be decrypted or decoded (§11.3 crypto-quarantine). The
    /// drain STOPS the pull at this changeset rather than skipping-and-forgetting it: silently
    /// dropping a peer's write while advancing the watermark is the "up to date" lie. The pull
    /// watermark is left at the last fully-merged seq so the NEXT cycle retries from here, and
    /// no later changeset is merged out of commit order. Carries ONLY the commit `seq`, the
    /// authoring `client_id`, and a short non-sensitive `detail` naming the failed stage —
    /// never any ciphertext, plaintext, or key material.
    #[error("pulled changeset seq={seq} from client {author_client_id} could not be decrypted/decoded ({detail})")]
    CryptoSkip {
        seq: i64,
        author_client_id: uuid::Uuid,
        detail: String,
    },
    /// The relay could not be reached at the transport layer — a connection failure or a
    /// timeout (reqwest `is_connect()` / `is_timeout()`), NOT an HTTP-status error. Kept
    /// distinct from [`SyncError::Http`] so the drain surfaces the amber "can't reach relay"
    /// state (parity with the T025 probe classification) instead of the destructive "sync
    /// error". Carries the reqwest display, which includes the relay URL only — never any
    /// token material (the bearer travels as an HTTP header, not in the error).
    #[error("relay unreachable: {0}")]
    Network(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

impl SyncError {
    /// True for a transport-layer connectivity failure (an unreachable relay), as opposed
    /// to an HTTP-status error or a local/crypto fault. Drives the drain's mapping of a
    /// mid-session relay outage to the "unreachable" display state rather than "error".
    #[must_use]
    pub fn is_network(&self) -> bool {
        matches!(self, SyncError::Network(_))
    }
}
