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
//! at rest (CRYPTO_SPEC §10).
//!
//! Command surface (all registered live in `lib.rs`, `lib/sync.ts` is the TS
//! contract): the relay probe (`sync_probe`), the status poll (`sync_status`),
//! the auth ceremony round-trips (`sync_signup` / `sync_login_begin` /
//! `sync_login_finish` / `sync_recover` / `sync_approve_device` / `sync_sign_out`),
//! and enable (`sync_enable`, THE single enable path — crr_migrate a copy then
//! start the drain). `start_drain_if_enabled` is the deliverable-A boot wiring
//! invoked from the `setup` hook.
//!
//! Session store layout (three logical entries under one keychain service): the
//! persistent device identity (`identity-v1`, survives sign-out) holds the §7.1
//! `client_id` + Ed25519 seed; the session (tokens + vault key + roster metadata)
//! is sealed under a random 32-byte wrapping key kept in `session-key-v1` and
//! written to the `sync-session.enc` file (Windows credential-blob overflow fix,
//! T029). In debug the whole store is a plaintext dev file (T020: dev-rebuild
//! keychain re-prompt spam) keyed by `session-v1`.
//!
//! Parked (compiled, NOT registered as commands): `sync_seed` / `sync_join` — the
//! future "migrate an existing library" (snapshot bootstrap + reconcile) feature.
//! They stay in the tree with `reconcile.rs`; see SYNC_REMEDIATION.md §6b. The
//! module keeps `#![allow(dead_code)]` because that parked surface (and the DTOs
//! it alone returns) is intentionally unreferenced by the live command set.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

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
    LoginFinishResponse, RecoverRequest, RecoverResponse, RefreshRequest, RosterEnvelope,
    RosterUploadRequest, RosterUploadResponse, SignupRequest, TokenResponse,
};
use yapstack_sync::crypto::{ChangesetCipher, SnapshotCipher};
use yapstack_sync::snapshot::{self, SnapshotMeta};
use yapstack_sync::transport::SyncTransport;
use yapstack_sync::{
    cascade, outbox, reconcile, schema, state, transport::HttpTransport, uniqueness, CrsqlDb,
    CRSQLITE_ENGINE_VERSION, SYNC_SCHEMA_VERSION,
};

const KEYCHAIN_SERVICE: &str = "dev.yapstack.app.sync";
/// Session account name for the DEBUG plaintext dev-file store ONLY. In debug the whole
/// session JSON lives under this key in `sync-dev-creds.json` (T020: a dev rebuild changes
/// the code signature, so the macOS keychain re-prompts on every read — the dev store makes
/// zero keychain calls). In RELEASE the session is NOT stored here at all: it lives in the
/// key-wrapped `sync-session.enc` file with its wrapping key in `KEYCHAIN_ACCOUNT_SESSION_KEY`
/// (T029, Windows credential-blob overflow fix).
const KEYCHAIN_ACCOUNT: &str = "session-v1";
/// Session wrapping-key account (T029). Holds ONLY a base64-encoded random 32-byte
/// XChaCha20-Poly1305 key (~44 chars — far under every platform's credential limit). The
/// session JSON itself is encrypted under this key and written to `session_enc_path()`; the
/// keychain never again stores the oversized session BLOB. Same service as the session/identity
/// entries; distinct account so sign-out clears the key + file without touching `identity-v1`.
const KEYCHAIN_ACCOUNT_SESSION_KEY: &str = "session-key-v1";
/// A DISTINCT keychain account (same service) holding ONLY this install's persistent
/// device identity (§7.1): `client_id` + the Ed25519 signing seed. Deliberately separate
/// from the session entry so sign-out — which deletes the session entry — never destroys
/// the identity. If the two shared one entry, sign-out→sign-in on the SAME machine would
/// lose the id, mint a fresh one, and re-enrol as a phantom PENDING device (§7.5). This
/// entry survives sign-out; only the session entry is cleared.
const KEYCHAIN_ACCOUNT_IDENTITY: &str = "identity-v1";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// AAD domain for the password-wrapped vault key (CRYPTO_SPEC §4.2).
const WRAP_VAULT_PW_DOMAIN: &[u8] = b"yapstack.wrap.vault.pw.v1";
/// AAD domain for the recovery-wrapped vault key (CRYPTO_SPEC §4.2/§6.2).
const WRAP_VAULT_REC_DOMAIN: &[u8] = b"yapstack.wrap.vault.rec.v1";
/// AAD domain binding the at-rest session file envelope (T029). Distinct from every key-wrap /
/// changeset / setting domain so a session ciphertext can never be confused with another
/// surface (CRYPTO_SPEC §5 domain separation). Bound as the second AAD field after the
/// authenticated version byte.
///
/// R3 anti-rollback: bumped `v1 → v2` when `vault_key_epoch` was added as a THIRD AAD field
/// (`session_aad`). A `v1` blob (domain-only AAD, no epoch) can never open under the `v2` AAD,
/// so any file sealed by an older build fails cleanly → signed-out degrade. This is safe by
/// construction: there are ZERO production installs and dev builds use the plaintext debug
/// store, so no migration path is owed (T029 compatibility call).
const SESSION_STORE_DOMAIN: &[u8] = b"yapstack.session.store.v2";
/// How often the drain cycles when idle. SSE wakeups (T008) can shorten this
/// later; a fixed poll is correct and simplest for v1.
const DRAIN_INTERVAL: Duration = Duration::from_secs(5);

/// How many CONSECUTIVE drain cycles must hit a (non-fatal) push/pull error before the
/// panel flips to a distinct "Sync error" state (F2). A single blip — relay restart, laptop
/// sleep, a momentary network drop — is common and self-heals next cycle, so we require a
/// short run before nagging the owner; the verbatim error is then surfaced honestly. A later
/// clean cycle clears it. Value 2: the smallest run that distinguishes a real outage from a
/// one-cycle transient.
const DRAIN_FAIL_SURFACE_THRESHOLD: u32 = 2;

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
    /// Access bearer token (§3.2). Short-lived (15 min); rotated by the refresh flow.
    bearer: String,
    /// Long-lived (30 d) refresh token (§5/§10). Used to mint a fresh access token when
    /// the drain sees a 401 (Bug A). `Option` with a serde default so an EXISTING
    /// persisted session written before this field existed still deserializes: a missing
    /// refresh token degrades to "needs re-login" (the drain surfaces auth-expired), it
    /// is NEVER a deserialization failure that would sign the owner out. NEVER logged.
    #[serde(default)]
    refresh_token: Option<String>,
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

/// This install's persistent device identity (§7.1), held in its OWN keychain entry so it
/// SURVIVES sign-out. `client_id` is the stable §7.1 UUIDv4; `device_sk_b64` is the private
/// Ed25519 signing seed — kept in the keychain, NEVER transmitted (only the public key is
/// listed in the roster, §7.5). The session entry may still carry a convenience copy, but
/// THIS entry is the single source of truth for identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceIdentity {
    client_id: Uuid,
    /// base64 of this device's 32-byte Ed25519 signing seed (§7.1).
    device_sk_b64: String,
}

impl DeviceIdentity {
    fn seed(&self) -> Result<[u8; 32], String> {
        let bytes = B64
            .decode(self.device_sk_b64.as_bytes())
            .map_err(|_| "corrupt device_sk in keychain".to_string())?;
        bytes
            .try_into()
            .map_err(|_| "device_sk wrong length".to_string())
    }
}

// ----- Credential storage backend (release = OS keychain; debug = local file) -----
//
// A thin get/set/delete abstraction over the two logical entries (`session-v1`,
// `identity-v1`). RELEASE routes to the OS keychain (CRYPTO_SPEC §10 — the vault key +
// bearer live at rest in the platform secret store). DEBUG routes to a plaintext JSON
// file under the app config dir instead.
//
// WHY (T020): in a dev build the code signature changes on every `tauri dev` rebuild, so
// macOS never persists the "Always Allow" ACL and re-prompts on EVERY keychain read. The
// frontend polls `sync_status` ~1/s → a Keychain prompt per second, unusable. In debug we
// therefore make ZERO keychain calls.
//
// DEBUG-ONLY. The file store is compiled out of release entirely (`cfg(debug_assertions)`);
// it is plaintext because it only ever exists on a developer's own machine. The RELEASE
// keychain path MUST be exercised before shipping — it is the only at-rest store that
// satisfies CRYPTO_SPEC §10.

#[cfg(debug_assertions)]
static DEV_STORE_DIR: OnceLock<PathBuf> = OnceLock::new();

#[cfg(debug_assertions)]
fn dev_store_path() -> PathBuf {
    DEV_STORE_DIR
        .get()
        .cloned()
        .unwrap_or_else(std::env::temp_dir)
        .join("sync-dev-creds.json")
}

/// Directory for the key-wrapped session file (T029), set from the app config dir at boot.
/// Available in BOTH debug and release: the encrypted session file (`sync-session.enc`) is
/// the same on every platform — only the wrapping KEY's home differs (keychain in release,
/// dev file in debug, via `store_get/set`).
static SESSION_STORE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Path to the encrypted at-rest session file (T029). Falls back to the temp dir before
/// `init_credential_store` runs, mirroring `dev_store_path`.
fn session_enc_path() -> PathBuf {
    SESSION_STORE_DIR
        .get()
        .cloned()
        .unwrap_or_else(std::env::temp_dir)
        .join("sync-session.enc")
}

#[cfg(debug_assertions)]
fn dev_read_map() -> Result<std::collections::BTreeMap<String, String>, String> {
    match std::fs::read_to_string(dev_store_path()) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(debug_assertions)]
fn dev_write_map(map: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    let path = dev_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

#[cfg(debug_assertions)]
fn store_get(account: &str) -> Result<Option<String>, String> {
    Ok(dev_read_map()?.get(account).cloned())
}

#[cfg(not(debug_assertions))]
fn store_get(account: &str) -> Result<Option<String>, String> {
    match keyring::Entry::new(KEYCHAIN_SERVICE, account)
        .map_err(|e| e.to_string())?
        .get_password()
    {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(debug_assertions)]
fn store_set(account: &str, value: &str) -> Result<(), String> {
    let mut map = dev_read_map()?;
    map.insert(account.to_string(), value.to_string());
    dev_write_map(&map)
}

#[cfg(not(debug_assertions))]
fn store_set(account: &str, value: &str) -> Result<(), String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account)
        .map_err(|e| e.to_string())?
        .set_password(value)
        .map_err(|e| e.to_string())
}

#[cfg(debug_assertions)]
fn store_delete(account: &str) -> Result<(), String> {
    let mut map = dev_read_map()?;
    map.remove(account);
    dev_write_map(&map)
}

