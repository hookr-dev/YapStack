mod commands;
mod db;
mod db_service;
mod device_broker;
mod logging;
mod system_volume;
// YapStack Sync (Gate 5B, T010b). Behind the off-by-default `sync` feature so
// the default build never links the vendored cr-sqlite CRR engine (nightly +
// panic=abort). See src/sync.rs for the isolation rationale.
#[cfg(feature = "sync")]
mod sync;

const WINDOW_MAIN: &str = "main";
#[cfg(target_os = "macos")]
const WINDOW_DICTATION: &str = "dictation";
#[cfg(target_os = "macos")]
const WINDOW_RECORDING_INDICATOR: &str = "recording-indicator";
#[cfg(target_os = "macos")]
const WINDOW_INSIGHT: &str = "insight";

use std::collections::HashSet;
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tauri::{
    menu::{Menu, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder},
    tray::{TrayIcon, TrayIconBuilder},
    AppHandle, Emitter, Manager, RunEvent,
};
#[cfg(target_os = "macos")]
use tauri_nspanel::{CollectionBehavior, PanelLevel, StyleMask, WebviewWindowExt};
use tokio::sync::{watch, Mutex};
use yapstack_audio::AudioManager;
use yapstack_transcription::ModelManager;

// Lock ordering (acquire in this order to prevent deadlocks):
//   1. AudioManagerState
//   2. TranscriptionSchedulerState
//   3. LiveTranscriptionState
//   4. ModelManagerState
//   5. TrayState
use commands::audio::{AudioManagerState, BufferStatusDto, CaptureStatusDto, RingBufferInfoDto};

#[cfg(target_os = "macos")]
tauri_nspanel::tauri_panel! {
    panel!(OverlayPanel {
        config: {
            can_become_key_window: false,
            is_floating_panel: true
        }
    })
}

/// Returns true if buffer info has meaningfully changed for UI purposes.
/// Ignores `samples_written` and `available_samples` (change every poll but
/// aren't displayed). Coarsens `available_seconds` to 1-second granularity.
fn buffer_display_changed(
    prev: &commands::audio::BufferStatusDto,
    curr: &commands::audio::BufferStatusDto,
) -> bool {
    fn info_changed(
        a: &Option<commands::audio::RingBufferInfoDto>,
        b: &Option<commands::audio::RingBufferInfoDto>,
    ) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => {
                a.available_seconds as u32 != b.available_seconds as u32
                    || a.capacity_seconds != b.capacity_seconds
                    || a.sample_rate != b.sample_rate
                    || a.channels != b.channels
            }
            (None, None) => false,
            _ => true,
        }
    }
    info_changed(&prev.mic, &curr.mic) || info_changed(&prev.system, &curr.system)
}

fn build_tray_menu(
    app: &AppHandle,
    is_capturing: bool,
    is_recording: bool,
) -> tauri::Result<Menu<tauri::Wry>> {
    let status_text = if is_recording {
        "Status: Recording"
    } else if is_capturing {
        "Status: Listening"
    } else {
        "Status: Idle"
    };
    let status = MenuItemBuilder::with_id("status", status_text)
        .enabled(false)
        .build(app)?;

    let sep1 = PredefinedMenuItem::separator(app)?;

    let can_start_session = is_capturing && !is_recording;
    let new_session = MenuItemBuilder::with_id("new_session", "New Session")
        .enabled(can_start_session)
        .build(app)?;

    let backfill_submenu =
        SubmenuBuilder::with_id(app, "backfill_submenu", "New Session with Rewind")
            .enabled(can_start_session)
            .item(
                &MenuItemBuilder::with_id("new_session_bf_30", "30 Seconds")
                    .enabled(can_start_session)
                    .build(app)?,
            )
            .item(
                &MenuItemBuilder::with_id("new_session_bf_60", "1 Minute")
                    .enabled(can_start_session)
                    .build(app)?,
            )
            .item(
                &MenuItemBuilder::with_id("new_session_bf_120", "2 Minutes")
                    .enabled(can_start_session)
                    .build(app)?,
            )
            .item(
                &MenuItemBuilder::with_id("new_session_bf_300", "5 Minutes")
                    .enabled(can_start_session)
                    .build(app)?,
            )
            .item(
                &MenuItemBuilder::with_id("new_session_bf_all", "All Available")
                    .enabled(can_start_session)
                    .build(app)?,
            )
            .build()?;

    let sep2 = PredefinedMenuItem::separator(app)?;

    let open = MenuItemBuilder::with_id("open", "Open YapStack").build(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit YapStack").build(app)?;

    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&open, &sep1, &status];

    let listen_toggle = if is_capturing {
        MenuItemBuilder::with_id("stop_capture", "Stop Listening")
            .enabled(!is_recording)
            .build(app)?
    } else {
        MenuItemBuilder::with_id("start_capture", "Start Listening").build(app)?
    };
    items.push(&listen_toggle);

    items.push(&sep2 as &dyn tauri::menu::IsMenuItem<tauri::Wry>);
    items.push(&new_session);
    items.push(&backfill_submenu);

    // Conditionally add "Stop Session" only when recording
    let stop_session;
    if is_recording {
        stop_session = MenuItemBuilder::with_id("stop_session", "Stop Session").build(app)?;
        items.push(&stop_session);
    }

    items.extend([&sep3 as &dyn tauri::menu::IsMenuItem<tauri::Wry>, &quit]);

    Menu::with_items(app, &items)
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_MAIN) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

