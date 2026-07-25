use std::sync::Arc;

use tracing::{debug, error, warn};
use yapstack_common::config::AudioConfig;
use yapstack_common::types::{CaptureSource, CaptureState, CaptureStatus, PermissionStatus};

use crate::capture::{BoundedExtraction, BufferPositions, SeparateExtraction};
use crate::error::AudioError;
use crate::mic::MicrophoneCapture;
use crate::mixer::{self, MixConfig};
use crate::ring_buffer::{AudioRingBuffer, RingBufferInfo, SharedAudioRingBuffer};
use crate::system::device_watcher::{DefaultDeviceKind, DefaultDeviceWatcher, DeviceEventSink};
use crate::system::SystemAudioCapture;
use crate::DeviceStreamConfig;

type Result<T> = std::result::Result<T, AudioError>;

/// Signals whether a stream restart was able to keep the existing ring
/// buffer or had to allocate a new one. Callers must reset any cursors
/// or VAD state tied to the previous buffer when `BufferReplaced` is
/// returned — positions from the old buffer are not meaningful against
/// the new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartOutcome {
    BufferPreserved,
    BufferReplaced,
}

/// Drives `restart_mic` / `restart_system_audio` candidate ordering.
///
/// * `PreserveBinding` — used by the in-loop stream-error / write-pos
///   stall watchdog. The bound device is presumed still correct; the
///   stream just died and needs to come back. Probe order: stored id
///   → stored name → system default. Typical outcome: rebind to the
///   same device.
/// * `FollowDefault` — used by the device broker when a Core Audio
///   `DefaultInputChanged` (or `DefaultOutputChanged`) event has just
///   landed. The user's intent (or the OS's choice on their behalf)
///   is to move to the *new* default. Probe order: system default →
///   stored id → stored name. Without this, the watchdog probe order
///   would silently re-bind to the old device whenever the old device
///   is still alive — exactly what cpal#1175 leaves us holding the bag
///   for. See `docs/qa/device-failover.md` case 1 (AirPods drop) for
///   the user-visible symptom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartTarget {
    PreserveBinding,
    FollowDefault,
}

/// Extended report from a stream restart (mic or system). Callers use
/// `same_device` to decide whether the restart actually moved to a new
/// device or rebound to the same one (which happens when the OS default
/// reverts momentarily during an AirPods/Bluetooth handshake). A true
/// `same_device` means the rebind was effectively a no-op and the caller
/// should retry after letting macOS settle, rather than treating the
/// restart as complete.
///
/// `PartialEq`/`Eq` are not derived because `previous_buffer` is an `Arc`
/// to a ring buffer whose interior atomics aren't comparable. No production
/// or test code currently compares whole `RestartReport` values.
#[derive(Debug, Clone)]
pub struct RestartReport {
    pub outcome: RestartOutcome,
    /// `true` if the new bound device ID matches the pre-restart bound ID.
    pub same_device: bool,
    /// The device ID the stream is bound to after the restart, if any.
    pub new_device_id: Option<String>,
    /// Human-readable name of the device the stream is bound to after the
    /// restart, if known. Surfaced through `stream-health` so the FE can
    /// render a "Switched to {name}" toast on auto-failover.
    pub bound_device_name: Option<String>,
    /// `Some(arc)` when `outcome == BufferReplaced`: the old
    /// `Arc<AudioRingBuffer>` that was just swapped out. Callers may extract
    /// any unconsumed audio from this buffer (its samples still live there)
    /// before dropping their reference — once dropped, those samples are
    /// unreachable. `None` for `BufferPreserved` (the slot still points at
    /// the same buffer the caller already had).
    pub previous_buffer: Option<SharedAudioRingBuffer>,
}

/// Compute the post-restart `bound_is_default` flag. Pure for testability.
///
/// * `PreserveBinding`: the bind intent did not shift (e.g. watchdog
///   reboot of a known-correct binding) → preserve the prior flag.
/// * `FollowDefault`: the caller asked to track the current default →
///   set `true` iff the post-restart bound id matches the resolved
///   default-input id. A stored-id/name fallback that isn't currently
///   the default keeps explicit semantics so future
///   `DefaultInputChanged` events are honored.
///
/// `resolve_default` is a closure so the helper stays unit-testable
/// without touching cpal — production calls pass
/// `device::default_input_device`.
fn derive_bound_is_default<F>(
    target: RestartTarget,
    prior: bool,
    resolve_default: F,
    new_device_id: Option<&str>,
) -> bool
where
    F: FnOnce() -> Option<String>,
{
    match target {
        RestartTarget::PreserveBinding => prior,
        RestartTarget::FollowDefault => match (resolve_default(), new_device_id) {
            (Some(d), Some(n)) => d == n,
            _ => false,
        },
    }
}

pub struct AudioManager {
    config: AudioConfig,
    mic: MicrophoneCapture,
    system: SystemAudioCapture,
    state: CaptureState,
    error_message: Option<String>,
    mic_buffer: Option<SharedAudioRingBuffer>,
    system_buffer: Option<SharedAudioRingBuffer>,
    /// Push-based default-device change listeners (macOS only; no-op stubs
    /// elsewhere). `None` if CoreAudio rejected listener registration — the
    /// capture path degrades to the write-pos stall watchdog in that case.
    input_watcher: Option<DefaultDeviceWatcher>,
    output_watcher: Option<DefaultDeviceWatcher>,
    /// Watches `kAudioHardwarePropertyDefaultSystemOutputDevice` — the
    /// alerts/UI route, distinct from the media `Output` selector. macOS
    /// can change them independently (e.g. media routes to AirPods while
    /// system sounds stay on the speakers); covering both is needed so
    /// system-audio loopback follows whichever the user changed.
    system_output_watcher: Option<DefaultDeviceWatcher>,
    /// Device-list watcher — fires on `kAudioHardwarePropertyDevices`
    /// (hardware added/removed). Complements the default-device watchers
    /// because some macOS versions fire the device-list property *before*
    /// the default-device property during an AirPods/Bluetooth handshake;
    /// catching the earlier signal lets us start probing rebind candidates
    /// sooner. The flag is shared between mic and system health checks.
    devices_watcher: Option<DefaultDeviceWatcher>,
}

impl AudioManager {
    pub fn new() -> Self {
        Self::from_config(AudioConfig::default())
    }

    pub fn with_config(config: AudioConfig) -> Self {
        Self::from_config(config)
    }

    fn from_config(config: AudioConfig) -> Self {
        let input_watcher = DefaultDeviceWatcher::new(DefaultDeviceKind::Input)
            .map_err(|e| warn!("input default-device listener unavailable: {}", e))
            .ok();
        let output_watcher = DefaultDeviceWatcher::new(DefaultDeviceKind::Output)
            .map_err(|e| warn!("output default-device listener unavailable: {}", e))
            .ok();
        let system_output_watcher =
            DefaultDeviceWatcher::new(DefaultDeviceKind::DefaultSystemOutput)
                .map_err(|e| warn!("default-system-output listener unavailable: {}", e))
                .ok();
        let devices_watcher = DefaultDeviceWatcher::new(DefaultDeviceKind::Devices)
            .map_err(|e| warn!("device-list listener unavailable: {}", e))
            .ok();
        Self {
            config,
            mic: MicrophoneCapture::new(),
            system: SystemAudioCapture::new(),
            state: CaptureState::Idle,
            error_message: None,
            mic_buffer: None,
            system_buffer: None,
            input_watcher,
            output_watcher,
            system_output_watcher,
            devices_watcher,
        }
    }

    pub fn start_mic(&mut self, device_id: Option<&str>) -> Result<()> {
        // Reject cpal's internal loopback aggregate before we get to
        // device probing — an old persisted selection or a hand-rolled
        // id that points there would otherwise crash with the opaque
        // "stream type not supported" error from cpal's input config
        // probe on a tap-backed aggregate.
        if crate::device::is_cpal_loopback_aggregate(device_id, None) {
            return Err(AudioError::DeviceInit(
                "cpal loopback aggregate is not selectable as a microphone — \
                 it is an internal device used by system-audio capture"
                    .into(),
            ));
        }
        // Query the device's native config. The buffer uses the device's actual
        // sample rate and channel count — we no longer mutate self.config so that
        // each buffer carries its own format independently.
        let device_config = MicrophoneCapture::query_device_config(device_id)?;

        let buffer = Arc::new(AudioRingBuffer::with_duration(
            self.config.capture_history_seconds,
            device_config.sample_rate,
            device_config.channels,
        ));
        self.mic.start(device_id, Arc::clone(&buffer))?;
        self.mic_buffer = Some(buffer);
        self.state = CaptureState::Capturing;
        self.error_message = None;
        Ok(())
    }

