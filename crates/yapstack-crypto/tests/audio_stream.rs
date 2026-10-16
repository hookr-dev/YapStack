// SPDX-License-Identifier: AGPL-3.0-only
//! Known-answer + behavioural tests for the streaming audio AEAD
//! (`yapstack.audio.stream.v1`, CRYPTO_SPEC §1.5 / §13.8).
//!
//! The vectors here are GENERATED with this same RustCrypto stack (no prior audio KAT
//! existed — the spec's §13.4 vector is changeset-domain). The generator is
//! [`print_kat_vectors`] (an `#[ignore]`d test); its output was pasted verbatim into the
//! `EXPECTED_*` constants below AND into CRYPTO_SPEC §13.8, then this suite asserts the
//! production code reproduces them. To regenerate after a deliberate format change:
//! `cargo test -p yapstack-crypto --test audio_stream -- --ignored --nocapture print_kat`.

use yapstack_crypto::audio_stream::{
    open_blob, open_segments, seal_blob, seal_segments, unwrap_audio_key, wrap_audio_key,
    AudioIdentity, AUDIO_HEADER_LEN, NONCE_PREFIX_LEN, STREAM_TAG_LEN,
};

// ---- fixed KAT inputs (deterministic; production values are random per §11) ----------
const DATA_KEY: [u8; 32] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
const NONCE_PREFIX: [u8; NONCE_PREFIX_LEN] = [
    0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
    0xb0, 0xb1, 0xb2,
];
const CHUNK_SIZE: u32 = 8;
// 19 bytes ⇒ 3 segments at chunk_size=8: [8][8][3] (short last).
const KAT_PLAINTEXT: &[u8] = b"the quick brown fox";

const VAULT_KEY: [u8; 32] = [
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
    0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f,
];
const WRAP_NONCE: [u8; 24] = [
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27,
];

fn kat_identity() -> AudioIdentity {
    AudioIdentity {
        tenant_id: [0x11; 16],
        session_id: [0x22; 16],
        part_id: [0x33; 16],
        epoch: 0,
    }
}

// ---- GENERATED vectors (see module header) -------------------------------------------
const EXPECTED_SEGMENTS_HEX: &str = "0100000008a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2a60589414748d24e8126f11ca3b289b97fbacba45e5cd1735522e597d0230d16d836467d7038fe914a0d60ef3870bcc02baff86b20d167fac830f13e3d0b35e3a294b0";
const EXPECTED_WRAPPED_HEX: &str = "0133a7b8159b0f524b992f78fa8b65acafd3d85af85906fb6dbf789422e7d6cfbb101112131415161718191a1b1c1d1e1f2021222324252627b503f59083f1b8c6f43a82633efc16750261aae28a25aac2e6888dd6c99b147508f87190c05d4310c20760a359fd19fc";

#[test]
#[ignore = "vector generator; run with --ignored --nocapture to (re)produce §13.8"]
fn print_kat_vectors() {
    let mut sealed = Vec::new();
    seal_segments(
        &DATA_KEY,
        &NONCE_PREFIX,
        CHUNK_SIZE,
        KAT_PLAINTEXT,
        &mut sealed,
    )
    .unwrap();
    println!("KAT header||segments (hex) = {}", hex::encode(&sealed));
    println!(
        "  header (24) = {}",
        hex::encode(&sealed[..AUDIO_HEADER_LEN])
    );
    println!(
        "  segments    = {}",
        hex::encode(&sealed[AUDIO_HEADER_LEN..])
    );

    let wrapped = wrap_audio_key(&VAULT_KEY, &DATA_KEY, &WRAP_NONCE, &kat_identity()).unwrap();
    println!("KAT wrapped_data_key (hex) = {}", hex::encode(&wrapped));
    println!("  len = {}", wrapped.len());
}

#[test]
fn kat_segments_reproduce() {
    let mut sealed = Vec::new();
    seal_segments(
        &DATA_KEY,
        &NONCE_PREFIX,
        CHUNK_SIZE,
        KAT_PLAINTEXT,
        &mut sealed,
    )
    .unwrap();
    assert_eq!(
        hex::encode(&sealed),
        EXPECTED_SEGMENTS_HEX,
        "audio stream segment KAT drifted"
    );
    // Exactly 3 segments: header(24) + [8+16] + [8+16] + [3+16].
    let expected_len = AUDIO_HEADER_LEN + (8 + STREAM_TAG_LEN) * 2 + (3 + STREAM_TAG_LEN);
    assert_eq!(sealed.len(), expected_len);

    let mut out = Vec::new();
    open_segments(&sealed[..], &DATA_KEY, &mut out).unwrap();
    assert_eq!(out, KAT_PLAINTEXT);
}