type TrayState = Arc<Mutex<Option<TrayIcon>>>;
type ShutdownSignal = Arc<watch::Sender<bool>>;

/// Set of canonicalized directories the `audio-stream://` protocol handler
/// and `delete_audio_files` will operate on. Seeded at startup from
/// `$APP_DATA_DIR/audio` plus every distinct directory referenced by
/// `session_audio_parts.file_path`, then appended whenever Rust finalizes a
/// new part. Held in a sync `Mutex` because the protocol handler runs on a
/// non-async thread.
pub type TrustedAudioDirs = Arc<StdMutex<HashSet<PathBuf>>>;

/// Path to the SQLite DB tauri-plugin-sql owns. Stored as state so commands
/// (live finalize, reconciliation) can open their own `rusqlite` connections
/// without re-resolving `app_config_dir` every time.
pub type DbPath = Arc<PathBuf>;

fn update_tray_menu(app: &AppHandle, is_capturing: bool, is_recording: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let tray_state = app.state::<TrayState>();
        let tray_guard = tray_state.lock().await;
        if let Some(tray) = tray_guard.as_ref() {
            if let Ok(menu) = build_tray_menu(&app, is_capturing, is_recording) {
                let _ = tray.set_menu(Some(menu));
            }
        }
    });
}

pub(crate) fn is_allowed_audio_path(audio_base_dir: &Path, abs_path: &Path) -> bool {
    let resolved = match std::fs::canonicalize(abs_path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let canonical_base = match std::fs::canonicalize(audio_base_dir) {
        Ok(p) => p,
        Err(_) => return false,
    };
    resolved.starts_with(canonical_base)
}

/// True if `abs_path` resolves under any of the directories registered in
/// `TrustedAudioDirs`. The set is seeded from `session_audio_parts` at
/// startup and grown whenever Rust finalizes a part, so it tracks every
/// location the user has actually saved audio to — not just the current
/// `audioSaveLocation` setting.
pub(crate) fn audio_path_trusted(app: &AppHandle, abs_path: &Path) -> bool {
    let Some(state) = app.try_state::<TrustedAudioDirs>() else {
        return false;
    };
    let Ok(guard) = state.lock() else {
        return false;
    };
    guard.iter().any(|dir| is_allowed_audio_path(dir, abs_path))
}

/// True if `dir` (canonicalized) is registered in `TrustedAudioDirs`. Useful
/// when authorizing operations on a path whose target file may not exist —
/// e.g. an idempotent delete where the row points at an already-removed file
/// — because canonicalizing a missing file fails closed in
/// `is_allowed_audio_path`.
pub(crate) fn audio_dir_trusted(app: &AppHandle, dir: &Path) -> bool {
    let Some(state) = app.try_state::<TrustedAudioDirs>() else {
        return false;
    };
    let Ok(guard) = state.lock() else {
        return false;
    };
    let Ok(canonical) = std::fs::canonicalize(dir) else {
        return false;
    };
    guard.iter().any(|trusted| canonical.starts_with(trusted))
}

/// Adds `dir`'s canonical form to the trusted set. Called from the
/// finalize path each time Rust writes a new part to a directory.
pub(crate) fn register_trusted_audio_dir(app: &AppHandle, dir: &Path) {
    let Some(state) = app.try_state::<TrustedAudioDirs>() else {
        return;
    };
    let Ok(mut guard) = state.lock() else {
        return;
    };
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    guard.insert(canonical);
}

fn read_file_range(path: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let length = end
        .checked_sub(start)
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid range"))?;

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn run() {
    // The sync ceremony commands (T010e) are registered ONLY when the off-by-default
    // `sync` feature is on — the module and its deps (cr-sqlite/yapstack-crypto) exist
    // only there. `collect_commands!` cannot `#[cfg]` individual entries, so the builder
    // is constructed per-feature; the base command list is identical in both branches.
    #[cfg(not(feature = "sync"))]
    let specta_builder =
        tauri_specta::Builder::<tauri::Wry>::new().commands(tauri_specta::collect_commands![
            commands::health_check,
            commands::backend_ready,
            commands::audio::list_audio_devices,
            commands::audio::get_default_input_device,
            commands::audio::start_capture,
            commands::audio::stop_capture,
            commands::audio::get_capture_status,
            commands::audio::check_system_audio_permission,
            commands::audio::get_buffer_info,
            commands::audio::peek_capture_energy,
            commands::capture::delete_session_wav,
            commands::capture::delete_audio_files,
            commands::transcription::get_available_models,
            commands::transcription::download_model,
            commands::transcription::delete_model,
            commands::transcription::init_transcription_client,
            commands::transcription::shutdown_transcription_client,
            commands::transcription::get_transcription_status,
            commands::transcription::get_engine_catalogue,
            commands::transcription::get_parakeet_models,
            commands::transcription::get_recommended_parakeet_variant,
            commands::transcription::download_parakeet_model,
            commands::transcription::delete_parakeet_model,
            commands::transcription::get_sortformer_status,
            commands::transcription::download_sortformer_model,
            commands::transcription::delete_sortformer_model,
            commands::live_transcription::start_live_transcription,
            commands::live_transcription::stop_live_transcription,
            commands::live_transcription::get_live_transcription_status,
            commands::live_transcription::update_vocabulary_hints,
            commands::dictation::clipboard_paste,
            commands::system_volume::apply_volume_duck,
            commands::system_volume::restore_volume,
            commands::permissions::check_screen_capture_permission,
            commands::permissions::request_screen_capture_permission,
            commands::get_autostart_enabled,
            commands::set_autostart_enabled,
            commands::show_overlay_panel,
            commands::hide_overlay_panel,
            commands::get_cursor_position,
            commands::set_overlay_ignore_cursor_events,
            commands::logs::get_recent_logs,
            commands::logs::clear_logs,
            commands::logs::get_log_dir,
            commands::logs::reveal_log_dir,
            commands::logs::log_frontend,
            db_service::db_execute,
            db_service::db_select,
        ]);
    #[cfg(feature = "sync")]
    let specta_builder =
        tauri_specta::Builder::<tauri::Wry>::new().commands(tauri_specta::collect_commands![
            commands::health_check,
            commands::backend_ready,
            commands::audio::list_audio_devices,
            commands::audio::get_default_input_device,
            commands::audio::start_capture,
            commands::audio::stop_capture,
            commands::audio::get_capture_status,
            commands::audio::check_system_audio_permission,
            commands::audio::get_buffer_info,
            commands::audio::peek_capture_energy,
            commands::capture::delete_session_wav,
            commands::capture::delete_audio_files,
            commands::transcription::get_available_models,
            commands::transcription::download_model,
            commands::transcription::delete_model,
            commands::transcription::init_transcription_client,
            commands::transcription::shutdown_transcription_client,
            commands::transcription::get_transcription_status,
            commands::transcription::get_engine_catalogue,
            commands::transcription::get_parakeet_models,
            commands::transcription::get_recommended_parakeet_variant,
            commands::transcription::download_parakeet_model,
            commands::transcription::delete_parakeet_model,
            commands::transcription::get_sortformer_status,
            commands::transcription::download_sortformer_model,
            commands::transcription::delete_sortformer_model,
            commands::live_transcription::start_live_transcription,
            commands::live_transcription::stop_live_transcription,
            commands::live_transcription::get_live_transcription_status,
            commands::live_transcription::update_vocabulary_hints,
            commands::dictation::clipboard_paste,
            commands::system_volume::apply_volume_duck,
            commands::system_volume::restore_volume,
            commands::permissions::check_screen_capture_permission,
            commands::permissions::request_screen_capture_permission,
            commands::get_autostart_enabled,
            commands::set_autostart_enabled,
            commands::show_overlay_panel,
            commands::hide_overlay_panel,
            commands::get_cursor_position,
            commands::set_overlay_ignore_cursor_events,
            commands::logs::get_recent_logs,
            commands::logs::clear_logs,
            commands::logs::get_log_dir,
            commands::logs::reveal_log_dir,
            commands::logs::log_frontend,
            db_service::db_execute,
            db_service::db_select,
            sync::sync_probe,
            sync::sync_status,
            sync::sync_signup,
            sync::sync_login_begin,
            sync::sync_login_finish,
            sync::sync_recover,
            sync::sync_enable,
            sync::sync_approve_device,
            sync::sync_sign_out,
        ]);

    #[cfg(debug_assertions)]
    {
        let types_path = "../src/lib/types.ts";
        specta_builder
            .export(
                specta_typescript::Typescript::default()
                    .header("// @ts-nocheck")
                    .bigint(specta_typescript::BigIntExportBehavior::Number)
                    .formatter(specta_typescript::formatter::prettier),
                types_path,
            )
            .expect("failed to export specta types");
        // Specta + the prettier formatter leaves trailing whitespace on
        // JSDoc comment continuation lines (e.g. ` * ` produced from a Rust
        // doc-comment with a blank intermediate line). Prettier doesn't
        // normalize them and `git diff --check` flags them as "trailing
        // whitespace". Strip per-line as a post-export pass so the
        // generated file is clean every time and future regenerations
        // can't reintroduce the diff noise.
        if let Ok(contents) = std::fs::read_to_string(types_path) {
            let trimmed: String = contents
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n");
            // Preserve a final newline if the original had one.
            let needs_trailing_newline = contents.ends_with('\n');
            let out = if needs_trailing_newline {
                format!("{trimmed}\n")
            } else {
                trimmed
            };
            if out != contents {
                let _ = std::fs::write(types_path, out);
            }
        }
    }

    let mut builder = tauri::Builder::default();
    // Must be the first plugin registered so a second launch short-circuits
    // before any other plugin (tray, DB writer, global-shortcut) initializes.
    // The callback surfaces the running instance's main window.
    builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        show_main_window(app);
    }));
    #[cfg(target_os = "macos")]
    {
        builder = builder.plugin(tauri_nspanel::init());
    }
    builder = builder
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_http::init());
    // NOTE: tauri-plugin-sql was removed (Option A′ stage 2). The app's live
    // `yapstack.db` is now served by the repo-owned `db_service` command backend
    // (see `db_service::db_execute` / `db_select`), which owns a cr-sqlite-capable
    // connection pool and runs the migration list in-repo. The service and its
    // startup migrations are created in `setup`, before `backend_ready` is set.

    // Only init Aptabase when the key was provided at compile time (prod builds)
    if let Some(key) = option_env!("APTABASE_KEY") {
        builder = builder.plugin(tauri_plugin_aptabase::Builder::new(key).build());
    }

    builder
        .register_uri_scheme_protocol("audio-stream", |ctx, request| {
            let uri_path = request.uri().path();
            let raw_path = uri_path.trim_start_matches('/');

            // URL-decode the path to handle encoded slashes/spaces
            let decoded_path = percent_encoding::percent_decode_str(raw_path).decode_utf8_lossy();

            // Security: only serve audio files
            if !decoded_path.ends_with(".wav") && !decoded_path.ends_with(".mp3") {
                return tauri::http::Response::builder()
                    .status(400)
                    .body(Vec::new())
                    .unwrap();
            }

            // If the decoded path is absolute, use it directly (custom save location)
            // after verifying it resolves within the app data audio directory.
            // Otherwise, resolve relative to the default audio directory.
            let file_path = if std::path::Path::new(decoded_path.as_ref()).is_absolute() {
                let abs_path = std::path::PathBuf::from(decoded_path.as_ref());
                if !audio_path_trusted(ctx.app_handle(), &abs_path) {
                    return tauri::http::Response::builder()
                        .status(403)
                        .body(Vec::new())
                        .unwrap();
                }
                abs_path
            } else {
                // No path traversal in relative filenames
                if decoded_path.contains('/')
                    || decoded_path.contains('\\')
                    || decoded_path.contains("..")
                {
                    return tauri::http::Response::builder()
                        .status(400)
                        .body(Vec::new())
                        .unwrap();
                }
                let app_data_dir = match ctx.app_handle().path().app_data_dir() {
                    Ok(d) => d,
                    Err(_) => {
                        return tauri::http::Response::builder()
                            .status(500)
                            .body(Vec::new())
                            .unwrap()
                    }
                };
                app_data_dir.join("audio").join(decoded_path.as_ref())
            };
            let content_type = if file_path.extension().is_some_and(|e| e == "mp3") {
                "audio/mpeg"
            } else {
                "audio/wav"
            };

            let file_len = match std::fs::metadata(&file_path) {
                Ok(m) => m.len(),
                Err(_) => {
                    return tauri::http::Response::builder()
                        .status(404)
                        .body(Vec::new())
                        .unwrap()
                }
            };

            // Parse Range header for seeking support
            let range_header = request
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("bytes="));

            if let Some(range_spec) = range_header {
                // Parse "start-end" or "start-"
                let parts: Vec<&str> = range_spec.splitn(2, '-').collect();
                let start: u64 = parts[0].parse().unwrap_or(0);
                let end: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                    parts[1].parse().unwrap_or(file_len - 1)
                } else {
                    file_len - 1
                };

                let start = start.min(file_len.saturating_sub(1));
                let end = end.min(file_len.saturating_sub(1));

                if end < start {
                    return tauri::http::Response::builder()
                        .status(416)
                        .body(Vec::new())
                        .unwrap();
                }

                let length = end - start + 1;
                let buf = match read_file_range(&file_path, start, end) {
                    Ok(data) => data,
                    Err(_) => {
                        return tauri::http::Response::builder()
                            .status(500)
                            .body(Vec::new())
                            .unwrap()
                    }
                };

                tauri::http::Response::builder()
                    .status(206)
                    .header("Content-Type", content_type)
                    .header("Content-Length", length)
                    .header("Content-Range", format!("bytes {start}-{end}/{file_len}"))
                    .header("Accept-Ranges", "bytes")
                    .body(buf)
                    .unwrap()
            } else {
                // Full file response
                let data = match std::fs::read(&file_path) {
                    Ok(d) => d,
                    Err(_) => {
                        return tauri::http::Response::builder()
                            .status(500)
                            .body(Vec::new())
                            .unwrap()
                    }
                };

                tauri::http::Response::builder()
                    .status(200)
                    .header("Content-Type", content_type)
                    .header("Content-Length", file_len)
                    .header("Accept-Ranges", "bytes")
                    .body(data)
                    .unwrap()
            }
        })
        .manage(Arc::new(Mutex::new(AudioManager::new())) as commands::audio::AudioManagerState)
        .manage(Arc::new(Mutex::new(None::<TrayIcon>)) as TrayState)
        .manage(
            Arc::new(Mutex::new(commands::live_transcription::LiveTranscriptionSlots::new()))
                as commands::live_transcription::LiveTranscriptionState,
        )
        .manage(Arc::new(StdMutex::new(None)) as commands::live_transcription::RestartIntentInbox)
        .manage(Arc::new(AtomicBool::new(false)) as commands::live_transcription::LiveSessionPresent)
        .manage(Arc::new(commands::BackendReadyFlag::new()) as commands::BackendReadyState)
        .manage(Arc::new(StdMutex::new(HashSet::<PathBuf>::new())) as TrustedAudioDirs)
        .manage({
            let (tx, _) = watch::channel(false);
            Arc::new(tx) as ShutdownSignal
        })
        .invoke_handler(specta_builder.invoke_handler())
        .setup(move |app| {
            specta_builder.mount_events(app);

            // Initialise tracing → stderr + rotating file + UI ring buffer.
            // Must happen before other state registration so those log lines
            // are captured. The WorkerGuard is managed so the non-blocking
            // file writer survives for the lifetime of the app.
            let log_dir = app
                .path()
                .app_log_dir()
                .expect("failed to resolve app log directory");
            std::fs::create_dir_all(&log_dir).ok();
            let (log_buffer, log_guard) = logging::init(&log_dir, app.handle().clone());
            app.manage(log_buffer);
            app.manage(log_guard);

            // Periodic process-memory sampling. Runs forever in a daemon
            // thread; gives us a historical RSS/VSZ trace in the rolling
            // log so we can correlate growth with crashes / lockups. The
            // FE-side `performance.memory` sampler is complementary —
            // captures JS-heap on Chromium webviews, no-ops on WKWebView.
            let _ = logging::spawn_memory_sampler();

            // Initialize model manager with app data directory
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data directory");

            // tauri-plugin-sql writes the DB to app_config_dir().
            let db_path = app
                .path()
                .app_config_dir()
                .map(|p| p.join("yapstack.db"))
                .unwrap_or_else(|_| app_data_dir.join("yapstack.db"));

            // Manage every command dependency BEFORE the db/filesystem sweeps
            // below: the webviews load concurrently with this hook, and a
            // command invoked before its state is managed fails with
            // "state not managed". (The frontend additionally gates on
            // `backend_ready`, set at the end of setup.)
            app.manage(Arc::new(db_path.clone()) as DbPath);

            // R6 item 4(c): resolve the credential-store home (SESSION_STORE_DIR) and warm
            // the credential cache FIRST — before `recover_interrupted_cutover` and
            // `DbService::open`. `session_enc_path()` falls back to `std::env::temp_dir()`
            // until SESSION_STORE_DIR is set, so ANY session read that raced ahead of the old
            // init point (line ~692) resolved the sealed `sync-session.enc` to the temp dir at
            // boot while sign-in wrote it under the config dir — a write/read PATH MISMATCH
            // that reads as "signed out on every restart". Setting it here makes the boot-read
            // and sign-in-write paths provably identical, and emits the once-per-boot session
            // trace (which logs the RESOLVED path + backend outcome) at the earliest safe point.
            #[cfg(feature = "sync")]
            if let Ok(config_dir) = app.path().app_config_dir() {
                sync::init_credential_store(&config_dir);
            }

            // F1.2: recover any CRR cutover that was interrupted by a crash/kill
            // BEFORE opening the DB. `DbService::open` would otherwise CREATE an empty
            // `yapstack.db` over a mid-swap state (live renamed to backup, staging not
            // yet swapped in) and the auto-cutover would then destroy the leftovers —
            // silent total data loss. Recovery reads the durable cutover journal and
            // completes or rolls back the swap deterministically, so `open` below always
            // sees the real data (or a genuinely fresh install). Sync-feature-only: a
            // cutover (and thus a journal) can only exist under the `sync` feature.
            #[cfg(feature = "sync")]
            sync::recover_interrupted_cutover(&db_path);

            // Repo-owned DB command backend (Option A′ stage 2). Opening the
            // service runs the in-repo migration list on its writer connection
            // BEFORE any `db_execute`/`db_select` command can be served, and
            // before `backend_ready` unblocks the frontend. Migration continuity
            // with the removed tauri-plugin-sql is preserved via the existing
            // `_sqlx_migrations` bookkeeping (see `db_service::run_migrations`).
            let db_service: db_service::DbServiceState = Arc::new(
                db_service::DbService::open(&db_path)
                    .expect("failed to open repo-owned DB service"),
            );
            app.manage(db_service.clone());

            // YapStack Sync runtime (deliverable A): manage the drain handle and,
            // if the keychain holds an enabled session, start the encrypted
            // push/pull drain on its own dedicated single-thread runtime. No-op
            // when signed out. Entirely behind the `sync` cargo feature.
            #[cfg(feature = "sync")]
            {
                // Credential store home + cache warm-up already ran above (before the DB open),
                // so `sync_status` never re-reads the keychain per call (T020) AND the sealed
                // session path is deterministic before any read (R6 item 4c).
                app.manage(sync::cred_cache_handle());
                let sync_runtime: sync::SyncRuntimeState =
                    Arc::new(StdMutex::new(None));
                app.manage(sync_runtime.clone());
                sync::start_drain_if_enabled(
                    &app.handle().clone(),
                    &db_path,
                    &sync_runtime,
                    &db_service,
                );
            }

            let model_manager = ModelManager::new(app_data_dir.clone());
            app.manage(
                Arc::new(Mutex::new(model_manager)) as commands::transcription::ModelManagerState
            );

            // Initialize transcription scheduler state (starts as None — the
            // long-lived scheduler is constructed on first
            // `init_transcription_client` and lives until shutdown).
            app.manage(
                Arc::new(Mutex::new(None))
                    as commands::transcription::TranscriptionSchedulerState,
            );

            // Mic-ownership coordination shared between the session and
            // dictation live loops. Always present; the flag flips at runtime.
            app.manage(Arc::new(commands::transcription::DictationOwnsMic::new())
                as commands::transcription::DictationOwnsMicState);

            db::ensure_runtime_schema(&db_path);

            // Seed the trusted-audio-dirs set from existing parts rows + the
            // default audio dir, then sweep those dirs for orphan files left
            // by a crash between WAV finalize and the prior FE-driven INSERT.
            let app_audio_dir = app_data_dir.join("audio");
            let trusted_dirs = db::list_audio_part_directories(&db_path, &app_audio_dir);
            db::reconcile_audio_parts(&db_path, &trusted_dirs);
            // Re-list after reconciliation so newly-recovered parts'
            // directories land in the trusted set.
            let trusted_dirs = db::list_audio_part_directories(&db_path, &app_audio_dir);
            if let Some(state) = app.try_state::<TrustedAudioDirs>() {
                if let Ok(mut guard) = state.lock() {
                    guard.extend(trusted_dirs);
                }
            }

            let menu = build_tray_menu(app.handle(), false, false)?;

            // macOS uses a monochrome template glyph tinted by the system;
            // `icon_as_template` is a no-op elsewhere, and the black glyph
            // vanishes on the dark Win10/11 taskbar. Off macOS, fall back to
            // the bundled colored window icon so the tray stays visible — the
            // tray is the only re-entry point after close-to-tray.
            #[cfg(target_os = "macos")]
            let tray_builder = TrayIconBuilder::new()
                .icon(tauri::image::Image::from_bytes(include_bytes!(
                    "../icons/tray-icon.png"
                ))?)
                .icon_as_template(true);
            #[cfg(not(target_os = "macos"))]
            let tray_builder = {
                let mut b = TrayIconBuilder::new();
                if let Some(icon) = app.default_window_icon() {
                    b = b.icon(icon.clone());
                }
                b
            };

            let tray = tray_builder
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "start_capture" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app.state::<commands::audio::AudioManagerState>();
                            let mut manager = state.lock().await;
                            if manager
                                .start_capture(yapstack_common::types::CaptureSource::MicOnly, None)
                                .is_ok()
                            {
                                drop(manager);
                                update_tray_menu(&app, true, false);
                            }
                        });
                    }
                    "stop_capture" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app.state::<commands::audio::AudioManagerState>();
                            let mut manager = state.lock().await;
                            if manager.stop_all().is_ok() {
                                drop(manager);
                                update_tray_menu(&app, false, false);
                            }
                        });
                    }
                    "new_session" => {
                        let _ = app.emit("tray:new-session", 0u32);
                        show_main_window(app);
                    }
                    "new_session_bf_30" => {
                        let _ = app.emit("tray:new-session", 30u32);
                        show_main_window(app);
                    }
                    "new_session_bf_60" => {
                        let _ = app.emit("tray:new-session", 60u32);
                        show_main_window(app);
                    }
                    "new_session_bf_120" => {
                        let _ = app.emit("tray:new-session", 120u32);
                        show_main_window(app);
                    }
                    "new_session_bf_300" => {
                        let _ = app.emit("tray:new-session", 300u32);
                        show_main_window(app);
                    }
                    "new_session_bf_all" => {
                        let _ = app.emit("tray:new-session-all", ());
                        show_main_window(app);
                    }
                    "stop_session" => {
                        let _ = app.emit("tray:stop-session", ());
                    }
                    "open" => {
                        show_main_window(app);
                    }
                    "quit" => {
                        let shutdown = app.state::<ShutdownSignal>();
                        let _ = shutdown.send(true);
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            // Store tray handle for later menu updates
            let tray_state = app.state::<TrayState>();
            let tray_state_clone = tray_state.inner().clone();
            tauri::async_runtime::spawn(async move {
                let mut guard = tray_state_clone.lock().await;
                *guard = Some(tray);
            });

            // Always-on device-change broker. Owns the receiving end of
            // the audio crate's runtime-agnostic device-event sink,
            // emits `devices-changed`, and
            // routes auto-failover restart intents through the live
            // transcription loop (or directly to AudioManager when no
            // live session is running).
            device_broker::spawn(
                app.handle().clone(),
                app.state::<AudioManagerState>().inner().clone(),
                app.state::<commands::live_transcription::RestartIntentInbox>()
                    .inner()
                    .clone(),
                app.state::<commands::live_transcription::LiveSessionPresent>()
                    .inner()
                    .clone(),
                app.state::<ShutdownSignal>().subscribe(),
            );

            // Spawn background event emitter for capture status + buffer info + tray updates
            // Only emits when values actually change to avoid flooding the webview.
            let audio_state = app.state::<AudioManagerState>().inner().clone();
            let live_state = app
                .state::<commands::live_transcription::LiveTranscriptionState>()
                .inner()
                .clone();
            let handle = app.handle().clone();
            let mut shutdown_rx = app.state::<ShutdownSignal>().subscribe();
            tauri::async_runtime::spawn(async move {
                let mut prev_status: Option<CaptureStatusDto> = None;
                let mut prev_buffer: Option<BufferStatusDto> = None;
                let mut prev_capturing: Option<bool> = None;
                let mut prev_recording: Option<bool> = None;
                loop {
                    let should_stop = tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(500)) => false,
                        _ = shutdown_rx.changed() => true,
                    };
                    if should_stop {
                        break;
                    }
                    let manager = audio_state.lock().await;
                    let status: CaptureStatusDto = manager.status().into();
                    let buffer_info = BufferStatusDto {
                        mic: manager.mic_buffer_info().map(RingBufferInfoDto::from),
                        system: manager.system_buffer_info().map(RingBufferInfoDto::from),
                    };
                    drop(manager);

                    let is_capturing =
                        matches!(status.state, commands::audio::CaptureStateDto::Capturing);
                    let is_recording = {
                        let guard = live_state.lock().await;
                        // Tray "recording" indicator reflects the session
                        // slot only — dictation is short-lived and shouldn't
                        // flip the menu state.
                        guard
                            .session
                            .runtime()
                            .is_some_and(|r| r.controller.is_running())
                    };

                    if prev_status.as_ref() != Some(&status) {
                        let _ = handle.emit("capture-status", &status);
                        prev_status = Some(status);
                    }
                    if prev_buffer
                        .as_ref()
                        .is_none_or(|prev| buffer_display_changed(prev, &buffer_info))
                    {
                        let _ = handle.emit("buffer-info", &buffer_info);
                        prev_buffer = Some(buffer_info);
                    }
                    if prev_capturing != Some(is_capturing) || prev_recording != Some(is_recording)
                    {
                        update_tray_menu(&handle, is_capturing, is_recording);
                        prev_capturing = Some(is_capturing);
                        prev_recording = Some(is_recording);
                    }
                }
            });

            // Remove native decorations on Windows (titleBarStyle: "Overlay" is macOS-only)
            #[cfg(target_os = "windows")]
            {
                if let Some(window) = app.get_webview_window(WINDOW_MAIN) {
                    let _ = window.set_decorations(false);
                }
            }

            // Convert overlay windows to NSPanels for fullscreen visibility
            #[cfg(target_os = "macos")]
            for label in [WINDOW_DICTATION, WINDOW_RECORDING_INDICATOR, WINDOW_INSIGHT] {
                if let Some(window) = app.get_webview_window(label) {
                    let panel = window
                        .to_panel::<OverlayPanel>()
                        .expect("failed to convert overlay window to panel");
                    panel.set_level(PanelLevel::MainMenu.value());
                    // Base panel style: non-activating so clicking it never
                    // steals focus from the app the user is working in. The
                    // insight overlay is additionally user-resizable (its
                    // tauri.conf entry sets `resizable: true`); without the
                    // Resizable mask bit here, NSPanel conversion would silently
                    // drop that. The dictation bubble and recording indicator
                    // are intentionally fixed-size, so they keep the bare mask.
                    let mut style = StyleMask::empty().nonactivating_panel();
                    if label == WINDOW_INSIGHT {
                        style = style.resizable();
                    }
                    panel.set_style_mask(style.into());
                    panel.set_collection_behavior(
                        CollectionBehavior::new()
                            .can_join_all_spaces()
                            .full_screen_auxiliary()
                            .into(),
                    );
                }
            }

            // Every command dependency is managed — unblock frontends that
            // gate on `backend_ready` before their first engine command.
            // Deliberately NO companion event: Tauri drops events emitted
            // before a webview's listener attaches (no replay), which is the
            // same race this flag closes. The frontend's command poll is the
            // sole readiness signal.
            app.state::<commands::BackendReadyState>().set_ready();

            Ok(())
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("Fatal: failed to build tauri application: {e}");
            std::process::exit(1);
        })
        .run(|_app, event| match event {
            #[cfg(target_os = "macos")]
            RunEvent::Reopen {
                has_visible_windows: false,
                ..
            } => {
                show_main_window(_app);
            }
            RunEvent::Exit => {
                // The aptabase plugin's own on_event handler calls flush_blocking()
                // on exit, which now uses tauri::async_runtime::block_on (fork fix).
                // No manual flush needed here.

                // Safety net for the dictation duck feature: if the app
                // exits while a recording is still ducked, put the user's
                // volume back. No-op if no snapshot is held.
                let _ = system_volume::restore();
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_temp_dir() -> PathBuf {
        let unique = format!(
            "yapstack_lib_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn allowed_audio_path_rejects_outside_base() {
        let dir = test_temp_dir();
        let base = dir.join("audio");
        std::fs::create_dir_all(&base).expect("create base");

        let outside = dir.join("outside.wav");
        std::fs::write(&outside, b"wav").expect("write outside");
        assert!(!is_allowed_audio_path(&base, &outside));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allowed_audio_path_accepts_inside_base() {
        let dir = test_temp_dir();
        let base = dir.join("audio");
        std::fs::create_dir_all(&base).expect("create base");

        let inside = base.join("clip.wav");
        std::fs::write(&inside, b"wav").expect("write inside");
        assert!(is_allowed_audio_path(&base, &inside));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allowed_audio_path_fails_closed_when_base_missing() {
        let dir = test_temp_dir();
        let base = dir.join("audio");
        let outside = dir.join("outside.wav");
        std::fs::write(&outside, b"wav").expect("write outside");

        assert!(!is_allowed_audio_path(&base, &outside));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_range_reads_exact_window() {
        let dir = test_temp_dir();
        let path = dir.join("bytes.bin");
        std::fs::write(&path, [1u8, 2, 3, 4, 5]).expect("write file");

        let data = read_file_range(&path, 1, 3).expect("read range");
        assert_eq!(data, vec![2, 3, 4]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_range_rejects_invalid_bounds() {
        let dir = test_temp_dir();
        let path = dir.join("bytes.bin");
        std::fs::write(&path, [1u8, 2, 3]).expect("write file");
        assert!(read_file_range(&path, 3, 1).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