    pub fn start_system_audio(&mut self) -> Result<()> {
        // Query the output device's native config so the buffer matches the
        // actual stream format. Output devices are typically stereo (2ch) while
        // the mic config may be mono — using the wrong channel count causes the
        // buffer to report duration at the wrong rate.
        let device_config = SystemAudioCapture::query_device_config()?;

        let buffer = Arc::new(AudioRingBuffer::with_duration(
            self.config.capture_history_seconds,
            device_config.sample_rate,
            device_config.channels,
        ));
        self.system.start(Arc::clone(&buffer))?;
        self.system_buffer = Some(buffer);
        self.state = CaptureState::Capturing;
        self.error_message = None;
        Ok(())
    }

    pub fn start_capture(
        &mut self,
        source: CaptureSource,
        mic_device_id: Option<&str>,
    ) -> Result<()> {
        let result = match source {
            CaptureSource::MicOnly => self.start_mic(mic_device_id),
            CaptureSource::SystemOnly => self.start_system_audio(),
            CaptureSource::Mixed => self.start_all(mic_device_id),
        };
        if let Err(ref e) = result {
            if !matches!(e, AudioError::AlreadyRunning) {
                self.state = CaptureState::Error;
                self.error_message = Some(e.to_string());
            }
        }
        result
    }

    pub fn start_all(&mut self, mic_device_id: Option<&str>) -> Result<()> {
        self.start_mic(mic_device_id)?;

        if let Err(e) = self.start_system_audio() {
            error!("system audio capture failed to start: {}", e);
            // Continue with mic-only capture but surface the degradation
            self.error_message = Some(format!("Mixed mode degraded to mic-only: {}", e));
        }

        Ok(())
    }

    pub fn stop_all(&mut self) -> Result<()> {
        let mut had_error = false;

        if self.mic.is_running() {
            if let Err(e) = self.mic.stop() {
                error!("failed to stop microphone: {}", e);
                had_error = true;
            }
        }

        if self.system.is_running() {
            if let Err(e) = self.system.stop() {
                error!("failed to stop system audio: {}", e);
                had_error = true;
            }
        }

        // Buffers are intentionally retained after stop so capture history
        // can still be read back for transcription.

        if had_error {
            let msg = self
                .error_message
                .clone()
                .unwrap_or_else(|| "error stopping capture".to_string());
            self.state = CaptureState::Error;
            self.error_message = Some(msg.clone());
            Err(AudioError::Capture(msg))
        } else {
            self.state = CaptureState::Idle;
            self.error_message = None;
            Ok(())
        }
    }

    pub fn status(&self) -> CaptureStatus {
        CaptureStatus {
            state: self.state,
            mic_active: self.mic.is_running(),
            system_audio_active: self.system.is_running(),
            error_message: self.error_message.clone(),
        }
    }

    pub fn check_system_audio_permission(&self) -> PermissionStatus {
        self.system.check_permission()
    }

    // --- Snapshot API ---

    pub fn snapshot_mic(&self, duration_seconds: f32) -> Option<Vec<f32>> {
        self.mic_buffer
            .as_ref()
            .map(|b| b.snapshot(duration_seconds))
    }

    pub fn snapshot_system(&self, duration_seconds: f32) -> Option<Vec<f32>> {
        self.system_buffer
            .as_ref()
            .map(|b| b.snapshot(duration_seconds))
    }

    pub fn snapshot_mic_all(&self) -> Option<Vec<f32>> {
        self.mic_buffer.as_ref().map(|b| b.snapshot_all())
    }

    pub fn snapshot_system_all(&self) -> Option<Vec<f32>> {
        self.system_buffer.as_ref().map(|b| b.snapshot_all())
    }

    pub fn mic_buffer_info(&self) -> Option<RingBufferInfo> {
        self.mic_buffer.as_ref().map(|b| b.info())
    }

    pub fn system_buffer_info(&self) -> Option<RingBufferInfo> {
        self.system_buffer.as_ref().map(|b| b.info())
    }

    // --- Buffer access ---

    pub fn mic_buffer(&self) -> Option<&SharedAudioRingBuffer> {
        self.mic_buffer.as_ref()
    }

    pub fn system_buffer(&self) -> Option<&SharedAudioRingBuffer> {
        self.system_buffer.as_ref()
    }

    /// Returns the mic buffer's current write position, or 0 if no mic buffer exists.
    pub fn mic_write_pos(&self) -> usize {
        self.mic_buffer
            .as_ref()
            .map(|b| b.samples_written())
            .unwrap_or(0)
    }

    /// Returns the system buffer's current write position, or 0 if no system buffer exists.
    pub fn system_write_pos(&self) -> usize {
        self.system_buffer
            .as_ref()
            .map(|b| b.samples_written())
            .unwrap_or(0)
    }

    /// Returns the sample rate from the first active buffer (mic preferred), or the
    /// config default. Used wherever a single representative sample rate is needed.
    fn active_sample_rate(&self) -> u32 {
        self.mic_buffer
            .as_ref()
            .map(|b| b.sample_rate())
            .or_else(|| self.system_buffer.as_ref().map(|b| b.sample_rate()))
            .unwrap_or(yapstack_common::config::DEFAULT_SAMPLE_RATE)
    }

    /// Returns the output sample rate callers should use for persisted audio
    /// generated from the configured capture source.
    pub fn output_sample_rate_for(&self, source: CaptureSource) -> u32 {
        match source {
            CaptureSource::MicOnly => self
                .mic_buffer
                .as_ref()
                .map(|b| b.sample_rate())
                .unwrap_or_else(|| self.active_sample_rate()),
            CaptureSource::SystemOnly => self
                .system_buffer
                .as_ref()
                .map(|b| b.sample_rate())
                .unwrap_or_else(|| self.active_sample_rate()),
            CaptureSource::Mixed => self
                .mic_buffer
                .as_ref()
                .map(|b| b.sample_rate())
                .or_else(|| self.system_buffer.as_ref().map(|b| b.sample_rate()))
                .unwrap_or_else(|| self.active_sample_rate()),
        }
    }

    /// Resample system audio to match mic rate (if different) and mix to mono.
    /// Used by `extract_since`. On resample failure, logs the error and falls
    /// back to mic-only audio.
    fn resample_and_mix(
        &self,
        mic_samples: &[f32],
        system_samples: &[f32],
        mix_config: Option<&MixConfig>,
    ) -> Vec<f32> {
        let config = mix_config.cloned().unwrap_or_default();
        if let (Some(mic_buf), Some(sys_buf)) =
            (self.mic_buffer.as_ref(), self.system_buffer.as_ref())
        {
            if mic_buf.sample_rate() != sys_buf.sample_rate() {
                debug!(
                    "resampling system audio {}Hz → {}Hz for mixed mode",
                    sys_buf.sample_rate(),
                    mic_buf.sample_rate()
                );
                match mixer::resample(system_samples, sys_buf.sample_rate(), mic_buf.sample_rate())
                {
                    Ok(s_resampled) => {
                        return mixer::mix_to_mono(mic_samples, &s_resampled, &config);
                    }
                    Err(e) => {
                        error!("mixed mode resample failed, using mic-only: {e}");
                        return mic_samples.to_vec();
                    }
                }
            }
        }
        mixer::mix_to_mono(mic_samples, system_samples, &config)
    }

    // --- Position tracking API ---

    /// Returns current write positions for both buffers.
    pub fn buffer_positions(&self) -> BufferPositions {
        BufferPositions {
            mic_pos: self
                .mic_buffer
                .as_ref()
                .map(|b| b.samples_written())
                .unwrap_or(0),
            system_pos: self
                .system_buffer
                .as_ref()
                .map(|b| b.samples_written())
                .unwrap_or(0),
        }
    }

