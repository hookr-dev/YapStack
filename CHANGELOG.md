# Changelog

All notable changes to YapStack will be documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- **Optional end-to-end-encrypted multi-device sync** (off by default; not yet included in official release builds — requires building with the `sync` feature). Devices sync through a self-hosted blind relay (`deploy/` Docker stack: server + Postgres + MinIO) that stores only ciphertext: notes, sessions, transcripts, and dictations converge via on-device CRDT merge; audio syncs as encrypted blobs with on-demand fetch and a one-time library backfill; new devices join via a fingerprint-approval ceremony; live sessions render as read-only follow-along on other devices; sync status is honest end-to-end (catching-up counts, unreadable-changeset quarantine warnings, distinct unreachable/expired states). See `docs/self-hosting.md` and `docs/CRYPTO_SPEC.md`.
- **Segment context-menu reliability on upgraded databases.** Editing, hiding, and deleting transcript segments failed on databases whose sync schema was upgraded across builds; the app now detects and repairs stale change-tracking triggers automatically at startup, and database errors surface their real cause in toasts and logs instead of a generic failure.
- **Transcript copy & export.** The session actions (⋮) menu gains **Copy transcript** (to the clipboard) and **Export transcript** (save as Markdown/text), each offering the full transcript, just your side, or just the other speaker's side. Copy and Export are grouped with "Show audio file" as a distinct content-export section of the menu.
- **Paginated logs.** The Logs panel loads more recent entries on open and adds a "Load older entries" control to page further back through the in-memory buffer.

### Changed
- **Parakeet uses the GPU by default on Windows.** WebGPU acceleration was opt-in behind `YAPSTACK_PARAKEET_ACCEL=webgpu` while it awaited hardware validation; that validation passed, so Windows now runs Parakeet on the GPU automatically. If the GPU can't initialize, transcription falls back to CPU and says so in the logs instead of silently pretending it used the GPU. Set `YAPSTACK_PARAKEET_ACCEL=cpu` to force CPU. Whisper remains CPU on Windows (#81).
- **Advanced transcription settings are capability-gated.** Prompt Context / Prompt Decay now appear only for engines that support an initial prompt (hidden for Parakeet); persisted values are kept when the rows are hidden.
- **Folder dialog reframed around AI context.** The folder description is now the prominent "Context for AI" field, and the icon grid collapses into an expandable picker. The transcript bulk-selection bar's copy is simplified to a single attributed-markdown copy.

### Fixed
- **Full/Max backfill no longer drops the oldest ~15s.** A "Full buffer" rewind previously lost ~15s of the earliest audio to a capacity-proportional safety margin; it now returns the full requested window (the safety margin is a fixed sub-second guard applied only when over-requesting).
- **Backfill no longer bounces the transcript.** Older segments arriving during live transcription no longer jump the viewport; a scrolled-up reader stays put, while the live tail still follows when you're pinned to the bottom.
- **Session-list scroll position is remembered.** Returning from a session to the list restores your scroll position (independently per filter) instead of snapping to the top.
- **Logs "Reveal" button works again.** It now opens the OS file manager on the current log file; it was calling an ungranted permission and silently failing.

## [1.0.0-alpha.12] - 2026-07-04