#[cfg(not(debug_assertions))]
fn store_delete(account: &str) -> Result<(), String> {
    match keyring::Entry::new(KEYCHAIN_SERVICE, account)
        .map_err(|e| e.to_string())?
        .delete_credential()
    {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

// ----- Backing-store (de)serialisation of the two logical entries -----

// ----- Key-wrapped session store (T029: Windows credential-blob overflow fix) -----
//
// The OS keychain caps credential blobs on Windows (`CRED_MAX_CREDENTIAL_BLOB_SIZE` = 2560
// UTF-16 chars); the full session JSON (tokens + vault key + roster metadata) blows past it,
// failing sign-in. macOS Keychain has no such cap, so this only ever surfaced on Windows —
// but the fix is UNIFORM across platforms (no Windows special-case): the keychain now holds
// ONLY a random 32-byte wrapping key (base64, ~44 chars); the session JSON is sealed under
// that key (XChaCha20-Poly1305, matching `yapstack-sync::crypto` conventions — version byte
// first, random 192-bit nonce, AAD = LP(version, domain)) and written to `sync-session.enc`.
// `identity-v1` is UNCHANGED (small, and its survives-sign-out semantics from T019 must hold).

/// The keychain / dev-store value backing the wrapped session: the random 32-byte wrapping
/// key (base64) AND the `vault_key_epoch` of the session currently sealed under it. The epoch
/// is bound into the sealed file's AAD (`session_aad`) and re-checked on open. This is the
/// anti-rollback binding (T029 Judge): an attacker with FILE access but NOT keychain access
/// cannot swap in an OLDER sealed session under the same wrapping key, because that older blob
/// was sealed at a lower epoch and its AAD no longer matches the keychain-recorded epoch → the
/// AEAD open fails → clean signed-out. The guarantee holds only against a file-only attacker;
/// a full keychain compromise is out of scope (that adversary already holds the wrapping key).
#[derive(Serialize, Deserialize)]
struct WrapKeyEntry {
    /// base64 of the 32-byte XChaCha20-Poly1305 wrapping key.
    k: String,
    /// `vault_key_epoch` of the session sealed under `k` (§7.4 anti-rollback binding).
    #[serde(default)]
    epoch: u32,
}

/// AAD for the at-rest session envelope: `LP(version, SESSION_STORE_DOMAIN, epoch_u32)`. The
/// `epoch` is bound as the third field per the CRYPTO_SPEC `wrap.data` convention (§5) so a
/// session sealed at one epoch cannot be opened as another — the rollback binding. Domain is
/// `v2` (was `v1` without this field), so a `v1` blob fails to open here (clean degrade).
fn session_aad(epoch: u32) -> Vec<u8> {
    yapstack_crypto::aead::lp(&[
        &[yapstack_crypto::VERSION],
        SESSION_STORE_DOMAIN,
        &epoch.to_be_bytes(),
    ])
}

/// Seal the session JSON under the wrapping key. Standard envelope
/// (`0x01 || nonce24 || ct||tag`) via `yapstack-crypto::aead::seal_standard`, AAD =
/// `session_aad(epoch)` — same LP construction the changeset/setting surfaces use, now with
/// the session's `vault_key_epoch` bound in (anti-rollback).
fn seal_session(wrap_key: &[u8; 32], epoch: u32, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let aad = session_aad(epoch);
    yapstack_crypto::aead::seal_standard(wrap_key, &nonce, plaintext, &aad)
        .map_err(|e| e.to_string())
}

/// Open a sealed session file, binding `expected_epoch` into the AAD. Any failure (wrong key,
/// tamper, truncation, version/domain skew, OR an epoch that does not match the one recorded
/// beside the wrapping key — i.e. a rollback) is a clean `Err` the caller degrades to
/// signed-out — never a panic.
fn open_session(wrap_key: &[u8; 32], expected_epoch: u32, blob: &[u8]) -> Result<Vec<u8>, String> {
    let aad = session_aad(expected_epoch);
    yapstack_crypto::aead::open_standard(wrap_key, blob, &aad).map_err(|e| e.to_string())
}

/// Parse the wrapping-key entry (key + recorded epoch) from its stored JSON form.
fn decode_wrap_entry(s: &str) -> Result<([u8; 32], u32), String> {
    let entry: WrapKeyEntry =
        serde_json::from_str(s).map_err(|_| "corrupt session wrapping-key entry".to_string())?;
    let bytes = B64
        .decode(entry.k.as_bytes())
        .map_err(|_| "corrupt session wrapping key".to_string())?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "session wrapping key wrong length".to_string())?;
    Ok((key, entry.epoch))
}

/// Serialize the wrapping-key entry (key + the epoch of the session sealed under it).
fn encode_wrap_entry(key: &[u8; 32], epoch: u32) -> Result<String, String> {
    serde_json::to_string(&WrapKeyEntry {
        k: B64.encode(key),
        epoch,
    })
    .map_err(|e| e.to_string())
}

fn new_wrap_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    k
}

/// Mockable seam over the wrapping key + legacy session blob, so the wrap/unwrap/migrate/
/// degrade logic is exercised in `cargo test` against an in-memory fake rather than the real
/// keychain. The production impl (`KeychainSessionKeyStore`) routes to `store_get/set/delete`
/// (keychain in release, dev file in debug).
trait SessionKeyStore {
    fn get_key(&self) -> Result<Option<String>, String>;
    fn set_key(&self, b64: &str) -> Result<(), String>;
    fn delete_key(&self) -> Result<(), String>;
}

struct KeychainSessionKeyStore;
impl SessionKeyStore for KeychainSessionKeyStore {
    fn get_key(&self) -> Result<Option<String>, String> {
        store_get(KEYCHAIN_ACCOUNT_SESSION_KEY)
    }
    fn set_key(&self, b64: &str) -> Result<(), String> {
        store_set(KEYCHAIN_ACCOUNT_SESSION_KEY, b64)
    }
    fn delete_key(&self) -> Result<(), String> {
        store_delete(KEYCHAIN_ACCOUNT_SESSION_KEY)
    }
}

fn read_session_blob(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn write_session_blob(path: &Path, blob: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Write-then-rename so a crash mid-write never leaves a torn file that would decrypt-fail.
    let tmp = path.with_extension("enc.tmp");
    std::fs::write(&tmp, blob).map_err(|e| e.to_string())?;
    // Owner-only permissions on unix: the file holds the sealed session ciphertext, and even
    // ciphertext should not be world-readable (defence in depth against local snooping /
    // offline attack surface). Applied to the TEMP file too so there is never a window where
    // the pre-rename file is broader than 0600. Windows inherits the user-scoped AppData ACL,
    // so this is a unix-only no-op elsewhere.
    set_owner_only(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    // rename preserves the mode; re-assert on the final path in case it pre-existed with a
    // broader mode (e.g. an older build wrote it before this hardening landed).
    set_owner_only(path)
}

/// Restrict a file to `0600` (owner read/write only) on unix. No-op on other platforms —
/// Windows relies on the user-scoped AppData ACL and has no POSIX mode to set.
#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn remove_session_blob(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Read the session: keychain key + encrypted file → decrypt → session. Missing file OR
/// missing key OR decrypt failure → signed-out (clean degrade). Inconsistent components (a
/// stray key without a file, or vice versa) are cleaned up best-effort — steady-state
/// hygiene, then treated as signed-out.
fn load_session_wrapped(
    ks: &impl SessionKeyStore,
    enc_path: &Path,
) -> Result<Option<Session>, String> {
    match (ks.get_key()?, read_session_blob(enc_path)?) {
        (Some(entry_str), Some(blob)) => {
            // Corrupt entry or AEAD failure (wrong key / tamper / truncation / epoch rollback)
            // → signed-out. We do NOT delete here: a genuinely tampered file is unrecoverable
            // and the next sign-in overwrites it, but a transient keychain read must never
            // destroy a session file.
            let Ok((key, expected_epoch)) = decode_wrap_entry(&entry_str) else {
                return Ok(None);
            };
            match open_session(&key, expected_epoch, &blob) {
                Ok(json) => match serde_json::from_slice::<Session>(&json) {
                    // Anti-rollback: the sealed session's own epoch MUST equal the epoch
                    // recorded beside the wrapping key. The AAD binding already enforces this
                    // cryptographically (we opened under `expected_epoch`; an older blob sealed
                    // at a lower epoch fails the AEAD tag), so reaching here with a mismatch is
                    // impossible for a file-only attacker — the explicit check is defence in
                    // depth and pins the invariant.
                    Ok(s) if s.epoch == expected_epoch => Ok(Some(s)),
                    Ok(_) | Err(_) => Ok(None),
                },
                Err(_) => Ok(None),
            }
        }
        // Stray wrapping key with no file: clean it up, then signed-out.
        (Some(_), None) => {
            let _ = ks.delete_key();
            Ok(None)
        }
        // Orphan file with no key: it can never be decrypted — remove it, then signed-out.
        (None, Some(_)) => {
            let _ = remove_session_blob(enc_path);
            Ok(None)
        }
        (None, None) => Ok(None),
    }
}

/// Persist the session under the wrapping scheme: load-or-create the 32-byte wrapping key
/// (generated on first persist), seal the JSON, and write the file.
fn store_session_wrapped(
    ks: &impl SessionKeyStore,
    enc_path: &Path,
    s: &Session,
) -> Result<(), String> {
    let json = serde_json::to_string(s).map_err(|e| e.to_string())?;
    // Reuse the existing wrapping key if present and valid; otherwise mint a fresh one. (A
    // corrupt existing entry would strand the new ciphertext, so we replace it.) The RECORDED
    // epoch is always overwritten below with THIS session's epoch.
    let key = match ks.get_key()? {
        Some(entry_str) => decode_wrap_entry(&entry_str)
            .map(|(k, _prev_epoch)| k)
            .unwrap_or_else(|_| new_wrap_key()),
        None => new_wrap_key(),
    };
    // Seal + write the file FIRST, then commit the wrapping-key entry (key + this session's
    // epoch). The entry is the load-time commit point: if a crash lands between the two, the
    // entry's epoch will not match the (absent or older) file's AAD, so the next load degrades
    // cleanly to signed-out rather than opening a stale session.
    let blob = seal_session(&key, s.epoch, json.as_bytes())?;
    write_session_blob(enc_path, &blob)?;
    ks.set_key(&encode_wrap_entry(&key, s.epoch)?)?;
    Ok(())
}

/// Sign-out cleanup for the wrapped store: delete the file and the wrapping key. Best-effort
/// (a keychain/file hiccup must never block sign-out). `identity-v1` is untouched here — its
/// preservation is handled by the caller (`clear_session`, T019).
fn clear_session_wrapped(ks: &impl SessionKeyStore, enc_path: &Path) -> Result<(), String> {
    let _ = remove_session_blob(enc_path);
    let _ = ks.delete_key();
    Ok(())
}

// DEBUG keeps the plaintext dev-file store UNCHANGED: the session is one JSON value under
// `session-v1` in `sync-dev-creds.json`. There is no Windows credential-blob limit on a local
// dev file, and the dev store is plaintext-by-design (developer's own machine, T020). The
// key-wrapping scheme is exercised in debug via the unit tests' in-memory `SessionKeyStore`
// fake, not the dev file, so the production RELEASE path is fully covered without perturbing
// the dev store. RELEASE routes the session through the key-wrapped file (Windows-safe).

#[cfg(debug_assertions)]
fn load_session_from_store() -> Result<Option<Session>, String> {
    match store_get(KEYCHAIN_ACCOUNT)? {
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

#[cfg(not(debug_assertions))]
fn load_session_from_store() -> Result<Option<Session>, String> {
    load_session_wrapped(&KeychainSessionKeyStore, &session_enc_path())
}

#[cfg(debug_assertions)]
fn store_session_to_store(s: &Session) -> Result<(), String> {
    let json = serde_json::to_string(s).map_err(|e| e.to_string())?;
    store_set(KEYCHAIN_ACCOUNT, &json)
}

#[cfg(not(debug_assertions))]
fn store_session_to_store(s: &Session) -> Result<(), String> {
    store_session_wrapped(&KeychainSessionKeyStore, &session_enc_path(), s)
}

#[cfg(debug_assertions)]
fn clear_session_from_store() -> Result<(), String> {
    store_delete(KEYCHAIN_ACCOUNT)
}

#[cfg(not(debug_assertions))]
fn clear_session_from_store() -> Result<(), String> {
    clear_session_wrapped(&KeychainSessionKeyStore, &session_enc_path())
}

fn load_identity_from_store() -> Result<Option<DeviceIdentity>, String> {
    match store_get(KEYCHAIN_ACCOUNT_IDENTITY)? {
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

fn store_identity_to_store(id: &DeviceIdentity) -> Result<(), String> {
    let json = serde_json::to_string(id).map_err(|e| e.to_string())?;
    store_set(KEYCHAIN_ACCOUNT_IDENTITY, &json)
}

// ----- In-memory credential cache (T020) -----
//
// The backing store is read ONCE (at boot, and re-populated on each sign-in) into this
// process-wide cache; every subsequent READ (the polled `sync_status`, the drain's
// `client_id`, roster building) is served from here, NOT from the store. WRITES update the
// store AND this cache in lock-step, so a freshly signed-in / signed-out / enabled state is
// reflected immediately with no staleness. Net effect: the keychain is touched ~once per
// launch / sign-in rather than ~once per status poll (the macOS prompt SPAM this fixes).
//
// A module-global (mirroring `pending_login_cell`) so the boot hook and the free identity
// helpers reach it without threading `State` through every command; the same handle is also
// registered as Tauri managed state in lib.rs setup.

#[derive(Default)]
pub struct CredCacheInner {
    loaded: bool,
    session: Option<Session>,
    identity: Option<DeviceIdentity>,
}

/// Process-wide credential cache handle (also registered as Tauri managed state).
pub type SyncCredCache = Arc<RwLock<CredCacheInner>>;

fn cred_cache() -> &'static SyncCredCache {
    static CACHE: OnceLock<SyncCredCache> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(RwLock::new(CredCacheInner::default())))
}

/// A clone of the shared cache handle, for registration as Tauri managed state in lib.rs.
pub fn cred_cache_handle() -> SyncCredCache {
    cred_cache().clone()
}

/// Populate the cache from the backing store ONCE. Subsequent calls are a cheap flag check
/// and never touch the store (hence never the keychain). Safe to call from any thread.
fn ensure_cache_loaded() {
    if cred_cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .loaded
    {
        return;
    }
    let session = load_session_from_store().ok().flatten();
    let identity = load_identity_from_store().ok().flatten();
    let mut g = cred_cache().write().unwrap_or_else(|e| e.into_inner());
    if !g.loaded {
        g.session = session;
        g.identity = identity;
        g.loaded = true;
    }
}

/// Boot hook (lib.rs setup): point the DEBUG file store at the app config dir and warm the
/// cache once, so the first `sync_status` poll never blocks on the store. In release the
/// path arg is unused (the keychain needs no directory).
pub fn init_credential_store(config_dir: &Path) {
    // The key-wrapped session file (T029) lives here on every platform.
    let _ = SESSION_STORE_DIR.set(config_dir.to_path_buf());
    #[cfg(debug_assertions)]
    {
        let _ = DEV_STORE_DIR.set(config_dir.to_path_buf());
    }
    ensure_cache_loaded();
}

/// Signed-in session, served from the in-memory cache (loaded from the store on first use).
/// Reads NEVER hit the keychain after the initial warm-up (T020).
fn load_session() -> Result<Option<Session>, String> {
    ensure_cache_loaded();
    Ok(cred_cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .session
        .clone())
}

/// Persist the session to the backing store AND refresh the cache in lock-step.
fn store_session(s: &Session) -> Result<(), String> {
    store_session_to_store(s)?;
    let mut g = cred_cache().write().unwrap_or_else(|e| e.into_inner());
    g.session = Some(s.clone());
    g.loaded = true;
    Ok(())
}

/// Device identity, served from the cache (loaded from the store on first use).
fn load_identity() -> Result<Option<DeviceIdentity>, String> {
    ensure_cache_loaded();
    Ok(cred_cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .identity
        .clone())
}

/// Persist the identity to the backing store AND refresh the cache in lock-step.
fn store_identity(id: &DeviceIdentity) -> Result<(), String> {
    store_identity_to_store(id)?;
    let mut g = cred_cache().write().unwrap_or_else(|e| e.into_inner());
    g.identity = Some(id.clone());
    g.loaded = true;
    Ok(())
}

#[cfg(test)]
fn reset_cache_for_test() {
    *cred_cache().write().unwrap_or_else(|e| e.into_inner()) = CredCacheInner::default();
}

/// Sign-out: delete ONLY the session entry (store + cache). The persistent device identity
/// (§7.1) is preserved in its own `identity-v1` entry so a subsequent sign-in on the SAME
/// install presents the SAME `client_id` and is recognised as the existing device, NOT
/// re-enrolled as a phantom PENDING one (§7.5). The cache's session is cleared in lock-step
/// so status → disconnected.
fn clear_session() -> Result<(), String> {
    clear_session_from_store()?;
    // A signed-out state has no drain; clear any lingering auth-expired/blocked health and
    // push-progress so a subsequent sign-in does not inherit a stale status or backlog.
    set_drain_health(DrainHealth::Ok);
    reset_drain_progress();
    let mut g = cred_cache().write().unwrap_or_else(|e| e.into_inner());
    g.session = None;
    g.loaded = true;
    Ok(())
}

// ----- Wire DTOs (camelCase to match lib/sync.ts) -----

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
    /// T024 push progress. Unacked outbox entries still to push (0 == up to date).
    pending_entries: u64,
    /// Total ciphertext bytes of those unacked entries (base64 upload is ~4/3 of this).
    pending_bytes: u64,
    /// Entries acked since the current drain thread started (cumulative this session).
    acked_this_session: u64,
    /// RFC3339 of the last time the outbox fully drained with the relay reachable;
    /// null before the first successful drain. The panel renders it relative to now.
    last_success: Option<String>,
}

// ----- Typed relay connection probe (T025) -----
//
// `sync_probe` returns a TYPED result the redesigned Sync page branches on: reachability /
// TLS / not-a-relay are distinct error classes, and a version gap is advisory metadata on
// SUCCESS (never a failure). It is the ONLY relay-metadata call on the client — `billing_url`
// (self-host vs hosted upgrade affordance) rides on `sync_status` (`SyncStatusDto.billing_url`).

/// Sentinel body for the probe. A 2xx counts as a YapStack relay ONLY if the body
/// deserializes here with BOTH `protocol_version` and `engine_version` present
/// (crates/yapstack-server/src/routes.rs `SyncInfoResponse` — note there is no
/// `server_version` field). serde treats a missing required field as a parse error, so
/// the caller maps a bare proxy 200 or a `{"res":"pong"}` health stub to `NotARelay`.
#[derive(Deserialize)]
struct RelayProbeBody {
    protocol_version: u32,
    engine_version: String,
    #[serde(default)]
    min_client_version: Option<String>,
}

/// `sync_probe` success payload (mirrors `RelayProbeOk` in `lib/sync.ts`).
#[derive(Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RelayProbeOk {
    engine_version: String,
    protocol_version: u32,
    /// Elapsed from request start to response head, milliseconds.
    latency_ms: u64,
    /// The URL actually probed after normalization, so the UI can echo/persist it.
    normalized_url: String,
    /// Populated ONLY when this client is older than the relay's published minimum.
    /// Advisory — the probe still succeeds ("update this app", never blocking, §0.3).
    version_advisory: Option<VersionAdvisory>,
}

/// Advisory that this client is behind the relay's published minimum (mirrors
/// `RelayVersionAdvisory` in `lib/sync.ts`). Rides on probe SUCCESS, never a failure.
#[derive(Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct VersionAdvisory {
    /// Minimum client version the relay publishes.
    min_client_version: String,
    /// Verbatim human-readable advisory line.
    raw: String,
}

/// Typed probe failure classes (mirrors `RelayProbeError` in `lib/sync.ts`). Serialized
/// tagged on `kind` (kebab-case) so TS can discriminate. Every variant carries the
/// verbatim `raw` detail — errors are surfaced to the user, never swallowed.
#[derive(Debug, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RelayProbeError {
    /// DNS failure / connection refused / timeout (5s budget), or TLS that could not be
    /// distinguished from a plain connect failure.
    Unreachable { raw: String },
    /// TLS certificate or handshake failure.
    TlsError { raw: String },
    /// An HTTP response arrived but this is not a YapStack relay: non-2xx status, or a 2xx
    /// whose body is missing the `protocol_version` + `engine_version` sentinel.
    NotARelay { raw: String },
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

// ----- Drain health (Bug A/B status surfacing) -----
//
// The drain runs on its own thread; `sync_status` runs on the command thread. This
// process-global cell is the one-way channel between them: the drain writes its latest
// health, and `build_status_dto` reads it to surface an auth-expired phase or a
// distinct oversized/blocked `last_error` — the "existing sync status mechanism" the
// frontend already renders (phase string + last_error). Never carries token material.

/// The drain's latest self-reported health.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum DrainHealth {
    /// Cycling normally (or not started).
    #[default]
    Ok,
    /// The relay rejected the access token AND a refresh was impossible (no refresh
    /// token) or also rejected — the owner must sign in again. The drain has STOPPED
    /// (it never hot-loops on a dead token).
    AuthExpired,
    /// A queued entry cannot be pushed (e.g. an unrepairable oversized poison entry).
    /// Carries a human message for `last_error`. The drain keeps pulling but this entry
    /// stays put — surfaced ONCE, never a 5s 413 hot-loop.
    Blocked(String),
    /// [`DRAIN_FAIL_SURFACE_THRESHOLD`] CONSECUTIVE drain cycles hit a (non-fatal) transport
    /// error in push OR pull (F2). Surfaces the VERBATIM latest error as `last_error` so the
    /// frontend's `deriveSyncDisplay` renders the distinct "Sync error" state — before F2 the
    /// owner watched "syncing" with nothing moving and zero feedback. The drain keeps
    /// retrying (not fatal); a single transient blip stays quiet, and a later clean cycle
    /// clears it.
    Failing(String),
    /// Like [`DrainHealth::Failing`] but the surfaced error is a transport-layer CONNECTIVITY
    /// failure ([`yapstack_sync::SyncError::Network`] — the relay is unreachable), classified
    /// at the transport (never by string-matching). Surfaced as the distinct `unreachable`
    /// phase so the UI shows the amber "Can't reach relay" state instead of the destructive
    /// red "Sync error" (R3, closes the TODO(T02x) pair). The drain keeps retrying; a later
    /// clean cycle clears it.
    Unreachable(String),
}

