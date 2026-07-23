//! System output volume control for the dictation duck feature.
//!
//! While the user is actively recording a dictation, we lower the system
//! output volume by a configured fraction (the "duck amount") so they can
//! hear themselves and anyone trying to talk over them. The duck is
//! relative to the current system volume at the time of duck — `amount`
//! is the fraction we *reduce by*, so `current * (1 - amount)` is what
//! we land on. Setting amount=0.8 on a system at 50% lands on 10%; on a
//! system at 30% it lands on 6%. The prior level is snapshotted at duck
//! time and restored when recording ends (or on app exit as a safety net).
//!
//! Reduce-by semantics mean we never raise: with amount in [0, 1] the
//! computed absolute is always ≤ current.
//!
//! The snapshot tracks the **device id** alongside the level. If the user
//! switches default output mid-dictation (AirPods connect, USB DAC unplug),
//! `restore` targets the *original* device that was ducked rather than the
//! new default — otherwise we'd leak ducked state on the original device
//! and clobber the new device's volume.
//!
//! macOS uses `kAudioHardwareServiceDeviceProperty_VirtualMainVolume` (FourCC
//! 'vmvc'), the same property the menu-bar slider drives. It's the modern,
//! device-agnostic way to set "the system volume": works on built-in
//! speakers, AirPods, USB DACs, and aggregate devices, including those whose
//! hardware doesn't expose a true master scalar on the main element.
//!
//! Windows drives `IAudioEndpointVolume::SetMasterVolumeLevelScalar` on the
//! default render endpoint — the same scalar the taskbar volume slider
//! drives — with the endpoint ID string as the device identity. Linux is a
//! stub that returns `Unsupported`; a future port plugs in against the same
//! `SystemOutputVolume` trait.

use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    // Windows has a real controller, so nothing constructs this variant there.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    #[error("system output volume control is not supported on this platform")]
    Unsupported,
    #[cfg(target_os = "macos")]
    #[error("CoreAudio call failed: OSStatus={0}")]
    CoreAudio(i32),
    #[cfg(target_os = "windows")]
    #[error("WASAPI call failed: {0}")]
    Wasapi(String),
}

/// Opaque identifier for an output device. On macOS this is an
/// `AudioObjectID`; on Windows the WASAPI endpoint ID string; on stub
/// platforms a placeholder that's never read.
#[cfg(not(target_os = "windows"))]
pub type DeviceId = u32;
#[cfg(target_os = "windows")]
pub type DeviceId = String;

pub trait SystemOutputVolume: Send + Sync {
    /// Resolve the current default output device. Returns `Unsupported` on
    /// platforms or system states without a usable default (e.g. a headless
    /// mac where `kAudioHardwarePropertyDefaultOutputDevice` resolves to
    /// `kAudioObjectUnknown` / 0).
    fn default_device(&self) -> Result<DeviceId, VolumeError>;
    /// Read the volume of `device` in [0.0, 1.0].
    fn get(&self, device: &DeviceId) -> Result<f32, VolumeError>;
    /// Set `device`'s volume. `level` is clamped to [0.0, 1.0].
    fn set(&self, device: &DeviceId, level: f32) -> Result<(), VolumeError>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum DuckOutcome {
    /// Volume on `device` was set from `from` to `to` (= `from * (1 - amount)`),
    /// and `(device, from)` is now snapshotted. With `amount = 0` this still
    /// fires — `to` equals `from` and the snapshot lets `restore` no-op safely.
    Applied {
        device: DeviceId,
        from: f32,
        to: f32,
    },
    /// A snapshot was already held from a prior `apply_duck`; we re-applied
    /// the same reduce-by `amount` against the snapshotted *original* level
    /// (not the currently-ducked level — that would compound) without
    /// overwriting the original snapshot.
    AlreadyDucked { amount: f32 },
}

#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    device: DeviceId,
    level: f32,
}

static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);

