//! Always-on device-change broker.
//!
//! Owns the receiving end of the audio crate's runtime-agnostic
//! [`DeviceEventSink`]. Lives in the Tauri layer (not the audio crate)
//! because it needs `tokio` and an [`AppHandle`] to emit events to the
//! frontend — the audio crate is intentionally synchronous and
//! runtime-agnostic.
//!
//! Spawned once during Tauri `setup()` and runs for the app's lifetime.
//! Reacts to:
//!
//! * `kAudioHardwarePropertyDevices` (hardware add/remove) →
//!   re-enumerate, emit `devices-changed`.
//! * Default-input / default-output / default-system-output changes →
//!   re-enumerate (so `is_default` flags refresh) and emit
//!   `devices-changed`. Per-kind events are not surfaced — the
//!   re-emitted device list is the single FE-facing signal.
//!
//! Events arriving in a 250 ms window are coalesced — a bluetooth
//! handshake typically fires `Devices`, then `DefaultOutput`, then
//! sometimes `DefaultSystemOutput` in rapid succession; the broker
//! emits one consolidated set of FE events at the trailing edge and
//! dispatches at most one restart per Source per debounce window.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use yapstack_audio::system::device_watcher::{DeviceEvent, DeviceEventSink};

use crate::commands::audio::AudioDeviceInfoDto;
use crate::commands::audio::AudioManagerState;
use crate::commands::live_transcription::{LiveSessionPresent, RestartIntent, RestartIntentInbox};

/// Coalescing window for bursty Core Audio listener events. 250 ms is
/// short enough that the FE sees device changes almost instantly and
/// long enough to absorb the typical bluetooth-handshake event burst
/// (Devices → DefaultOutput → DefaultSystemOutput within ~50 ms).
const DEBOUNCE_MS: u64 = 250;

/// Spawn the device-change broker on the Tauri async runtime. Returns
/// immediately; the broker runs until the shutdown signal flips to
/// `true` or the audio manager drops its watchers.
pub fn spawn(
    app_handle: AppHandle,
    audio_state: AudioManagerState,
    restart_intent_inbox: RestartIntentInbox,
    live_session_present: LiveSessionPresent,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    tauri::async_runtime::spawn(async move {
        let (tx, mut rx) = mpsc::unbounded_channel::<DeviceEvent>();

        // Build the sink. The closure runs on the Core Audio listener
        // thread; it must not block or .await, so it only forwards into
        // the unbounded channel and returns.
        let sink_tx = tx.clone();
        let sink: DeviceEventSink = Arc::new(move |event| {
            let _ = sink_tx.send(event);
        });
        {
            let manager = audio_state.lock().await;
            manager.subscribe_device_events(Some(sink));
        }
        drop(tx);

        info!("device broker: started");

        loop {
            tokio::select! {
                maybe_event = rx.recv() => match maybe_event {
                    Some(first_event) => {
                        debug!("device broker: listener fired ({:?}), opening debounce window", first_event);
                        let mut state = CollapsedEvents::default();
                        state.merge(first_event);

                        // Drain any events that arrive within the
                        // debounce window before deciding what to emit.
                        let deadline = Instant::now() + Duration::from_millis(DEBOUNCE_MS);
                        loop {
                            match tokio::time::timeout_at(deadline, rx.recv()).await {
                                Ok(Some(next)) => state.merge(next),
                                Ok(None) => {
                                    // Sink dropped — handled by the outer
                                    // loop; flush what we have first.
                                    debug!("device broker: channel closed mid-debounce");
                                    break;
                                }
                                Err(_) => break, // window elapsed
                            }
                        }

                        info!(
                            "device broker: debounce flush — \
                             device_list={}, default_input={}, default_output={}, default_system_output={}",
                            state.device_list,
                            state.default_input,
                            state.default_output,
                            state.default_system_output,
                        );
                        flush(
                            &app_handle,
                            &audio_state,
                            &restart_intent_inbox,
                            &live_session_present,
                            state,
                        )
                        .await;
                    }
                    None => {
                        info!("device broker: sink detached, exiting");
                        break;
                    }
                },
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        let manager = audio_state.lock().await;
                        manager.subscribe_device_events(None);
                        info!("device broker: shutdown signal received, exiting");
                        break;
                    }
                }
            }
        }
    });
}

