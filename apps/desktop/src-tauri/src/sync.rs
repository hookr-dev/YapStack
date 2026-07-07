// SPDX-License-Identifier: AGPL-3.0-only
//! Desktop integration of the `yapstack-sync` runtime (Gate 5B, T010b).
//!
//! This whole module is behind the off-by-default `sync` cargo feature. That is
//! the deliberate **isolation** of two hazards the vendored cr-sqlite CRR engine
//! carries (see notes/T010-sync-runtime.md CAVEATS). (1) the build needs the
//! pinned `nightly-2023-10-05` for the vendored bundle; (2) the bundle ships
//! `panic = "abort"` / a shadow `eh_personality`, so any binary that links it
//! aborts (not unwinds) on a Rust panic. Keeping it feature-gated means the
//! normal desktop debug build never links the CRR engine and is unaffected by
//! either hazard; the panic=abort shadow is only ever present in the
//! (release-profile, already-panic=abort) sync build.
//!
//! Responsibilities here: (A) start the `yapstack-sync` drain on a dedicated
//! single-thread runtime (`drain_once` holds `&Connection` across awaits →
//! `!Send`) and schedule `cascade_gc` + `enforce_uniqueness` after merges;
//! (F) run `crr_migrate` once, against a `.backup` COPY of the live DB (never
//! the live DB), gated on `SYNC_SCHEMA_VERSION`; plus the OS-keychain vault key
//! at rest (CRYPTO_SPEC §10) and `sync_wrap_secret` (deliverable E backend:
//! envelope-wrap the AI apiKey under the vault key).
//!
//! NOT wired here (needs a live relay + two machines → T011/T012, owner UAT):
//! the end-to-end auth ceremony HTTP round-trips (signup/login/recover/approve).
//! Those command handlers exist so the frontend contract is complete and return
//! an explicit "not yet wired" error rather than a silent stub.
//!
//! The `#[tauri::command]` handlers below are intentionally NOT yet registered
//! in the (tauri-specta) invoke handler — that registration + Specta type
//! generation lands with the T011 relay ceremony. They are the documented
//! command seam the frontend `lib/sync.ts` targets, so `dead_code` is expected
//! and allowed at the module level until then. `start_drain_if_enabled` (the
//! deliverable-A boot wiring) IS live from the `setup` hook.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{Manager, State};
use uuid::Uuid;

use yapstack_common::auth::{
    DevicesResponse, LoginBeginRequest, LoginBeginResponse, LoginFinishRequest,
    LoginFinishResponse, RecoverRequest, RecoverResponse, RosterEnvelope, RosterUploadRequest,
    RosterUploadResponse, SignupRequest, TokenResponse,
};
use yapstack_sync::crypto::ChangesetCipher;
use yapstack_sync::{
    cascade, outbox, schema, state, transport::HttpTransport, uniqueness, CrsqlDb,
    CRSQLITE_ENGINE_VERSION, SYNC_SCHEMA_VERSION,
};

const KEYCHAIN_SERVICE: &str = "dev.yapstack.app.sync";
const KEYCHAIN_ACCOUNT: &str = "session-v1";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// AAD domain for a vault-wrapped device setting secret (AI apiKey). Distinct
/// from the changeset / audio / share domains so a wrapped setting can never be
/// confused with another surface (CRYPTO_SPEC §5).
const SETTING_DOMAIN: &[u8] = b"yapstack.setting.v1";
/// AAD domain for the password-wrapped vault key (CRYPTO_SPEC §4.2).
const WRAP_VAULT_PW_DOMAIN: &[u8] = b"yapstack.wrap.vault.pw.v1";
/// AAD domain for the recovery-wrapped vault key (CRYPTO_SPEC §4.2/§6.2).
const WRAP_VAULT_REC_DOMAIN: &[u8] = b"yapstack.wrap.vault.rec.v1";
/// How often the drain cycles when idle. SSE wakeups (T008) can shorten this
/// later; a fixed poll is correct and simplest for v1.
const DRAIN_INTERVAL: Duration = Duration::from_secs(5);

// ----- Persisted session (OS keychain — never localStorage / plaintext SQLite) -----

/// The signed-in session held in the OS keychain (macOS Keychain / Windows
/// Credential Manager via `keyring`). Holds the vault key and bearer token —
/// the secrets that must NEVER touch the webview or the plaintext DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    server_url: String,
    email: String,
    /// base64 of the 32-byte vault key (CRYPTO_SPEC §4.1).
    vault_key_b64: String,
    /// `vault_key_epoch` (§7.4 anti-rollback).
    epoch: u32,
    /// Workspace/tenant id bound into changeset AAD (§5.2).
    tenant_id: Uuid,
    /// Access bearer token (§3.2). Rotated by the auth flow.
    bearer: String,
    /// This device's fingerprint, base32 4×4 (§7.5.5).
    device_fingerprint: Option<String>,
    /// True once `crr_migrate` has run and the drain is live.
    sync_enabled: bool,
    /// This install's fresh `client_id` (§7.1 UUIDv4). Stable across re-logins on the
    /// same device; a brand-new install mints a new one and enrolls as PENDING (§7.5).
    #[serde(default)]
    client_id: Uuid,
    /// base64 of this device's 32-byte Ed25519 signing seed (§7.1). The keypair identity
    /// listed in the roster; kept in the keychain, never transmitted.
    #[serde(default)]
    device_sk_b64: String,
    /// First-seen `salt_enc` (base64) for the §3.2-C3 known-device salt-mismatch alert.
    #[serde(default)]
    salt_enc_b64: Option<String>,
    /// Highest signed-roster `counter` verified for this account (§7.4 client anti-rollback).
    #[serde(default)]
    roster_counter: i64,
    /// Fingerprint of the last verified signed roster (§7.5.5) for the out-of-band epoch check.
    #[serde(default)]
    roster_fingerprint: Option<String>,
}

impl Session {
    fn vault_key(&self) -> Result<[u8; 32], String> {
        let bytes = B64
            .decode(self.vault_key_b64.as_bytes())
            .map_err(|_| "corrupt vault key in keychain".to_string())?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "vault key wrong length".to_string())?;
        Ok(arr)
    }
}

fn keychain_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).map_err(|e| e.to_string())
}

