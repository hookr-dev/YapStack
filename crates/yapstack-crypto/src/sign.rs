// SPDX-License-Identifier: AGPL-3.0-only
//! Ed25519 device-roster signing (CRYPTO_SPEC §7). The roster signing key is
//! derived from the vault key (§7.2), so any vault-key holder can author the roster
//! and rotating the vault key rotates roster authority.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::{kdf, CryptoError};

/// Derive the vault-bound Ed25519 signing key (§7.2).
#[must_use]
pub fn roster_signing_key(vault_key: &[u8; 32]) -> SigningKey {
    let seed = kdf::devlist_sign_seed(vault_key);
    SigningKey::from_bytes(&seed)
}

/// Sign canonical roster bytes (§7.3). Returns the 64-byte detached signature.
#[must_use]
pub fn sign_roster(vault_key: &[u8; 32], canonical: &[u8]) -> [u8; 64] {
    let sk = roster_signing_key(vault_key);
    sk.sign(canonical).to_bytes()
}

/// Verify a roster signature against a 32-byte public key.
///
/// # Errors
/// [`CryptoError::Signature`] on an invalid public key or a bad signature.
pub fn verify_roster(
    public_key: &[u8; 32],
    canonical: &[u8],
    signature: &[u8; 64],
) -> Result<(), CryptoError> {
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| CryptoError::Signature)?;
    let sig = Signature::from_bytes(signature);
    vk.verify(canonical, &sig)
        .map_err(|_| CryptoError::Signature)
}

/// AAD-style domain for the login proof of possession (CRYPTO_SPEC §3.2 amendment A10).
pub const LOGIN_POP_DOMAIN: &[u8] = b"yapstack.login.pop.v1";

/// The canonical message a device signs to prove it holds its Ed25519 private key at
/// login / recovery (A10): `LP(domain, nonce, client_id(16), ed25519_pub(32), email_lower)`.
/// Binding the nonce defeats replay, the `client_id` + public key defeat identity
/// substitution, and the lower-cased email pins the account the nonce was issued for.
#[must_use]
pub fn login_pop_message(
    nonce: &[u8],
    client_id: &[u8; 16],
    ed25519_pub: &[u8; 32],
    email_lower: &str,
) -> Vec<u8> {
    crate::aead::lp(&[
        LOGIN_POP_DOMAIN,
        nonce,
        client_id,
        ed25519_pub,
        email_lower.as_bytes(),
    ])
}

/// Sign the login proof with the device's 32-byte Ed25519 seed (§7.1).
#[must_use]
pub fn sign_login_pop(device_seed: &[u8; 32], message: &[u8]) -> [u8; 64] {
    SigningKey::from_bytes(device_seed).sign(message).to_bytes()
}

/// Verify a login proof against a 32-byte public key using strict verification (rejects
/// non-canonical / small-order points, so a captured proof cannot be malleated).
///
/// # Errors
/// [`CryptoError::Signature`] on an invalid public key or a bad signature.
pub fn verify_login_pop(
    public_key: &[u8; 32],
    message: &[u8],
    signature: &[u8; 64],
) -> Result<(), CryptoError> {
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| CryptoError::Signature)?;
    let sig = Signature::from_bytes(signature);
    vk.verify_strict(message, &sig)
        .map_err(|_| CryptoError::Signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn verify_roster_roundtrip() {
        let sk = key();
        let pk = sk.verifying_key().to_bytes();
        let msg = b"1720353600\nnonce-abc\nPUT\n/admin/v1/tenants/x/limits\ndeadbeef";
        let sig = sk.sign(msg).to_bytes();
        assert!(verify_roster(&pk, msg, &sig).is_ok());
    }

    #[test]
    fn verify_roster_rejects_tamper_and_wrong_key() {
        let sk = key();
        let pk = sk.verifying_key().to_bytes();
        let msg = b"authentic control-plane request";
        let sig = sk.sign(msg).to_bytes();

        // Tampered message (e.g. a relay altered one roster entry).
        assert!(verify_roster(&pk, b"tampered request", &sig).is_err());

        // Wrong signer.
        let other = SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(verify_roster(&other, msg, &sig).is_err());
    }

    #[test]
    fn login_pop_message_field_order_and_domain() {
        // LP(domain, nonce, client_id, pub, email_lower) — a change in order or domain
        // would silently break every client/relay pair, so pin the exact bytes.
        let nonce = [0xAAu8; 4];
        let cid = [0x11u8; 16];
        let pk = [0x22u8; 32];
        let m = login_pop_message(&nonce, &cid, &pk, "a@b.c");
        let mut want = Vec::new();
        for f in [LOGIN_POP_DOMAIN, &nonce[..], &cid[..], &pk[..], b"a@b.c"] {
            want.extend_from_slice(&(f.len() as u32).to_be_bytes());
            want.extend_from_slice(f);
        }
        assert_eq!(m, want);
        assert_eq!(
            hex::encode(LOGIN_POP_DOMAIN),
            "796170737461636b2e6c6f67696e2e706f702e7631"
        );
        let seed = [7u8; 32];
        let sig = sign_login_pop(&seed, &m);
        let pk_real = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        assert!(verify_login_pop(&pk_real, &m, &sig).is_ok());
        let mut tampered = m.clone();
        tampered[5] ^= 1;
        assert!(verify_login_pop(&pk_real, &tampered, &sig).is_err());
    }
}