/// Tracks which kinds of events fired during a debounce window. Booleans
/// rather than counts because emitting the same FE event twice for the
/// same window adds no signal — the FE re-reads OS state on receipt.
#[derive(Default, Debug)]
struct CollapsedEvents {
    device_list: bool,
    default_input: bool,
    default_output: bool,
    default_system_output: bool,
}

impl CollapsedEvents {
    fn merge(&mut self, event: DeviceEvent) {
        match event {
            DeviceEvent::DeviceListChanged => self.device_list = true,
            DeviceEvent::DefaultInputChanged => self.default_input = true,
            DeviceEvent::DefaultOutputChanged => self.default_output = true,
            DeviceEvent::DefaultSystemOutputChanged => self.default_system_output = true,
        }
    }
}

async fn flush(
    app_handle: &AppHandle,
    audio_state: &AudioManagerState,
    inbox: &RestartIntentInbox,
    live_session_present: &LiveSessionPresent,
    state: CollapsedEvents,
) {
    // FE event emission. Re-enumerate the device list whenever any
    // kind fired, not only on `DeviceListChanged`: a default-only
    // change updates `is_default` flags on the existing dtos, and we
    // need the FE store to see the new flags. (Cheap on macOS: a
    // single Core Audio enumeration round-trip.) Per-kind events are
    // intentionally not surfaced — there's no FE consumer and the
    // re-emitted device list carries everything the UI needs.
    let any_default_changed =
        state.default_input || state.default_output || state.default_system_output;
    if state.device_list || any_default_changed {
        emit_devices_changed(app_handle);
    }

    // Restart dispatch — Output and SystemOutput coalesce into one
    // System restart. Each dispatch is `IsAlive`-gated to absorb the
    // AirPods/Bluetooth revert window.
    if state.default_input {
        dispatch_mic_restart(audio_state, inbox, live_session_present).await;
    }
    if state.default_output || state.default_system_output {
        dispatch_system_restart(audio_state, inbox, live_session_present).await;
    }
}

