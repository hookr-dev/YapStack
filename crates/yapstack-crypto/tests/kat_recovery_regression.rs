// SPDX-License-Identifier: AGPL-3.0-only
//! RED regression gate for the §13.7 recovery-split KAT tautology finding.
//!
//! The shipping `kat_recovery_key_derivation` (tests/kat.rs:150-170) asserts
//! `hex::encode(kdf::recovery_key(x)) == { let expected = hex::encode(&rk); expected }`
//! i.e. `f(x) == f(x)` — an unfalsifiable self-comparison that pins NOTHING. It also
//! uses input `00112233..` instead of §13.7's `000102..`, and never exercises
//! `recovery_auth_key` (okm[32..64], the value POSTed to /auth/recover) at all.
//!
//! This test replaces the tautology with the LITERAL pinned vectors from
//! docs/CRYPTO_SPEC.md §13.7 (LOCKED). It is GREEN on the correct tree (the current
//! implementation reproduces the spec bytes) and becomes RED the instant
//! `kdf::recovery_key` drifts — a changed `RECOVERY_INFO`, an Extract-then-Expand
//! posture switch (kdf.rs A6 note), or an `hkdf::expand` edit — which is exactly the
//! drift the existing tautology cannot catch and which would silently orphan every
//! already-shipped `wrapped_vault_key_recovery` blob (permanent vault lockout).
//!
//! Proof that this is a real gate where the shipped one is not: mutate `RECOVERY_INFO`
//! in kdf.rs (e.g. `v1` -> `v2`); `kat_recovery_key_derivation` STILL passes, this test
//! FAILS. See the investigator's mutation-experiment evidence.

use yapstack_crypto::{hkdf, kdf};

/// §13.7 pinned input: `recovery_bytes (20) = 000102030405060708090a0b0c0d0e0f10111213`.
const SPEC_RECOVERY_BYTES: [u8; 20] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13,
];

/// §13.7 pinned outputs (HKDF-SHA256 Expand-only, info = "yapstack.recovery.v1", L=64).
const SPEC_RECOVERY_KEY_HEX: &str =
    "39684df2364fab3f87645c80ad59c9eec298ea212c84f3fbb7c49f303f6557c5";
const SPEC_RECOVERY_AUTH_KEY_HEX: &str =
    "1e5ad1dbc106f5791bb28d514097f462fece2837b3b5a02b6708364d6655db8b";

/// `recovery_key` (block 1 = okm[0..32]) MUST equal the §13.7 literal vector.
#[test]
fn kat_13_7_recovery_key_literal_vector() {
    let rk = kdf::recovery_key(&SPEC_RECOVERY_BYTES);
    assert_eq!(
        hex::encode(&rk),
        SPEC_RECOVERY_KEY_HEX,
        "recovery_key drifted from the LOCKED §13.7 vector — every shipped \
         wrapped_vault_key_recovery blob would fail to open (permanent vault lockout)"
    );
}

/// `recovery_auth_key` (block 2 = okm[32..64], POSTed to /auth/recover) MUST equal the
/// §13.7 literal vector. The shipped tautology KAT never touched this value at all.
#[test]
fn kat_13_7_recovery_auth_key_literal_vector() {
    // §13.7 derives both blocks from a single L=64 Expand-only over the same PRK+info
    // that `kdf::recovery_key` uses (Expand-only, RECOVERY_INFO). Block 2 is okm[32..64].
    let okm = hkdf::expand(&SPEC_RECOVERY_BYTES, kdf::RECOVERY_INFO, 64);
    assert_eq!(&okm[..32], &hex::decode(SPEC_RECOVERY_KEY_HEX).unwrap()[..]);
    assert_eq!(
        hex::encode(&okm[32..64]),
        SPEC_RECOVERY_AUTH_KEY_HEX,
        "recovery_auth_key drifted from the LOCKED §13.7 vector — /auth/recover would \
         second-hash the wrong bytes and reject the recovery-code login"
    );
}