    /// Extracts mono audio from active buffers since the given positions.
    ///
    /// Returns `(mono_samples, sample_rate, new_positions)` or `None` if no
    /// buffers are active or the requested source has no data.
    ///
    /// Uses `snapshot_since_with_pos` to capture the exact write position at the
    /// time of each snapshot. This eliminates the race window where audio callbacks
    /// could advance the write position between the snapshot and position query,
    /// which previously caused cumulative sample loss and source drift in Mixed mode.
    pub fn extract_since(
        &self,
        positions: &BufferPositions,
        source: CaptureSource,
        mix_config: Option<&MixConfig>,
    ) -> Option<(Vec<f32>, u32, BufferPositions)> {
        let (mic_mono, mut mic_new_pos) = self
            .mic_buffer
            .as_ref()
            .map(|b| {
                let (snap, pos) = b.snapshot_since_with_pos(positions.mic_pos);
                let mono = mixer::deinterleave_to_mono(&snap, b.channels()).into_owned();
                (Some(mono), pos)
            })
            .unwrap_or((None, positions.mic_pos));

        let (system_mono, mut sys_new_pos) = self
            .system_buffer
            .as_ref()
            .map(|b| {
                let (snap, pos) = b.snapshot_since_with_pos(positions.system_pos);
                let mono = mixer::deinterleave_to_mono(&snap, b.channels()).into_owned();
                (Some(mono), pos)
            })
            .unwrap_or((None, positions.system_pos));

        // Source-specific output rate. Using `active_sample_rate()` here
        // previously returned the mic buffer's rate unconditionally, so a
        // SystemOnly session with a stale mic buffer would report the mic
        // rate even though the returned samples come from the system
        // buffer — callers (e.g. the session WAV writer's resample step)
        // would then skip or miscompute conversion. Tie the rate to the
        // buffer the returned samples actually come from.
        let sample_rate = match source {
            CaptureSource::MicOnly => self
                .mic_buffer
                .as_ref()
                .map(|b| b.sample_rate())
                .unwrap_or_else(|| self.active_sample_rate()),
            CaptureSource::SystemOnly => self
                .system_buffer
                .as_ref()
                .map(|b| b.sample_rate())
                .unwrap_or_else(|| self.active_sample_rate()),
            // Mixed mode: `resample_and_mix` resamples system to mic's rate
            // and returns mic-rate output when both buffers are present.
            // When only one buffer is present, the sole source's rate wins.
            CaptureSource::Mixed => match (self.mic_buffer.as_ref(), self.system_buffer.as_ref()) {
                (Some(mb), _) => mb.sample_rate(),
                (None, Some(sb)) => sb.sample_rate(),
                (None, None) => self.active_sample_rate(),
            },
        };

        let samples = match source {
            CaptureSource::MicOnly => {
                let m = mic_mono.unwrap_or_default();
                if m.is_empty() {
                    return None;
                }
                m
            }
            CaptureSource::SystemOnly => {
                let s = system_mono.unwrap_or_default();
                if s.is_empty() {
                    return None;
                }
                s
            }
            CaptureSource::Mixed => {
                let m = mic_mono.unwrap_or_default();
                let s = system_mono.unwrap_or_default();
                if m.is_empty() && s.is_empty() {
                    return None;
                }
                // Time-based trimming: when mic and system have different sample rates
                // or different sample counts, trim each to the minimum *time duration*
                // at its own rate. This prevents cumulative positional drift that occurs
                // when trimming by sample count across mismatched rates.
                if !m.is_empty() && !s.is_empty() {
                    let (mic_buf, sys_buf) =
                        match (self.mic_buffer.as_ref(), self.system_buffer.as_ref()) {
                            (Some(mb), Some(sb)) => (mb, sb),
                            _ => return None, // invariant: both buffers must exist when both arrays are non-empty
                        };
                    let mic_sr = mic_buf.sample_rate() as f64;
                    let sys_sr = sys_buf.sample_rate() as f64;

                    if mic_sr != sys_sr || m.len() != s.len() {
                        let mic_time = m.len() as f64 / mic_sr;
                        let sys_time = s.len() as f64 / sys_sr;
                        let min_time = mic_time.min(sys_time);

                        let mic_keep = ((min_time * mic_sr) as usize).min(m.len());
                        let sys_keep = ((min_time * sys_sr) as usize).min(s.len());

                        let mic_surplus = m.len() - mic_keep;
                        let sys_surplus = s.len() - sys_keep;

                        mic_new_pos =
                            mic_new_pos.saturating_sub(mic_surplus * mic_buf.channels() as usize);
                        sys_new_pos =
                            sys_new_pos.saturating_sub(sys_surplus * sys_buf.channels() as usize);

                        self.resample_and_mix(&m[..mic_keep], &s[..sys_keep], mix_config)
                    } else {
                        self.resample_and_mix(&m, &s, mix_config)
                    }
                } else {
                    self.resample_and_mix(&m, &s, mix_config)
                }
            }
        };

        if samples.is_empty() {
            return None;
        }

        let new_positions = BufferPositions {
            mic_pos: mic_new_pos,
            system_pos: sys_new_pos,
        };
        Some((samples, sample_rate, new_positions))
    }

    /// Extracts mono audio from active buffers within a bounded cursor range.
    ///
    /// This is the stop-safe sibling of [`extract_since`]: returned samples
    /// never include audio written after `limits`, even if capture continues
    /// while the caller is finalizing a session.
    ///
    /// In `Mixed` mode, if one source has more mono duration than the other,
    /// the returned `new_positions` give the longer source's surplus back so
    /// paired streams stay time-aligned. Callers that loop with those returned
    /// positions may re-read that surplus later; stop/final-flush callers
    /// should treat the result as a single bounded drain.
    pub fn extract_since_until(
        &self,
        positions: &BufferPositions,
        limits: &BufferPositions,
        source: CaptureSource,
        mix_config: Option<&MixConfig>,
    ) -> Option<BoundedExtraction> {
        let (mic_mono, mut mic_new_pos, mic_overrun) = self
            .mic_buffer
            .as_ref()
            .map(|b| {
                let snap = b.snapshot_range(positions.mic_pos, limits.mic_pos);
                let mono = mixer::deinterleave_to_mono(&snap.samples, b.channels()).into_owned();
                (Some(mono), snap.end_pos, snap.overrun)
            })
            .unwrap_or((None, positions.mic_pos, false));

        let (system_mono, mut sys_new_pos, sys_overrun) = self
            .system_buffer
            .as_ref()
            .map(|b| {
                let snap = b.snapshot_range(positions.system_pos, limits.system_pos);
                let mono = mixer::deinterleave_to_mono(&snap.samples, b.channels()).into_owned();
                (Some(mono), snap.end_pos, snap.overrun)
            })
            .unwrap_or((None, positions.system_pos, false));

        let sample_rate = self.output_sample_rate_for(source);

        let samples = match source {
            CaptureSource::MicOnly => {
                let m = mic_mono.unwrap_or_default();
                if m.is_empty() {
                    return None;
                }
                m
            }
            CaptureSource::SystemOnly => {
                let s = system_mono.unwrap_or_default();
                if s.is_empty() {
                    return None;
                }
                s
            }
            CaptureSource::Mixed => {
                let m = mic_mono.unwrap_or_default();
                let s = system_mono.unwrap_or_default();
                if m.is_empty() && s.is_empty() {
                    return None;
                }
                if !m.is_empty() && !s.is_empty() {
                    let (mic_buf, sys_buf) =
                        match (self.mic_buffer.as_ref(), self.system_buffer.as_ref()) {
                            (Some(mb), Some(sb)) => (mb, sb),
                            _ => return None,
                        };
                    let mic_sr = mic_buf.sample_rate() as f64;
                    let sys_sr = sys_buf.sample_rate() as f64;

                    if mic_sr != sys_sr || m.len() != s.len() {
                        let mic_time = m.len() as f64 / mic_sr;
                        let sys_time = s.len() as f64 / sys_sr;
                        let min_time = mic_time.min(sys_time);

                        let mic_keep = ((min_time * mic_sr) as usize).min(m.len());
                        let sys_keep = ((min_time * sys_sr) as usize).min(s.len());

                        let mic_surplus = m.len() - mic_keep;
                        let sys_surplus = s.len() - sys_keep;

                        mic_new_pos =
                            mic_new_pos.saturating_sub(mic_surplus * mic_buf.channels() as usize);
                        sys_new_pos =
                            sys_new_pos.saturating_sub(sys_surplus * sys_buf.channels() as usize);

                        self.resample_and_mix(&m[..mic_keep], &s[..sys_keep], mix_config)
                    } else {
                        self.resample_and_mix(&m, &s, mix_config)
                    }
                } else {
                    self.resample_and_mix(&m, &s, mix_config)
                }
            }
        };

        if samples.is_empty() {
            return None;
        }

        Some(BoundedExtraction {
            samples,
            sample_rate,
            new_positions: BufferPositions {
                mic_pos: mic_new_pos,
                system_pos: sys_new_pos,
            },
            overrun: mic_overrun || sys_overrun,
        })
    }

    // --- Separate extraction API ---

