use std::collections::HashMap;

use cpal::traits::{DeviceTrait, HostTrait};
use yapstack_common::types::{AudioDeviceInfo, DeviceType};

use crate::error::AudioError;

type Result<T> = std::result::Result<T, AudioError>;

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| {
        // On Windows/WASAPI, name() returns the short DeviceDesc (e.g., "Microphone").
        // The FriendlyName (e.g., "Microphone (Razer BlackShark)") is in extended().
        // On macOS, extended() is empty so we fall back to name().
        d.extended()
            .first()
            .cloned()
            .unwrap_or_else(|| d.name().to_string())
    })
}

fn device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

/// UID of cpal's internal loopback aggregate device on macOS. cpal
/// allocates this at runtime when a system-audio loopback Stream is
/// constructed (`host/coreaudio/macos/loopback.rs`); see its source
/// doc-comment ("users shouldn't be using it") — it is *not* a device
/// the user should ever be able to select. It is created with
/// `kAudioEndPointDeviceIsPrivateKey: true`, which hides it from
/// System Settings → Sound and Audio MIDI Setup, but **not** from
/// in-process `host.input_devices()` enumeration. Selecting it as a
/// mic crashes capture with "stream type not supported" because the
/// aggregate's stream format reflects a process tap on a specific
/// output device, not a real input.
const CPAL_LOOPBACK_AGGREGATE_UID: &str = "com.cpal.LoopbackRecordAggregateDevice";

/// Returns true when a cpal device id (`"coreaudio:<uid>"` on macOS) or
/// a bare device name corresponds to cpal's internal loopback aggregate.
/// Filters apply to enumeration *and* to user-supplied `mic_device_id`
/// validation in `start_capture`.
pub(crate) fn is_cpal_loopback_aggregate(id: Option<&str>, name: Option<&str>) -> bool {
    if let Some(id) = id {
        if id.contains(CPAL_LOOPBACK_AGGREGATE_UID) {
            return true;
        }
    }
    matches!(name, Some("Cpal loopback record aggregate device"))
}

pub fn list_input_devices() -> Result<Vec<AudioDeviceInfo>> {
    let host = cpal::default_host();
    let default_id = host.default_input_device().and_then(|d| device_id(&d));

    // Deduplicate by name: on Windows/WASAPI, multiple endpoints share the same
    // friendly name. Keep the first seen entry, but prefer the one whose ID
    // matches the default device's ID.
    let mut seen: HashMap<String, AudioDeviceInfo> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for device in host.input_devices()? {
        let name = match device_name(&device) {
            Some(n) => n,
            None => continue,
        };
        // Defensive: an empty name passes the cpal description() check
        // but renders as a confusing blank entry in the picker. Skip.
        if name.trim().is_empty() {
            continue;
        }
        let id = device_id(&device);

        // cpal's runtime-allocated loopback aggregate leaks into input
        // enumeration on macOS even though it's flagged private. Hide
        // it — see `is_cpal_loopback_aggregate` for context.
        if is_cpal_loopback_aggregate(id.as_deref(), Some(&name)) {
            continue;
        }

        let is_default = id.is_some() && id == default_id;

        if let Some(existing) = seen.get_mut(&name) {
            // Replace if the new entry is the default (prefer default endpoint)
            if is_default && !existing.is_default {
                *existing = AudioDeviceInfo {
                    id,
                    name: name.clone(),
                    device_type: DeviceType::Input,
                    is_default,
                };
            }
        } else {
            order.push(name.clone());
            seen.insert(
                name.clone(),
                AudioDeviceInfo {
                    id,
                    name,
                    device_type: DeviceType::Input,
                    is_default,
                },
            );
        }
    }

    // If no entry was marked as default by ID match, fall back to name-based
    // default detection (macOS doesn't always have matching IDs).
    if !seen.values().any(|d| d.is_default) {
        let default_name = host.default_input_device().and_then(|d| device_name(&d));
        if let Some(ref dn) = default_name {
            if let Some(entry) = seen.get_mut(dn) {
                entry.is_default = true;
            }
        }
    }

    Ok(order
        .into_iter()
        .filter_map(|name| seen.remove(&name))
        .collect())
}