fn drain_health_cell() -> &'static RwLock<DrainHealth> {
    static CELL: OnceLock<RwLock<DrainHealth>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(DrainHealth::Ok))
}

/// Set the drain health, returning true if it CHANGED (so the caller logs only on a
/// transition, never every 5s cycle).
fn set_drain_health(next: DrainHealth) -> bool {
    let mut g = drain_health_cell()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if *g != next {
        *g = next;
        true
    } else {
        false
    }
}

fn drain_health() -> DrainHealth {
    drain_health_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// What the F2 threshold step decided this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailSurface {
    /// A failure, but not enough consecutive ones yet — stay quiet (a single blip).
    Quiet,
    /// The consecutive-failure run reached [`DRAIN_FAIL_SURFACE_THRESHOLD`] — surface the
    /// verbatim error as a distinct failing state.
    Surface,
    /// A clean cycle — reset the run and clear any prior failing state.
    Clear,
}

/// Fold one cycle's success/failure into the running consecutive-failure count and decide
/// whether to surface a failing state, stay quiet, or clear (F2). Pure — extracted so the
/// "1 blip stays quiet, 2-in-a-row surfaces, a later success clears" contract is unit-tested
/// without spinning up the drain thread. `cycle_failed` is true for any non-fatal transport
/// error or a cycle-fatal local fault; the sticky Oversized state manages its own reset.
fn fail_surface_step(consecutive_errors: &mut u32, cycle_failed: bool) -> FailSurface {
    if cycle_failed {
        *consecutive_errors = consecutive_errors.saturating_add(1);
        if *consecutive_errors >= DRAIN_FAIL_SURFACE_THRESHOLD {
            FailSurface::Surface
        } else {
            FailSurface::Quiet
        }
    } else {
        *consecutive_errors = 0;
        FailSurface::Clear
    }
}

// ----- Drain progress (T024 push-progress surfacing) -----
//
// A second one-way channel from the drain thread to the polled `sync_status`, alongside
// `DrainHealth`. Where health carries the exceptional states (auth-expired / blocked),
// this carries the NORMAL push progress the owner had no way to see: how many entries /
// bytes are still queued, how many have been acked this drain session, whether a push is
// in flight, and when the outbox was last fully drained. Read straight from the cheap
// `outbox::pending` view each cycle — no counter to keep in step with the table. Carries
// only counts/bytes/timestamps: never token, key, or plaintext material.

/// The drain's latest self-reported push progress.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DrainProgress {
    /// Unacked entries still to push as of the last cycle (0 == up to date).
    pending_entries: u64,
    /// Total ciphertext bytes of those unacked entries.
    pending_bytes: u64,
    /// Entries acked since THIS drain thread started (cumulative across cycles). Resets
    /// when a fresh drain spawns (re-login / enable), so it reads as "this session".
    acked_this_session: u64,
    /// True while a backlog remains (`pending_entries > 0`) — drives the `syncing` phase.
    syncing: bool,
    /// RFC3339 of the last cycle that completed with the outbox fully drained AND the
    /// relay reachable — the "last synced" time the panel shows relative to now.
    last_success: Option<String>,
}

fn drain_progress_cell() -> &'static RwLock<DrainProgress> {
    static CELL: OnceLock<RwLock<DrainProgress>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(DrainProgress::default()))
}

fn drain_progress() -> DrainProgress {
    drain_progress_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Overwrite the whole progress snapshot (the drain rebuilds it each cycle).
fn set_drain_progress(next: DrainProgress) {
    *drain_progress_cell()
        .write()
        .unwrap_or_else(|e| e.into_inner()) = next;
}

/// Clear progress back to "nothing pending, never synced" — used when a drain stops or
/// the session is cleared so a subsequent poll never shows a stale backlog.
fn reset_drain_progress() {
    set_drain_progress(DrainProgress::default());
}

/// The `exp` (unix seconds) claim of a JWT, decoded WITHOUT verifying the signature.
/// This is only used to schedule a proactive refresh (A5) — a scheduling hint, not a
/// trust decision — so an unverified read of the public claim is acceptable client-side.
/// Returns `None` if the token is not a well-formed JWT with a numeric `exp`.
fn jwt_exp_unverified(token: &str) -> Option<i64> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp")?.as_i64()
}

/// True if the access token is expired or within `skew` seconds of expiry (so the drain
/// should refresh proactively rather than eat a first-cycle 401).
fn access_token_stale(token: &str, skew_secs: i64) -> bool {
    match jwt_exp_unverified(token) {
        Some(exp) => chrono::Utc::now().timestamp() + skew_secs >= exp,
        None => false, // unknown shape → let the 401 path handle it.
    }
}