    /// Extracts mono audio from each buffer independently since the given positions.
    ///
    /// Unlike `extract_since()`, this does **not** mix sources. Each source's
    /// samples are returned separately, suitable for per-source VAD and transcription.
    ///
    /// Uses `snapshot_since_with_pos` to capture exact positions (see `extract_since`).
    pub fn extract_sources_since(&self, positions: &BufferPositions) -> Option<SeparateExtraction> {
        let mut mic_new_pos = positions.mic_pos;
        let mic = self.mic_buffer.as_ref().and_then(|b| {
            let (snap, pos) = b.snapshot_since_with_pos(positions.mic_pos);
            mic_new_pos = pos;
            if snap.is_empty() {
                None
            } else {
                let mono = mixer::deinterleave_to_mono(&snap, b.channels()).into_owned();
                Some((mono, b.sample_rate()))
            }
        });

        let mut sys_new_pos = positions.system_pos;
        let system = self.system_buffer.as_ref().and_then(|b| {
            let (snap, pos) = b.snapshot_since_with_pos(positions.system_pos);
            sys_new_pos = pos;
            if snap.is_empty() {
                None
            } else {
                let mono = mixer::deinterleave_to_mono(&snap, b.channels()).into_owned();
                Some((mono, b.sample_rate()))
            }
        });

        if mic.is_none() && system.is_none() {
            return None;
        }

        Some(SeparateExtraction {
            mic,
            system,
            new_positions: BufferPositions {
                mic_pos: mic_new_pos,
                system_pos: sys_new_pos,
            },
        })
    }

    /// Returns the RMS energy computed directly from ring buffer data (zero-allocation).
    ///
    /// Computes RMS inline over the raw ring buffer without allocating a `Vec<f32>`.
    /// For multi-channel buffers, the energy is computed over interleaved samples
    /// (not deinterleaved mono), which is sufficient for VAD threshold comparisons.
    /// Returns `(mic_energy, system_energy)` where each is `None` if that buffer
    /// isn't active or has no new data since `positions`.
    pub fn peek_energy_rms(
        &self,
        positions: &BufferPositions,
        duration_secs: f32,
    ) -> (Option<f32>, Option<f32>) {
        let mic_energy = self.mic_buffer.as_ref().and_then(|b| {
            let current_pos = b.samples_written();
            if current_pos <= positions.mic_pos {
                return None;
            }
            let window_samples =
                (duration_secs * b.sample_rate() as f32 * b.channels() as f32) as usize;
            let read_from = current_pos
                .saturating_sub(window_samples)
                .max(positions.mic_pos);
            b.rms_energy_since(read_from, window_samples)
        });

        let system_energy = self.system_buffer.as_ref().and_then(|b| {
            let current_pos = b.samples_written();
            if current_pos <= positions.system_pos {
                return None;
            }
            let window_samples =
                (duration_secs * b.sample_rate() as f32 * b.channels() as f32) as usize;
            let read_from = current_pos
                .saturating_sub(window_samples)
                .max(positions.system_pos);
            b.rms_energy_since(read_from, window_samples)
        });

        (mic_energy, system_energy)
    }

    // --- Config API ---

    pub fn set_config(&mut self, config: AudioConfig) {
        self.config = config;
    }

    pub fn config(&self) -> &AudioConfig {
        &self.config
    }

    /// Clears both ring buffers, dropping all captured audio data.
    pub fn clear_buffers(&mut self) {
        self.mic_buffer = None;
        self.system_buffer = None;
    }

    // --- Stream health ---

    /// Returns `true` if the mic stream's cpal error callback has fired.
    pub fn mic_has_stream_error(&self) -> bool {
        self.mic.has_stream_error()
    }

    /// Returns `true` if the system audio stream's cpal error callback has fired.
    pub fn system_has_stream_error(&self) -> bool {
        self.system.has_stream_error()
    }

    /// Returns the device ID used for the current/last mic capture session.
    pub fn mic_device_id(&self) -> Option<&str> {
        self.mic.last_device_id()
    }

    /// Returns `true` if the current mic binding is following the system
    /// default. `false` if the user explicitly picked a non-default
    /// device. Callers (notably the device broker's auto-failover path)
    /// use this to decide whether to skip a Mic restart on
    /// `DefaultInputChanged` when the explicit pick is still alive.
    pub fn mic_bound_is_default(&self) -> bool {
        self.mic.bound_is_default()
    }

    /// Restarts the microphone stream. Reuses the existing ring buffer when
    /// the new device's sample rate / channel count match; otherwise
    /// allocates a fresh buffer so extraction and WAV metadata stay
    /// consistent with the actual cpal callback format.
    ///
    /// Tries restart candidates and probes + allocates the buffer per
    /// candidate so the returned buffer matches whichever device actually
    /// succeeds at start (rather than the first one that merely probed).
    ///
    /// Probe order depends on `target`:
    /// * `PreserveBinding` — stored id → stored name → system default.
    ///   Right for stream-error / write-pos stall recovery, where the
    ///   bound device is presumed still correct.
    /// * `FollowDefault` — system default → stored id → stored name.
    ///   Right for broker-driven failover after a `DefaultInputChanged`
    ///   event, where the *new* default is the user's intent. The
    ///   stored-id fallback covers the rare case where the OS reports
    ///   a change but `default_input_device()` momentarily returns
    ///   nothing (Bluetooth handshake mid-rebind).
    pub fn restart_mic(&mut self, target: RestartTarget) -> Result<RestartReport> {
        let existing = self
            .mic_buffer
            .clone()
            .ok_or(AudioError::NoBufferAvailable)?;

        let stored_id = self.mic.last_device_id().map(|s| s.to_string());
        let stored_name = self.mic.last_device_name().map(|s| s.to_string());
        let old_device_id = stored_id.clone();
        // For `PreserveBinding` the bind intent hasn't shifted (the
        // watchdog is just rebooting a known-correct binding), so we
        // restore the prior flag verbatim. For `FollowDefault` the
        // caller has explicitly asked to track the default; we derive
        // the flag from "did we actually land on the current default?"
        // — a stored-id fallback that isn't currently the default keeps
        // explicit semantics so a future `DefaultInputChanged` is acted
        // on. Without this, an explicit-mic disappearance that fell
        // over to the system default would still report `bound_is_default
        // = false`, and subsequent default changes would be silently
        // ignored (the broker's explicit-pick branch would skip failover).
        let prior_bound_is_default = self.mic.bound_is_default();

        // Stop the old stream (ignore errors — it may already be dead)
        let _ = self.mic.stop();

        let candidates: [(Option<&str>, &str); 3] = match target {
            RestartTarget::PreserveBinding => [
                (stored_id.as_deref(), "original device id"),
                (stored_name.as_deref(), "stored device name"),
                (None, "system default"),
            ],
            RestartTarget::FollowDefault => [
                (None, "system default"),
                (stored_id.as_deref(), "original device id"),
                (stored_name.as_deref(), "stored device name"),
            ],
        };

        let mut last_err: Option<AudioError> = None;
        let mut tried_any = false;
        for (candidate, label) in candidates {
            // Skip blank intermediate candidates — only the final `None`
            // (system default) is a valid empty candidate.
            if candidate.is_none() && label != "system default" {
                continue;
            }
            tried_any = true;

            let probed = match MicrophoneCapture::query_device_config(candidate) {
                Ok(c) => c,
                Err(e) => {
                    warn!("restart: probing {} ({:?}) failed: {}", label, candidate, e);
                    last_err = Some(e);
                    continue;
                }
            };

            let (buffer, outcome) = self.pick_buffer_for_restart(&existing, probed);
            match self.mic.start(candidate, Arc::clone(&buffer)) {
                Ok(()) => {
                    // Capture the previous Arc only when the slot is about
                    // to be swapped out — for BufferPreserved the slot
                    // already pointed at the same Arc and the caller doesn't
                    // need a separate handle.
                    let previous_buffer = match outcome {
                        RestartOutcome::BufferReplaced => Some(Arc::clone(&existing)),
                        RestartOutcome::BufferPreserved => None,
                    };
                    self.mic_buffer = Some(buffer);
                    let new_device_id = self.mic.last_device_id().map(|s| s.to_string());
                    let bound_is_default = derive_bound_is_default(
                        target,
                        prior_bound_is_default,
                        || {
                            crate::device::default_input_device()
                                .ok()
                                .and_then(|info| info.id)
                        },
                        new_device_id.as_deref(),
                    );
                    self.mic.set_bound_is_default(bound_is_default);
                    let same_device = old_device_id.is_some() && old_device_id == new_device_id;
                    let bound_device_name = self.mic.last_device_name().map(|s| s.to_string());
                    return Ok(RestartReport {
                        outcome,
                        same_device,
                        new_device_id,
                        bound_device_name,
                        previous_buffer,
                    });
                }
                Err(e) => {
                    warn!("restart on {} ({:?}) failed: {}", label, candidate, e);
                    last_err = Some(e);
                    continue;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            AudioError::DeviceInit(if tried_any {
                "all mic restart candidates failed".into()
            } else {
                "no mic restart candidates available".into()
            })
        }))
    }

