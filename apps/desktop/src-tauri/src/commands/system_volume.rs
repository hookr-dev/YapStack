//! Tauri commands wrapping the generic `system_volume` mechanism. Callers
//! decide the policy of when to duck (dictation today, possibly other flows
//! later). The commands themselves know nothing about dictation.

use tracing::{debug, warn};

use crate::system_volume;

/// Reduce the system output volume by `amount` (0.0..=1.0) of its current
/// level — i.e. land on `current * (1 - amount)`. Snapshots the prior
/// value so `restore_volume` can put it back. Always succeeds from the
/// frontend's perspective: any platform-level error is logged and
/// swallowed because volume control is a UX nicety, not load-bearing for
/// the caller.
#[tauri::command]
#[specta::specta]
// On stub platforms (Linux), VolumeError has only the Unsupported variant,
// so the catch-all arm (for the macOS/Windows-specific variants) is
// unreachable there.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows")),
    allow(unreachable_patterns)
)]
pub async fn apply_volume_duck(amount: f32) {
    match system_volume::apply_duck(amount) {
        Ok(outcome) => debug!("volume duck: {:?}", outcome),
        Err(system_volume::VolumeError::Unsupported) => {
            debug!("volume duck: platform not supported, skipping");
        }
        Err(e) => {
            warn!("volume duck failed: {}", e);
        }
    }
}

/// Restore the volume snapshotted at the most recent `apply_volume_duck`
/// call. No-op if no snapshot is held.
#[tauri::command]
#[specta::specta]
// Same as apply_volume_duck: the catch-all arm is unreachable on stub platforms.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows")),
    allow(unreachable_patterns)
)]
pub async fn restore_volume() {
    match system_volume::restore() {
        Ok(()) => debug!("volume restore: ok"),
        Err(system_volume::VolumeError::Unsupported) => {
            debug!("volume restore: platform not supported, skipping");
        }
        Err(e) => {
            warn!("volume restore failed: {}", e);
        }
    }
}
