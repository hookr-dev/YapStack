// SPDX-License-Identifier: AGPL-3.0-only
#![forbid(unsafe_code)]
//! # yapstack-crypto
//!
//! RustCrypto implementation of `docs/CRYPTO_SPEC.md` (World B). This is the Rust
//! half of the "one spec, two stacks" contract: byte-for-byte identical
//! constructions to the JS/`@noble` share-viewer stack, proven by the §13 KAT
//! vectors reproduced in `tests/kat.rs` (which FAIL CLOSED on any mismatch).
//!
//! JS/WASM parity is DEFERRED to Phase 4 (the share-viewer does not exist yet); the
//! Rust KAT gate stands alone until then. See the crate `tests/kat.rs` header.

pub mod aead;
pub mod audio_stream;
pub mod hkdf;
pub mod kdf;
pub mod sign;

/// Envelope version byte (§11.1). `0x01` for this spec. Also the FIRST AAD field
/// on every surface (C1, §5.2), so it is authenticated by the tag.
pub const VERSION: u8 = 0x01;

/// HKDF `info` for the committing-envelope key-commitment (§1.4).
pub const COMMIT_INFO: &[u8] = b"yapstack.commit.v1";

// Per-surface AAD domain strings (§5.2).
pub const DOMAIN_CHANGESET: &[u8] = b"yapstack.changeset.v1";
// RETIRED, never shipped (CRYPTO_SPEC §5.2 amendment): `yapstack.audio.v1`, the
// whole-blob audio domain. Zero blobs were ever sealed under it (the
// `audio_blobs`/`audio_objects` tables are empty in every deployment — the load-bearing
// invariant). Never seal under it again; audio uses the streaming construction below.
/// §1.5 format identifier — NOT an AAD field (the clear header is the per-segment AAD);
/// mirrors the §12 registry row.
pub const DOMAIN_AUDIO_STREAM: &[u8] = b"yapstack.audio.stream.v1";
/// Wrap domain for the per-blob audio data key (§4.2 audio amendment). Bound into the
/// committing wrap AAD together with the identity tuple `(tenant_id, session_id,
/// part_id, epoch_u32)`.
pub const DOMAIN_WRAP_AUDIO_STREAM: &[u8] = b"yapstack.wrap.audio.stream.v1";
pub const DOMAIN_SHARE: &[u8] = b"yapstack.share.v1";

/// Crypto failures. Every variant is a quarantine/deny path (§11.3) — callers must
/// never panic on attacker-supplied bytes.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("AEAD seal failed")]
    Seal,
    #[error("AEAD open failed (bad tag or tampered ciphertext)")]
    Open,
    #[error("committing envelope commitment mismatch")]
    Commitment,
    #[error("unknown or downgraded envelope version: {0:#04x}")]
    Version(u8),
    #[error("malformed envelope (too short)")]
    Malformed,
    #[error("i/o error during streaming crypto: {0}")]
    Io(String),
    #[error("KDF parameters below floor or invalid")]
    KdfParams,
    #[error("KDF derivation failed")]
    Kdf,
    #[error("Ed25519 signature verification failed")]
    Signature,
}
