// SPDX-License-Identifier: AGPL-3.0-only
//! HKDF-SHA256 (RFC 5869), implemented directly over HMAC so the same audited
//! code path serves every derivation in the spec — including the 160-bit recovery
//! PRK (§6.2), which RustCrypto's `Hkdf::from_prk` would reject for being shorter
//! than `HashLen`. The RFC math places no such floor on `Expand`; the guard is a
//! library policy, not a spec requirement. Keeping one implementation means the
//! KAT vectors (§13) exercise exactly the bytes production uses.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// RFC 5869 §2.2 — Extract. `salt` may be empty (treated as `HashLen` zeros by HMAC).
#[must_use]
pub fn extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(salt).expect("HMAC accepts any key length");
    mac.update(ikm);
    mac.finalize().into_bytes().into()
}

/// RFC 5869 §2.3 — Expand. `prk` need not be >= `HashLen`; the spec's recovery
/// derivation (§6.2) deliberately expands a 160-bit PRK.
///
/// # Panics
/// Panics if `okm_len` exceeds `255 * HashLen` (8160 bytes) — an unreachable
/// programming error for the fixed-length derivations in this crate.
#[must_use]
pub fn expand(prk: &[u8], info: &[u8], okm_len: usize) -> Vec<u8> {
    assert!(
        okm_len <= 255 * 32,
        "HKDF-Expand okm_len exceeds 255*HashLen"
    );
    let mut okm = Vec::with_capacity(okm_len);
    let mut t: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while okm.len() < okm_len {
        let mut mac = HmacSha256::new_from_slice(prk).expect("HMAC accepts any key length");
        mac.update(&t);
        mac.update(info);
        mac.update(&[counter]);
        t = mac.finalize().into_bytes().to_vec();
        okm.extend_from_slice(&t);
        counter = counter.checked_add(1).expect("HKDF block counter overflow");
    }
    okm.truncate(okm_len);
    okm
}

/// Full RFC 5869 Extract-then-Expand (used by the committing envelope, §1.4,
/// where `salt = nonce`).
#[must_use]
pub fn extract_expand(salt: &[u8], ikm: &[u8], info: &[u8], okm_len: usize) -> Vec<u8> {
    let prk = extract(salt, ikm);
    expand(&prk, info, okm_len)
}