### Added
- **AI Connections & Profiles.** The single global provider is replaced by reusable **Connections** (a provider's API key + endpoint — OpenAI, OpenRouter, or any OpenAI-compatible server) and **Profiles** (a Connection paired with a specific model). Each AI feature (Chat, AI actions, dictation slots) is assigned a Profile, so different features can use different models. Settings → AI gains Connections / Profiles / Assignments sections with a shared profile picker, cached per-Connection model catalogs, and a Connections-first onboarding step. Existing single-provider settings migrate automatically (keys preserved as Connections). AI errors surface to the user — there is no silent fallback (#63).
- **Live Insights.** A floating always-on-top overlay runs a user-defined LLM extraction against the live session transcript and shows the result while you record. Each Insight has a **Type** (Rolling Summary, Glossary, Action Items, Decisions, Open Questions, Current Topic, or Custom — seeds a starter prompt) and a **Cadence** (Responsive / Balanced / Relaxed / Custom) that decides *when* it refires: it accumulates new speech (word threshold) and fires on a natural pause, bounded by a minimum interval and a maximum-wait liveness backstop. The overlay auto-fits its height to the content (no scrollbar), supports click-through over the area beneath it, and lets you switch the active Insight from its header (#63).
- **Windows build pipeline (experimental, unpublished).** CI now compiles, lints, and tests the entire workspace for `x86_64-pc-windows-msvc` on every PR, and the release workflow builds a Windows NSIS installer + auto-updater artifacts alongside the macOS build. Windows artifacts are canary/pre-release only — not yet published or supported. Windows-specific hardening landed with the pipeline: a single-instance guard (relaunching focuses the running app instead of spawning a second process), working window controls on the custom title bar, a per-platform tray icon, and no console-window flash during dictation. Parakeet on Windows runs on CPU by default with the int8 single-file model; WebGPU acceleration is opt-in via `YAPSTACK_PARAKEET_ACCEL=webgpu` until hardware-validated (#78).

### Changed
- **Per-chat model override** is now stored in a dedicated `chat_context_settings` table; clearing a Connection or Profile cascades to any chat/dictation/Insight that referenced it (with affected items listed in the delete confirmation) so nothing is left pointing at a deleted Profile (#63).

### Fixed
- **Recording indicator is draggable from its entire surface.** Previously only parts of the chrome acted as a drag region (#66).

## [1.0.0-alpha.11] - 2026-05-15

### Added
- **Dictation works during an active session.** Hitting a dictation hotkey while a recording session is in progress now returns the dictated text (paste / clipboard / new-note, per slot config) without disturbing the session. Dictated content stays out of the session transcript: the session's own mic-side processing suspends for the duration of the dictation, with a rising-edge flush that preserves the partial word the user spoke just before triggering the hotkey, and a falling-edge reset that drops the dictation window's audio from session VAD state (#48).
- New `LiveSourceKind` (`session` | `dictation`) routing dimension carried end-to-end on every live-transcription event (segments, status, backfill-complete) and accepted on `start_live_transcription` / `stop_live_transcription` / `get_live_transcription_status`. Two-prong frontend filters (`source_kind` + `session_id`) keep dictation lifecycle from leaking into session UI state and vice-versa (#48).
- New scheduler `JobOrigin::Dictation` priority tier — order is `FinalFlush > Dictation > Live (mic/system round-robin) > Backfill`. Dictation chunks jump the queue past session live chunks. The scheduler is non-preemptive, so a dictation chunk still waits behind any sidecar job already in flight at trigger time (#48).
- **Frontend logs now land in the unified log file and LogsPanel.** A new Tauri-side bridge routes JS `log.error/warn/info/debug` calls, every `console.error` / `console.warn`, uncaught errors, and unhandled promise rejections through the same `tracing` subscriber that handles native logs — so they appear on stderr, in the rolling daily file under `app_log_dir()`, and in LogsPanel alongside Rust output. A "Snapshot" button in LogsPanel captures a JS-heap snapshot on demand, and a 60 s background sampler records process RSS so heap-growth analysis works on macOS where WKWebView doesn't expose `performance.memory` (#44).

### Changed
- The transcription scheduler is now a **long-lived app-level singleton**. It's constructed once at engine init and shared by every live runtime (one session + one dictation concurrently); previously each session built its own scheduler and round-tripped the `TranscriptionClient` back into shared state on stop. `init_transcription_client` is now idempotent on a matching engine/config and returns an explicit "shut down first" error on mismatch — engine swap is an explicit two-step operation. `submit()` returns `Result<Receiver, SchedulerError>` and rejects with `Shutdown` once the scheduler is terminal so racing `Arc<Scheduler>` clones can't enqueue into a dead worker. Backfill-gating is now a per-producer bitmask (`{LiveMic, LiveSystem, Dictation}`) so one runtime clearing its bit while another is mid-utterance can't unblock backfill prematurely (#48).
- `LiveTranscriptionState` is now a two-slot struct (`session`, `dictation`) with explicit `Idle | Starting | Running | Stopping` lifecycle states — same-kind double-start is rejected even during the finalization window, so a stop-then-fast-start can't race the prior task's tail emission (#48).
- **AI chat tool invalidation fires per round, not at end-of-conversation.** Each tool's `execute()` writes to SQLite immediately as the model emits the call, so the UI store needs to reflect those writes as they happen — both for the human (list view) and for the model's `getToolContext()` snapshot on the next round. `useChatMessages.handleSend` now invokes `onToolsExecuted` for the tools that ran in each round at the natural per-round boundary, dropping the previous batched-at-end call. Side effect: a chat that errors after one round still leaves the store consistent with what landed on disk, so the session list won't desync from the DB after a rate-limit / network drop (#52).
- **Dictation volume ducking is now reduce-by-percentage, not set-to-percentage.** Matching Discord and the Windows ducking model, the slider now lowers the *current* system volume by the configured fraction (`current × (1 − amount)`) rather than snapping to an absolute level — quiet starting volumes get gently nudged, loud starts drop further. Existing users' settings are converted in the persist merge (`new = 1 − old`) so effective duck strength is preserved. Setting label and subtext clarify "while dictating" instead of the older generic "while recording" (#49).

### Fixed
- **Double-clicking a chat action (Summarize, Key Points, etc.) no longer fires the action twice.** The `isStreaming` React state guard didn't apply the disabled attribute until the next render, so a fast second click on the same `CommandItem` slipped through and triggered duplicate LLM requests (and could trip OpenAI rate limits when chained). `useChatMessages.handleSend` now bails synchronously via a `useRef` reentrancy guard (#51).
- **Hour-long timestamps now wrap correctly.** Sessions and chat timestamps past 60 minutes render as `1:00:00` / `1:01:00` instead of `60:00` / `61:00`. A third copy of the same buggy formatter inside `AIChatMessage` is replaced with the shared `formatTime` (#43).
- **Right-click actions on transcript segments (Edit / Delete / Hide / Insert into Notes) now take effect immediately during an active recording.** The DB write happened right away, but `refreshViewSessionSegments` early-returned for the live session — so `activeSessionSegments` (the array `NoteDetailView` renders during recording) didn't update until stop. The action looked like a no-op until the session ended (#45).
- **Resize handle no longer shows a stray blue focus outline** (#50).
- **Drag-select on the transcript starts from anywhere in the visible area.** Previously a drag that began on the top or bottom edge of a segment bubble fell through to native text selection and painted a blue highlight. The marquee now starts on any `pointerdown` in the container and promotes after a 6 px movement threshold; sub-threshold clicks still trigger the bubble's `onClick` (a one-shot `onClickCapture` interceptor blocks the synthesized click on a real drag), and active editing controls are exempt so caret/drag-select inside a contenteditable bubble is never clobbered. Marquee rendering also moved off React state, so a drag now performs zero React work per `pointermove` (#60).
- **Live-transcription audio offsets stay correct across device-change buffer swaps.** When the ring buffer was replaced mid-session (e.g. AirPods reconnect), every post-swap chunk's `audio_offset_seconds` was being rewound by however many seconds of wall clock had elapsed inside the loop — a 60-min wall / 58-min audio session was observed landing with `final_lag_secs ≈ 388 s` and all post-swap segments stamped ~6 min behind real session-time. A per-source `audio_offset_anchor_seconds` now advances on every `BufferReplaced`; in-flight rescue chunks and pre-swap unflushed audio are preserved through the swap (with WAV append for `MicOnly` / `SystemOnly`); an in-flight dictation window is rebased into the new buffer's coordinate space rather than dropped; and both rescues are truncated at the earliest dictation boundary so dictated audio can't leak into session content. New `marker = "buffer_replaced_rebase"` info log carries the diagnostic state for after-the-fact analysis (#59).

## [1.0.0-alpha.10] - 2026-05-04

### Fixed
- **Right-click context-menu actions on transcription segments now fire.** Edit / Copy / Insert into Notes / Hide / Delete had been silently no-op'ing — the transcript's drag-select handler was treating menu-item clicks as marquee starts and swallowing the click (#28).

### Changed
- **Settings → General footer rebranded** (#29).

## [1.0.0-alpha.9] - 2026-05-03

### Added
- **Auto-failover on device change** — the device picker now reflects the OS state in real time. When AirPods drop, a USB mic is unplugged, or the user changes the system default in Settings, capture automatically rebinds the affected Stream to the new system default and shows a transient toast naming the new device ("Switched mic to MacBook Pro Microphone"). System-audio loopback follows the default output (and the alerts/UI route) the same way. `Mixed` capture stops both Sources fail-fast if either can't be recovered (#25).
- New Tauri event `devices-changed`. The frontend replaces its cached device list and reconciles the persisted `selectedMicDeviceId` when its device disappears. The broker re-emits this event whenever the device list changes *or* any system default flips, so the FE store's `is_default` flags always reflect the current OS state without a second listener (#25).
- Fourth Core Audio property listener for `kAudioHardwarePropertyDefaultSystemOutputDevice` (the alerts/UI route), distinct from the media output selector (#25).
- `RestartTarget::FollowDefault` mode for `restart_mic`/broker-driven failover. Probes the new system default *first*, then falls through to stored id/name. The watchdog path keeps `PreserveBinding` (stored id first) for stream-error / write-pos-stall recovery (#25).
- **Multi-color highlights in the note editor.** Yellow, green, blue, purple, red palette themed for both light and dark; pick from the highlight dropdown in the toolbar or selection bubble. Highlights re-theme automatically — they're stored as CSS variable references that resolve per theme at render time (#26).
- **Heading dropdown shows the current level.** The heading button reads "H1" / "H2" / "Normal" and the active row inside the dropdown is highlighted (#26).
- **Selection bubble menu is scoped to inline marks** — bold, italic, underline, strike, code, highlight, link. Block-level formatting (headings, lists, blockquote, code block) lives in the static toolbar, matching Notion / Linear / Novel conventions (#26).
- **Static toolbar adds Link and Code Block buttons.** Multi-line fenced ` ``` ` code blocks were already supported by the input rule and round-trip through markdown; the toolbar now exposes them alongside an explicit Link control (#26).
- **Pasting markdown with fenced code blocks parses correctly.** When a paste's plain-text payload contains a ` ``` ` fence and there's no rich HTML alternative on the clipboard, the paste is parsed as markdown — so copying a code block out of a terminal, GitHub issue, or markdown file lands as a real code block instead of a flat string (#26).

### Changed
- The Rust audio crate's listener path moved from `AtomicBool` flag-polling to a runtime-agnostic `DeviceEventSink` consumed by an always-on Tauri-side broker. The broker debounces bursty Core Audio events in a 250 ms window and gates restarts on `kAudioDevicePropertyDeviceIsAlive`, replacing the previous unconditional 200 ms `thread::sleep` workaround for the AirPods/Bluetooth revert window (#25).
- Dropped the defensive ~30 s name-comparison drift poll inside the live-transcription loop. Device changes flow exclusively through the broker now; the missed-event safety net is no longer needed (#25).
- `stream-health` event payload now carries `bound_device_name` so the FE can render device names in auto-failover toasts (#25).
- Promoted broker decisions (debounce flush, failover routing source/target, same-device rebind warnings) to `info`/`warn` level so the failover chain is visible in default-level logs without enabling `RUST_LOG=debug` (#25).
- **Toolbar and bubble-menu active states are reactive and visible.** Buttons re-render on selection changes (via `useEditorState`) and gain an accent underline when their mark or block is active, so cases like "bold persists on a new line" are obvious (#26).
- **Sidebar shortcut moved from ⌘B to ⌘\\ (Notion convention)** so it stops fighting TipTap's bold binding inside notes. Both shortcuts now work as intended; existing custom rebinds are preserved (#26).
- **In-app shortcuts can fire while typing in a note.** Command palette (⌘K), sidebar (⌘\\), settings (⌘,), filter switches (⌘1/⌘2), new note (⌘N), stop recording (⌘.), toggle chat (⌘J), and pin (⌘D) now work with editor focus. Escape and ⌘⌫ still defer to the editor (#26).
- **Selection bubble menu stays inside the editor.** Floating UI's flip/shift now use the editor's contenteditable as the boundary, so the bubble can no longer land on the static toolbar above or the floating chat bar below; it also renders at `z-50` so it always sits above other floating UI (#26).

### Fixed
- cpal's runtime-allocated loopback aggregate (`com.cpal.LoopbackRecordAggregateDevice`) no longer appears in the input-device picker. The aggregate is a private cpal implementation detail used for system-audio loopback; it leaked into in-process device enumeration despite being flagged private to System Settings, and selecting it as a microphone crashed capture with "stream type not supported." Filtered at enumeration time and additionally rejected by `start_mic` as defense-in-depth (#25).
- `device_liveness` (formerly `is_device_alive`) now actually gates broker-driven restarts on macOS. The previous `strip_cpal_prefix` helper expected `"CoreAudio:"` (CamelCase), but cpal's `HostId::Display` lowercases — the real prefix is `"coreaudio:"`. Since the helper was a no-op for every real device id, the AirPods-revert IsAlive gate had been bypassing every gate-check since this code shipped (#25).
- Broker-driven Mic failover now actually moves to the new default. The old probe order (stored-id → stored-name → default) caused the live loop to silently re-bind to the *previous* device whenever it was still alive — exactly the symptom users hit when plugging into a Thunderbolt dock that brings a new audio interface online while the laptop's built-in mic stays present (#25).
- Devices with empty names are filtered from the picker so the dropdown can't render a confusing blank entry (#25).
- **Explicit microphone disappearance now fails over to the system default.** The broker's explicit-pick liveness check used a single boolean that fail-opened on "couldn't tell" — an unplugged USB mic that was missing from the device list still reported `alive=true`, so subsequent `DefaultInputChanged` events were silently dropped. Replaced with a tri-state `DeviceLiveness` (`Alive`/`Dead`/`Absent`/`Unknown`); the explicit-pick branch now only skips failover on `Alive` (#25).
- **Broker-driven `FollowDefault` restart now correctly updates `bound_is_default`.** Previously the flag was preserved verbatim across restart, so an explicit pick that disappeared and successfully fell over to the system default still flagged the binding as "explicit" — future default changes were silently ignored. The flag is now derived from "did the post-restart bind id match the resolved default?" (#25).
- **Mid-stop device-change events can no longer race the live-transcription final flush.** The broker's direct-restart fallback used inbox-presence to decide between routing through the live loop and calling `AudioManager::restart_*` directly. `stop_live_transcription` clears the inbox before the loop's final flush completes, so a device-change in that window would replace the ring buffer while the loop was still extracting at snapshotted stop positions. Added a separate `LiveSessionPresent` flag the spawned task clears only after scheduler shutdown, and routing now consults it (#25).
- **`devices-changed` is now re-emitted on any default-device change**, not just on `DeviceListChanged`. Prevents stale `is_default` flags in the FE store when only the system default flips (e.g. user picks a different mic in System Settings) (#25).
- **Note checklist checkboxes are themed and aligned.** Checked state uses the accent color with a contrast checkmark and a focus-visible ring; the checkbox aligns with the first line of text and stays aligned across wraps (#26).

### Removed
- `AudioManager::{mic_default_changed, system_audio_default_changed, device_list_changed, mic_input_drifted, system_audio_output_drifted, live_default_input_name, live_default_output_name}` and `DefaultDeviceWatcher::take_change` — superseded by the event-driven sink path (#25).

## [1.0.0-alpha.8] - 2026-05-02

### Added
- **Dictation volume ducking (macOS).** Opt-in setting on the Dictation tab lowers the system output volume to a configured target (default 20%) while a dictation is recording, then restores it the moment recording ends. Only ever lowers — never raises — so a quieter starting volume isn't bumped up. Tracks the *device* you started ducking, so swapping outputs mid-dictation (AirPods connect, USB DAC unplug) restores the original device rather than leaving it stuck at the ducked level. No-op on Windows / Linux (#22).

### Changed
- **Backfill no longer starves live transcription.** Long backfill jobs are now sliced and yield to live work at the sidecar. Live audio keeps up with real time even when a fresh session is rewinding into a full 5-minute backfill window (#20).
- **Stopping a live session is now a clean cut-off.** When you press stop, the loop snapshots the audio boundary at that instant plus a short tail grace, finalizes whatever's inside, and ignores anything captured after. The final transcript and WAV no longer drift based on host load (#20).
- **Session WAV uses the right sample rate** for the configured capture source (`MicOnly`, `SystemOnly`, or `Mixed`) rather than always preferring whichever buffer happened to exist (#20).
- **Live-status diagnostics:** the slow-sidecar indicator now reports preserved-drain backlog (audio queued behind inference) instead of the old head-drop counter, which implied audio was being discarded. Audio is now preserved through catch-up; the new fields show how far behind the live tier is (#20).
- **Repository now follows the `AGENTS.md` convention** for AI-assistant instructions. `CLAUDE.md`, `.github/copilot-instructions.md`, and `.cursor/rules/main.mdc` are now thin stubs pointing at the canonical `AGENTS.md`. New documentation routing under `docs/INDEX.md`, `docs/GLOSSARY.md`, `docs/AGENT_GUIDE.md`, `docs/LINEAR_TICKETS.md`, and `docs/adr/`. CONTRIBUTING.md restructured for both human and AI contributors with Quickstart, Definition of Done, and Verification commands sections.
- **Internal sidecar crate renamed** `yapstack-sidecar` → `yapstack-transcription-sidecar` to make room for additional sidecar workers (e.g. embeddings) without naming ambiguity. No user-visible behaviour change; build scripts are now `scripts/build-sidecars.sh` (wrapper) and `scripts/build-transcription-sidecar.sh` (#21).

### Fixed
- **Session MP3 export no longer silently downsamples.** A 48 kHz capture exported at 64 kbps is now written at 48 kHz; previously LAME's auto-rate selection dropped it to 22.05 kHz, leaving the audio file and the database disagreeing on sample rate (#20).
- **"Processing prior audio" divider** in the chat now sits at the actual backfill→live boundary instead of being pinned to the bottom of the transcript. As the rewind buffer streams in, the divider slides up into the right place (#19).
- **Lag metric is no longer overstated** when backfill chunks arrive after live chunks. The reported lag tracks the maximum completed audio offset rather than overwriting on every chunk, so a late backfill chunk for older audio can't pull the counter backwards and inflate the displayed lag (#20).
- **"Backfill in progress" affordance clears immediately** when there's nothing to backfill (resume, empty buffer, or a backfill request that the ring buffer can't honor). Previously the badge could stay stuck for the entire session (#20).
- **Transcription engine self-heals after a wedged shutdown.** If the engine fails to release cleanly when stopping a session (rare; happens when the sidecar hangs past the 5-minute drain ceiling), the app now resets engine state and re-runs auto-setup before the next action instead of leaving the next session to fail with `NotInitialized` (#20).
- **Dev sidecar mirror now survives `cargo` rebuilds.** Local development could leave the feature-rich sidecar binary clobbered by a feature-poor `cargo build` rebuild, breaking subsequent `pnpm tauri dev` runs (#18).

## [1.0.0-alpha.7] - 2026-04-30

### Added
- Folder auto-recommendation: when keywords from a folder's name, description, or tags on sessions previously filed under it appear in the live transcript, the top folder surfaces inline above the session view as a "Recommended" pill. Click the pill to confirm "Add to {folder}", pick a different folder from the full list, or dismiss. Single-folder workspaces show inline accept/dismiss instead of a dropdown (#17).
- Default dictation slots for new users: "Clean & Focus" (Ctrl+Shift+C) and "Engineer" (Ctrl+Shift+X) AI post-processors alongside Raw Dictation (now bound to Ctrl+Shift+D). Existing users' configured slots are untouched (#17).
- Mid-session folder picker: the session header actions dropdown renders during recording with a Folders submenu, so a session can be filed without stopping it (#17).
- "Show audio file" action on dictation history entries (icon button + context-menu item) — reveals the WAV/MP3 in Finder, matching the existing session affordance (#17).
- Mic-only clarification in the Dictation settings header so users know dictation never captures system audio (#17).

### Changed
- Rewind dropdown supports short buffers: the "Full buffer" action now honors any available backfill — including sub-30-second and sub-second amounts — rather than gating on hardcoded steps. Sub-second precision is preserved end-to-end (#16).
- Engine and capture status now surface only via the title-bar status dot. The centered "Loading transcription engine…" banner in the main content area is gone. The dot pulses amber during downloading/initializing and is steady green only when actually capturing (#17).
- Mid-session context menu replaces the disabled "Delete" with a destructive "Stop recording" entry so users have an actionable path forward instead of a silently disabled item (#17).
- Rebinding a shortcut to a key already used by another shortcut (or dictation slot) now rejects with a toast naming the conflicting owner. Previously the new binding silently stole the key from its prior owner (#17).
- Updater install progress bar is strictly monotonic. Repeated `Started` events and out-of-order chunk callbacks no longer pull the bar backward; sub-1% chunks no longer trigger redundant re-renders. The same guard applies to the model-download progress (Whisper / Parakeet / Sortformer fetch) (#17).

### Fixed
- Auto-suggested folder no longer keeps surfacing other folders after the user has already filed the session. Acceptance and override are now one-shot for the session; dismiss stays per-folder so the next-best match can still surface if the user keeps rejecting picks (#17).

## [1.0.0-alpha.6] - 2026-04-30

### Fixed
- Voice dictation: random/garbled characters when copying transcripts via `pbcopy` on macOS — `LC_CTYPE` is now forced to `UTF-8` when invoking `pbcopy` so multibyte output round-trips correctly (#15).
- CI: install `cmake` before the Rust build so whisper.cpp configures cleanly.
- CI: pass `-march=native` so whisper.cpp detects `i8mm` on Apple Silicon runners.
- CI: stub `libwebgpu_dawn.dylib` placeholder alongside the existing ONNX Runtime placeholders.

## [1.0.0-alpha.5] - 2026-04-28

First public alpha release.

### Added

- Real-time mic + system audio capture with always-on 5-minute ring buffer.
- Whisper transcription engine (Metal-accelerated on Apple Silicon, broad language support).
- Parakeet TDT v3 transcription engine via parakeet-rs + ONNX Runtime, with int8 variant + WebGPU acceleration on Apple Silicon.
- Sortformer speaker diarization (Parakeet only).
- Per-source VAD (Silero V5), hallucination filtering, two-tier prompt context, prompt decay.
- Stream health monitoring with auto-restart (up to 3 attempts).
- Voice dictation: multiple slots, hold/toggle modes, per-slot AI processing prompts, paste/copy/note actions, history grouped by day.
- Per-session AI chat with multi-turn tool calling (10 tools: rename, pin, save-to-notes, tag, folder, search, etc.).
- Tiptap rich-text notes editor with version history.
- Folders + tags + Cmd-K search across sessions, notes, and segments.
- Privacy-first analytics via Aptabase (no transcript content tracked).
- macOS desktop integration: system tray, recording indicator overlay, customizable global shortcuts.
- Custom audio playback protocol with seeking and 0.5×–2× playback speeds.

### Platform support

- macOS (Apple Silicon): officially supported.
- macOS (Intel): best-effort.
- Windows: experimental — local builds only, no official CI/CD artifacts.
- Linux: not yet supported.