pub fn list_output_devices() -> Result<Vec<AudioDeviceInfo>> {
    let host = cpal::default_host();
    let default_id = host.default_output_device().and_then(|d| device_id(&d));

    let mut seen: HashMap<String, AudioDeviceInfo> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for device in host.output_devices()? {
        let name = match device_name(&device) {
            Some(n) => n,
            None => continue,
        };
        if name.trim().is_empty() {
            continue;
        }
        let id = device_id(&device);
        if is_cpal_loopback_aggregate(id.as_deref(), Some(&name)) {
            continue;
        }
        let is_default = id.is_some() && id == default_id;

        if let Some(existing) = seen.get_mut(&name) {
            if is_default && !existing.is_default {
                *existing = AudioDeviceInfo {
                    id,
                    name: name.clone(),
                    device_type: DeviceType::Output,
                    is_default,
                };
            }
        } else {
            order.push(name.clone());
            seen.insert(
                name.clone(),
                AudioDeviceInfo {
                    id,
                    name,
                    device_type: DeviceType::Output,
                    is_default,
                },
            );
        }
    }

    if !seen.values().any(|d| d.is_default) {
        let default_name = host.default_output_device().and_then(|d| device_name(&d));
        if let Some(ref dn) = default_name {
            if let Some(entry) = seen.get_mut(dn) {
                entry.is_default = true;
            }
        }
    }

    Ok(order
        .into_iter()
        .filter_map(|name| seen.remove(&name))
        .collect())
}

pub fn list_all_devices() -> Result<Vec<AudioDeviceInfo>> {
    let mut devices = list_input_devices()?;
    devices.extend(list_output_devices()?);
    Ok(devices)
}

pub fn default_input_device() -> Result<AudioDeviceInfo> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or(AudioError::NoDevicesAvailable)?;
    let name = device
        .description()
        .map(|d| {
            d.extended()
                .first()
                .cloned()
                .unwrap_or_else(|| d.name().to_string())
        })
        .map_err(|e| AudioError::DeviceInit(e.to_string()))?;
    let id = device_id(&device);

    Ok(AudioDeviceInfo {
        id,
        name,
        device_type: DeviceType::Input,
        is_default: true,
    })
}

pub fn default_output_device() -> Result<AudioDeviceInfo> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioError::NoDevicesAvailable)?;
    let name = device
        .description()
        .map(|d| {
            d.extended()
                .first()
                .cloned()
                .unwrap_or_else(|| d.name().to_string())
        })
        .map_err(|e| AudioError::DeviceInit(e.to_string()))?;
    let id = device_id(&device);

    Ok(AudioDeviceInfo {
        id,
        name,
        device_type: DeviceType::Output,
        is_default: true,
    })
}

/// Resolves an input device by ID first, then by name, then falls back to system default.
pub(crate) fn resolve_input_device(id: Option<&str>, name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();

    // Try ID-based lookup first (most reliable on Windows)
    if let Some(target_id) = id {
        if let Ok(parsed) = target_id.parse::<cpal::DeviceId>() {
            if let Some(device) = host.device_by_id(&parsed) {
                return Ok(device);
            }
        }
        // ID didn't parse/match — try it as a device name (handles frontend d.id ?? d.name pattern)
        if let Some(device) = host
            .input_devices()?
            .find(|d| device_name(d).as_deref() == Some(target_id))
        {
            return Ok(device);
        }
    }

    // Try name-based lookup
    if let Some(target_name) = name {
        if let Some(device) = host
            .input_devices()?
            .find(|d| device_name(d).as_deref() == Some(target_name))
        {
            return Ok(device);
        }
        // Name didn't match — fall through to default
    }

    // Fall back to system default
    if id.is_some() || name.is_some() {
        // Caller specified something but we couldn't find it — still fall back
        // rather than erroring, since the user's device may have been unplugged.
    }

    host.default_input_device()
        .ok_or(AudioError::NoDevicesAvailable)
}