/// Why a token refresh failed — the distinction that decides whether the drain SIGNS OUT or
/// merely RETRIES. Conflating these is the R3 HIGH bug: a spent refresh token (crash-window
/// lockout) must sign out, but a momentary relay outage must NOT.
#[derive(Debug)]
enum RefreshFailure {
    /// The relay REJECTED the refresh token (HTTP 401) — or there is no refresh token to
    /// present. This is TERMINAL for the session: rotation already spent the old token, and
    /// the relay's reuse detection (server `auth.rs` `refresh()`: a rotated/revoked token
    /// re-presented → the WHOLE token family is revoked → 401) means no retry can ever
    /// recover it. Only a fresh sign-in does. Carries no token material.
    Rejected(String),
    /// The relay was UNREACHABLE (reqwest `is_connect()` / `is_timeout()`). The refresh token
    /// is STILL VALID — this is a connectivity blip, not auth expiry. Retry next cycle; NEVER
    /// sign out. Surfaced as the amber "can't reach relay" state.
    Network(String),
    /// Any other transient failure (5xx, body decode, a local store error). The refresh token
    /// is presumed still valid; retry next cycle. Surfaced as the "sync error" state.
    Transient(String),
}

/// Attempt a SINGLE token refresh (Bug A) against `POST /auth/refresh` using the
/// persisted refresh token, and persist the ROTATED pair (new access + new refresh) to
/// the store AND the in-memory cache BEFORE returning the new access token. Rotation
/// kills the old refresh token on use, so losing the new one would lock the account out
/// — hence persist-before-use. On failure returns a typed [`RefreshFailure`] so the caller
/// distinguishes a REJECTED token (sign out) from an UNREACHABLE relay (retry). NEVER logs
/// any token.
async fn refresh_access_token() -> Result<String, RefreshFailure> {
    let mut session = load_session()
        .map_err(RefreshFailure::Transient)?
        .ok_or_else(|| RefreshFailure::Rejected("not signed in".to_string()))?;
    let refresh_token = session.refresh_token.clone().ok_or_else(|| {
        RefreshFailure::Rejected("no refresh token on this session — sign in again".to_string())
    })?;
    let url = format!("{}/auth/refresh", base_url(&session.server_url));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&RefreshRequest { refresh_token })
        .send()
        .await
        // A connect/timeout failure means the relay is unreachable — NOT that the token is
        // bad; the classification mirrors the T025 probe / transport layer (no string parse).
        .map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                RefreshFailure::Network(e.to_string())
            } else {
                RefreshFailure::Transient(e.to_string())
            }
        })?;
    match classify_refresh_status(resp.status()) {
        // Clean status — decode the rotated pair and persist it before handing back the access
        // token (persist-before-use: rotation already spent the presented refresh token).
        None => {
            let tokens: TokenResponse = resp
                .json()
                .await
                .map_err(|e| RefreshFailure::Transient(e.to_string()))?;
            session.bearer = tokens.access_token.clone();
            session.refresh_token = Some(tokens.refresh_token);
            store_session(&session).map_err(RefreshFailure::Transient)?;
            Ok(tokens.access_token)
        }
        Some(fail) => Err(fail),
    }
}

/// Classify a refresh response STATUS into the terminal-vs-transient decision (pure, so the
/// "401 ⇒ sign out, 5xx ⇒ retry" rule is unit-tested without a live relay). `None` means the
/// status is a success the caller should decode. A 401 is the relay rejecting the refresh
/// token (reuse-detection family revocation on a spent token) → [`RefreshFailure::Rejected`];
/// any other non-2xx is a server-side transient → [`RefreshFailure::Transient`].
fn classify_refresh_status(status: reqwest::StatusCode) -> Option<RefreshFailure> {
    if status.is_success() {
        None
    } else if status == reqwest::StatusCode::UNAUTHORIZED {
        Some(RefreshFailure::Rejected(
            "relay rejected the refresh token (401)".to_string(),
        ))
    } else {
        Some(RefreshFailure::Transient(format!(
            "relay error {}",
            status.as_u16()
        )))
    }
}

/// The `DrainHealth` a failed refresh maps to WITHOUT signing out — i.e. for the retryable
/// [`RefreshFailure::Network`] / [`RefreshFailure::Transient`] cases. Returns `None` for a
/// [`RefreshFailure::Rejected`], which is terminal and handled by `expire_session_terminally`
/// instead. Pure — makes the "network ⇒ unreachable, transient ⇒ failing, rejected ⇒ sign
/// out" decision unit-testable.
fn retryable_refresh_health(f: &RefreshFailure) -> Option<DrainHealth> {
    match f {
        RefreshFailure::Rejected(_) => None,
        RefreshFailure::Network(m) => Some(DrainHealth::Unreachable(m.clone())),
        RefreshFailure::Transient(m) => Some(DrainHealth::Failing(m.clone())),
    }
}

/// Strip the spent credentials (access + refresh tokens) from a session while KEEPING
/// everything else — email, server URL, vault handle, `sync_enabled`, device identity. Pure:
/// the caller persists the result. This is what makes a terminal refresh failure recoverable
/// WITHOUT a full sign-out: the session record survives so `sync_status` still surfaces the
/// AuthExpired ("sign in again") state, but the already-spent refresh token is gone, so the
/// next boot presents NO refresh token (an immediate local error) instead of re-presenting the
/// rotated one to the relay's reuse detector.
fn invalidate_session_credentials(mut s: Session) -> Session {
    s.bearer = String::new();
    s.refresh_token = None;
    s
}

