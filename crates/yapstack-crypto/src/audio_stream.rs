// SPDX-License-Identifier: AGPL-3.0-only
//! Streaming AEAD for audio blobs — `yapstack.audio.stream.v1` (CRYPTO_SPEC §1.5).
//!
//! Audio blobs can be very large (the owner's largest single WAV is 599 MB), and the
//! whole-blob standard envelope (`aead::seal_standard`) is strictly one-shot — it takes
//! the entire plaintext as a single slice, so constant-memory whole-blob sealing is
//! impossible with it. This module wraps the **maintained** RustCrypto STREAM
//! construction (`chacha20poly1305::aead::stream::{EncryptorBE32, DecryptorBE32}` over
//! XChaCha20-Poly1305) to give true O(chunk) memory in **both** directions. STREAM is
//! **never hand-rolled**: the per-segment nonce (`nonce_prefix(19) || counter(u32be) ||
//! last_flag(1)`) is assembled by the crate, giving positional binding via the counter
//! and last-block truncation detection via the `last_flag`.
//!
//! ## Blob layout (§1.5 / §Data flow)
//! ```text
//! audio_blob = LP(wrapped_data_key)   # committing envelope §1.4/§4.2, wrap AAD binds identity
//!            || header                # version(1)=0x01 || chunk_size(u32be) || nonce_prefix(19)
//!            || seg_0 … seg_n         # STREAM segments; each = ct || tag(16); last may be short
//!
//! stream plaintext = original_audio_bytes   # the codec tag is carried by the caller, not here
//! ```
//! The clear `header` is passed as **AAD on every segment**, so tampering with
//! `chunk_size` or the version byte fails the first `open`. The version byte is the
//! first authenticated header field (C1 discipline).
//!
//! ## Identity binding (D6, §4.2 audio amendment)
//! The per-blob data key is wrapped under `vault_key` with the committing envelope; the
//! wrap AAD is `LP(version, "yapstack.wrap.audio.stream.v1", tenant_id, session_id,
//! part_id, epoch_u32)`. A blob replayed under another tenant/session/part/epoch fails at
//! **unwrap**, before any segment is touched. Identity is bound at the wrap layer only —
//! the segments themselves carry no identity, so they need never be re-encrypted on a
//! part-id change.
//!
//! ## Notes for the crypto reviewer
//! - The STREAM primitive is the crate's `StreamBE32` (5-byte overhead: `counter(u32be)
//!   || last_flag(1)`); XChaCha's 24-byte nonce minus 5 = a **19-byte** `nonce_prefix`,
//!   fresh-random per blob. We never touch the counter or flag ourselves.
//! - Last-segment discipline: seal calls `encrypt_last` on the final chunk (even for an
//!   empty source, which yields exactly one last segment); open calls `decrypt_last` on
//!   the final segment. A dropped final segment makes the previous (`last_flag=0`) segment
//!   be opened as last (`last_flag=1`) → nonce mismatch → auth failure (truncation
//!   rejected). A swapped/added segment shifts the counter → auth failure (reorder /
//!   extension rejected).
//! - Both directions read/write through generic `Read`/`Write`, buffering at most
//!   `chunk_size (+ tag)` plaintext/ciphertext at once — the working set is independent of
//!   blob size. The caller tees the sealed `Write` into a SHA-256 hasher to
//!   content-address the whole blob in the same pass (no second read).
//! - Untrusted-header hardening: `chunk_size` from a tampered header is bounded by
//!   [`MAX_CHUNK_SIZE`] before any allocation, and the wrapped-key length prefix by
//!   [`MAX_WRAPPED_KEY_LEN`]; both reject rather than allocate unboundedly.

use std::io::{Read, Write};

use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::aead::Payload;
use chacha20poly1305::XChaCha20Poly1305;
use rand_core::{OsRng, RngCore};
use zeroize::Zeroizing;

use crate::aead::{lp, open_committing, seal_committing};
use crate::{CryptoError, DOMAIN_WRAP_AUDIO_STREAM, VERSION};

/// Streaming-audio envelope version (§1.5). Distinct constant from the crate [`VERSION`]
/// so a future format bump moves only audio.
pub const AUDIO_STREAM_VERSION: u8 = 0x01;

/// v1 plaintext chunk size: 1 MiB per segment (recorded in the header for agility).
pub const AUDIO_CHUNK_SIZE: u32 = 1024 * 1024;

/// STREAM nonce prefix length = XChaCha nonce (24) − `StreamBE32` overhead (5).
pub const NONCE_PREFIX_LEN: usize = 19;

/// Poly1305 tag length appended to every segment ciphertext.
pub const STREAM_TAG_LEN: usize = 16;