/// Resolve the current default-input UID, gate on `device_liveness`
/// (one re-check at +250 ms if the device isn't yet alive), then route
/// the restart through the live-transcription inbox if a session is
/// running, or call `AudioManager::restart_mic` directly if a non-live
/// Capture is active.
///
/// Skips entirely when the user explicitly picked a non-default Mic
/// that is still alive — `DefaultInputChanged` only matters for that
/// user when their device disappears; that case arrives as a
/// `DeviceListChanged` instead and is handled by FE reconciliation
/// resetting `selectedMicDeviceId` to follow-default. No-op when no
/// Capture is active.
async fn dispatch_mic_restart(
    audio_state: &AudioManagerState,
    inbox: &RestartIntentInbox,
    live_session_present: &LiveSessionPresent,
) {
    // Snapshot bound state under the lock; release before doing any
    // sleep/gate work so we don't block other async actors.
    let (bound_is_default, bound_uid, mic_active) = {
        let manager = audio_state.lock().await;
        let status = manager.status();
        (
            manager.mic_bound_is_default(),
            manager.mic_device_id().map(|s| s.to_string()),
            status.mic_active,
        )
    };

    if !bound_is_default {
        // Explicit pick — strict policy. Honor the pick *only* when we
        // can confirm Alive. `Absent` (genuinely unplugged), `Dead`, and
        // `Unknown` (couldn't tell) all fall through to failover, since
        // it is safer to bind to the new default than to silently keep
        // a stale binding when an explicit pick may have disappeared.
        // See `DeviceLiveness` docs in the audio crate for the policy
        // split between this call site and `alive_gate`.
        let liveness = bound_uid
            .as_deref()
            .map(strip_cpal_prefix)
            .map(yapstack_audio::device::device_liveness)
            .unwrap_or(yapstack_audio::device::DeviceLiveness::Unknown);
        if liveness == yapstack_audio::device::DeviceLiveness::Alive {
            info!(
                "device broker: explicit Mic ({:?}) still alive — ignoring DefaultInputChanged",
                bound_uid
            );
            return;
        }
        info!(
            "device broker: explicit Mic ({:?}) liveness={:?} — falling over to system default",
            bound_uid, liveness
        );
    }

    let new_default = yapstack_audio::device::default_input_device().ok();
    let default_uid = new_default.as_ref().and_then(|info| info.id.clone());
    let default_name = new_default.as_ref().map(|info| info.name.clone());
    alive_gate(default_uid.as_deref()).await;

    let sent = try_send_intent(inbox, RestartIntent::Mic);
    let decision = decide_routing(
        sent,
        live_session_present.load(Ordering::Acquire),
        mic_active,
    );
    match decision {
        RoutingDecision::SentThroughInbox => {
            info!(
                "device broker: routing Mic failover to live loop (from={:?} → to={:?})",
                bound_uid, default_name
            );
            return;
        }
        RoutingDecision::SkipMidStop => {
            debug!(
                "device broker: live session in teardown — skipping direct Mic restart \
                 (event arrived after inbox close, before loop exit)"
            );
            return;
        }
        RoutingDecision::SkipNoActiveCapture => {
            debug!("device broker: no live loop and Mic not active — skipping restart");
            return;
        }
        RoutingDecision::DirectRestart => {}
    }

    let mut manager = audio_state.lock().await;
    match manager.restart_mic(yapstack_audio::manager::RestartTarget::FollowDefault) {
        Ok(report) => {
            if report.same_device {
                warn!(
                    "device broker: direct Mic restart re-bound to the same device ({:?}) — \
                     OS may still be settling; retry on next event",
                    report.new_device_id
                );
            } else {
                info!(
                    "device broker: direct Mic restart succeeded ({:?}, bound={:?})",
                    report.outcome, report.bound_device_name
                );
            }
        }
        Err(e) => warn!("device broker: direct Mic restart failed: {}", e),
    }
}

async fn dispatch_system_restart(
    audio_state: &AudioManagerState,
    inbox: &RestartIntentInbox,
    live_session_present: &LiveSessionPresent,
) {
    let new_default = yapstack_audio::device::default_output_device().ok();
    let default_uid = new_default.as_ref().and_then(|info| info.id.clone());
    let default_name = new_default.as_ref().map(|info| info.name.clone());
    let (bound_name, system_active) = {
        let manager = audio_state.lock().await;
        (
            manager.system_audio_bound_device().map(|s| s.to_string()),
            manager.status().system_audio_active,
        )
    };
    alive_gate(default_uid.as_deref()).await;

    let sent = try_send_intent(inbox, RestartIntent::System);
    let decision = decide_routing(
        sent,
        live_session_present.load(Ordering::Acquire),
        system_active,
    );
    match decision {
        RoutingDecision::SentThroughInbox => {
            info!(
                "device broker: routing System failover to live loop (from={:?} → to={:?})",
                bound_name, default_name
            );
            return;
        }
        RoutingDecision::SkipMidStop => {
            debug!("device broker: live session in teardown — skipping direct System restart");
            return;
        }
        RoutingDecision::SkipNoActiveCapture => {
            debug!("device broker: no live loop and System audio not active — skipping restart");
            return;
        }
        RoutingDecision::DirectRestart => {}
    }

    let mut manager = audio_state.lock().await;
    match manager.restart_system_audio() {
        Ok(report) => {
            if report.same_device {
                warn!(
                    "device broker: direct System restart re-bound to the same device ({:?}) — \
                     OS may still be settling",
                    report.new_device_id
                );
            } else {
                info!(
                    "device broker: direct System restart succeeded ({:?}, bound={:?})",
                    report.outcome, report.bound_device_name
                );
            }
        }
        Err(e) => warn!("device broker: direct System restart failed: {}", e),
    }
}

