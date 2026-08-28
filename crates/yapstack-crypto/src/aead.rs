// SPDX-License-Identifier: AGPL-3.0-only
//! AEAD envelopes per CRYPTO_SPEC §1 and §5.
//!
//! - The one content cipher is XChaCha20-Poly1305 (192-bit random nonce).
//! - AAD is the canonical length-prefixed encoding `LP` (§5.1).
//! - The 1-byte version is the FIRST AAD field on every surface (C1, §5.2), so a
//!   flipped plaintext version byte fails authentication rather than downgrading.
//! - Two envelope shapes: standard (changesets/audio) and committing (wrapped keys
//!   and shares), the latter with an HKDF `UtC` key-commitment (§1.4).

use crate::hkdf;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use subtle::ConstantTimeEq;

use crate::{CryptoError, COMMIT_INFO, VERSION};

/// Canonical length-prefixed AAD encoding (§5.1): each field is
/// `u32be(len) || bytes`. Unambiguous regardless of field contents.
#[must_use]
pub fn lp(fields: &[&[u8]]) -> Vec<u8> {
    let total: usize = fields.iter().map(|f| 4 + f.len()).sum();
    let mut out = Vec::with_capacity(total);
    for f in fields {
        let len = u32::try_from(f.len()).expect("AAD field exceeds u32");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(f);
    }
    out
}

/// Seal into the standard envelope: `version(1) || nonce(24) || ct || tag(16)`.
/// `aad` MUST already lead with the version byte (use [`lp`] with `&[VERSION]`
/// first); this function does not prepend it, so callers keep full control of the
/// per-surface field list (§5.2).
///
/// # Errors
/// Returns [`CryptoError::Seal`] if the underlying AEAD fails (unreachable for
/// valid keys/nonces).
pub fn seal_standard(
    key: &[u8; 32],
    nonce: &[u8; 24],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Seal)?;
    let mut out = Vec::with_capacity(1 + 24 + ct.len());
    out.push(VERSION);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a standard envelope. The caller supplies the exact `aad` it expects; the
/// leading (authenticated) version byte in `aad` must match the blob's version.
///
/// # Errors
/// Returns [`CryptoError::Version`], [`CryptoError::Malformed`], or
/// [`CryptoError::Open`] (quarantine, never a panic — §11.3).
pub fn open_standard(key: &[u8; 32], blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < 1 + 24 + 16 {
        return Err(CryptoError::Malformed);
    }
    if blob[0] != VERSION {
        return Err(CryptoError::Version(blob[0]));
    }
    let nonce = &blob[1..25];
    let ct = &blob[25..];
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| CryptoError::Open)
}

/// Seal into the committing envelope:
/// `version(1) || commitment(32) || nonce(24) || ct || tag(16)` (§1.4).
/// Required for every wrapped key and every share.
///
/// # Errors
/// Returns [`CryptoError::Seal`] on AEAD failure.
pub fn seal_committing(
    k_root: &[u8; 32],
    nonce: &[u8; 24],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let okm = hkdf::extract_expand(nonce, k_root, COMMIT_INFO, 64);
    let commitment = &okm[0..32];
    let mut k_aead = [0u8; 32];
    k_aead.copy_from_slice(&okm[32..64]);

    let cipher = XChaCha20Poly1305::new((&k_aead).into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Seal)?;

    let mut out = Vec::with_capacity(1 + 32 + 24 + ct.len());
    out.push(VERSION);
    out.extend_from_slice(commitment);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a committing envelope: recompute `okm` from `(k_root, nonce)`, verify the
/// 32-byte commitment in constant time, then AEAD-open (§1.4).
///
/// # Errors
/// [`CryptoError::Version`], [`CryptoError::Malformed`], [`CryptoError::Commitment`],
/// or [`CryptoError::Open`] — all quarantine paths (§11.3), never a panic.
pub fn open_committing(k_root: &[u8; 32], blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < 1 + 32 + 24 + 16 {
        return Err(CryptoError::Malformed);
    }
    if blob[0] != VERSION {
        return Err(CryptoError::Version(blob[0]));
    }
    let commitment = &blob[1..33];
    let nonce = &blob[33..57];
    let ct = &blob[57..];

    let okm = hkdf::extract_expand(nonce, k_root, COMMIT_INFO, 64);
    // Constant-time commitment check before touching the AEAD (§1.4).
    if okm[0..32].ct_eq(commitment).unwrap_u8() != 1 {
        return Err(CryptoError::Commitment);
    }
    let mut k_aead = [0u8; 32];
    k_aead.copy_from_slice(&okm[32..64]);
    let cipher = XChaCha20Poly1305::new((&k_aead).into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| CryptoError::Open)
}
