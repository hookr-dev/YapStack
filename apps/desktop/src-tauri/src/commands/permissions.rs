//! macOS capture-permission helpers.
//!
//! On macOS 14.2+ the Core Audio process tap used by cpal's system audio
//! loopback is gated by the Screen Recording TCC permission. Requesting that
//! permission up-front avoids silent capture failures later. Mic permission
//! is still triggered lazily by cpal the first time a mic stream is opened.

use serde::Serialize;
use specta::Type;

use crate::commands::error::CommandError;

#[derive(Debug, Clone, Copy, Serialize, Type)]
pub enum ScreenCapturePermissionDto {
    // Granted/NotDetermined are only constructed on macOS (the inverse of
    // Unavailable below), so off macOS rustc sees them as never built.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Granted,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    NotDetermined,
    // Only returned on non-macOS where Screen Recording TCC doesn't exist; the
    // target_os cfg means rustc can't see this variant constructed on macOS.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unavailable,
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[cfg(target_os = "macos")]
fn preflight() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

#[cfg(target_os = "macos")]
fn request() {
    unsafe {
        let _ = CGRequestScreenCaptureAccess();
    }
}

#[tauri::command]
#[specta::specta]
pub async fn check_screen_capture_permission() -> Result<ScreenCapturePermissionDto, CommandError> {
    #[cfg(target_os = "macos")]
    let result = if preflight() {
        ScreenCapturePermissionDto::Granted
    } else {
        ScreenCapturePermissionDto::NotDetermined
    };
    #[cfg(not(target_os = "macos"))]
    let result = ScreenCapturePermissionDto::Unavailable;
    Ok(result)
}

/// Triggers the Screen Recording TCC prompt if the app has never asked. The
/// OS dialog is shown asynchronously, so callers should re-check via
/// `check_screen_capture_permission` after the user has responded.
#[tauri::command]
#[specta::specta]
pub async fn request_screen_capture_permission() -> Result<ScreenCapturePermissionDto, CommandError>
{
    #[cfg(target_os = "macos")]
    let result = if preflight() {
        ScreenCapturePermissionDto::Granted
    } else {
        request();
        ScreenCapturePermissionDto::NotDetermined
    };
    #[cfg(not(target_os = "macos"))]
    let result = ScreenCapturePermissionDto::Unavailable;
    Ok(result)
}
