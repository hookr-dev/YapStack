//! Pushes a "default device changed" signal into our stream-health path via
//! a Core Audio property listener. Replaces the only-symptom-based cpal error
//! / write-pos watchdogs for the dominant mid-session silent-death mode on
//! macOS, where the default input or output device changes (AirPods connect,
//! USB mic unplug, Sound Settings toggle, exclusive-mode grab) and the cpal
//! stream is left bound to a device that no longer carries audio.
//!
//! The same primitive handles input, output, and device-list (hardware
//! added/removed) via three property selectors on the same system object.
//! Construct one `DefaultDeviceWatcher` per kind.
//!
//! On Windows the same primitive is implemented via WASAPI's
//! `IMMNotificationClient` (`OnDefaultDeviceChanged` / `OnDeviceAdded` /
//! `OnDeviceRemoved` / `OnDeviceStateChanged`) registered on a dedicated
//! COM (MTA) thread. On remaining platforms (Linux) this is a no-op stub
//! so call sites don't need `#[cfg]` gates.
//!
//! # Why we do this in-house instead of relying on cpal
//!
//! cpal 0.17.x (and as of this writing, cpal master too) **does not
//! automatically reroute a stream when the default device changes**. The
//! stream stays bound to the device it was created against and silently
//! produces either nothing (output loopback when the underlying device is
//! no longer actively playing) or zero-filled callbacks (input). Upstream
//! tracking:
//!
//! - [`cpal#1175`](https://github.com/RustAudio/cpal/issues/1175) — "default
//!   devices don't get automatically rerouted upon disconnection"
//!   (confirmed by maintainer 2026-04-22). Filed against the newer
//!   `AudioHardwareCreateProcessTap` path from
//!   [PR #1003](https://github.com/RustAudio/cpal/pull/1003), so upgrading
//!   cpal does *not* fix this.
//! - [`cpal#704`](https://github.com/RustAudio/cpal/issues/704),
//!   [`cpal#1012`](https://github.com/RustAudio/cpal/issues/1012),
//!   [`cpal#1030`](https://github.com/RustAudio/cpal/issues/1030) — related
//!   older reports of silent fallback on input disconnect and loopback
//!   edge cases.
//!
//! **If cpal ever lands a `DeviceChanged` error-callback variant or native
//! auto-rerouting**, reassess whether this watcher is still needed. Until
//! then, this is the authoritative signal; cpal's error callback is a
//! backup (Layer 1) and `write_pos` stall detection is a second backup
//! (Layer 2).
//!
//! # Known edge case: default-output "revert" during AirPods handshake
//!
//! On AirPods / Bluetooth output connect, macOS briefly reports the *old*
//! device as default for a window of ~100-200 ms while the AVAudioEngine
//! aggregate is being set up (cf. AirPods + AVAudioEngine
//! [notes](https://supermegaultragroovy.com/2021/01/28/more-on-avaudioengine-airpods/)
//! and Apple [DF thread 763583](https://developer.apple.com/forums/thread/763583)).
//! A naive listener-driven restart that re-queries `default_output_device()`
//! during that revert window binds back to the same dead device.
//!
//! The mitigation lives one layer up in the Tauri-side
//! `device_broker`: events are coalesced in a 250 ms debounce window
//! and the resulting restart is gated on
//! [`crate::device::device_liveness`] for the new default's UID, with
//! one re-check at +250 ms before falling through. We also subscribe
//! to `kAudioHardwarePropertyDevices` (device-list change) because it
//! fires earlier than the default-device properties on some macOS
//! versions during a Bluetooth handshake.

use std::sync::{Arc, RwLock};

use crate::error::AudioError;

/// Runtime-agnostic device-change event. The audio crate emits these
/// without taking a dependency on tokio, async runtimes, or Tauri types.
/// Consumers (the Tauri-side broker, tests) provide a [`DeviceEventSink`]
/// closure to receive them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceEvent {
    /// Hardware device list changed (added or removed). Fires on
    /// `kAudioHardwarePropertyDevices`.
    DeviceListChanged,
    /// Default input device changed.
    DefaultInputChanged,
    /// Default *media* output device changed.
    DefaultOutputChanged,
    /// Default *alerts/UI* output device changed
    /// (`kAudioHardwarePropertyDefaultSystemOutputDevice`). Distinct from
    /// `DefaultOutputChanged`; consumers typically coalesce both into one
    /// system-audio restart attempt.
    DefaultSystemOutputChanged,
}

/// Closure invoked on the Core Audio listener thread whenever a watched
/// property changes. Must be cheap and non-blocking — typical
/// implementations only forward the event into a channel and return.
pub type DeviceEventSink = Arc<dyn Fn(DeviceEvent) + Send + Sync>;

