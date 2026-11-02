// SPDX-License-Identifier: AGPL-3.0-only
//! # yapstack-sync
//!
//! On-device sync runtime for the blind-relay E2E sync (SYNC_SPEC §1.1, §6). It statically links
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
//! ceremony, starting the drain lanes) lives in `apps/desktop/src-tauri/src/sync.rs`;
//! the system as built is specified in `docs/SYNC_SPEC.md`.

pub mod audio;
pub mod cascade;
pub mod change;
pub mod crsqlite;
pub mod crypto;
pub mod outbox;
pub mod quarantine;
pub mod schema;
pub mod snapshot;
pub mod state;
pub mod transport;
pub mod uniqueness;

pub use crsqlite::CrsqlDb;

use yapstack_crypto::CryptoError;

/// cr-sqlite `engine_version` bound into changeset AAD (CRYPTO_SPEC §5.1:
/// `major*1_000_000 + minor*1_000 + patch`; `0.16.3` → `16003`).
pub const CRSQLITE_ENGINE_VERSION: u32 = 16_003;

/// The current CRR schema version bound into changeset AAD (§5.4). Bump when the
/// synced schema changes; drives the §6 quarantine/handshake gate.
pub const SYNC_SCHEMA_VERSION: u32 = 1;

/// This build's sync capability level, sent on every pull and snapshot request
/// (`X-YapStack-Sync-Capability`, ADR 0006 §5.1). Level 1 = "has the table-readiness
/// quarantine" (ADR 0006 §4): a row for a table that isn't a CRR locally never fails a
/// changeset, so this device can safely receive rows for tables added after it shipped.
pub const SYNC_CAPABILITY_LEVEL: i32 = 1;

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
    /// The relay could not be reached at the transport layer — a connection failure or a
    /// timeout (reqwest `is_connect()` / `is_timeout()`), NOT an HTTP-status error. Kept
    /// distinct from [`SyncError::Http`] so the drain surfaces the amber "can't reach relay"
    /// state (parity with the T025 probe classification) instead of the destructive "sync
    /// error". Carries the reqwest display, which includes the relay URL only — never any
    /// token material (the bearer travels as an HTTP header, not in the error).
    #[error("relay unreachable: {0}")]
    Network(String),
    /// The relay answered 409 with its generic `conflict` code. On push that is the G3
    /// signal: the relay holds this `(client_id, client_seq)` under different ciphertext.
    /// Only this positively identified body may trigger the reseed that drops unacked
    /// entries; any other 409 stays an ordinary error.
    #[error("relay conflict: {0}")]
    RelayConflict(String),
    /// The relay refused a pull or snapshot read because this build's sync capability is
    /// below the workspace floor (HTTP 426, ADR 0006 §5.7). Only an app update clears it.
    #[error("Update YapStack to keep syncing (this device's sync capability {level} is below the workspace's {required})")]
    UpdateRequired {
        level: i32,
        required: i32,
        requested: i32,
    },
    /// The relay refused a push batch whose capability level is above the floor it holds
    /// right now (HTTP 409 `sync_capability_floor`, ADR 0006 §5.4). Distinct from the
    /// `client_seq`-regression 409, which drops the unacked outbox: this one must not.
    #[error("relay sync capability floor {required} is below this batch's level {batch}")]
    CapabilityFloor {
        batch: i32,
        required: i32,
        requested: i32,
    },
    /// A floor raise was refused because other recently seen devices report a lower level
    /// (HTTP 409 `sync_capability_blocked`). Push holds in `client_seq` order behind the
    /// gated entry until they update or are removed; pull carries on.
    #[error("waiting for {} to update YapStack", blocker_names(.blockers))]
    CapabilityBlocked {
        level: i32,
        blockers: Vec<yapstack_common::sync::CapabilityBlocker>,
    },
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

fn blocker_names(blockers: &[yapstack_common::sync::CapabilityBlocker]) -> String {
    let names: Vec<&str> = blockers
        .iter()
        .map(|b| {
            if b.label.is_empty() {
                "another device"
            } else {
                b.label.as_str()
            }
        })
        .collect();
    if names.is_empty() {
        "another device".into()
    } else {
        names.join(", ")
    }
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