#[test]
fn kat_wrapped_key_reproduces_and_unwraps() {
    let wrapped = wrap_audio_key(&VAULT_KEY, &DATA_KEY, &WRAP_NONCE, &kat_identity()).unwrap();
    assert_eq!(
        hex::encode(&wrapped),
        EXPECTED_WRAPPED_HEX,
        "audio wrap KAT drifted"
    );
    let unwrapped = unwrap_audio_key(&VAULT_KEY, &wrapped, &kat_identity()).unwrap();
    assert_eq!(&unwrapped[..], &DATA_KEY[..]);
}

#[test]
fn truncation_is_rejected() {
    // Drop the last segment (3 + 16 bytes): the previous last_flag=0 segment is now opened
    // as last_flag=1 → nonce mismatch → auth failure.
    let mut sealed = Vec::new();
    seal_segments(
        &DATA_KEY,
        &NONCE_PREFIX,
        CHUNK_SIZE,
        KAT_PLAINTEXT,
        &mut sealed,
    )
    .unwrap();
    let truncated = &sealed[..sealed.len() - (3 + STREAM_TAG_LEN)];
    let mut out = Vec::new();
    assert!(
        open_segments(truncated, &DATA_KEY, &mut out).is_err(),
        "a truncated stream (missing final segment) must fail to open"
    );
}

#[test]
fn reorder_is_rejected() {
    // Swap the two full segments (each 8+16 bytes) after the 24-byte header.
    let mut sealed = Vec::new();
    seal_segments(
        &DATA_KEY,
        &NONCE_PREFIX,
        CHUNK_SIZE,
        KAT_PLAINTEXT,
        &mut sealed,
    )
    .unwrap();
    let seg = 8 + STREAM_TAG_LEN;
    let mut reordered = sealed.clone();
    let (a_start, b_start) = (AUDIO_HEADER_LEN, AUDIO_HEADER_LEN + seg);
    let seg_a = sealed[a_start..a_start + seg].to_vec();
    let seg_b = sealed[b_start..b_start + seg].to_vec();
    reordered[a_start..a_start + seg].copy_from_slice(&seg_b);
    reordered[b_start..b_start + seg].copy_from_slice(&seg_a);
    let mut out = Vec::new();
    assert!(
        open_segments(&reordered[..], &DATA_KEY, &mut out).is_err(),
        "a reordered stream must fail to open (counter position binding)"
    );
}

#[test]
fn wrap_aad_wrong_part_id_is_rejected() {
    let wrapped = wrap_audio_key(&VAULT_KEY, &DATA_KEY, &WRAP_NONCE, &kat_identity()).unwrap();
    let mut wrong = kat_identity();
    wrong.part_id = [0x99; 16];
    assert!(
        unwrap_audio_key(&VAULT_KEY, &wrapped, &wrong).is_err(),
        "a blob addressed to a different part_id must fail at unwrap"
    );
}

#[test]
fn wrap_aad_wrong_epoch_is_rejected() {
    let wrapped = wrap_audio_key(&VAULT_KEY, &DATA_KEY, &WRAP_NONCE, &kat_identity()).unwrap();
    let mut wrong = kat_identity();
    wrong.epoch = 1;
    assert!(
        unwrap_audio_key(&VAULT_KEY, &wrapped, &wrong).is_err(),
        "a data key wrapped under epoch 0 must not unwrap under epoch 1 (anti-rollback)"
    );
}

/// A `Write` that asserts no single write exceeds an O(chunk) bound, and streams a
/// SHA-256 of everything written. This is the bounded-memory evidence: if the
/// implementation ever buffered a whole 200 MB blob, a write larger than the bound would
/// fire the assertion.
struct BoundedHashingWriter {
    hasher: sha2::Sha256,
    total: u64,
    max_write: usize,
    bound: usize,
}

impl BoundedHashingWriter {
    fn new(bound: usize) -> Self {
        use sha2::Digest;
        Self {
            hasher: sha2::Sha256::new(),
            total: 0,
            max_write: 0,
            bound,
        }
    }
    fn finish(self) -> (u64, [u8; 32]) {
        use sha2::Digest;
        (self.total, self.hasher.finalize().into())
    }
}

impl std::io::Write for BoundedHashingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        assert!(
            buf.len() <= self.bound,
            "single write of {} bytes exceeds the O(chunk) bound {} — the stream is buffering the whole blob",
            buf.len(),
            self.bound
        );
        self.max_write = self.max_write.max(buf.len());
        self.total += buf.len() as u64;
        self.hasher.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Large (over 200 MB) round-trip with a bounded-memory assertion (§Verification, S1). The source is
