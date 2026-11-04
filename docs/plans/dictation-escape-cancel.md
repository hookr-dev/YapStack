> _Drafted by Claude during to-prd session._

# Dictation Escape-to-Cancel

**Status:** executed (7279d6f, documented in 6c1bbe2). The Escape hotkey is registered in `apps/desktop/src/hooks/useDictation.ts`; ARCHITECTURE.md describes the cancel flow.

## Problem Statement

A Dictation, once started, has no exit door. The user holds (or toggles) the Dictation slot's Global hotkey, the Dictation Bubble appears, and from that moment on the only path forward is to let it run to completion — capture, transcribe, run AI processing, deliver the Output action, and write a Dictation history entry. If midway through dictating the user changes their mind — they misspoke, picked the wrong slot, the wrong app is focused, the AI prompt is wrong, or they simply don't want this output anymore — they have to wait for the whole pipeline to finish and then manually undo the result (delete the pasted text, clear the clipboard, delete the new note, delete the Dictation history entry). Worst case: the Bubble is stuck in `transcribing` or `processing` for several seconds while the user watches a result they don't want being assembled.

This is true regardless of which phase the Dictation is in: `recording`, `transcribing`, `processing`, or the post-failure `error` display window. There is no panic button.

## Solution

Pressing **Escape** while a Dictation is in any open phase fully aborts that Dictation. The Capture stops, any in-flight transcription is abandoned, any in-flight AI request is aborted, no Output action runs (no paste, no clipboard write, no new note), no Dictation history entry is written, and the Bubble shows a brief "Cancelled" confirmation before closing.

Escape is registered as a Global hotkey for as long as the Dictation is non-idle, so it works whether the user is focused on the YapStack main window, the Bubble, or any other application — which is the realistic case, since dictation is most commonly used while typing into another app.

## User Stories

1. As a Dictation user, I want to press Escape while recording to throw away what I just said, so that I don't have to let it finish and then undo the result.
2. As a Dictation user, I want to press Escape while the Bubble shows `Transcribing`, so that I can abandon a Dictation whose audio is already captured but I no longer want.
3. As a Dictation user, I want to press Escape while the Bubble shows `Processing`, so that I can abandon a Dictation whose AI rewrite I no longer want without waiting for it to complete.
4. As a Dictation user, I want to press Escape while the Bubble shows `Failed`, so that I can dismiss the error immediately instead of waiting for it to time out.
5. As a Dictation user, I want Escape to work even when YapStack is not the focused application, so that I can cancel a Dictation while typing in another app — which is the normal case for `paste` and `clipboard` Output actions.
6. As a Dictation user, I want Escape to never paste, copy to clipboard, or create a new note, so that cancelling truly cancels — the Output action must not fire.
7. As a Dictation user, I want a cancelled Dictation to leave no trace in Dictation history, so that the sidebar list isn't polluted with abandoned attempts.
8. As a Dictation user, I want the Bubble to give me a brief visual confirmation that the Dictation was cancelled, so that I know the cancel landed and I'm not still recording silently.
9. As a Dictation user, I want Escape during a `hold` (push-to-talk) Dictation to cancel even if I'm still holding the slot's hotkey, so that I can release the hotkey afterwards without re-triggering anything.
10. As a Dictation user, I want Escape during a `toggle` Dictation to leave the toggle state cleared, so that the next press of the slot's hotkey starts a fresh Dictation.
11. As a Dictation user, I want Escape outside of a Dictation to do nothing Dictation-related, so that pressing Escape in unrelated contexts (closing a modal, blurring an input) keeps its normal behavior — Escape is only hijacked for the duration a Dictation is open.
12. As a Dictation user, I want the Capture stream to stop on cancel, so that the mic light and any system-audio loopback indicator clear immediately.
13. As a Dictation user, I want the streamed Session WAV for a cancelled Dictation to be cleaned up if it was being written, so that abandoned audio doesn't accumulate in `$APP_DATA_DIR/audio/`.
14. As a Dictation user, I want Escape never to crash the engine or leave the Sidecar in a bad state, so that my next Dictation works normally without restarting the app.
15. As a Dictation user, I want cancelled Dictations to be tracked in analytics distinctly from completions and failures, so that the project can see how often this exit path is used and refine the feature.
16. As a Dictation user, I want Escape during a Dictation to never mistakenly cancel an unrelated Live transcription session that happens to be running concurrently — the cancel is scoped to the active Dictation only.
17. As a developer, I want a single, testable cancel function on the Lifecycle Hook that reduces over the phase machine, so that I can reason about every transition rather than scattering cancel-handling across each phase.

## Implementation Decisions

### Modules to modify

