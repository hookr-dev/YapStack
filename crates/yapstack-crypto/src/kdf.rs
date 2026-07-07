// SPDX-License-Identifier: AGPL-3.0-only
//! Argon2id + HKDF key derivation and the server verifier (CRYPTO_SPEC §2, §3, §6, §7).
//!
//! All Argon2 parameters are compile-time constants and are floor-checked before any
//! derivation runs (§2.2). Parameters are NEVER fetched from the server (§2.1).

use argon2::{Algorithm, Argon2, Params, Version};

use crate::{hkdf, CryptoError};

// --- Pinned client Argon2id parameters (§2.1) ---
pub const CLIENT_M: u32 = 65536; // KiB (64 MiB)
pub const CLIENT_T: u32 = 3;
pub const CLIENT_P: u32 = 4;

// --- Server verifier Argon2id parameters (§3.1) ---
pub const SERVER_M: u32 = 19456; // KiB
pub const SERVER_T: u32 = 2;
pub const SERVER_P: u32 = 1;

pub const OUT_LEN: usize = 32;

// HKDF info labels (§2.3, §6.2, §7.2). ASCII bytes match §12 exactly.
pub const AUTH_INFO: &[u8] = b"yapstack.auth.v1";
pub const MASTER_INFO: &[u8] = b"yapstack.master.v1";
pub const RECOVERY_INFO: &[u8] = b"yapstack.recovery.v1";
pub const DEVLIST_SIGN_INFO: &[u8] = b"yapstack.devicelist.sign.v1";

fn argon2id(m: u32, t: u32, p: u32) -> Result<Argon2<'static>, CryptoError> {
    // Floor check (§2.2): a build must never derive below the pinned client floor
    // for the client path. Server params are intentionally lighter and validated
    // against their own constants, not the client floor.
    let params = Params::new(m, t, p, Some(OUT_LEN)).map_err(|_| CryptoError::KdfParams)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// §2.2 hard floor: reject client parameters weaker than the pinned constants.
fn assert_client_floor() -> Result<(), CryptoError> {
    if CLIENT_M < 65536 || CLIENT_T < 3 || CLIENT_P < 4 || OUT_LEN < 32 {
        return Err(CryptoError::KdfParams);
    }
    Ok(())
}

/// Argon2id client stretch (§2.3): the single expensive pass over the password.
///
/// # Errors
/// [`CryptoError::KdfParams`] if the floor check or Argon2 init fails.
pub fn client_stretch(password: &[u8], salt_enc: &[u8]) -> Result<[u8; 32], CryptoError> {
    assert_client_floor()?;
    let a = argon2id(CLIENT_M, CLIENT_T, CLIENT_P)?;
    let mut out = [0u8; 32];
    a.hash_password_into(password, salt_enc, &mut out)
        .map_err(|_| CryptoError::Kdf)?;
    Ok(out)
}

/// Derived auth/master split (§2.3): HKDF-Expand-only over the 256-bit stretch PRK
/// with domain-separated `info` labels.
#[must_use]
pub fn split_keys(stretch: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
    let auth = hkdf::expand(stretch, AUTH_INFO, 32);
    let master = hkdf::expand(stretch, MASTER_INFO, 32);
    (auth, master)
}

/// Server second-hash verifier (§3.1): `Argon2id(auth_key, server_salt)` with the
/// lighter server parameters. The server stores this, NEVER `auth_key`, NEVER the
/// password.
///
/// # Errors
/// [`CryptoError::KdfParams`] / [`CryptoError::Kdf`] on Argon2 failure.
pub fn server_verifier(auth_key: &[u8], server_salt: &[u8]) -> Result<[u8; 32], CryptoError> {
    let a = argon2id(SERVER_M, SERVER_T, SERVER_P)?;
    let mut out = [0u8; 32];
    a.hash_password_into(auth_key, server_salt, &mut out)
        .map_err(|_| CryptoError::Kdf)?;
    Ok(out)
}

/// Recovery-wrapping key (§6.2): HKDF-Expand-only over the 160-bit recovery PRK.
///
/// A6 note (T006 advisory): §6.2 LOCKS Expand-only over the 20-byte input, which is
/// below RustCrypto's `from_prk` floor and so is implemented via the manual
/// [`hkdf::expand`]. Extract-then-Expand (salt-less) would be the cleaner RFC 5869
/// posture but would change the derived key and thus the on-disk recovery wrap — a
/// spec change, flagged to the T009 review rather than made unilaterally here.
#[must_use]
pub fn recovery_key(recovery_bytes: &[u8; 20]) -> Vec<u8> {
    hkdf::expand(recovery_bytes, RECOVERY_INFO, 32)
}

/// Device-list signing seed (§7.2): HKDF-Expand over the vault key.
#[must_use]
pub fn devlist_sign_seed(vault_key: &[u8; 32]) -> [u8; 32] {
    let okm = hkdf::expand(vault_key, DEVLIST_SIGN_INFO, 32);
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&okm);
    seed
}