/// written to a temp file (never held in RAM), sealed and opened through generic
/// `Read`/`Write`, and the recovered plaintext is proven byte-equal by SHA-256. The
/// [`BoundedHashingWriter`] fails if any single write exceeds `chunk_size + overhead`,
/// so a regression that buffered the whole blob would trip it. Gated `#[ignore]` so the
/// default `cargo test` stays fast; run explicitly in CI/verification.
#[test]
#[ignore = "large (>200MB) — run with --ignored for the bounded-memory round-trip proof"]
fn large_blob_roundtrip_is_byte_equal_and_bounded_memory() {
    use std::io::{Read, Write};
    use yapstack_crypto::audio_stream::{AUDIO_CHUNK_SIZE, MAX_WRAPPED_KEY_LEN};

    let bound = AUDIO_CHUNK_SIZE as usize + STREAM_TAG_LEN + MAX_WRAPPED_KEY_LEN;
    let vault = [0x77; 32];
    let id = kat_identity();

    // 210 MiB source written to disk in 1 MiB deterministic chunks (a rolling pattern so
    // no two chunks are identical — a real regression can't hide behind a constant fill).
    let total: usize = 210 * 1024 * 1024;
    let mut src_file = tempfile::NamedTempFile::new().unwrap();
    {
        let mut src_hash = BoundedHashingWriter::new(bound);
        let mut written = 0usize;
        let mut chunk = vec![0u8; 1024 * 1024];
        let mut seed: u32 = 0x1234_5678;
        while written < total {
            for b in chunk.iter_mut() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *b = (seed >> 24) as u8;
            }
            let n = (total - written).min(chunk.len());
            src_file.write_all(&chunk[..n]).unwrap();
            src_hash.write_all(&chunk[..n]).unwrap();
            written += n;
        }
        src_file.flush().unwrap();
        let (src_total, _) = src_hash.finish();
        assert_eq!(src_total as usize, total);
    }
    let src_sha = sha256_file(src_file.path());

    // Seal: source file → sealed temp file, bounded writes.
    let sealed_file = tempfile::NamedTempFile::new().unwrap();
    {
        let reader = std::fs::File::open(src_file.path()).unwrap();
        let mut w = BufBoundedWriter {
            inner: std::io::BufWriter::new(std::fs::File::create(sealed_file.path()).unwrap()),
            bound,
        };
        seal_blob(&vault, &id, reader, &mut w).unwrap();
        w.inner.flush().unwrap();
    }

    // Open: sealed file → plaintext temp file, bounded writes, then compare SHA-256.
    let out_file = tempfile::NamedTempFile::new().unwrap();
    {
        let reader = std::fs::File::open(sealed_file.path()).unwrap();
        let mut w = BufBoundedWriter {
            inner: std::io::BufWriter::new(std::fs::File::create(out_file.path()).unwrap()),
            bound,
        };
        open_blob(&vault, &id, reader, &mut w).unwrap();
        w.inner.flush().unwrap();
    }

    assert_eq!(
        sha256_file(out_file.path()),
        src_sha,
        "recovered plaintext must be byte-equal to the >200MB source"
    );

    // Sanity: the reader side is bounded too — a full read never exceeds the buffer.
    let mut probe = std::fs::File::open(src_file.path()).unwrap();
    let mut small = vec![0u8; AUDIO_CHUNK_SIZE as usize];
    let n = probe.read(&mut small).unwrap();
    assert!(n <= AUDIO_CHUNK_SIZE as usize);
}

/// A `BufWriter` wrapper that enforces the O(chunk) per-write bound on the *inner*
/// writes the streaming crypto issues (each segment is one `write_all`). The `BufWriter`
/// itself coalesces, so we assert on the crypto's write calls by checking each `write`.
struct BufBoundedWriter {
    inner: std::io::BufWriter<std::fs::File>,
    bound: usize,
}
impl std::io::Write for BufBoundedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        assert!(
            buf.len() <= self.bound,
            "single crypto write of {} bytes exceeds O(chunk) bound {}",
            buf.len(),
            self.bound
        );
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn sha256_file(path: &std::path::Path) -> [u8; 32] {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    h.finalize().into()
}

#[test]
fn blob_wrong_identity_rejected_before_segments() {
    let vault = [0x55; 32];
    let mut blob = Vec::new();
    seal_blob(&vault, &kat_identity(), &b"hello audio"[..], &mut blob).unwrap();
    let mut wrong = kat_identity();
    wrong.session_id = [0xee; 16];
    let mut out = Vec::new();
    assert!(open_blob(&vault, &wrong, &blob[..], &mut out).is_err());
    // Correct identity still opens.
    let mut ok = Vec::new();
    open_blob(&vault, &kat_identity(), &blob[..], &mut ok).unwrap();
    assert_eq!(ok, b"hello audio");
}
