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
    verify_detached(public_key, canonical, signature)
}

/// Generic detached Ed25519 verification of `message` against a 32-byte public key.
///
/// Mechanism-named (the caller decides the policy): used both for the vault-bound
/// device roster (§7) and, by the relay, for the control-plane admin request signature
/// (ENTITLEMENTS_SEAM.md wire contract). The relay NEVER holds the corresponding
/// signing key.
///
/// # Errors
/// [`CryptoError::Signature`] on an invalid public key or a bad signature.
pub fn verify_detached(
    public_key: &[u8; 32],
    message: &[u8],
    signature: &[u8; 64],
) -> Result<(), CryptoError> {
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| CryptoError::Signature)?;
    let sig = Signature::from_bytes(signature);
    vk.verify(message, &sig).map_err(|_| CryptoError::Signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn verify_detached_roundtrip() {
        let sk = key();
        let pk = sk.verifying_key().to_bytes();
        let msg = b"1720353600\nnonce-abc\nPUT\n/admin/v1/tenants/x/limits\ndeadbeef";
        let sig = sk.sign(msg).to_bytes();
        assert!(verify_detached(&pk, msg, &sig).is_ok());
    }

    #[test]
    fn verify_detached_rejects_tamper_and_wrong_key() {
        let sk = key();
        let pk = sk.verifying_key().to_bytes();
        let msg = b"authentic control-plane request";
        let sig = sk.sign(msg).to_bytes();

        // Tampered message (e.g. an attacker retargeted the tenant in the path).
        assert!(verify_detached(&pk, b"tampered request", &sig).is_err());

        // Wrong signer.
        let other = SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(verify_detached(&other, msg, &sig).is_err());
    }
}
