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
//! (A3 cutover) `perform_cutover` turns the LIVE `yapstack.db` INTO the CRR
//! database (the copy architecture is gone) so the drain captures every UI write —
//! run at Enable and, once, on drain start for a device enabled under the old copy
//! architecture; plus the OS-keychain vault key at rest (CRYPTO_SPEC §10).
//!
//! Command surface (all registered live in `lib.rs`, `lib/sync.ts` is the TS
//! contract): the relay probe (`sync_probe`), the status poll (`sync_status`),
//! the auth ceremony round-trips (`sync_signup` / `sync_login_begin` /
//! `sync_login_finish` / `sync_recover` / `sync_approve_device` / `sync_sign_out`),
//! and enable (`sync_enable`, THE single enable path — cutover the live DB to CRR
//! then start the drain). `start_drain_if_enabled` is the deliverable-A boot wiring
//! invoked from the `setup` hook (and runs the one-time cutover for an already-
//! enabled device on a non-CRR live DB).
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};
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
/// How often the INDEPENDENT audio-upload lane cycles once idle (S2). Longer than the
/// changeset interval — audio is best-effort background work, not correctness-critical.
const AUDIO_DRAIN_INTERVAL: Duration = Duration::from_secs(8);

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

/// The at-rest sealed-session filename, joined onto the resolved store directory.
const SESSION_ENC_FILENAME: &str = "sync-session.enc";

// ----- Deletion-reason vocabulary (R10 fix 5) -----
//
// Every credential / sealed-file removal (or replacement) carries an explicit REASON that is
// logged at the delete site. After R10 fix 1 (non-destructive reads) the ONLY two reasons a
// production build may ever remove or replace either session component are an explicit user
// sign-out and a successful overwrite by a fresh sign-in — nothing on a read/boot path may
// delete. These are the exhaustive production inventory; any third reason appearing in a log
// is a regression.

/// Reason: the user explicitly signed out (`clear_session_wrapped`). Removes BOTH the sealed
/// file and the wrapping-key entry.
const DELETE_REASON_SIGN_OUT: &str = "user_sign_out";

/// Reason: a fresh sign-in overwrote an existing wrapped session (`store_session_wrapped`) —
/// the sealed file is replaced via atomic rename and the wrapping-key entry via `set_key`.
const DELETE_REASON_OVERWRITE: &str = "overwrite";

/// Pure resolver for the encrypted at-rest session file path (T029), factored out of
/// [`session_enc_path`] so the determinism invariant is unit-testable without touching the
/// process-global `SESSION_STORE_DIR`.
///
/// R6 item 4(c): `store_dir = Some(config_dir)` once `init_credential_store` has run, and
/// `None` before it. The path-mismatch bug is precisely `resolve_session_enc_path(None)` (a
/// boot read that raced ahead of init) NOT equalling `resolve_session_enc_path(Some(config_dir))`
/// (the sign-in write → config dir). With init hoisted before every session touch (lib.rs),
/// both the boot read and the sign-in write resolve through `Some(config_dir)` and are
/// identical.
///
/// R10 fix 2: an unresolved store dir is NO LONGER a silent `%TEMP%` fallback in RELEASE — it
/// is an ERROR. Writing the real session to `%TEMP%` (a) escapes the durable, ACL-scoped config
/// dir and (b) hides the init race instead of surfacing it. The pure seam below takes
/// `allow_temp_fallback` explicitly so a test can exercise BOTH the release (no-fallback →
/// error) and the debug (fallback retained for dev ergonomics) behaviour under one build.
fn resolve_session_enc_path_seam(
    store_dir: Option<&Path>,
    allow_temp_fallback: bool,
) -> Result<PathBuf, String> {
    match store_dir {
        Some(dir) => Ok(dir.join(SESSION_ENC_FILENAME)),
        None if allow_temp_fallback => temp_fallback_path(),
        None => Err(
            "session store dir unresolved (init_credential_store has not run); refusing the \
             %TEMP% fallback"
                .to_string(),
        ),
    }
}

/// Whether an unresolved store dir may fall back to `%TEMP%`. COMPILE-TIME hard (R10.1 N4):
/// selected by `#[cfg]`, NOT the RUNTIME `cfg!(debug_assertions)` value the previous seam
/// passed through. The old form compiled the permissive `true` AND the `std::env::temp_dir()`
/// body into every binary and merely guarded them behind a runtime flag — so a
/// release-with-`debug-assertions=on` profile silently re-enabled the fallback. Now the
/// permissive const and the fallback body (`temp_fallback_path`) are physically ABSENT from a
/// normal release build. (A fully profile-proof gate against release-with-debug-assertions
/// would key on a dedicated cargo feature; that edit lives outside this file's allowed surface,
/// so `any(test, debug_assertions)` is the strongest in-file gate.)
#[cfg(any(test, debug_assertions))]
const TEMP_FALLBACK_ALLOWED: bool = true;
#[cfg(not(any(test, debug_assertions)))]
const TEMP_FALLBACK_ALLOWED: bool = false;

/// The `%TEMP%` fallback target for an unresolved store dir. COMPILE-TIME gated (R10.1 N4):
/// the permissive body — the ONLY reference to `std::env::temp_dir()` on the session path —
/// exists solely in test/debug builds. A production release build compiles the `Err` body, so
/// `std::env::temp_dir()` is never linked into the release session path at all.
#[cfg(any(test, debug_assertions))]
fn temp_fallback_path() -> Result<PathBuf, String> {
    Ok(std::env::temp_dir().join(SESSION_ENC_FILENAME))
}

#[cfg(not(any(test, debug_assertions)))]
fn temp_fallback_path() -> Result<PathBuf, String> {
    Err(
        "session store dir unresolved (init_credential_store has not run); refusing the \
         %TEMP% fallback"
            .to_string(),
    )
}

/// Resolve the sealed session path. DEBUG/dev keeps the temp fallback (dev ergonomics + unit
/// tests); RELEASE forbids it — an unresolved `SESSION_STORE_DIR` is an error the caller
/// surfaces (read degrades with a clear log; write hard-fails so sign-in visibly fails rather
/// than writing the session to `%TEMP%`).
fn resolve_session_enc_path(store_dir: Option<&Path>) -> Result<PathBuf, String> {
    resolve_session_enc_path_seam(store_dir, TEMP_FALLBACK_ALLOWED)
}

/// Path to the encrypted at-rest session file (T029). In RELEASE this errors before
/// `init_credential_store` has set `SESSION_STORE_DIR` (no `%TEMP%` fallback, R10 fix 2).
fn session_enc_path() -> Result<PathBuf, String> {
    resolve_session_enc_path(SESSION_STORE_DIR.get().map(PathBuf::as_path))
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

// RELEASE, non-Windows (macOS `apple-native`): the `keyring` crate over the OS keychain,
// UNCHANGED. macOS Keychain persists correctly and has no credential-blob limit issue.
#[cfg(all(not(debug_assertions), not(target_os = "windows")))]
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

// RELEASE, Windows: the direct `win_creds` shim (R5) writing with
// `CRED_PERSIST_LOCAL_MACHINE` instead of keyring's hardcoded ENTERPRISE. Entry naming and
// blob encoding are byte-compatible with keyring, so entries written by either are readable.
#[cfg(all(not(debug_assertions), target_os = "windows"))]
fn store_get(account: &str) -> Result<Option<String>, String> {
    win_creds::get(account)
}

#[cfg(debug_assertions)]
fn store_set(account: &str, value: &str) -> Result<(), String> {
    let mut map = dev_read_map()?;
    map.insert(account.to_string(), value.to_string());
    dev_write_map(&map)
}

#[cfg(all(not(debug_assertions), not(target_os = "windows")))]
fn store_set(account: &str, value: &str) -> Result<(), String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account)
        .map_err(|e| e.to_string())?
        .set_password(value)
        .map_err(|e| e.to_string())
}

#[cfg(all(not(debug_assertions), target_os = "windows"))]
fn store_set(account: &str, value: &str) -> Result<(), String> {
    win_creds::set(account, value)
}

#[cfg(debug_assertions)]
fn store_delete(account: &str, reason: &str) -> Result<(), String> {
    tracing::info!(entry = %account, reason, "sync: credential delete (dev-file)");
    let mut map = dev_read_map()?;
    map.remove(account);
    dev_write_map(&map)
}

#[cfg(all(not(debug_assertions), not(target_os = "windows")))]
fn store_delete(account: &str, reason: &str) -> Result<(), String> {
    tracing::info!(entry = %account, reason, "sync: credential delete (keyring)");
    match keyring::Entry::new(KEYCHAIN_SERVICE, account)
        .map_err(|e| e.to_string())?
        .delete_credential()
    {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(all(not(debug_assertions), target_os = "windows"))]
fn store_delete(account: &str, reason: &str) -> Result<(), String> {
    win_creds::delete(account, reason)
}

// ----- Windows credential shim (R5) — wire-format helpers + FFI backend -----
//
// `keyring` 3.x's Windows backend hardcodes `Persist: CRED_PERSIST_ENTERPRISE`
// (keyring-3.6.3/src/windows.rs:246). ENTERPRISE credentials are roamable and, on some
// account/domain configurations, have been observed NOT to survive a logoff — which silently
// destroys the owner's session-wrapping key across restarts, forcing a fresh sign-in on every
// launch. We instead write the wrap-key + identity entries DIRECTLY with
// `CRED_PERSIST_LOCAL_MACHINE`, stored on the local machine and durable across logon sessions.
// `keyring` is kept for the macOS `apple-native` path (no such issue, no blob-size cap).
//
// Wire compatibility with `keyring` is PRESERVED so any pre-existing entry stays readable:
//   - target name = `{account}.{service}` — keyring's `{username}.{service}` convention
//     (windows.rs:378), e.g. `session-key-v1.dev.yapstack.app.sync`
//   - `UserName`   = account
//   - blob        = the value as little-endian UTF-16 with no NUL — byte-identical to keyring's
//     password encoding (windows.rs:86-88, extracted at 421-434)
//   - `Type`       = `CRED_TYPE_GENERIC`
// The ONLY behavioural change versus keyring is the persistence class.
//
// The pure wire-format transforms below are NOT cfg-gated so they compile and are unit-tested
// on every platform (they are the compatibility-critical part). Only the `win_creds` FFI glue
// is Windows-only; it cannot be compiled on this macOS host (a transitive C build-script,
// `ring`, needs the Windows SDK), so that glue is verified by construction against the
// keyring-3.6.3 + windows-sys 0.60 API and must be compiled by the Windows CI build.

/// keyring's Windows target-name convention (windows.rs:378): `{account}.{service}`.
fn cred_target_name(account: &str, service: &str) -> String {
    format!("{account}.{service}")
}

/// Encode a value as keyring's credential blob: little-endian UTF-16, no NUL terminator
/// (windows.rs:86-88).
fn cred_encode_blob(value: &str) -> Vec<u8> {
    value.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Decode a keyring credential blob (little-endian UTF-16) back to a `String`. Trailing odd
/// byte (should never occur for a keyring-written blob) is ignored via `chunks_exact`.
fn cred_decode_blob(bytes: &[u8]) -> String {
    let u16s: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u16s)
}

#[cfg(all(not(debug_assertions), target_os = "windows"))]
mod win_creds {
    use super::{cred_decode_blob, cred_encode_blob, cred_target_name, KEYCHAIN_SERVICE};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND, FILETIME};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    /// UTF-16 with a trailing NUL, for the wide C-string fields (`TargetName` / `UserName`).
    fn to_wstr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Read one entry. `ERROR_NOT_FOUND` → `Ok(None)` (parity with keyring's `NoEntry`); any
    /// other Win32 failure surfaces as an `Err` naming the code (no secret material).
    pub(super) fn get(account: &str) -> Result<Option<String>, String> {
        let target = to_wstr(&cred_target_name(account, KEYCHAIN_SERVICE));
        let mut p_cred: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `target` is a valid NUL-terminated wide string; on success CredReadW
        // allocates a CREDENTIALW we free with CredFree below.
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut p_cred) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_NOT_FOUND {
                // R6 item 4(b): "not found" is the exact tell of a credential that did NOT
                // survive the restart (the CRED_PERSIST_LOCAL_MACHINE persistence question).
                // Name only — never the value.
                tracing::info!(entry = %account, "win_creds: read (not found)");
                return Ok(None);
            }
            tracing::warn!(entry = %account, "win_creds: read failed (win32 error {err})");
            return Err(format!("CredReadW failed (win32 error {err})"));
        }
        // SAFETY: CredReadW returned success → p_cred is a valid, non-null allocation.
        let cred = unsafe { &*p_cred };
        let blob_len = cred.CredentialBlobSize as usize;
        let value = if blob_len == 0 || cred.CredentialBlob.is_null() {
            String::new()
        } else {
            // SAFETY: the blob is `blob_len` bytes at CredentialBlob for the lifetime of the
            // allocation (until CredFree).
            let bytes = unsafe { std::slice::from_raw_parts(cred.CredentialBlob, blob_len) };
            cred_decode_blob(bytes)
        };
        // SAFETY: p_cred was allocated by CredReadW and has not been freed yet.
        unsafe { CredFree(p_cred as *const core::ffi::c_void) };
        // Byte-length of the credential BLOB only — never the wrapping key itself.
        tracing::info!(
            entry = %account,
            blob_bytes = blob_len,
            "win_creds: read (found)"
        );
        Ok(Some(value))
    }

    /// Write (create or replace) one entry with `CRED_PERSIST_LOCAL_MACHINE`.
    pub(super) fn set(account: &str, value: &str) -> Result<(), String> {
        let mut target = to_wstr(&cred_target_name(account, KEYCHAIN_SERVICE));
        let mut username = to_wstr(account);
        let mut blob = cred_encode_blob(value);
        let mut cred = CREDENTIALW {
            Flags: CRED_FLAGS::default(),
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: std::ptr::null_mut(),
            LastWritten: FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: username.as_mut_ptr(),
        };
        // SAFETY: all pointer fields reference the local buffers above, which outlive the call.
        let ok = unsafe { CredWriteW(&mut cred as *const CREDENTIALW, 0) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            tracing::warn!(entry = %account, "win_creds: write failed (win32 error {err})");
            return Err(format!("CredWriteW failed (win32 error {err})"));
        }
        // R6 item 4(b): entry name + BLOB byte-length only (never the value). Confirms the
        // write landed with CRED_PERSIST_LOCAL_MACHINE so a next-boot "not found" read
        // pinpoints a persistence failure rather than a never-written entry.
        tracing::info!(
            entry = %account,
            blob_bytes = blob.len(),
            "win_creds: write (LOCAL_MACHINE)"
        );
        Ok(())
    }

    /// Delete one entry. `ERROR_NOT_FOUND` is a no-op success (parity with keyring's `NoEntry`).
    /// `reason` (R10 fix 5) names WHY the credential is being removed — logged at the delete
    /// site so an audit of the credential lifecycle can confirm the only production reasons are
    /// `user_sign_out` and `overwrite`. Never logs the value.
    pub(super) fn delete(account: &str, reason: &str) -> Result<(), String> {
        let target = to_wstr(&cred_target_name(account, KEYCHAIN_SERVICE));
        // SAFETY: `target` is a valid NUL-terminated wide string.
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_NOT_FOUND {
                tracing::info!(entry = %account, reason, "win_creds: delete (already absent)");
                return Ok(());
            }
            tracing::warn!(entry = %account, reason, "win_creds: delete failed (win32 error {err})");
            return Err(format!("CredDeleteW failed (win32 error {err})"));
        }
        tracing::info!(entry = %account, reason, "win_creds: delete (removed)");
        Ok(())
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
    /// Delete the wrapping-key entry. `reason` (R10 fix 5) names WHY — only ever
    /// `user_sign_out` or `overwrite` in production; logged at the backend delete site.
    fn delete_key(&self, reason: &str) -> Result<(), String>;
}

struct KeychainSessionKeyStore;
impl SessionKeyStore for KeychainSessionKeyStore {
    fn get_key(&self) -> Result<Option<String>, String> {
        store_get(KEYCHAIN_ACCOUNT_SESSION_KEY)
    }
    fn set_key(&self, b64: &str) -> Result<(), String> {
        store_set(KEYCHAIN_ACCOUNT_SESSION_KEY, b64)
    }
    fn delete_key(&self, reason: &str) -> Result<(), String> {
        store_delete(KEYCHAIN_ACCOUNT_SESSION_KEY, reason)
    }
}

fn read_session_blob(path: &Path) -> Result<Option<Vec<u8>>, String> {
    // R6 item 4(b): make the sealed-session read outcome visible on disk — path + result +
    // ciphertext byte-length only (no plaintext, no key). A Windows boot can no longer be
    // silent about whether the file was even found, and the logged path is the direct check
    // for the write/read path-mismatch hypothesis (4c).
    match std::fs::read(path) {
        Ok(b) => {
            tracing::info!(
                sealed_path = %path.display(),
                bytes = b.len(),
                "sync: sealed session file read (found)"
            );
            Ok(Some(b))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                sealed_path = %path.display(),
                "sync: sealed session file read (not found)"
            );
            Ok(None)
        }
        Err(e) => {
            tracing::warn!(
                sealed_path = %path.display(),
                "sync: sealed session file read failed: {e}"
            );
            Err(e.to_string())
        }
    }
}

