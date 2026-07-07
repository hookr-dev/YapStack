// SPDX-License-Identifier: AGPL-3.0-only
//! R2: snapshot-bootstrap — ship a compact DB SNAPSHOT instead of replaying the
//! ~434k per-cell `crsql_changes` a fresh full-sync would need (T003 measured the
//! owner's 41 MB DB at ~434k initial changes/device).
//!
//! ## Why a snapshot, not a changeset replay
//! A brand-new join device could, in principle, `pull` every changeset the relay
//! holds and feed each into `crsql_changes`. For the owner's real library that is
//! hundreds of thousands of round-tripped cells — slow, and it re-encrypts/re-uploads
//! the same state the seed already holds. Instead the SEED device produces one compact
//! artifact: a `VACUUM INTO` copy of its already-CRR-migrated database (rows +
//! cr-sqlite clock/site tables intact), which the join device opens directly as its
//! CRR base. The seed encrypts it ([`crate::crypto::SnapshotCipher`]) and uploads it as
//! an opaque blob; the relay stores bytes it cannot read (§9-style blob store).
//!
//! ## What the join does with it
//! The join writes the decrypted bytes to a fresh file, opens it, then **re-sites** it
//! (see [`crate::reconcile::resite_as_fresh_peer`]) so the snapshot's rows become a
//! peer's history and the join gets its own fresh `site_id` — WITHOUT re-pushing the
//! seed's rows. It then pulls only the incremental changesets committed *after* the
//! snapshot baseline and reconciles its own local-only rows.
//!
//! The artifact is byte-for-byte a SQLite file, so it is content-addressable by its
//! ciphertext SHA-256 exactly like an audio blob — the relay presigns it to object
//! storage and never sees plaintext.

use std::path::Path;

use rusqlite::Connection;

use crate::SyncError;

/// Metadata describing one snapshot generation. Travels alongside the opaque blob so a
/// join device knows the baseline cursor to resume incremental pull from. None of these
/// fields are secret (the blob itself is E2E-encrypted); they are routing metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotMeta {
    /// Monotonic snapshot generation (bound into the snapshot AEAD AAD).
    pub generation: u64,
    /// The relay `changeset_seq` the seed had synced through when it produced the
    /// snapshot. The join sets its pull watermark here so it resumes incremental pull
    /// *after* the snapshot instead of re-pulling everything the snapshot already holds.
    pub baseline_seq: i64,
}

/// Produce a compact snapshot of a CRR database as raw SQLite file bytes.
///
/// Uses `VACUUM INTO`, which only READS the source (never opens it for write) and
/// writes a fresh, compacted copy carrying every table — including the cr-sqlite
/// `*__crsql_clock`, `crsql_site_id`, and bookkeeping tables the join needs to open it
/// as a CRR base. The temp file is written next to `dest_hint`'s directory so it shares
/// the app-data volume, then read back and removed.
///
/// `src_db_path` MUST be the device's PREPARED sync copy (already `crr_migrate`d), never
/// the live app DB.
pub fn produce_snapshot_bytes(
    src_db_path: &Path,
    scratch_dir: &Path,
) -> Result<Vec<u8>, SyncError> {
    let tmp = scratch_dir.join(format!("yapstack-snapshot-{}.tmp", std::process::id()));
    // Best-effort clean of any stale scratch file from a crashed run.
    let _ = std::fs::remove_file(&tmp);
    {
        let src = Connection::open(src_db_path)?;
        let target = tmp
            .to_str()
            .ok_or_else(|| SyncError::Migration("non-UTF8 snapshot scratch path".into()))?;
        src.execute("VACUUM INTO ?1", [target])?;
    }
    let bytes = std::fs::read(&tmp)
        .map_err(|e| SyncError::Migration(format!("read snapshot scratch: {e}")))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(bytes)
}

/// Write decrypted snapshot bytes to `dest_path`, replacing any existing file. The
/// caller then opens `dest_path` as a [`crate::CrsqlDb`] and re-sites it.
pub fn write_snapshot_file(dest_path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if dest_path.exists() {
        std::fs::remove_file(dest_path)
            .map_err(|e| SyncError::Migration(format!("remove stale snapshot dest: {e}")))?;
    }
    std::fs::write(dest_path, bytes)
        .map_err(|e| SyncError::Migration(format!("write snapshot dest: {e}")))?;
    Ok(())
}

/// The lowercase-hex SHA-256 of a (ciphertext) snapshot blob, used to content-address
/// it in the blob store exactly like an audio blob (§9). Computed over the ENCRYPTED
/// bytes — the relay never sees plaintext.
#[must_use]
pub fn ciphertext_sha256_hex(ciphertext: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(ciphertext))
}