/// React to a TERMINAL refresh failure (the relay rejected the refresh token, or none exists):
/// wipe the spent credentials from the persisted session (keeping the session record + device
/// identity, T019) and raise [`DrainHealth::AuthExpired`] so the UI lands on the "Session
/// expired — sign in again" surface (T023/R2) — never a retry loop, never a generic error.
///
/// We deliberately DO NOT call `clear_session` (which would drop the whole session and degrade
/// the UI to the neutral "disconnected/off" state, NOT the AuthExpired surface the requirement
/// names): instead we keep the record and null only the tokens. Order matters — the store
/// write happens first, then `set_drain_health(AuthExpired)`. Returns whether AuthExpired newly
/// transitioned (for one-shot logging). NEVER logs tokens.
fn expire_session_terminally() -> bool {
    match load_session() {
        Ok(Some(s)) => {
            let stripped = invalidate_session_credentials(s);
            if let Err(e) = store_session(&stripped) {
                tracing::warn!("sync drain: could not persist expired-session state: {e}");
            }
        }
        Ok(None) => {} // already signed out — nothing to strip.
        Err(e) => tracing::warn!("sync drain: reading session during expiry failed: {e}"),
    }
    set_drain_health(DrainHealth::AuthExpired)
}

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
        session.client_id,
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
    // This install's persistent §7.1 client_id (from the identity keychain entry, via the
    // session). Passed in — rather than minted per-DB by `state::client_id` — so the device
    // that AUTHORS changesets is the SAME device listed in the signed roster (§7.5).
    client_id: Uuid,
) -> Result<DrainHandle, String> {
    // A fresh drain starts from a clean health slate (clears any lingering auth-expired
    // / blocked state from a previously stopped drain, e.g. after a re-login) and a clean
    // progress slate (session ack count resets; no stale backlog shown, T024).
    set_drain_health(DrainHealth::Ok);
    reset_drain_progress();
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
            let cipher = ChangesetCipher::new(
                vault_key,
                epoch,
                tenant_id,
                SYNC_SCHEMA_VERSION,
                CRSQLITE_ENGINE_VERSION,
            );
            let sv = SYNC_SCHEMA_VERSION as i32;
            let ev = CRSQLITE_ENGINE_VERSION as i32;

            // A5: if the persisted access token is already expired/near-expiry, refresh
            // proactively so the first cycle doesn't have to eat a 401 first. Staleness is
            // only a scheduling hint (unverified `exp`).
            let stale = access_token_stale(&bearer, 60);
            let transport = HttpTransport::new(server_url, bearer);
            if stale {
                match rt.block_on(refresh_access_token()) {
                    Ok(new_access) => transport.set_bearer(&new_access),
                    // R3 (HIGH — crash-window lockout): a stale access token we cannot refresh
                    // at BOOT because the relay REJECTED the refresh token is terminal. This is
                    // the exact crash-window case — the persisted refresh token was already
                    // rotated before we could save its replacement, so the relay's reuse
                    // detection revoked the family. Land on the AuthExpired surface and stop;
                    // do NOT enter the loop just to hot-eat 401s against a dead token.
                    Err(RefreshFailure::Rejected(e)) => {
                        if expire_session_terminally() {
                            tracing::warn!(
                                "sync drain: refresh token rejected at start — session expired; sign in again ({e})"
                            );
                        }
                        return;
                    }
                    // Unreachable / transient: the refresh token is still valid, so keep the
                    // (stale) bearer and let the in-loop 401 path retry once connectivity is
                    // back. A momentary outage must never sign the owner out.
                    Err(RefreshFailure::Network(e)) | Err(RefreshFailure::Transient(e)) => {
                        tracing::warn!("sync drain: proactive token refresh deferred (transient): {e}")
                    }
                }
            }

            // T024: measure the pre-existing backlog ONCE so a big initial sync announces
            // itself in the log and shows "syncing" in the UI immediately. `had_backlog`
            // gates the one-shot "up to date" transition log; `acked_session` is the
            // cumulative this-session ack count surfaced in the status payload.
            let mut had_backlog = match outbox::pending(conn) {
                Ok(p) => {
                    if p.entries > 0 {
                        tracing::info!(
                            "sync: pushing {} pending {} ({:.1} MiB)",
                            p.entries,
                            if p.entries == 1 { "entry" } else { "entries" },
                            p.bytes as f64 / (1024.0 * 1024.0)
                        );
                    }
                    set_drain_progress(DrainProgress {
                        pending_entries: p.entries,
                        pending_bytes: p.bytes,
                        acked_this_session: 0,
                        syncing: p.entries > 0,
                        last_success: None,
                    });
                    p.entries > 0
                }
                Err(e) => {
                    tracing::warn!("sync: initial backlog read failed: {e}");
                    false
                }
            };
            let mut acked_session: u64 = 0;
            // F2: consecutive cycles that hit a (non-fatal) transport error in either
            // direction. Flips the panel to a distinct failing state past the threshold; any
            // clean cycle (or a sticky Oversized) resets it.
            let mut consecutive_errors: u32 = 0;

            while !stop.load(Ordering::SeqCst) {
                // A cycle now ALWAYS attempts BOTH push and pull (F1); a per-direction
                // transport error rides on the report rather than aborting the cycle. Only a
                // cycle-fatal LOCAL (sqlite/replay) fault returns `Err` — never crash the
                // thread on it; count it toward the failing threshold and retry next cycle.
                let mut report =
                    match rt.block_on(outbox::drain_once(conn, &cipher, &transport, client_id, sv, ev)) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = e.to_string();
                            if fail_surface_step(&mut consecutive_errors, true) == FailSurface::Surface
                                && set_drain_health(DrainHealth::Failing(msg.clone()))
                            {
                                tracing::warn!("sync drain cycle failed: {msg}");
                            }
                            std::thread::sleep(DRAIN_INTERVAL);
                            continue;
                        }
                    };

                // Bug A (T023), now covering BOTH directions (F1): a 401 in push OR pull →
                // refresh the access token once and retry the whole cycle.
                if report.has_unauthorized() {
                    match rt.block_on(refresh_access_token()) {
                        Ok(new_access) => {
                            transport.set_bearer(&new_access);
                            report = match rt.block_on(outbox::drain_once(
                                conn, &cipher, &transport, client_id, sv, ev,
                            )) {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::warn!("sync drain cycle failed after refresh: {e}");
                                    std::thread::sleep(DRAIN_INTERVAL);
                                    continue;
                                }
                            };
                        }
                        // R3 (HIGH): the relay REJECTED the refresh token (401 — reuse
                        // detection revoked the family after the crash-rotation window), or
                        // there is none. Terminal: strip the spent credentials + raise
                        // AuthExpired ("sign in again"), then stop. NEVER hot-loop a dead token,
                        // and never re-present the spent token on the next boot.
                        Err(RefreshFailure::Rejected(e)) => {
                            if expire_session_terminally() {
                                tracing::warn!(
                                    "sync drain: refresh token rejected — session expired; stopping drain (sign in again) ({e})"
                                );
                            }
                            break;
                        }
                        // R3: couldn't refresh RIGHT NOW (relay unreachable / 5xx), but the
                        // refresh token is still valid — a connectivity blip, not auth expiry.
                        // Surface it (unreachable vs failing) and retry next cycle; do NOT sign
                        // out. `retryable_refresh_health` never returns None here (Rejected is
                        // handled above).
                        Err(fail) => {
                            if let Some(health) = retryable_refresh_health(&fail) {
                                if fail_surface_step(&mut consecutive_errors, true)
                                    == FailSurface::Surface
                                    && set_drain_health(health)
                                {
                                    tracing::warn!(
                                        "sync drain: token refresh deferred — {fail:?}"
                                    );
                                }
                            }
                            std::thread::sleep(DRAIN_INTERVAL);
                            continue;
                        }
                    }
                }
                // Refresh succeeded but a direction STILL 401'd → the token is truly dead.
                // Terminal, same treatment as a rejected refresh.
                if report.has_unauthorized() {
                    if expire_session_terminally() {
                        tracing::warn!(
                            "sync drain: refreshed token still rejected — session expired; stopping drain"
                        );
                    }
                    break;
                }

                if report.applied + report.replayed > 0 {
                    // Reinstate the stripped FK cascade + UNIQUE invariants deterministically
                    // after a merge (R4/R5).
                    if let Err(e) = cascade::cascade_gc(conn) {
                        tracing::warn!("sync drain: cascade_gc: {e}");
                    }
                    if let Err(e) = uniqueness::enforce_uniqueness(conn) {
                        tracing::warn!("sync drain: enforce_uniqueness: {e}");
                    }
                }

                // F2: surface the tolerated push/pull transport errors via the existing
                // DrainHealth channel — before this the owner had ZERO feedback while a push or
                // pull silently failed every cycle.
                match report.first_transport_error() {
                    // Bug B4: a guaranteed-413 entry too large to push as a single request. Its
                    // own sticky Blocked state (not a transient flap), surfaced ONCE — the push
                    // guard already blocked the HTTP call, so no 5s hot-loop.
                    Some(yapstack_sync::SyncError::Oversized { client_seq, size }) => {
                        consecutive_errors = 0;
                        let msg = format!(
                            "A queued change (#{client_seq}, ~{} MiB on the wire) is too large to sync and was held back.",
                            size / (1024 * 1024)
                        );
                        if set_drain_health(DrainHealth::Blocked(msg.clone())) {
                            tracing::warn!("sync drain: {msg}");
                        }
                    }
                    // Any other transient relay error (push OR pull): count it, and after a
                    // short run flip the panel to a distinct state carrying the VERBATIM error.
                    // R3: a transport-layer CONNECTIVITY failure (typed
                    // `SyncError::Network`, classified in the transport — never by string
                    // match) becomes the amber "unreachable" state; every other relay error
                    // stays the destructive "failing" state.
                    Some(e) => {
                        let msg = e.to_string();
                        let health = if e.is_network() {
                            DrainHealth::Unreachable(msg.clone())
                        } else {
                            DrainHealth::Failing(msg.clone())
                        };
                        if fail_surface_step(&mut consecutive_errors, true) == FailSurface::Surface
                            && set_drain_health(health)
                        {
                            tracing::warn!(
                                "sync drain: relay error on {consecutive_errors} consecutive cycles: {msg}"
                            );
                        }
                    }
                    // A fully clean cycle — clear the run and any prior failing/blocked state.
                    None => {
                        fail_surface_step(&mut consecutive_errors, false);
                        set_drain_health(DrainHealth::Ok);
                    }
                }

                // T024 progress: recompute the backlog from the outbox after this cycle's
                // push, accumulate the session ack count, and log ONLY on real progress /
                // transitions — never every idle 5s cycle.
                acked_session += report.pushed as u64;
                match outbox::pending(conn) {
                    Ok(p) => {
                        if report.pushed > 0 {
                            tracing::info!(
                                "sync: pushed {} {}, {} remaining",
                                report.pushed,
                                if report.pushed == 1 { "entry" } else { "entries" },
                                p.entries
                            );
                            had_backlog = true;
                        } else if !had_backlog && p.entries > 0 {
                            // Fresh local writes appeared this cycle but could not be pushed
                            // yet — announce the new backlog once.
                            tracing::info!(
                                "sync: pushing {} pending {} ({:.1} MiB)",
                                p.entries,
                                if p.entries == 1 { "entry" } else { "entries" },
                                p.bytes as f64 / (1024.0 * 1024.0)
                            );
                            had_backlog = true;
                        }
                        // "Up to date" only counts when the outbox is empty AND no transport
                        // error occurred this cycle (else we'd claim success mid-failure).
                        let clean = report.first_transport_error().is_none();
                        let last_success = if p.entries == 0 && clean {
                            if had_backlog {
                                tracing::info!("sync: up to date");
                                had_backlog = false;
                            }
                            Some(chrono::Utc::now().to_rfc3339())
                        } else {
                            drain_progress().last_success
                        };
                        set_drain_progress(DrainProgress {
                            pending_entries: p.entries,
                            pending_bytes: p.bytes,
                            acked_this_session: acked_session,
                            syncing: p.entries > 0,
                            last_success,
                        });
                    }
                    Err(e) => tracing::warn!("sync: backlog read failed: {e}"),
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

/// The SINGLE SOURCE OF TRUTH for this install's device identity (§7.1) — used by signup,
/// login, recovery, the roster, and the drain. Returns `(ed25519_seed, client_id)`.
///
/// Resolution order:
///  1. the persistent IDENTITY keychain entry, if present (survives sign-out);
///  2. MIGRATION — an existing session entry's identity (a pre-upgrade install whose id
///     lived only in the session), promoted into and persisted as the identity entry;
///  3. a freshly minted `client_id` + Ed25519 seed, PERSISTED to the identity entry before
///     returning so it is stable from now on.
///
/// Because the identity survives sign-out, a re-login on the SAME install presents the SAME
/// `client_id` and is recognised as the existing device — NOT re-enrolled as PENDING. A
/// genuinely NEW install has neither entry, so it mints a fresh id and DOES enrol as a
/// PENDING device (§7.5) — the approval flow for a new machine (e.g. a Windows join) is
/// unaffected.
fn load_or_create_device_identity() -> Result<([u8; 32], Uuid), String> {
    // 1. Authoritative identity entry.
    if let Some(id) = load_identity()? {
        if let Ok(seed) = id.seed() {
            return Ok((seed, id.client_id));
        }
        // Corrupt identity entry — fall through and re-mint below.
    }
    // 2. Migrate a pre-upgrade session-only identity into the identity entry.
    if let Some(s) = load_session()? {
        if !s.device_sk_b64.is_empty() {
            let id = DeviceIdentity {
                client_id: s.client_id,
                device_sk_b64: s.device_sk_b64,
            };
            if let Ok(seed) = id.seed() {
                store_identity(&id)?;
                return Ok((seed, id.client_id));
            }
        }
    }
    // 3. Fresh install: mint + persist so the id is stable across sign-out from now on.
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let id = DeviceIdentity {
        client_id: Uuid::new_v4(),
        device_sk_b64: B64.encode(seed),
    };
    store_identity(&id)?;
    Ok((seed, id.client_id))
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
    // Fold in the drain's self-reported health (Bug A/B): an expired session becomes a
    // distinct `auth_expired` phase with an actionable message; a held-back oversized
    // entry surfaces as `last_error` without changing the connected phase.
    let connected_phase = if session.sync_enabled {
        "connected".to_string()
    } else {
        "connecting".to_string()
    };
    // T024: read the drain's push-progress snapshot and, when the drain is healthy and a
    // backlog remains, surface a distinct `syncing` phase (the owner had no way to tell a
    // push was in flight). auth_expired / blocked keep their T023 treatment (they take
    // precedence over syncing — an expired session is not "syncing").
    let progress = drain_progress();
    let (phase, last_error) = match drain_health() {
        DrainHealth::AuthExpired => (
            "auth_expired".to_string(),
            Some("Your session expired. Sign in again to resume sync.".to_string()),
        ),
        DrainHealth::Blocked(msg) => (connected_phase, Some(msg)),
        // R3: a mid-session relay that became UNREACHABLE (typed transport `Network` error,
        // classified at the transport — no string parsing) surfaces as a distinct
        // `unreachable` phase. `deriveSyncDisplay` maps it to the amber "Can't reach relay"
        // state instead of the destructive red "Sync error". DTO vehicle: a new `phase` string
        // value only — `SyncStatusDto.phase` is already `string`, so NO DTO shape / types.ts
        // regen is needed.
        DrainHealth::Unreachable(msg) => ("unreachable".to_string(), Some(msg)),
        // A persistently failing relay (F2) surfaces its verbatim error as `last_error` on
        // the connected phase; `deriveSyncDisplay` renders any set `last_error` as the
        // distinct "Sync error — needs attention" state, no DTO/contract change needed.
        DrainHealth::Failing(msg) => (connected_phase, Some(msg)),
        DrainHealth::Ok if session.sync_enabled && progress.syncing => {
            ("syncing".to_string(), last_error)
        }
        DrainHealth::Ok => (connected_phase, last_error),
    };
    SyncStatusDto {
        phase,
        server_url: session.server_url.clone(),
        email: Some(session.email.clone()),
        device_fingerprint: session.device_fingerprint.clone(),
        roster,
        vault_key_epoch: Some(session.epoch),
        roster_fingerprint: session.roster_fingerprint.clone(),
        sync_enabled: session.sync_enabled,
        last_error,
        billing_url: None,
        pending_entries: progress.pending_entries,
        pending_bytes: progress.pending_bytes,
        acked_this_session: progress.acked_this_session,
        last_success: progress.last_success,
    }
}

// ----- Tauri commands (contract mirrors apps/desktop/src/lib/sync.ts) -----

/// Normalize a user-entered relay URL before probing: trim, prepend `https://` when
/// schemeless (never downgrade an explicit scheme — an http retry is the user's choice,
/// §1a), and strip trailing slashes so `{url}/sync/info` is well-formed.
fn normalize_relay_url(input: &str) -> String {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// Parse `major.minor.patch` plus a "has pre-release tag" flag from a semver-ish string,
/// tolerating a missing minor/patch. Build metadata (`+…`) is ignored. Returns `None` on a
/// non-numeric core so an unparseable value yields NO advisory rather than a fake one.
fn parse_semver_core(v: &str) -> Option<(u64, u64, u64, bool)> {
    let core = v.split('+').next().unwrap_or(v);
    let (core, has_pre) = match core.split_once('-') {
        Some((c, pre)) => (c, !pre.is_empty()),
        None => (core, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next().unwrap_or("0").trim().parse().ok()?;
    let patch = parts.next().unwrap_or("0").trim().parse().ok()?;
    Some((major, minor, patch, has_pre))
}

/// True when `app_version` is strictly older than the relay's published
/// `min_client_version` (→ "update this app"). Unparseable inputs → `false` (no advisory);
/// we never manufacture an advisory from garbage.
///
/// POLICY (T029 Bug B): compare RELEASE CORES ONLY — a pre-release of `X.Y.Z` SATISFIES a
/// minimum of `X.Y.Z` and is NOT flagged. Rationale: the advisory exists solely to nag
/// genuinely outdated clients, and the relay publishes a single release-shaped minimum — it
/// cannot meaningfully express a prerelease floor (a server that wanted to bar
/// `1.0.0-alpha.11` but allow `1.0.0-alpha.12` has no way to say so). Treating our own
/// in-development `1.0.0-alpha.12` as "older than 1.0.0" produced a false "update YapStack"
/// nag against the relay's own advertised `1.0.0` during Windows UAT; comparing cores only
/// fixes that without ever hiding a real major/minor/patch gap.
fn client_is_older(app_version: &str, min_client_version: &str) -> bool {
    let (Some(a), Some(m)) = (
        parse_semver_core(app_version),
        parse_semver_core(min_client_version),
    ) else {
        return false;
    };
    // Release cores only; the parsed pre-release flag is deliberately ignored (see POLICY).
    (a.0, a.1, a.2) < (m.0, m.1, m.2)
}

/// Build the version advisory when this client is behind the relay's minimum. Advisory
/// only — the direction is "update this app" because the relay publishes the MINIMUM
/// client version (§0.3), never "the server is behind".
fn compute_version_advisory(
    app_version: &str,
    min_client_version: &str,
) -> Option<VersionAdvisory> {
    if client_is_older(app_version, min_client_version) {
        Some(VersionAdvisory {
            min_client_version: min_client_version.to_string(),
            raw: format!(
                "This app is version {app_version}; the relay requires at least \
                 {min_client_version}. Update YapStack to keep syncing."
            ),
        })
    } else {
        None
    }
}

/// Flatten a reqwest error's full source chain into one verbatim string. The hard product
/// rule is that the underlying detail (rustls alert, refused syscall, …) always reaches the
/// UI, so we never collapse to just the top-level `Display`.
fn error_chain(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = std::error::Error::source(err);
    while let Some(s) = source {
        parts.push(s.to_string());
        source = s.source();
    }
    parts.join(": ")
}

/// Heuristic TLS classifier over the flattened error chain.
///
/// TLS DISTINGUISHABILITY (verified for this dep config): reqwest is built with
/// `default-features = false, features = ["json", "rustls-tls"]` (Cargo.toml). In that
/// config reqwest exposes NO typed TLS-error variant — a handshake/cert failure surfaces as
/// a *connect* error (`err.is_connect() == true`) whose SOURCE chain terminates in a
/// `rustls::Error`. So `is_connect()` alone cannot separate "certificate rejected" from
/// "port refused"; the only signal available without adding a rustls dependency is the
/// chain text. We match rustls' documented alert/cert phrasings here. When the chain
/// carries none of these markers we fall back to `Unreachable` but keep the verbatim chain
/// — per T025, TLS that is genuinely indistinguishable degrades to Unreachable, never to a
/// swallowed error.
fn chain_looks_like_tls(chain: &str) -> bool {
    let c = chain.to_ascii_lowercase();
    [
        "certificate",
        "tls handshake",
        "handshakefailure",
        "invalid peer certificate",
        "unknownissuer",
        "notvalidforname",
        "received fatal alert",
        "corrupt message",
        "self-signed",
        "self signed",
        "rustls",
        "bad certificate",
    ]
    .iter()
    .any(|m| c.contains(m))
}

/// Map a send-time reqwest error to a typed probe error. Order matters: timeout first
/// (explicit 5s budget), then TLS (chain heuristic), then everything else → Unreachable.
fn classify_send_error(err: &reqwest::Error) -> RelayProbeError {
    let raw = error_chain(err);
    if err.is_timeout() {
        return RelayProbeError::Unreachable { raw };
    }
    if chain_looks_like_tls(&raw) {
        return RelayProbeError::TlsError { raw };
    }
    // `is_connect()` covers DNS/refused; anything else at send time (redirect loop, etc.)
    // is still "couldn't complete the request" → Unreachable with the verbatim chain.
    RelayProbeError::Unreachable { raw }
}

/// Cap a response body for inclusion in a verbatim `raw` message so an HTML error page
/// can't blow up the surfaced string; the status/endpoint context is always kept.
fn body_snippet(body: &str) -> String {
    const MAX: usize = 300;
    let trimmed = body.trim();
    if trimmed.chars().count() > MAX {
        let head: String = trimmed.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        trimmed.to_string()
    }
}

/// Core of `sync_probe`, parameterized on the app version + request budget so tests can
/// drive it against a local socket with a short timeout. See `sync_probe` for the contract.
async fn probe_relay(
    server_url: &str,
    app_version: &str,
    timeout: Duration,
) -> Result<RelayProbeOk, RelayProbeError> {
    let normalized_url = normalize_relay_url(server_url);
    let endpoint = format!("{normalized_url}/sync/info");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| RelayProbeError::Unreachable { raw: e.to_string() })?;

    let started = Instant::now();
    let resp = match client.get(&endpoint).send().await {
        Ok(r) => r,
        Err(e) => return Err(classify_send_error(&e)),
    };
    let latency_ms = started.elapsed().as_millis() as u64;

    let status = resp.status();
    // Read the body verbatim regardless of status so `raw` can carry the real detail.
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(RelayProbeError::NotARelay {
            raw: format!("HTTP {status} from {endpoint}: {}", body_snippet(&body)),
        });
    }

    // Sentinel: a 2xx is a relay ONLY if the body carries protocol_version + engine_version.
    let parsed: RelayProbeBody = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => {
            return Err(RelayProbeError::NotARelay {
                raw: format!(
                    "2xx from {endpoint} but the body is not a YapStack relay info document \
                     ({e}): {}",
                    body_snippet(&body)
                ),
            });
        }
    };

    let version_advisory = parsed
        .min_client_version
        .as_deref()
        .and_then(|min| compute_version_advisory(app_version, min));

    Ok(RelayProbeOk {
        engine_version: parsed.engine_version,
        protocol_version: parsed.protocol_version,
        latency_ms,
        normalized_url,
        version_advisory,
    })
}

/// Typed relay connection probe (T025). Returns a TYPED result the UI branches on:
/// `Unreachable` / `TlsError` / `NotARelay` are distinct classes, and a version gap is
/// advisory metadata on SUCCESS — never a failure. 5s request budget; the app version is
/// read the same way as `commands::health_check` (`env!("CARGO_PKG_VERSION")`, kept in
/// lockstep with tauri.conf.json by the build).
#[tauri::command]
#[specta::specta]
pub async fn sync_probe(server_url: String) -> Result<RelayProbeOk, RelayProbeError> {
    probe_relay(
        &server_url,
        env!("CARGO_PKG_VERSION"),
        Duration::from_secs(5),
    )
    .await
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
            pending_entries: 0,
            pending_bytes: 0,
            acked_this_session: 0,
            last_success: None,
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
        session.client_id,
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

    // §7.1 device identity (persistent, survives sign-out) + §7.5 first-device self-enrolled
    // roster (counter 0, epoch 0). Sourcing from the identity entry means a re-login on this
    // same install later presents the SAME client_id and is recognised, not re-enrolled.
    let (dev_seed, client_id) = load_or_create_device_identity()?;
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
        refresh_token: Some(tokens.refresh_token),
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

    let (dev_seed, client_id) = load_or_create_device_identity()?;
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
        refresh_token: Some(resp.refresh_token),
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

    let (dev_seed, client_id) = load_or_create_device_identity()?;
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
        refresh_token: Some(resp.refresh_token),
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

// ----- R1/R2 two-populated-device onboarding (seed / join) -----
//
// OWNER DECISION: the Mac (primary ~41 MB DB) SEEDS; the Windows device JOINs. The seed
// publishes an encrypted DB SNAPSHOT (R2 — not a ~434k-change replay); the join
// re-bootstraps from it and RECONCILES its own local-only rows with app-level dedup,
// NEVER independently CRRifying-and-merging (which is silently lossy). See sync::reconcile.

/// One surfaced reconciliation collision (an ambiguous local row that was NOT silently
/// dropped — the owner reviews these before discarding the old local DB).
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct CollisionDto {
    pub table: String,
    pub pk: String,
    /// "content_diverged" (same PK, different values) or "logical_duplicate".
    pub kind: String,
}

/// Result of a join reconciliation. `accounted == inserted + matched + collisions` and
/// equals the join's local row count — the no-silent-loss guarantee, surfaced to the UI.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ReconcileReportDto {
    pub inserted_local_only: u32,
    pub matched_identical: u32,
    pub collisions: Vec<CollisionDto>,
}

fn snapshot_scratch_dir(live_db: &Path) -> PathBuf {
    live_db
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// SEED (deliverable R2 seed side): CRRify a COPY of the primary DB, publish it as one
/// encrypted snapshot, and suppress re-pushing the whole history as changesets (the
/// snapshot carries it). Then start the drain. This device holds the authoritative
/// library; the other device joins from the snapshot.
///
/// PARKED — unregistered from the command builder (`lib.rs`), NOT a live command. This is the
/// future "migrate an existing library" (snapshot bootstrap) feature; it stays compiled with
/// `reconcile.rs` and re-enters the surface when that feature lands. See SYNC_REMEDIATION.md §6b.
#[allow(dead_code)]
pub async fn sync_seed(
    app: tauri::AppHandle,
    runtime: State<'_, SyncRuntimeState>,
) -> Result<SyncStatusDto, String> {
    let mut session = load_session()?.ok_or_else(|| "Sign in before seeding.".to_string())?;
    let vault_key = session.vault_key()?;
    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();

    // CRRify a COPY of the live DB (VACUUM INTO — the live DB is never opened for write).
    let sync_db = prepare_library_for_sync(&db_path)?;

    // Produce + encrypt the snapshot from the prepared CRR copy.
    let scratch = snapshot_scratch_dir(&db_path);
    let bytes = snapshot::produce_snapshot_bytes(&sync_db, &scratch).map_err(|e| e.to_string())?;
    let generation = 1u64; // v1: one snapshot generation per seed (re-seed bumps later).
    let cipher = SnapshotCipher::new(vault_key, session.epoch, session.tenant_id);
    let blob = cipher
        .encrypt(generation, &bytes)
        .map_err(|e| e.to_string())?;

    let transport = HttpTransport::new(base_url(&session.server_url), session.bearer.clone());
    // Baseline = the relay's current cursor: the join resumes incremental pull from here.
    let baseline_seq = transport
        .completeness()
        .await
        .map(|c| c.max_changeset_seq)
        .map_err(|e| e.to_string())?;
    transport
        .put_snapshot(
            SnapshotMeta {
                generation,
                baseline_seq,
            },
            &blob,
        )
        .await
        .map_err(|e| e.to_string())?;

    // R2 seed side: do NOT re-emit the whole history as changesets — advance the push
    // watermark past every row already captured in the snapshot.
    {
        let db = CrsqlDb::open(&sync_db).map_err(|e| e.to_string())?;
        let max_dbv: i64 = db
            .conn()
            .query_row(
                "SELECT coalesce(max(db_version),0) FROM crsql_changes WHERE site_id = crsql_site_id()",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        state::set_push_watermark(db.conn(), max_dbv).map_err(|e| e.to_string())?;
    }

    start_and_store_drain(&session, &runtime, &sync_db)?;
    session.sync_enabled = true;
    store_session(&session)?;
    Ok(build_status_dto(&session).await)
}

/// JOIN (deliverable R1 + R2 join side): re-bootstrap from the seed's snapshot into a
/// fresh CRR base, RECONCILE this device's own local-only rows into it (preserved, with
/// ambiguous collisions surfaced), then start the drain. NEVER independently
/// CRRifies-and-merges the live DB (silently lossy). The live DB is only ever READ.
///
/// PARKED — unregistered from the command builder (`lib.rs`), NOT a live command. This is the
/// future "migrate an existing library" (snapshot bootstrap + reconcile) feature; it stays
/// compiled with `reconcile.rs` and re-enters the surface when that feature lands. See
/// SYNC_REMEDIATION.md §6b.
#[allow(dead_code)]
pub async fn sync_join(
    app: tauri::AppHandle,
    runtime: State<'_, SyncRuntimeState>,
) -> Result<ReconcileReportDto, String> {
    let mut session = load_session()?.ok_or_else(|| "Sign in before joining.".to_string())?;
    let vault_key = session.vault_key()?;
    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();

    // Pull the seed's snapshot (surface, never fall back to a lossy self-merge).
    let transport = HttpTransport::new(base_url(&session.server_url), session.bearer.clone());
    let (meta, blob) = transport
        .get_snapshot()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            "No snapshot on the relay yet — publish one from the seed device first.".to_string()
        })?;
    let cipher = SnapshotCipher::new(vault_key, session.epoch, session.tenant_id);
    let snap_bytes = cipher
        .decrypt(meta.generation, &blob)
        .map_err(|e| e.to_string())?;

    // Write it as the CRR base and re-site so this device becomes an independent peer
    // (fresh site id + client id) that will not re-push the seed's history.
    let sync_db = sync_db_path(&db_path);
    snapshot::write_snapshot_file(&sync_db, &snap_bytes).map_err(|e| e.to_string())?;
    {
        let base = CrsqlDb::open(&sync_db).map_err(|e| e.to_string())?;
        reconcile::resite_as_fresh_peer(base.conn()).map_err(|e| e.to_string())?;
        reconcile::reset_join_local_state(base.conn()).map_err(|e| e.to_string())?;
    }

    // Reopen (so cr-sqlite re-reads the fresh site id) and reconcile local-only rows.
    let base = CrsqlDb::open(&sync_db).map_err(|e| e.to_string())?;
    state::set_pull_watermark(base.conn(), meta.baseline_seq).map_err(|e| e.to_string())?;

    // The live DB is opened READ-ONLY — it is never written (data-safety invariant).
    let live_ro = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())?;
    let report =
        reconcile::reconcile_local_rows(&live_ro, base.conn()).map_err(|e| e.to_string())?;
    drop(live_ro);

    // Reinstate the stripped FK cascade + UNIQUE invariants after reconciliation.
    cascade::cascade_gc(base.conn()).map_err(|e| e.to_string())?;
    uniqueness::enforce_uniqueness(base.conn()).map_err(|e| e.to_string())?;
    mark_prepared(base.conn()).map_err(|e| e.to_string())?;
    drop(base);

    start_and_store_drain(&session, &runtime, &sync_db)?;
    session.sync_enabled = true;
    store_session(&session)?;

    Ok(ReconcileReportDto {
        inserted_local_only: report.inserted_local_only as u32,
        matched_identical: report.matched_identical as u32,
        collisions: report
            .collisions
            .into_iter()
            .map(|c| CollisionDto {
                table: c.table,
                pk: c.pk,
                kind: match c.kind {
                    reconcile::CollisionKind::ContentDiverged => "content_diverged".into(),
                    reconcile::CollisionKind::LogicalDuplicate => "logical_duplicate".into(),
                },
            })
            .collect(),
    })
}

