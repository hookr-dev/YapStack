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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use rand::RngCore;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{Manager, State};
use uuid::Uuid;

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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncInfoDto {
    server_url: String,
    version: String,
    billing_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRosterEntryDto {
    fingerprint: String,
    is_self: bool,
    pending: bool,
    label: Option<String>,
}

#[derive(Serialize)]
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

// ----- Tauri commands (contract mirrors apps/desktop/src/lib/sync.ts) -----

#[tauri::command]
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
pub fn sync_status() -> Result<SyncStatusDto, String> {
    let session = load_session()?;
    match session {
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
        Some(s) => Ok(SyncStatusDto {
            phase: if s.sync_enabled {
                "connected".into()
            } else {
                "connecting".into()
            },
            server_url: s.server_url,
            email: Some(s.email),
            device_fingerprint: s.device_fingerprint,
            // Roster/epoch surface once the ceremony (T011) populates them.
            roster: vec![],
            vault_key_epoch: Some(s.epoch),
            roster_fingerprint: None,
            sync_enabled: s.sync_enabled,
            last_error: None,
            billing_url: None,
        }),
    }
}

#[tauri::command]
pub fn sync_enable(
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
    sync_status()
}

/// Vault-wrap a plaintext secret (AI apiKey / baseUrl) under the vault key held
/// in the OS keychain, before it can reach any syncable surface (deliverable E).
/// The plaintext is consumed here and never persisted; the caller stores only
/// the returned committing envelope (CRYPTO_SPEC §1.4 / §4).
#[tauri::command]
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
pub fn sync_sign_out(runtime: State<'_, SyncRuntimeState>) -> Result<(), String> {
    if let Ok(mut guard) = runtime.lock() {
        if let Some(mut handle) = guard.take() {
            handle.stop();
        }
    }
    clear_session()
}

// --- Relay auth ceremony: client-crypto is implemented in yapstack-crypto, but
//     the end-to-end HTTP round-trips against the live relay + the two-device
//     approval flow require a running relay and a second machine to validate.
//     Wiring + verification is T011 (relay auth) / T012 (§15 + owner UAT). These
//     handlers exist so the frontend contract is complete and fail loudly rather
//     than silently no-op.

const CEREMONY_PENDING: &str =
    "Relay sign-in is not wired in this build. The device sync runtime (drain, \
     crr_migrate, keychain vault) is ready; the auth ceremony lands with the \
     two-device relay integration.";

#[tauri::command]
pub fn sync_signup(_req: serde_json::Value) -> Result<serde_json::Value, String> {
    Err(CEREMONY_PENDING.into())
}

#[tauri::command]
pub fn sync_login_begin(_server_url: String, _email: String) -> Result<serde_json::Value, String> {
    Err(CEREMONY_PENDING.into())
}

#[tauri::command]
pub fn sync_login_finish(_password: String) -> Result<SyncStatusDto, String> {
    Err(CEREMONY_PENDING.into())
}

#[tauri::command]
pub fn sync_recover(
    _server_url: String,
    _email: String,
    _recovery_code: String,
) -> Result<SyncStatusDto, String> {
    Err(CEREMONY_PENDING.into())
}

#[tauri::command]
pub fn sync_approve_device(_fingerprint: String) -> Result<SyncStatusDto, String> {
    Err(CEREMONY_PENDING.into())
}