/// Tri-state liveness result. Distinguishes "the UID is genuinely missing
/// from the system" from "we couldn't tell" so callers can apply the right
/// policy. The two real call sites differ:
///
/// * The debounce/alive-gate (used to absorb the AirPods/Bluetooth revert
///   window) wants the lenient policy: proceed on `Alive` or `Unknown`,
///   wait+retry on `Dead`/`Absent`. A genuine "couldn't tell" should not
///   stall a restart that the actual cpal `start` call will validate
///   anyway.
/// * The explicit-pick liveness check (used to decide whether to skip
///   failover when the user picked a non-default mic) wants the strict
///   policy: only skip failover on `Alive`. `Absent`/`Dead`/`Unknown`
///   should all fail over to the new default — uncertainty about whether
///   an explicit device disappeared is safer to resolve as "fail over".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceLiveness {
    /// The UID was found in `kAudioHardwarePropertyDevices` and
    /// `kAudioDevicePropertyDeviceIsAlive` reported true.
    Alive,
    /// The UID was found but `IsAlive` reported false. Rare on macOS but
    /// the API permits it.
    Dead,
    /// Device list enumeration succeeded and no device with this UID was
    /// present. The device is genuinely gone.
    Absent,
    /// Could not determine liveness — platform enumeration or per-device
    /// query failed, the UID is empty, or this platform has no liveness
    /// probe (Linux).
    Unknown,
}

/// Probe the Core Audio liveness of a device by its `kAudioDevicePropertyDeviceUID`.
///
/// `uid` is the bare UID string (NOT cpal's `"coreaudio:<uid>"` form —
/// callers must strip that prefix first; an unstripped id falls into
/// `Absent`).
///
/// Returns [`DeviceLiveness::Unknown`] on non-macOS or when any Core
/// Audio call fails. See [`DeviceLiveness`] for the distinction between
/// `Absent` and `Unknown`.
#[cfg(target_os = "macos")]
pub fn device_liveness(uid: &str) -> DeviceLiveness {
    use coreaudio_sys::{
        kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceUID,
        kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kCFStringEncodingUTF8, noErr,
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
        AudioObjectPropertyAddress, CFIndex, CFRelease, CFStringGetCString, CFStringGetLength,
        CFStringRef,
    };
    use std::ffi::c_void;
    use std::mem::size_of;

    if uid.is_empty() {
        return DeviceLiveness::Unknown;
    }

    unsafe {
        // Step 1: enumerate all device IDs.
        let devices_addr = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut size: u32 = 0;
        let status = AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject,
            &devices_addr,
            0,
            std::ptr::null(),
            &mut size,
        );
        if status != noErr as i32 || size == 0 {
            return DeviceLiveness::Unknown;
        }
        let count = (size as usize) / size_of::<AudioObjectID>();
        let mut device_ids: Vec<AudioObjectID> = vec![0; count];
        let mut size_io = size;
        let status = AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &devices_addr,
            0,
            std::ptr::null(),
            &mut size_io,
            device_ids.as_mut_ptr() as *mut c_void,
        );
        if status != noErr as i32 {
            return DeviceLiveness::Unknown;
        }

        // Step 2: scan for a device whose UID matches.
        for device_id in device_ids {
            let uid_addr = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyDeviceUID,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut device_uid_ref: CFStringRef = std::ptr::null();
            let mut sz: u32 = size_of::<CFStringRef>() as u32;
            let s = AudioObjectGetPropertyData(
                device_id,
                &uid_addr,
                0,
                std::ptr::null(),
                &mut sz,
                &mut device_uid_ref as *mut CFStringRef as *mut c_void,
            );
            if s != noErr as i32 || device_uid_ref.is_null() {
                continue;
            }

            // CFString -> Rust String (copy then release the CFString — Core
            // Audio returns a +1 retained reference that the caller owns).
            let len: CFIndex = CFStringGetLength(device_uid_ref);
            let max_size: CFIndex = (len * 4) + 1; // UTF-8 worst case
            let mut buf = vec![0i8; max_size as usize];
            let ok = CFStringGetCString(
                device_uid_ref,
                buf.as_mut_ptr(),
                max_size,
                kCFStringEncodingUTF8,
            );
            CFRelease(device_uid_ref as *const _);
            if ok == 0 {
                continue;
            }
            let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let bytes: &[u8] = std::slice::from_raw_parts(buf.as_ptr() as *const u8, nul);
            let device_uid = match std::str::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if device_uid != uid {
                continue;
            }

            // Step 3: query IsAlive on the matching device.
            let alive_addr = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyDeviceIsAlive,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut alive: u32 = 0;
            let mut sz: u32 = size_of::<u32>() as u32;
            let s = AudioObjectGetPropertyData(
                device_id,
                &alive_addr,
                0,
                std::ptr::null(),
                &mut sz,
                &mut alive as *mut u32 as *mut c_void,
            );
            if s != noErr as i32 {
                return DeviceLiveness::Unknown;
            }
            return if alive != 0 {
                DeviceLiveness::Alive
            } else {
                DeviceLiveness::Dead
            };
        }

        // Enumeration succeeded but no device with this UID exists.
        DeviceLiveness::Absent
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn device_liveness(_uid: &str) -> DeviceLiveness {
    DeviceLiveness::Unknown
}

