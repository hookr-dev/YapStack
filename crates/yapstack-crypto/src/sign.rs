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