/// Outcome of `decide_routing` — the four mutually exclusive choices
/// the broker can make once it has tried the live-loop inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoutingDecision {
    /// Intent was queued through the inbox; the live loop will handle it.
    SentThroughInbox,
    /// Inbox is empty but a live session is mid-stop. Skip — a direct
    /// restart would race the loop's snapshotted final-flush positions.
    SkipMidStop,
    /// Inbox empty, no live session, and the relevant capture stream
    /// is not active either. Nothing to restart.
    SkipNoActiveCapture,
    /// Inbox empty, no live session, capture is active. Direct restart
    /// via `AudioManager::restart_*` is safe.
    DirectRestart,
}

/// Pure routing decision used by `dispatch_mic_restart` /
/// `dispatch_system_restart`. Extracted so the four-way branching is
/// unit-testable without a real `AudioManager` or `AppHandle`.
fn decide_routing(
    intent_sent: bool,
    live_session_present: bool,
    capture_active: bool,
) -> RoutingDecision {
    if intent_sent {
        return RoutingDecision::SentThroughInbox;
    }
    if live_session_present {
        // See `LiveSessionPresent` docs: stop_live_transcription clears
        // the inbox before the loop's final flush, so an empty inbox
        // with the flag still set means we're inside the teardown
        // window and a direct restart would replace the ring buffer
        // mid-finalize.
        return RoutingDecision::SkipMidStop;
    }
    if !capture_active {
        return RoutingDecision::SkipNoActiveCapture;
    }
    RoutingDecision::DirectRestart
}

/// Best-effort send into the live-loop's restart-intent inbox. Returns
/// `true` if a sender was present and the send queued; `false` if no
/// session is running or the receiver has been dropped (caller routes
/// the decision through `decide_routing`).
fn try_send_intent(inbox: &RestartIntentInbox, intent: RestartIntent) -> bool {
    let guard = match inbox.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            warn!("device broker: restart-intent inbox poisoned — recovering");
            poisoned.into_inner()
        }
    };
    match guard.as_ref() {
        Some(sender) => sender.send(intent).is_ok(),
        None => false,
    }
}

/// Strip cpal's `DeviceId` host prefix (`"coreaudio:"` on macOS,
/// `"wasapi:"` on Windows) so the bare device identifier — a
/// `kAudioDevicePropertyDeviceUID` string or a WASAPI endpoint ID — can
/// be passed to `yapstack_audio::device::device_liveness`. cpal's
/// `HostId::Display` lowercases the host name (see
/// `cpal/src/platform/mod.rs`), so the real prefixes are lowercase; the
/// CamelCase arms are belt-and-braces in case cpal ever changes the
/// formatter.
fn strip_cpal_prefix(uid: &str) -> &str {
    for prefix in ["coreaudio:", "CoreAudio:", "wasapi:", "WASAPI:"] {
        if let Some(rest) = uid.strip_prefix(prefix) {
            return rest;
        }
    }
    uid
}

/// Bluetooth/AirPods absorbs the default-device change before the route
/// is fully alive. Lenient policy: `Alive` and `Unknown` (couldn't tell)
/// proceed immediately — a genuine "couldn't tell" must not stall a
/// restart that the actual cpal `start` will validate anyway. `Dead` and
/// `Absent` get one more debounce window to settle, then fall through;
/// the restart attempt itself surfaces a real failure via `stream-health`
/// if it doesn't recover.
async fn alive_gate(uid: Option<&str>) {
    use yapstack_audio::device::DeviceLiveness;

    let Some(uid) = uid else {
        return;
    };
    let bare = strip_cpal_prefix(uid);
    match yapstack_audio::device::device_liveness(bare) {
        DeviceLiveness::Alive | DeviceLiveness::Unknown => return,
        DeviceLiveness::Dead | DeviceLiveness::Absent => {}
    }
    debug!(
        "device broker: target {} not yet alive — waiting {}ms before restart",
        bare, DEBOUNCE_MS
    );
    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
}

