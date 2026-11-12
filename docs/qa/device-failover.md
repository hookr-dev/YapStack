# Manual QA: device-change auto-failover

Hardware-driven scenarios that the unit-test harness can't cover. Run on macOS (Apple silicon, current OS) before shipping a release that touches the audio capture path.

Prerequisites:
- Bluetooth headset (AirPods or equivalent) paired and connectable.
- USB microphone available for hot-plug.
- Built-in mic + speakers usable as the fallback default.

> **Domain language**: terms like Capture, Mic, System audio, Source, Stream, Stream restart, Auto-failover, and Device broker are defined in [docs/UBIQUITOUS_LANGUAGE.md](../UBIQUITOUS_LANGUAGE.md).

---

## 1. AirPods mid-session drop (Mic, follow-default)

**Setup.** Pair AirPods. In System Settings → Sound, set them as the default input. In YapStack settings, leave the mic selection on "follow system default" (no explicit pick). Start a Capture and let it run for ~5 seconds.

**Action.** Power off the AirPods (or take them out and let macOS disconnect them).

**Expect.**
- Capture continues without complete loss of audio. A brief gap (≤500 ms) is acceptable.
- A single transient toast appears: "Switched mic to {built-in mic name}".
- Live transcription segments resume within a few seconds.
- No `restart_abandoned` toast.

## 2. USB mic mid-session unplug (Mic, follow-default)

**Setup.** Plug a USB mic. Confirm it became the system default input. Start a Capture.

**Action.** Unplug the USB mic.

**Expect.**
- Auto-failover toast names the new default (typically "MacBook Pro Microphone").
- Capture continues; segments keep flowing.

## 3. System Settings default-input change mid-session

**Setup.** With at least two input devices available, start a Capture (still on follow-default).

**Action.** Open System Settings → Sound → Input and pick a different device.

**Expect.**
- Toast names the device the user just picked.
- No restart loop / no spurious second toast within a second of the first.

## 4. Idle hot-plug visibility

**Setup.** Open YapStack's audio settings panel (or Onboarding's audio step) without any active Capture.

**Action.** Plug in a USB mic that wasn't connected when the app launched.

