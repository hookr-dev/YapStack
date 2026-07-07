// SPDX-License-Identifier: AGPL-3.0-only
//! Changeset encryption per CRYPTO_SPEC §4 (envelope hierarchy) and §5 (AAD).
//!
//! Every changeset gets a FRESH random 32-byte data key (§4.1), wrapped under the
//! account `vault_key` with the COMMITTING envelope (§4.2,
//! `AAD = LP(version, "yapstack.wrap.data.v1", epoch_u32)`), and the serialized
//! changeset is sealed under that data key with the STANDARD envelope and the
//! changeset AAD (§5.2,
//! `LP(version, "yapstack.changeset.v1", tenant_id, client_id, client_seq,
//! schema_version, engine_version)`). The outbox blob is `wrapped_data_key ||
//! sealed_changeset`. The relay never sees plaintext or a bare data key.
//!
//! All crypto is delegated to `yapstack-crypto`; this module only builds AADs and
//! composes the two envelopes.

use rand::RngCore;
use uuid::Uuid;
use yapstack_crypto::aead::{lp, open_committing, open_standard, seal_committing, seal_standard};
use yapstack_crypto::{CryptoError, DOMAIN_CHANGESET, VERSION};

use crate::SyncError;

/// Domain string for the per-changeset data-key wrap (§4.2).
const WRAP_DATA_DOMAIN: &[u8] = b"yapstack.wrap.data.v1";

/// Fixed length of a committing envelope wrapping a 32-byte key:
/// `version(1) + commitment(32) + nonce(24) + ct(32) + tag(16)`.
const WRAPPED_KEY_LEN: usize = 1 + 32 + 24 + 32 + 16;

/// Immutable per-account/device crypto context for changesets.
#[derive(Clone)]
pub struct ChangesetCipher {
    vault_key: [u8; 32],
    epoch: u32,
    tenant_id: Uuid,
    schema_version: u32,
    engine_version: u32,
}

impl ChangesetCipher {
    pub fn new(
        vault_key: [u8; 32],
        epoch: u32,
        tenant_id: Uuid,
        schema_version: u32,
        engine_version: u32,
    ) -> Self {
        Self {
            vault_key,
            epoch,
            tenant_id,
            schema_version,
            engine_version,
        }
    }

    fn wrap_aad(&self) -> Vec<u8> {
        lp(&[&[VERSION], WRAP_DATA_DOMAIN, &self.epoch.to_be_bytes()])
    }

    fn changeset_aad(&self, client_id: Uuid, client_seq: i64) -> Vec<u8> {
        lp(&[
            &[VERSION],
            DOMAIN_CHANGESET,
            self.tenant_id.as_bytes(),
            client_id.as_bytes(),
            &(client_seq as u64).to_be_bytes(),
            &self.schema_version.to_be_bytes(),
            &self.engine_version.to_be_bytes(),
        ])
    }

    /// Encrypt one changeset for `(client_id, client_seq)`. Returns the opaque
    /// outbox blob `wrapped_data_key || sealed_changeset`.
    pub fn encrypt(
        &self,
        client_id: Uuid,
        client_seq: i64,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, SyncError> {
        let mut data_key = [0u8; 32];
        let mut wrap_nonce = [0u8; 24];
        let mut ct_nonce = [0u8; 24];
        let mut rng = rand::thread_rng();
        rng.fill_bytes(&mut data_key);
        rng.fill_bytes(&mut wrap_nonce);
        rng.fill_bytes(&mut ct_nonce);

        let wrapped = seal_committing(&self.vault_key, &wrap_nonce, &data_key, &self.wrap_aad())?;
        debug_assert_eq!(wrapped.len(), WRAPPED_KEY_LEN);
        let sealed = seal_standard(
            &data_key,
            &ct_nonce,
            plaintext,
            &self.changeset_aad(client_id, client_seq),
        )?;

        let mut out = Vec::with_capacity(wrapped.len() + sealed.len());
        out.extend_from_slice(&wrapped);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Decrypt an outbox blob authored by `(client_id, client_seq)`.
    ///
    /// A tampered or version-mismatched blob returns an error (a §11.3 quarantine
    /// path — never a panic on attacker bytes).
    pub fn decrypt(
        &self,
        client_id: Uuid,
        client_seq: i64,
        blob: &[u8],
    ) -> Result<Vec<u8>, SyncError> {
        if blob.len() < WRAPPED_KEY_LEN {
            return Err(SyncError::Crypto(CryptoError::Malformed));
        }
        let (wrapped, sealed) = blob.split_at(WRAPPED_KEY_LEN);
        let data_key_vec = open_committing(&self.vault_key, wrapped, &self.wrap_aad())?;
        let data_key: [u8; 32] = data_key_vec
            .as_slice()
            .try_into()
            .map_err(|_| SyncError::Crypto(CryptoError::Malformed))?;
        let pt = open_standard(
            &data_key,
            sealed,
            &self.changeset_aad(client_id, client_seq),
        )?;
        Ok(pt)
    }
}