fn controller() -> Box<dyn SystemOutputVolume> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::CoreAudioController)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows_impl::WasapiController)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(stub::StubController)
    }
}

/// Duck the system volume by a fraction `amount` (in 0.0..=1.0) of the
/// current level on the default output device. The new level is
/// `current * (1 - amount)`: `amount = 0.0` is a no-op write at the
/// current level, `amount = 1.0` mutes. Snapshots the prior `(device, level)`
/// pair so `restore` puts that exact device back, even if the user changes
/// default output during the duck.
pub fn apply_duck(amount: f32) -> Result<DuckOutcome, VolumeError> {
    apply_duck_inner(&SNAPSHOT, controller().as_ref(), amount)
}

fn apply_duck_inner(
    snap_cell: &Mutex<Option<Snapshot>>,
    ctrl: &dyn SystemOutputVolume,
    amount: f32,
) -> Result<DuckOutcome, VolumeError> {
    let amount = amount.clamp(0.0, 1.0);
    let device = ctrl.default_device()?;
    let current = ctrl.get(&device)?;

    // Hold the snapshot mutex across BOTH the snapshot write AND the volume
    // set. Without this, a concurrent `restore_inner` call landing between
    // "publish snapshot" and "lower volume" could clear the snapshot and
    // restore the original level, after which our `set` would land
    // with no snapshot left to recover from — leaving the user ducked with
    // no way to undo it. Trade: `ctrl.set` runs under the mutex (one
    // CoreAudio call, typically sub-millisecond on macOS), in exchange for
    // strict apply/restore atomicity.
    let mut snap = snap_cell.lock().expect("system_volume snapshot poisoned");
    if let Some(snapped) = snap.clone() {
        // Already ducked — re-apply against the snapshotted *original*
        // level (not the currently-ducked level, which would compound).
        // Only set if we'd actually be lowering; never raise.
        let absolute = (snapped.level * (1.0 - amount)).clamp(0.0, 1.0);
        if ctrl.get(&snapped.device)? > absolute {
            ctrl.set(&snapped.device, absolute)?;
        }
        return Ok(DuckOutcome::AlreadyDucked { amount });
    }

    let absolute = (current * (1.0 - amount)).clamp(0.0, 1.0);

    // Write snapshot first, then apply. If the set call fails, the snapshot
    // is rolled back so a later restore can't try to "recover" to a level
    // we never actually reached.
    // DeviceId is Copy (u32) on macOS but String on Windows; the clone is
    // load-bearing for the cross-platform type.
    #[allow(clippy::clone_on_copy)]
    {
        *snap = Some(Snapshot {
            device: device.clone(),
            level: current,
        });
    }
    if let Err(e) = ctrl.set(&device, absolute) {
        *snap = None;
        return Err(e);
    }
    Ok(DuckOutcome::Applied {
        device,
        from: current,
        to: absolute,
    })
}

/// Restore the snapshotted volume on the snapshotted device, if any. No-op
/// if `apply_duck` was never called or was a Skipped no-op. Clears the
/// snapshot regardless of whether the underlying `set` call succeeds — a
/// stale snapshot for a vanished device is worse than a one-off log line.
pub fn restore() -> Result<(), VolumeError> {
    restore_inner(&SNAPSHOT, controller().as_ref())
}