fn load_session() -> Result<Option<Session>, String> {
    match keychain_entry()?.get_password() {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| e.to_string()),
        // No entry yet = signed out.
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn store_session(s: &Session) -> Result<(), String> {
    let json = serde_json::to_string(s).map_err(|e| e.to_string())?;
    keychain_entry()?
        .set_password(&json)
        .map_err(|e| e.to_string())
}

fn clear_session() -> Result<(), String> {
    match keychain_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

// ----- Wire DTOs (camelCase to match lib/sync.ts) -----

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SyncInfoDto {
    server_url: String,
    version: String,
    billing_url: Option<String>,
}

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRosterEntryDto {
    fingerprint: String,
    is_self: bool,
    pending: bool,
    label: Option<String>,
}

/// Args for `sync_signup` (mirrors `SignupRequest` in `lib/sync.ts`).
#[derive(Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SignupArgs {
    server_url: String,
    email: String,
    password: String,
}

/// One-time `sync_signup` result (mirrors `SignupResult` in `lib/sync.ts`).
#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SignupResultDto {
    recovery_code: String,
    device_fingerprint: String,
}

/// `sync_login_begin` result (mirrors `LoginBeginResult` in `lib/sync.ts`).
#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct LoginBeginResultDto {
    salt_mismatch: bool,
}

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusDto {
    phase: String,
    server_url: String,
    email: Option<String>,
    device_fingerprint: Option<String>,
    roster: Vec<DeviceRosterEntryDto>,
    vault_key_epoch: Option<u32>,
    roster_fingerprint: Option<String>,
    sync_enabled: bool,
    last_error: Option<String>,
    billing_url: Option<String>,
}

/// `/sync/info` server response (mirror of `yapstack_server` `SyncInfoResponse`).
#[derive(Deserialize)]
struct SyncInfoResponse {
    engine_version: String,
    billing_url: Option<String>,
}

// ----- crr_migrate on a COPY (deliverable F) -----

/// Path where the CRR-enabled sync copy of the library lives. Kept beside the
/// live DB so it shares the app data dir's backup/permissions posture.
fn sync_db_path(live_db: &Path) -> PathBuf {
    live_db.with_file_name("yapstack.sync.db")
}

/// Prepare the library for sync ONCE: make a hot COPY of the live DB (via
/// `VACUUM INTO`, which only *reads* the source — the live DB is never opened
/// for write), then register CRR + `crr_migrate` + reinstate the app-layer
/// cascade/uniqueness on the COPY. Gated on `SYNC_SCHEMA_VERSION`: if the copy
/// is already prepared at this schema version, it is a no-op. The live DB is
/// left completely untouched (safety pattern per T010 CAVEATS / T004 R-notes).
fn prepare_library_for_sync(live_db: &Path) -> Result<PathBuf, String> {
    let sync_db = sync_db_path(live_db);

    // Idempotency gate: already prepared at this schema version?
    if sync_db.exists() {
        if let Ok(db) = CrsqlDb::open(&sync_db) {
            if prepared_version(db.conn()).unwrap_or(0) == SYNC_SCHEMA_VERSION {
                return Ok(sync_db);
            }
        }
        // Stale / partial copy — discard and rebuild from a fresh snapshot.
        std::fs::remove_file(&sync_db).map_err(|e| e.to_string())?;
    }

    // Hot snapshot of the live DB. VACUUM INTO reads the source and writes a
    // fresh compacted copy; it never mutates the source.
    {
        let live = Connection::open(live_db).map_err(|e| e.to_string())?;
        let target = sync_db
            .to_str()
            .ok_or_else(|| "non-UTF8 sync db path".to_string())?;
        live.execute("VACUUM INTO ?1", [target])
            .map_err(|e| format!("snapshot failed: {e}"))?;
    }

    // Transform the COPY into CRR form and reinstate the stripped invariants.
    let db = CrsqlDb::open(&sync_db).map_err(|e| e.to_string())?;
    let conn = db.conn();
    schema::crr_migrate(conn).map_err(|e| format!("crr_migrate: {e}"))?;
    cascade::cascade_gc(conn).map_err(|e| format!("cascade_gc: {e}"))?;
    uniqueness::enforce_uniqueness(conn).map_err(|e| format!("enforce_uniqueness: {e}"))?;
    mark_prepared(conn).map_err(|e| e.to_string())?;
    Ok(sync_db)
}

fn ensure_prep_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _yapstack_sync_prep(schema_version INTEGER NOT NULL);",
    )
}

fn prepared_version(conn: &Connection) -> rusqlite::Result<u32> {
    ensure_prep_table(conn)?;
    let v: Option<i64> = conn
        .query_row(
            "SELECT schema_version FROM _yapstack_sync_prep LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(v.unwrap_or(0) as u32)
}

fn mark_prepared(conn: &Connection) -> rusqlite::Result<()> {
    ensure_prep_table(conn)?;
    conn.execute("DELETE FROM _yapstack_sync_prep", [])?;
    conn.execute(
        "INSERT INTO _yapstack_sync_prep(schema_version) VALUES (?1)",
        [SYNC_SCHEMA_VERSION as i64],
    )?;
    Ok(())
}

// ----- Dedicated single-thread drain runtime (deliverable A) -----

/// Handle to the running drain thread. Dropping/`stop`-ing sets the shutdown
/// flag; the thread exits at the next cycle boundary.
pub struct DrainHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl DrainHandle {
    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Managed Tauri state holding the live drain handle (None until sync enabled).
pub type SyncRuntimeState = Arc<Mutex<Option<DrainHandle>>>;

/// Boot-time hook (called from the Tauri `setup` closure): if the keychain holds
/// an enabled session and the prepared CRR copy exists, start the drain on its
/// dedicated thread. No-op when signed out or sync not yet enabled. This is the
/// concrete deliverable-A wiring into src-tauri startup.
pub fn start_drain_if_enabled(live_db: &Path, runtime: &SyncRuntimeState) {
    let session = match load_session() {
        Ok(Some(s)) if s.sync_enabled => s,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!("sync: keychain read failed at boot: {e}");
            return;
        }
    };
    let vault_key = match session.vault_key() {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("sync: {e}");
            return;
        }
    };
    let sync_db = sync_db_path(live_db);
    if !sync_db.exists() {
        tracing::warn!("sync enabled but prepared DB missing; re-run enable");
        return;
    }
    match spawn_drain(
        sync_db,
        session.server_url,
        session.bearer,
        vault_key,
        session.epoch,
        session.tenant_id,
    ) {
        Ok(handle) => {
            if let Ok(mut g) = runtime.lock() {
                *g = Some(handle);
            }
        }
        Err(e) => tracing::error!("sync: drain spawn failed at boot: {e}"),
    }
}