**Expect.**
- The device list reflects the new device within ~250 ms (broker's debounce window).
- No app restart needed. No manual refresh button needed.

## 5. System audio output change mid-session

**Setup.** Start Capture in `Mixed` or `SystemOnly` mode with the built-in speakers as default output.

**Action.** In Control Center (or System Settings → Sound → Output), switch the default output (e.g. to AirPods, then back).

**Expect.**
- A toast names the new system audio device on each change.
- The system-audio loopback Stream rebinds to the new output. System-audio segments resume after the brief gap.

## 6. Mixed-source unrecoverable failure (fail-fast)

**Setup.** Plug a USB mic. Start a `Mixed` Capture using that mic. Confirm both Mic and System audio are flowing.

**Action.** Force the Mic restart attempts to exhaust. Only restarts that fail, or watchdog restarts that rebind the same device while its stream stays dead, count toward `STREAM_RESTART_MAX_ATTEMPTS` (3). A device-change restart that lands on the device already in use counts as a success, so rapid unplug/replug no longer exhausts the budget. Reproduce with a Mic whose restarts fail (each `restart failed (attempt N)` log line counts) or whose stream keeps dying on the same device after the watchdog rebinds it (each `rebound to the same device — stream may still be dead` line counts), until the log shows attempt 3/3.

**Expect.**
- A `restart_abandoned` error toast appears for the Mic.
- Both Sources stop. The session ends through the normal stop path (segments drained, WAV finalized, Stopped phase emitted).
- No "limping along on system audio only" state.

## 7. Explicit non-default mic preserved when still alive

**Setup.** In settings, explicitly pick a non-default mic (e.g. the built-in mic when AirPods are the default). Start a Capture.

**Action.** Change the system default input via System Settings (without disconnecting the user's chosen mic).

**Expect.**
- **No** Auto-failover toast. The user's pick still works, so the broker leaves it alone.
- Capture continues uninterrupted on the explicitly chosen device.

## 8. Explicit pick disappears (force failover)

**Setup.** Plug a USB mic, explicitly pick it in settings, start a Capture.

**Action.** Unplug the USB mic.

**Expect.**
- The persisted `selectedMicDeviceId` is reconciled to "follow default" (visible in settings: the picker shows the new default).
- A "Selected microphone disappeared — using {default name}" toast appears (id `mic-disappeared`).
- The Capture rebinds to the new default and continues.

## 9. AirPods reconnect during preflight

**Setup.** Connect AirPods, set as default input, then disconnect — wait for macOS to fall back to the built-in. Start a new session immediately.

**Expect.**
- Preflight does not trigger a default-device-change restart (that path is gone). It only catches stream errors and write-pos stalls.
- The session starts cleanly on whichever device is currently default.

## 10. Thunderbolt audio interface connect (real-world failover regression)

**Setup.** Boot YapStack on the built-in mic. Start a Capture (any source).

**Action.** Plug the laptop into a Thunderbolt dock with audio capability (or a dock that brings a USB audio interface online). Wait a few seconds for macOS to enumerate it and update the default input.

**Expect.**
- A `device broker: debounce flush — ... default_input=true ...` log line confirms the listener fired (visible at `info` level).
- A `routing Mic failover to live loop (from=... → to=...)` log line shows the resolved transition.
- Toast "Switched mic to {new device}" appears.
- Audio segments resume on the new device. The debug overlay's mic level meter responds to your voice.
- **Regression check:** before the `RestartTarget::FollowDefault` fix, the broker would re-bind to the *old* device because the old device was still alive (the laptop's built-in mic doesn't disappear when a dock appears). If the toast says "Switched to MacBook Pro Microphone" but the OS says the dock is now default, that fix has regressed.

## 11. cpal loopback aggregate must not appear in the picker

**Setup.** Start a `Mixed` or `SystemOnly` Capture so cpal allocates its private loopback aggregate device. Open Settings → Audio.

**Expect.**
- The Input Device dropdown does **not** show "Cpal loopback record aggregate device" or any entry whose id contains `com.cpal.LoopbackRecordAggregateDevice`.
- The Output Device list (if visible) likewise does not show it.
- No empty-named entries appear.

**Regression check.** Stop and restart Capture; cpal recreates the aggregate. Confirm it still doesn't surface in the picker.

## 12. Manual selection of an output device as a mic must error cleanly

**Setup.** Even after the fixes, defensively confirm that if a user somehow has a stale `selectedMicDeviceId` pointing at the loopback aggregate or any output-only device, capture refuses cleanly rather than crashing.

**Action.** In a debug build, set `useAppStore.getState().updateSettings({ selectedMicDeviceId: "coreaudio:com.cpal.LoopbackRecordAggregateDevice" })` from the devtools console, then start a Capture.

**Expect.**
- `start_capture` returns an error with a message that names the loopback aggregate as not selectable.
- The FE surfaces the error toast through the existing capture-error path.
- No process crash, no opaque "stream type not supported" message.

## 13. Concurrent device storm

**Setup.** Start a Capture. While it's running, perform: USB unplug → wait 1 s → AirPods connect → wait 1 s → AirPods disconnect.

**Expect.**
- The broker coalesces overlapping events within each 250 ms window. At most one toast per discrete user action.
- No `restart_abandoned` from over-eager retries.
- Capture is alive at the end on whichever device is current.

---

## Reading the broker logs

The most common failure mode is "the failover ran but bound to the wrong device." With the logging promotions, the diagnosis flow is:

1. `device broker: listener fired (...)` — `debug` level. Confirms the Core Audio property listener actually fired for that selector. Missing → the listener never registered (boot error?) or the OS didn't push the event.
2. `device broker: debounce flush — device_list=..., default_input=..., default_output=...` — `info` level. Confirms the broker observed the kinds you'd expect for the user action. AirPods drop → `default_input=true`; speaker swap → `default_output=true`.
3. `device broker: routing Mic failover to live loop (from=... → to=...)` — `info`. Names both endpoints of the failover. If `from == to`, the dispatch is moot (broker should have skipped); if `to == None`, the new default couldn't be resolved.
4. `stream health: Microphone needs restart (device-change, target=FollowDefault), attempt 1/3` — `warn`. The live loop received the intent and is invoking the restart with the right probe order.
5. `device broker: direct Mic restart re-bound to the same device (...)` — `warn`. Same-device rebind on broker-driven dispatch is a regression signal: the new default's id matches the old. Either the OS hasn't fully committed the change, or `RestartTarget::FollowDefault` isn't being honored.
6. `stream-health` event with `bound_device_name` — eventual outcome. The FE's "Switched to {name}" toast displays this.

If steps 1–3 fire but step 4 doesn't, the `RestartIntent` is being lost between broker and live loop — check that `RestartIntentInbox` is set (live transcription is running) and the broker's `try_send_intent` returned `true`.
