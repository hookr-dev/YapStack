pub mod audio;
pub mod capture;
pub mod dictation;
pub mod error;
pub mod export;
pub mod live_transcription;
pub mod logs;
pub mod permissions;
pub mod silero_vad;
pub mod system_volume;
pub mod transcription;
pub mod transcription_scheduler;

use serde::Serialize;
use specta::Type;
use tauri::Manager;

#[derive(Debug, Clone, Serialize, Type)]
pub struct HealthStatus {
    pub status: String,
    pub version: String,
}

#[tauri::command]
#[specta::specta]
pub fn health_check() -> HealthStatus {
    HealthStatus {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// True once the setup hook has managed every command dependency. Tauri
/// creates config-declared webview windows BEFORE running the setup hook
/// (tauri 2.11 `app.rs`: windows built at the top of the internal `setup()`,
/// user hook invoked after), so a fast frontend boot can invoke commands
/// whose `State` isn't managed yet (observed on Windows: "state not managed
/// ... on command `download_model`"). This flag is managed on the builder
/// chain — extractable from the very first invoke — and flips at the end of
/// setup; the frontend polls it before its first engine command. Same shape
/// as the official splashscreen-guide handshake.
///
/// A bare bool (not a Ready/Failed tri-state) is honest ONLY while the setup
/// hook stays synchronous and fatal-on-error — a setup failure exits the
/// process, so "never ready" cannot be observed by a live frontend. If setup
/// ever spawns fallible async init, upgrade this to carry the failure.
///
/// Newtype, NOT a bare `Arc<AtomicBool>`: Tauri manages state by `TypeId`,
/// `LiveSessionPresent` is already a managed `Arc<AtomicBool>`, and a
/// duplicate `manage` is a silent no-op — an alias here would make the
/// end-of-setup store flip the live-session flag instead.
pub struct BackendReadyFlag(std::sync::atomic::AtomicBool);

impl BackendReadyFlag {
    pub fn new() -> Self {
        Self(std::sync::atomic::AtomicBool::new(false))
    }

    pub fn set_ready(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Default for BackendReadyFlag {
    fn default() -> Self {
        Self::new()
    }
}

pub type BackendReadyState = std::sync::Arc<BackendReadyFlag>;

#[tauri::command]
#[specta::specta]
pub fn backend_ready(state: tauri::State<'_, BackendReadyState>) -> bool {
    state.is_ready()
}

#[tauri::command]
#[specta::specta]
pub fn get_autostart_enabled(app: tauri::AppHandle) -> Result<bool, error::CommandError> {
    let manager = app.state::<tauri_plugin_autostart::AutoLaunchManager>();
    manager
        .is_enabled()
        .map_err(|e| error::CommandError::Internal {
            message: e.to_string(),
        })
}

#[tauri::command]
#[specta::specta]
pub fn set_autostart_enabled(
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<(), error::CommandError> {
    let manager = app.state::<tauri_plugin_autostart::AutoLaunchManager>();
    if enabled {
        manager.enable().map_err(|e| error::CommandError::Internal {
            message: e.to_string(),
        })
    } else {
        manager
            .disable()
            .map_err(|e| error::CommandError::Internal {
                message: e.to_string(),
            })
    }
}

/// Window labels that are legitimate overlay panels. The overlay commands take
/// a free-form `label` from the renderer; restricting it to this set keeps a
/// bug (or a compromised renderer) from show/hide-ing or click-through-ing an
/// arbitrary window — notably the main window, which on non-macOS would be
/// hidden by `hide_overlay_panel("main")`.
const OVERLAY_PANELS: [&str; 3] = ["dictation", "recording-indicator", "insight"];

fn ensure_overlay_label(label: &str) -> Result<(), error::CommandError> {
    if OVERLAY_PANELS.contains(&label) {
        Ok(())
    } else {
        Err(error::CommandError::InvalidInput {
            message: format!("'{label}' is not an overlay panel"),
        })
    }
}

#[tauri::command]
#[specta::specta]
pub async fn show_overlay_panel(
    app: tauri::AppHandle,
    label: String,
) -> Result<(), error::CommandError> {
    ensure_overlay_label(&label)?;
    #[cfg(target_os = "macos")]
    {
        let app_clone = app.clone();
        app.run_on_main_thread(move || {
            use tauri_nspanel::ManagerExt;
            if let Ok(panel) = app_clone.get_webview_panel(&label) {
                panel.show();
            }
        })
        .map_err(|e| error::CommandError::Internal {
            message: e.to_string(),
        })?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(window) = app.get_webview_window(&label) {
            window.set_always_on_top(true).ok();
            window.show().map_err(|e| error::CommandError::Internal {
                message: e.to_string(),
            })?;
        }
    }
    Ok(())
}

/// Global cursor position in physical pixels, in screen coordinate space.
/// Used by the Insight overlay to implement region-based click-through:
/// the JS polls this and toggles click-through based on whether the cursor
/// falls inside the overlay's header strip or its body.
#[tauri::command]
#[specta::specta]
pub async fn get_cursor_position(app: tauri::AppHandle) -> Result<(f64, f64), error::CommandError> {
    let pos = app
        .cursor_position()
        .map_err(|e| error::CommandError::Internal {
            message: e.to_string(),
        })?;
    Ok((pos.x, pos.y))
}

/// Toggle click-through (mouse-event pass-through) on an overlay window.
/// On macOS we *must* call `set_ignores_mouse_events` on the underlying
/// NSPanel — Tauri's JS `setIgnoreCursorEvents` routes through the
/// pre-conversion NSWindow handle and silently no-ops after
/// `tauri-nspanel` swaps in its panel subclass.
#[tauri::command]
#[specta::specta]
pub async fn set_overlay_ignore_cursor_events(
    app: tauri::AppHandle,
    label: String,
    ignore: bool,
) -> Result<(), error::CommandError> {
    // Click-through is an insight-overlay-only affordance (region-based
    // header/body pass-through). No other overlay toggles it.
    if label != "insight" {
        return Err(error::CommandError::InvalidInput {
            message: format!(
                "click-through is only supported on the insight overlay (got '{label}')"
            ),
        });
    }
    #[cfg(target_os = "macos")]
    {
        let app_clone = app.clone();
        app.run_on_main_thread(move || {
            use tauri_nspanel::ManagerExt;
            if let Ok(panel) = app_clone.get_webview_panel(&label) {
                panel.set_ignores_mouse_events(ignore);
            }
        })
        .map_err(|e| error::CommandError::Internal {
            message: e.to_string(),
        })?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(window) = app.get_webview_window(&label) {
            window
                .set_ignore_cursor_events(ignore)
                .map_err(|e| error::CommandError::Internal {
                    message: e.to_string(),
                })?;
        }
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn hide_overlay_panel(
    app: tauri::AppHandle,
    label: String,
) -> Result<(), error::CommandError> {
    ensure_overlay_label(&label)?;
    #[cfg(target_os = "macos")]
    {
        let app_clone = app.clone();
        app.run_on_main_thread(move || {
            use tauri_nspanel::ManagerExt;
            if let Ok(panel) = app_clone.get_webview_panel(&label) {
                panel.hide();
            }
        })
        .map_err(|e| error::CommandError::Internal {
            message: e.to_string(),
        })?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(window) = app.get_webview_window(&label) {
            window.set_always_on_top(false).ok();
            window.hide().map_err(|e| error::CommandError::Internal {
                message: e.to_string(),
            })?;
        }
    }
    Ok(())
}