/// Spawn the drain on its own OS thread with a **current-thread** tokio runtime.
/// The `rusqlite::Connection` (and thus the `!Send` `drain_once` future) lives
/// entirely on this thread — never crossing a thread boundary. After each cycle
/// that applied or replayed changes we run `cascade_gc` + `enforce_uniqueness`
/// (the drain itself only replays quarantine; the merge-time invariants are the
/// desktop's responsibility per the T010 handoff).
fn spawn_drain(
    sync_db: PathBuf,
    server_url: String,
    bearer: String,
    vault_key: [u8; 32],
    epoch: u32,
    tenant_id: Uuid,
) -> Result<DrainHandle, String> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let stop = shutdown.clone();

    let join = std::thread::Builder::new()
        .name("yapstack-sync-drain".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("sync drain: runtime build failed: {e}");
                    return;
                }
            };

            // Open the CRR copy on THIS thread; the connection never leaves it.
            let db = match CrsqlDb::open(&sync_db) {
                Ok(db) => db,
                Err(e) => {
                    tracing::error!("sync drain: cannot open CRR db: {e}");
                    return;
                }
            };
            let conn = db.conn();
            if let Err(e) = outbox::ensure_outbox_table(conn) {
                tracing::error!("sync drain: outbox table: {e}");
                return;
            }
            let client_id = match state::client_id(conn) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("sync drain: client_id: {e}");
                    return;
                }
            };
            let transport = HttpTransport::new(server_url, bearer);
            let cipher = ChangesetCipher::new(
                vault_key,
                epoch,
                tenant_id,
                SYNC_SCHEMA_VERSION,
                CRSQLITE_ENGINE_VERSION,
            );
            let sv = SYNC_SCHEMA_VERSION as i32;
            let ev = CRSQLITE_ENGINE_VERSION as i32;

            while !stop.load(Ordering::SeqCst) {
                match rt.block_on(outbox::drain_once(
                    conn, &cipher, &transport, client_id, sv, ev,
                )) {
                    Ok(report) => {
                        if report.applied + report.replayed > 0 {
                            // Reinstate the stripped FK cascade + UNIQUE invariants
                            // deterministically after a merge (R4/R5).
                            if let Err(e) = cascade::cascade_gc(conn) {
                                tracing::warn!("sync drain: cascade_gc: {e}");
                            }
                            if let Err(e) = uniqueness::enforce_uniqueness(conn) {
                                tracing::warn!("sync drain: enforce_uniqueness: {e}");
                            }
                        }
                    }
                    // Surface, never crash the thread — a transient relay/auth
                    // error must not tear down sync; retry next cycle.
                    Err(e) => tracing::warn!("sync drain cycle failed: {e}"),
                }
                std::thread::sleep(DRAIN_INTERVAL);
            }
            tracing::info!("sync drain stopped");
        })
        .map_err(|e| e.to_string())?;

    Ok(DrainHandle {
        shutdown,
        join: Some(join),
    })
}

// ----- Auth-ceremony crypto + roster helpers (CRYPTO_SPEC §3/§4/§6/§7) -----
//
// All key derivation runs CLIENT-SIDE with the pinned `yapstack-crypto` primitives.
// The client transmits ONLY `auth_key` / `recovery_auth_key` (HKDF-derived, second-
// hashed server-side) and opaque committing-envelope blobs + the signed roster. The
// password, `master_key`, `vault_key`, `recovery_key`, and the recovery code itself
// NEVER leave this process (CRYPTO_SPEC §3.1/§3.2).

/// Split the 160-bit recovery code into the recovery WRAP key and the recovery AUTH
/// key by HKDF-Expand over the `yapstack.recovery.v1` PRK (§6.2). Block 1 (`[0..32]`)
/// is byte-identical to `kdf::recovery_key` (the vault-wrap key); block 2 (`[32..64]`)
/// is the domain-separated `recovery_auth_key` sent to `/auth/recover`, mirroring the
/// password path's auth/master split (§2.3) so the code's wrap key is never exposed as
/// an auth token. Uses only the locked label + `hkdf::expand` primitive (no new crypto).
fn recovery_key_and_auth(recovery_bytes: &[u8; 20]) -> ([u8; 32], [u8; 32]) {
    let okm =
        yapstack_crypto::hkdf::expand(recovery_bytes, yapstack_crypto::kdf::RECOVERY_INFO, 64);
    let mut wrap = [0u8; 32];
    wrap.copy_from_slice(&okm[0..32]);
    let mut auth = [0u8; 32];
    auth.copy_from_slice(&okm[32..64]);
    (wrap, auth)
}

/// Committing-envelope wrap of the vault key under `k_root` (master or recovery key),
/// AAD = `LP(version, domain)` (§4.2, C1 version-first).
fn wrap_vault_key(
    k_root: &[u8; 32],
    vault_key: &[u8; 32],
    domain: &[u8],
) -> Result<Vec<u8>, String> {
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let aad = yapstack_crypto::aead::lp(&[&[yapstack_crypto::VERSION], domain]);
    yapstack_crypto::aead::seal_committing(k_root, &nonce, vault_key, &aad)
        .map_err(|e| e.to_string())
}

/// Unwrap a committing-envelope vault key blob (§4.2). Decrypt failure = quarantine
/// (§11.3), surfaced verbatim; the caller never proceeds with a forged key.
fn unwrap_vault_key(k_root: &[u8; 32], blob: &[u8], domain: &[u8]) -> Result<[u8; 32], String> {
    let aad = yapstack_crypto::aead::lp(&[&[yapstack_crypto::VERSION], domain]);
    let pt =
        yapstack_crypto::aead::open_committing(k_root, blob, &aad).map_err(|e| e.to_string())?;
    pt.try_into()
        .map_err(|_| "unwrapped vault key has wrong length".to_string())
}

/// Device fingerprint (§7.5.5): `SHA-256(ed25519_pub)[..10]` → RFC4648 base32 (upper,
/// no pad) → 16 chars. Returned ungrouped; the UI groups it 4×4 for display/compare.
fn device_fingerprint(ed25519_pub: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(ed25519_pub);
    data_encoding::BASE32_NOPAD.encode(&hash[..10])
}