fn write_session_blob(path: &Path, blob: &[u8]) -> Result<(), String> {
    tracing::info!(
        sealed_path = %path.display(),
        bytes = blob.len(),
        "sync: sealed session file write"
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Write-then-rename so a crash mid-write never leaves a torn FINAL file that would
    // decrypt-fail. R10.1 N1 fix 2: the tmp name is UNIQUE per write (`<final>.tmp.<pid>.<seq>`)
    // rather than a single fixed `enc.tmp`. `session_store_lock` already serializes writers, but
    // a private tmp per write means even a caller that somehow bypassed the lock cannot interleave
    // truncate/write on ONE shared tmp and publish a torn blob → next-boot AEAD failure. A crashed
    // write leaves a harmless stray tmp (never the final file); `cleanup_stale_session_tmp` sweeps
    // it on the next successful write.
    let seq = SESSION_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = session_tmp_path(path, seq);
    std::fs::write(&tmp, blob).map_err(|e| e.to_string())?;
    // Owner-only permissions on unix: the file holds the sealed session ciphertext, and even
    // ciphertext should not be world-readable (defence in depth against local snooping /
    // offline attack surface). Applied to the TEMP file too so there is never a window where
    // the pre-rename file is broader than 0600. Windows inherits the user-scoped AppData ACL,
    // so this is a unix-only no-op elsewhere.
    set_owner_only(&tmp)?;
    // R10 fix 4 — A3.1 journal discipline: fsync the temp file so its BYTES are durable on
    // disk BEFORE the rename publishes it. Without this, a crash/power-loss after the rename
    // could leave a zero-length or torn file that decrypt-fails, degrading a signed-in user to
    // signed-out on the next boot (the exact durability hole behind the session-loss reports).
    fsync_file(&tmp).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    // rename preserves the mode; re-assert on the final path in case it pre-existed with a
    // broader mode (e.g. an older build wrote it before this hardening landed).
    set_owner_only(path)?;
    // fsync the parent directory so the rename itself (the directory entry) is durable — a
    // crash after this point can never lose the newly-published session file.
    if let Some(parent) = path.parent() {
        fsync_dir(parent).map_err(|e| e.to_string())?;
    }
    // R10.1 N1 fix 2: best-effort sweep of stray tmp files (`<final>.tmp.*`) left by a prior
    // crashed write. Runs on a SUCCESSFUL write while holding `session_store_lock`, so no live
    // writer owns a tmp here — any match is pure litter. Errors are ignored; cleanup must never
    // fail an otherwise-durable write.
    cleanup_stale_session_tmp(path);
    Ok(())
}

/// Monotonic per-write counter feeding the unique session tmp name (R10.1 N1). Combined with
/// the process id it makes every in-flight session write own a private tmp file, so two writers
/// can never collide on one tmp even if the serializing lock were bypassed.
static SESSION_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A UNIQUE tmp path for one session write: the FINAL file name + `.tmp.<pid>.<seq>` (R10.1
/// N1). `with_file_name` preserves the whole `sync-session.enc` name (not just its stem) so the
/// tmp is unambiguously a sibling of — and prefixed by — the final file, which is exactly what
/// `cleanup_stale_session_tmp` matches on.
fn session_tmp_path(final_path: &Path, seq: u64) -> PathBuf {
    let mut name = final_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| SESSION_ENC_FILENAME.into());
    name.push(format!(".tmp.{}.{}", std::process::id(), seq));
    final_path.with_file_name(name)
}

/// Best-effort removal of leftover session tmp files (`<final>.tmp.*`) from prior crashed
/// writes (R10.1 N1). Called on a SUCCESSFUL write under `session_store_lock`, so no live writer
/// owns a tmp; every match is stale litter (a crash between write and rename). All errors are
/// swallowed — cleanup is hygiene, never a reason to fail a durable write.
fn cleanup_stale_session_tmp(final_path: &Path) {
    let Some(dir) = final_path.parent() else {
        return;
    };
    let Some(final_name) = final_path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let prefix = format!("{final_name}.tmp.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Serializes ALL wrapped-session store mutations (`store_session_wrapped` +
/// `clear_session_wrapped`) so the drain's token-refresh write can never interleave with a
/// UI-driven sign-in / enable / join write — or a sign-out — on the SAME sealed file +
/// wrapping-key pair (Judge R10 N1). Without it, two writers could publish a torn final file or
/// a file/key pair sealed under mismatched keys → AEAD open fails next boot → the observed
/// signed-out session loss. Poison-tolerant (same idiom as `cred_cache()`): a panic mid-write
/// must not wedge every future session write. This lock guards ONLY the store mutation; the
/// in-memory cache has its own `RwLock`, and the two are always taken in disjoint scopes (no
/// nesting), so there is no lock-ordering hazard.
fn session_store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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

fn remove_session_blob(path: &Path, reason: &str) -> Result<(), String> {
    // R10 fix 5: name WHY the sealed file is being removed. After fix 1 the only production
    // caller is sign-out (`user_sign_out`); no read/boot path removes the file. No secrets.
    tracing::info!(sealed_path = %path.display(), reason, "sync: sealed session file remove");
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
            let (key, expected_epoch) = match decode_wrap_entry(&entry_str) {
                Ok(v) => v,
                // decode-fail arm: the wrapping-key ENTRY exists but is unparseable (e.g. a
                // stale round-2 bare-base64 value from a pre-`WrapKeyEntry` build). R10 fix 1:
                // do NOT delete on a read/boot path. A transient store returning a stale/garbled
                // value is indistinguishable from a truly corrupt entry here, and deleting would
                // destroy a recoverable component. PRESERVE it and degrade signed-out; the next
                // successful sign-in overwrites the entry (`store_session_wrapped`, the ONLY
                // non-sign-out replacement). No secret is logged.
                Err(_) => {
                    tracing::warn!(
                        "sync boot: session wrapping-key entry present but undecodable (decode-fail) — PRESERVING entry + file (no deletion on a read path), degrading signed-out; a fresh sign-in overwrites it"
                    );
                    return Ok(None);
                }
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
                    // The AEAD opened but the plaintext is not a current-epoch Session (bad
                    // JSON or an epoch skew). Unusable, but we do NOT destroy the file: a fresh
                    // sign-in overwrites it. Log the arm (no secrets).
                    Ok(_) | Err(_) => {
                        tracing::warn!(
                            "sync boot: session opened but plaintext not a current-epoch Session (open-fail) — degrading signed-out, file preserved"
                        );
                        Ok(None)
                    }
                },
                // open-fail arm: AEAD open failed (wrong key / tamper / truncation / epoch
                // rollback). We do NOT delete: a genuinely tampered file is unrecoverable and
                // the next sign-in overwrites it, but a TRANSIENT keychain read returning a
                // stale/other key must never destroy the session file. Log the arm (no secrets).
                Err(_) => {
                    tracing::warn!(
                        "sync boot: session file AEAD open failed (open-fail) — degrading signed-out, file preserved"
                    );
                    Ok(None)
                }
            }
        }
        // file-missing arm: a wrapping key with no file. R10 fix 1: do NOT delete the key. A
        // transiently-missing file (an unsynced/rolled-back write, a path hiccup, AV quarantine)
        // presents identically to a truly orphaned key, and deleting the key would escalate a
        // recoverable transient into permanent session destruction. PRESERVE the key and degrade
        // signed-out; if the file returns next boot the session recovers, and a fresh sign-in
        // overwrites the key harmlessly otherwise. Log exactly what was found/missing.
        (Some(_), None) => {
            tracing::warn!(
                "sync boot: wrapping-key entry present but session file MISSING (file-missing) — PRESERVING key (possible transient/unsynced file), degrading signed-out; no deletion on a read path"
            );
            Ok(None)
        }
        // entry-missing arm: a session file with NO wrapping-key entry. Previously we DELETED
        // the file here — but a transient keychain miss (the exact Windows-persistence failure
        // R5 fixes) presents identically to a truly orphaned file, and deleting destroys the
        // ONLY recoverable artifact. Do NOT delete: leave the file, log, degrade signed-out.
        // If the entry comes back next boot (transient miss resolved), the session recovers; if
        // it was genuinely orphaned, a fresh sign-in overwrites the file harmlessly.
        (None, Some(_)) => {
            tracing::warn!(
                "sync boot: session file present but wrapping-key entry missing (entry-missing) — PRESERVING file (possible transient keychain miss), degrading signed-out"
            );
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
    // R10.1 N1 fix 1: serialize the file+wrapping-key mutation against every other session
    // writer (a racing sign-out, or the drain's token-refresh write) so the sealed file and its
    // key entry are always committed as one consistent pair.
    let _guard = session_store_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let json = serde_json::to_string(s).map_err(|e| e.to_string())?;
    // Reuse the existing wrapping key if present and valid; otherwise mint a fresh one. (A
    // corrupt existing entry would strand the new ciphertext, so we replace it.) The RECORDED
    // epoch is always overwritten below with THIS session's epoch.
    let key = match ks.get_key()? {
        Some(entry_str) => {
            // R10 fix 5: a fresh sign-in over an existing wrapped session REPLACES both
            // components (the sealed file via atomic rename below, the wrapping-key entry via
            // `set_key`). This is the ONLY non-sign-out path allowed to remove/replace either
            // component; name the reason so a credential-lifecycle audit sees exactly the two
            // production reasons (`user_sign_out` + `overwrite`). No secret is logged.
            tracing::info!(
                reason = DELETE_REASON_OVERWRITE,
                "sync: overwriting existing wrapped session (fresh sign-in)"
            );
            decode_wrap_entry(&entry_str)
                .map(|(k, _prev_epoch)| k)
                .unwrap_or_else(|_| new_wrap_key())
        }
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
    // R10.1 N1 fix 1: same serialization as `store_session_wrapped` — a sign-out must not
    // interleave with a concurrent token-refresh/sign-in write on the shared file+key pair.
    let _guard = session_store_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _ = remove_session_blob(enc_path, DELETE_REASON_SIGN_OUT);
    let _ = ks.delete_key(DELETE_REASON_SIGN_OUT);
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
    // R10 fix 2: an unresolved store path is a READ FAILURE surfaced as `Err` (never a
    // %TEMP% fallback) — `ensure_cache_loaded` then leaves the cache UNLOADED so a later
    // access retries, rather than caching a bogus signed-out for the process lifetime.
    let path = session_enc_path()?;
    load_session_wrapped(&KeychainSessionKeyStore, &path)
}

#[cfg(debug_assertions)]
fn store_session_to_store(s: &Session) -> Result<(), String> {
    let json = serde_json::to_string(s).map_err(|e| e.to_string())?;
    store_set(KEYCHAIN_ACCOUNT, &json)
}

#[cfg(not(debug_assertions))]
fn store_session_to_store(s: &Session) -> Result<(), String> {
    // R10 fix 2: if the store path is unresolved, sign-in FAILS LOUDLY here (the `?`) rather
    // than sealing the real session into %TEMP%.
    let path = session_enc_path()?;
    store_session_wrapped(&KeychainSessionKeyStore, &path, s)
}

#[cfg(debug_assertions)]
fn clear_session_from_store() -> Result<(), String> {
    store_delete(KEYCHAIN_ACCOUNT, DELETE_REASON_SIGN_OUT)
}

#[cfg(not(debug_assertions))]
fn clear_session_from_store() -> Result<(), String> {
    let path = session_enc_path()?;
    clear_session_wrapped(&KeychainSessionKeyStore, &path)
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

/// Static label of the credential BACKEND compiled into this build — the home of the
/// session wrapping key + identity entry. Diagnostics only; no secrets. (The sealed session
/// FILE path is logged separately via `session_enc_path()`.)
fn cred_store_backend() -> &'static str {
    #[cfg(debug_assertions)]
    {
        "dev-file"
    }
    #[cfg(all(not(debug_assertions), target_os = "windows"))]
    {
        "win_creds(CRED_PERSIST_LOCAL_MACHINE)"
    }
    #[cfg(all(not(debug_assertions), not(target_os = "windows")))]
    {
        "keyring"
    }
}

/// Set once the once-per-boot trace reported a session-read ERROR (R10.1 N2). A later
/// successful load reads this to decide whether to emit the one-shot recovery follow-up, so the
/// `Once`-guarded boot trace's permanent `error` verdict is honestly corrected on recovery.
static BOOT_TRACE_WAS_ERROR: AtomicBool = AtomicBool::new(false);

/// Pure decision for the R10.1 N2 recovery line: emit the one-shot "session recovered on
/// retry" trace IFF the boot trace recorded a read error, a later load has now SUCCEEDED, and
/// it has not already been emitted. Pure so the error-then-success honesty is unit-testable
/// without touching the process-global statics.
fn should_emit_session_recovered(
    boot_was_error: bool,
    load_succeeded: bool,
    already_emitted: bool,
) -> bool {
    boot_was_error && load_succeeded && !already_emitted
}

/// Emit the R10.1 N2 recovery follow-up AT MOST ONCE: when the boot trace recorded a read
/// ERROR and a subsequent load has now SUCCEEDED, log that the session recovered on retry —
/// the honest correction of the permanent `session_present="error"` verdict the `Once`-guarded
/// boot trace would otherwise leave. No secrets: a single presence boolean.
fn maybe_log_session_recovered(load_succeeded: bool) {
    static EMITTED: AtomicBool = AtomicBool::new(false);
    if !should_emit_session_recovered(
        BOOT_TRACE_WAS_ERROR.load(Ordering::Relaxed),
        load_succeeded,
        EMITTED.load(Ordering::Relaxed),
    ) {
        return;
    }
    // One-shot even under a race between two retrying threads: only the winner of the swap logs.
    if EMITTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let session_present = cred_cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .session
        .is_some();
    tracing::info!(
        session_present,
        "sync boot: session recovered on retry (an earlier boot trace reported a read error)"
    );
}

/// Emit the boot session-load outcome EXACTLY ONCE per process, on the first session access.
///
/// R6 item 4(a): the previous trace lived inline in `ensure_cache_loaded`, PAST its
/// `if loaded { return }` early-exit. The `loaded` flag is also flipped silently by the
/// write helpers (`store_session` / `store_identity` / `clear_session`), so if the cache was
/// warmed by a write — or the boot read raced anything — the trace was skipped and the boot
/// outcome was never surfaced (the Round-5 Windows silence). This helper is gated by its own
/// `Once`, INDEPENDENT of the cache flag, so the first session touch of the boot always logs
/// the outcome once — including the RESOLVED sealed-session path and the backend, which is
/// the smoking-gun diagnostic for the write/read path-mismatch hypothesis (4c). No secrets:
/// presence booleans + a filesystem path + a static backend label only.
fn log_session_boot_trace(read_error: bool) {
    static BOOT_TRACE: Once = Once::new();
    BOOT_TRACE.call_once(|| {
        // R10.1 N2: remember that the FIRST boot trace recorded a read error, so a later
        // successful load can emit the honest "recovered on retry" follow-up rather than
        // leaving the permanent `session_present="error"` verdict as the last word.
        if read_error {
            BOOT_TRACE_WAS_ERROR.store(true, Ordering::Relaxed);
        }
        let (session_some, identity_present) = {
            let g = cred_cache().read().unwrap_or_else(|e| e.into_inner());
            (g.session.is_some(), g.identity.is_some())
        };
        // R10 fix 3: a boot READ error is reported DISTINCTLY from a genuine signed-out — an
        // Err (transient store failure) is "error", NOT "false". "false" means the store was
        // read cleanly and no session exists; "error" means the read failed and the cache was
        // left unloaded for a later retry. Conflating them is the swallow this fix removes.
        let session_present: &str = if read_error {
            "error"
        } else if session_some {
            "true"
        } else {
            "false"
        };
        let sealed = match session_enc_path() {
            Ok(p) => p.display().to_string(),
            Err(e) => format!("<unresolved: {e}>"),
        };
        tracing::info!(
            session_present,
            identity_present,
            store_backend = cred_store_backend(),
            sealed_path = %sealed,
            store_dir_resolved = SESSION_STORE_DIR.get().is_some(),
            "sync boot: credential cache warm-up complete"
        );
    });
}

/// Populate the cache from the backing store ONCE. Subsequent calls are a cheap flag check
/// and never touch the store (hence never the keychain). Safe to call from any thread. The
/// first call of the boot (whichever path reaches it) emits the once-per-boot session trace.
fn ensure_cache_loaded() {
    if cred_cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .loaded
    {
        // Cache already warm (e.g. a sign-in write beat the boot read). Still guarantee the
        // once-per-boot trace fires — it is idempotent and reads the current cache state.
        log_session_boot_trace(false);
        return;
    }
    // R10 fix 3: distinguish a genuine signed-out (`Ok(None)` → cache it) from a store READ
    // FAILURE (`Err` → do NOT mark loaded, so a later poll-driven access retries). Previously
    // `.ok().flatten()` collapsed both into a cached signed-out for the whole process lifetime,
    // so one transient read wedged the session as signed-out until the next launch (stopping
    // the drain — the incomplete-pull driver).
    let (session, identity, loaded_ok) =
        resolve_cache_from_reads(load_session_from_store(), load_identity_from_store());
    {
        let mut g = cred_cache().write().unwrap_or_else(|e| e.into_inner());
        if !g.loaded {
            g.session = session;
            g.identity = identity;
            // Only mark the cache loaded when BOTH reads succeeded. On a read error we leave
            // `loaded = false`; the next access (polled ~5s, no hot loop) retries. This is
            // deliberately bounded — accesses are poll-driven, so there is no retry storm.
            g.loaded = loaded_ok;
        }
    }
    // Log AFTER the cache is populated (and the write lock released) so the trace reflects the
    // resolved state, exactly once per boot — reporting a read error distinctly.
    log_session_boot_trace(!loaded_ok);
    // R10.1 N2: if an earlier boot trace reported a read error and THIS retry succeeded, emit
    // the one-shot recovery follow-up so the log does not leave a stale "error" as the last word.
    // Only the genuine read-retry path (this branch) can recover; a sign-in write warming the
    // cache is a fresh sign-in, not a recovery, and correctly does not trigger the line.
    maybe_log_session_recovered(loaded_ok);
}

/// Decide the cache state from the two backing-store reads (R10 fix 3). A clean `Ok(None)` is
/// a genuine signed-out and IS cached; an `Err` is a transient read failure that must NOT mark
/// the cache loaded (so a later access retries). Returns `(session, identity, loaded_ok)` where
/// `loaded_ok` is false iff either read failed. Pure + logging-only, so the swallow-vs-retry
/// decision is unit-testable without the real keychain.
fn resolve_cache_from_reads(
    session_read: Result<Option<Session>, String>,
    identity_read: Result<Option<DeviceIdentity>, String>,
) -> (Option<Session>, Option<DeviceIdentity>, bool) {
    let read_failed = session_read.is_err() || identity_read.is_err();
    let (session, session_err) = match session_read {
        Ok(s) => (s, None),
        Err(e) => (None, Some(e)),
    };
    let (identity, identity_err) = match identity_read {
        Ok(i) => (i, None),
        Err(e) => (None, Some(e)),
    };
    // R10.1 N3: this resolver runs on every ~5s poll while the cache is unloaded, so an
    // UNCONDITIONAL warn on a persistently-failing store grows the log without bound. Gate it to
    // the transition edges only — log the FIRST failure and the recovery, stay silent in between
    // — the same discipline as `fail_surface_step` (R2 drain logging). The tuple result is
    // unchanged, so the swallow-vs-retry decision remains exactly as before.
    let mut prev = SESSION_READ_PREV_FAILED.load(Ordering::Relaxed);
    let edge = read_log_step(&mut prev, read_failed);
    SESSION_READ_PREV_FAILED.store(prev, Ordering::Relaxed);
    match edge {
        ReadLogEdge::Failure => tracing::warn!(
            session_err = session_err.as_deref().unwrap_or("-"),
            identity_err = identity_err.as_deref().unwrap_or("-"),
            "sync boot: credential store read FAILED — leaving cache unloaded so a later access retries (NOT caching signed-out); throttled: silent until it recovers"
        ),
        ReadLogEdge::Recovery => tracing::info!(
            "sync boot: credential store read recovered — cache now loads normally (was previously failing)"
        ),
        ReadLogEdge::Silent => {}
    }
    (session, identity, !read_failed)
}

/// Transition state for the R10.1 N3 read-failure log throttle: `true` once the last resolve
/// saw a failed store read. Only the OK→FAIL and FAIL→OK edges log; the steady-failing polls in
/// between are silent (the unbounded log growth this fixes).
static SESSION_READ_PREV_FAILED: AtomicBool = AtomicBool::new(false);

/// Which (if any) log line a store-read outcome warrants, given the previous failed-state.
#[derive(Debug, PartialEq, Eq)]
enum ReadLogEdge {
    Silent,
    Failure,
    Recovery,
}

/// Pure edge detector (shared with the tests): log the FIRST failure (OK→FAIL) and the
/// recovery (FAIL→OK), stay silent while the state is unchanged. Advances `prev` in place —
/// the same discipline as `fail_surface_step` for the drain.
fn read_log_step(prev_failed: &mut bool, now_failed: bool) -> ReadLogEdge {
    let edge = match (*prev_failed, now_failed) {
        (false, true) => ReadLogEdge::Failure,
        (true, false) => ReadLogEdge::Recovery,
        _ => ReadLogEdge::Silent,
    };
    *prev_failed = now_failed;
    edge
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

/// This device's fingerprint (§7.5.5) as presented in `SyncStatusDto.device_fingerprint`
/// (the `session.device_fingerprint` field the status DTO clones, `build_status_dto`),
/// the public base32 hash of the roster identity — **NOT** any key/token material.
///
/// Served from the warmed in-memory credential cache via [`load_session`] (T020), so it is
/// cheap and non-blocking at boot: once `init_credential_store` warms the cache, a read
/// never hits the keychain. Returns `None` when sync was never configured (no session),
/// when the session is unreadable, or when a legacy session predates fingerprint
/// attribution — i.e. exactly the ownership-ambiguous case where the boot orphan sweep
/// must NOT guess (LIVE_SESSION_STATE.md "Sweep rules"). Because the session entry is
/// deleted on sign-out, a credential-clear over a retained CRR DB also yields `None`,
/// keeping that case foreign-safe rather than mis-attributing peer rows to a stale self.
pub fn current_device_fingerprint() -> Option<String> {
    load_session()
        .ok()
        .flatten()
        .and_then(|s| s.device_fingerprint)
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
    reset_audio_lane();
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
    /// R12: changesets this device is still behind the last-known relay tip on the PULL side
    /// (0 when caught up or the tip is unknown). Drives the "catching up (N to go)" copy. A
    /// device is honestly "up to date" only when this is 0 AND the outbox is empty.
    pull_behind: u64,
    /// S2 — audio upload lane (DISTINCT from changeset sync). Blobs (recordings) still to
    /// seal+upload to the relay across both priorities (0 == every local recording is backed
    /// up). Surfaced under the "audio-upload" label in the panel; never merged with the
    /// changeset backlog.
    audio_upload_outstanding: u64,
    /// Of `audio_upload_outstanding`, the low-priority backfill of the existing library (D9).
    audio_backfill_outstanding: u64,
    /// Audio blobs the uploader marked `failed` (needs attention; app-start + manual retry).
    audio_upload_failed: u64,
    /// Cumulative recordings the relay confirms it holds for this device.
    audio_uploaded_total: u64,
    /// Whether the one-time idempotent backfill walk has completed on this device (D9).
    audio_backfill_complete: bool,
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
    // Same out-of-band ALTER pass as the live in-place cutover: ensure the seed
    // snapshot produced from this copy carries the runtime-patched columns as
    // CRR-tracked (not absent), so a peer joining via snapshot never quarantines
    // speaker_id / chat_messages column changes. Idempotent (schema.rs:329,332).
    schema::apply_out_of_band_alters(conn).map_err(|e| format!("apply_out_of_band_alters: {e}"))?;
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

// ----- Enable-time CRR cutover (Option A′ final stage, A3) -----
//
// The copy architecture is gone: `sync_enable` (and the already-enabled migration on
// drain start) turn the LIVE `yapstack.db` INTO the cr-sqlite CRR database, so the
// drain captures every write the UI makes. The proven A1-spike sequence is:
//   1. idempotency gate  2. VACUUM INTO live→staging  3. CRRify staging (crr_migrate +
//   cascade_gc + enforce_uniqueness + mark_prepared + reset watermarks + checkpoint)
//   4. quiesce + drop ALL live handles  5. remove stale sidecars  6. atomic rename
//   live→backup, staging→live  7. reopen pool + assert row counts == pre-swap
//   8. on ANY failure: roll back (backup→live) and report verbatim.
// Extension-less writes to CRR tables fail loudly (never corrupt) — the safety net.

/// The pre-cutover backup of the live DB — the escape hatch (§5). Kept forever;
/// NEVER auto-deleted. Restore by copying it back over `yapstack.db` with sync off.
fn backup_db_path(live_db: &Path) -> PathBuf {
    live_db.with_file_name("yapstack.db.pre-sync-backup")
}

/// Scratch path for the CRRified staging copy built during a cutover.
fn cutover_staging_path(live_db: &Path) -> PathBuf {
    live_db.with_file_name("yapstack.db.crr-staging")
}

// ----- Durable cutover journal + crash-safe boot recovery (F1) -----
//
// The cutover swaps two files with two non-atomic `rename`s (live→backup, then
// staging→live). A crash BETWEEN them leaves the live path MISSING, which — before
// this journal — let `DbService::open` create an empty DB on next boot and the
// auto-cutover then delete the leftovers, silently destroying the library.
//
// Fix: write a durable journal BEFORE the first rename and fsync the directory after
// each rename. On next boot, `recover_interrupted_cutover` runs BEFORE `DbService::open`
// and derives the correct outcome from the JOURNAL (not from mere file presence),
// completing or rolling back the interrupted swap. The journal is removed only after
// the swap is fully verified. Recovery is total: every reachable disk state has a
// defined outcome that never deletes the last copy of user data.

/// Path of the durable cutover journal. Its presence at boot means a cutover was
/// interrupted and must be recovered before the DB is opened.
fn cutover_journal_path(live_db: &Path) -> PathBuf {
    live_db.with_file_name("yapstack.db.cutover-journal")
}

const CUTOVER_JOURNAL_VERSION: u32 = 1;

/// Which non-atomic step the cutover had reached when the journal was last written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum CutoverPhase {
    /// Journal written; the first rename (live→backup) is NOT yet confirmed durable.
    SwapStarted,
    /// live→backup confirmed + dir fsynced; staging→live NOT yet confirmed.
    BackupMoved,
    /// staging→live confirmed + dir fsynced; only verify/cleanup remained.
    SwapCompleted,
}

/// The durable source of truth for cutover recovery. Content markers (pre-swap
/// per-table row counts + the live file size) let recovery validate a staging
/// candidate before completing the swap onto it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CutoverJournal {
    version: u32,
    phase: CutoverPhase,
    live_file: String,
    backup_file: String,
    staging_file: String,
    /// Per-user-table row counts captured from the ORIGINAL live DB before the swap.
    pre_counts: Vec<(String, i64)>,
    /// The original live file's size in bytes (a cheap secondary content marker).
    live_size: u64,
}

/// The bare file name of `p` (the journal records names, not absolute paths).
fn file_name_string(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// fsync a file's contents + metadata to stable storage.
///
/// The handle MUST be opened for WRITE. On Windows `sync_all` calls
/// `FlushFileBuffers`, which the kernel only honours on a handle holding
/// `GENERIC_WRITE`; a read-only handle returns `ERROR_ACCESS_DENIED` (os error 5).
/// On unix fsync on a read-only fd is legal, which is why a read-only open passed
/// every macOS test yet broke the live Windows cutover — do NOT "simplify" this back
/// to `File::open`. `write(true)` opens existing content without truncating.
fn fsync_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()
}

/// fsync a directory entry so a `rename`/create/remove within it is durable. On unix
/// we open+fsync the directory. On non-unix (Windows) std cannot fsync a directory;
/// this is a documented best-effort no-op — NTFS journals metadata and `MoveFileEx`
/// renames are atomic, and boot recovery re-derives truth from the journal regardless.
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// fsync the directory that holds the live DB (best-effort — logged, never fatal).
fn fsync_live_dir(live_db: &Path) {
    if let Some(dir) = live_db.parent() {
        let _ = fsync_dir(dir);
    }
}

/// Durably write (or overwrite) the cutover journal: write to a temp file, fsync it,
/// atomically rename it over the journal path, then fsync the directory.
fn write_journal(live_db: &Path, journal: &CutoverJournal) -> Result<(), String> {
    let path = cutover_journal_path(live_db);
    let mut tmp_os = path.clone().into_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    let bytes = serde_json::to_vec(journal).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    fsync_file(&tmp).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    fsync_live_dir(live_db);
    Ok(())
}

/// Read + parse the cutover journal, if one exists and is well-formed.
fn read_journal(live_db: &Path) -> Option<CutoverJournal> {
    let bytes = std::fs::read(cutover_journal_path(live_db)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove the cutover journal (its absence tells the next boot there is nothing to do).
fn remove_journal(live_db: &Path) {
    let _ = std::fs::remove_file(cutover_journal_path(live_db));
    fsync_live_dir(live_db);
}

/// Move a DB file (and any `-wal`/`-shm` sidecars) aside, NEVER deleting it (F1.3).
fn move_db_aside(from: &Path, to: &Path) {
    let _ = std::fs::rename(from, to);
    for ext in ["-wal", "-shm"] {
        let mut f = from.as_os_str().to_owned();
        f.push(ext);
        let mut t = to.as_os_str().to_owned();
        t.push(ext);
        let (fp, tp) = (PathBuf::from(f), PathBuf::from(t));
        if fp.exists() {
            let _ = std::fs::rename(&fp, &tp);
        }
    }
}

/// A monotonic-ish suffix (seconds since epoch) for a moved-aside backup filename.
fn timestamp_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Crash-safe copy of `src` INTO the live path, KEEPING `src` (used to restore a
/// backup we must preserve forever): stale live sidecars are cleared, the copy lands
/// via a temp file + atomic rename, and the directory is fsynced.
fn copy_into_live(src: &Path, live: &Path) -> std::io::Result<()> {
    remove_sidecars(live);
    let mut tmp_os = live.as_os_str().to_owned();
    tmp_os.push(".recover-tmp");
    let tmp = PathBuf::from(tmp_os);
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(src, &tmp)?;
    fsync_file(&tmp)?;
    std::fs::rename(&tmp, live)?;
    fsync_live_dir(live);
    Ok(())
}

/// Move `src` (a scratch staging file, safe to consume) INTO the live path atomically.
fn rename_into_live(src: &Path, live: &Path) -> std::io::Result<()> {
    remove_sidecars(live);
    std::fs::rename(src, live)?;
    fsync_live_dir(live);
    Ok(())
}

/// True if any cutover recovery artifact is present beside the live DB.
fn recovery_artifacts_present(live_db: &Path) -> bool {
    cutover_journal_path(live_db).exists()
        || backup_db_path(live_db).exists()
        || cutover_staging_path(live_db).exists()
}

/// True if the live DB holds zero rows in every SYNC table — the signature of a
/// freshly-auto-created DB (a genuine empty library, or the empty DB `DbService::open`
/// would create over an interrupted swap). SYNC tables (not all-user-tables) are used
/// deliberately: FTS `%_config` shadow tables always carry a version row, so an
/// all-tables check is never zero even on a brand-new DB. Unreadable/absent → treated
/// as empty (never cut over).
fn live_has_no_user_rows(live_db: &Path) -> bool {
    match synced_table_row_counts(live_db) {
        Ok(counts) => counts.iter().all(|(_, n)| *n == 0),
        Err(_) => true,
    }
}

/// Complete or roll back an interrupted cutover from its durable journal (F1.2).
///
/// MUST run at boot BEFORE `DbService::open`, which would otherwise create an empty
/// DB over a mid-swap state. Dead simple + total by construction: every reachable
/// disk state maps to a defined outcome that NEVER deletes the last copy of user data.
///
/// Decision table (live/backup/staging are the three DB paths; "staging valid" means
/// it exists and its per-user-table row counts match the journal's pre-swap marker):
///
/// ```text
/// NO journal:
///   live present                        -> normal boot, do nothing.
///   live MISSING, backup present        -> restore backup->live (copy; keep backup).
///   live MISSING, staging present only  -> restore staging->live (rename; only copy).
///   live MISSING, nothing present       -> genuinely fresh; do nothing.
/// journal SwapStarted, live present     -> first rename never landed; ORIGINAL data is
///                                          at live -> do nothing (drop journal).
/// journal SwapStarted(live missing) or BackupMoved:
///   live present                        -> second rename landed; swap done, keep backup.
///   live missing, staging valid         -> complete: rename staging->live.
///   live missing, staging bad, backup   -> roll back: restore backup->live.
///   live missing, nothing usable        -> leave as-is (should be unreachable).
/// journal SwapCompleted:
///   live present                        -> done; keep backup.
///   live missing, backup present        -> restore backup->live.
///   live missing, staging valid         -> complete: rename staging->live.
/// ```
///
/// In every branch the journal is removed at the end (the only thing deleted).
pub fn recover_interrupted_cutover(live_db: &Path) {
    let backup = backup_db_path(live_db);
    let staging = cutover_staging_path(live_db);

    match read_journal(live_db) {
        Some(journal) => {
            recover_from_journal(live_db, &backup, &staging, &journal);
            // The interrupted cutover is resolved: the journal is the ONLY thing deleted.
            remove_journal(live_db);
        }
        None => {
            // No journal. The only dangerous no-journal state is a MISSING live while a
            // recovery copy exists (e.g. the journal write itself was lost). NEVER let the
            // app proceed to create an empty live — restore the best available copy.
            if !live_db.exists() {
                if backup.exists() {
                    let _ = copy_into_live(&backup, live_db); // keep the backup (kept forever)
                } else if staging.exists() {
                    let _ = rename_into_live(&staging, live_db); // staging is the only copy
                }
                // else: genuinely fresh (no data anywhere) — nothing to recover.
            }
        }
    }
}

/// The journal-driven half of [`recover_interrupted_cutover`]. See its decision table.
fn recover_from_journal(live_db: &Path, backup: &Path, staging: &Path, journal: &CutoverJournal) {
    // "staging valid": it exists AND its user-table row counts match the pre-swap marker.
    let staging_valid = staging.exists()
        && all_user_table_row_counts(staging)
            .map(|c| c == journal.pre_counts)
            .unwrap_or(false);

    match journal.phase {
        // First rename never confirmed AND the original is still at live → nothing to do.
        CutoverPhase::SwapStarted if live_db.exists() => {
            tracing::info!(
                "sync recovery: interrupted cutover (SwapStarted) — live DB intact, nothing to restore"
            );
        }
        // SwapStarted-with-live-missing (crash before the phase advance) OR BackupMoved:
        // the first rename landed; the second did not confirm.
        CutoverPhase::SwapStarted | CutoverPhase::BackupMoved => {
            if live_db.exists() {
                // BackupMoved but live present → the second rename actually completed.
                tracing::info!(
                    "sync recovery: interrupted cutover — swap already completed; keeping backup"
                );
            } else if staging_valid {
                tracing::warn!("sync recovery: completing interrupted cutover (staging→live)");
                let _ = rename_into_live(staging, live_db);
            } else if backup.exists() {
                tracing::warn!(
                    "sync recovery: staging missing/invalid — rolling interrupted cutover back (backup→live)"
                );
                let _ = copy_into_live(backup, live_db);
            } else {
                tracing::error!(
                    "sync recovery: interrupted cutover with no usable copy of the live DB — leaving disk as-is"
                );
            }
        }
        // Second rename confirmed; only verify/cleanup remained.
        CutoverPhase::SwapCompleted => {
            if live_db.exists() {
                tracing::info!("sync recovery: interrupted cutover (SwapCompleted) — live present, keeping backup");
            } else if backup.exists() {
                tracing::warn!("sync recovery: live vanished post-swap — restoring backup→live");
                let _ = copy_into_live(backup, live_db);
            } else if staging_valid {
                let _ = rename_into_live(staging, live_db);
            }
        }
    }
}

/// True once the live DB is itself CRR-prepared at the current schema version (the
/// idempotency gate). Inspected via a read-only connection — extension-less reads on
/// a CRR DB are safe (A1 spike) and never create/mutate anything.
fn live_is_crr_prepared(live_db: &Path) -> bool {
    if !live_db.exists() {
        return false;
    }
    let Ok(conn) = Connection::open_with_flags(
        live_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return false;
    };
    let clock: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions__crsql_clock'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !clock {
        return false;
    }
    let ver: Option<i64> = conn
        .query_row(
            "SELECT schema_version FROM _yapstack_sync_prep LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    ver.unwrap_or(0) == SYNC_SCHEMA_VERSION as i64
}

/// Per-table row counts for every synced table (read-only). Used to assert the swap
/// preserved data exactly (step 7). Sorted for a stable comparison.
fn synced_table_row_counts(db: &Path) -> rusqlite::Result<Vec<(String, i64)>> {
    let conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut out = Vec::new();
    for t in schema::SYNC_TABLES {
        // A table may legitimately be absent on an old dev DB; treat as 0 rows.
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [t],
                |_| Ok(()),
            )
            .is_ok();
        let n: i64 = if exists {
            conn.query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| r.get(0))?
        } else {
            0
        };
        out.push((t.to_string(), n));
    }
    out.sort();
    Ok(out)
}

/// Per-user-table row counts (F5): EVERY table in `sqlite_master` except SQLite
/// internals (`sqlite_%`), our own underscore-prefixed bookkeeping (`_sqlx_migrations`,
/// `_yapstack_sync_%`), and cr-sqlite's bookkeeping/shadow tables (`crsql_%`,
/// `%__crsql_%`). This is the swap's data-preservation oracle — identical before and
/// after the cutover — and is broader than [`synced_table_row_counts`] (which counts
/// only the CRR tables) so a dropped/renamed local table is caught too. Sorted for a
/// stable comparison.
fn all_user_table_row_counts(db: &Path) -> rusqlite::Result<Vec<(String, i64)>> {
    let conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let names: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' \
               AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
               AND name NOT LIKE '\\_%' ESCAPE '\\' \
               AND name NOT LIKE 'crsql\\_%' ESCAPE '\\' \
               AND name NOT LIKE '%\\_\\_crsql\\_%' ESCAPE '\\'",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::new();
    for t in names {
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| r.get(0))?;
        out.push((t, n));
    }
    out.sort();
    Ok(out)
}

/// Remove a DB file plus its `-wal`/`-shm` sidecars (best-effort).
fn remove_db_files(db: &Path) {
    let _ = std::fs::remove_file(db);
    remove_sidecars(db);
}

/// Remove only the `-wal`/`-shm` sidecars for `db` (best-effort).
fn remove_sidecars(db: &Path) {
    for ext in ["-wal", "-shm"] {
        let mut s = db.as_os_str().to_owned();
        s.push(ext);
        let _ = std::fs::remove_file(PathBuf::from(s));
    }
}

/// Discard the legacy copy-architecture database (`yapstack.sync.db`) and its stale
/// `_yapstack_sync_meta` watermarks. Uniform-reset (§3): the pre-cutover copy on an
/// already-enabled device holds merged data that is discarded and re-pulled into the
/// now-CRR live DB (where it is finally visible). Idempotent / best-effort.
fn discard_legacy_sync_copy(live_db: &Path) {
    let legacy = sync_db_path(live_db);
    if legacy.exists() {
        tracing::info!(
            "sync: discarding legacy copy-architecture DB at {} (data re-captured/re-pulled into the CRR live DB)",
            legacy.display()
        );
    }
    remove_db_files(&legacy);
}

/// Test-only fault injection points for the cutover sequence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CutoverFault {
    None,
    /// Fail right after the VACUUM INTO (before any live handle is closed): the live
    /// DB is untouched and must be byte-identical, no backup produced.
    AfterVacuum,
    /// Fail after the staging→live rename + reopen (exercises the backup→live
    /// rollback that restores the byte-identical original live DB).
    AfterRename,
    /// Fail BETWEEN the two renames (live→backup done, staging→live not started),
    /// leaving the mid-swap state on disk so a test can drive boot recovery (F1).
    /// Does NOT roll back — the journal + `recover_interrupted_cutover` must.
    BetweenRenames,
}

/// Perform the enable-time CRR cutover on the LIVE DB. See the module section above
/// for the full sequence. Idempotent: a no-op (returns `Ok`) if the live DB is
/// already CRR-prepared. On ANY failure the live DB is restored byte-identically and
/// the verbatim error is returned; the app's DB pool is always left reopened.
fn perform_cutover(
    live_db: &Path,
    db_service: &crate::db_service::DbServiceState,
) -> Result<(), String> {
    cutover_with_fault(live_db, db_service, CutoverFault::None)
}

fn cutover_with_fault(
    live_db: &Path,
    db_service: &crate::db_service::DbServiceState,
    fault: CutoverFault,
) -> Result<(), String> {
    // Step 1: idempotency gate. An already-CRR live DB just needs its legacy copy gone.
    if live_is_crr_prepared(live_db) {
        discard_legacy_sync_copy(live_db);
        return Ok(());
    }

    // F1.4 empty-live guard (belt-and-suspenders on top of boot recovery): refuse to cut
    // over a live DB that looks freshly auto-created while recovery artifacts still exist —
    // that exact pattern (an empty live created by DbService::open after an interrupted
    // swap) is the data-loss trap. Boot recovery should have run first; if we still see
    // this, surface an error rather than enshrining the empty DB as the new sync DB.
    if recovery_artifacts_present(live_db) && live_has_no_user_rows(live_db) {
        return Err(
            "cutover refused: the live database looks freshly created while cutover recovery \
             artifacts (journal/backup/staging) are present. Restart the app so boot recovery \
             can restore your data before enabling sync."
                .to_string(),
        );
    }

    let staging = cutover_staging_path(live_db);
    let backup = backup_db_path(live_db);
    remove_db_files(&staging); // clear any partial staging from a prior aborted run

    // Step 2: pre-swap counts across ALL user tables (F5) — the data-preservation oracle
    // and the journal's content marker.
    let pre_counts =
        all_user_table_row_counts(live_db).map_err(|e| format!("pre-swap row counts: {e}"))?;

    // Step 2 (spike): VACUUM INTO reads the live DB and writes a fresh compacted copy;
    // the live DB is never opened for write.
    {
        let live = Connection::open(live_db).map_err(|e| e.to_string())?;
        let target = staging
            .to_str()
            .ok_or_else(|| "non-UTF8 staging path".to_string())?;
        live.execute("VACUUM INTO ?1", [target])
            .map_err(|e| format!("VACUUM INTO staging: {e}"))?;
    }

    if fault == CutoverFault::AfterVacuum {
        remove_db_files(&staging);
        return Err("injected cutover fault after VACUUM (live DB untouched)".to_string());
    }

    // Step 3: transform the staging copy into CRR form, reinstate the app-layer
    // invariants, reset watermarks to zero (§3 uniform reset), checkpoint, drop handle.
    let prep = (|| -> Result<(), String> {
        let db = CrsqlDb::open(&staging).map_err(|e| e.to_string())?;
        let conn = db.conn();
        schema::crr_migrate(conn).map_err(|e| format!("crr_migrate: {e}"))?;
        // (Bug fix) crr_migrate CRRifies whatever column shape the live copy currently
        // has — on a fresh device that is the base-migration shape (13-col segments,
        // 6-col chat_messages) because the frontend's runtime ALTERs (db.ts) had not
        // yet added speaker_id / the chat_messages columns before cutover ran. Apply the
        // codified out-of-band ALTERs through the crsql alter dance NOW, while the table
        // is CRR but before it becomes live, so those columns are CRR-tracked instead of
        // being added later by a bare (and swallowed) frontend ALTER that desyncs the
        // clock and quarantines every incoming change for them forever. Idempotent: a
        // long-lived DB that already carries the columns is skipped (schema.rs:329,332).
        schema::apply_out_of_band_alters(conn)
            .map_err(|e| format!("apply_out_of_band_alters: {e}"))?;
        cascade::cascade_gc(conn).map_err(|e| format!("cascade_gc: {e}"))?;
        uniqueness::enforce_uniqueness(conn).map_err(|e| format!("enforce_uniqueness: {e}"))?;
        mark_prepared(conn).map_err(|e| e.to_string())?;
        // Uniform state reset: full re-capture/re-push (0) + full re-pull (0). cr-sqlite
        // merge is idempotent/convergent so re-applying pulled changes is safe.
        state::ensure_meta_table(conn).map_err(|e| e.to_string())?;
        state::set_push_watermark(conn, 0).map_err(|e| e.to_string())?;
        state::set_pull_watermark(conn, 0).map_err(|e| e.to_string())?;
        // Step 4a: merge the -wal into the file before we drop the handle + rename.
        // F2: the checkpoint result is load-bearing — verify a FULL checkpoint (busy=0,
        // 0 frames remaining) before we later force-remove the staging sidecars, or we
        // would silently drop un-checkpointed frames.
        // `log`/`checkpointed` are -1 when the DB is NOT in WAL mode (staging comes from
        // `VACUUM INTO`, whose target defaults to rollback-journal mode) — that means
        // there is no -wal to fold, which is fine. Only `busy != 0` (a lock blocked it) or
        // `log > 0` (frames still in an active -wal) indicate a non-self-contained file.
        let (busy, log, ckpt): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(|e| e.to_string())?;
        if busy != 0 || log > 0 {
            return Err(format!(
                "staging checkpoint incomplete (busy={busy}, log_frames={log}, checkpointed={ckpt})"
            ));
        }
        Ok(())
    })();
    if let Err(e) = prep {
        remove_db_files(&staging);
        return Err(format!(
            "cutover staging prep failed (live DB untouched): {e}"
        ));
    }

    // Step 4b: quiesce + drop ALL live handles so the file can be renamed.
    if let Err(e) = db_service.close_for_swap() {
        let _ = db_service.reopen();
        remove_db_files(&staging);
        return Err(format!("cutover could not quiesce the DB pool: {e}"));
    }

    // Step 5: remove stale sidecars for both files. Safe now: `close_for_swap` and the
    // staging prep BOTH confirmed a FULL checkpoint (0 frames), so the main files are
    // self-contained and the -wal/-shm hold nothing (F2).
    remove_sidecars(live_db);
    remove_sidecars(&staging);

    // Step 5b: preserve any pre-existing backup by MOVING it aside with a timestamp
    // suffix — NEVER delete a backup without positive proof it is stale (F1.3). Disk is
    // cheap; a user's transcripts are not. The "kept forever, never auto-deleted" promise
    // now holds.
    if backup.exists() {
        let aside = live_db.with_file_name(format!(
            "yapstack.db.pre-sync-backup.{}",
            timestamp_suffix()
        ));
        tracing::info!(
            "sync: pre-existing backup found — moving it aside to {} (never deleted)",
            aside.display()
        );
        move_db_aside(&backup, &aside);
    }

    // Step 5c: DURABLE CUTOVER JOURNAL (F1.1). Written + fsynced BEFORE the first rename.
    // From here on, boot recovery — not file presence — is the source of truth: a crash
    // between the two renames is completed or rolled back by `recover_interrupted_cutover`.
    let mut journal = CutoverJournal {
        version: CUTOVER_JOURNAL_VERSION,
        phase: CutoverPhase::SwapStarted,
        live_file: file_name_string(live_db),
        backup_file: file_name_string(&backup),
        staging_file: file_name_string(&staging),
        pre_counts: pre_counts.clone(),
        live_size: std::fs::metadata(live_db).map(|m| m.len()).unwrap_or(0),
    };
    if let Err(e) = write_journal(live_db, &journal) {
        let _ = db_service.reopen();
        remove_db_files(&staging);
        return Err(format!(
            "cutover could not write the recovery journal (live DB untouched): {e}"
        ));
    }

    // Step 6a: rename live→backup, then fsync the directory so the rename is durable, then
    // advance the journal phase. Recovery treats SwapStarted-with-live-missing identically
    // to BackupMoved, so a crash before the phase write is still handled safely.
    if let Err(e) = std::fs::rename(live_db, &backup) {
        remove_journal(live_db);
        let _ = db_service.reopen();
        remove_db_files(&staging);
        return Err(format!(
            "cutover rename live→backup failed (live DB in place): {e}"
        ));
    }
    fsync_live_dir(live_db);
    journal.phase = CutoverPhase::BackupMoved;
    let _ = write_journal(live_db, &journal);

    // Injected crash BETWEEN the two renames: leave the mid-swap state on disk (live
    // missing, backup + staging present, journal at BackupMoved) for a boot-recovery test.
    if fault == CutoverFault::BetweenRenames {
        return Err(
            "injected cutover fault between renames (mid-swap state left for recovery)".to_string(),
        );
    }

    // Step 6b: rename staging→live, fsync the directory, advance the journal phase.
    if let Err(e) = std::fs::rename(&staging, live_db) {
        // live→backup happened but staging→live failed: restore the original from backup.
        let _ = std::fs::rename(&backup, live_db);
        fsync_live_dir(live_db);
        remove_journal(live_db);
        let _ = db_service.reopen();
        remove_db_files(&staging);
        return Err(format!(
            "cutover rename staging→live failed; rolled back: {e}"
        ));
    }
    fsync_live_dir(live_db);
    journal.phase = CutoverPhase::SwapCompleted;
    let _ = write_journal(live_db, &journal);

    // Step 7: reopen on the now-CRR live DB.
    if let Err(e) = db_service.reopen() {
        rollback_to_backup(live_db, &backup, db_service);
        return Err(format!(
            "cutover reopen failed; rolled back to pre-sync DB: {e}"
        ));
    }

    if fault == CutoverFault::AfterRename {
        rollback_to_backup(live_db, &backup, db_service);
        return Err("injected cutover fault after rename; rolled back".to_string());
    }

    // Step 8: verify EVERY user table's row count survived exactly (F5).
    match all_user_table_row_counts(live_db) {
        Ok(post) if post == pre_counts => {}
        Ok(post) => {
            rollback_to_backup(live_db, &backup, db_service);
            return Err(format!(
                "cutover row-count mismatch (pre {pre_counts:?} != post {post:?}); rolled back"
            ));
        }
        Err(e) => {
            rollback_to_backup(live_db, &backup, db_service);
            return Err(format!(
                "cutover post-swap count read failed; rolled back: {e}"
            ));
        }
    }

    // Verified and durable. Remove the journal LAST — its absence is what tells boot
    // recovery there is nothing to do. The backup is KEPT forever (never auto-deleted).
    remove_journal(live_db);

    // Success. The legacy copy (if any) is now redundant; discard it.
    discard_legacy_sync_copy(live_db);
    tracing::info!(
        "sync: CRR cutover complete — live DB is now the sync DB; pre-sync backup kept at {}",
        backup.display()
    );
    Ok(())
}

/// Restore the original live DB from its backup after a post-rename failure, then
/// reopen the pool. Best-effort: the backup is the source of truth for recovery.
fn rollback_to_backup(
    live_db: &Path,
    backup: &Path,
    db_service: &crate::db_service::DbServiceState,
) {
    // Drop any live pool BEFORE renaming: this helper is reached after a SUCCESSFUL
    // `reopen()` (post-rename fault / row-count mismatch), so the pool holds open
    // handles on `live_db`. On Windows a `rename`/`remove_file` over a file SQLite
    // still has open fails with a sharing violation (the VFS opens without
    // FILE_SHARE_DELETE); unix tolerates it, which is why this was invisible on macOS.
    // No checkpoint (`close_for_swap`) — the CRR live is being discarded, and a
    // checkpoint gate could refuse to drop and re-strand the handles.
    db_service.discard_pool();
    // Move the (now-CRR) failed live aside, then restore the byte-identical backup.
    let failed = live_db.with_file_name("yapstack.db.crr-cutover-failed");
    remove_db_files(&failed);
    let _ = std::fs::rename(live_db, &failed);
    remove_sidecars(live_db);
    if let Err(e) = std::fs::rename(backup, live_db) {
        tracing::error!(
            "sync: CRITICAL — cutover rollback could not restore the backup {} → {}: {e}; \
             the pre-sync data is intact at the backup path",
            backup.display(),
            live_db.display()
        );
    }
    remove_db_files(&failed);
    // The interrupted cutover is resolved (rolled back); drop the journal so the next
    // boot does not attempt recovery on a state we already restored.
    remove_journal(live_db);
    fsync_live_dir(live_db);
    let _ = db_service.reopen();
}

// ----- Dedicated single-thread drain runtime (deliverable A) -----

/// Handle to the running drain thread. Dropping/`stop`-ing sets the shutdown
/// flag; the thread exits at the next cycle boundary.
pub struct DrainHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    /// The independent audio-upload lane thread (S2). Bundled into the same handle so every
    /// existing stop-site (sign-out, re-login re-spawn, enable) tears BOTH lanes down
    /// together — no separate managed state to keep in lock-step.
    audio_shutdown: Arc<AtomicBool>,
    audio_join: Option<std::thread::JoinHandle<()>>,
}

impl DrainHandle {
    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.audio_shutdown.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        if let Some(join) = self.audio_join.take() {
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
    /// R12: PULL-side backlog. True when this device's pull watermark is behind the
    /// last-known relay tip — it is actively CATCHING UP and must NOT claim "up to date"
    /// merely because the outbox is empty. Distinct from `syncing` (which is the PUSH
    /// backlog). Set at cycle START (from the last-known tip) so a long catch-up pull shows
    /// honestly WHILE it grinds, not only after it finishes.
    catching_up: bool,
    /// R12: changesets still to pull to reach the last-known relay tip (0 when caught up or
    /// the tip is not yet known). Counts CHANGESETS (commit-ordered seqs), not cells.
    pull_behind: u64,
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

// ----- Audio upload lane status (S2) -----
//
// The audio uploader runs on its OWN thread (independent of the changeset drain — a
// 599 MB upload must never starve changeset sync). This process-global cell is the same
// one-way channel pattern as `DrainProgress`/`DrainHealth`: the uploader publishes its
// latest lane counts each cycle; `build_status_dto` reads them into the status DTO under
// the DISTINCT "audio-upload" label (never merged with changeset health). Never carries
// token material or a plaintext path.

/// Point-in-time audio-upload lane snapshot surfaced to the Sync panel.
#[derive(Debug, Default, Clone, Copy)]
struct AudioLaneSnapshot {
    /// Outstanding work across BOTH priorities (pending + sealing + uploading).
    outstanding: u64,
    /// Of `outstanding`, the low-priority backfill lane (the D9 historical library walk).
    backfill_outstanding: u64,
    /// Entries the uploader marked `failed` (needs attention; app-start + manual retry).
    failed: u64,
    /// Cumulative blobs the relay confirms it holds for this device (queue rows in `done`).
    uploaded_total: u64,
    /// Whether the idempotent backfill walk has completed on this device (D9).
    backfill_complete: bool,
}

fn audio_lane_cell() -> &'static RwLock<AudioLaneSnapshot> {
    static CELL: OnceLock<RwLock<AudioLaneSnapshot>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(AudioLaneSnapshot::default()))
}

fn audio_lane() -> AudioLaneSnapshot {
    *audio_lane_cell().read().unwrap_or_else(|e| e.into_inner())
}

fn set_audio_lane(next: AudioLaneSnapshot) {
    *audio_lane_cell().write().unwrap_or_else(|e| e.into_inner()) = next;
}

/// Clear the audio-lane snapshot (uploader stopped / signed out) so a poll never shows a
/// stale upload backlog.
fn reset_audio_lane() {
    set_audio_lane(AudioLaneSnapshot::default());
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

/// Tauri event emitted to the frontend after a drain cycle that MERGED pulled peer
/// changes (`applied > 0`). Item 2 (R6): the drain applies remote writes to the live DB
/// but nothing told the frontend, so pulled sessions/notes/folders only appeared after an
/// app restart. The store listens for this and refreshes the coarse queries (debounced).
pub const SYNC_APPLIED_EVENT: &str = "sync://applied";

/// Payload for [`SYNC_APPLIED_EVENT`]. Counts only — no row content, table names are the
/// coarse hint the frontend already refreshes wholesale (no fine-grained invalidation v1).
#[derive(Clone, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
struct SyncAppliedEvent {
    /// Rows merged from peers this cycle (the trigger; always > 0 when emitted).
    applied: usize,
    /// Rows deferred to quarantine this cycle (unknown-column schema gap).
    quarantined: usize,
    /// Quarantined rows replayed this cycle once their column existed.
    replayed: usize,
}

/// The [`SYNC_APPLIED_EVENT`] payload for a drain cycle, or `None` when nothing peer-visible
/// was merged. Extracted from the drain loop so the "emit iff `applied > 0`" decision is
/// unit-testable without a running Tauri app (item 2). A purely quarantine/replay cycle
/// changes no currently-visible rows, so it does NOT trigger a refresh.
fn applied_event_for(report: &outbox::DrainReport) -> Option<SyncAppliedEvent> {
    (report.applied > 0).then_some(SyncAppliedEvent {
        applied: report.applied,
        quarantined: report.quarantined,
        replayed: report.replayed,
    })
}

/// Boot-time hook (called from the Tauri `setup` closure): if the keychain holds an
/// enabled session, ensure the live DB is CRR-prepared (running the one-time A3
/// cutover if this device was enabled under the old copy architecture — the owner's
/// Mac migrates itself on next restart with no re-click) and start the drain on the
/// LIVE CRR DB. No-op when signed out or sync not yet enabled.
pub fn start_drain_if_enabled(
    app: &tauri::AppHandle,
    live_db: &Path,
    runtime: &SyncRuntimeState,
    db_service: &crate::db_service::DbServiceState,
) {
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
    // Already-enabled migration (A3): if this device is enabled but the live DB is not
    // yet CRR (old copy architecture, or enabled before this build), run the cutover
    // ONCE now — same backup + rollback discipline as the Enable button.
    if !live_is_crr_prepared(live_db) {
        tracing::info!("sync: enabled device with a non-CRR live DB — running one-time A3 cutover");
        if let Err(e) = perform_cutover(live_db, db_service) {
            tracing::error!("sync: auto-cutover failed at boot; drain not started: {e}");
            return;
        }
    } else {
        discard_legacy_sync_copy(live_db);
    }
    match spawn_drain(
        app.clone(),
        live_db.to_path_buf(),
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
// The drain is parameterised by the full session tuple plus the AppHandle it emits
// `sync://applied` on; grouping these into a struct would not aid clarity here.
#[allow(clippy::too_many_arguments)]
fn spawn_drain(
    app: tauri::AppHandle,
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
    reset_audio_lane();

    // S2: spawn the INDEPENDENT audio-upload lane before the changeset drain consumes the
    // session tuple. It opens its own connection to the same CRR live DB on its own thread
    // (a 599 MB upload must never block the changeset drain), and shares the persisted
    // bearer (reloaded when stale) rather than refreshing itself — two independent refreshers
    // would race the reuse-detection family revocation (T009). Best-effort: an uploader
    // spawn failure is logged and the changeset drain still starts.
    let (audio_shutdown, audio_join) = match spawn_audio_uploader(
        sync_db.clone(),
        server_url.clone(),
        bearer.clone(),
        vault_key,
        epoch,
        tenant_id,
    ) {
        Ok((sd, jh)) => (sd, Some(jh)),
        Err(e) => {
            tracing::error!("sync: audio uploader spawn failed (upload lane disabled): {e}");
            (Arc::new(AtomicBool::new(true)), None)
        }
    };

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
            // F2 boot-time self-heal: this connection has the crsql extension loaded and
            // the live DB is CRR-prepared by the time any drain starts (start_drain_if_enabled
            // guarantees cutover ran; enable/re-login likewise). A DB that was CRR-prepared by
            // a build PRIOR to the cutover fix above — or that gains new OUT_OF_BAND_ALTERS
            // entries in a future update — is missing those CRR-tracked columns, so every
            // incoming change for them quarantines (is_unknown_column) and never replays. Apply
            // the codified alters here once, on the drain's own connection, BEFORE the loop:
            // idempotent (skips columns that already exist), and the very next replay_pending
            // cycle then recovers all previously quarantined changes automatically. No separate
            // replay trigger is needed.
            if let Err(e) = schema::apply_out_of_band_alters(conn) {
                // Non-fatal: a failure here leaves the pre-existing (quarantining) behaviour
                // rather than aborting the drain. Log loudly so it is not silent.
                tracing::error!("sync drain: out-of-band alter self-heal failed: {e}");
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

            // R12: seed the last-known relay tip so a device that booted BEHIND on the PULL
            // side shows an honest "catching up" state during the (possibly long) first drain
            // instead of a stale "up to date". ONE cheap `GET /sync/completeness` at spawn;
            // it is refreshed every cycle thereafter from the drain report (push tip /
            // clean-pull watermark), so this is the only added round-trip. A device that has
            // NEVER reached the relay must not read "up to date": a connectivity failure here
            // lands the existing amber `Unreachable` health (which takes precedence over both
            // catching-up and up-to-date), so the pre-first-sync window is honest.
            let mut last_server_max: Option<i64> = match rt.block_on(transport.completeness()) {
                Ok(c) => Some(c.max_changeset_seq),
                Err(e) => {
                    if e.is_network() {
                        set_drain_health(DrainHealth::Unreachable(e.to_string()));
                    }
                    tracing::debug!("sync: initial relay tip unknown (completeness failed): {e}");
                    None
                }
            };
            // Behind = last-known tip − pull watermark, clamped at 0 (0 when caught up or the
            // tip is unknown). This is the PULL backlog the up-to-date gate now honours.
            let behind_now = |server_max: Option<i64>| -> u64 {
                match (server_max, state::pull_watermark(conn)) {
                    (Some(m), Ok(wm)) => (m - wm).max(0) as u64,
                    _ => 0,
                }
            };
            let initial_behind = behind_now(last_server_max);
            // One-shot "catching up" transition log, mirroring `had_backlog` for the push side.
            let mut announced_catch_up = initial_behind > 0;
            if initial_behind > 0 {
                tracing::info!(
                    "sync: catching up — {initial_behind} changeset(s) behind the relay"
                );
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
                        catching_up: p.entries == 0 && initial_behind > 0,
                        pull_behind: initial_behind,
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
                // R12: BEFORE the (possibly long, multi-page) drain, publish the catching-up
                // state if the last-known tip already says we are behind on the PULL. Without
                // this, a big catch-up pull would leave the UI on the PRIOR state (e.g. a stale
                // "up to date") for its whole grind — the exact display-honesty bug. Only
                // touches progress when actually behind, so a caught-up device never strobes.
                let start_behind = behind_now(last_server_max);
                if start_behind > 0 {
                    if !announced_catch_up {
                        tracing::info!(
                            "sync: catching up — {start_behind} changeset(s) behind the relay"
                        );
                        announced_catch_up = true;
                    }
                    let prev = drain_progress();
                    set_drain_progress(DrainProgress {
                        catching_up: prev.pending_entries == 0,
                        pull_behind: start_behind,
                        ..prev
                    });
                }

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

                // Item 3 (R6): pull-side observability. The push transitions log above; before
                // this the PULL/apply half was a black box (Round-5 Windows logs had ZERO pull
                // lines across hours of syncing). Log per cycle ONLY when a merge actually landed
                // — never per idle cycle, counts only, no row content or secrets.
                if report.applied > 0 || report.quarantined > 0 || report.replayed > 0 {
                    tracing::info!(
                        "sync: pulled and applied {} change(s) ({} quarantined, {} replayed)",
                        report.applied,
                        report.quarantined,
                        report.replayed
                    );
                }

                // Item 2 (R6): tell the frontend a merge landed so it can refresh the affected
                // views WITHOUT an app restart. Only when peer rows were actually applied
                // (`applied > 0`); a purely-quarantine/replay cycle changes no visible rows.
                // Best-effort emit — a missing webview (not yet mounted) must never wedge the
                // drain, exactly like `device_broker`'s `devices-changed`.
                if let Some(ev) = applied_event_for(&report) {
                    if let Err(e) = app.emit(SYNC_APPLIED_EVENT, ev) {
                        tracing::warn!("sync drain: emit {SYNC_APPLIED_EVENT} failed: {e}");
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

                // R12: fold the tip THIS cycle disclosed (push tip / clean-pull watermark) into
                // the running last-known tip. `changeset_seq` is dense/monotonic so the tip
                // only ever advances; a cycle that learned nothing (nothing to push AND the
                // pull failed) leaves the previous tip intact rather than regressing it.
                if let Some(m) = report.server_max {
                    last_server_max = Some(last_server_max.map_or(m, |prev| prev.max(m)));
                }
                // Behind after this cycle: the last-known tip minus the watermark we reached.
                // A CLEAN full pull drives this to 0; a failed/partial pull leaves the honest
                // shortfall so we never claim "up to date" over an un-merged peer backlog.
                let pull_behind = last_server_max
                    .map_or(0i64, |m| (m - report.pull_watermark).max(0)) as u64;

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
                        // "Up to date" (R12) counts ONLY when ALL hold:
                        //   1. the outbox is empty (`p.entries == 0`) — push fully acked;
                        //   2. the PULL watermark has reached the last-known relay tip
                        //      (`pull_behind == 0`) — the fix: an empty outbox is NOT up to
                        //      date while a peer's changesets remain un-merged;
                        //   3. no transport error AND no crypto skip this cycle (`clean`) —
                        //      else we'd claim success mid-failure, or over a peer write we
                        //      could not decrypt (R5). A crypto failure already rides on
                        //      `pull_error`, so (3)'s crypto clause is belt-and-suspenders.
                        // While (2) fails we publish a DISTINCT `catching_up` state carrying the
                        // honest `pull_behind` count; connectivity/auth health (Unreachable /
                        // Failing / AuthExpired) already takes precedence over both in
                        // `build_status_dto` (unreachable/failing > catching-up > up-to-date).
                        let clean = report.first_transport_error().is_none()
                            && report.crypto_skipped == 0;
                        let caught_up = p.entries == 0 && pull_behind == 0 && clean;
                        let last_success = if caught_up {
                            if had_backlog || announced_catch_up {
                                tracing::info!("sync: up to date");
                                had_backlog = false;
                                announced_catch_up = false;
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
                            // Catching-up is a PULL-side state; a pending PUSH backlog
                            // (`syncing`) takes visual precedence, so only flag it when the
                            // outbox is empty but the pull is still behind.
                            catching_up: p.entries == 0 && pull_behind > 0,
                            pull_behind,
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
        audio_shutdown,
        audio_join,
    })
}

// ----- Audio upload lane (S2 — independent background uploader) -----

/// Publish the current audio-lane counts to the status cell (read by `build_status_dto`).
/// Reads are cheap sqlite `count(*)`s on the local, non-CRR queue table.
fn publish_audio_lane(conn: &rusqlite::Connection) {
    use yapstack_sync::audio;
    let counts = match audio::lane_status(conn) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("audio uploader: lane_status: {e}");
            return;
        }
    };
    // The low-priority backfill lane's still-outstanding share (for the "backfilling library"
    // copy). A direct query keeps the engine's `AudioLaneStatus` untouched.
    let backfill_outstanding: u64 = conn
        .query_row(
            "SELECT count(*) FROM _yapstack_audio_upload_queue \
             WHERE priority = ?1 AND state IN ('pending','sealing','uploading')",
            [audio::PRIORITY_BACKFILL],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as u64)
        .unwrap_or(0);
    let backfill_complete = audio::backfill_walk_completed(conn).unwrap_or(false);
    set_audio_lane(AudioLaneSnapshot {
        outstanding: counts.outstanding(),
        backfill_outstanding,
        failed: counts.failed,
        uploaded_total: counts.done,
        backfill_complete,
    });
}

/// Spawn the audio-upload lane on its own OS thread with a current-thread runtime (the
/// `rusqlite::Connection` and the `!Send` `drain_one` future never leave it — same pattern
/// as the changeset drain). At start it sweeps orphaned seal temps (A3), re-arms crashed +
/// failed entries (D9 app-start retry), and runs the idempotent backfill walk (D9), then
/// drains ONE blob at a time, NORMAL before LOW (the engine's ordering), publishing lane
/// counts to the status cell. It NEVER refreshes the token itself — it reloads the bearer
/// the changeset drain persisted when it is near expiry (two independent refreshers would
/// race reuse-detection, T009).
#[allow(clippy::too_many_arguments)]
fn spawn_audio_uploader(
    sync_db: PathBuf,
    server_url: String,
    bearer: String,
    vault_key: [u8; 32],
    epoch: u32,
    tenant_id: Uuid,
) -> Result<(Arc<AtomicBool>, std::thread::JoinHandle<()>), String> {
    use yapstack_sync::audio::{self, AudioSealContext, DrainStep};
    let shutdown = Arc::new(AtomicBool::new(false));
    let stop = shutdown.clone();
    let join = std::thread::Builder::new()
        .name("yapstack-audio-uploader".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("audio uploader: runtime build failed: {e}");
                    return;
                }
            };
            // Own connection to the same CRR live DB (extension loaded on THIS connection).
            let db = match CrsqlDb::open(&sync_db) {
                Ok(db) => db,
                Err(e) => {
                    tracing::error!("audio uploader: cannot open CRR db: {e}");
                    return;
                }
            };
            let conn = db.conn();
            if let Err(e) = audio::ensure_queue_table(conn) {
                tracing::error!("audio uploader: queue table: {e}");
                return;
            }
            // Seal temps live beside the live DB (THIS device's dir, D2) — never the source
            // file's dir, never the source device's path.
            let temp_dir = sync_db.with_file_name("audio-upload-tmp");
            // A3: reclaim seal temps a crash left mid-seal (only files with OUR prefix).
            match audio::sweep_orphan_temps(&temp_dir) {
                Ok(n) if n > 0 => {
                    tracing::info!("audio uploader: swept {n} orphaned seal temp(s)")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("audio uploader: temp sweep: {e}"),
            }
            // App-start recovery (D9): re-arm crashed in-flight + retry failed entries.
            if let Err(e) = audio::reset_in_flight(conn) {
                tracing::warn!("audio uploader: reset_in_flight: {e}");
            }
            if let Err(e) = audio::retry_failed(conn) {
                tracing::warn!("audio uploader: retry_failed: {e}");
            }
            // Backfill walk (owner: starts immediately, idempotent, on EVERY device that ever
            // recorded — the server-completeness invariant). Enqueues every local part whose
            // file exists on THIS device at LOW priority, behind new recordings.
            match audio::backfill_walk(conn, |p| std::path::Path::new(p).exists()) {
                Ok(r) => {
                    if r.enqueued > 0 || r.missing_file > 0 {
                        tracing::info!(
                            "audio backfill: examined {} part(s), enqueued {}, {} missing here",
                            r.examined,
                            r.enqueued,
                            r.missing_file
                        );
                    }
                }
                Err(e) => tracing::warn!("audio backfill walk failed: {e}"),
            }
            let ctx = AudioSealContext {
                vault_key,
                tenant_id: *tenant_id.as_bytes(),
                epoch,
            };
            let transport = HttpTransport::new(server_url, bearer.clone());
            let mut current_bearer = bearer;
            publish_audio_lane(conn);

            while !stop.load(Ordering::SeqCst) {
                // Drain to Idle, ONE blob at a time (so a 599 MB upload never starves the UI
                // poll or the changeset lane). Between blobs, reload the shared bearer only
                // when it is near expiry — never refresh (rotate) it here.
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if access_token_stale(&current_bearer, 60) {
                        if let Ok(Some(s)) = load_session() {
                            if s.bearer != current_bearer {
                                current_bearer = s.bearer.clone();
                                transport.set_bearer(&current_bearer);
                            }
                        }
                    }
                    match rt.block_on(audio::drain_one(conn, &transport, &ctx, &temp_dir)) {
                        Ok(DrainStep::Idle) => break,
                        Ok(DrainStep::Uploaded { .. }) | Ok(DrainStep::DroppedDeleted { .. }) => {
                            publish_audio_lane(conn);
                        }
                        Ok(DrainStep::Failed { part_id, error }) => {
                            tracing::warn!("audio upload failed for part {part_id}: {error}");
                            publish_audio_lane(conn);
                        }
                        Err(e) => {
                            // A sqlite/bookkeeping fault — back off and retry next cycle; never
                            // crash the lane.
                            tracing::warn!("audio uploader cycle error: {e}");
                            break;
                        }
                    }
                }
                publish_audio_lane(conn);
                std::thread::sleep(AUDIO_DRAIN_INTERVAL);
            }
            tracing::info!("audio uploader stopped");
        })
        .map_err(|e| e.to_string())?;
    Ok((shutdown, join))
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
    // S2: the audio-upload lane's latest published counts (own one-way cell, like progress).
    let audio = audio_lane();
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
        // A PUSH backlog (outbox not yet drained) is the `syncing` phase, as before.
        DrainHealth::Ok if session.sync_enabled && progress.syncing => {
            ("syncing".to_string(), last_error)
        }
        // R12: the outbox is empty but the PULL is still behind the relay tip — a DISTINCT
        // `catching_up` phase (never the green "up to date"). Ranks below the connectivity /
        // auth states above and below `syncing`, giving the precedence
        // unreachable/failing/auth > syncing > catching-up > up-to-date.
        DrainHealth::Ok if session.sync_enabled && progress.catching_up => {
            ("catching_up".to_string(), last_error)
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
        pull_behind: progress.pull_behind,
        audio_upload_outstanding: audio.outstanding,
        audio_backfill_outstanding: audio.backfill_outstanding,
        audio_upload_failed: audio.failed,
        audio_uploaded_total: audio.uploaded_total,
        audio_backfill_complete: audio.backfill_complete,
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
            pull_behind: 0,
            audio_upload_outstanding: 0,
            audio_backfill_outstanding: 0,
            audio_upload_failed: 0,
            audio_uploaded_total: 0,
            audio_backfill_complete: false,
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
    let db_service = app
        .try_state::<crate::db_service::DbServiceState>()
        .ok_or_else(|| "db service unavailable".to_string())?
        .inner()
        .clone();

    // A3 cutover: turn the LIVE yapstack.db INTO the CRR database (backup kept), so the
    // drain captures every UI write. Runs on a blocking thread (VACUUM/checkpoint/rename).
    {
        let live = db_path.clone();
        tokio::task::spawn_blocking(move || perform_cutover(&live, &db_service))
            .await
            .map_err(|e| e.to_string())??;
    }

    // Deliverable A: start the drain on its dedicated thread — now on the LIVE CRR DB.
    let vault_key = session.vault_key()?;
    let handle = spawn_drain(
        app.clone(),
        db_path.clone(),
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

/// S2 producer seam: enqueue every finalized audio part of `session_id` onto the durable
/// upload queue at NORMAL priority (jumps ahead of the LOW-priority backfill). Called
/// FIRE-AND-FORGET from the frontend on session finalize — recording is never blocked by
/// this, and the durable queue survives a restart. Idempotent (INSERT-OR-IGNORE by
/// `part_id`), so a re-finalize or an overlap with the backfill walk never double-uploads.
/// A no-op when sync is not enabled on this device (nothing drains the queue yet; the
/// backfill walk will re-enqueue on next enable). Opens a lightweight plain connection to
/// the live DB and only ever writes the local, non-CRR queue table.
#[tauri::command]
#[specta::specta]
pub fn audio_enqueue_session(app: tauri::AppHandle, session_id: String) -> Result<u32, String> {
    // Only meaningful once sync is on — otherwise the queue has no drainer.
    match load_session()? {
        Some(s) if s.sync_enabled => {}
        _ => return Ok(0),
    }
    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    yapstack_sync::audio::ensure_queue_table(&conn).map_err(|e| e.to_string())?;

    let mut stmt = conn
        .prepare(
            "SELECT id, file_path FROM session_audio_parts \
             WHERE session_id = ?1 AND file_path IS NOT NULL AND file_path <> ''",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([&session_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let parts: Vec<(String, String)> = rows.filter_map(Result::ok).collect();
    drop(stmt);

    let mut enqueued = 0u32;
    for (part_id, file_path) in parts {
        match yapstack_sync::audio::enqueue_on_save(&conn, &part_id, &file_path, &session_id) {
            Ok(true) => enqueued += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!("audio_enqueue_session: enqueue {part_id}: {e}"),
        }
    }
    Ok(enqueued)
}

/// S2 manual-retry seam for the Sync panel: re-arm every `failed` audio-upload entry so the
/// uploader picks them up on its next cycle (D9 — failed entries retry on app start PLUS
/// manual retry). Returns the number re-armed. No-op when sync is disabled.
#[tauri::command]
#[specta::specta]
pub fn audio_retry_failed_uploads(app: tauri::AppHandle) -> Result<u32, String> {
    match load_session()? {
        Some(s) if s.sync_enabled => {}
        _ => return Ok(0),
    }
    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let n = yapstack_sync::audio::retry_failed(&conn).map_err(|e| e.to_string())?;
    Ok(n as u32)
}

/// S3 dictation fold-in — FIRE-AND-FORGET enqueue of a saved dictation's WAV onto the upload
/// queue (NORMAL priority). Dictation audio has no `session_audio_parts` row; the part
/// identity IS the `dictation_history.id` (self-referential AAD `session_id`, matching the
/// backfill walk + fetch path). Idempotent (INSERT-OR-IGNORE by part_id). No-op when sync is
/// off. Returns 1 if newly enqueued, else 0.
#[tauri::command]
#[specta::specta]
pub fn audio_enqueue_dictation(app: tauri::AppHandle, dictation_id: String) -> Result<u32, String> {
    match load_session()? {
        Some(s) if s.sync_enabled => {}
        _ => return Ok(0),
    }
    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    yapstack_sync::audio::ensure_queue_table(&conn).map_err(|e| e.to_string())?;
    let wav: Option<String> = conn
        .query_row(
            "SELECT wav_file_path FROM dictation_history \
             WHERE id = ?1 AND wav_file_path IS NOT NULL AND wav_file_path <> ''",
            [&dictation_id],
            |r| r.get(0),
        )
        .ok();
    let Some(wav) = wav else { return Ok(0) };
    // session_id == part_id (self): dictation has no owning session.
    match yapstack_sync::audio::enqueue_on_save(&conn, &dictation_id, &wav, &dictation_id) {
        Ok(true) => Ok(1),
        Ok(false) => Ok(0),
        Err(e) => Err(e.to_string()),
    }
}

// ----- Fetch-on-demand playback (S3, D2/D3) -----
//
// The single-flight registry + fetch worker live here (the desktop owns the transport, the
// vault key, and the cache dir); the coalescing/decrypt-to-cache primitives are the engine's
// (`yapstack_sync::audio`). Player views POLL `audio_prepare_part` per missing part — it
// resolves D2 order (cache hit → same-device path is handled FE-side via `audio_files_exist`
// → fetch), joins/starts the single in-flight fetch, and reports progress or a terminal
// state. `audio_cancel_part` cancels an in-flight fetch AND clears the slot (the retry seam).

/// Process-wide single-flight fetch registry (one in-flight download per part_id).
fn fetch_registry() -> &'static yapstack_sync::audio::FetchRegistry {
    static CELL: OnceLock<yapstack_sync::audio::FetchRegistry> = OnceLock::new();
    CELL.get_or_init(yapstack_sync::audio::FetchRegistry::new)
}

/// Keep-until-clear fetch cache dir (this device's dir, D2 — never the source device's path).
fn fetch_cache_dir(db_path: &Path) -> PathBuf {
    db_path.with_file_name("audio-fetch-cache")
}

/// Encrypted-download scratch dir (beside the cache, on this device).
fn fetch_temp_dir(db_path: &Path) -> PathBuf {
    db_path.with_file_name("audio-fetch-tmp")
}

/// Player-facing fetch state for one part (polled). Distinct honest states, never silent.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AudioPreparePartDto {
    /// The decrypted audio is in the local cache (or was already local) — `path` is a
    /// trusted absolute path the `audio-stream://` protocol serves.
    Ready { path: String },
    /// A download is in flight; `received`/`total` (bytes) drive the "Fetching N%" bar
    /// (`total == 0` = not yet known).
    Fetching { received: u64, total: u64 },
    /// Admitted but not yet started — the global concurrency cap (2) is busy and this part
    /// waits in the FIFO admission queue. The player renders "waiting".
    Queued,
    /// The relay has no blob for this part (source device hasn't uploaded it) — the
    /// honest "audio is on <device>" state. NOT a stable terminal anymore: the slot is
    /// cleared so the player's 30s re-probe re-attempts, and the fetch starts by itself
    /// once the source device's upload lands.
    NotOnServer,
    /// The cache volume positively reported insufficient space (disk precheck) — `needed`
    /// bytes is the full clean-disk budget for the "Not enough disk space (need ~X)" copy.
    /// Slot cleared, so a retry after freeing space starts fresh.
    NoSpace { needed: u64 },
    /// Can't reach the sync server (relay unreachable).
    Unreachable,
    /// The blob failed to decrypt/verify — a tamper signal (logged at warn server-side of the
    /// fetch worker). Distinct copy: "audio failed verification".
    VerificationFailed,
    /// Any other fetch/IO fault, surfaced verbatim (never auto-routed).
    Error { message: String },
}

/// Parse a hyphenated- or simple-format UUID string into its 16 raw bytes for the crypto AAD.
fn uuid16(s: &str) -> Result<[u8; 16], String> {
    Uuid::parse_str(s)
        .map(|u| *u.as_bytes())
        .map_err(|_| format!("not a UUID: {s}"))
}

/// The part's owning-session id (for the AAD) + file extension, resolved from the local DB.
/// Session recordings come from `session_audio_parts`; dictation audio's identity is its own
/// `dictation_history.id` (self-referential session_id) — the S3 dictation fold-in.
fn resolve_part_identity(conn: &rusqlite::Connection, part_id: &str) -> Option<(String, String)> {
    if let Ok((session_id, format)) = conn.query_row(
        "SELECT session_id, format FROM session_audio_parts WHERE id = ?1",
        [part_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    ) {
        let ext = if format.eq_ignore_ascii_case("mp3") {
            "mp3"
        } else {
            "wav"
        };
        return Some((session_id, ext.to_string()));
    }
    if let Ok(wav) = conn.query_row(
        "SELECT wav_file_path FROM dictation_history WHERE id = ?1",
        [part_id],
        |r| r.get::<_, String>(0),
    ) {
        let ext = if wav.to_ascii_lowercase().ends_with(".mp3") {
            "mp3"
        } else {
            "wav"
        };
        // session_id == part_id (self) — matches the enqueue + backfill dictation identity.
        return Some((part_id.to_string(), ext.to_string()));
    }
    None
}

/// Build the player-facing DTO from a slot's live progress + terminal outcome. On a retryable
/// terminal (unreachable / not-on-server / no-space / other error) the slot is cleared so the
/// NEXT poll (a user retry, or the player's automatic 30s re-probe for not-on-server) starts
/// fresh; `Ready` and `VerificationFailed` are the only stable terminals (a re-download of a
/// tampered blob would fail identically).
fn slot_to_dto(
    part_id: &str,
    slot: &yapstack_sync::audio::FetchSlot,
    cache_path: &Path,
) -> AudioPreparePartDto {
    use yapstack_sync::audio::FetchResult;
    match slot.outcome() {
        None if slot.is_queued() => AudioPreparePartDto::Queued,
        None => {
            let (received, total) = slot.progress.snapshot();
            AudioPreparePartDto::Fetching { received, total }
        }
        Some(Ok(FetchResult::Fetched)) => AudioPreparePartDto::Ready {
            path: cache_path.to_string_lossy().into_owned(),
        },
        Some(Ok(FetchResult::NotOnServer)) => {
            // Clear the slot (auto-fetch S3.5): the source device may simply not have
            // uploaded YET, so the player re-probes on a 30s cadence and each probe must be
            // a real re-attempt, not a read of this stale terminal.
            fetch_registry().remove(part_id);
            AudioPreparePartDto::NotOnServer
        }
        Some(Ok(FetchResult::NoSpace { needed })) => {
            // Clear so a retry AFTER the user frees space starts a fresh download.
            fetch_registry().remove(part_id);
            AudioPreparePartDto::NoSpace { needed }
        }
        Some(Ok(FetchResult::Cancelled)) => {
            // A cancel leaves the slot idle; clear it so a replay starts a fresh fetch.
            fetch_registry().remove(part_id);
            AudioPreparePartDto::NotOnServer
        }
        Some(Err(code)) => {
            let dto = match code.as_str() {
                "unreachable" => AudioPreparePartDto::Unreachable,
                "verification" => AudioPreparePartDto::VerificationFailed,
                other => AudioPreparePartDto::Error {
                    message: other.strip_prefix("error:").unwrap_or(other).to_string(),
                },
            };
            // Verification is a stable terminal (re-download would fail identically); the
            // transient errors clear so a manual retry re-attempts.
            if !matches!(dto, AudioPreparePartDto::VerificationFailed) {
                fetch_registry().remove(part_id);
            }
            dto
        }
    }
}

/// Available bytes on the volume holding `path` (walking up to the nearest existing
/// ancestor), or `None` when the platform can't say — the disk precheck FAILS OPEN on
/// `None` (engine contract: never fabricate a NoSpace). Unix: `statvfs` (`f_bavail *
/// f_frsize` — the unprivileged-available figure, matching what a write can actually use).
/// Windows: `GetDiskFreeSpaceExW`'s caller-available counter.
fn volume_free_space(path: &Path) -> Option<u64> {
    let mut probe = path;
    while !probe.exists() {
        probe = probe.parent()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `c` is a valid NUL-terminated path and `st` is sized for statvfs; the
        // struct is only read after a 0 (success) return.
        let rc = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        let st = unsafe { st.assume_init() };
        // Field widths differ per libc target (u32 on macOS, u64 on Linux) — widen to u64.
        // The cast is a no-op on targets where the field is already u64, hence the allow.
        #[allow(clippy::unnecessary_cast)]
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = probe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut avail: u64 = 0;
        // SAFETY: `wide` is a NUL-terminated UTF-16 path; `avail` outlives the call; the
        // two totals are explicitly optional (null) per the API contract.
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut avail,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(avail)
    }
}

/// S3 fetch-on-demand entry point (polled per missing part). Resolves D2 order, joins the
/// single in-flight fetch (starting it on the first call), and returns the current state.
#[tauri::command]
#[specta::specta]
pub fn audio_prepare_part(
    app: tauri::AppHandle,
    part_id: String,
) -> Result<AudioPreparePartDto, String> {
    let session = match load_session()? {
        Some(s) if s.sync_enabled => s,
        // No server to fetch from → stays the honest on-device state.
        _ => return Ok(AudioPreparePartDto::NotOnServer),
    };
    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let Some((session_id, ext)) = resolve_part_identity(&conn, &part_id) else {
        return Err(format!("unknown audio part: {part_id}"));
    };
    drop(conn);

    let cache_dir = fetch_cache_dir(&db_path);
    let cache_path = cache_dir.join(format!("{part_id}.{ext}"));

    // D2 step 1: a present cache file wins outright. Register the cache dir as trusted so the
    // audio-stream:// handler serves it, then hand back the path.
    if yapstack_sync::audio::cache_hit(&cache_path) {
        crate::register_trusted_audio_dir(&app, &cache_dir);
        return Ok(AudioPreparePartDto::Ready {
            path: cache_path.to_string_lossy().into_owned(),
        });
    }

    // Coalesce onto an existing slot (queued or in flight) without re-reading the keychain.
    if let Some(slot) = fetch_registry().get(&part_id) {
        return Ok(slot_to_dto(&part_id, &slot, &cache_path));
    }

    // Build the deferred starter: when the registry admits this part (immediately if a
    // permit is free, else FIFO after a running fetch finishes) it spawns the ONE download
    // worker thread (a 599 MB download must not block the command runtime). Every started
    // worker calls `task_finished` exactly once so the global cap (2) frees correctly.
    let vault_key = session.vault_key()?;
    let id = yapstack_crypto::audio_stream::AudioIdentity {
        tenant_id: *session.tenant_id.as_bytes(),
        session_id: uuid16(&session_id)?,
        part_id: uuid16(&part_id)?,
        epoch: session.epoch,
    };
    let server_url = session.server_url.clone();
    let bearer = session.bearer.clone();
    let temp_dir = fetch_temp_dir(&db_path);
    let cache_for_worker = cache_path.clone();
    let part_for_worker = part_id.clone();
    let app_for_worker = app.clone();
    let cache_dir_for_worker = cache_dir.clone();
    let part_for_starter = part_id.clone();
    let starter: yapstack_sync::audio::FetchStarter = Box::new(move || {
        // The slot exists by the time the registry runs any starter; a missing slot means
        // it was removed while queued, which cancel_if_queued prevents for queued entries —
        // guard anyway so a race can only ever no-op (still releasing the permit).
        let Some(slot_for_worker) = fetch_registry().get(&part_for_starter) else {
            fetch_registry().task_finished();
            return;
        };
        let spawn = std::thread::Builder::new()
            .name("yapstack-audio-fetch".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        slot_for_worker.finish(Err(format!("error:runtime: {e}")));
                        fetch_registry().task_finished();
                        return;
                    }
                };
                let transport = HttpTransport::new(server_url, bearer);
                // Disk precheck probe (S3 auto-fetch): platform free-space, fail-open.
                let probe = |p: &Path| volume_free_space(p);
                let result = rt.block_on(yapstack_sync::audio::fetch_blob_to_cache(
                    &transport,
                    &vault_key,
                    &id,
                    &part_for_worker,
                    &temp_dir,
                    &cache_for_worker,
                    &slot_for_worker.progress,
                    Some(&probe),
                ));
                let terminal = match result {
                    Ok(r) => Ok(r),
                    Err(e) if e.is_network() => Err("unreachable".to_string()),
                    Err(yapstack_sync::SyncError::Crypto(_)) => {
                        tracing::warn!(
                            "audio fetch: decrypt/verification failed for part {part_for_worker} \
                             (tamper signal)"
                        );
                        Err("verification".to_string())
                    }
                    Err(e) => Err(format!("error:{e}")),
                };
                if matches!(terminal, Ok(yapstack_sync::audio::FetchResult::Fetched)) {
                    crate::register_trusted_audio_dir(&app_for_worker, &cache_dir_for_worker);
                }
                slot_for_worker.finish(terminal);
                fetch_registry().task_finished();
            });
        if let Err(e) = spawn {
            // Can't run: record the honest error terminal on the slot and free the permit
            // (the starter may have been run from the queue, long after the command call).
            if let Some(slot) = fetch_registry().get(&part_for_starter) {
                slot.finish(Err(format!("error:audio fetch spawn failed: {e}")));
            }
            fetch_registry().task_finished();
        }
    });
    let slot = fetch_registry().submit(&part_id, starter);
    Ok(slot_to_dto(&part_id, &slot, &cache_path))
}

/// Cancel an in-flight fetch AND clear the part's slot (the user-facing X / retry-reset
/// seam: a subsequent `audio_prepare_part` starts a fresh download). Also purges a queued,
/// not-yet-started entry.
#[tauri::command]
#[specta::specta]
pub fn audio_cancel_part(part_id: String) -> Result<(), String> {
    if let Some(slot) = fetch_registry().get(&part_id) {
        slot.progress.request_cancel();
    }
    fetch_registry().remove(&part_id);
    Ok(())
}

/// Navigate-away semantics (auto-fetch S3.5): drop the part ONLY if it is still waiting in
/// the admission queue; an in-flight download is left to complete into the cache (bounded
/// by the global cap), so reopening the session finds it cached or nearly there. Returns
/// whether a queued entry was dropped.
#[tauri::command]
#[specta::specta]
pub fn audio_release_part(part_id: String) -> Result<bool, String> {
    Ok(fetch_registry().cancel_if_queued(&part_id))
}

/// Fetched-audio cache footprint for the sync panel row ("Fetched audio: N MB — Clear").
#[derive(Debug, Clone, Copy, serde::Serialize, specta::Type)]
pub struct AudioCacheStatsDto {
    pub bytes: u64,
    pub files: u64,
}

/// Total bytes + file count of this device's audio-fetch cache (decrypt temps excluded).
#[tauri::command]
#[specta::specta]
pub fn audio_cache_stats(app: tauri::AppHandle) -> Result<AudioCacheStatsDto, String> {
    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();
    let s = yapstack_sync::audio::cache_stats(&fetch_cache_dir(&db_path));
    Ok(AudioCacheStatsDto {
        bytes: s.bytes,
        files: s.files,
    })
}

/// Clear the audio-fetch cache: deletes settled cache files ONLY — never the fetch-tmp of
/// in-flight downloads (a sibling dir this command doesn't touch), never in-cache-dir
/// decrypt temps, and never a file whose part has a live fetch slot (in-flight or
/// unobserved-terminal — the simplest rule that can't corrupt a running fetch). Source
/// audio is untouched by construction (the cache dir only ever holds fetched copies).
/// Returns the remaining footprint (what the skips kept).
#[tauri::command]
#[specta::specta]
pub fn audio_cache_clear(app: tauri::AppHandle) -> Result<AudioCacheStatsDto, String> {
    let db_path = app
        .try_state::<crate::DbPath>()
        .ok_or_else(|| "db path unavailable".to_string())?
        .inner()
        .as_ref()
        .clone();
    let cache_dir = fetch_cache_dir(&db_path);
    let _removed =
        yapstack_sync::audio::cache_clear(&cache_dir, |part| fetch_registry().is_active(part));
    let s = yapstack_sync::audio::cache_stats(&cache_dir);
    Ok(AudioCacheStatsDto {
        bytes: s.bytes,
        files: s.files,
    })
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

    start_and_store_drain(&app, &session, &runtime, &sync_db)?;
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

    start_and_store_drain(&app, &session, &runtime, &sync_db)?;
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
    app: &tauri::AppHandle,
    session: &Session,
    runtime: &State<'_, SyncRuntimeState>,
    sync_db: &Path,
) -> Result<(), String> {
    let vault_key = session.vault_key()?;
    let handle = spawn_drain(
        app.clone(),
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
    fn applied_event_emitted_only_when_peer_rows_merged() {
        // Item 2 (R6): the drain fires `sync://applied` iff `applied > 0`. A clean/idle cycle
        // and a pure quarantine/replay cycle (no currently-visible rows changed) do NOT.
        let idle = outbox::DrainReport::default();
        assert!(
            applied_event_for(&idle).is_none(),
            "idle cycle emits nothing"
        );

        let quarantine_only = outbox::DrainReport {
            applied: 0,
            quarantined: 3,
            replayed: 2,
            ..Default::default()
        };
        assert!(
            applied_event_for(&quarantine_only).is_none(),
            "a quarantine/replay-only cycle changes no visible rows → no refresh"
        );

        let merged = outbox::DrainReport {
            applied: 5,
            quarantined: 1,
            replayed: 2,
            ..Default::default()
        };
        assert_eq!(
            applied_event_for(&merged),
            Some(SyncAppliedEvent {
                applied: 5,
                quarantined: 1,
                replayed: 2,
            }),
            "a merge of peer rows carries the counts to the frontend"
        );
        assert_eq!(SYNC_APPLIED_EVENT, "sync://applied");
    }

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

    // ----- A3 enable-time CRR cutover -----

    /// Build a populated NON-CRR live DB (real schema + FTS-backed segments) served by
    /// a `DbService`, returning the temp dir (kept alive), its path, and the service.
    fn cutover_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        crate::db_service::DbServiceState,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yapstack.db");
        let svc = crate::db_service::DbService::open(&path).expect("open service");
        // Mirror the frontend db.ts startup out-of-band ALTERs so the fixture matches a
        // real production live schema (the columns crr_migrate's rebuild_body expects).
        // Skip any already present in the migrated schema (as db.ts does).
        for (table, col, coldef) in schema::OUT_OF_BAND_ALTERS {
            let present = svc
                .select(
                    &format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name='{col}'"),
                    &[],
                )
                .unwrap();
            if present.is_empty() {
                svc.execute(&format!("ALTER TABLE {table} ADD COLUMN {coldef}"), &[])
                    .unwrap();
            }
        }
        svc.execute(
            "INSERT INTO sessions (id, title, source) VALUES ('s1','First','Mic')",
            &[],
        )
        .unwrap();
        svc.execute(
            "INSERT INTO sessions (id, title, source) VALUES ('s2','Second','Mic')",
            &[],
        )
        .unwrap();
        // The segments AFTER INSERT trigger populates segments_fts.
        svc.execute(
            "INSERT INTO segments (id, session_id, source, text, audio_offset_seconds, chunk_duration_seconds) \
             VALUES ('g1','s1','Mic','hello searchable world',0,1)",
            &[],
        )
        .unwrap();
        svc.execute(
            "INSERT INTO segments (id, session_id, source, text, audio_offset_seconds, chunk_duration_seconds) \
             VALUES ('g2','s1','Mic','another segment here',1,1)",
            &[],
        )
        .unwrap();
        (dir, path, std::sync::Arc::new(svc))
    }

    /// Build a live DB through the REAL db_service migration runner (all migrations,
    /// fresh) and seed a couple of rows — WITHOUT the frontend's out-of-band
    /// speaker_id ALTER. `add_speaker_id` mimics a long-lived dev DB whose frontend
    /// already patched segments.speaker_id in (the Mac-shaped 14-column divergence).
    fn migration_chain_fixture(
        add_speaker_id: bool,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        crate::db_service::DbServiceState,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yapstack.db");
        let svc = crate::db_service::DbService::open(&path).expect("open service");
        if add_speaker_id {
            svc.execute("ALTER TABLE segments ADD COLUMN speaker_id INTEGER", &[])
                .unwrap();
        }
        svc.execute(
            "INSERT INTO sessions (id, title, source) VALUES ('s1','First','Mic')",
            &[],
        )
        .unwrap();
        svc.execute(
            "INSERT INTO segments (id, session_id, source, text, audio_offset_seconds, chunk_duration_seconds) \
             VALUES ('g1','s1','Mic','hello searchable world',0,1)",
            &[],
        )
        .unwrap();
        (dir, path, std::sync::Arc::new(svc))
    }

    /// R8 regression: a FRESH-install live DB built purely by the migration chain has a
    /// 13-column `segments` (speaker_id is a frontend runtime patch, not a migration).
    /// The hardcoded rebuild expected 14 columns and crashed cutover with
    /// `segments__crr_rebuild has 14 columns but 13 values were supplied`. The derived
    /// rebuild must follow the live 13-column shape. This test — built through the REAL
    /// db_service runner, not a hand-written fixture — is the one that would have caught
    /// today's bug.
    #[test]
    fn cutover_fresh_migration_chain_no_speaker_id_succeeds() {
        let (_dir, path, svc) = migration_chain_fixture(false);
        // Precondition: the migration chain leaves segments at 13 columns, no speaker_id.
        let cols = svc
            .select("SELECT name FROM pragma_table_info('segments')", &[])
            .unwrap();
        assert_eq!(cols.len(), 13, "fresh migration chain: 13-column segments");
        assert!(
            !cols
                .iter()
                .any(|r| r["name"] == serde_json::json!("speaker_id")),
            "fresh migration chain must NOT carry speaker_id"
        );

        assert!(!live_is_crr_prepared(&path));
        let pre = synced_table_row_counts(&path).unwrap();

        perform_cutover(&path, &svc).expect("fresh-chain cutover must succeed");

        assert!(live_is_crr_prepared(&path), "live DB is now CRR");
        assert!(backup_db_path(&path).exists(), "pre-sync backup kept");
        assert_eq!(
            synced_table_row_counts(&path).unwrap(),
            pre,
            "row counts preserved"
        );

        // A post-cutover write to the reopened CRR pool captures to crsql_changes.
        svc.execute(
            "INSERT INTO segments (id, session_id, source, text, audio_offset_seconds, chunk_duration_seconds) \
             VALUES ('g2','s1','Mic','after cutover',1,1)",
            &[],
        )
        .unwrap();
        let db = CrsqlDb::open(&path).unwrap();
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM crsql_changes WHERE \"table\"='segments'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            n > 0,
            "post-cutover write must be captured by crsql_changes"
        );

        // F1: cutover now runs the out-of-band ALTER pass, so a fresh 13-col device
        // gains a CRR-tracked `speaker_id` instead of leaving it absent — the missing
        // column is exactly what quarantined every incoming speaker_id change forever in
        // the two-device UAT. It must exist AND still be part of a CRR table.
        assert!(
            schema::column_exists(db.conn(), "segments", "speaker_id").unwrap(),
            "cutover must add the out-of-band speaker_id column"
        );
        assert!(
            schema::is_crr(db.conn(), "segments").unwrap(),
            "segments must remain CRR after the wrapped out-of-band alter"
        );
    }

    /// R8 Mac-shaped divergence: a dev DB whose schema DIFFERS from the fresh chain by
    /// the runtime-patched 14th column (segments.speaker_id) must ALSO cut over cleanly.
    /// Dynamic derivation makes this automatic — the same code path adapts to 14 columns.
    #[test]
    fn cutover_dev_db_with_speaker_id_succeeds() {
        let (_dir, path, svc) = migration_chain_fixture(true);
        let cols = svc
            .select("SELECT name FROM pragma_table_info('segments')", &[])
            .unwrap();
        assert_eq!(cols.len(), 14, "dev DB: 14-column segments");
        assert_eq!(cols[13]["name"], serde_json::json!("speaker_id"));

        let pre = synced_table_row_counts(&path).unwrap();
        perform_cutover(&path, &svc).expect("dev-DB (14-col) cutover must succeed");
        assert!(live_is_crr_prepared(&path));
        assert_eq!(
            synced_table_row_counts(&path).unwrap(),
            pre,
            "row counts preserved"
        );

        // speaker_id survives the rebuild and a write to it captures cleanly.
        svc.execute(
            "INSERT INTO segments (id, session_id, source, text, audio_offset_seconds, chunk_duration_seconds, speaker_id) \
             VALUES ('g2','s1','Mic','after cutover',1,1,7)",
            &[],
        )
        .unwrap();
        let db = CrsqlDb::open(&path).unwrap();
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM crsql_changes WHERE \"table\"='segments'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            n > 0,
            "post-cutover write must be captured by crsql_changes"
        );
    }

    #[test]
    fn cutover_full_sequence_preserves_data_fts_and_prepares_crr() {
        let (_dir, path, svc) = cutover_fixture();
        assert!(!live_is_crr_prepared(&path), "starts non-CRR");
        let pre = synced_table_row_counts(&path).unwrap();

        perform_cutover(&path, &svc).expect("cutover");

        // Live DB is now CRR-prepared; the backup escape hatch exists.
        assert!(live_is_crr_prepared(&path), "live DB is now CRR");
        assert!(backup_db_path(&path).exists(), "pre-sync backup kept");

        // Row counts intact.
        assert_eq!(synced_table_row_counts(&path).unwrap(), pre);
        let rows = svc
            .select("SELECT count(*) AS c FROM sessions", &[])
            .unwrap();
        assert_eq!(rows[0]["c"], serde_json::json!(2));

        // FTS survives and MATCH works through the reopened pool.
        let hits = svc
            .select(
                "SELECT segment_id FROM segments_fts WHERE segments_fts MATCH 'searchable'",
                &[],
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["segment_id"], serde_json::json!("g1"));

        // Watermarks reset to zero on the new CRR live DB.
        let db = CrsqlDb::open(&path).unwrap();
        assert_eq!(state::push_watermark(db.conn()).unwrap(), 0);
        assert_eq!(state::pull_watermark(db.conn()).unwrap(), 0);
        drop(db);

        // Second run is a no-op (idempotency gate) — no error, still CRR.
        perform_cutover(&path, &svc).expect("second cutover is a no-op");
        assert!(live_is_crr_prepared(&path));
    }

    #[test]
    fn cutover_discards_legacy_copy_and_zeroes_watermarks() {
        let (_dir, path, svc) = cutover_fixture();
        // Simulate a device enabled under the OLD copy architecture: a stale
        // yapstack.sync.db with non-zero watermarks beside the live DB.
        let legacy = sync_db_path(&path);
        {
            let db = CrsqlDb::open(&legacy).unwrap();
            state::ensure_meta_table(db.conn()).unwrap();
            state::set_push_watermark(db.conn(), 137).unwrap();
            state::set_pull_watermark(db.conn(), 99).unwrap();
        }
        assert!(legacy.exists());

        perform_cutover(&path, &svc).expect("cutover");

        // Legacy copy discarded; new live CRR watermarks are zero.
        assert!(!legacy.exists(), "legacy copy removed");
        let db = CrsqlDb::open(&path).unwrap();
        assert_eq!(state::push_watermark(db.conn()).unwrap(), 0);
        assert_eq!(state::pull_watermark(db.conn()).unwrap(), 0);
    }

    #[test]
    fn already_enabled_non_crr_live_triggers_cutover() {
        // The drain-start decision: an enabled device whose live DB is not CRR must run
        // the cutover once. This asserts that exact guard + its effect.
        let (_dir, path, svc) = cutover_fixture();
        assert!(
            !live_is_crr_prepared(&path),
            "enabled-but-non-CRR: needs cutover"
        );
        perform_cutover(&path, &svc).expect("auto-cutover");
        assert!(
            live_is_crr_prepared(&path),
            "after auto-cutover the guard is satisfied (no re-run)"
        );
        assert!(backup_db_path(&path).exists());
    }

    #[test]
    fn cutover_rollback_before_close_leaves_live_untouched() {
        // A failure BEFORE any live handle is dropped: live DB byte-identical, no backup.
        let (_dir, path, svc) = cutover_fixture();
        // Quiesce+reopen once so the file is checkpointed and stable for byte comparison.
        svc.close_for_swap().unwrap();
        svc.reopen().unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = cutover_with_fault(&path, &svc, CutoverFault::AfterVacuum)
            .expect_err("injected fault must fail");
        assert!(err.contains("after VACUUM"));
        assert!(
            !backup_db_path(&path).exists(),
            "no backup on pre-close failure"
        );
        assert!(!live_is_crr_prepared(&path), "live DB still non-CRR");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "live DB byte-identical"
        );
        // Service still works.
        assert_eq!(
            svc.select("SELECT count(*) AS c FROM sessions", &[])
                .unwrap()[0]["c"],
            serde_json::json!(2)
        );
    }

    #[test]
    fn cutover_rollback_after_rename_restores_byte_identical_live() {
        // A failure AFTER the swap exercises the backup→live rollback (step 8).
        let (_dir, path, svc) = cutover_fixture();
        svc.close_for_swap().unwrap();
        svc.reopen().unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = cutover_with_fault(&path, &svc, CutoverFault::AfterRename)
            .expect_err("injected fault must fail");
        assert!(err.contains("after rename"));
        // The backup was renamed back to live; live is the original, non-CRR.
        assert!(!backup_db_path(&path).exists(), "backup restored into live");
        assert!(
            !live_is_crr_prepared(&path),
            "rolled back to non-CRR original"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "rollback restores byte-identical live DB"
        );
        // Service reopened and functional on the restored DB.
        assert_eq!(
            svc.select("SELECT count(*) AS c FROM sessions", &[])
                .unwrap()[0]["c"],
            serde_json::json!(2)
        );
    }

    // ----- F1 crash-safety: durable journal + boot recovery -----

    /// Build a journal for a test scenario at `phase` with the given content marker.
    fn test_journal(live: &Path, phase: CutoverPhase, pre_counts: Vec<(String, i64)>) {
        write_journal(
            live,
            &CutoverJournal {
                version: CUTOVER_JOURNAL_VERSION,
                phase,
                live_file: file_name_string(live),
                backup_file: file_name_string(&backup_db_path(live)),
                staging_file: file_name_string(&cutover_staging_path(live)),
                pre_counts,
                live_size: 0,
            },
        )
        .unwrap();
    }

    /// Crash-window simulation: an injected fault BETWEEN the two renames leaves a
    /// mid-swap state on disk; boot recovery COMPLETES the swap from the durable journal
    /// with zero data loss, a cleaned journal, and the backup kept.
    #[test]
    fn cutover_crash_between_renames_recovers_via_journal() {
        let (_dir, path, svc) = cutover_fixture();
        svc.close_for_swap().unwrap();
        svc.reopen().unwrap();
        let pre = all_user_table_row_counts(&path).unwrap();

        let err = cutover_with_fault(&path, &svc, CutoverFault::BetweenRenames)
            .expect_err("injected mid-swap fault");
        assert!(err.contains("between renames"), "{err}");

        // Mid-swap on disk: live gone, backup + staging + journal present.
        assert!(!path.exists(), "live renamed to backup");
        assert!(backup_db_path(&path).exists(), "backup present");
        assert!(cutover_staging_path(&path).exists(), "staging present");
        assert!(cutover_journal_path(&path).exists(), "journal present");

        recover_interrupted_cutover(&path);

        assert!(path.exists(), "live restored");
        assert!(!cutover_journal_path(&path).exists(), "journal cleaned");
        assert!(backup_db_path(&path).exists(), "backup kept");
        assert_eq!(
            all_user_table_row_counts(&path).unwrap(),
            pre,
            "no data loss across the recovered swap"
        );
        assert!(
            live_is_crr_prepared(&path),
            "recovery completed the swap: live is the CRR DB"
        );

        // A fresh service opens the recovered live and sees the real data.
        let svc2 = std::sync::Arc::new(crate::db_service::DbService::open(&path).unwrap());
        assert_eq!(
            svc2.select("SELECT count(*) AS c FROM sessions", &[])
                .unwrap()[0]["c"],
            serde_json::json!(2)
        );
    }

    /// `fsync_file` MUST open the file for WRITE before `sync_all`. On unix a read-only
    /// open would also pass here, so this test's real job is to PIN the write-access open
    /// (a read-only `File::open` returns ERROR_ACCESS_DENIED / os error 5 on Windows).
    #[test]
    fn fsync_file_succeeds_on_freshly_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("durable.bin");
        std::fs::write(&path, b"payload").unwrap();
        // The exact call `write_journal`/`copy_into_live` make: must not error.
        fsync_file(&path).expect("fsync_file must succeed on a write-opened handle");
        // Opening for write must not have truncated the existing content.
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
    }

    /// Boot-recovery matrix — (journal + BackupMoved, valid staging) → complete the swap.
    #[test]
    fn recovery_matrix_journal_backup_moved_valid_staging_completes() {
        let (_dir, path, svc) = cutover_fixture();
        svc.close_for_swap().unwrap(); // drop handles + checkpoint
        let counts = all_user_table_row_counts(&path).unwrap();
        let backup = backup_db_path(&path);
        let staging = cutover_staging_path(&path);
        std::fs::copy(&path, &staging).unwrap(); // a valid staging candidate
        std::fs::rename(&path, &backup).unwrap(); // live→backup done
        test_journal(&path, CutoverPhase::BackupMoved, counts.clone());

        recover_interrupted_cutover(&path);

        assert!(path.exists(), "staging→live completed");
        assert!(!staging.exists(), "valid staging consumed");
        assert!(backup.exists(), "backup kept");
        assert!(!cutover_journal_path(&path).exists(), "journal removed");
        assert_eq!(all_user_table_row_counts(&path).unwrap(), counts);
    }

    /// Boot-recovery matrix — (no journal, live missing, backup present) → restore backup.
    #[test]
    fn recovery_matrix_no_journal_live_missing_backup_restores() {
        let (_dir, path, svc) = cutover_fixture();
        svc.close_for_swap().unwrap();
        let counts = all_user_table_row_counts(&path).unwrap();
        let backup = backup_db_path(&path);
        std::fs::rename(&path, &backup).unwrap();
        assert!(!path.exists());

        recover_interrupted_cutover(&path);

        assert!(path.exists(), "restored from backup");
        assert!(backup.exists(), "backup kept (copied, not consumed)");
        assert_eq!(all_user_table_row_counts(&path).unwrap(), counts);
        assert!(!cutover_journal_path(&path).exists());
    }

    /// Boot-recovery matrix — (no journal, live missing, staging only) → restore staging.
    #[test]
    fn recovery_matrix_no_journal_live_missing_staging_only_restores() {
        let (_dir, path, svc) = cutover_fixture();
        svc.close_for_swap().unwrap();
        let counts = all_user_table_row_counts(&path).unwrap();
        let staging = cutover_staging_path(&path);
        std::fs::rename(&path, &staging).unwrap(); // staging is the only copy
        assert!(!path.exists());

        recover_interrupted_cutover(&path);

        assert!(path.exists(), "restored from staging");
        assert!(!staging.exists(), "staging consumed (it was the only copy)");
        assert_eq!(all_user_table_row_counts(&path).unwrap(), counts);
    }

    /// Boot-recovery matrix — (journal present but staging corrupt) → roll back to backup,
    /// deleting nothing except the completed journal.
    #[test]
    fn recovery_matrix_journal_present_staging_corrupt_rolls_back_to_backup() {
        let (_dir, path, svc) = cutover_fixture();
        svc.close_for_swap().unwrap();
        let counts = all_user_table_row_counts(&path).unwrap();
        let backup = backup_db_path(&path);
        let staging = cutover_staging_path(&path);
        std::fs::rename(&path, &backup).unwrap();
        std::fs::write(&staging, b"not a sqlite database").unwrap(); // corrupt staging
        test_journal(&path, CutoverPhase::BackupMoved, counts.clone());

        recover_interrupted_cutover(&path);

        assert!(path.exists(), "restored from backup (staging was corrupt)");
        assert!(backup.exists(), "backup kept");
        assert!(staging.exists(), "corrupt staging NOT deleted");
        assert!(
            !cutover_journal_path(&path).exists(),
            "only the journal was removed"
        );
        assert_eq!(
            all_user_table_row_counts(&path).unwrap(),
            counts,
            "the real data is live"
        );
    }

    /// F1.4 empty-live guard: refuse to cut over a freshly-created empty live DB while
    /// recovery artifacts are present (rather than enshrining the empty DB).
    #[test]
    fn cutover_refuses_empty_live_with_recovery_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yapstack.db");
        let svc = std::sync::Arc::new(crate::db_service::DbService::open(&path).unwrap());
        // A leftover recovery artifact beside a fresh, empty live DB.
        std::fs::write(backup_db_path(&path), b"pretend-prior-backup").unwrap();
        assert!(!live_is_crr_prepared(&path));

        let err = perform_cutover(&path, &svc).expect_err("must refuse an empty live");
        assert!(err.contains("cutover refused"), "{err}");
        // The empty live was NOT turned into a CRR DB.
        assert!(!live_is_crr_prepared(&path));
    }

    /// F1.3 stale-backup preservation: a pre-existing backup is MOVED aside (timestamped),
    /// never deleted; both files exist afterward and the old bytes survive verbatim.
    #[test]
    fn cutover_moves_existing_backup_aside_never_deletes_it() {
        let (dir, path, svc) = cutover_fixture();
        let backup = backup_db_path(&path);
        std::fs::write(&backup, b"PRIOR-BACKUP-KEEP-ME").unwrap();

        perform_cutover(&path, &svc).expect("cutover");

        assert!(backup.exists(), "new pre-sync backup present");
        let mut aside = None;
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if name.starts_with("yapstack.db.pre-sync-backup.") {
                aside = Some(dir.path().join(name));
            }
        }
        let aside = aside.expect("moved-aside timestamped backup must exist");
        assert_eq!(
            std::fs::read(&aside).unwrap(),
            b"PRIOR-BACKUP-KEEP-ME",
            "the old backup was preserved verbatim, not deleted"
        );
        assert!(live_is_crr_prepared(&path), "cutover still completed");
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
        let _ = store_delete(KEYCHAIN_ACCOUNT, "test_reset");
        let _ = store_delete(KEYCHAIN_ACCOUNT_IDENTITY, "test_reset");
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
        let _ = store_delete(KEYCHAIN_ACCOUNT_IDENTITY, "test_reset");
        let _ = store_delete(KEYCHAIN_ACCOUNT, "test_reset");
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
            fn delete_key(&self, _reason: &str) -> Result<(), String> {
                *self.key.borrow_mut() = None;
                Ok(())
            }
        }

        /// Thread-safe stand-in for the wrapping-key store, for the concurrency test (the
        /// `RefCell`-backed `FakeKeyStore` is `!Sync` and cannot cross threads).
        #[derive(Default)]
        struct SyncFakeKeyStore {
            key: std::sync::Mutex<Option<String>>,
        }
        impl SessionKeyStore for SyncFakeKeyStore {
            fn get_key(&self) -> Result<Option<String>, String> {
                Ok(self.key.lock().unwrap_or_else(|e| e.into_inner()).clone())
            }
            fn set_key(&self, b64: &str) -> Result<(), String> {
                *self.key.lock().unwrap_or_else(|e| e.into_inner()) = Some(b64.to_string());
                Ok(())
            }
            fn delete_key(&self, _reason: &str) -> Result<(), String> {
                *self.key.lock().unwrap_or_else(|e| e.into_inner()) = None;
                Ok(())
            }
        }

        fn temp_enc_path() -> PathBuf {
            std::env::temp_dir().join(format!("yapstack-sess-test-{}.enc", Uuid::new_v4()))
        }

        /// True if any tmp sibling of `final_path` (`<final>.tmp.*`, the R10.1 unique-tmp scheme)
        /// currently exists — a torn/leftover write. Used by the durability + cleanup tests.
        fn any_session_tmp_present(final_path: &Path) -> bool {
            let Some(dir) = final_path.parent() else {
                return false;
            };
            let Some(name) = final_path.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            let prefix = format!("{name}.tmp.");
            let Ok(entries) = std::fs::read_dir(dir) else {
                return false;
            };
            entries.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix))
            })
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
        fn sealed_path_is_identical_at_boot_read_and_signin_write() {
            // R6 item 4(c): the sealed-session path MUST be the same at the boot READ and the
            // sign-in WRITE. Both go through `resolve_session_enc_path(Some(config_dir))` with
            // the SAME resolved store dir, so they are byte-identical.
            let config_dir = std::env::temp_dir().join(format!("yapstack-cfg-{}", Uuid::new_v4()));
            let boot_read = resolve_session_enc_path(Some(&config_dir)).unwrap();
            let signin_write = resolve_session_enc_path(Some(&config_dir)).unwrap();
            assert_eq!(
                boot_read, signin_write,
                "boot read and sign-in write resolve to the SAME sealed file"
            );
            assert_eq!(
                boot_read,
                config_dir.join(SESSION_ENC_FILENAME),
                "resolves under the config dir, deterministically"
            );
        }

        #[test]
        fn pre_init_fallback_path_diverges_from_config_path() {
            // The bug this guards against: a session touch BEFORE `init_credential_store` sets
            // SESSION_STORE_DIR resolves via the `None` → temp-dir fallback, which does NOT
            // equal the post-init config-dir path — so a boot read that raced ahead of init
            // would look in the temp dir while sign-in wrote under the config dir. lib.rs now
            // hoists init before every session touch; this asserts the divergence the reorder
            // eliminates (config_dir is deliberately not the temp dir).
            let config_dir = std::env::temp_dir().join(format!("yapstack-cfg-{}", Uuid::new_v4()));
            // In a debug (test) build the temp fallback is retained (dev ergonomics), so a
            // `None` store dir resolves under the temp dir. R10 fix 2 removes this fallback in
            // RELEASE; the release-error behaviour is asserted via the seam below.
            let pre_init = resolve_session_enc_path(None).unwrap();
            let post_init = resolve_session_enc_path(Some(&config_dir)).unwrap();
            assert_ne!(
                pre_init, post_init,
                "pre-init temp fallback must NOT collide with the config-dir path (the mismatch)"
            );
            assert_eq!(pre_init, std::env::temp_dir().join(SESSION_ENC_FILENAME));
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
        fn missing_file_degrades_and_preserves_stray_key() {
            // R10 fix 1: a wrapping key with NO session file degrades signed-out but PRESERVES
            // the key. A transiently-missing file (unsynced/rolled-back write, path hiccup, AV
            // quarantine) is indistinguishable from a truly orphaned key here, and deleting the
            // key would escalate a recoverable transient into permanent session destruction.
            // Only sign-out / overwrite may delete — never a read/boot path.
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();
            std::fs::remove_file(&path).unwrap();

            assert!(load_session_wrapped(&ks, &path).unwrap().is_none());
            // The wrapping key is PRESERVED (previously this arm deleted it).
            assert!(
                ks.get_key().unwrap().is_some(),
                "the wrapping key must be preserved across a transiently-missing file"
            );
        }

        #[test]
        fn missing_key_degrades_but_preserves_the_file() {
            // R5: a session file with NO wrapping-key entry degrades signed-out — but the file
            // is PRESERVED, not deleted. A transient keychain miss (the Windows-persistence
            // failure R5 fixes) is indistinguishable from a truly orphaned file at this point,
            // and deleting would destroy the only recoverable artifact. If the key returns next
            // boot, the session recovers.
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();
            // Snapshot the entry so we can restore it (modelling a keychain miss that resolves).
            let saved_key = ks.get_key().unwrap().unwrap();
            ks.delete_key("test_reset").unwrap();

            assert!(load_session_wrapped(&ks, &path).unwrap().is_none());
            // The file must SURVIVE the key-miss degrade.
            assert!(
                read_session_blob(&path).unwrap().is_some(),
                "the session file must be preserved across a wrapping-key miss"
            );
            // And when the wrapping key comes back, the same file opens the session again.
            ks.set_key(&saved_key).unwrap();
            assert!(
                load_session_wrapped(&ks, &path).unwrap().is_some(),
                "the session recovers once the wrapping-key entry returns"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn windows_cred_target_name_matches_keyring_convention() {
            // R5 wire-compat: keyring's Windows backend keys the credential by
            // `{username}.{service}` (keyring-3.6.3/src/windows.rs:378). Our shim MUST produce
            // the identical target so an entry written by either is readable by the other.
            assert_eq!(
                cred_target_name(KEYCHAIN_ACCOUNT_SESSION_KEY, KEYCHAIN_SERVICE),
                "session-key-v1.dev.yapstack.app.sync"
            );
            assert_eq!(
                cred_target_name(KEYCHAIN_ACCOUNT_IDENTITY, KEYCHAIN_SERVICE),
                "identity-v1.dev.yapstack.app.sync"
            );
        }

        #[test]
        fn windows_cred_blob_is_keyring_utf16le_and_round_trips() {
            // R5 wire-compat: keyring stores the value as little-endian UTF-16 with NO NUL
            // terminator (keyring-3.6.3/src/windows.rs:86-88). Assert the exact byte layout and
            // a decode round-trip over ASCII, multi-byte, and non-BMP (surrogate-pair) chars —
            // the wrapping-key entry is base64 (ASCII) but the encoding must be fully general.
            assert_eq!(cred_encode_blob("AB"), vec![0x41, 0x00, 0x42, 0x00]);
            assert_eq!(cred_encode_blob(""), Vec::<u8>::new());
            for v in ["", "session-key-b64==", "héllo 世界", "emoji 😀 tail"] {
                assert_eq!(
                    cred_decode_blob(&cred_encode_blob(v)),
                    v,
                    "UTF-16LE blob must round-trip for {v:?}"
                );
                // No NUL terminator: byte length is exactly 2 per UTF-16 code unit.
                assert_eq!(cred_encode_blob(v).len(), v.encode_utf16().count() * 2);
            }
        }

        #[test]
        fn undecodable_wrap_entry_degrades_and_preserves() {
            // R10 fix 1 (reverses the old R5 cleanup): a wrapping-key ENTRY that no longer
            // parses degrades signed-out but is PRESERVED — no deletion on a read/boot path. A
            // transient store returning a stale/garbled value is indistinguishable from a truly
            // corrupt entry here, and deleting would destroy a recoverable component. A fresh
            // sign-in overwrites it (the ONLY non-sign-out replacement), un-wedging the store
            // without ever deleting on a read.
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();
            // Overwrite the entry with a bare base64 string (not the `WrapKeyEntry` JSON).
            ks.set_key(&B64.encode([4u8; 32])).unwrap();
            assert!(
                decode_wrap_entry(&ks.get_key().unwrap().unwrap()).is_err(),
                "precondition: the entry is genuinely undecodable"
            );

            assert!(load_session_wrapped(&ks, &path).unwrap().is_none());
            assert!(
                ks.get_key().unwrap().is_some(),
                "the undecodable wrapping-key entry must be PRESERVED (no delete on a read path)"
            );
            // A fresh sign-in overwrites the stale entry and the session opens again.
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();
            assert!(
                load_session_wrapped(&ks, &path).unwrap().is_some(),
                "a fresh sign-in overwrites the stale entry and recovers the session"
            );
            let _ = std::fs::remove_file(&path);
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

        #[test]
        fn release_resolution_without_store_dir_is_error_not_temp() {
            // R10 fix 2: model the RELEASE build via the seam (allow_temp_fallback = false). An
            // unresolved store dir must be an Err — NEVER a silent %TEMP% path — so a sign-in
            // WRITE fails loudly and a boot READ degrades with a clear log instead of sealing the
            // real session under a temp dir.
            let err = resolve_session_enc_path_seam(None, false);
            assert!(
                err.is_err(),
                "release resolution with no store dir must ERROR, not fall back to %TEMP%"
            );
            // With a store dir it resolves deterministically under it, in BOTH modes.
            let dir = std::env::temp_dir().join(format!("yapstack-r10-{}", Uuid::new_v4()));
            assert_eq!(
                resolve_session_enc_path_seam(Some(&dir), false).unwrap(),
                dir.join(SESSION_ENC_FILENAME)
            );
            assert_eq!(
                resolve_session_enc_path_seam(Some(&dir), true).unwrap(),
                dir.join(SESSION_ENC_FILENAME)
            );
            // The DEBUG seam retains the temp fallback (dev ergonomics + unit tests).
            assert_eq!(
                resolve_session_enc_path_seam(None, true).unwrap(),
                std::env::temp_dir().join(SESSION_ENC_FILENAME)
            );
        }

        #[test]
        fn boot_read_error_leaves_cache_unloaded_for_retry() {
            // R10 fix 3: a transient store READ error (Err) must NOT be cached as signed-out —
            // `resolve_cache_from_reads` returns loaded=false so a later poll-driven access
            // retries. A clean Ok(None) IS a genuine signed-out and marks the cache loaded.
            let (s, i, loaded) = resolve_cache_from_reads(Err("boom".into()), Ok(None));
            assert!(s.is_none() && i.is_none());
            assert!(
                !loaded,
                "a read Err must leave the cache UNLOADED (retry), not cache signed-out"
            );

            // A clean signed-out marks the cache loaded (no needless retry).
            let (_, _, loaded_clean) = resolve_cache_from_reads(Ok(None), Ok(None));
            assert!(
                loaded_clean,
                "a clean Ok(None) is a genuine signed-out → cache it"
            );

            // A subsequent SUCCESSFUL read populates the cache.
            let (s3, _, loaded3) = resolve_cache_from_reads(Ok(Some(sample_session())), Ok(None));
            assert!(
                s3.is_some() && loaded3,
                "a later successful read populates the cache"
            );

            // An identity-read error alone also blocks marking loaded.
            let (_, _, loaded4) = resolve_cache_from_reads(Ok(None), Err("id-boom".into()));
            assert!(
                !loaded4,
                "an identity read Err also leaves the cache unloaded"
            );
        }

        #[test]
        fn session_write_is_durable_tmp_gone_final_present_roundtrips() {
            // R10 fix 4: the write is tmp → fsync file → rename → fsync parent dir (fsync by
            // construction via the shared helpers). Behaviourally: the temp file is gone (renamed
            // away, never left torn), the final file is present, and the content round-trips.
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            let s = sample_session();
            store_session_wrapped(&ks, &path, &s).unwrap();

            // R10.1 N1: the tmp name is now unique (`<final>.tmp.<pid>.<seq>`), so assert NO tmp
            // sibling of the final file survives the write (renamed away + swept), rather than
            // checking one fixed legacy name.
            assert!(
                !any_session_tmp_present(&path),
                "no atomic-write temp file may survive the write (renamed away, never left torn)"
            );
            assert!(
                path.exists(),
                "the final sealed file must be present after the write"
            );
            let loaded = load_session_wrapped(&ks, &path).unwrap().unwrap();
            assert_eq!(json_of(&loaded), json_of(&s), "content must round-trip");
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn sign_out_removes_both_components_and_reason_inventory_is_exact() {
            // R10 fix 5: sign-out (`clear_session_wrapped`) removes BOTH the sealed file and the
            // wrapping-key entry, carrying the `user_sign_out` reason. After fix 1 the ONLY two
            // production deletion/replacement reasons are sign-out and overwrite — assert that
            // exact inventory here.
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            store_session_wrapped(&ks, &path, &sample_session()).unwrap();
            assert!(ks.get_key().unwrap().is_some());
            assert!(path.exists());

            clear_session_wrapped(&ks, &path).unwrap();
            assert!(
                ks.get_key().unwrap().is_none(),
                "sign-out must clear the wrapping-key entry"
            );
            assert!(
                !path.exists(),
                "sign-out must remove the sealed session file"
            );

            // The exhaustive production reason inventory.
            assert_eq!(DELETE_REASON_SIGN_OUT, "user_sign_out");
            assert_eq!(DELETE_REASON_OVERWRITE, "overwrite");
        }

        #[test]
        fn concurrent_session_writes_never_tear_the_final_file() {
            // R10.1 N1: two threads write DIFFERENT sessions to the SAME file+key pair in a
            // tight loop. `session_store_lock` (inside `store_session_wrapped`) serializes them,
            // and each write owns a unique tmp — so no write ever errors on a tmp collision and
            // the final file is ALWAYS one of the two sessions, INTACT (round-trips), never torn.
            let ks = std::sync::Arc::new(SyncFakeKeyStore::default());
            let path = temp_enc_path();

            let mut s_a = sample_session();
            s_a.email = "aaa@example.com".into();
            s_a.tenant_id = Uuid::from_u128(0xA);
            let mut s_b = sample_session();
            s_b.email = "bbb@example.com".into();
            s_b.tenant_id = Uuid::from_u128(0xB);
            // Same epoch so either session opens under the shared wrapping-key entry's epoch.
            assert_eq!(s_a.epoch, s_b.epoch);

            const N: usize = 60;
            let handles: Vec<_> = [s_a.clone(), s_b.clone()]
                .into_iter()
                .map(|s| {
                    let ks = ks.clone();
                    let path = path.clone();
                    std::thread::spawn(move || {
                        for _ in 0..N {
                            // Must NEVER Err (no tmp collisions, no torn intermediate).
                            store_session_wrapped(&*ks, &path, &s).unwrap();
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }

            // The published file must open cleanly and equal exactly ONE of the two sessions.
            let loaded = load_session_wrapped(&*ks, &path)
                .unwrap()
                .expect("a fully-written session must be present, never torn");
            assert!(
                json_of(&loaded) == json_of(&s_a) || json_of(&loaded) == json_of(&s_b),
                "the surviving session must be one of the two writers' sessions, intact"
            );
            // No tmp litter after the storm.
            assert!(
                !any_session_tmp_present(&path),
                "no tmp sibling may survive concurrent writes"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn session_tmp_path_is_unique_per_write_and_prefixed_by_final() {
            // R10.1 N1 fix 2: each write gets a private tmp `<final>.tmp.<pid>.<seq>` — distinct
            // per seq and unambiguously a sibling of (prefixed by) the final file name.
            let final_path = std::env::temp_dir().join(SESSION_ENC_FILENAME);
            let t0 = session_tmp_path(&final_path, 0);
            let t1 = session_tmp_path(&final_path, 1);
            assert_ne!(t0, t1, "different seqs → different tmp files");
            let prefix = format!("{SESSION_ENC_FILENAME}.tmp.");
            for t in [&t0, &t1] {
                let name = t.file_name().unwrap().to_str().unwrap();
                assert!(
                    name.starts_with(&prefix),
                    "tmp name {name} must be prefixed by the final file name"
                );
                assert_eq!(t.parent(), final_path.parent(), "tmp is a sibling of final");
            }
        }

        #[test]
        fn stale_session_tmp_is_swept_on_successful_write() {
            // R10.1 N1 fix 2: a stray tmp from a prior crashed write is harmless litter and is
            // cleaned by the next successful write of the SAME final file.
            let ks = FakeKeyStore::default();
            let path = temp_enc_path();
            // Plant a stale tmp as if a previous write had crashed after write() before rename().
            let stale = session_tmp_path(&path, 999);
            std::fs::write(&stale, b"garbage-from-a-crashed-write").unwrap();
            assert!(stale.exists());

            store_session_wrapped(&ks, &path, &sample_session()).unwrap();

            assert!(
                !stale.exists(),
                "a stale tmp must be swept on the next successful write"
            );
            assert!(
                !any_session_tmp_present(&path),
                "no tmp sibling may remain after a successful write"
            );
            assert!(path.exists(), "the final sealed file is present");
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn recovery_line_emits_once_only_after_an_error_then_success() {
            // R10.1 N2: the one-shot "session recovered on retry" line fires IFF a boot trace
            // recorded a read error AND a later load succeeded AND it has not fired yet.
            // Error boot alone → no line.
            assert!(!should_emit_session_recovered(true, false, false));
            // No prior error → never a recovery line (a clean boot needs no correction).
            assert!(!should_emit_session_recovered(false, true, false));
            // Error-then-success → emit exactly once.
            assert!(should_emit_session_recovered(true, true, false));
            // Already emitted → silent thereafter.
            assert!(!should_emit_session_recovered(true, true, true));
        }

        #[test]
        fn read_log_step_logs_first_failure_and_recovery_only() {
            // R10.1 N3: transition-gated — the FIRST failure and the recovery log; steady-state
            // failing polls and steady-state healthy polls are silent (bounded log growth).
            let mut prev = false;
            // First failure surfaces.
            assert_eq!(read_log_step(&mut prev, true), ReadLogEdge::Failure);
            // Repeated failures are silent (this is the unbounded-warn fix).
            assert_eq!(read_log_step(&mut prev, true), ReadLogEdge::Silent);
            assert_eq!(read_log_step(&mut prev, true), ReadLogEdge::Silent);
            // Recovery surfaces once.
            assert_eq!(read_log_step(&mut prev, false), ReadLogEdge::Recovery);
            // Steady healthy is silent.
            assert_eq!(read_log_step(&mut prev, false), ReadLogEdge::Silent);
            // A fresh failure edge surfaces again (no permanent suppression).
            assert_eq!(read_log_step(&mut prev, true), ReadLogEdge::Failure);
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
