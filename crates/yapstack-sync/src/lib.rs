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

pub mod cascade;
pub mod change;
pub mod crsqlite;
pub mod crypto;
pub mod outbox;
pub mod quarantine;
pub mod schema;
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
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}