/// Roster fingerprint (§7.5.5): `SHA-256(canonical_bytes(roster))[..10]` → base32 4×4,
/// shown next to `vault_key_epoch` for the out-of-band approval check.
fn roster_fingerprint_from_canonical(canonical: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(canonical);
    data_encoding::BASE32_NOPAD.encode(&hash[..10])
}

/// A friendly device label (best-effort; not security-relevant).
fn device_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| format!("{} device", std::env::consts::OS))
}

// ----- Signed device roster (§7.3) — one canonical encoder, shared by sign & verify --

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RosterDeviceEntry {
    client_id: Uuid,
    /// base64 of the device's 32-byte Ed25519 public key.
    ed25519_pub: String,
    label: String,
    added_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignedRoster {
    version: u8,
    tenant_id: Uuid,
    vault_key_epoch: u32,
    counter: u64,
    devices: Vec<RosterDeviceEntry>,
}

/// Canonical roster bytes (§7.3): `LP(§5)` over version, tenant_id, vault_key_epoch(u32be),
/// counter(u64be), device count(u32be), then each device's {client_id, ed25519_pub(raw),
/// label, added_at}. Deterministic and unambiguous (LP), so the signer and any verifier
/// reconstructing the roster from its stored JSON produce identical bytes.
fn roster_canonical_bytes(r: &SignedRoster) -> Result<Vec<u8>, String> {
    let mut owned: Vec<Vec<u8>> = Vec::with_capacity(5 + r.devices.len() * 4);
    owned.push(vec![r.version]);
    owned.push(r.tenant_id.as_bytes().to_vec());
    owned.push(r.vault_key_epoch.to_be_bytes().to_vec());
    owned.push(r.counter.to_be_bytes().to_vec());
    owned.push(
        u32::try_from(r.devices.len())
            .map_err(|_| "roster too large".to_string())?
            .to_be_bytes()
            .to_vec(),
    );
    for d in &r.devices {
        owned.push(d.client_id.as_bytes().to_vec());
        let pk = B64
            .decode(d.ed25519_pub.as_bytes())
            .map_err(|_| "roster: invalid ed25519_pub base64".to_string())?;
        owned.push(pk);
        owned.push(d.label.as_bytes().to_vec());
        owned.push(d.added_at.as_bytes().to_vec());
    }
    let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    Ok(yapstack_crypto::aead::lp(&refs))
}

/// The public half of the vault-derived Ed25519 roster signing key (§7.2). This is the
/// key a bootstrapping device derives from the vault key it just unwrapped and verifies
/// the served roster against (§7.5 step 2) — the relay holds no such key and cannot forge.
fn roster_verifying_pub(vault_key: &[u8; 32]) -> [u8; 32] {
    yapstack_crypto::sign::roster_signing_key(vault_key)
        .verifying_key()
        .to_bytes()
}

/// Verify a served roster (§7.5 step 2) and report `(epoch, counter, this_device_active,
/// roster_fingerprint)`. Verification is the client's job: derive the roster public key
/// from the locally-unwrapped vault key, check the Ed25519 signature over the canonical
/// bytes, and enforce §7.4 client anti-rollback against the cached counter (fresh device
/// = TOFU). The relay only checked structural signature presence (§7.5); the crypto is here.
fn verify_and_inspect_roster(
    device_list: Option<&serde_json::Value>,
    signature_b64: Option<&str>,
    vault_key: &[u8; 32],
    this_client_id: Uuid,
    cached_counter: i64,
) -> Result<(u32, u64, bool, String), String> {
    let (Some(value), Some(sig_b64)) = (device_list, signature_b64) else {
        return Err("relay served no signed roster for this account".into());
    };
    let roster: SignedRoster =
        serde_json::from_value(value.clone()).map_err(|e| format!("malformed roster JSON: {e}"))?;
    let canonical = roster_canonical_bytes(&roster)?;
    let sig_bytes = B64
        .decode(sig_b64.as_bytes())
        .map_err(|_| "roster signature: invalid base64".to_string())?;
    let sig: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| "roster signature: expected 64 bytes".to_string())?;
    let pubk = roster_verifying_pub(vault_key);
    // §7.5 step 2: the vault-derived roster key must have authored this roster.
    yapstack_crypto::sign::verify_roster(&pubk, &canonical, &sig).map_err(|_| {
        "roster signature verification FAILED — refusing hostile/forged roster".to_string()
    })?;
    // §7.4 client anti-rollback: never accept a roster older than one already verified.
    if (roster.counter as i64) < cached_counter {
        return Err(format!(
            "roster counter {} is older than the last verified counter {cached_counter} — rollback rejected",
            roster.counter
        ));
    }
    let active = roster.devices.iter().any(|d| d.client_id == this_client_id);
    let fp = roster_fingerprint_from_canonical(&canonical);
    Ok((roster.vault_key_epoch, roster.counter, active, fp))
}

// ----- Two-round login state + HTTP helpers -----

#[derive(Clone)]
struct PendingLogin {
    server_url: String,
    email: String,
    salt_enc: Vec<u8>,
}

fn pending_login_cell() -> &'static Mutex<Option<PendingLogin>> {
    static CELL: OnceLock<Mutex<Option<PendingLogin>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn base_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Send a request and decode JSON, surfacing a relay error body VERBATIM on non-2xx
/// (never auto-routed or masked — the user fixes a broken connection, per privacy posture).
async fn send_json<Resp: serde::de::DeserializeOwned>(
    rb: reqwest::RequestBuilder,
) -> Result<Resp, String> {
    let resp = rb.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("relay error {}: {}", status.as_u16(), body.trim()));
    }
    resp.json::<Resp>().await.map_err(|e| e.to_string())
}

/// `GET /devices` with the session bearer (§7.5 step 3).
async fn http_get_devices(session: &Session) -> Result<DevicesResponse, String> {
    let url = format!("{}/devices", base_url(&session.server_url));
    send_json(
        reqwest::Client::new()
            .get(&url)
            .bearer_auth(&session.bearer),
    )
    .await
}

/// Reuse this install's device identity if the keychain already holds one for this
/// account, else mint a fresh `client_id` + Ed25519 seed (§7.1). A brand-new install
/// therefore enrolls as a PENDING device on login (§7.5); a re-login on the same device
/// keeps its identity and stays active.
fn load_or_create_device_identity(server_url: &str, email: &str) -> ([u8; 32], Uuid) {
    if let Ok(Some(s)) = load_session() {
        if s.server_url == server_url
            && s.email.eq_ignore_ascii_case(email)
            && !s.device_sk_b64.is_empty()
        {
            if let Ok(seed) = B64.decode(s.device_sk_b64.as_bytes()) {
                if let Ok(arr) = <[u8; 32]>::try_from(seed.as_slice()) {
                    return (arr, s.client_id);
                }
            }
        }
    }
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    (seed, Uuid::new_v4())
}