fn restore_inner(
    snap_cell: &Mutex<Option<Snapshot>>,
    ctrl: &dyn SystemOutputVolume,
) -> Result<(), VolumeError> {
    // Hold the mutex across both the snapshot read+clear AND the volume
    // set, mirroring `apply_duck_inner`. Together they form a serialized
    // critical section: an apply running concurrently with this restore
    // either fully precedes us (sees no snapshot, takes one, sets target —
    // we then no-op) or fully follows us (we set back to original, then
    // apply takes a fresh snapshot of the restored level and ducks again).
    // Never an interleaved state where we've cleared the snapshot mid-apply.
    let mut snap = snap_cell.lock().expect("system_volume snapshot poisoned");
    let Some(snapped) = snap.take() else {
        return Ok(());
    };
    ctrl.set(&snapped.device, snapped.level)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{DeviceId, SystemOutputVolume, VolumeError};
    use coreaudio_sys::{
        kAudioDevicePropertyScopeOutput, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
        noErr, AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectSetPropertyData,
    };
    use std::mem;

    /// FourCC `'vmvc'` for `kAudioHardwareServiceDeviceProperty_VirtualMainVolume`.
    /// Not exposed by coreaudio-sys (it lives in AudioToolbox's AudioServices
    /// header rather than the AudioHardware ones bindgen pulls in) so we
    /// inline the selector value. Apple's deprecation note since 10.9 says
    /// to call AudioObjectGetPropertyData / SetPropertyData directly with
    /// this selector, which is what we do.
    const K_VIRTUAL_MAIN_VOLUME: u32 = 0x766d7663;

    pub struct CoreAudioController;

    fn volume_address() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: K_VIRTUAL_MAIN_VOLUME,
            mScope: kAudioDevicePropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    impl SystemOutputVolume for CoreAudioController {
        fn default_device(&self) -> Result<DeviceId, VolumeError> {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut device_id: AudioObjectID = 0;
            let mut size = mem::size_of::<AudioObjectID>() as u32;
            // SAFETY: stack out param, matching size; CoreAudio writes at
            // most `size` bytes to `device_id`.
            let status = unsafe {
                AudioObjectGetPropertyData(
                    kAudioObjectSystemObject,
                    &address,
                    0,
                    std::ptr::null(),
                    &mut size,
                    &mut device_id as *mut _ as *mut _,
                )
            };
            if status != noErr as i32 {
                return Err(VolumeError::CoreAudio(status));
            }
            // `kAudioObjectUnknown` (id 0) is what CoreAudio returns when no
            // default output device exists — e.g. a headless mac with no
            // built-in or attached audio. Surface this as `Unsupported`
            // rather than letting it propagate into a `!obj` error on the
            // next AudioObject call.
            if device_id == 0 {
                return Err(VolumeError::Unsupported);
            }
            Ok(device_id as DeviceId)
        }

        fn get(&self, device: &DeviceId) -> Result<f32, VolumeError> {
            let device = *device;
            let address = volume_address();
            let mut value: f32 = 0.0;
            let mut size = mem::size_of::<f32>() as u32;
            // SAFETY: stack out param, matching size.
            let status = unsafe {
                AudioObjectGetPropertyData(
                    device as AudioObjectID,
                    &address,
                    0,
                    std::ptr::null(),
                    &mut size,
                    &mut value as *mut _ as *mut _,
                )
            };
            if status != noErr as i32 {
                return Err(VolumeError::CoreAudio(status));
            }
            Ok(value.clamp(0.0, 1.0))
        }

        fn set(&self, device: &DeviceId, level: f32) -> Result<(), VolumeError> {
            let device = *device;
            let address = volume_address();
            let value: f32 = level.clamp(0.0, 1.0);
            let size = mem::size_of::<f32>() as u32;
            // SAFETY: read-only ref to stack `value`, matching size.
            let status = unsafe {
                AudioObjectSetPropertyData(
                    device as AudioObjectID,
                    &address,
                    0,
                    std::ptr::null(),
                    size,
                    &value as *const _ as *const _,
                )
            };
            if status != noErr as i32 {
                return Err(VolumeError::CoreAudio(status));
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::{DeviceId, SystemOutputVolume, VolumeError};
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    };

    /// Drives `IAudioEndpointVolume` master scalar on the default render
    /// endpoint — the same value the taskbar volume slider moves. The
    /// device identity is the WASAPI endpoint ID string, so `restore`
    /// targets the originally-ducked endpoint even if the default output
    /// changed mid-dictation (same contract as the macOS controller).
    pub struct WasapiController;

    fn wasapi_err(e: windows::core::Error) -> VolumeError {
        VolumeError::Wasapi(e.to_string())
    }

    /// Duck/restore fire from arbitrary (tokio) worker threads: initialize
    /// COM per call. S_OK/S_FALSE must be balanced with `CoUninitialize`;
    /// RPC_E_CHANGED_MODE (thread already STA) leaves COM usable but must
    /// NOT be balanced — the same rules cpal's guard follows.
    fn with_com<T>(f: impl FnOnce() -> Result<T, VolumeError>) -> Result<T, VolumeError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let need_uninit = hr.is_ok();
        let out = f();
        if need_uninit {
            unsafe { CoUninitialize() };
        }
        out
    }

    fn enumerator() -> Result<IMMDeviceEnumerator, VolumeError> {
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(wasapi_err)
    }

    fn device_by_id(enumerator: &IMMDeviceEnumerator, id: &str) -> Result<IMMDevice, VolumeError> {
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }.map_err(wasapi_err)
    }

    fn endpoint_volume(device: &IMMDevice) -> Result<IAudioEndpointVolume, VolumeError> {
        unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }.map_err(wasapi_err)
    }

    impl SystemOutputVolume for WasapiController {
        fn default_device(&self) -> Result<DeviceId, VolumeError> {
            with_com(|| {
                let enumerator = enumerator()?;
                let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
                    .map_err(wasapi_err)?;
                let pw = unsafe { device.GetId() }.map_err(wasapi_err)?;
                let id = unsafe { pw.to_string() }
                    .map_err(|e| VolumeError::Wasapi(format!("endpoint id not UTF-16: {e}")));
                // GetId allocates via CoTaskMemAlloc; free unconditionally,
                // including on the to_string error path.
                unsafe { CoTaskMemFree(Some(pw.0 as *const _)) };
                id
            })
        }

        fn get(&self, device: &DeviceId) -> Result<f32, VolumeError> {
            with_com(|| {
                let enumerator = enumerator()?;
                let device = device_by_id(&enumerator, device)?;
                let volume = endpoint_volume(&device)?;
                unsafe { volume.GetMasterVolumeLevelScalar() }.map_err(wasapi_err)
            })
        }

        fn set(&self, device: &DeviceId, level: f32) -> Result<(), VolumeError> {
            with_com(|| {
                let enumerator = enumerator()?;
                let device = device_by_id(&enumerator, device)?;
                let volume = endpoint_volume(&device)?;
                // Null event-context GUID: we don't need to correlate our own
                // change notifications.
                unsafe {
                    volume.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null())
                }
                .map_err(wasapi_err)
            })
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod stub {
    use super::{DeviceId, SystemOutputVolume, VolumeError};

    pub struct StubController;

    impl SystemOutputVolume for StubController {
        fn default_device(&self) -> Result<DeviceId, VolumeError> {
            Err(VolumeError::Unsupported)
        }
        fn get(&self, _device: &DeviceId) -> Result<f32, VolumeError> {
            Err(VolumeError::Unsupported)
        }
        fn set(&self, _device: &DeviceId, _level: f32) -> Result<(), VolumeError> {
            Err(VolumeError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ---- Mock controller: drives the inner functions without real hardware. ----

    /// Builds a platform `DeviceId` from a test-friendly integer
    /// (`u32` on macOS/stub, `String` on Windows).
    #[cfg(not(target_os = "windows"))]
    fn dev(n: u32) -> DeviceId {
        n
    }
    #[cfg(target_os = "windows")]
    fn dev(n: u32) -> DeviceId {
        n.to_string()
    }

    struct MockController {
        default: Mutex<DeviceId>,
        volumes: Mutex<HashMap<DeviceId, f32>>,
        /// Optional sleep injected into `set` so concurrent tests can
        /// deterministically widen the apply/restore race window without
        /// relying on probabilistic interleavings.
        set_delay: std::time::Duration,
    }

    impl MockController {
        fn with_devices(devices: &[(DeviceId, f32)], default: DeviceId) -> Self {
            let mut volumes = HashMap::new();
            for (id, vol) in devices {
                volumes.insert(id.clone(), *vol);
            }
            Self {
                default: Mutex::new(default),
                volumes: Mutex::new(volumes),
                set_delay: std::time::Duration::ZERO,
            }
        }

        fn with_set_delay(mut self, delay: std::time::Duration) -> Self {
            self.set_delay = delay;
            self
        }

        fn set_default(&self, id: DeviceId) {
            *self.default.lock().unwrap() = id;
        }

        fn snapshot_of(&self, device: DeviceId) -> f32 {
            *self.volumes.lock().unwrap().get(&device).unwrap()
        }

        fn with_device(vol: f32) -> Self {
            Self::with_devices(&[(dev(1), vol)], dev(1))
        }
    }

    impl SystemOutputVolume for MockController {
        fn default_device(&self) -> Result<DeviceId, VolumeError> {
            Ok(self.default.lock().unwrap().clone())
        }
        fn get(&self, device: &DeviceId) -> Result<f32, VolumeError> {
            self.volumes
                .lock()
                .unwrap()
                .get(device)
                .copied()
                .ok_or(VolumeError::Unsupported)
        }
        fn set(&self, device: &DeviceId, level: f32) -> Result<(), VolumeError> {
            if !self.set_delay.is_zero() {
                std::thread::sleep(self.set_delay);
            }
            self.volumes
                .lock()
                .unwrap()
                .insert(device.clone(), level.clamp(0.0, 1.0));
            Ok(())
        }
    }

    fn fresh_snap() -> Mutex<Option<Snapshot>> {
        Mutex::new(None)
    }

    /// Helper: assert two f32s are within a small epsilon. Floating-point
    /// chains like `0.4 * 0.7` don't land on the literal 0.28; we want
    /// `(0.27999... - 0.28).abs() < 1e-6` to pass.
    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "expected ~{b}, got {a}");
    }

    #[test]
    fn applied_lowers_and_snapshots() {
        // current=0.80, amount=0.80 → absolute = 0.80 * 0.20 = 0.16.
        let snap = fresh_snap();
        let ctrl = MockController::with_device(0.80);

        let outcome = apply_duck_inner(&snap, &ctrl, 0.80).expect("apply_duck");
        match outcome {
            DuckOutcome::Applied { device, from, to } => {
                assert_eq!(device, dev(1));
                close(from, 0.80);
                close(to, 0.16);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        close(ctrl.snapshot_of(dev(1)), 0.16);
        let s = snap.lock().unwrap().clone().expect("snapshot present");
        assert_eq!(s.device, dev(1));
        close(s.level, 0.80);
    }

    #[test]
    fn applied_uses_reduction_of_current() {
        // Lock in the reduce-BY math across a few representative inputs.
        for &(current, amount, expected) in &[
            (0.50_f32, 0.90_f32, 0.05_f32),
            (1.00, 0.80, 0.20),
            (0.30, 0.50, 0.15),
            (0.40, 0.70, 0.12),
        ] {
            let snap = fresh_snap();
            let ctrl = MockController::with_device(current);
            apply_duck_inner(&snap, &ctrl, amount).expect("apply");
            close(ctrl.snapshot_of(dev(1)), expected);
        }
    }

    #[test]
    fn amount_zero_is_effective_noop_and_still_snapshots() {
        // amount=0 means "reduce by 0%" — write current back to itself,
        // but still capture a snapshot so restore is well-defined.
        let snap = fresh_snap();
        let ctrl = MockController::with_device(0.50);

        let outcome = apply_duck_inner(&snap, &ctrl, 0.0).expect("apply_duck");
        match outcome {
            DuckOutcome::Applied { from, to, .. } => {
                close(from, 0.50);
                close(to, 0.50);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        close(ctrl.snapshot_of(dev(1)), 0.50);
        assert!(snap.lock().unwrap().is_some(), "snapshot captured");

        restore_inner(&snap, &ctrl).expect("restore");
        close(ctrl.snapshot_of(dev(1)), 0.50);
    }

    #[test]
    fn amount_one_mutes() {
        let snap = fresh_snap();
        let ctrl = MockController::with_device(0.50);

        apply_duck_inner(&snap, &ctrl, 1.0).expect("apply");
        close(ctrl.snapshot_of(dev(1)), 0.0);

        restore_inner(&snap, &ctrl).expect("restore");
        close(ctrl.snapshot_of(dev(1)), 0.50);
    }

    #[test]
    fn restore_recovers_snapshotted_level_and_clears_state() {
        let snap = fresh_snap();
        let ctrl = MockController::with_device(0.80);

        apply_duck_inner(&snap, &ctrl, 0.80).expect("apply");
        restore_inner(&snap, &ctrl).expect("restore");

        close(ctrl.snapshot_of(dev(1)), 0.80);
        assert!(snap.lock().unwrap().is_none(), "snapshot cleared");
    }

    #[test]
    fn restore_targets_original_device_after_default_switch() {
        // Real-world: built-in speakers ducked, then AirPods connect and
        // become default. Restore must put speakers back, not AirPods.
        let snap = fresh_snap();
        let ctrl = MockController::with_devices(&[(dev(1), 0.80), (dev(2), 0.50)], dev(1));

        apply_duck_inner(&snap, &ctrl, 0.80).expect("apply on device 1");
        close(ctrl.snapshot_of(dev(1)), 0.16);
        close(ctrl.snapshot_of(dev(2)), 0.50);

        // User connects AirPods mid-dictation → default switches.
        ctrl.set_default(dev(2));

        restore_inner(&snap, &ctrl).expect("restore");

        close(ctrl.snapshot_of(dev(1)), 0.80);
        close(ctrl.snapshot_of(dev(2)), 0.50);
    }

    #[test]
    fn restore_is_noop_when_no_snapshot_held() {
        let snap = fresh_snap();
        let ctrl = MockController::with_device(0.50);

        restore_inner(&snap, &ctrl).expect("restore should noop");
        close(ctrl.snapshot_of(dev(1)), 0.50);
    }

    #[test]
    fn already_ducked_does_not_clobber_snapshot() {
        let snap = fresh_snap();
        let ctrl = MockController::with_device(0.80);

        apply_duck_inner(&snap, &ctrl, 0.80).expect("first apply");
        // A re-entrant duck call (e.g. retry) must not capture the now-ducked
        // 0.16 as a new snapshot, or restore would leave the user at 0.16.
        let outcome = apply_duck_inner(&snap, &ctrl, 0.80).expect("second apply");
        assert!(matches!(outcome, DuckOutcome::AlreadyDucked { .. }));

        restore_inner(&snap, &ctrl).expect("restore");
        close(ctrl.snapshot_of(dev(1)), 0.80);
    }

    #[test]
    fn re_entry_uses_snapshotted_level_not_current() {
        // Regression: re-applying with the same amount must be idempotent
        // against the original level. Computing from the *currently-ducked*
        // level would compound (e.g., 0.80 → 0.40 → 0.20 → 0.10 …).
        let snap = fresh_snap();
        let ctrl = MockController::with_device(0.80);

        apply_duck_inner(&snap, &ctrl, 0.50).expect("first apply");
        close(ctrl.snapshot_of(dev(1)), 0.40);

        apply_duck_inner(&snap, &ctrl, 0.50).expect("second apply");
        close(ctrl.snapshot_of(dev(1)), 0.40);

        apply_duck_inner(&snap, &ctrl, 0.50).expect("third apply");
        close(ctrl.snapshot_of(dev(1)), 0.40);

        restore_inner(&snap, &ctrl).expect("restore");
        close(ctrl.snapshot_of(dev(1)), 0.80);
    }

    #[test]
    fn apply_and_restore_are_atomic_under_contention() {
        // Regression test for the apply/restore race: with the snapshot
        // mutex held only across `is_some / *snap = ...` and *not* across
        // the volume `set` call, a concurrent restore can land between the
        // two and clear the snapshot before set runs. After both complete
        // the volume is at the duck target with no snapshot — i.e. the
        // user is permanently ducked with no way to recover.
        //
        // We use a 50ms sleep injected into MockController::set to widen
        // the window deterministically: thread A starts apply, holds the
        // mutex past its set; the main thread fires restore; if the impl
        // releases the mutex before set, the test will show vol=0.20 with
        // snap=None (broken). With the mutex held across set, restore must
        // wait, and we observe one of the two consistent post-states.
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        let snap = Arc::new(Mutex::new(None::<Snapshot>));
        let ctrl = Arc::new(
            MockController::with_device(0.80).with_set_delay(Duration::from_millis(50)),
        );

        let snap_a = Arc::clone(&snap);
        let ctrl_a = Arc::clone(&ctrl);
        // amount=0.80 on current=0.80 → ducked level = 0.16.
        let apply_handle =
            thread::spawn(move || apply_duck_inner(snap_a.as_ref(), ctrl_a.as_ref(), 0.80));

        // Give thread A enough lead time to acquire the snapshot mutex
        // before the restore call below races to grab it.
        thread::sleep(Duration::from_millis(10));

        restore_inner(snap.as_ref(), ctrl.as_ref()).expect("restore");
        apply_handle
            .join()
            .expect("apply thread")
            .expect("apply ok");

        let vol = ctrl.snapshot_of(dev(1));
        let snap_final = snap.lock().unwrap();

        // Consistent post-states only — no broken interleave.
        let consistent = match snap_final.is_some() {
            true => (vol - 0.16).abs() < 1e-3,  // apply won outright
            false => (vol - 0.80).abs() < 1e-3, // apply→restore both ran in order
        };
        assert!(
            consistent,
            "inconsistent post-state: vol={vol} snap={:?}",
            *snap_final
        );
    }

    // ---- Hardware-touching tests. Mutate the user's actual system volume,
    //      so they're #[ignore]d by default. Run on demand with:
    //          cargo test -p yapstack-desktop --lib system_volume:: -- --ignored
    //      from a developer's macOS box that has audio output.
    //      They also fail on a headless mac (no default output device);
    //      `default_device()` returns `Unsupported` in that case, which is
    //      correct behavior and not a test failure we want gating CI. ----

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches real system audio; run with --ignored"]
    fn hardware_get_returns_unit_interval() {
        let ctrl = controller();
        let device = ctrl.default_device().expect("default device");
        let v = ctrl.get(&device).expect("get system volume");
        assert!((0.0..=1.0).contains(&v), "volume out of range: {v}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches real system audio; run with --ignored"]
    fn hardware_apply_duck_lowers_and_restore_recovers() {
        let snap = fresh_snap();
        let ctrl = controller();
        let device = ctrl.default_device().expect("default device");
        let original = ctrl.get(&device).expect("get original");

        // Force a known starting point well above any plausible duck target.
        let start = 0.6_f32;
        ctrl.set(&device, start).expect("set start");
        let observed_start = ctrl.get(&device).expect("get observed start");

        let amount = 0.8_f32;
        let expected = observed_start * (1.0 - amount);
        apply_duck_inner(&snap, ctrl.as_ref(), amount).expect("apply_duck");
        let after_duck = ctrl.get(&device).expect("get after duck");
        assert!(
            (after_duck - expected).abs() < 0.05,
            "expected duck near {expected} (= {observed_start} * {}), got {after_duck}",
            1.0 - amount
        );

        restore_inner(&snap, ctrl.as_ref()).expect("restore");
        let after_restore = ctrl.get(&device).expect("get after restore");
        assert!(
            (after_restore - observed_start).abs() < 0.05,
            "expected restore near {observed_start}, got {after_restore}"
        );

        // Best-effort cleanup.
        let _ = ctrl.set(&device, original);
    }
}