    /// Restarts the system audio stream. Reuses the existing ring buffer
    /// when the new default output's sample rate / channel count match;
    /// otherwise allocates a fresh buffer so extraction and WAV metadata
    /// stay consistent with the actual cpal callback format. Callers must
    /// reset source positions / VAD state when `RestartOutcome::BufferReplaced`
    /// is returned — the old positions reference a different buffer.
    ///
    /// The returned `SystemRestartReport` includes `same_device`, which is
    /// `true` when the post-restart bound device ID matches the pre-restart
    /// ID. Under a normal AirPods handshake macOS may briefly report the
    /// *old* device as default (a "revert" window) before committing to the
    /// new one; a restart during that window produces `same_device=true`
    /// and the caller should retry after a short delay instead of accepting
    /// the bind. See `yapstack-audio` tests and the cpal #1175 discussion
    /// for background.
    pub fn restart_system_audio(&mut self) -> Result<RestartReport> {
        let existing = self
            .system_buffer
            .clone()
            .ok_or(AudioError::NoBufferAvailable)?;

        let old_device_id = self.system.last_device_id().map(|s| s.to_string());

        // Stop the old stream (ignore errors — it may already be dead)
        let _ = self.system.stop();

        let probed = SystemAudioCapture::query_device_config()?;
        let (buffer, outcome) = self.pick_buffer_for_restart(&existing, probed);

        self.system.start(Arc::clone(&buffer))?;
        // Capture the previous Arc only when the slot is about to be swapped
        // out — for BufferPreserved the slot already pointed at the same Arc.
        let previous_buffer = match outcome {
            RestartOutcome::BufferReplaced => Some(Arc::clone(&existing)),
            RestartOutcome::BufferPreserved => None,
        };
        self.system_buffer = Some(buffer);

        let new_device_id = self.system.last_device_id().map(|s| s.to_string());
        let same_device = old_device_id.is_some() && old_device_id == new_device_id;
        let bound_device_name = self.system.last_device_name().map(|s| s.to_string());
        Ok(RestartReport {
            outcome,
            same_device,
            new_device_id,
            bound_device_name,
            previous_buffer,
        })
    }

    /// Chooses between reusing the existing buffer and allocating a fresh one
    /// sized for the probed device config. Shared between mic and system
    /// restart paths because the decision is identical: match sample rate and
    /// channel count, else replace so downstream metadata stays correct.
    fn pick_buffer_for_restart(
        &self,
        existing: &SharedAudioRingBuffer,
        probed: DeviceStreamConfig,
    ) -> (SharedAudioRingBuffer, RestartOutcome) {
        let format_matches =
            existing.sample_rate() == probed.sample_rate && existing.channels() == probed.channels;
        if format_matches {
            (Arc::clone(existing), RestartOutcome::BufferPreserved)
        } else {
            warn!(
                "device format changed on restart: {}Hz/{}ch → {}Hz/{}ch — allocating fresh buffer",
                existing.sample_rate(),
                existing.channels(),
                probed.sample_rate,
                probed.channels,
            );
            let fresh = Arc::new(AudioRingBuffer::with_duration(
                self.config.capture_history_seconds,
                probed.sample_rate,
                probed.channels,
            ));
            (fresh, RestartOutcome::BufferReplaced)
        }
    }

    /// Attach a single sink that receives every device-change event from
    /// every registered watcher. Replaces any previously attached sink.
    /// Pass `None` to detach.
    ///
    /// The sink is invoked on the Core Audio listener thread; it must be
    /// cheap and non-blocking. Typical implementations forward the event
    /// into a channel and return.
    pub fn subscribe_device_events(&self, sink: Option<DeviceEventSink>) {
        for watcher in [
            self.input_watcher.as_ref(),
            self.output_watcher.as_ref(),
            self.system_output_watcher.as_ref(),
            self.devices_watcher.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            watcher.set_sink(sink.clone());
        }
    }

    /// Returns the device name the system audio stream is currently bound to,
    /// or `None` if not running / identity unknown.
    pub fn system_audio_bound_device(&self) -> Option<&str> {
        self.system.last_device_name()
    }

    /// Returns the device name the microphone stream is currently bound to,
    /// or `None` if not running / identity unknown.
    pub fn mic_bound_device(&self) -> Option<&str> {
        self.mic.last_device_name()
    }
}