/// Spawn the drain on its dedicated thread and store the handle (shared by seed + join).
fn start_and_store_drain(
    session: &Session,
    runtime: &State<'_, SyncRuntimeState>,
    sync_db: &Path,
) -> Result<(), String> {
    let vault_key = session.vault_key()?;
    let handle = spawn_drain(
        sync_db.to_path_buf(),
        session.server_url.clone(),
        session.bearer.clone(),
        vault_key,
        session.epoch,
        session.tenant_id,
        session.client_id,
    )?;
    let mut guard = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?;
    if let Some(mut prev) = guard.take() {
        prev.stop();
    }
    *guard = Some(handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_surface_threshold_gates_a_single_blip_but_surfaces_a_run() {
        // F2 contract: one failed cycle stays QUIET (a transient blip self-heals); the
        // SECOND consecutive failure (== DRAIN_FAIL_SURFACE_THRESHOLD) surfaces; a later
        // clean cycle CLEARS the run so the next single blip is quiet again.
        assert_eq!(DRAIN_FAIL_SURFACE_THRESHOLD, 2, "test assumes threshold 2");
        let mut n: u32 = 0;

        // 1st failure — below threshold, quiet.
        assert_eq!(fail_surface_step(&mut n, true), FailSurface::Quiet);
        assert_eq!(n, 1);
        // 2nd consecutive failure — reaches threshold, surface the verbatim error.
        assert_eq!(fail_surface_step(&mut n, true), FailSurface::Surface);
        assert_eq!(n, 2);
        // 3rd consecutive failure — stays surfaced (still at/over threshold).
        assert_eq!(fail_surface_step(&mut n, true), FailSurface::Surface);
        assert_eq!(n, 3);
        // A clean cycle clears the run.
        assert_eq!(fail_surface_step(&mut n, false), FailSurface::Clear);
        assert_eq!(n, 0);
        // A single blip after recovery is quiet again (no flap).
        assert_eq!(fail_surface_step(&mut n, true), FailSurface::Quiet);
        assert_eq!(n, 1);
    }

    #[test]
    fn invalidate_session_credentials_drops_tokens_keeps_identity() {
        // R3 (HIGH): a terminal refresh failure strips ONLY the spent tokens — the session
        // record (so the AuthExpired "sign in again" surface still renders) and the device
        // identity (T019) survive, and the next boot presents NO refresh token.
        let s = Session {
            server_url: "https://relay.test".into(),
            email: "a@b.com".into(),
            vault_key_b64: B64.encode([1u8; 32]),
            epoch: 4,
            tenant_id: Uuid::from_u128(9),
            bearer: "access-token".into(),
            refresh_token: Some("refresh-token".into()),
            device_fingerprint: Some("FFFFGGGGHHHHJJJJ".into()),
            sync_enabled: true,
            client_id: Uuid::from_u128(1234),
            device_sk_b64: B64.encode([7u8; 32]),
            salt_enc_b64: None,
            roster_counter: 2,
            roster_fingerprint: None,
        };
        let stripped = invalidate_session_credentials(s);
        assert!(stripped.bearer.is_empty(), "spent access token dropped");
        assert!(
            stripped.refresh_token.is_none(),
            "spent refresh token dropped"
        );
        // Everything else preserved so the session still surfaces AuthExpired, not signed-out.
        assert_eq!(stripped.email, "a@b.com");
        assert_eq!(stripped.client_id, Uuid::from_u128(1234));
        assert_eq!(stripped.device_sk_b64, B64.encode([7u8; 32]));
        assert!(stripped.sync_enabled);
        assert_eq!(stripped.vault_key_b64, B64.encode([1u8; 32]));
    }

    #[test]
    fn refresh_status_401_is_terminal_others_transient() {
        use reqwest::StatusCode;
        // A success status is not a failure — the caller decodes the rotated pair.
        assert!(classify_refresh_status(StatusCode::OK).is_none());
        // 401 is the relay REJECTING the refresh token (reuse detection revoked the family on
        // a spent token) — terminal, sign in again.
        assert!(matches!(
            classify_refresh_status(StatusCode::UNAUTHORIZED),
            Some(RefreshFailure::Rejected(_))
        ));
        // Any other non-2xx is a server-side transient — retry; the refresh token is still valid.
        assert!(matches!(
            classify_refresh_status(StatusCode::INTERNAL_SERVER_ERROR),
            Some(RefreshFailure::Transient(_))
        ));
        assert!(matches!(
            classify_refresh_status(StatusCode::BAD_GATEWAY),
            Some(RefreshFailure::Transient(_))
        ));
    }

    #[test]
    fn refresh_failure_maps_to_the_right_drain_health() {
        // R3 decision: rejected ⇒ terminal (None here → the expire/AuthExpired path); a network
        // failure ⇒ amber "unreachable"; any other transient ⇒ "failing". Network/transient
        // NEVER sign out.
        assert!(
            retryable_refresh_health(&RefreshFailure::Rejected("x".into())).is_none(),
            "a rejected refresh is terminal, not retried"
        );
        assert!(matches!(
            retryable_refresh_health(&RefreshFailure::Network("x".into())),
            Some(DrainHealth::Unreachable(_))
        ));
        assert!(matches!(
            retryable_refresh_health(&RefreshFailure::Transient("x".into())),
            Some(DrainHealth::Failing(_))
        ));
    }

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
    fn device_identity_seed_roundtrips_to_stable_key() {
        // The persisted identity blob must decode back to the exact seed, and that seed must
        // drive a deterministic Ed25519 identity — the invariant that makes a re-login on the
        // same install present the SAME device key (not a phantom one).
        let seed = [11u8; 32];
        let id = DeviceIdentity {
            client_id: Uuid::from_u128(42),
            device_sk_b64: B64.encode(seed),
        };
        assert_eq!(id.seed().unwrap(), seed);
        let pub1 = SigningKey::from_bytes(&id.seed().unwrap())
            .verifying_key()
            .to_bytes();
        let pub2 = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        assert_eq!(pub1, pub2);
        // A corrupt blob is rejected rather than silently yielding a wrong key.
        let bad = DeviceIdentity {
            client_id: Uuid::nil(),
            device_sk_b64: "not-base64!!".into(),
        };
        assert!(bad.seed().is_err());
    }

    /// End-to-end identity lifecycle against the REAL backing store (OS keychain in release,
    /// the dev file store in debug): migration out of a session-only install, survival across
    /// sign-out, and a stable id on re-login. Gated `#[ignore]` because store access is
    /// unavailable/headless in CI; run explicitly with `cargo test --features sync -- --ignored`.
    /// Non-destructive: it backs up and restores whatever entries the machine already holds. The
    /// cache is reset at each checkpoint so the assertions exercise the persistent store, not
    /// just the in-memory cache.
    #[test]
    #[ignore = "touches the real credential store (backs up + restores); run with --ignored"]
    fn identity_survives_session_clear() {
        let orig_session = load_session_from_store().ok().flatten();
        let orig_identity = load_identity_from_store().ok().flatten();
        let _ = store_delete(KEYCHAIN_ACCOUNT);
        let _ = store_delete(KEYCHAIN_ACCOUNT_IDENTITY);
        reset_cache_for_test();

        // A pre-upgrade install: identity lives ONLY in the session entry.
        let seed = [7u8; 32];
        let client_id = Uuid::from_u128(1234);
        let session = Session {
            server_url: "https://relay.test".into(),
            email: "id-test@example.com".into(),
            vault_key_b64: B64.encode([1u8; 32]),
            epoch: 0,
            tenant_id: Uuid::nil(),
            bearer: "t".into(),
            refresh_token: None,
            device_fingerprint: None,
            sync_enabled: false,
            client_id,
            device_sk_b64: B64.encode(seed),
            salt_enc_b64: None,
            roster_counter: 0,
            roster_fingerprint: None,
        };
        store_session(&session).unwrap();

        // First resolve migrates the session identity into the identity entry.
        let (got_seed, got_id) = load_or_create_device_identity().unwrap();
        assert_eq!(got_id, client_id);
        assert_eq!(got_seed, seed);
        // Re-read from the persistent store (not just the cache) to prove it was written.
        reset_cache_for_test();
        assert_eq!(load_identity().unwrap().unwrap().client_id, client_id);

        // Sign out clears the session but MUST preserve the identity.
        clear_session().unwrap();
        reset_cache_for_test();
        assert!(load_session().unwrap().is_none());
        assert_eq!(load_identity().unwrap().unwrap().client_id, client_id);

        // Sign back in on the SAME install → SAME client_id (no phantom pending device).
        let (_, relogin_id) = load_or_create_device_identity().unwrap();
        assert_eq!(relogin_id, client_id);

        // Restore the machine's original credential-store state.
        let _ = store_delete(KEYCHAIN_ACCOUNT_IDENTITY);
        let _ = store_delete(KEYCHAIN_ACCOUNT);
        reset_cache_for_test();
        if let Some(s) = orig_session {
            let _ = store_session(&s);
        }
        if let Some(i) = orig_identity {
            let _ = store_identity(&i);
        }
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

    // ----- Key-wrapped session store (T029) -----
    //
    // Exercises the RELEASE session store's crypto/file/migration/degrade logic against an
    // in-memory `SessionKeyStore` fake + a temp file (the "mockable keychain seam"), so the
    // production path is verified in `cargo test` (a debug build) without a real keychain and
    // without perturbing the dev-file store.
    mod session_store {
        use super::super::*;
        use std::cell::RefCell;

        /// In-memory stand-in for the keychain (release) / dev file (debug): holds the tiny
        /// wrapping key.
        #[derive(Default)]
        struct FakeKeyStore {
            key: RefCell<Option<String>>,
        }
        impl SessionKeyStore for FakeKeyStore {
            fn get_key(&self) -> Result<Option<String>, String> {
                Ok(self.key.borrow().clone())
            }
            fn set_key(&self, b64: &str) -> Result<(), String> {
                *self.key.borrow_mut() = Some(b64.to_string());
                Ok(())
            }
            fn delete_key(&self) -> Result<(), String> {
                *self.key.borrow_mut() = None;
                Ok(())
            }
        }

        fn temp_enc_path() -> PathBuf {
            std::env::temp_dir().join(format!("yapstack-sess-test-{}.enc", Uuid::new_v4()))
        }

        /// A session with realistically LARGE token/key material — the exact payload that
        /// overflowed the Windows keychain when stored inline.
        fn sample_session() -> Session {
            Session {
                server_url: "https://relay.example.com".into(),
                email: "owner@example.com".into(),
                vault_key_b64: B64.encode([9u8; 32]),
                epoch: 3,
                tenant_id: Uuid::from_u128(77),
                // Two ~800-char JWT-shaped tokens: comfortably over the 2560-UTF-16 cap.
                bearer: "h.".to_string() + &"A".repeat(800) + ".sig",
                refresh_token: Some("r.".to_string() + &"B".repeat(800) + ".sig"),
                device_fingerprint: Some("AAAABBBBCCCCDDDD".into()),
                sync_enabled: true,
                client_id: Uuid::from_u128(1234),
                device_sk_b64: B64.encode([7u8; 32]),
                salt_enc_b64: Some(B64.encode([2u8; 16])),
                roster_counter: 5,
                roster_fingerprint: Some("EEEEFFFFGGGGHHHH".into()),
            }
        }

        fn json_of(s: &Session) -> String {
            serde_json::to_string(s).unwrap()
        }

        #[test]
        fn wrap_unwrap_roundtrip_through_file() {
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            let s = sample_session();
            store_session_wrapped(&ks, &path, &s).unwrap();

            // The on-disk bytes are ciphertext (version byte + nonce + AEAD ct), not plaintext.
            let raw = std::fs::read(&path).unwrap();
            assert_eq!(raw[0], yapstack_crypto::VERSION, "version byte first");
            assert!(!raw.windows(5).any(|w| w == b"owner"), "no plaintext email");
            assert!(!raw.windows(4).any(|w| w == b"relay"), "no plaintext URL");

            let loaded = load_session_wrapped(&ks, &path).unwrap().unwrap();
            assert_eq!(json_of(&loaded), json_of(&s));
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn tampered_file_degrades_to_signed_out() {
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();

            // Flip one ciphertext byte (past the version byte) → AEAD open must fail cleanly.
            let mut raw = std::fs::read(&path).unwrap();
            let last = raw.len() - 1;
            raw[last] ^= 0x01;
            std::fs::write(&path, &raw).unwrap();

            assert!(
                load_session_wrapped(&ks, &path).unwrap().is_none(),
                "a tampered session file must degrade to signed-out, never a session"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn missing_file_degrades_and_cleans_stray_key() {
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();
            std::fs::remove_file(&path).unwrap();

            assert!(load_session_wrapped(&ks, &path).unwrap().is_none());
            // The orphaned wrapping key is cleaned up so the next boot starts clean.
            assert!(ks.get_key().unwrap().is_none());
        }

        #[test]
        fn missing_key_degrades_and_cleans_orphan_file() {
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();
            ks.delete_key().unwrap();

            assert!(load_session_wrapped(&ks, &path).unwrap().is_none());
            // The undecryptable orphan file is removed.
            assert!(read_session_blob(&path).unwrap().is_none());
        }

        #[test]
        fn keychain_payload_stays_tiny_and_identity_has_headroom() {
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();

            // What the keychain now holds for the session is ONLY the base64 wrapping key.
            let key_val = ks.get_key().unwrap().expect("wrapping key persisted");
            assert!(
                key_val.chars().count() < 200,
                "keychain session payload must be < 200 chars (was {})",
                key_val.chars().count()
            );

            // `identity-v1` is unchanged and must keep comfortable headroom under the Windows
            // 2560-UTF-16 credential cap.
            let identity = DeviceIdentity {
                client_id: Uuid::from_u128(1234),
                device_sk_b64: B64.encode([7u8; 32]),
            };
            let id_len = serde_json::to_string(&identity).unwrap().chars().count();
            assert!(
                id_len < 2560,
                "identity-v1 must stay under the Windows cap (was {id_len})"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[cfg(unix)]
        #[test]
        fn sealed_file_is_owner_only_0600() {
            use std::os::unix::fs::PermissionsExt;
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "sealed session file must be owner read/write only"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn two_seals_use_distinct_nonces_and_ciphertexts() {
            // T029 Judge (nonce uniqueness): every seal draws a fresh 192-bit nonce, so two
            // seals of the SAME session under the SAME key/epoch differ in the nonce region
            // AND the ciphertext — never a nonce reuse (which XChaCha20-Poly1305 forbids).
            let key = [5u8; 32];
            let pt = json_of(&sample_session());
            let a = seal_session(&key, 3, pt.as_bytes()).unwrap();
            let b = seal_session(&key, 3, pt.as_bytes()).unwrap();
            // Envelope: version(1) || nonce(24) || ct||tag. Nonce is bytes [1..25).
            assert_ne!(a[1..25], b[1..25], "nonces must differ across seals");
            assert_ne!(a, b, "ciphertexts must differ across seals");
            // Both still open back to the same plaintext under the same key+epoch.
            assert_eq!(open_session(&key, 3, &a).unwrap(), pt.as_bytes());
            assert_eq!(open_session(&key, 3, &b).unwrap(), pt.as_bytes());
        }

        #[test]
        fn rollback_to_older_epoch_is_rejected() {
            // Anti-rollback (T029 Judge): the current session is sealed at epoch 3 and the
            // keychain records epoch 3. An attacker with FILE access (but not keychain access)
            // re-seals an OLDER session (epoch 2) under the SAME wrapping key and overwrites the
            // file, leaving the keychain entry untouched (still epoch 3). Loading must REJECT:
            // the old blob's AAD binds epoch 2, but we open under the recorded epoch 3 → AEAD
            // failure → signed-out. Guarantee: a file-only attacker cannot roll back to an
            // older sealed session under the same wrapping key.
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            let mut current = sample_session();
            current.epoch = 3;
            store_session_wrapped(&ks, &path, &current).unwrap();

            // Recover the wrapping key the store minted (attacker has the FILE + can read the
            // wrapping key only in this test harness; the real threat model is file-only, and
            // even WITH the key the epoch binding blocks the rollback).
            let (key, recorded_epoch) = decode_wrap_entry(&ks.get_key().unwrap().unwrap()).unwrap();
            assert_eq!(recorded_epoch, 3);

            // Forge an older (epoch 2) session sealed under the same key, overwrite the file.
            let mut older = sample_session();
            older.epoch = 2;
            let forged = seal_session(&key, 2, json_of(&older).as_bytes()).unwrap();
            write_session_blob(&path, &forged).unwrap();

            // Keychain still says epoch 3 → open under epoch 3 → the epoch-2 blob is rejected.
            assert!(
                load_session_wrapped(&ks, &path).unwrap().is_none(),
                "an older-epoch sealed session must not open under the recorded epoch"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn old_domain_aad_blob_fails_to_open_cleanly() {
            // A file sealed by an OLDER build (domain `v1`, no epoch field in the AAD) must not
            // open under the current `v2` epoch-bound AAD — it degrades to signed-out, never a
            // panic. No migration is owed (zero production installs; dev uses the debug store).
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            let s = sample_session();
            let key = new_wrap_key();
            // Reproduce the OLD v1 construction: AAD = LP(version, "yapstack.session.store.v1").
            let mut nonce = [0u8; 24];
            OsRng.fill_bytes(&mut nonce);
            let old_aad = yapstack_crypto::aead::lp(&[
                &[yapstack_crypto::VERSION],
                b"yapstack.session.store.v1",
            ]);
            let old_blob = yapstack_crypto::aead::seal_standard(
                &key,
                &nonce,
                json_of(&s).as_bytes(),
                &old_aad,
            )
            .unwrap();
            write_session_blob(&path, &old_blob).unwrap();
            ks.set_key(&encode_wrap_entry(&key, s.epoch).unwrap())
                .unwrap();

            assert!(
                load_session_wrapped(&ks, &path).unwrap().is_none(),
                "a v1-AAD blob must fail cleanly under the v2 epoch-bound AAD"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    // ----- Typed relay connection probe (T025) -----
    //
    // Live-server cases use a one-shot raw-HTTP responder over a loopback socket (no extra
    // deps; mirrors the `yapstack-sync` transport.rs idiom). Plain-HTTP servers are probed
    // via explicit `http://` URLs so URL normalization does not force `https` onto them;
    // normalization itself is covered by a pure unit test.
    mod probe {
        use super::super::*;
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::thread;

        const APP: &str = "1.0.0";

        /// Read the request until end-of-headers so the client's write completes before we
        /// reply and close.
        fn drain_request(sock: &mut TcpStream) {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = sock.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }

        /// Spawn a one-shot responder that serves `response` to the first connection.
        fn serve_once(response: String) -> (String, thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = thread::spawn(move || {
                if let Ok((mut sock, _)) = listener.accept() {
                    drain_request(&mut sock);
                    let _ = sock.write_all(response.as_bytes());
                    let _ = sock.flush();
                }
            });
            (format!("http://{addr}"), handle)
        }

        fn http_response(status_line: &str, content_type: &str, body: &str) -> String {
            format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: {content_type}\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
        }

        #[tokio::test(flavor = "current_thread")]
        async fn ok_returns_versions_latency_and_normalized_url() {
            let body =
                r#"{"protocol_version":1,"min_client_version":"1.0.0","engine_version":"0.16.3"}"#;
            let (base, handle) = serve_once(http_response("200 OK", "application/json", body));
            // Pass a trailing slash: normalization must strip it before `/sync/info`.
            let ok = probe_relay(&format!("{base}/"), APP, Duration::from_secs(5))
                .await
                .expect("a valid relay must probe ok");
            assert_eq!(ok.engine_version, "0.16.3");
            assert_eq!(ok.protocol_version, 1);
            assert_eq!(ok.normalized_url, base);
            assert!(ok.latency_ms < 5_000);
            assert!(ok.version_advisory.is_none());
            handle.join().unwrap();
        }

        #[tokio::test(flavor = "current_thread")]
        async fn version_gap_is_advisory_not_failure() {
            let body =
                r#"{"protocol_version":1,"min_client_version":"2.0.0","engine_version":"0.16.3"}"#;
            let (base, handle) = serve_once(http_response("200 OK", "application/json", body));
            let ok = probe_relay(&base, "1.0.0", Duration::from_secs(5))
                .await
                .expect("a version gap must still succeed (advisory, not failure)");
            let adv = ok
                .version_advisory
                .expect("older client than min_client_version → advisory");
            assert_eq!(adv.min_client_version, "2.0.0");
            assert!(adv.raw.contains("2.0.0"));
            handle.join().unwrap();
        }

        #[tokio::test(flavor = "current_thread")]
        async fn html_200_is_not_a_relay() {
            let (base, handle) =
                serve_once(http_response("200 OK", "text/html", "<html>hello</html>"));
            let err = probe_relay(&base, APP, Duration::from_secs(5))
                .await
                .unwrap_err();
            match err {
                RelayProbeError::NotARelay { raw } => assert!(raw.contains("html")),
                other => panic!("expected NotARelay for an HTML 200, got {other:?}"),
            }
            handle.join().unwrap();
        }

        #[tokio::test(flavor = "current_thread")]
        async fn wrong_json_200_is_not_a_relay() {
            let (base, handle) = serve_once(http_response(
                "200 OK",
                "application/json",
                r#"{"res":"pong"}"#,
            ));
            let err = probe_relay(&base, APP, Duration::from_secs(5))
                .await
                .unwrap_err();
            assert!(matches!(err, RelayProbeError::NotARelay { .. }));
            handle.join().unwrap();
        }

        #[tokio::test(flavor = "current_thread")]
        async fn partial_sentinel_200_is_not_a_relay() {
            // engine_version present but protocol_version missing → sentinel must still fail.
            let (base, handle) = serve_once(http_response(
                "200 OK",
                "application/json",
                r#"{"engine_version":"0.16.3"}"#,
            ));
            let err = probe_relay(&base, APP, Duration::from_secs(5))
                .await
                .unwrap_err();
            assert!(matches!(err, RelayProbeError::NotARelay { .. }));
            handle.join().unwrap();
        }

        #[tokio::test(flavor = "current_thread")]
        async fn non_2xx_is_not_a_relay() {
            let (base, handle) = serve_once(http_response("404 Not Found", "text/plain", "nope"));
            let err = probe_relay(&base, APP, Duration::from_secs(5))
                .await
                .unwrap_err();
            match err {
                RelayProbeError::NotARelay { raw } => assert!(raw.contains("404")),
                other => panic!("expected NotARelay for a 404, got {other:?}"),
            }
            handle.join().unwrap();
        }

        #[tokio::test(flavor = "current_thread")]
        async fn connection_refused_is_unreachable() {
            // Bind then immediately drop → a closed port on loopback (refused on connect).
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            let err = probe_relay(&format!("http://{addr}"), APP, Duration::from_secs(5))
                .await
                .unwrap_err();
            assert!(
                matches!(err, RelayProbeError::Unreachable { .. }),
                "refused connect must be Unreachable, got {err:?}"
            );
        }

        #[tokio::test(flavor = "current_thread")]
        async fn hanging_handler_times_out_as_unreachable() {
            // Accept but never respond; a short budget must trip reqwest's is_timeout().
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = thread::spawn(move || {
                if let Ok((mut sock, _)) = listener.accept() {
                    drain_request(&mut sock);
                    thread::sleep(Duration::from_millis(500));
                    // Drop without responding — the client times out first.
                    drop(sock);
                }
            });
            let err = probe_relay(&format!("http://{addr}"), APP, Duration::from_millis(150))
                .await
                .unwrap_err();
            assert!(
                matches!(err, RelayProbeError::Unreachable { .. }),
                "timeout must be Unreachable, got {err:?}"
            );
            handle.join().unwrap();
        }

        #[test]
        fn normalize_prepends_https_and_strips_trailing_slashes() {
            assert_eq!(
                normalize_relay_url("sync.yapstack.app"),
                "https://sync.yapstack.app"
            );
            assert_eq!(
                normalize_relay_url("  sync.yapstack.app/  "),
                "https://sync.yapstack.app"
            );
            assert_eq!(
                normalize_relay_url("https://relay.test///"),
                "https://relay.test"
            );
            // An explicit http scheme is a deliberate user choice — never silently upgraded.
            assert_eq!(
                normalize_relay_url("http://192.168.1.9:8080/"),
                "http://192.168.1.9:8080"
            );
        }

        #[test]
        fn version_advisory_direction_and_edges() {
            // Older core → advisory ("update this app"). (T029: this genuine gap still nags.)
            assert!(compute_version_advisory("0.9.0", "1.0.0").is_some());
            // Equal or newer → no advisory (never "the server is behind").
            assert!(compute_version_advisory("1.0.0", "1.0.0").is_none());
            assert!(compute_version_advisory("1.2.0", "1.0.0").is_none());
            // T029 Bug B policy: a pre-release of the required release SATISFIES the minimum
            // (compare release cores only). The owner's own `1.0.0-alpha.12` build must NOT
            // nag itself against the relay's advertised `1.0.0`.
            assert!(compute_version_advisory("1.0.0-alpha.12", "1.0.0").is_none());
            // A release is NOT older than its own pre-release (cores equal → satisfied).
            assert!(compute_version_advisory("1.0.0", "1.0.0-alpha.1").is_none());
            // A genuinely older core is still flagged even when it carries a pre-release tag.
            assert!(compute_version_advisory("0.9.0-beta.1", "1.0.0").is_some());
            // Unparseable server value → no manufactured advisory.
            assert!(compute_version_advisory("1.0.0", "not-a-version").is_none());
        }

        #[test]
        fn tls_classifier_matches_rustls_phrasings_only() {
            assert!(chain_looks_like_tls(
                "error sending request: invalid peer certificate: UnknownIssuer"
            ));
            assert!(chain_looks_like_tls(
                "received fatal alert: HandshakeFailure"
            ));
            assert!(chain_looks_like_tls("rustls: corrupt message"));
            // A plain refused / DNS connect error must NOT be read as TLS.
            assert!(!chain_looks_like_tls(
                "tcp connect error: Connection refused (os error 61)"
            ));
            assert!(!chain_looks_like_tls(
                "dns error: failed to lookup address information"
            ));
        }
    }
}