/// Build the UI status DTO for a signed-in session, fetching the live device index
/// (`GET /devices`) so pending-device approvals surface (§7.5). A relay error is
/// surfaced in `last_error` (roster falls back to empty) rather than failing the call.
async fn build_status_dto(session: &Session) -> SyncStatusDto {
    let mut roster = Vec::new();
    let mut last_error = None;
    match http_get_devices(session).await {
        Ok(devs) => {
            for d in devs.devices {
                let pk = B64.decode(d.ed25519_pub.as_bytes()).unwrap_or_default();
                roster.push(DeviceRosterEntryDto {
                    fingerprint: device_fingerprint(&pk),
                    is_self: d.client_id == session.client_id,
                    pending: d.status == "pending",
                    label: if d.label.is_empty() {
                        None
                    } else {
                        Some(d.label)
                    },
                });
            }
        }
        Err(e) => last_error = Some(e),
    }
    SyncStatusDto {
        phase: if session.sync_enabled {
            "connected".into()
        } else {
            "connecting".into()
        },
        server_url: session.server_url.clone(),
        email: Some(session.email.clone()),
        device_fingerprint: session.device_fingerprint.clone(),
        roster,
        vault_key_epoch: Some(session.epoch),
        roster_fingerprint: session.roster_fingerprint.clone(),
        sync_enabled: session.sync_enabled,
        last_error,
        billing_url: None,
    }
}

// ----- Tauri commands (contract mirrors apps/desktop/src/lib/sync.ts) -----