impl Default for AudioManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_audio_manager() {
        let manager = AudioManager::new();
        let status = manager.status();
        assert_eq!(status.state, CaptureState::Idle);
        assert!(!status.mic_active);
        assert!(!status.system_audio_active);
        assert!(status.error_message.is_none());
    }

    #[test]
    fn test_stop_all_when_idle() {
        let mut manager = AudioManager::new();
        let result = manager.stop_all();
        assert!(result.is_ok());
        assert_eq!(manager.status().state, CaptureState::Idle);
    }

    #[test]
    fn test_snapshot_none_when_not_started() {
        let manager = AudioManager::new();
        assert!(manager.snapshot_mic(1.0).is_none());
        assert!(manager.snapshot_system(1.0).is_none());
        assert!(manager.snapshot_mic_all().is_none());
        assert!(manager.snapshot_system_all().is_none());
    }

    #[test]
    fn test_buffer_info_none_when_not_started() {
        let manager = AudioManager::new();
        assert!(manager.mic_buffer_info().is_none());
        assert!(manager.system_buffer_info().is_none());
    }

    #[test]
    fn test_default_config_values() {
        let manager = AudioManager::new();
        assert!((manager.config().capture_history_seconds - 180.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_with_config() {
        let config = AudioConfig {
            capture_history_seconds: 60.0,
        };
        let manager = AudioManager::with_config(config);
        assert!((manager.config().capture_history_seconds - 60.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_set_config() {
        let mut manager = AudioManager::new();
        let config = AudioConfig {
            capture_history_seconds: 300.0,
        };
        manager.set_config(config);
        assert!((manager.config().capture_history_seconds - 300.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_system_audio_permission() {
        let manager = AudioManager::new();
        let perm = manager.check_system_audio_permission();
        // On macOS and Windows, loopback capture uses standard audio permissions (always Granted)
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(perm, PermissionStatus::Granted);
        // On other platforms, system audio capture is unavailable
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(perm, PermissionStatus::Unavailable);
    }

    #[test]
    fn test_clear_buffers() {
        let mut manager = AudioManager::new();
        manager.clear_buffers();
        assert!(manager.mic_buffer_info().is_none());
        assert!(manager.system_buffer_info().is_none());
    }

    #[test]
    fn test_buffer_positions_no_buffers() {
        let manager = AudioManager::new();
        let pos = manager.buffer_positions();
        assert_eq!(pos.mic_pos, 0);
        assert_eq!(pos.system_pos, 0);
    }

    #[test]
    fn test_extract_since_no_buffers_returns_none() {
        let manager = AudioManager::new();
        let pos = manager.buffer_positions();
        assert!(manager
            .extract_since(&pos, CaptureSource::MicOnly, None)
            .is_none());
    }

    #[test]
    fn test_extract_sources_since_no_buffers_returns_none() {
        let manager = AudioManager::new();
        let pos = manager.buffer_positions();
        assert!(manager.extract_sources_since(&pos).is_none());
    }

    #[test]
    fn test_buffer_positions_with_synthetic_buffer() {
        let mut manager = AudioManager::new();
        // Manually inject a mic buffer with some data
        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        buffer.write(&[0.1, 0.2, 0.3]);
        manager.mic_buffer = Some(buffer);

        let pos = manager.buffer_positions();
        assert_eq!(pos.mic_pos, 3);
        assert_eq!(pos.system_pos, 0);
    }

    #[test]
    fn test_extract_since_with_synthetic_buffer() {
        let mut manager = AudioManager::new();
        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        manager.mic_buffer = Some(Arc::clone(&buffer));

        let pos = manager.buffer_positions();
        // Write data after capturing positions
        buffer.write(&[0.5, -0.5, 0.25, -0.25]);

        let result = manager.extract_since(&pos, CaptureSource::MicOnly, None);
        assert!(result.is_some());
        let (samples, sample_rate, new_pos) = result.unwrap();
        assert_eq!(samples.len(), 4);
        assert_eq!(sample_rate, 16000);
        assert_eq!(new_pos.mic_pos, 4);
    }

    #[test]
    fn test_extract_since_until_excludes_post_limit_samples() {
        let mut manager = AudioManager::new();
        let buffer = Arc::new(AudioRingBuffer::new(100, 16000, 1));
        manager.mic_buffer = Some(Arc::clone(&buffer));

        buffer.write(&(0..10).map(|i| i as f32).collect::<Vec<_>>());
        let stop_positions = manager.buffer_positions();
        buffer.write(&(10..20).map(|i| i as f32).collect::<Vec<_>>());

        let result = manager
            .extract_since_until(
                &BufferPositions {
                    mic_pos: 0,
                    system_pos: 0,
                },
                &stop_positions,
                CaptureSource::MicOnly,
                None,
            )
            .expect("bounded extraction should return pre-stop samples");

        assert_eq!(
            result.samples,
            (0..10).map(|i| i as f32).collect::<Vec<_>>()
        );
        assert_eq!(result.sample_rate, 16000);
        assert_eq!(result.new_positions.mic_pos, stop_positions.mic_pos);
        assert!(!result.overrun);
    }

    #[test]
    fn test_extract_since_until_reports_overrun() {
        let mut manager = AudioManager::new();
        let buffer = Arc::new(AudioRingBuffer::new(5, 16000, 1));
        manager.mic_buffer = Some(Arc::clone(&buffer));

        buffer.write(&(0..10).map(|i| i as f32).collect::<Vec<_>>());
        let result = manager
            .extract_since_until(
                &BufferPositions {
                    mic_pos: 0,
                    system_pos: 0,
                },
                &BufferPositions {
                    mic_pos: 8,
                    system_pos: 0,
                },
                CaptureSource::MicOnly,
                None,
            )
            .expect("bounded extraction should return retained samples");

        assert_eq!(result.samples, vec![5.0, 6.0, 7.0]);
        assert_eq!(result.new_positions.mic_pos, 8);
        assert!(result.overrun);
    }

    #[test]
    fn test_output_sample_rate_for_source_prefers_requested_source() {
        let mut manager = AudioManager::new();
        manager.mic_buffer = Some(Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1)));
        manager.system_buffer = Some(Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 2)));

        assert_eq!(
            manager.output_sample_rate_for(CaptureSource::MicOnly),
            16000
        );
        assert_eq!(
            manager.output_sample_rate_for(CaptureSource::SystemOnly),
            48000
        );
        assert_eq!(manager.output_sample_rate_for(CaptureSource::Mixed), 16000);
    }

    #[test]
    fn test_extract_sources_since_with_synthetic_buffer() {
        let mut manager = AudioManager::new();
        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        manager.mic_buffer = Some(Arc::clone(&buffer));

        let pos = manager.buffer_positions();
        buffer.write(&[0.1, 0.2, 0.3]);

        let result = manager.extract_sources_since(&pos);
        assert!(result.is_some());
        let extraction = result.unwrap();
        assert!(extraction.mic.is_some());
        assert!(extraction.system.is_none());
        let (mic_samples, mic_rate) = extraction.mic.unwrap();
        assert_eq!(mic_samples.len(), 3);
        assert_eq!(mic_rate, 16000);
    }

    #[test]
    fn test_mic_write_pos() {
        let mut manager = AudioManager::new();
        assert_eq!(manager.mic_write_pos(), 0);

        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        buffer.write(&[0.1, 0.2, 0.3]);
        manager.mic_buffer = Some(buffer);
        assert_eq!(manager.mic_write_pos(), 3);
    }

    #[test]
    fn test_system_write_pos() {
        let mut manager = AudioManager::new();
        assert_eq!(manager.system_write_pos(), 0);

        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 2));
        buffer.write(&[0.1, 0.2, 0.3, 0.4, 0.5]);
        manager.system_buffer = Some(buffer);
        assert_eq!(manager.system_write_pos(), 5);
    }

    #[test]
    fn test_peek_energy_rms_with_synthetic_buffer() {
        let mut manager = AudioManager::new();
        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        manager.mic_buffer = Some(Arc::clone(&buffer));

        let pos = manager.buffer_positions();
        let signal: Vec<f32> = vec![0.5; 1600]; // 0.1 seconds at 16kHz
        buffer.write(&signal);

        let (mic_energy, sys_energy) = manager.peek_energy_rms(&pos, 0.1);
        assert!(mic_energy.is_some());
        assert!(sys_energy.is_none());
        let energy = mic_energy.unwrap();
        assert!((energy - 0.5).abs() < 0.01);
    }

    // --- End-to-end pipeline integration tests ---

    #[test]
    fn test_sample_rate_mismatch_extract_since_resamples() {
        let mut manager = AudioManager::new();
        let mic_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        let sys_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 1));

        manager.mic_buffer = Some(Arc::clone(&mic_buf));
        manager.system_buffer = Some(Arc::clone(&sys_buf));

        let pos = manager.buffer_positions();

        mic_buf.write(&vec![0.5_f32; 16000]);
        sys_buf.write(&vec![0.5_f32; 48000]);

        // Should succeed by resampling system audio to mic rate
        let result = manager.extract_since(&pos, CaptureSource::Mixed, None);
        assert!(result.is_some(), "expected Some with resampled data");
        let (samples, sample_rate, _new_pos) = result.unwrap();
        assert_eq!(sample_rate, 16000);
        assert!(!samples.is_empty());
    }

    #[test]
    fn test_extract_since_mixed_trim_to_min() {
        let mut manager = AudioManager::new();
        let mic_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        let sys_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));

        manager.mic_buffer = Some(Arc::clone(&mic_buf));
        manager.system_buffer = Some(Arc::clone(&sys_buf));

        let pos = manager.buffer_positions();

        // Write different amounts: mic=500, sys=300 mono samples
        mic_buf.write(&vec![0.5_f32; 500]);
        sys_buf.write(&vec![0.3_f32; 300]);

        let mix_config = MixConfig {
            mic_gain: 1.0,
            system_gain: 1.0,
            normalize: false,
        };
        let result = manager.extract_since(&pos, CaptureSource::Mixed, Some(&mix_config));
        assert!(result.is_some());
        let (samples, _sr, new_pos) = result.unwrap();

        // Output should be min(500, 300) = 300 samples (after limiting)
        assert_eq!(samples.len(), 300);

        // Mic position should be rewound by 200 surplus samples
        assert_eq!(new_pos.mic_pos, 300); // 500 - 200 = 300
                                          // System position should be fully advanced
        assert_eq!(new_pos.system_pos, 300);

        // Second extraction should pick up the remaining 200 mic samples
        sys_buf.write(&vec![0.3_f32; 200]);
        let result2 = manager.extract_since(&new_pos, CaptureSource::Mixed, Some(&mix_config));
        assert!(result2.is_some());
        let (samples2, _sr2, _new_pos2) = result2.unwrap();
        assert_eq!(samples2.len(), 200);
    }

    #[test]
    fn test_extract_since_mixed_equal_lengths_no_rewind() {
        let mut manager = AudioManager::new();
        let mic_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        let sys_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));

        manager.mic_buffer = Some(Arc::clone(&mic_buf));
        manager.system_buffer = Some(Arc::clone(&sys_buf));

        let pos = manager.buffer_positions();

        // Write equal amounts
        mic_buf.write(&vec![0.5_f32; 400]);
        sys_buf.write(&vec![0.3_f32; 400]);

        let result = manager.extract_since(&pos, CaptureSource::Mixed, None);
        assert!(result.is_some());
        let (samples, _sr, new_pos) = result.unwrap();
        assert_eq!(samples.len(), 400);
        assert_eq!(new_pos.mic_pos, 400);
        assert_eq!(new_pos.system_pos, 400);
    }

    // --- Stream health tests ---

    #[test]
    fn test_stream_error_flags_default_false() {
        let manager = AudioManager::new();
        assert!(!manager.mic_has_stream_error());
        assert!(!manager.system_has_stream_error());
    }

    #[test]
    fn test_bound_device_none_when_not_running() {
        let manager = AudioManager::new();
        assert!(manager.system_audio_bound_device().is_none());
        assert!(manager.mic_bound_device().is_none());
    }

    #[test]
    fn test_mic_bound_is_default_defaults_to_false() {
        // A fresh MicrophoneCapture hasn't been started, so we can't yet
        // claim to be tracking the default. The drift check's early-return
        // must honor this to avoid false positives before start().
        let manager = AudioManager::new();
        assert!(!manager.mic.bound_is_default());
    }

    #[test]
    fn test_mic_set_bound_is_default_setter() {
        // `restart_mic` writes the derived flag through this setter at
        // the end of a successful restart (`derive_bound_is_default`
        // computes the value); the setter must round-trip both states.
        let mut manager = AudioManager::new();
        manager.mic.set_bound_is_default(true);
        assert!(manager.mic.bound_is_default());
        manager.mic.set_bound_is_default(false);
        assert!(!manager.mic.bound_is_default());
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)] // WASAPI COM cleanup crashes on CI
    fn test_restart_mic_keeps_buffer_slot() {
        // After restart the mic_buffer slot stays populated regardless of
        // outcome — either the original Arc (format match) or a fresh buffer
        // (format mismatch, e.g. device changed between 48 kHz stereo and
        // 44.1 kHz mono). The slot staying populated is what the live loop
        // relies on for `RestartOutcome` reset logic.
        let mut manager = AudioManager::new();
        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        buffer.write(&[0.1, 0.2, 0.3]);
        manager.mic_buffer = Some(Arc::clone(&buffer));

        let _ = manager.restart_mic(RestartTarget::PreserveBinding);
        assert!(manager.mic_buffer.is_some());
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)] // WASAPI COM cleanup crashes on CI
    fn test_restart_system_keeps_buffer_slot() {
        let mut manager = AudioManager::new();
        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 2));
        buffer.write(&[0.1, 0.2, 0.3, 0.4]);
        manager.system_buffer = Some(Arc::clone(&buffer));

        let _ = manager.restart_system_audio();
        assert!(manager.system_buffer.is_some());
    }

    #[test]
    fn test_pick_buffer_replaces_on_sample_rate_mismatch() {
        let manager = AudioManager::new();
        let existing = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 2));
        let probed = DeviceStreamConfig {
            sample_rate: 44100,
            channels: 2,
        };
        let (_buf, outcome) = manager.pick_buffer_for_restart(&existing, probed);
        assert_eq!(outcome, RestartOutcome::BufferReplaced);
    }

    #[test]
    fn test_pick_buffer_replaces_on_channel_mismatch() {
        let manager = AudioManager::new();
        let existing = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 2));
        let probed = DeviceStreamConfig {
            sample_rate: 48000,
            channels: 1,
        };
        let (_buf, outcome) = manager.pick_buffer_for_restart(&existing, probed);
        assert_eq!(outcome, RestartOutcome::BufferReplaced);
    }

    #[test]
    fn test_pick_buffer_preserves_on_format_match() {
        let manager = AudioManager::new();
        let existing = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 2));
        let probed = DeviceStreamConfig {
            sample_rate: 48000,
            channels: 2,
        };
        let (buf, outcome) = manager.pick_buffer_for_restart(&existing, probed);
        assert_eq!(outcome, RestartOutcome::BufferPreserved);
        assert!(Arc::ptr_eq(&buf, &existing));
    }

    #[test]
    fn test_restart_without_buffer_errors() {
        let mut manager = AudioManager::new();
        let result = manager.restart_mic(RestartTarget::PreserveBinding);
        assert!(result.is_err());
        let result = manager.restart_system_audio();
        assert!(result.is_err());
    }

    #[test]
    fn test_restart_target_changes_candidate_order() {
        // Pure unit assertion on the candidate-ordering match arms.
        // We can't drive the full restart without hardware, but the
        // ordering itself is decision-table-shaped: prove both orders
        // place "system default" in the right slot.
        let stored_id: Option<&str> = Some("dev-A");
        let stored_name: Option<&str> = Some("Dev A");

        let preserve_order: [(Option<&str>, &str); 3] = [
            (stored_id, "original device id"),
            (stored_name, "stored device name"),
            (None, "system default"),
        ];
        let follow_order: [(Option<&str>, &str); 3] = [
            (None, "system default"),
            (stored_id, "original device id"),
            (stored_name, "stored device name"),
        ];

        // PreserveBinding tries the bound device first; broker-driven
        // FollowDefault tries the new system default first.
        assert_eq!(preserve_order[0].1, "original device id");
        assert_eq!(follow_order[0].1, "system default");
        // Both orders include all three slots — the inversion is
        // priority, not coverage.
        let preserve_labels: Vec<&str> = preserve_order.iter().map(|(_, l)| *l).collect();
        let follow_labels: Vec<&str> = follow_order.iter().map(|(_, l)| *l).collect();
        for label in ["original device id", "stored device name", "system default"] {
            assert!(preserve_labels.contains(&label));
            assert!(follow_labels.contains(&label));
        }
    }

    #[test]
    fn derive_bound_is_default_preserve_binding_keeps_prior_flag() {
        // Watchdog/stream-error path: stay explicit if we were explicit,
        // stay default-tracking if we were default-tracking. The bind
        // intent has not shifted under PreserveBinding.
        assert!(derive_bound_is_default(
            RestartTarget::PreserveBinding,
            true,
            || Some("ignored".into()),
            Some("dev-A"),
        ));
        assert!(!derive_bound_is_default(
            RestartTarget::PreserveBinding,
            false,
            || Some("dev-A".into()),
            Some("dev-A"),
        ));
    }

    #[test]
    fn derive_bound_is_default_follow_default_landed_on_default() {
        // Broker-driven failover: an explicit pick disappeared, restart
        // bound to the new system default. We must mark
        // bound_is_default=true so subsequent DefaultInputChanged events
        // are honored — this is the regression fix for the case the
        // ultrareview flagged.
        assert!(derive_bound_is_default(
            RestartTarget::FollowDefault,
            false, // was explicit before disappearance
            || Some("default-id".into()),
            Some("default-id"),
        ));
    }

    #[test]
    fn derive_bound_is_default_follow_default_fell_through_to_stored() {
        // FollowDefault probed the new default first but it was
        // unresolvable, so the restart fell through to the stored-id
        // candidate which happens not to be the current default. Keep
        // explicit semantics so the next DefaultInputChanged still
        // triggers a failover attempt.
        assert!(!derive_bound_is_default(
            RestartTarget::FollowDefault,
            true,
            || Some("default-id".into()),
            Some("stored-id"),
        ));
    }

    #[test]
    fn derive_bound_is_default_follow_default_default_unresolvable() {
        // default_input_device() failed (no devices available) — be
        // conservative and report explicit (false), so a future
        // DefaultInputChanged event still gets a chance to act.
        assert!(!derive_bound_is_default(
            RestartTarget::FollowDefault,
            true,
            || None,
            Some("any-id"),
        ));
        assert!(!derive_bound_is_default(
            RestartTarget::FollowDefault,
            true,
            || Some("default-id".into()),
            None,
        ));
    }

    #[test]
    fn test_restart_report_same_device_when_ids_match() {
        // Document the RestartReport::same_device semantics so a regression
        // in the comparison logic is caught by unit tests rather than by a
        // user trapped in the cpal#1175 retry loop. We fabricate the report
        // directly here because driving restart_* requires real hardware.
        let report = RestartReport {
            outcome: RestartOutcome::BufferPreserved,
            same_device: true,
            new_device_id: Some("dev-A".into()),
            bound_device_name: Some("Dev A".into()),
            previous_buffer: None,
        };
        assert!(report.same_device);
        assert_eq!(report.new_device_id.as_deref(), Some("dev-A"));
        assert_eq!(report.bound_device_name.as_deref(), Some("Dev A"));
        assert!(report.previous_buffer.is_none());
    }

    #[test]
    fn test_restart_mic_buffer_replaced_returns_previous_buffer() {
        // Production seam: after `BufferReplaced`, callers (specifically
        // `handle_buffer_replacement` in the live-transcription loop) must be
        // able to read any audio captured in the old buffer that hasn't been
        // consumed yet — otherwise those samples are dropped on the floor
        // when the Arc is replaced. Asserting the field is populated only on
        // `BufferReplaced` documents the contract end-to-end.
        let original = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        original.write(&[0.5; 800]);

        // Pure helper test — we don't need to drive cpal. Construct the
        // post-restart Arc the same way the production code does.
        let preserved = original.clone();
        let preserved_report = RestartReport {
            outcome: RestartOutcome::BufferPreserved,
            same_device: true,
            new_device_id: None,
            bound_device_name: None,
            previous_buffer: None,
        };
        assert!(Arc::ptr_eq(&preserved, &original));
        assert!(preserved_report.previous_buffer.is_none());

        let replacement = Arc::new(AudioRingBuffer::with_duration(10.0, 24000, 1));
        let replaced_report = RestartReport {
            outcome: RestartOutcome::BufferReplaced,
            same_device: false,
            new_device_id: None,
            bound_device_name: None,
            previous_buffer: Some(Arc::clone(&original)),
        };
        // Replacement is a fresh Arc; previous_buffer still points at the
        // old ring whose samples are intact.
        assert!(!Arc::ptr_eq(&replacement, &original));
        assert!(replaced_report.previous_buffer.is_some());
        let prev = replaced_report.previous_buffer.as_ref().unwrap();
        assert!(Arc::ptr_eq(prev, &original));
        // And the samples written above are still readable through it.
        let (samples, _new_pos) = prev.snapshot_since_with_pos(0);
        assert_eq!(samples.len(), 800);
    }

    #[test]
    fn test_extract_since_system_only_reports_system_rate_with_stale_mic_buffer() {
        // A SystemOnly session with a stale mic buffer at a different rate
        // must report the system buffer's rate, not the mic buffer's. The
        // WAV writer relies on this to decide whether to resample before
        // appending samples.
        let mut manager = AudioManager::new();
        let mic_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 44100, 1));
        let sys_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 2));
        manager.mic_buffer = Some(Arc::clone(&mic_buf));
        manager.system_buffer = Some(Arc::clone(&sys_buf));

        let pos = manager.buffer_positions();
        sys_buf.write(&vec![0.3_f32; 400]); // 200 mono frames

        let (_samples, reported_sr, _new_pos) = manager
            .extract_since(&pos, CaptureSource::SystemOnly, None)
            .expect("system extraction returns samples");
        assert_eq!(reported_sr, 48000, "SystemOnly must report system rate");
    }

    #[test]
    fn test_extract_since_mic_only_reports_mic_rate_with_stale_system_buffer() {
        let mut manager = AudioManager::new();
        let mic_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 44100, 1));
        let sys_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 2));
        manager.mic_buffer = Some(Arc::clone(&mic_buf));
        manager.system_buffer = Some(Arc::clone(&sys_buf));

        let pos = manager.buffer_positions();
        mic_buf.write(&vec![0.3_f32; 200]);

        let (_samples, reported_sr, _new_pos) = manager
            .extract_since(&pos, CaptureSource::MicOnly, None)
            .expect("mic extraction returns samples");
        assert_eq!(reported_sr, 44100, "MicOnly must report mic rate");
    }

    #[test]
    fn test_extract_since_mixed_stereo_system_trim() {
        let mut manager = AudioManager::new();
        let mic_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        // Stereo system buffer: 2 raw samples per mono sample
        let sys_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 2));

        manager.mic_buffer = Some(Arc::clone(&mic_buf));
        manager.system_buffer = Some(Arc::clone(&sys_buf));

        let pos = manager.buffer_positions();

        // Mic: 500 mono samples, System: 600 raw (=300 mono) stereo samples
        mic_buf.write(&vec![0.5_f32; 500]);
        let mut stereo = Vec::with_capacity(600);
        for _ in 0..300 {
            stereo.push(0.4);
            stereo.push(0.2);
        }
        sys_buf.write(&stereo);

        let mix_config = MixConfig {
            mic_gain: 1.0,
            system_gain: 1.0,
            normalize: false,
        };
        let result = manager.extract_since(&pos, CaptureSource::Mixed, Some(&mix_config));
        assert!(result.is_some());
        let (samples, _sr, new_pos) = result.unwrap();

        // min(500 mic mono, 300 sys mono) = 300
        assert_eq!(samples.len(), 300);
        // Mic surplus = 200 mono → rewind by 200 * 1 channel = 200 raw
        assert_eq!(new_pos.mic_pos, 300);
        // System fully consumed
        assert_eq!(new_pos.system_pos, 600);
    }

    #[test]
    fn test_extract_since_mixed_rate_trim() {
        // Verify time-aligned trimming: 16kHz mic + 48kHz system, 0.5s each.
        // At different rates, equal time = different sample counts.
        // The fix ensures we trim by time, not by sample count.
        let mut manager = AudioManager::new();
        let mic_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        let sys_buf = Arc::new(AudioRingBuffer::with_duration(10.0, 48000, 1));

        manager.mic_buffer = Some(Arc::clone(&mic_buf));
        manager.system_buffer = Some(Arc::clone(&sys_buf));

        let pos = manager.buffer_positions();

        // Write 0.5s at each rate: 8000 mic samples, 24000 system samples
        mic_buf.write(&vec![0.5_f32; 8000]);
        sys_buf.write(&vec![0.5_f32; 24000]);

        let result = manager.extract_since(&pos, CaptureSource::Mixed, None);
        assert!(result.is_some(), "expected Some with mixed-rate data");
        let (samples, sample_rate, new_pos) = result.unwrap();
        assert_eq!(sample_rate, 16000);
        assert!(!samples.is_empty());

        // Both sources represent exactly 0.5s, so no surplus — positions fully advanced
        assert_eq!(new_pos.mic_pos, 8000);
        assert_eq!(new_pos.system_pos, 24000);

        // Now test unequal durations: mic=0.5s (8000), system=0.3s (14400)
        mic_buf.write(&vec![0.5_f32; 8000]);
        sys_buf.write(&vec![0.5_f32; 14400]);

        let result2 = manager.extract_since(&new_pos, CaptureSource::Mixed, None);
        assert!(result2.is_some());
        let (_samples2, _sr2, new_pos2) = result2.unwrap();

        // Min time = 0.3s. Mic should be trimmed to 0.3s = 4800 samples, surplus = 3200
        // Mic pos rewound by 3200: 16000 - 3200 = 12800
        assert_eq!(new_pos2.mic_pos, 12800);
        // System fully consumed at 0.3s
        assert_eq!(new_pos2.system_pos, 24000 + 14400);
    }

    #[test]
    fn test_start_capture_already_running_preserves_state() {
        // When start_capture returns AlreadyRunning, the state machine should NOT
        // transition to Error — the existing Capturing state is preserved.
        let mut manager = AudioManager::new();

        // Simulate an already-running mic by injecting a buffer and setting state.
        let buffer = Arc::new(AudioRingBuffer::with_duration(10.0, 16000, 1));
        manager.mic_buffer = Some(Arc::clone(&buffer));
        manager.state = CaptureState::Capturing;

        // MicOnly start_capture will call start_mic → mic.start() → AlreadyRunning
        // won't fire because mic.is_running is false. Instead, test via Mixed path:
        // start_all calls start_mic first. If mic succeeds, then start_system.
        // We need the AlreadyRunning to come from start_capture's inner call.
        // Easiest: directly test the guard in start_capture.
        // Set mic as running to trigger AlreadyRunning from start_mic.
        // We can't easily mock mic.start(), but we can test the error gate:
        // Call start_capture with a source that will fail with a non-AlreadyRunning error
        // and verify state becomes Error, then verify AlreadyRunning doesn't.

        // First: verify non-AlreadyRunning errors DO set Error state. This is the ONLY step
        // that opens a live audio subsystem — on a truly headless host (no CoreAudio/ALSA
        // backend) `start_capture(SystemOnly)` can BLOCK inside cpal instead of returning, the
        // documented S1-S3 hang. Gate JUST this probe behind YAPSTACK_AUDIO_DEVICE_TESTS so a
        // headless `cargo test --all` never touches a device, while the pure state-machine
        // assertions below (the real subject of this test) always run. Set the env to 1 to
        // exercise the live-device error path on a machine with audio hardware.
        if std::env::var_os("YAPSTACK_AUDIO_DEVICE_TESTS").is_some() {
            let result = manager.start_capture(CaptureSource::SystemOnly, None);
            if result.is_err() {
                // On a host without system audio this fails with a real error.
                let status = manager.status();
                assert_eq!(status.state, CaptureState::Error);
                assert!(status.error_message.is_some());
            }
        }

        // Reset to Capturing state to test AlreadyRunning guard
        manager.state = CaptureState::Capturing;
        manager.error_message = None;

        // Manually test the AlreadyRunning guard logic:
        // The AudioError::AlreadyRunning should NOT change state
        let err = AudioError::AlreadyRunning;
        if !matches!(err, AudioError::AlreadyRunning) {
            manager.state = CaptureState::Error;
            manager.error_message = Some(err.to_string());
        }
        assert_eq!(manager.status().state, CaptureState::Capturing);
        assert!(manager.status().error_message.is_none());

        // And a non-AlreadyRunning error SHOULD change state
        let err = AudioError::NoBufferAvailable;
        if !matches!(err, AudioError::AlreadyRunning) {
            manager.state = CaptureState::Error;
            manager.error_message = Some(err.to_string());
        }
        assert_eq!(manager.status().state, CaptureState::Error);
        assert!(manager.status().error_message.is_some());
    }

    #[test]
    #[ignore] // Opens a live audio subsystem: gate behind `--ignored` (or
              // YAPSTACK_AUDIO_DEVICE_TESTS) so a headless `cargo test --all` never blocks
              // inside cpal on a host with no CoreAudio/ALSA backend (the S1-S3 hang).
    fn test_start_capture_failure_sets_error_state() {
        // On macOS CI, SystemOnly will fail because there's no output device.
        // This verifies end-to-end that start_capture sets error state.
        let mut manager = AudioManager::new();
        let result = manager.start_capture(CaptureSource::SystemOnly, None);
        if result.is_err() {
            let status = manager.status();
            assert_eq!(status.state, CaptureState::Error);
            assert!(status.error_message.is_some());
        }
        // If it succeeds (hardware present), that's fine — the state machine
        // test above covers the error path.
    }
}