/// Clear header length: `version(1) || chunk_size(u32be) || nonce_prefix(19)`.
pub const AUDIO_HEADER_LEN: usize = 1 + 4 + NONCE_PREFIX_LEN;

/// Upper bound on a header-declared `chunk_size` (64 MiB). A tampered header cannot force
/// an unbounded allocation; well above the v1 constant, leaving room for agility.
pub const MAX_CHUNK_SIZE: u32 = 64 * 1024 * 1024;

/// Upper bound on the `LP(wrapped_data_key)` length prefix. The committing wrap of a
/// 32-byte key is 73 bytes (§1.3); the cap rejects an absurd prefix before allocating.
pub const MAX_WRAPPED_KEY_LEN: usize = 4096;

/// The four identity fields bound into the audio data-key wrap AAD (D6, §4.2).
/// `tenant_id`, `session_id`, and `part_id` are the raw 16-byte UUID encodings (a
/// simple-format 32-hex part id decodes to these 16 bytes); `epoch` is the vault-key
/// rotation epoch, big-endian.
#[derive(Debug, Clone, Copy)]
pub struct AudioIdentity {
    pub tenant_id: [u8; 16],
    pub session_id: [u8; 16],
    pub part_id: [u8; 16],
    pub epoch: u32,
}

impl AudioIdentity {
    /// Wrap AAD: `LP(version, "yapstack.wrap.audio.stream.v1", tenant_id, session_id,
    /// part_id, epoch_u32)` (§4.2 audio amendment). `version` is the first field (C1).
    #[must_use]
    pub fn wrap_aad(&self) -> Vec<u8> {
        lp(&[
            &[VERSION],
            DOMAIN_WRAP_AUDIO_STREAM,
            &self.tenant_id,
            &self.session_id,
            &self.part_id,
            &self.epoch.to_be_bytes(),
        ])
    }
}

/// Build the clear header authenticated as AAD on every segment.
#[must_use]
pub fn audio_header(
    chunk_size: u32,
    nonce_prefix: &[u8; NONCE_PREFIX_LEN],
) -> [u8; AUDIO_HEADER_LEN] {
    let mut h = [0u8; AUDIO_HEADER_LEN];
    h[0] = AUDIO_STREAM_VERSION;
    h[1..5].copy_from_slice(&chunk_size.to_be_bytes());
    h[5..AUDIO_HEADER_LEN].copy_from_slice(nonce_prefix);
    h
}

/// Wrap a per-blob data key under `vault_key` (committing envelope §4.2), binding the
/// identity tuple as AAD. `wrap_nonce` is a fresh 24-byte random nonce.
///
/// # Errors
/// [`CryptoError::Seal`] on AEAD failure (unreachable for valid inputs).
pub fn wrap_audio_key(
    vault_key: &[u8; 32],
    data_key: &[u8; 32],
    wrap_nonce: &[u8; 24],
    id: &AudioIdentity,
) -> Result<Vec<u8>, CryptoError> {
    seal_committing(vault_key, wrap_nonce, data_key, &id.wrap_aad())
}

/// Unwrap a per-blob data key, verifying the committing envelope AND that the wrap AAD
/// matches *this* identity (a wrong `part_id` or `epoch` fails here, before any segment).
///
/// # Errors
/// [`CryptoError::Version`] / [`CryptoError::Commitment`] / [`CryptoError::Open`] /
/// [`CryptoError::Malformed`] — all quarantine paths.
pub fn unwrap_audio_key(
    vault_key: &[u8; 32],
    wrapped: &[u8],
    id: &AudioIdentity,
) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let pt = open_committing(vault_key, wrapped, &id.wrap_aad())?;
    if pt.len() != 32 {
        return Err(CryptoError::Malformed);
    }
    let mut k = Zeroizing::new([0u8; 32]);
    k.copy_from_slice(&pt);
    Ok(k)
}

/// Read up to `buf.len()` bytes, looping over short reads until the buffer is full or EOF.
/// Returns the number of bytes actually read (`< buf.len()` iff EOF was reached).
fn read_full<R: Read>(src: &mut R, buf: &mut [u8]) -> Result<usize, CryptoError> {
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CryptoError::Io(e.to_string())),
        }
    }
    Ok(filled)
}

fn read_exact_or<R: Read>(src: &mut R, buf: &mut [u8]) -> Result<(), CryptoError> {
    let n = read_full(src, buf)?;
    if n != buf.len() {
        return Err(CryptoError::Malformed);
    }
    Ok(())
}

fn write_all<W: Write>(dst: &mut W, bytes: &[u8]) -> Result<(), CryptoError> {
    dst.write_all(bytes)
        .map_err(|e| CryptoError::Io(e.to_string()))
}