/// Probe WASAPI endpoint liveness by its endpoint ID string.
///
/// `uid` is the bare endpoint ID (`{0.0.1.00000000}.{guid...}`) — NOT
/// cpal's `"wasapi:<id>"` form; callers must strip that prefix first (an
/// unstripped id falls into `Absent`: `GetDevice` has no record of it).
///
/// `Dead` covers endpoints Windows still knows about but that aren't
/// active (`DEVICE_STATE_DISABLED` / `NOTPRESENT` / `UNPLUGGED`);
/// `Absent` means the registry has no endpoint with this ID at all.
#[cfg(target_os = "windows")]
pub fn device_liveness(uid: &str) -> DeviceLiveness {
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{
        IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // HRESULT_FROM_WIN32(ERROR_NOT_FOUND): GetDevice's "no endpoint with
    // this id" result. Genuinely gone, distinct from a failed query.
    const E_NOTFOUND: windows::core::HRESULT = windows::core::HRESULT(0x8007_0490_u32 as i32);

    if uid.is_empty() {
        return DeviceLiveness::Unknown;
    }

    // Called from arbitrary worker threads (the broker's tokio pool):
    // initialize COM per call. S_OK/S_FALSE must be balanced with
    // CoUninitialize; RPC_E_CHANGED_MODE (thread already STA) leaves COM
    // usable but must NOT be balanced.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let need_uninit = hr.is_ok();

    let wide: Vec<u16> = uid.encode_utf16().chain(std::iter::once(0)).collect();
    let liveness = (|| {
        let enumerator: IMMDeviceEnumerator =
            match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
                Ok(e) => e,
                Err(_) => return DeviceLiveness::Unknown,
            };
        match unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) } {
            Ok(device) => match unsafe { device.GetState() } {
                Ok(state) if state == DEVICE_STATE_ACTIVE => DeviceLiveness::Alive,
                Ok(_) => DeviceLiveness::Dead,
                Err(_) => DeviceLiveness::Unknown,
            },
            Err(e) if e.code() == E_NOTFOUND => DeviceLiveness::Absent,
            Err(_) => DeviceLiveness::Unknown,
        }
    })();
    if need_uninit {
        unsafe { CoUninitialize() };
    }
    liveness
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // Requires audio hardware
    fn test_list_input_devices() {
        let devices = list_input_devices().unwrap();
        assert!(!devices.is_empty(), "expected at least one input device");
        for device in &devices {
            assert_eq!(device.device_type, DeviceType::Input);
        }
    }

    #[test]
    #[ignore] // Requires audio hardware
    fn test_default_input_device() {
        let device = default_input_device().unwrap();
        assert!(device.is_default);
        assert_eq!(device.device_type, DeviceType::Input);
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)] // WASAPI COM cleanup crashes on CI
    fn test_resolve_nonexistent_device_by_name() {
        // Falls back to default when name doesn't match
        let result = resolve_input_device(None, Some("nonexistent_device_xyz_12345"));
        // Should succeed (falls back to system default) or fail with NoDevicesAvailable
        // depending on hardware — just verify it doesn't panic
        let _ = result;
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn test_resolve_nonexistent_device_by_id() {
        // Falls back to default when ID doesn't match
        let result = resolve_input_device(Some("InvalidHost:fake_id"), None);
        let _ = result;
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn test_resolve_default_device() {
        // Both None → system default
        let result = resolve_input_device(None, None);
        let _ = result;
    }

    #[test]
    fn loopback_aggregate_is_recognized_by_id() {
        // cpal formats the id as "coreaudio:com.cpal.LoopbackRecordAggregateDevice".
        assert!(is_cpal_loopback_aggregate(
            Some("coreaudio:com.cpal.LoopbackRecordAggregateDevice"),
            None,
        ));
        // Bare UID also matches (callers that strip the host prefix).
        assert!(is_cpal_loopback_aggregate(
            Some("com.cpal.LoopbackRecordAggregateDevice"),
            None,
        ));
    }

    #[test]
    fn loopback_aggregate_is_recognized_by_name_only() {
        // Belt-and-braces in case cpal ever fails to surface the UID
        // but still returns the device name.
        assert!(is_cpal_loopback_aggregate(
            None,
            Some("Cpal loopback record aggregate device"),
        ));
    }

    #[test]
    fn unrelated_devices_are_not_loopback_aggregate() {
        assert!(!is_cpal_loopback_aggregate(
            Some("coreaudio:BuiltInMicrophoneDevice"),
            Some("MacBook Pro Microphone"),
        ));
        assert!(!is_cpal_loopback_aggregate(None, None));
        assert!(!is_cpal_loopback_aggregate(Some(""), Some("")));
    }

    #[test]
    fn device_liveness_unknown_uid_is_not_alive() {
        // The strict policy in the broker only skips failover on `Alive`.
        // For a UID that doesn't match any system device the result must
        // therefore be anything *except* `Alive` — typically `Absent`
        // (enumeration succeeded, no match) on macOS, or `Unknown`
        // (Core Audio enumeration failed, or non-macOS stub). Both
        // outcomes correctly trigger failover; we don't pin one over
        // the other because the Core Audio enumeration result varies
        // by environment (sandboxing, headless CI). A separate hardware
        // test below pins `Alive` for a known-live UID.
        assert_ne!(
            device_liveness("definitely-not-a-real-device-uid-zzzz"),
            DeviceLiveness::Alive
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore] // Requires Core Audio enumeration to succeed (interactive macOS only).
    fn device_liveness_unknown_uid_is_absent_when_core_audio_enumerates() {
        // Hardware-gated companion to the policy test above: on a real
        // macOS desktop where Core Audio enumeration succeeds, an unknown
        // UID resolves to `Absent` specifically (not `Unknown`). This is
        // the path real users hit when an explicit USB mic is unplugged
        // mid-session, so we want a focused regression check — but it's
        // ignored by default because headless CI returns `Unknown`.
        assert_eq!(
            device_liveness("definitely-not-a-real-device-uid-zzzz"),
            DeviceLiveness::Absent
        );
    }

    #[test]
    fn device_liveness_empty_uid_is_unknown() {
        // An empty UID is a caller bug, not a real query — surface it
        // as Unknown so the lenient policy proceeds and the strict
        // policy fails over (both safer than asserting Absent).
        assert_eq!(device_liveness(""), DeviceLiveness::Unknown);
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore)] // Non-macOS stub: always Unknown.
    #[ignore] // Requires hardware to assert Alive on a known UID.
    fn device_liveness_alive_for_default_input() {
        let host = cpal::default_host();
        let dev = host.default_input_device().expect("need default input");
        let id = device_id(&dev).expect("default input has an id");
        let uid = id
            .strip_prefix("coreaudio:")
            .expect("macOS device id format is 'coreaudio:<uid>'");
        assert_eq!(device_liveness(uid), DeviceLiveness::Alive);
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn device_liveness_non_macos_is_unknown() {
        assert_eq!(device_liveness("anything"), DeviceLiveness::Unknown);
    }

    #[test]
    #[ignore] // Requires audio hardware
    fn test_resolve_input_device_id_as_name_fallback() {
        // When `id` is actually a device name string (because frontend sent d.id ?? d.name
        // and d.id was null), resolve_input_device should match by name after ID parse fails.
        let devices = list_input_devices().unwrap();
        assert!(!devices.is_empty(), "need at least one input device");

        let target = &devices[0];
        // Pass the device *name* as the `id` parameter, with `name` = None
        let result = resolve_input_device(Some(&target.name), None);
        assert!(result.is_ok(), "should resolve device name passed as id");

        let resolved = result.unwrap();
        let resolved_name = device_name(&resolved);
        assert_eq!(
            resolved_name.as_deref(),
            Some(target.name.as_str()),
            "resolved device should match the target by name"
        );
    }
}