- **Dictation Lifecycle Hook** — gains a single `cancel()` action that branches on the current phase and runs the correct teardown, then enters a new terminal `cancelled` phase. The phase machine grows from `idle | recording | transcribing | processing | done` to `idle | recording | transcribing | processing | cancelling | done`. The hook also gains an Escape listener whose registration mirrors the lifetime of "non-idle Dictation".
- **Global Shortcut Router** — gains a registration of Escape as a Global hotkey that fires only while a Dictation is non-idle. Registered on Dictation start, unregistered on Dictation idle. Dispatches a new `dictation-cancel` custom event on the same window-event channel as `dictation-start` / `dictation-stop`.
- **Dictation Bubble** — gains a `cancelled` `BubbleState` variant with a neutral grey ring and the label "Cancelled". Shown for ~800–1200 ms before the bubble hides, mirroring the existing `no-speech` / `error` self-hide pattern.
- **Event System** — adds the `dictation-cancel` window-event constant and the `cancelled` `BubbleState` to the typed event payload union.
- **Analytics** — adds `trackDictationCancelled({ slot_id, phase, duration_ms })` alongside the existing `trackDictationCompleted` and `trackDictationFailed` wrappers.

### Modules unchanged

- **Live Transcription Controller / Loop** — `stop_live_transcription` already drives a graceful loop teardown via the oneshot stop signal, and `stop_capture` already releases the cpal Streams. Cancel reuses both. No new Tauri command.
- **Transcription Client / Sidecar Process** — the Sidecar's currently-in-flight Chunk transcribe is allowed to complete; its result is discarded by the Lifecycle Hook (the segment listener is unmounted before the result lands, and the hook is in `cancelling` phase so it ignores any tail event). No new IPC verb to abort a transcribe mid-flight. This trade is acceptable because (a) Dictation Chunks are short (≤10 s by VAD config), (b) adding a per-request abort verb would require backend protocol churn, and (c) the engine's CPU/GPU cost during the dangling transcribe is bounded.
- **Audio Capture Manager** — no change. `stop_all` is the existing teardown.

### Cancel sequence (phase × what runs)

The cancel function drives the same five steps in order, no-oping any step that doesn't apply to the current phase:

1. Mark phase `cancelling` and emit `cancelled` to the Bubble.
2. Abort the AI `AbortController` if one is live (covers `processing`, and any `transcribing` window where AI is queued).
3. Tear down the segment / status / WAV listeners — discard any tail segments.
4. Call `stop_live_transcription` and (when it returns) `stop_capture`. Errors are swallowed; cancel must be infallible.
5. Skip `insertDictationHistory`. Skip the Output action. If a streamed Session WAV exists for the Dictation's synthetic session id, delete it.
6. After the Bubble's confirmation window, hide the Bubble, clear `dictationSessionId` in the store, dispatch `yapstack:dictation-idle` (so toggle-mode state clears), unregister Escape as a Global hotkey, and return phase to `idle`.

### Hotkey scope and conflicts

Escape is registered as a Global hotkey only when a Dictation transitions out of `idle` and unregistered when it transitions back to `idle` (success or cancel). When YapStack itself is not the focused app, the OS routes Escape to YapStack only because of this registration; while a Dictation is idle, Escape behaves as the OS / the active app would naturally handle it.

When YapStack's main window is focused and a Dictation is active, the Global hotkey takes precedence over any in-app Escape handler — modals, inputs, command palette — because cancelling the Dictation is the more urgent semantic. Listeners on Escape inside the YapStack window during an active Dictation are still allowed to fire by their own mechanism but should expect the Dictation to disappear.

### Race conditions

- **Cancel during the `startLiveTranscription` await** — there is already a "ghost transcription" guard at the end of `handleStart` that calls `stop_live_transcription` if the phase changed mid-await. Cancel reuses that guard: setting phase to `cancelling` while `start_live_transcription` is in-flight causes the guard to fire on resolution, which calls the same teardown.
- **Cancel during the `stop_live_transcription` await inside `handleStop`** — the `transcribing` phase is reachable mid-stop; cancel from there must abort the `Stopped`-event wait, abort the AI request that hasn't yet started, and skip the persistence/output blocks. Implemented by checking phase == `cancelling` after each `await` in the existing `handleStop` body, returning early when true.
- **Cancel during `done` self-hide** — Escape pressed during the 800–1200 ms post-success or post-error display window is a no-op for cancel purposes (nothing left to cancel) but should still hide the Bubble immediately for responsiveness. Treat as a fast hide, not a full cancel.
- **Cancel before the Bubble has rendered** — if `start_live_transcription` errored synchronously and the hook never showed the Bubble, cancel is a no-op.

### Streamed Session WAV cleanup