/// Seal `src` into `header || seg_0 … seg_n` on `dst`, O(chunk) memory. The final chunk
/// (even an empty one) is sealed with `encrypt_last`. Callers that need the identity wrap
/// prefix use [`seal_blob`]; this is the core segment layer the KATs pin.
///
/// # Errors
/// [`CryptoError::Seal`] on AEAD failure, [`CryptoError::Io`] on read/write failure.
pub fn seal_segments<R: Read, W: Write>(
    data_key: &[u8; 32],
    nonce_prefix: &[u8; NONCE_PREFIX_LEN],
    chunk_size: u32,
    mut src: R,
    dst: &mut W,
) -> Result<(), CryptoError> {
    assert!(chunk_size > 0, "chunk_size must be non-zero");
    let header = audio_header(chunk_size, nonce_prefix);
    write_all(dst, &header)?;

    let key = GenericArray::from_slice(&data_key[..]);
    let nonce = GenericArray::from_slice(&nonce_prefix[..]);
    let mut enc = EncryptorBE32::<XChaCha20Poly1305>::new(key, nonce);

    let cs = chunk_size as usize;
    let mut buf = vec![0u8; cs];
    // One-chunk lookahead so we know which chunk is the LAST (→ `encrypt_last`).
    let mut pending: Option<Vec<u8>> = None;
    loop {
        let n = read_full(&mut src, &mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(prev) = pending.take() {
            let seg = enc
                .encrypt_next(Payload {
                    msg: &prev,
                    aad: &header,
                })
                .map_err(|_| CryptoError::Seal)?;
            write_all(dst, &seg)?;
        }
        pending = Some(buf[..n].to_vec());
        if n < cs {
            break; // short read ⇒ EOF; this chunk is the last
        }
    }
    let last = pending.unwrap_or_default();
    let seg = enc
        .encrypt_last(Payload {
            msg: &last,
            aad: &header,
        })
        .map_err(|_| CryptoError::Seal)?;
    write_all(dst, &seg)?;
    Ok(())
}

/// Open `header || seg_0 … seg_n` from `src` onto `dst`, O(chunk) memory. Rejects a
/// tampered/downgraded version byte, an out-of-bounds `chunk_size`, truncation (missing
/// final segment ⇒ the previous `last_flag=0` segment opened as last fails), reordering,
/// and any AAD mismatch (all → `CryptoError::Open`/`Version`/`Malformed`).
///
/// # Errors
/// See above — every failure is a quarantine/deny path, never a panic.
pub fn open_segments<R: Read, W: Write>(
    mut src: R,
    data_key: &[u8; 32],
    dst: &mut W,
) -> Result<(), CryptoError> {
    let mut header = [0u8; AUDIO_HEADER_LEN];
    read_exact_or(&mut src, &mut header)?;
    if header[0] != AUDIO_STREAM_VERSION {
        return Err(CryptoError::Version(header[0]));
    }
    let chunk_size = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    if chunk_size == 0 || chunk_size > MAX_CHUNK_SIZE {
        return Err(CryptoError::Malformed);
    }
    let mut prefix = [0u8; NONCE_PREFIX_LEN];
    prefix.copy_from_slice(&header[5..AUDIO_HEADER_LEN]);

    let key = GenericArray::from_slice(&data_key[..]);
    let nonce = GenericArray::from_slice(&prefix[..]);
    let mut dec = DecryptorBE32::<XChaCha20Poly1305>::new(key, nonce);

    let seg_len = chunk_size as usize + STREAM_TAG_LEN;
    let mut buf = vec![0u8; seg_len];
    let mut pending: Option<Vec<u8>> = None;
    loop {
        let n = read_full(&mut src, &mut buf)?;
        if n == 0 {
            break;
        }
        if n < STREAM_TAG_LEN {
            return Err(CryptoError::Malformed); // a segment is at least its tag
        }
        if let Some(prev) = pending.take() {
            let pt = dec
                .decrypt_next(Payload {
                    msg: &prev,
                    aad: &header,
                })
                .map_err(|_| CryptoError::Open)?;
            write_all(dst, &pt)?;
        }
        pending = Some(buf[..n].to_vec());
        if n < seg_len {
            break; // short read ⇒ EOF; this segment is the last
        }
    }
    // No segment at all after a valid header is a truncated blob.
    let last = pending.ok_or(CryptoError::Malformed)?;
    let pt = dec
        .decrypt_last(Payload {
            msg: &last,
            aad: &header,
        })
        .map_err(|_| CryptoError::Open)?;
    write_all(dst, &pt)?;
    Ok(())
}

/// Seal a whole audio blob: fresh random data key + nonce prefix, identity-bound wrap,
/// then streamed segments. Writes `LP(wrapped_data_key) || header || segments` to `dst`,
/// O(chunk) memory. The 1 MiB v1 [`AUDIO_CHUNK_SIZE`] is used.
///
/// The caller tees `dst` into a SHA-256 hasher to content-address the whole blob in one
/// pass. `data_key` and the nonce prefix are drawn from `OsRng` (§9.2) and zeroized.
///
/// # Errors
/// [`CryptoError::Seal`] / [`CryptoError::Io`].
pub fn seal_blob<R: Read, W: Write>(
    vault_key: &[u8; 32],
    id: &AudioIdentity,
    src: R,
    dst: &mut W,
) -> Result<(), CryptoError> {
    let mut data_key = Zeroizing::new([0u8; 32]);
    let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
    let mut wrap_nonce = [0u8; 24];
    OsRng.fill_bytes(&mut data_key[..]);
    OsRng.fill_bytes(&mut nonce_prefix);
    OsRng.fill_bytes(&mut wrap_nonce);

    let wrapped = wrap_audio_key(vault_key, &data_key, &wrap_nonce, id)?;
    let wlen = u32::try_from(wrapped.len()).map_err(|_| CryptoError::Seal)?;
    write_all(dst, &wlen.to_be_bytes())?;
    write_all(dst, &wrapped)?;

    seal_segments(&data_key, &nonce_prefix, AUDIO_CHUNK_SIZE, src, dst)
}

/// Open a whole audio blob: read `LP(wrapped_data_key)`, unwrap the data key verifying the
/// identity AAD (a foreign `part_id`/`epoch` fails here), then stream-decrypt the segments
/// onto `dst`, O(chunk) memory.
///
/// # Errors
/// [`CryptoError::Version`] / [`CryptoError::Commitment`] / [`CryptoError::Open`] /
/// [`CryptoError::Malformed`] / [`CryptoError::Io`].
pub fn open_blob<R: Read, W: Write>(
    vault_key: &[u8; 32],
    id: &AudioIdentity,
    mut src: R,
    dst: &mut W,
) -> Result<(), CryptoError> {
    let mut lenb = [0u8; 4];
    read_exact_or(&mut src, &mut lenb)?;
    let wlen = u32::from_be_bytes(lenb) as usize;
    if wlen == 0 || wlen > MAX_WRAPPED_KEY_LEN {
        return Err(CryptoError::Malformed);
    }
    let mut wrapped = vec![0u8; wlen];
    read_exact_or(&mut src, &mut wrapped)?;
    let data_key = unwrap_audio_key(vault_key, &wrapped, id)?;
    open_segments(src, &data_key, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> AudioIdentity {
        AudioIdentity {
            tenant_id: [0x11; 16],
            session_id: [0x22; 16],
            part_id: [0x33; 16],
            epoch: 0,
        }
    }

    #[test]
    fn header_is_24_bytes_and_leads_with_version() {
        let h = audio_header(AUDIO_CHUNK_SIZE, &[0xab; NONCE_PREFIX_LEN]);
        assert_eq!(h.len(), 24);
        assert_eq!(h[0], AUDIO_STREAM_VERSION);
        assert_eq!(
            u32::from_be_bytes([h[1], h[2], h[3], h[4]]),
            AUDIO_CHUNK_SIZE
        );
    }

    #[test]
    fn segments_roundtrip_multi_and_empty() {
        let data_key = [7u8; 32];
        let prefix = [9u8; NONCE_PREFIX_LEN];
        for pt in [
            Vec::new(),
            vec![1u8; 5],
            vec![2u8; 8],  // exact one chunk (cs=8)
            vec![3u8; 19], // 3 segments at cs=8: 8,8,3
            vec![4u8; 16], // exact two chunks
        ] {
            let mut sealed = Vec::new();
            seal_segments(&data_key, &prefix, 8, &pt[..], &mut sealed).unwrap();
            let mut out = Vec::new();
            open_segments(&sealed[..], &data_key, &mut out).unwrap();
            assert_eq!(out, pt, "roundtrip for len {}", pt.len());
        }
    }

    #[test]
    fn blob_roundtrip_binds_identity() {
        let vault = [0x55; 32];
        let pt = vec![42u8; 3 * 1024 * 1024 + 17]; // 3 full 1 MiB segments + short last
        let mut blob = Vec::new();
        seal_blob(&vault, &id(), &pt[..], &mut blob).unwrap();
        let mut out = Vec::new();
        open_blob(&vault, &id(), &blob[..], &mut out).unwrap();
        assert_eq!(out, pt);
    }
}