#[tauri::command]
#[specta::specta]
pub async fn sync_info(server_url: String) -> Result<SyncInfoDto, String> {
    let url = format!("{}/sync/info", server_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json::<SyncInfoResponse>()
        .await
        .map_err(|e| e.to_string())?;
    Ok(SyncInfoDto {
        server_url,
        version: resp.engine_version,
        billing_url: resp.billing_url,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn sync_status() -> Result<SyncStatusDto, String> {
    match load_session()? {
        None => Ok(SyncStatusDto {
            phase: "disconnected".into(),
            server_url: String::new(),
            email: None,
            device_fingerprint: None,
            roster: vec![],
            vault_key_epoch: None,
            roster_fingerprint: None,
            sync_enabled: false,
            last_error: None,
            billing_url: None,
        }),
        // Signed in: surface the live device index (incl. pending approvals, §7.5).
        Some(s) => Ok(build_status_dto(&s).await),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn sync_enable(
    app: tauri::AppHandle,
    runtime: State<'_, SyncRuntimeState>,
) -> Result<SyncStatusDto, String> {
    let mut session = load_session()?.ok_or_else(|| "Sign in before enabling sync.".to_string())?;

    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();

    // Deliverable F: crr_migrate on a COPY, gated on SYNC_SCHEMA_VERSION.
    let sync_db = prepare_library_for_sync(&db_path)?;

    // Deliverable A: start the drain on its dedicated thread.
    let vault_key = session.vault_key()?;
    let handle = spawn_drain(
        sync_db,
        session.server_url.clone(),
        session.bearer.clone(),
        vault_key,
        session.epoch,
        session.tenant_id,
    )?;
    {
        let mut guard = runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?;
        if let Some(mut prev) = guard.take() {
            prev.stop();
        }
        *guard = Some(handle);
    }

    session.sync_enabled = true;
    store_session(&session)?;
    Ok(build_status_dto(&session).await)
}

/// Vault-wrap a plaintext secret (AI apiKey / baseUrl) under the vault key held
/// in the OS keychain, before it can reach any syncable surface (deliverable E).
/// The plaintext is consumed here and never persisted; the caller stores only
/// the returned committing envelope (CRYPTO_SPEC §1.4 / §4).
#[tauri::command]
#[specta::specta]
pub fn sync_wrap_secret(plaintext: String) -> Result<String, String> {
    let session = load_session()?.ok_or_else(|| "Sign in before wrapping secrets.".to_string())?;
    let vault_key = session.vault_key()?;
    let mut nonce = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let aad = yapstack_crypto::aead::lp(&[&[yapstack_crypto::VERSION], SETTING_DOMAIN]);
    let blob =
        yapstack_crypto::aead::seal_committing(&vault_key, &nonce, plaintext.as_bytes(), &aad)
            .map_err(|e| e.to_string())?;
    Ok(B64.encode(blob))
}

#[tauri::command]
#[specta::specta]
pub fn sync_sign_out(runtime: State<'_, SyncRuntimeState>) -> Result<(), String> {
    if let Ok(mut guard) = runtime.lock() {
        if let Some(mut handle) = guard.take() {
            handle.stop();
        }
    }
    clear_session()
}

// --- Relay auth ceremony (CRYPTO_SPEC §3/§6/§7). All key derivation is client-side;
//     the client transmits only auth_key / recovery_auth_key + opaque wrapped blobs +
//     the signed roster. Password / master_key / vault_key / recovery code never leave
//     this process. The vault key is stored in the OS keychain only (§10).

/// Create the account (§3.2 signup). Derives auth+master keys from the password, mints a
/// random vault key and a CSPRNG recovery code, wraps the vault key under BOTH the master
/// key and the recovery key (committing envelopes, §4.2/§6.2), authors the epoch-0 signed
/// roster (§7.5 first-device self-enrollment), and POSTs the verifier inputs + wraps +
/// roster. Returns the one-time recovery code (for forced capture) + this device's
/// fingerprint. NEVER sends the password / master_key / vault_key / recovery code.
#[tauri::command]
#[specta::specta]
pub async fn sync_signup(req: SignupArgs) -> Result<SignupResultDto, String> {
    let server_url = base_url(&req.server_url);

    // §2.3 client stretch → auth/master split.
    let mut salt_enc = [0u8; 16];
    OsRng.fill_bytes(&mut salt_enc);
    let stretch = yapstack_crypto::kdf::client_stretch(req.password.as_bytes(), &salt_enc)
        .map_err(|e| e.to_string())?;
    let (auth_key, master_key_v) = yapstack_crypto::kdf::split_keys(&stretch);
    let master_key: [u8; 32] = master_key_v
        .as_slice()
        .try_into()
        .map_err(|_| "master_key length".to_string())?;

    // §4.1 random vault key.
    let mut vault_key = [0u8; 32];
    OsRng.fill_bytes(&mut vault_key);

    // §6.1 recovery code (160-bit CSPRNG) → wrap key + auth key (§6.2).
    let mut recovery_bytes = [0u8; 20];
    OsRng.fill_bytes(&mut recovery_bytes);
    let (recovery_key, recovery_auth_key) = recovery_key_and_auth(&recovery_bytes);

    // §4.2/§6.2 committing wraps of the vault key.
    let wrapped_pw = wrap_vault_key(&master_key, &vault_key, WRAP_VAULT_PW_DOMAIN)?;
    let wrapped_rec = wrap_vault_key(&recovery_key, &vault_key, WRAP_VAULT_REC_DOMAIN)?;

    // §7.1 device identity + §7.5 first-device self-enrolled roster (counter 0, epoch 0).
    let client_id = Uuid::new_v4();
    let mut dev_seed = [0u8; 32];
    OsRng.fill_bytes(&mut dev_seed);
    let dev_pub = SigningKey::from_bytes(&dev_seed).verifying_key().to_bytes();
    let dev_pub_b64 = B64.encode(dev_pub);
    let label = device_label();
    let added_at = chrono::Utc::now().to_rfc3339();
    // The server assigns the workspace/tenant id at signup, so it is not known here; the
    // epoch-0 roster is signed with tenant_id=nil and re-anchored to the real tenant when
    // an existing device first re-signs (approval, §7.5). Client verification checks the
    // Ed25519 signature + §7.4 counter, not tenant equality, so this is sound for v1.
    let roster = SignedRoster {
        version: 1,
        tenant_id: Uuid::nil(),
        vault_key_epoch: 0,
        counter: 0,
        devices: vec![RosterDeviceEntry {
            client_id,
            ed25519_pub: dev_pub_b64.clone(),
            label: label.clone(),
            added_at,
        }],
    };
    let canonical = roster_canonical_bytes(&roster)?;
    let signature = yapstack_crypto::sign::sign_roster(&vault_key, &canonical);
    let roster_fp = roster_fingerprint_from_canonical(&canonical);
    let device_list_value =
        serde_json::to_value(&roster).map_err(|e| format!("roster encode: {e}"))?;

    let body = SignupRequest {
        email: req.email.clone(),
        auth_key: B64.encode(auth_key),
        recovery_auth_key: B64.encode(recovery_auth_key),
        salt_enc: B64.encode(salt_enc),
        wrapped_vault_key_password: B64.encode(&wrapped_pw),
        wrapped_vault_key_recovery: B64.encode(&wrapped_rec),
        device_list: RosterEnvelope {
            device_list: device_list_value,
            signature: B64.encode(signature),
            counter: 0,
            vault_key_epoch: 0,
            client_id,
            ed25519_pub: dev_pub_b64,
            label,
        },
    };

    let url = format!("{server_url}/auth/signup");
    let tokens: TokenResponse = send_json(reqwest::Client::new().post(&url).json(&body)).await?;

    let device_fingerprint = device_fingerprint(&dev_pub);
    let session = Session {
        server_url,
        email: req.email,
        vault_key_b64: B64.encode(vault_key),
        epoch: 0,
        tenant_id: tokens.tenant_id,
        bearer: tokens.access_token,
        device_fingerprint: Some(device_fingerprint.clone()),
        sync_enabled: false,
        client_id,
        device_sk_b64: B64.encode(dev_seed),
        salt_enc_b64: Some(B64.encode(salt_enc)),
        roster_counter: 0,
        roster_fingerprint: Some(roster_fp),
    };
    store_session(&session)?;

    // The recovery code is displayed once and never persisted (§6.1). base32, ungrouped;
    // the UI groups it 8×4.
    Ok(SignupResultDto {
        recovery_code: data_encoding::BASE32_NOPAD.encode(&recovery_bytes),
        device_fingerprint,
    })
}

/// Round 1 of two-round login (§3.2): fetch `salt_enc`, run the §3.2-C3 known-device
/// salt-mismatch check (a changed salt for an established account = hostile-relay signal),
/// and cache the round-1 state for `sync_login_finish`.
#[tauri::command]
#[specta::specta]
pub async fn sync_login_begin(
    server_url: String,
    email: String,
) -> Result<LoginBeginResultDto, String> {
    let server_url = base_url(&server_url);
    let url = format!("{server_url}/auth/login/begin");
    let resp: LoginBeginResponse =
        send_json(reqwest::Client::new().post(&url).json(&LoginBeginRequest {
            email: email.clone(),
        }))
        .await?;
    let served_salt = B64
        .decode(resp.salt_enc.as_bytes())
        .map_err(|_| "relay salt_enc: invalid base64".to_string())?;

    // §3.2-C3: on a known device, a changed salt is a tamper/downgrade signal.
    let salt_mismatch = match load_session()? {
        Some(s) if s.server_url == server_url && s.email.eq_ignore_ascii_case(&email) => {
            match s.salt_enc_b64 {
                Some(cached_b64) => B64
                    .decode(cached_b64.as_bytes())
                    .map(|cached| cached != served_salt)
                    .unwrap_or(false),
                None => false,
            }
        }
        _ => false, // first-time device: TOFU-accept the served salt (§3.2).
    };

    *pending_login_cell()
        .lock()
        .map_err(|_| "login state lock".to_string())? = Some(PendingLogin {
        server_url,
        email,
        salt_enc: served_salt,
    });
    Ok(LoginBeginResultDto { salt_mismatch })
}

/// Round 2 of login (§3.2): derive `auth_key`, present this device's Ed25519 pubkey +
/// client_id (so an unknown device enrolls PENDING, §7.5 step 1), authenticate, unwrap the
/// vault key, VERIFY the served roster signature (§7.5 step 2), and store the session. A
/// device not yet in the roster is PENDING and surfaces as such (needs approval).
#[tauri::command]
#[specta::specta]
pub async fn sync_login_finish(password: String) -> Result<SyncStatusDto, String> {
    let pending = pending_login_cell()
        .lock()
        .map_err(|_| "login state lock".to_string())?
        .clone()
        .ok_or_else(|| "Call login_begin first.".to_string())?;

    let stretch = yapstack_crypto::kdf::client_stretch(password.as_bytes(), &pending.salt_enc)
        .map_err(|e| e.to_string())?;
    let (auth_key, master_key_v) = yapstack_crypto::kdf::split_keys(&stretch);
    let master_key: [u8; 32] = master_key_v
        .as_slice()
        .try_into()
        .map_err(|_| "master_key length".to_string())?;

    let (dev_seed, client_id) = load_or_create_device_identity(&pending.server_url, &pending.email);
    let dev_pub = SigningKey::from_bytes(&dev_seed).verifying_key().to_bytes();

    let cached_counter = load_session()?
        .filter(|s| {
            s.server_url == pending.server_url && s.email.eq_ignore_ascii_case(&pending.email)
        })
        .map(|s| s.roster_counter)
        .unwrap_or(-1);

    let url = format!("{}/auth/login/finish", pending.server_url);
    let resp: LoginFinishResponse =
        send_json(reqwest::Client::new().post(&url).json(&LoginFinishRequest {
            email: pending.email.clone(),
            auth_key: B64.encode(auth_key),
            client_id: Some(client_id),
            ed25519_pub: Some(B64.encode(dev_pub)),
            label: Some(device_label()),
        }))
        .await?;

    let wrapped_pw = B64
        .decode(resp.wrapped_vault_key_password.as_bytes())
        .map_err(|_| "wrapped_vault_key_password: invalid base64".to_string())?;
    let vault_key = unwrap_vault_key(&master_key, &wrapped_pw, WRAP_VAULT_PW_DOMAIN)?;

    // §7.5 step 2: verify the roster with the vault-derived key we just unwrapped.
    let (epoch, counter, this_active, roster_fp) = verify_and_inspect_roster(
        resp.device_list.as_ref(),
        resp.signature.as_deref(),
        &vault_key,
        client_id,
        cached_counter,
    )?;

    let served_salt = B64
        .decode(resp.salt_enc.as_bytes())
        .map_err(|_| "relay salt_enc: invalid base64".to_string())?;
    let device_fingerprint = device_fingerprint(&dev_pub);
    let session = Session {
        server_url: pending.server_url,
        email: pending.email,
        vault_key_b64: B64.encode(vault_key),
        epoch,
        tenant_id: resp.tenant_id,
        bearer: resp.access_token,
        device_fingerprint: Some(device_fingerprint),
        // A newly-enrolled device is PENDING (not yet a roster member) and must not run
        // the drain until approved; keep sync disabled until this device is active.
        sync_enabled: false,
        client_id,
        device_sk_b64: B64.encode(dev_seed),
        salt_enc_b64: Some(B64.encode(served_salt)),
        roster_counter: counter as i64,
        roster_fingerprint: Some(roster_fp),
    };
    store_session(&session)?;
    let _ = this_active; // active-vs-pending is surfaced via the roster in the status DTO.
    Ok(build_status_dto(&session).await)
}

/// Recover access with the base32 recovery code (§6.2) when the password is lost:
/// authenticate via `recovery_auth_key`, receive `wrapped_vault_key_recovery`, unwrap the
/// vault key with the recovery key, verify the roster, and store the session (vault key →
/// keychain). The raw recovery code and recovery key never leave this process.
#[tauri::command]
#[specta::specta]
pub async fn sync_recover(
    server_url: String,
    email: String,
    recovery_code: String,
) -> Result<SyncStatusDto, String> {
    let server_url = base_url(&server_url);
    let normalized: String = recovery_code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect();
    let decoded = data_encoding::BASE32_NOPAD
        .decode(normalized.as_bytes())
        .map_err(|_| "recovery code: not valid base32".to_string())?;
    let recovery_bytes: [u8; 20] = decoded
        .as_slice()
        .try_into()
        .map_err(|_| "recovery code: expected 160 bits".to_string())?;
    let (recovery_key, recovery_auth_key) = recovery_key_and_auth(&recovery_bytes);

    let url = format!("{server_url}/auth/recover");
    let resp: RecoverResponse =
        send_json(reqwest::Client::new().post(&url).json(&RecoverRequest {
            email: email.clone(),
            recovery_auth_key: B64.encode(recovery_auth_key),
        }))
        .await?;

    let wrapped_rec = B64
        .decode(resp.wrapped_vault_key_recovery.as_bytes())
        .map_err(|_| "wrapped_vault_key_recovery: invalid base64".to_string())?;
    let vault_key = unwrap_vault_key(&recovery_key, &wrapped_rec, WRAP_VAULT_REC_DOMAIN)?;

    let (dev_seed, client_id) = load_or_create_device_identity(&server_url, &email);
    let dev_pub = SigningKey::from_bytes(&dev_seed).verifying_key().to_bytes();
    let cached_counter = load_session()?
        .filter(|s| s.server_url == server_url && s.email.eq_ignore_ascii_case(&email))
        .map(|s| s.roster_counter)
        .unwrap_or(-1);

    let (epoch, counter, _active, roster_fp) = verify_and_inspect_roster(
        resp.device_list.as_ref(),
        resp.signature.as_deref(),
        &vault_key,
        client_id,
        cached_counter,
    )?;

    let served_salt = B64
        .decode(resp.salt_enc.as_bytes())
        .map_err(|_| "relay salt_enc: invalid base64".to_string())?;
    let session = Session {
        server_url,
        email,
        vault_key_b64: B64.encode(vault_key),
        epoch,
        tenant_id: resp.tenant_id,
        bearer: resp.access_token,
        device_fingerprint: Some(device_fingerprint(&dev_pub)),
        sync_enabled: false,
        client_id,
        device_sk_b64: B64.encode(dev_seed),
        salt_enc_b64: Some(B64.encode(served_salt)),
        roster_counter: counter as i64,
        roster_fingerprint: Some(roster_fp),
    };
    store_session(&session)?;
    Ok(build_status_dto(&session).await)
}

/// Approve a pending device (§7.5 step 3/4). The human has already compared the
/// fingerprint OUT-OF-BAND in the UI (DeviceApprovalDialog); this existing active device
/// re-verifies the fingerprint matches a real pending device, adds it to the roster,
/// bumps the monotonic counter, re-signs with the vault-derived roster key, and uploads.
#[tauri::command]
#[specta::specta]
pub async fn sync_approve_device(fingerprint: String) -> Result<SyncStatusDto, String> {
    let mut session = load_session()?.ok_or_else(|| "Sign in first.".to_string())?;
    let vault_key = session.vault_key()?;

    let devices = http_get_devices(&session).await?;

    // Locate the pending device by its out-of-band-verified fingerprint (§7.5 step 4).
    let target = devices
        .devices
        .iter()
        .find(|d| {
            d.status == "pending"
                && B64
                    .decode(d.ed25519_pub.as_bytes())
                    .map(|pk| device_fingerprint(&pk) == fingerprint)
                    .unwrap_or(false)
        })
        .ok_or_else(|| "No pending device with that fingerprint.".to_string())?;
    let target_id = target.client_id;

    // New roster membership = every active device + this device + the approved one.
    let mut entries: Vec<RosterDeviceEntry> = Vec::new();
    let mut active_ids: Vec<Uuid> = Vec::new();
    for d in &devices.devices {
        let include =
            d.status == "active" || d.client_id == target_id || d.client_id == session.client_id;
        if include && !active_ids.contains(&d.client_id) {
            entries.push(RosterDeviceEntry {
                client_id: d.client_id,
                ed25519_pub: d.ed25519_pub.clone(),
                label: d.label.clone(),
                added_at: d.added_at.clone(),
            });
            active_ids.push(d.client_id);
        }
    }

    let new_counter = session.roster_counter + 1;
    let roster = SignedRoster {
        version: 1,
        tenant_id: session.tenant_id,
        vault_key_epoch: session.epoch,
        counter: new_counter as u64,
        devices: entries,
    };
    let canonical = roster_canonical_bytes(&roster)?;
    let signature = yapstack_crypto::sign::sign_roster(&vault_key, &canonical);
    let roster_fp = roster_fingerprint_from_canonical(&canonical);
    let device_list_value =
        serde_json::to_value(&roster).map_err(|e| format!("roster encode: {e}"))?;

    let url = format!("{}/devices/roster", base_url(&session.server_url));
    let resp: RosterUploadResponse = send_json(
        reqwest::Client::new()
            .put(&url)
            .bearer_auth(&session.bearer)
            .json(&RosterUploadRequest {
                device_list: device_list_value,
                signature: B64.encode(signature),
                counter: new_counter,
                vault_key_epoch: session.epoch as i64,
                active_devices: active_ids,
            }),
    )
    .await?;

    session.roster_counter = resp.counter;
    session.roster_fingerprint = Some(roster_fp);
    store_session(&session)?;
    Ok(build_status_dto(&session).await)
}

/// `GET /devices` for the UI: the account's device index (pending + active), fingerprinted
/// for out-of-band comparison (§7.5). Membership truth is the signed roster (client-verified
/// at login); this index is the relay's advisory view for surfacing pending approvals.
#[tauri::command]
#[specta::specta]
pub async fn sync_device_list() -> Result<Vec<DeviceRosterEntryDto>, String> {
    let session = load_session()?.ok_or_else(|| "Sign in first.".to_string())?;
    let devices = http_get_devices(&session).await?;
    Ok(devices
        .devices
        .into_iter()
        .map(|d| {
            let pk = B64.decode(d.ed25519_pub.as_bytes()).unwrap_or_default();
            DeviceRosterEntryDto {
                fingerprint: device_fingerprint(&pk),
                is_self: d.client_id == session.client_id,
                pending: d.status == "pending",
                label: if d.label.is_empty() {
                    None
                } else {
                    Some(d.label)
                },
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_wrap_block_matches_kdf_recovery_key() {
        // Block 1 of the 64-byte HKDF expansion MUST equal kdf::recovery_key (the vault
        // wrap key), so the recovery code still unwraps a vault key wrapped under it.
        let rb = [7u8; 20];
        let (wrap, auth) = recovery_key_and_auth(&rb);
        assert_eq!(wrap.to_vec(), yapstack_crypto::kdf::recovery_key(&rb));
        // The auth key is domain-separated (block 2), never equal to the wrap key.
        assert_ne!(wrap, auth);
    }

    #[test]
    fn roster_roundtrip_signs_and_verifies() {
        // A roster authored under a vault key verifies against that vault's roster pubkey,
        // and a different vault key's pubkey rejects it (§7.5 step 2).
        let vault_key = [3u8; 32];
        let dev_pub = SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes();
        let roster = SignedRoster {
            version: 1,
            tenant_id: Uuid::nil(),
            vault_key_epoch: 0,
            counter: 0,
            devices: vec![RosterDeviceEntry {
                client_id: Uuid::from_u128(1),
                ed25519_pub: B64.encode(dev_pub),
                label: "test".into(),
                added_at: "2026-01-01T00:00:00Z".into(),
            }],
        };
        let canonical = roster_canonical_bytes(&roster).unwrap();
        let sig = yapstack_crypto::sign::sign_roster(&vault_key, &canonical);
        let value = serde_json::to_value(&roster).unwrap();

        let (epoch, counter, active, _fp) = verify_and_inspect_roster(
            Some(&value),
            Some(&B64.encode(sig)),
            &vault_key,
            Uuid::from_u128(1),
            -1,
        )
        .unwrap();
        assert_eq!((epoch, counter, active), (0, 0, true));

        // Wrong vault key → verification fails.
        assert!(verify_and_inspect_roster(
            Some(&value),
            Some(&B64.encode(sig)),
            &[4u8; 32],
            Uuid::from_u128(1),
            -1,
        )
        .is_err());

        // A device not in the roster is reported inactive (pending).
        let (_, _, active2, _) = verify_and_inspect_roster(
            Some(&value),
            Some(&B64.encode(sig)),
            &vault_key,
            Uuid::from_u128(999),
            -1,
        )
        .unwrap();
        assert!(!active2);
    }

    #[test]
    fn anti_rollback_rejects_stale_counter() {
        let vault_key = [5u8; 32];
        let roster = SignedRoster {
            version: 1,
            tenant_id: Uuid::nil(),
            vault_key_epoch: 0,
            counter: 2,
            devices: vec![],
        };
        let canonical = roster_canonical_bytes(&roster).unwrap();
        let sig = yapstack_crypto::sign::sign_roster(&vault_key, &canonical);
        let value = serde_json::to_value(&roster).unwrap();
        // cached_counter 5 > roster counter 2 → rollback rejected.
        assert!(verify_and_inspect_roster(
            Some(&value),
            Some(&B64.encode(sig)),
            &vault_key,
            Uuid::nil(),
            5,
        )
        .is_err());
    }
}