When `session_id` is set on the live transcription start payload (which Dictation always sets to a synthetic id), the backend writes a Session WAV to `$APP_DATA_DIR/audio/{session_id}.wav` and emits `session-wav-ready` on stop. On cancel, the Lifecycle Hook awaits one of: `session-wav-ready` (then deletes the file via the existing audio-file delete command surface) or a short timeout (500 ms). The synthetic session id is never inserted into the `sessions` table, so there is no DB row to clean up — only the WAV.

### Analytics event

`dictation_cancelled` with the slot id, the phase the Dictation was cancelled from (`recording` | `transcribing` | `processing` | `error`), and the elapsed milliseconds since Dictation start. Distinct from `dictation_failed`. Booleans (none expected here) follow the existing 0/1 convention.

## Testing Decisions

A good test for this feature exercises observable behaviour through the Lifecycle Hook's external surface — events in, store and command-client effects out — without reaching into the phase ref or internal timers. Tests should simulate dispatching `dictation-start`, advancing through phases by emitting the relevant Tauri events, then dispatching `dictation-cancel`, and assert that the right commands were called, the right effects were skipped, and the Bubble received the right state stream.

### Modules to test

- **Dictation Lifecycle Hook cancel branch** (vitest / React Testing Library or a hook harness). Phase matrix: cancel from `recording`, `transcribing`, `processing`, and `error`. Output action matrix: `paste`, `clipboard`, `new-note`. Per cell, assert (a) `clipboardPaste` was not called, (b) `dbCreateManualSession` / `saveNote` were not called, (c) `insertDictationHistory` was not called, (d) `stopLiveTranscription` and `stopCapture` were called, (e) the AI `AbortController` was aborted iff AI was in-flight, (f) the Bubble received `cancelled` then was hidden, (g) `yapstack:dictation-idle` fired.
- **Global Shortcut Router register/unregister lifecycle**. On `dictation-start`, Escape becomes a Global hotkey. On `dictation-idle` (whether reached by completion or cancel), Escape is unregistered. Two consecutive Dictations don't leak a duplicate registration.
- **Race-condition coverage** — cancel during the `start_live_transcription` await, and cancel during the `stop_live_transcription` await — both must reach `idle` without writing history or running the Output action.

### Prior art

- Vitest specs in `apps/desktop/src/` covering hooks and store slices are the closest existing pattern. Mock the Tauri command client and the event listener primitives via the existing `vi.mock` setup in those tests.
- The Lifecycle Hook's existing "ghost transcription" guard demonstrates the pattern of reading `stateRef.current` after each `await` to early-exit; the cancel implementation should test the same reads.
- The Live Transcription Loop's own Rust-side stop test (cargo test in `yapstack-desktop`) is *not* re-tested here; we trust the existing graceful stop path. The cancel feature is purely a frontend phase-machine change plus a hotkey registration.

## Out of Scope

- A new IPC verb to abort the in-flight Sidecar transcribe. The current Chunk finishes and its result is discarded.
- Cancelling a Live transcription Session (the long-form recording flow). This PRD covers only Dictation. If the user wants a similar exit-on-Escape for full Sessions, that is a separate spec.
- Per-phase progress UI in the Bubble (e.g. a spinner with elapsed time). The Bubble's existing label transitions are enough.
- A user setting to disable the Escape-to-cancel binding or rebind it to a different key. Escape is the universal "abort" key; making it configurable is a future option, not a launch requirement.
- An undo window for cancellations. Cancel is destructive: no Output action ran, nothing to undo.
- A confirmation prompt before cancelling. The point of this feature is fast escape; a confirmation defeats the purpose.
- Cancelling a Dictation from another input device (e.g. pedal, MIDI). Keyboard-only.

## Further Notes

- The Bubble window does not currently capture keyboard focus on its own, so registering Escape only inside the Bubble's React tree would not work when the user is focused elsewhere. The Global hotkey route is required.
- The new `cancelled` `BubbleState` should not animate-pulse — it's a terminal state shown briefly. Use the static neutral-grey treatment.
- If a future iteration wants to abort the in-flight Sidecar transcribe (rather than discard its result), that would require a new `cancel { id }` IPC request and a per-request abort hook in each engine backend. Defer until measurement shows the dangling transcribe is actually a problem.
- This feature lightly couples the **Dictation Lifecycle Hook** to the **Global Shortcut Router** in a new direction (the hook now drives a registration the router exposes). Keep the surface small: a `registerCancelHotkey(handler)` / `unregisterCancelHotkey()` pair on the router rather than letting the hook reach into the Tauri global-shortcut plugin directly.
- The phase the cancellation fires from is the right granularity for analytics — coarser than the full state machine, finer than a single `cancelled` flag — and matches what the user would describe in a bug report ("I pressed Escape while it was processing").