fn emit_devices_changed(app_handle: &AppHandle) {
    let inputs = match yapstack_audio::device::list_input_devices() {
        Ok(v) => v,
        Err(e) => {
            warn!("device broker: list_input_devices failed: {}", e);
            return;
        }
    };
    let outputs = match yapstack_audio::device::list_output_devices() {
        Ok(v) => v,
        Err(e) => {
            warn!("device broker: list_output_devices failed: {}", e);
            return;
        }
    };
    let input_summary: Vec<String> = inputs
        .iter()
        .map(|d| format!("{}{}", d.name, if d.is_default { " (default)" } else { "" }))
        .collect();
    let output_summary: Vec<String> = outputs
        .iter()
        .map(|d| format!("{}{}", d.name, if d.is_default { " (default)" } else { "" }))
        .collect();
    info!(
        "device broker: devices-changed — inputs=[{}], outputs=[{}]",
        input_summary.join(", "),
        output_summary.join(", ")
    );
    let mut all = inputs;
    all.extend(outputs);
    let payload: Vec<AudioDeviceInfoDto> = all.into_iter().map(AudioDeviceInfoDto::from).collect();
    if let Err(e) = app_handle.emit("devices-changed", payload) {
        warn!("device broker: emit devices-changed failed: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn collapsed_events_default_all_false() {
        let s = CollapsedEvents::default();
        assert!(!s.device_list);
        assert!(!s.default_input);
        assert!(!s.default_output);
        assert!(!s.default_system_output);
    }

    #[test]
    fn collapsed_events_merge_sets_each_kind() {
        let mut s = CollapsedEvents::default();
        s.merge(DeviceEvent::DeviceListChanged);
        s.merge(DeviceEvent::DefaultInputChanged);
        s.merge(DeviceEvent::DefaultOutputChanged);
        s.merge(DeviceEvent::DefaultSystemOutputChanged);
        assert!(s.device_list);
        assert!(s.default_input);
        assert!(s.default_output);
        assert!(s.default_system_output);
    }

    #[test]
    fn collapsed_events_merge_is_idempotent_per_kind() {
        // Repeated events of the same kind during the debounce window
        // collapse to a single emit pass.
        let mut s = CollapsedEvents::default();
        s.merge(DeviceEvent::DefaultInputChanged);
        s.merge(DeviceEvent::DefaultInputChanged);
        s.merge(DeviceEvent::DefaultInputChanged);
        assert!(s.default_input);
        assert!(!s.default_output);
        assert!(!s.default_system_output);
        assert!(!s.device_list);
    }

    #[test]
    fn collapsed_events_merge_preserves_independent_kinds() {
        let mut s = CollapsedEvents::default();
        s.merge(DeviceEvent::DefaultOutputChanged);
        s.merge(DeviceEvent::DeviceListChanged);
        // Output and DeviceList both fired; the other two stayed clean.
        assert!(s.default_output);
        assert!(s.device_list);
        assert!(!s.default_input);
        assert!(!s.default_system_output);
    }

    #[test]
    fn strip_cpal_prefix_removes_lowercase_prefix() {
        // cpal's actual `HostId::Display` lowercases the host name, so
        // the prefix is "coreaudio:" — this is the path real device
        // ids take. Regression test for the bug where the helper used
        // CamelCase and silently no-op'd every macOS UID.
        assert_eq!(strip_cpal_prefix("coreaudio:BuiltInMic"), "BuiltInMic");
        assert_eq!(
            strip_cpal_prefix("coreaudio:com.apple.audio.SystemMicrophone"),
            "com.apple.audio.SystemMicrophone"
        );
    }

    #[test]
    fn strip_cpal_prefix_falls_back_on_camelcase() {
        // Belt-and-braces in case cpal ever changes its formatter back
        // to CamelCase or a hand-built id uses it.
        assert_eq!(strip_cpal_prefix("CoreAudio:BuiltInMic"), "BuiltInMic");
    }

    #[test]
    fn strip_cpal_prefix_leaves_other_strings_alone() {
        // Non-cpal-format strings (raw UID, empty, foreign prefix) must
        // pass through unchanged so callers can pipe them straight into
        // device_liveness's lookup path.
        assert_eq!(strip_cpal_prefix("BuiltInMic"), "BuiltInMic");
        assert_eq!(strip_cpal_prefix(""), "");
        assert_eq!(
            strip_cpal_prefix("wasapi:{0.0.1.00000000}.{72cf7635-4bbf-45bc-a195-b8d916da4aac}"),
            "{0.0.1.00000000}.{72cf7635-4bbf-45bc-a195-b8d916da4aac}"
        );
    }

    #[test]
    fn try_send_intent_returns_false_for_empty_inbox() {
        let inbox: RestartIntentInbox = Arc::new(StdMutex::new(None));
        assert!(!try_send_intent(&inbox, RestartIntent::Mic));
        assert!(!try_send_intent(&inbox, RestartIntent::System));
    }

    #[test]
    fn try_send_intent_routes_to_populated_inbox() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RestartIntent>();
        let inbox: RestartIntentInbox = Arc::new(StdMutex::new(Some(tx)));

        assert!(try_send_intent(&inbox, RestartIntent::Mic));
        assert!(try_send_intent(&inbox, RestartIntent::System));

        // Both intents arrived in order.
        let first = rx.try_recv().expect("first intent buffered");
        let second = rx.try_recv().expect("second intent buffered");
        assert!(matches!(first, RestartIntent::Mic));
        assert!(matches!(second, RestartIntent::System));
    }

    #[test]
    fn decide_routing_sends_through_inbox_when_intent_queued() {
        // The inbox accepted the intent — that's authoritative; the
        // other inputs are ignored.
        assert_eq!(
            decide_routing(true, true, true),
            RoutingDecision::SentThroughInbox
        );
        assert_eq!(
            decide_routing(true, false, false),
            RoutingDecision::SentThroughInbox
        );
    }

    #[test]
    fn decide_routing_skips_when_live_session_in_teardown() {
        // Inbox empty (`stop_live_transcription` cleared it) but the
        // spawned task hasn't cleared the presence flag yet — the loop
        // is still running its final flush. Direct-restart here would
        // race the snapshotted stop positions, so skip. This is the
        // P2 ultrareview fix.
        assert_eq!(
            decide_routing(false, true, true),
            RoutingDecision::SkipMidStop
        );
        assert_eq!(
            decide_routing(false, true, false),
            RoutingDecision::SkipMidStop
        );
    }

    #[test]
    fn decide_routing_skips_when_no_active_capture() {
        // No live session, no capture stream — nothing for a restart
        // to act on.
        assert_eq!(
            decide_routing(false, false, false),
            RoutingDecision::SkipNoActiveCapture
        );
    }

    #[test]
    fn decide_routing_direct_restart_when_idle_capture_active() {
        // No live loop owns audio state and the capture stream is
        // active (e.g. legacy `start_capture` without
        // `start_live_transcription`). Direct restart is safe.
        assert_eq!(
            decide_routing(false, false, true),
            RoutingDecision::DirectRestart
        );
    }

    #[test]
    fn try_send_intent_returns_false_when_receiver_was_dropped() {
        // The live loop ended (receiver dropped) but the inbox still
        // holds a stale sender. send() returns Err, broker falls back
        // to direct restart.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<RestartIntent>();
        drop(rx);
        let inbox: RestartIntentInbox = Arc::new(StdMutex::new(Some(tx)));

        assert!(!try_send_intent(&inbox, RestartIntent::Mic));
    }
}