/// Internal slot for a sink that may be attached after the watcher is
/// constructed. Cloned into the C-callback payload so the listener thread
/// reads through the same `Arc` the manager writes into.
pub(crate) type SharedSinkSlot = Arc<RwLock<Option<DeviceEventSink>>>;

/// Which CoreAudio system property a `DefaultDeviceWatcher` is observing.
///
/// `Input` / `Output` subscribe to the default-device selectors; `Devices`
/// subscribes to the device-list selector, which fires when hardware is
/// added or removed from the system (e.g. AirPods connect). On some macOS
/// versions the device-list notification precedes the default-device
/// notification during AirPods handshake, so it serves as an earlier
/// trigger for rebind.
///
/// `DefaultSystemOutput` subscribes to
/// `kAudioHardwarePropertyDefaultSystemOutputDevice`, which is distinct
/// from `Output` (`kAudioHardwarePropertyDefaultOutputDevice`): the former
/// is the route for system alerts and UI sounds, the latter for media.
/// Both can change independently; covering both is necessary to keep
/// system-audio loopback bound to whatever the user actually means by
/// "system output." See Hammerspoon's `hs.audiodevice.watcher` for prior
/// art covering all four selectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultDeviceKind {
    Input,
    Output,
    DefaultSystemOutput,
    Devices,
}

