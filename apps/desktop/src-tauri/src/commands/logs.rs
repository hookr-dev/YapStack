//! Tauri commands for the in-app log viewer.
//!
//! See `crate::logging` for the subscriber that populates the buffer and
//! writes the rotating file on disk.

use std::sync::Arc;

use serde::Deserialize;
use specta::Type;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

use super::error::CommandError;
use crate::logging::{LogBuffer, LogEntry};

/// Level passed by the frontend logger. Mirrors `tracing::Level` so the
/// in-process subscriber can route the event identically to a native call.
/// Lowercase serde matches the JS-side string literals.
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum FrontendLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// Snapshot of recent entries from the in-memory ring buffer.
/// `limit` caps the number returned (default 500 = the full buffer).
#[tauri::command]
#[specta::specta]
pub async fn get_recent_logs(
    limit: Option<usize>,
    buffer: State<'_, Arc<LogBuffer>>,
) -> Result<Vec<LogEntry>, CommandError> {
    Ok(buffer.snapshot(limit.unwrap_or(500)))
}

/// Clear the in-memory ring buffer. Does not touch on-disk log files.
#[tauri::command]
#[specta::specta]
pub async fn clear_logs(buffer: State<'_, Arc<LogBuffer>>) -> Result<(), CommandError> {
    buffer.clear();
    Ok(())
}

/// Return the resolved log directory path as a string, for display in the UI.
#[tauri::command]
#[specta::specta]
pub async fn get_log_dir(app: AppHandle) -> Result<String, CommandError> {
    let dir = app
        .path()
        .app_log_dir()
        .map_err(|e| CommandError::Internal {
            message: format!("failed to resolve log dir: {e}"),
        })?;
    Ok(dir.to_string_lossy().into_owned())
}

/// Forward a frontend log event into the unified `tracing` subscriber so it
/// lands on stderr, the rolling daily log file, AND the in-memory ring
/// buffer (and therefore the LogsPanel + any saved-machine-log archive).
///
/// `module` is an optional sub-target (e.g. "console", "window.error",
/// "heap"); it is rendered as a bracketed prefix on the message. We keep
/// `target` fixed at `frontend` so the subscriber filter (`frontend=debug`)
/// is a single knob.
///
/// Best-effort by contract — the JS caller fires-and-forgets and we never
/// surface a failure that would itself produce an error during error
/// reporting. Capping the message length prevents a runaway stack trace
/// from blowing past the ring buffer's per-entry budget.
#[tauri::command]
#[specta::specta]
pub async fn log_frontend(
    level: FrontendLogLevel,
    module: Option<String>,
    message: String,
) -> Result<(), CommandError> {
    const MAX_MESSAGE_LEN: usize = 8 * 1024;
    let trimmed = if message.len() > MAX_MESSAGE_LEN {
        let mut s = message;
        s.truncate(MAX_MESSAGE_LEN);
        s.push_str("…[truncated]");
        s
    } else {
        message
    };
    let formatted = match module.as_deref() {
        Some(m) if !m.is_empty() => format!("[{m}] {trimmed}"),
        _ => trimmed,
    };
    // One arm per level: `tracing::event!` requires a const level at the
    // call site so each level gets its own static metadata. A runtime-
    // dispatched form would lose per-level filtering — this match is the
    // canonical idiom in `tracing`-using codebases.
    match level {
        FrontendLogLevel::Error => tracing::error!(target: "frontend", "{formatted}"),
        FrontendLogLevel::Warn => tracing::warn!(target: "frontend", "{formatted}"),
        FrontendLogLevel::Info => tracing::info!(target: "frontend", "{formatted}"),
        FrontendLogLevel::Debug => tracing::debug!(target: "frontend", "{formatted}"),
        FrontendLogLevel::Trace => tracing::trace!(target: "frontend", "{formatted}"),
    }
    Ok(())
}

/// Open Finder / Explorer with the current log highlighted so the user can grab
/// the files to send to support.
///
/// Uses `reveal_item_in_dir` (covered by `opener:allow-reveal-item-in-dir`)
/// rather than `open_path` (which would need the un-granted
/// `opener:allow-open-path` and is denied at runtime). `reveal_item_in_dir`
/// highlights the passed item inside its *parent* folder:
///   - If a current rolling log file exists (`yapstack.log.YYYY-MM-DD`, see
///     `crate::logging::init`), we reveal that file so the user lands on the
///     actual log highlighted in the log directory.
///   - Otherwise we fall back to revealing the log directory item itself, which
///     opens its parent with the log folder highlighted — still useful with no
///     logs yet.
#[tauri::command]
#[specta::specta]
pub async fn reveal_log_dir(app: AppHandle) -> Result<(), CommandError> {
    let dir = app
        .path()
        .app_log_dir()
        .map_err(|e| CommandError::Internal {
            message: format!("failed to resolve log dir: {e}"),
        })?;

    // The rolling daily appender writes `yapstack.log.YYYY-MM-DD` (basename
    // `yapstack.log`, dated suffix). Pick the most recently modified matching
    // file so the user is dropped on the active log, not an old rotation.
    let target = latest_log_file(&dir).unwrap_or(dir);

    app.opener()
        .reveal_item_in_dir(&target)
        .map_err(|e| CommandError::Internal {
            message: format!("failed to reveal log dir: {e}"),
        })
}

/// Find the newest rolling log file (`yapstack.log*`) in `dir`, if any.
/// Returns `None` when the directory cannot be read or holds no log files yet,
/// so the caller can fall back to revealing the directory itself.
fn latest_log_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("yapstack.log"))
                && entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
        })
        .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok())
        .map(|entry| entry.path())
}