impl DefaultDeviceKind {
    // Consumed by the macOS and Windows watcher imps; dead only on the
    // remaining stub platforms (Linux).
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn to_event(self) -> DeviceEvent {
        match self {
            Self::Input => DeviceEvent::DefaultInputChanged,
            Self::Output => DeviceEvent::DefaultOutputChanged,
            Self::DefaultSystemOutput => DeviceEvent::DefaultSystemOutputChanged,
            Self::Devices => DeviceEvent::DeviceListChanged,
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use coreaudio_sys::{
        kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioHardwarePropertyDefaultSystemOutputDevice, kAudioHardwarePropertyDevices,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
        noErr, AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectPropertySelector, AudioObjectRemovePropertyListener,
    };
    use std::ffi::c_void;
    use tracing::{info, warn};

    pub(super) struct CallbackPayload {
        pub sink: SharedSinkSlot,
        pub kind: DefaultDeviceKind,
    }

    pub(super) struct WatcherInner {
        // Box'd to give the listener callback a stable pointer for the lifetime
        // of the watcher. The payload owns a clone of the `SharedSinkSlot` so
        // the callback can read through it without dereferencing the watcher
        // itself.
        payload_box: *mut CallbackPayload,
        property: AudioObjectPropertyAddress,
    }

    // SAFETY: the `payload_box` pointer is only accessed from the Core Audio
    // listener thread via `listener_proc`, which reads through `Arc`s. The
    // watcher's owning thread never dereferences the raw pointer — only
    // `Box::from_raw` on drop, after the listener is unregistered.
    unsafe impl Send for WatcherInner {}
    unsafe impl Sync for WatcherInner {}

    unsafe extern "C" fn listener_proc(
        _object_id: AudioObjectID,
        _n_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        client_data: *mut c_void,
    ) -> i32 {
        if client_data.is_null() {
            return 0;
        }
        let payload = &*(client_data as *const CallbackPayload);
        // Snapshot the sink under read lock and release it before invoking,
        // so a panicking sink can't poison the lock for future events.
        let snapshot = payload.sink.read().ok().and_then(|guard| guard.clone());
        if let Some(sink) = snapshot {
            sink(payload.kind.to_event());
        }
        0 // noErr
    }

    fn selector_for(kind: DefaultDeviceKind) -> AudioObjectPropertySelector {
        match kind {
            DefaultDeviceKind::Input => kAudioHardwarePropertyDefaultInputDevice,
            DefaultDeviceKind::Output => kAudioHardwarePropertyDefaultOutputDevice,
            DefaultDeviceKind::DefaultSystemOutput => {
                kAudioHardwarePropertyDefaultSystemOutputDevice
            }
            DefaultDeviceKind::Devices => kAudioHardwarePropertyDevices,
        }
    }

    pub(super) fn register(
        kind: DefaultDeviceKind,
        sink: SharedSinkSlot,
    ) -> Result<WatcherInner, AudioError> {
        let property = AudioObjectPropertyAddress {
            mSelector: selector_for(kind),
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };

        // Box the payload so the listener proc has a stable pointer for its lifetime.
        let payload_box: *mut CallbackPayload =
            Box::into_raw(Box::new(CallbackPayload { sink, kind }));

        // SAFETY: `payload_box` is a valid `*mut CallbackPayload` that outlives
        // the listener because we unregister the listener before dropping the
        // box in `Drop for WatcherInner`.
        let status = unsafe {
            AudioObjectAddPropertyListener(
                kAudioObjectSystemObject,
                &property,
                Some(listener_proc),
                payload_box as *mut c_void,
            )
        };

        if status != noErr as i32 {
            // Reclaim the box so we don't leak on the error path.
            let _ = unsafe { Box::from_raw(payload_box) };
            warn!(
                "CoreAudio default-{:?} listener registration failed: OSStatus={}",
                kind, status
            );
            return Err(AudioError::PlatformNotSupported);
        }

        info!(
            "CoreAudio default-{:?} device change listener registered",
            kind
        );
        Ok(WatcherInner {
            payload_box,
            property,
        })
    }

    impl Drop for WatcherInner {
        fn drop(&mut self) {
            // SAFETY: registration matched this exact property + client_data pair.
            let status = unsafe {
                AudioObjectRemovePropertyListener(
                    kAudioObjectSystemObject,
                    &self.property,
                    Some(listener_proc),
                    self.payload_box as *mut c_void,
                )
            };
            if status != noErr as i32 {
                warn!(
                    "CoreAudio default-device listener removal returned OSStatus={}",
                    status
                );
            }
            // SAFETY: the listener is unregistered; no thread can still be
            // reading through `payload_box`.
            let _ = unsafe { Box::from_raw(self.payload_box) };
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use std::sync::mpsc;
    use tracing::{info, warn};
    use windows::core::{implement, PCWSTR};
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, eRender, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
        IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    /// COM callback object registered with
    /// `IMMDeviceEnumerator::RegisterEndpointNotificationCallback`. One
    /// client is registered per watcher kind; each filters the firehose of
    /// endpoint notifications down to its kind's single `DeviceEvent`.
    #[implement(IMMNotificationClient)]
    struct NotificationClient {
        sink: SharedSinkSlot,
        kind: DefaultDeviceKind,
    }

    impl NotificationClient {
        fn fire(&self) {
            // Snapshot the sink under read lock and release it before
            // invoking, mirroring the macOS listener: a panicking sink must
            // not poison the lock for future events.
            let snapshot = self.sink.read().ok().and_then(|guard| guard.clone());
            if let Some(sink) = snapshot {
                sink(self.kind.to_event());
            }
        }
    }

    impl IMMNotificationClient_Impl for NotificationClient_Impl {
        fn OnDeviceStateChanged(
            &self,
            _device_id: &PCWSTR,
            _new_state: DEVICE_STATE,
        ) -> windows::core::Result<()> {
            if self.kind == DefaultDeviceKind::Devices {
                self.fire();
            }
            Ok(())
        }

        fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
            if self.kind == DefaultDeviceKind::Devices {
                self.fire();
            }
            Ok(())
        }

        fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
            if self.kind == DefaultDeviceKind::Devices {
                self.fire();
            }
            Ok(())
        }

        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            role: ERole,
            _default_device_id: &PCWSTR,
        ) -> windows::core::Result<()> {
            // Windows fires this once per role (console/multimedia/
            // communications) for the same physical change. Filter to
            // eConsole — the role cpal's default-device lookups use — so one
            // change produces one event instead of up to three.
            if role != eConsole {
                return Ok(());
            }
            match self.kind {
                DefaultDeviceKind::Input if flow == eCapture => self.fire(),
                DefaultDeviceKind::Output if flow == eRender => self.fire(),
                // Windows has no separate alerts/system-sounds route: system
                // sounds follow the console render endpoint, which the
                // `Output` watcher above already covers. Firing this kind too
                // would double-signal the same change into the broker.
                DefaultDeviceKind::DefaultSystemOutput => {}
                _ => {}
            }
            Ok(())
        }

        fn OnPropertyValueChanged(
            &self,
            _device_id: &PCWSTR,
            _key: &PROPERTYKEY,
        ) -> windows::core::Result<()> {
            Ok(())
        }
    }

    pub(super) struct WatcherInner {
        shutdown_tx: mpsc::Sender<()>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    pub(super) fn register(
        kind: DefaultDeviceKind,
        sink: SharedSinkSlot,
    ) -> Result<WatcherInner, AudioError> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

        // Dedicated thread: the enumerator + registered client are COM
        // objects that live and die on this thread. COM is initialized MTA,
        // so notification callbacks arrive on COM worker threads (delivery
        // does not require this thread to pump messages — it just parks,
        // keeping the registration alive until drop).
        let join = std::thread::Builder::new()
            .name(format!("wasapi-watch-{kind:?}"))
            .spawn(move || {
                let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if hr.is_err() {
                    // Fresh thread: RPC_E_CHANGED_MODE can't apply; any
                    // failure here is a real COM failure.
                    let _ = ready_tx.send(Err(format!("CoInitializeEx failed: {hr}")));
                    return;
                }
                let registration =
                    (|| -> windows::core::Result<(IMMDeviceEnumerator, IMMNotificationClient)> {
                        let enumerator: IMMDeviceEnumerator =
                            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
                        let client: IMMNotificationClient =
                            NotificationClient { sink, kind }.into();
                        unsafe { enumerator.RegisterEndpointNotificationCallback(&client)? };
                        Ok((enumerator, client))
                    })();
                match registration {
                    Ok((enumerator, client)) => {
                        let _ = ready_tx.send(Ok(()));
                        // Park until the watcher drops (Err means the sender
                        // was dropped without an explicit shutdown — treat
                        // the same).
                        let _ = shutdown_rx.recv();
                        unsafe {
                            let _ = enumerator.UnregisterEndpointNotificationCallback(&client);
                        }
                        drop(client);
                        drop(enumerator);
                        unsafe { CoUninitialize() };
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        unsafe { CoUninitialize() };
                    }
                }
            })
            .map_err(|e| {
                warn!("WASAPI watcher thread spawn failed: {e}");
                AudioError::PlatformNotSupported
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                info!(
                    "WASAPI default-{:?} device change listener registered",
                    kind
                );
                Ok(WatcherInner {
                    shutdown_tx,
                    join: Some(join),
                })
            }
            Ok(Err(msg)) => {
                let _ = join.join();
                warn!(
                    "WASAPI default-{:?} listener registration failed: {msg}",
                    kind
                );
                Err(AudioError::PlatformNotSupported)
            }
            Err(_) => {
                // Thread died without reporting (panic before send).
                let _ = join.join();
                warn!(
                    "WASAPI default-{:?} listener thread died during setup",
                    kind
                );
                Err(AudioError::PlatformNotSupported)
            }
        }
    }

    impl Drop for WatcherInner {
        fn drop(&mut self) {
            // Wake the parked thread; it unregisters the callback and
            // uninitializes COM before exiting.
            let _ = self.shutdown_tx.send(());
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }
}

/// Watches for default-device changes for a single direction (input or
/// output) — Core Audio property listeners on macOS, WASAPI
/// `IMMNotificationClient` on Windows. On remaining platforms this is a
/// no-op stub that never invokes the sink.
pub struct DefaultDeviceWatcher {
    kind: DefaultDeviceKind,
    sink: SharedSinkSlot,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    _inner: imp::WatcherInner,
}

impl DefaultDeviceWatcher {
    pub fn new(kind: DefaultDeviceKind) -> Result<Self, AudioError> {
        let sink: SharedSinkSlot = Arc::new(RwLock::new(None));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let inner = imp::register(kind, Arc::clone(&sink))?;
            Ok(Self {
                kind,
                sink,
                _inner: inner,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Ok(Self { kind, sink })
        }
    }

    pub fn kind(&self) -> DefaultDeviceKind {
        self.kind
    }

    /// Attach (or detach with `None`) a sink that the listener thread will
    /// invoke on every event. Replaces any previously attached sink. Safe
    /// to call from any thread; the watcher's listener thread snapshots
    /// the sink under a brief read lock per event.
    pub fn set_sink(&self, sink: Option<DeviceEventSink>) {
        match self.sink.write() {
            Ok(mut guard) => *guard = sink,
            Err(poisoned) => {
                // A previous panicking writer poisoned the lock. Forge ahead —
                // the listener uses `read().ok()` so a poisoned slot just
                // suppresses event delivery; recovering the inner allows
                // future events to flow again.
                let mut guard = poisoned.into_inner();
                *guard = sink;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn watcher_registers_for_each_kind() {
        for kind in [
            DefaultDeviceKind::Input,
            DefaultDeviceKind::Output,
            DefaultDeviceKind::DefaultSystemOutput,
            DefaultDeviceKind::Devices,
        ] {
            let w = DefaultDeviceWatcher::new(kind).expect("watcher registers");
            assert_eq!(w.kind(), kind);
        }
    }

    #[test]
    fn set_sink_attaches_and_detaches_without_panicking() {
        let w = DefaultDeviceWatcher::new(DefaultDeviceKind::Input).expect("input registers");
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let sink: DeviceEventSink = Arc::new(move |_| {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });
        w.set_sink(Some(sink));
        w.set_sink(None);
        // We can't synthesize a real Core Audio event in a unit test,
        // but the sink lock has been written to twice without panicking.
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn kind_to_event_mapping_is_total() {
        assert_eq!(
            DefaultDeviceKind::Input.to_event(),
            DeviceEvent::DefaultInputChanged
        );
        assert_eq!(
            DefaultDeviceKind::Output.to_event(),
            DeviceEvent::DefaultOutputChanged
        );
        assert_eq!(
            DefaultDeviceKind::DefaultSystemOutput.to_event(),
            DeviceEvent::DefaultSystemOutputChanged
        );
        assert_eq!(
            DefaultDeviceKind::Devices.to_event(),
            DeviceEvent::DeviceListChanged
        );
    }
}
