# Implementation Log

What was built, in what order, and key decisions made along the way.

---

## Phase 1 (Prior — Foundation)

Established the project skeleton and core audio infrastructure.

### What was built
- **Workspace structure**: 5 crates (`yapstack-common`, `yapstack-audio`, `yapstack-transcription`, `yapstack-sidecar`, `yapstack-desktop`)
- **yapstack-common**: `AppConfig`, `AudioConfig`, `TranscriptionConfig`, domain types (`CaptureState`, `AudioSource`, `DeviceType`, `PermissionStatus`, etc.)
- **yapstack-audio core**: `AudioRingBuffer` (lock-free SPSC), `MicrophoneCapture`, `SystemAudioCapture` (platform-abstracted), `AudioManager`, device enumeration, error types
- **yapstack-desktop**: Tauri app skeleton with tray icon, audio commands (list devices, start/stop capture, snapshots, buffer info), DTO layer with specta type generation
- **Frontend**: React 19 + Vite + TailwindCSS + Zustand skeleton, App component, test setup
- **macOS entitlements**: audio-input + screen-capture

### Key decisions
- **16kHz mono** as default audio format (Whisper-optimal)
- **180-second ring buffer** to balance memory (~11.5 MB) with capture history
- **Lock-free SPSC ring buffer** using `UnsafeCell` + `AtomicUsize` — audio callbacks cannot block
- **Platform abstraction via enum variants** for `SystemAudioCapture` — macOS/Windows/Unavailable
- **DTO pattern** — domain types don't depend on specta; Tauri commands use separate DTO structs with `From` impls

---

## Phase 2 — Mixer, WAV Export, Capture Triggers

Added audio mixing, WAV export, and capture orchestration (instant + session modes).

### What was built

| File | Purpose |
|------|---------|
| `yapstack-audio/src/mixer.rs` | `MixConfig`, `mix_to_mono()`, `apply_gain()`, `normalize_in_place()` — pure functions, 9 tests |
| `yapstack-audio/src/export.rs` | `write_wav()`, `write_wav_to_temp()` — 16-bit PCM via hound, 4 tests |
| `yapstack-audio/src/capture.rs` | `CapturedAudio`, `SessionMark`, `CaptureResult` data types |
| `commands/capture.rs` | 4 Tauri commands: `trigger_instant_capture`, `start_session`, `end_session`, `get_session_status` |

### Modified files
- **ring_buffer.rs**: Added `samples_written()` and `snapshot_since()` for session support (4 tests)
- **error.rs**: Added `WavExport`, `NoActiveSession`, `SessionAlreadyActive`, `NoBufferAvailable` + `From<hound::Error>`, `From<io::Error>`
- **yapstack-common/types.rs**: Added `CaptureSource` enum
- **manager.rs**: Added `session_mark` field, 8 new methods for capture extraction and session management (4 tests)
- **audio.rs** (DTOs): Added `CaptureSourceDto`, `CaptureResultDto`, `MixConfigDto`, `SessionStatusDto` + `From` impls

### Dependencies added
- `hound = "3.5"` — WAV file I/O
- `tempfile = "3"` — temp file management for WAV export

### Key decisions
- **Session tracking via write_pos snapshots** — `start_session()` records the monotonic `write_pos`, `end_session()` uses `snapshot_since()` to extract exactly the audio recorded during the session. No separate buffer needed.
- **Silent truncation for long sessions** — if a session exceeds buffer capacity, `snapshot_since()` clamps to capacity and a warning is logged. `CaptureResult.duration_seconds` reflects actual captured duration.
- **f32 clamping before i16 conversion** — `clamp(-1.0, 1.0)` before WAV export prevents overflow
- **Temp files persist** — `write_wav_to_temp()` uses `tempfile::Builder` with `.keep()` so files outlive the function call. Cleanup is the caller's responsibility.

---

## Phase 3 — Whisper Sidecar Integration

Added transcription via a sidecar process architecture: model management, JSON-line IPC, and Tauri commands.

### What was built

| File | Purpose |
|------|---------|
| `yapstack-transcription/src/model.rs` | `ModelSize`, `ModelManager` — download from HuggingFace, SHA-256 verification, list/delete, 8 tests |
| `yapstack-transcription/src/whisper.rs` | `WhisperClient` — spawns sidecar, JSON-line IPC, timeout handling |
| `yapstack-sidecar/src/main.rs` | Full sidecar binary — JSON-line protocol, `TranscriptionEngine` (behind `whisper` feature), WAV loading via hound |
| `commands/transcription.rs` | 6 Tauri commands: `get_available_models`, `download_model`, `delete_model`, `transcribe_audio`, `init_whisper_client`, `shutdown_whisper_client` |
| `scripts/build-sidecar.sh` | Build script for sidecar with target triple detection |

### Modified files
- **yapstack-common/types.rs**: Added `TranscriptSegment` (with confidence), `SidecarRequest`, `SidecarResponse` IPC protocol types (3 tests)
- **yapstack-transcription/error.rs**: Expanded to 11 variants: added `DownloadFailed`, `ChecksumMismatch`, `SidecarError`, `SidecarNotRunning`, `Timeout`, `Io`, `Json`, `Http`
- **yapstack-sidecar/Cargo.toml**: Restructured — removed yapstack-audio/yapstack-transcription deps, added `whisper` feature flag, direct deps on `hound`, `tracing-subscriber`
- **tauri.conf.json**: Added `externalBin` for sidecar bundling
- **apps/desktop/src-tauri/Cargo.toml**: Added `yapstack-transcription` dependency

### Dependencies added
- `reqwest = { version = "0.12", features = ["stream"] }` — HTTP client for model downloads
- `sha2 = "0.10"` — checksum verification
- `futures-util = "0.3"` — streaming download
- `tracing-subscriber = { version = "0.3", features = ["env-filter"] }` — sidecar logging to stderr

### Key decisions
- **Sidecar process architecture** — Whisper runs in a separate binary (`yapstack-sidecar`), not in the main app process. Benefits: crash isolation, heavy native dep (`whisper.cpp` + cmake) doesn't affect main app build, can be updated independently.
- **JSON-line IPC protocol** — Simple, debuggable. Each message is a tagged JSON object on one line. Serde's `#[serde(tag = "type")]` provides clean serialization.
- **whisper feature flag on sidecar** — Without cmake, the sidecar still compiles but responds with error messages. This keeps `cargo build --all` working on machines without cmake.
- **Model download from HuggingFace** — Uses `ggerganov/whisper.cpp` ggml models. Downloads to `app_data_dir/models/` with `.download` temp extension, renamed atomically on completion.
- **whisper-rs v0.15 API** — Uses `state.get_segment(i)` (returns `Option<WhisperSegment>`), `segment.to_str()`, `segment.start_timestamp()`/`end_timestamp()` (centiseconds), `segment.no_speech_probability()` for confidence.
- **Sidecar logging to stderr** — stdout is reserved for IPC. `tracing-subscriber` writes to stderr with env-filter support.
- **Sidecar binary naming** — `yapstack-sidecar-{target-triple}` in `apps/desktop/src-tauri/binaries/`. Tauri's `externalBin` config handles the target triple suffix automatically.

### whisper-rs API notes (v0.15.1)
The whisper-rs v0.15 API differs from older examples online:
- `full_n_segments()` returns `c_int` directly (not `Result`)
- Segment access via `state.get_segment(i) -> Option<WhisperSegment>` (not `full_get_segment_text`)
- `WhisperSegment` has `.to_str()`, `.start_timestamp()`, `.end_timestamp()`, `.no_speech_probability()`
- Timestamps are in **centiseconds** (multiply by 10 for milliseconds)

---

## Test Coverage Summary

| Crate | Tests | Notes |
|-------|-------|-------|
| yapstack-audio | 92 passed, 4 ignored | Ring buffer (incl. rms_energy_since), mixer, manager (incl. position API, synthetic buffer tests, stream health), device, export (incl. SessionWavWriter). Ignored tests need hardware. |
| yapstack-common | 18 passed | Config, types serde, IPC protocol, audio deinterleave |
| yapstack-transcription | 10 passed | Model management unit tests, VAD model management |
| yapstack-sidecar | 19 passed | Hallucination filter (`should_include_segment`), repetition detection, punctuation normalization |
| yapstack-desktop | 48 passed | CommandError, MixConfigDto sanitization, trim_leading_silence, capture_source_from_dto, VAD state machine, prompt decay |
| frontend (vitest) | 294 passed | 10 lib + 9 component + 2 hook test files. See Phases 18, 21, 22. |
| **Total** | **481 passed** | |

---

## Phase 4 — Capture Source Fix + Sidecar Resampling

Fixed two bugs in the capture-to-transcription pipeline.

### Bug 1: System-audio-only capture produces "no buffer" error

**Root cause**: `start_capture` Tauri command took an `include_system_audio: bool` parameter. It only had two paths: `true` → `start_all()` (mic first, system optional), `false` → `start_mic()` only. There was no path to start system audio without also starting the mic. When the user selected "System Only", the system buffer was never created, so `trigger_instant_capture()` / `end_session()` returned `AudioError::NoBufferAvailable`.

**Fix**: Replaced `include_system_audio: bool` with `capture_source: CaptureSourceDto` across the stack.

| File | Change |
|------|--------|
| `yapstack-audio/src/manager.rs` | Added `start_capture(source, mic_device_name)` dispatcher that routes `MicOnly` → `start_mic()`, `SystemOnly` → `start_system_audio()` (hard error if unavailable), `Mixed` → `start_all()` |
| `commands/audio.rs` | Replaced `include_system_audio: bool` param with `capture_source: CaptureSourceDto` |
| `stores/appStore.ts` | Removed `includeSystemAudio` from `Settings`, pass `captureSource` to `startCapture` |
| `components/AudioPanel.tsx` | Removed redundant "Include System Audio" toggle and `Switch` import |
| `src-tauri/src/lib.rs` | Tray handler uses `start_capture(MicOnly, None)` instead of `start_mic(None)` |

### Bug 2: Transcription produces garbled output ("[silence] [imitates sound]")

**Root cause**: Sample rate mismatch. `start_mic()` overrides `config.sample_rate` from 16kHz to the device's native rate (typically 48kHz on macOS). Audio is captured and exported to WAV at 48kHz. The sidecar passed these 48kHz samples directly to Whisper, which expects 16kHz mono. Whisper interpreted the audio as 3x faster than reality, producing garbled output. The WAV file played fine in media players because they honor the 48kHz header.

**Fix**: Added audio preprocessing in the sidecar before Whisper inference.

| File | Change |
|------|--------|
| `yapstack-sidecar/src/main.rs` | Added `to_mono()` (channel averaging) and `resample()` (linear interpolation) functions. After reading the WAV, sidecar converts to mono and resamples to 16kHz before calling `state.full()`. |

### Key decisions
- **Resample in the sidecar, not during capture** — The WAV file should contain audio at the device's native sample rate so it plays back correctly in any media player. The 16kHz requirement is specific to Whisper, so the conversion belongs at the transcription boundary.
- **Linear interpolation resampling** — Simple, no external dependencies, sufficient quality for speech audio. Handles arbitrary sample rate ratios (not just integer multiples).
- **Hard error for SystemOnly when unavailable** — Unlike `start_all()` which silently falls back to mic-only when system audio fails, `start_capture(SystemOnly)` returns `PlatformNotSupported` because the user explicitly chose system-only capture.

---

## Phase 5 — System Audio Sample Rate / Channel Mismatch Fix

Fixed system audio playing back slowed down (~0.5x) and distorted when captured alongside mic audio.

### Root cause

The mic is typically 48kHz **mono** (1ch), while the system output device is 48kHz **stereo** (2ch). `start_mic()` was writing the mic's config into `self.config` (48kHz, 1ch). All downstream code — `extract_captured_audio()`, `trigger_instant_capture()`, `end_session()` — used `self.config` to compute sample counts, WAV headers, and duration. So stereo system audio got a mono WAV header — the sidecar interpreted it at half the frame rate — resulting in 0.5x playback.

### What changed

| File | Change |
|------|--------|
| `yapstack-audio/src/mixer.rs` | Added `deinterleave_to_mono()` — converts interleaved multi-channel audio to mono by averaging channels per frame. 4 tests. Updated `mix_to_mono` doc: inputs must already be mono. |
| `yapstack-audio/src/error.rs` | Added `SampleRateMismatch { mic_rate, system_rate }` variant |
| `yapstack-audio/src/manager.rs` | `start_mic()` no longer mutates `self.config`; buffer uses `device_config` directly. `extract_captured_audio()` reads each buffer's `sample_rate()` / `channels()` and deinterleaves to mono. `trigger_instant_capture()` and `end_session()` rewritten to use mono-normalized `CapturedAudio`. WAV export always uses `channels = 1`. Mixed-source paths guard against sample rate mismatch. Deleted `extract_for_source()` (folded into `trigger_instant_capture`). |

### Key decisions
- **Per-buffer format independence** — Each ring buffer carries its own `sample_rate` and `channels`. Extraction methods read these from the buffer instead of relying on a shared `self.config`. This prevents one device's format from leaking into another device's audio path.
- **Deinterleave at extraction boundary** — Multi-channel data is converted to mono immediately after snapshot, before any mixing or WAV export. All downstream code operates on mono data.
- **SampleRateMismatch error** — Mixed-source capture fails explicitly if mic and system buffers have different sample rates, rather than producing garbled audio.
- **No changes to export, ring buffer, or sidecar** — The fix is entirely within the manager's extraction logic and the new `deinterleave_to_mono` utility.

---

## Phase 6 — Live Transcription

Added real-time live transcription with voice activity detection (VAD), per-source audio tracking, backfill, and prompt context.

### What was built

| File | Purpose |
|------|---------|
| `commands/live_transcription.rs` | Live transcription controller, VAD state machine, per-source chunk processing, backfill support. 3 Tauri commands: `start_live_transcription`, `stop_live_transcription`, `get_live_transcription_status` |
| `yapstack-audio/src/capture.rs` | `BufferPositions`, `SeparateExtraction` types for position-based extraction |
| `yapstack-audio/src/manager.rs` | `buffer_positions()`, `extract_since()`, `extract_sources_since()`, `peek_energy_rms()` — position-based APIs for live transcription |
| `yapstack-common/src/audio.rs` | `deinterleave_to_mono()` — canonical shared implementation |

### Key decisions
- **Per-source VAD** — mic and system audio tracked independently with separate energy thresholds and silence state machines. Each source produces its own chunks.
- **Backfill** — `backfill_seconds` captures audio from before the live session started, so the first chunk includes context.
- **Prompt context** — Prior transcript text (up to `prompt_context_chars`) is fed as `initial_prompt` to Whisper for better continuity.

---

## Phase 7 — Second-Pass Refactor

Systematic code review by two senior engineer agents (React + Rust) found 33 actionable items. All were implemented.

### What was fixed

**Bugs & race conditions (6 items)**:
- `setLivePhase("Stopped")` async state race — re-reads `selectedSessionId` inside `.then()` callback
- `onLiveSegment` stale `activeSessionId` guard after awaits
- `clearAllSessions` guard against active recording (refuses with toast)
- `wait_for_response` returning Progress as final response (check before return)
- `init_whisper_client` / `delete_model` hold model manager lock during async ops (extract paths first, drop lock)

**Error handling (6 items)**:
- `updateSettings` capture restart `.catch()` with toast
- `stopActiveSession` bare `catch {}` → `console.error`
- `autoSetup` `.refreshDevices()` `.catch()`
- `capture_history_seconds` input validation (positive + finite)
- `BufferDetail` division by zero guard
- Model download temp file cleanup on rename failure

**React patterns (5 items)**:
- `ModelSection` setState during render → `useEffect`
- `StatusBar` inline source labels → shared `SOURCE_LABELS_FULL` constant
- `deinterleave_to_mono` canonical in `yapstack_common::audio`, mixer wrapper `pub(crate)`
- `TranscriptionTab` option arrays hoisted to module scope
- Removed unused `_config` parameter from `MicrophoneCapture::start`

**Rust soundness (4 items)**:
- Removed `unsafe impl Sync for SendStream` (only `Send` needed)
- `debug_assert` for incomplete audio frames in `deinterleave_to_mono`
- `warn!()` log for `extract_since` silent `None` on sample rate mismatch
- `MixConfigDto::sanitized()` validates NaN/infinity/negative gains at command boundary

**Accessibility (3 items)**:
- `aria-label="Back to sessions"` on SettingsPanel back button
- `sr-only` Recording label in SessionListItem
- `aria-label="Settings"` on SessionSidebar settings button

---

## Phase 8 — Transcription Configurability

Added user-configurable transcription parameters and prompt context windowing.

### What was built

| File | Change |
|------|--------|
| `commands/live_transcription.rs` | Added `prompt_context_chars` to `LiveTranscriptionConfig` (default 350). Prior transcript text fed as `initial_prompt` to Whisper for continuity. |
| `yapstack-sidecar/src/main.rs` | `TranscriptionEngine::transcribe()` accepts `initial_prompt` parameter. Added Whisper params: `suppress_blank`, `suppress_nst`, `no_context`, `temperature_inc(0.0)`. Enabled flash attention via `ctx_params.flash_attn(true)`. |
| `yapstack-common/types.rs` | Added `initial_prompt: Option<String>` to `SidecarRequest::Transcribe` |
| `stores/appStore.ts` | Added `promptContextChars` to Settings. Settings version bumped to 4 with migration. |
| `components/settings/TranscriptionTab.tsx` | UI for prompt context chars setting |

### Key decisions
- **Prompt context at Whisper level** — Rather than concatenating audio chunks, feed prior text as `initial_prompt`. Simpler, no audio manipulation, Whisper uses it for vocabulary/style priming.
- **350 char default** — Enough for ~2 sentences of context without overwhelming the prompt budget.

---

## Phase 9 — Backfill & Reliability Improvements

Systematic improvements to the backfill transcription flow, concurrent processing, and error handling.

### What was built

| File | Change |
|------|--------|
| `commands/live_transcription.rs` | Refactored to shared `TranscriptionContext` struct. Backfill runs concurrently with live VAD via `tokio::select!`. Added `trim_leading_silence()` (50ms windows, 200ms pad). Added `is_backfill` flag to `LiveSegmentEvent`. Emits `backfill-complete` event. Removed `ProcessingBackfill` phase. Added `ChunkResult` return type. Interleaved backfill processing (window 0 for all sources, then window 1). Skip entirely silent chunks. |
| `stores/appStore.ts` | Replaced `backfillPendingSeconds: number | null` with `backfillActive: boolean`. Added `onBackfillComplete()` handler. `onLiveSegment` wrapped in serialization queue (`segmentQueueTail`) to prevent concurrent DB write races. Re-reads `activeSessionId` after awaits to guard against navigation races. Added `console.error` for all catch blocks (replaced silent failures). Guard against `clearAllSessions` during active recording. Capture restart skipped during active live transcription. |
| `hooks/useLiveTranscriptionEvents.ts` | Added `backfill-complete` event listener |
| `yapstack-sidecar/src/main.rs` | Added `should_include_segment()` hallucination filter. Filters: empty text, special tokens, low confidence (< 0.4), known patterns at marginal confidence (< 0.6). Tests for filter function. |
| `yapstack-audio/src/manager.rs` | `start_all()` surfaces degradation error message when system audio fails (instead of silent fallback). `extract_since()` logs warning on sample rate mismatch. |

### Key decisions
- **Concurrent backfill + live** — Backfill processes historical audio in a separate tokio task while the live VAD loop runs simultaneously. No blocking. `tokio::select!` waits for either stop signal or backfill completion.
- **Serialization queue** — `enqueueSegmentWork()` chains promises to ensure only one `onLiveSegment` handler runs at a time. Prevents interleaved reads/writes to `activeSessionSegments` from concurrent backfill and live events.
- **Interleaved backfill** — Process window 0 for all sources, then window 1, etc. Produces chronologically ordered output instead of all mic chunks then all system chunks.
- **Silence trimming** — `trim_leading_silence()` scans in 50ms windows, keeps 200ms pad before first detected energy to avoid clipping speech onset. Entirely silent chunks are skipped.
- **Hallucination filtering in sidecar** — Filtering happens at the transcription boundary, not in the frontend. Known Whisper failure modes (repeating "thank you", "[BLANK_AUDIO]") are caught before they reach the UI.
- **Degradation surfacing** — `start_all()` now sets `error_message` when system audio fails instead of silently falling back. Frontend can display "Mixed mode degraded to mic-only".

---

## Phase 10 — UI Rework

Complete frontend redesign: notes-first navigation model, side-by-side editing, audio playback with waveform, rich text notes, folders, drag-and-drop, search, and multiple polish fixes.

### Navigation & Layout Overhaul

| File | Purpose |
|------|---------|
| `App.tsx` | Root layout with `ThemeProvider` (light/dark/system) |
| `AppLayout.tsx` | Main layout: `AppSidebar` + content area + `SearchCommand` + `AIChatPanel` |
| `AppSidebar.tsx` | Left sidebar: folder tree, create session/note buttons, filter (all/pinned/folder), settings |
| `NoteCardList.tsx` | Grid of `NoteCard` components for the note-list view |
| `NoteCard.tsx` | Draggable session/note card with pin badge, folder indicator, context menu |
| `FolderItem.tsx` | Sidebar folder entry with rename/delete context menu, drop target for drag-and-drop |
| `SearchCommand.tsx` | Cmd+K palette via `cmdk` — fuzzy search sessions and notes |
| `AIChatPanel.tsx` | Bottom drawer (via `vaul`) placeholder for AI assistant |
| `RecordingBeacon.tsx` | Pulsing red dot indicator during active recording |

**Navigation model changed**: Views renamed from `"sessions" | "session-detail" | "settings"` to `"note-list" | "note-detail" | "settings"`. `ListFilter` added for filtering by all/pinned/folder.

### Note Detail View

| File | Purpose |
|------|---------|
| `NoteDetailView.tsx` | Orchestrates 4 layout modes: active recording, completed transcription (split pane), manual note, fallback |
| `SessionHeaderV2.tsx` | Header: back button, editable title, source/recording badges, duration, segment count, actions dropdown (delete) |
| `NoteEditor.tsx` | Tiptap rich text editor with formatting toolbar (Bold, Italic, H1, H2, Bullet List, Ordered List, Blockquote, Code), debounced autosave, version snapshots on blur |
| `NoteHistoryPanel.tsx` | Collapsible panel showing note version history with restore |

### Audio Playback & Waveform

| File | Purpose |
|------|---------|
| `AudioPlayer.tsx` | HTML5 `<audio>` player with play/pause, seek slider, speed control (0.5x–2x), time display |
| `commands/capture.rs` | Added `export_session_wav`, `delete_session_wav` commands |
| `lib.rs` | Custom `audio-stream://` URI scheme protocol with HTTP range request support for audio seeking |

### Editable Segments

| File | Purpose |
|------|---------|
| `EditableSegment.tsx` | Segment bubble with click-to-edit (inline textarea), context menu (Edit, Copy, Hide, Delete), visual indicators for low confidence, edited, and hidden states |
| `ChatBubble.tsx` | Read-only variant for active recording sessions (later merged into `EditableSegment` via `readOnly` prop) |
| `ChatView.tsx` | Segment list with auto-scroll, playback-synced active segment highlighting (`ring-2 ring-primary shadow-md scale-[1.02]`) |

### Resizable Panels

| File | Purpose |
|------|---------|
| `ui/resizable.tsx` | Wraps `react-resizable-panels` (`Group` → `ResizablePanelGroup`, `Panel` → `ResizablePanel`, `Separator` → `ResizableHandle` with grip icon) |

### New UI Primitives (shadcn)

Added: `command.tsx`, `context-menu.tsx`, `dialog.tsx`, `dropdown-menu.tsx`, `input.tsx`, `resizable.tsx`, `sheet.tsx`, `slider.tsx`, `textarea.tsx`, `tooltip.tsx`.

### Database Changes

6 SQLite migrations (up from 1):
- v2: folders, sessions.folder_id, is_pinned, pinned_at
- v3: Segment editing (original_text, edited_at, deleted_at, hidden)
- v4: notes + note_versions tables, sessions.session_type
- v5: sessions.wav_file_path, wav_duration_seconds
- v6: sessions.sort_order, shares table

Persistence moved from `sql.js` (WebAssembly) to `tauri-plugin-sql` (native SQLite via Tauri plugin).

### Store Changes

- Settings version bumped to 5 (added `theme: ThemeMode`)
- Navigation: `currentView`, `selectedSessionId`, `listFilter` replace simple session selection
- Folders: `folders`, `loadFolders`, `createFolder`, `renameFolder`, `deleteFolder`, `moveToFolder`
- Notes: `createManualNote` for standalone notes (no transcription)
- Playback: `playbackTime`, `isPlaying`, `setPlaybackTime`, `setIsPlaying`
- Segments: `editSegmentText`, `deleteSegment`, `toggleSegmentHidden`
- `MIN_BUFFER_SECONDS` increased from 60 to 300 (5 minutes of audio capture history)

### Bug Fixes

- **Audio playback**: `convertFileSrc()` asset URLs inaccessible in dev webview; replaced with custom `audio-stream://` protocol with range request support
- **Chat scroll**: `ChatView` `ScrollArea` required flex container parent; wrapped in `<div className="flex flex-col h-full min-h-0">`
- **Buffer size**: 60s ring buffer truncated recordings; increased to 300s (~28MB per buffer at 48kHz mono)
- **Hidden segments**: Changed from removing hidden segments to greying them out (opacity + line-through)
- **Active segment highlight**: Increased visibility from `ring-2 ring-ring` to `ring-2 ring-primary shadow-md scale-[1.02]`

### Frontend Dependencies Added

- Radix: context-menu, dialog, tooltip, dropdown-menu, slider
- Tiptap: react, starter-kit, extension-placeholder, pm
- react-resizable-panels, @dnd-kit/core, @dnd-kit/sortable, @dnd-kit/utilities
- cmdk@1, vaul

### Key Decisions

- **Notes-first model** — Sessions and manual notes unified under a single list view. Every session can have attached notes (via Tiptap rich text) alongside its transcript.
- **Split pane for completed transcriptions** — Transcript on the left, notes editor on the right, with a draggable `ResizableHandle` separator. Keeps both visible simultaneously.
- **Custom audio protocol** — Rather than working around `convertFileSrc()` cross-origin issues, a dedicated `audio-stream://` protocol provides reliable WAV serving with range request support for seeking.
- **Server-side waveform peaks** — Computing peaks in Rust avoids the need for `fetch()` + `AudioContext` in the browser, which would fail cross-origin.
- **5-minute buffer** — Increased from 60s to 300s. Memory impact (~28MB per buffer) is acceptable for a desktop app. Ensures recordings >1 minute are fully captured.
- **Hidden vs deleted segments** — Hidden segments are visually greyed out (opacity 40%, line-through) but remain visible, allowing users to review and unhide. Only explicit delete removes them.

---

## Phase 11 — Unified Error Types, Streaming WAV, Bug Fixes

Systematic improvements to error handling, a fix for sessions exceeding ring buffer capacity, and a UI bug fix.

### Unified CommandError

Replaced `Result<_, String>` across all Tauri commands with a structured `CommandError` tagged union.

| File | Change |
|------|--------|
| `commands/error.rs` (NEW) | `CommandError` enum with 6 kinds: `Audio`, `Transcription`, `NotInitialized`, `InvalidInput`, `NotFound`, `Internal`. `From` impls for `AudioError`, `TranscriptionError`, `std::io::Error`. |
| `commands/audio.rs` | All 9 commands migrated from `Result<_, String>` to `Result<_, CommandError>`. Added `tracing` logs. |
| `commands/transcription.rs` | All 6 commands migrated. Locks released before async I/O. |
| `commands/capture.rs` | All 6 commands migrated. Added `tracing` logs. |
| `commands/live_transcription.rs` | All 3 commands migrated. |

### Streaming Session WAV (fix: sessions > buffer capacity)

**Problem**: Session WAV audio was capped at ring buffer capacity (180s). For longer sessions, the beginning of the audio was lost because the circular buffer overwrites old samples.

**Fix**: Stream audio to a WAV file incrementally during the live transcription loop.

| File | Change |
|------|--------|
| `yapstack-audio/src/export.rs` | New `SessionWavWriter` struct: `new()`, `write_samples()`, `finalize()`, `duration_seconds()`. 3 tests. |
| `yapstack-audio/src/lib.rs` | Re-export `SessionWavWriter` |
| `commands/live_transcription.rs` | Added `session_id: Option<String>` to `LiveTranscriptionConfig`. `SessionWavState` struct. If `session_id` set: creates WAV at `$APP_DATA_DIR/audio/{session_id}.wav`, rewinds flush positions by backfill amount, flushes every 300ms in the poll loop (same lock as energy check), final flush + finalize on stop. Emits `"session-wav-ready"` event. |
| `stores/appStore.ts` | Passes `session_id: sessionId` to config. Removed `exportSessionWav` from stop handler. Added `onSessionWavReady` action. |
| `hooks/useLiveTranscriptionEvents.ts` | Added `"session-wav-ready"` event listener. |

### WhisperClient extraction

The live transcription loop now extracts the `WhisperClient` from shared state into a private `Arc<Mutex<Option<WhisperClient>>>` for zero-contention use. The client is returned to shared state on exit (even after panics via `AssertUnwindSafe`).

### Zero-allocation energy check

| File | Change |
|------|--------|
| `yapstack-audio/src/ring_buffer.rs` | New `rms_energy_since()` — computes RMS directly on the ring buffer without allocating a `Vec<f32>`. |
| `yapstack-audio/src/manager.rs` | New `peek_energy_rms()` — delegates to `rms_energy_since()` instead of snapshot + deinterleave. New `mic_write_pos()`, `system_write_pos()` convenience methods. |

### Bug fix: session title editing

`SessionHeaderV2` `handleSaveTitle` updated the DB and sidebar (`loadSessions()`) but didn't refresh `viewSession`, causing the title to visually revert. Fixed by calling `getSession()` to refresh `viewSession` in the store after saving.

### Key decisions
- **Stream WAV during live loop** — The ring buffer is SPSC and cannot be enlarged. Instead, piggyback on the existing 300ms poll loop to periodically extract new audio. Disk writes happen outside the AudioManager lock.
- **Session WAV flush in same lock as energy check** — `extract_since()` and `peek_energy_rms()` are combined in a single lock acquisition to avoid doubling contention.
- **`export_session_wav` kept as fallback** — Still useful for short sessions or re-export scenarios where the ring buffer hasn't wrapped.
- **Unified `CommandError`** — Tagged union enables structured frontend error handling. `From` impls provide ergonomic conversion from library errors.

---

## Phase 12 — AI Chat Tool Calling & Session Intelligence

Added OpenAI-compatible tool/function calling to the AI chat, enabling the AI to directly mutate session data (title, notes, pin state) with auto-apply and undo.

### Tool Registry (`ai-tools.ts`)

Modular architecture where each tool is a self-contained `ToolDefinition` with schema, executor, and undo handler. Adding a new tool = one `registerTool()` call. No changes needed in other files.

| File | Change |
|------|--------|
| `lib/ai-tools.ts` | **New.** Tool registry with `registerTool()`, `getRegisteredTools()`, `executeTool()`, `undoToolCalls()`. Three initial tools: `update_title`, `save_to_notes`, `pin_session`. Each captures previous state for undo. |
| `lib/ai.ts` | Added `StreamEvent` type and `streamChatWithTools()` async generator. Updated `assembleTranscriptContext()` to include segment IDs (`[seg:ID ts]`). Added `marked` for markdown→HTML (replaced hand-rolled parser). `buildMessages()` accepts optional `sessionMeta`. |
| `lib/ai-prompts.ts` | Rewrote all action directives to be tool-aware. `summarize` and `meeting-minutes` instruct AI to call `update_title` + `save_to_notes`. `key-points`/`action-items` append to notes. `clean-transcript` explicitly avoids tools. Added `getSystemPromptWithToolContext()` with session metadata. Added citation instruction (`[[seg:ID]]` format). Prompts use `**bold**` not `##` headings for Tiptap compat. |
| `components/FloatingChatBar.tsx` | Replaced `streamChat()` with `streamChatWithTools()`. Handles `tool_calls` events, executes tools, prepends `[tool:name]` badge lines. Undo system with 10s toast window. Refreshes `viewSession` + sidebar + notes after tools. New props: `segments`, `onCitationClick`. |
| `components/AIChatMessage.tsx` | Parses `[tool:name]` prefix lines into badge pills. Parses `[[seg:ID]]` into clickable citation chips with tooltip. Hides empty message bubble when only badges present. New props: `segments`, `onCitationClick`. |
| `components/NoteDetailView.tsx` | Passes `segments` and `handleCitationClick` to both `FloatingChatBar` instances. Moved `segments` declaration before hooks to fix use-before-declaration. |
| `components/SessionHeaderV2.tsx` | Added `useRef`-based sync so `titleText` updates when `session.title` prop changes externally (after AI title update). |

### Key decisions

- **Single-turn tool calling** — Tools execute locally without sending results back to the model. Avoids extra API round-trip. Works because our tools are simple mutations.
- **Badge lines in message content** — `[tool:name] detail` prefix lines persist in DB. Simple, survives reload, no schema change needed.
- **`marked` over hand-rolled parser** — The custom `markdownToBasicHtml` function couldn't handle all markdown patterns (mixed headings + lists in same block, nested inline formatting). `marked` handles everything correctly.
- **Bold labels over headings in prompts** — Tiptap's StarterKit renders `<h1>`/`<h2>` disproportionately large. Prompts explicitly instruct `**bold**` for section labels.
- **Segment IDs in transcript context** — `[seg:ID timestamp]` format enables citations. The AI is instructed to cite selectively with `[[seg:ID]]` markers.

### Dependencies added

- `marked` — Markdown to HTML conversion

---

## Phase 13 — Keyboard Shortcuts

Added in-app and global keyboard shortcuts with a customizable registry.

### What was built

| File | Purpose |
|------|---------|
| `lib/shortcuts.ts` | Shortcut registry: `SHORTCUTS` array (15 shortcuts across 3 categories: Recording, Navigation, Editor), `SHORTCUT_MAP` for fast lookup, `getBinding()` with override support, `eventToBinding()`/`eventToGlobalBinding()` for capture |
| `hooks/useKeyboardShortcuts.ts` | In-app shortcuts via capture-phase `keydown` listener (mounted in `AppLayout`). Skips when input/textarea/contenteditable focused or during shortcut capture mode. 11 in-app actions. |
| `hooks/useGlobalShortcuts.ts` | Global shortcuts via `@tauri-apps/plugin-global-shortcut` (mounted in `App.tsx`). 4 global shortcuts (new session, new session with backfill, stop recording, new note). Re-registers when bindings change. `focusWindow()` brings app to front. |
| `components/settings/ShortcutsTab.tsx` | Settings UI for viewing and rebinding shortcuts with capture mode |

### Custom events

Decoupled side effects via custom DOM events:
- `yapstack:toggle-chat` — Toggles the floating chat bar from anywhere
- `yapstack:confirm-delete-session` — Triggers delete confirmation dialog

### Store changes

- Settings version bumped from 5 → 9 (accumulated changes: v5→v6 `sidebarCollapsed`, v6→v7 `bufferMaxSeconds` + removed `backfillSeconds`, v7→v8 AI settings, v8→v9 `shortcutBindings`)
- Added `shortcutBindings: Record<string, string>` override map to settings

### Key decisions
- **In-app vs global split** — In-app shortcuts use `mod+key` format (capture phase). Global shortcuts use Tauri's `CmdOrCtrl+Key` format via the global-shortcut plugin.
- **`shortcutCaptureActive` shared flag** — Prevents shortcuts from firing while the user is rebinding in settings.
- **Custom events for decoupling** — `yapstack:toggle-chat` allows the keyboard shortcut hook to trigger FloatingChatBar without importing it or passing callbacks through the component tree.

### Dependencies added
- `@tauri-apps/plugin-global-shortcut`

---

## Phase 14 — AI Context Refactor + Hallucination Fixes

Two parallel improvements: modular AI context system enabling multi-session chat, and sidecar-level hallucination reduction.

### AI Context Refactor

| File | Purpose |
|------|---------|
| `lib/ai-context.ts` | `ContextSource`, `AIContextTools`, `SystemPromptBuilder`, `AIContextValue` types. Factory functions: `createSessionSources()`, `createSessionTools()`, `createSessionSystemPromptBuilder()`, `createMultiSessionSources()`, `createMultiSessionTools()`, `createMultiSessionSystemPromptBuilder()`. |
| `lib/ai-actions.ts` | `ActionDefinition` type, `ACTIONS` array (summarize, key-points, action-items, meeting-minutes), `getAction()`, `getActionIcon()` |
| `components/AIContextProvider.tsx` | React context provider wrapping `FloatingChatBar`. Manages source toggle state. `useAIContext()` hook. |
| `lib/db.ts` | `getChatMessages`/`deleteChatMessages` now take `contextKey` (not `sessionId`). `insertChatMessage` uses `context_key` field. New `getNotesForSessions()`. |
| `components/FloatingChatBar.tsx` | Consumes `AIContextValue` via `useAIContext()` instead of receiving individual props |

### Database changes

- **v8**: `chat_messages` — added `context_key TEXT NOT NULL`, made `session_id` nullable, added `idx_chat_messages_context(context_key, created_at)` index
- **v9**: `folders` — added `icon`, `color`, `description` columns. New `session_folders` junction table (many-to-many with unique constraint and indexes)

### Hallucination Fixes

| File | Change |
|------|--------|
| `yapstack-sidecar/src/main.rs` | Whisper params: `greedy(best_of=3)` (was 1), `no_context(true)`, `logprob_thold(-1.0)` (was -0.5), `max_tokens(100)` safety net. Added `normalize_for_repetition()` for punctuation-aware repetition detection. Expanded from ~6 to 47 filler hallucination patterns. Promoted filter logging to `info!`. Added Silero VAD integration via `WhisperVadParams`. |
| `crates/yapstack-transcription/src/model.rs` | VAD model management: `vad_model_path()`, `download_vad_model()`, `ensure_vad_model()`. Constants: `VAD_MODEL_FILENAME` (`ggml-silero-v6.2.0.bin`), `VAD_MODEL_URL`, `VAD_MODEL_SIZE_BYTES` (~885KB). |
| `crates/yapstack-transcription/src/whisper.rs` | `WhisperClient::spawn()` signature updated to accept `vad_model_path: Option<&Path>`. Passes `--vad-model` arg to sidecar when provided. |
| `commands/transcription.rs` | `init_whisper_client` now calls `ensure_vad_model()` before spawning, passes VAD model path to `WhisperClient::spawn()`. |

### Key decisions

- **Composable context via factory functions** — Session and multi-session contexts use the same `FloatingChatBar` component but different sources/tools/prompts assembled by factory functions. Adding a new context type only requires new factory functions in `ai-context.ts`.
- **`context_key` decouples chat from session_id** — Enables folder-level chat, "all sessions" chat, and "pinned" chat without schema gymnastics. Chat messages keyed by context string rather than FK.
- **Silero VAD as whisper.cpp preprocessing** — Different layer than the app-level `SourceVadState` in `live_transcription.rs`. App-level VAD chunks audio for Whisper. Silero VAD (inside whisper.cpp) skips non-speech within a chunk before decoding.
- **`no_context: true`** — Prevents whisper.cpp from carrying internal context between segments, which was a source of hallucination repetition across chunks.
- **Punctuation-normalized repetition detection** — `normalize_for_repetition()` inserts spaces around punctuation so "Yeah.Yeah.Yeah." is correctly detected as repetition.

---

## Phase 15 — General Settings + Voice Dictation

Added general settings tab, hold-to-talk voice dictation with dynamic slots, and the `YapStackIcon` component.

### General Settings

| File | Purpose |
|------|---------|
| `components/settings/GeneralTab.tsx` | Theme selection (light/dark/system), custom audio save location (folder picker + reveal), clear all sessions with confirmation |

Moved theme from inline settings into a dedicated General tab. Added `audioSaveLocation` to settings (v10 migration).

### Voice Dictation

| File | Purpose |
|------|---------|
| `commands/dictation.rs` | `clipboard_paste` Tauri command — clipboard write via `pbcopy`/`clip`, optional auto-paste via `osascript` keystroke simulation |
| `hooks/useDictation.ts` | Dictation lifecycle hook — state machine (idle → recording → transcribing → processing → done), controls bubble window, routes output |
| `components/DictationBubble.tsx` | Transparent always-on-top window showing dictation state (red=recording, blue=transcribing, purple=processing, green=done) |
| `components/settings/DictationTab.tsx` | Dynamic slot management — add/delete slots, configure name, keybind, output action, AI prompt per slot |
| `components/YapStackIcon.tsx` | SVG mask-based icon component using `bg-current` + CSS mask, used in DictationBubble |

### How dictation works

1. User presses and holds a global keybind → `useGlobalShortcuts` dispatches `yapstack:dictation-start` with `{ slotId }`
2. `useDictation` validates (enabled, engine ready, not in active recording), shows bubble, starts timing
3. User releases key → `yapstack:dictation-stop` dispatched
4. Hook captures audio via `triggerInstantCapture()`, transcribes via `transcribeAudio()`
5. If `slot.aiEnabled && slot.prompt`: post-processes transcription with AI API
6. Routes output based on `slot.outputAction`:
   - `"paste"`: clipboard + auto-paste via osascript
   - `"clipboard"`: clipboard only
   - `"new-note"`: creates manual session with notes, opens in main window

### App.tsx restructuring

Split into `MainApp` (main window with all hooks) and `App` (router). `App` checks `?window=dictation` query param — renders `DictationBubble` for the dictation window, `MainApp` for the main window. This avoids React rules-of-hooks violations from conditional hook calls.

### Dictation window configuration (tauri.conf.json)

```json
{
  "label": "dictation",
  "url": "index.html?window=dictation",
  "width": 220, "height": 64,
  "visible": false, "resizable": false, "decorations": false,
  "transparent": true, "alwaysOnTop": true, "skipTaskbar": true,
  "focus": false, "shadow": false
}
```

### Store changes

- Settings persist version: 9 → 12
- v9→v10: `audioSaveLocation: string | null`
- v10→v11: `dictation: DictationSettings` (enabled + slots array)
- v11→v12: Added `outputAction` to `DictationSlot` (default `"paste"`)
- Default: 1 slot ("Raw Dictation", paste output, no AI)

### Key decisions

- **Separate Tauri window for bubble** — Avoids DOM layering issues, enables always-on-top positioning outside the main window bounds, and transparent rendering without affecting main window layout.
- **Hold-to-talk (not toggle)** — Global shortcut Pressed/Released states map naturally to start/stop. More intuitive for dictation than toggle-on/toggle-off.
- **Dynamic slots (not fixed count)** — Users can add/delete slots with different keybinds, AI prompts, and output actions. No hardcoded slot limit.
- **`WebviewWindow.getByLabel()` is async** — Tauri v2 change from v1. Must `await` it in `useDictation`.
- **Keybinds in existing `shortcutBindings` map** — Dictation keybinds use keys `global.dictation-{slotId}` in the same override map as other shortcuts. No separate storage needed.
- **V1 limitation: unavailable during active recording** — WhisperClient is exclusively held by live transcription. Dictation requires the client for `transcribeAudio()`. Future: could use a second WhisperClient instance.
- **No new npm deps** — Reuses existing `@tauri-apps/plugin-global-shortcut` already added for keyboard shortcuts.

---

## Phase 16 — macOS Desktop UX Polish

Added system tray enhancements, close-to-minimize, floating recording indicator overlay, and build hardening.

### System Tray

| File | Change |
|------|--------|
| `src-tauri/src/lib.rs` | Rewrote `build_tray_menu(app, is_capturing, is_recording)` with dynamic items. Dedicated monochrome PNG tray icon via `Image::from_bytes(include_bytes!(...))` + `icon_as_template(true)`. Recording state tracked in 500ms polling loop. New event handlers: `open`, `new_session`, `new_session_bf_*`, `stop_session`. `show_main_window()` helper. |
| `src-tauri/icons/tray-icon.png` | 44×44 YapStack logo, black on transparent (macOS template icon) |
| `src-tauri/icons/tray-icon@2x.png` | 88×88 retina version |
| `src-tauri/Cargo.toml` | Added `image-png` feature to tauri for PNG icon loading |

Menu structure: Open YapStack → separator → Status (Idle/Listening/Recording) → Start/Stop Listening (disabled during recording) → separator → New Session + backfill submenu (30s, 1m, 2m, 5m, All) → [Stop Session] → separator → Quit YapStack.

### Close-to-Minimize

| File | Change |
|------|--------|
| `src/App.tsx` | `onCloseRequested` handler: `event.preventDefault()` → save position → `appWindow.hide()` |

Cmd+Q exits normally (OS-level, bypasses `closeRequested`). Tray "Quit" calls `app.exit(0)`.

### Recording Indicator

| File | Purpose |
|------|---------|
| `src/components/RecordingIndicator.tsx` | Transparent 56×120 overlay with pulsing YapStack icon (red ring) + `GripVertical` drag handle. Click icon → emits `recording-indicator:open-main`. |
| `src/hooks/useRecordingIndicator.ts` | Manages show/hide based on `activeSessionId` + `showRecordingIndicator` setting + main window focus. Positions at middle-right on first show. Listens for click events → navigates to active session + focuses main window. |
| `src-tauri/tauri.conf.json` | Added `recording-indicator` window (56×120, transparent, alwaysOnTop, no decorations/shadow, skipTaskbar, no focus) |

### Tray Events

| File | Purpose |
|------|---------|
| `src/hooks/useTrayEvents.ts` | Listens for `tray:new-session`, `tray:new-session-all`, `tray:stop-session` events. Guards on engine/capture state. Mounted in AppLayout. |

### Window Routing

`App.tsx` routes based on `?window=` URL param:
- `dictation` → `DictationBubble`
- `recording-indicator` → `RecordingIndicator`
- default → `MainApp`

### Settings

- Added `showRecordingIndicator: boolean` (default `true`) to Settings
- Persist version 12 → 13 with migration
- Toggle in `GeneralTab` ("Recording Indicator" / "Show floating indicator when recording and app is not focused")

### Build Hardening

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | `[profile.release]`: `strip = "symbols"`, `lto = "thin"`, `codegen-units = 1`, `panic = "abort"` |
| `vite.config.ts` | `sourcemap: false`, `esbuild.drop: ["console", "debugger"]` (production only), `legalComments: "none"` |
| `scripts/build-sidecar.sh` | `strip` binary after copy (belt-and-suspenders) |

### App Icons

Regenerated all icon sizes — white YapStack logo on zinc-900 (#18181b) background with 22% corner radius. Generated `.icns` via `iconutil` and `.ico` manually.

### Key Decisions

- **Dedicated tray icon PNG** — `app.default_window_icon()` with `icon_as_template(true)` renders as a white square because the app icon has no transparency. A separate `tray-icon.png` with alpha transparency is required for macOS template icons.
- **`image-png` feature** — Required on the `tauri` crate to parse PNG bytes via `Image::from_bytes()`.
- **Close-to-minimize** — Standard macOS desktop pattern. Users expect the tray to keep the app alive. Window position is saved to localStorage before hiding.
- **Cross-window events for recording indicator** — The indicator is a separate Tauri window (no shared Zustand store). Communication uses Tauri's `emit`/`listen` event system.
- **`CaptureStateDto` is an enum** — Comparison uses `matches!()` macro, not string equality.

---

## Phase 18 — Frontend Test Infrastructure

Added comprehensive frontend test coverage: 149 tests across 16 files covering library utilities, components, and the app root.

### Test infrastructure files

| File | Purpose |
|------|---------|
| `vitest.config.ts` | jsdom environment, `@/` path alias, setup file |
| `src/test/setup.ts` | `@testing-library/jest-dom` matchers + `ResizeObserver` polyfill |
| `src/test/tauri-mocks.ts` | Factory functions for all Tauri API mocks (core, event, window, dpi, webview, SQL, commands) |
| `src/test/match-media.ts` | `window.matchMedia` polyfill for jsdom |

### Test files created

**Library tests (7 files)**:
- `lib/utils.test.ts` — `cn()`, `formatShortcutDisplay()`, `formatTimestamp()`, `formatDuration()`
- `lib/shortcuts.test.ts` — Registry lookup, binding overrides, event conversion, shortcut categories
- `lib/folder-tree.test.ts` — Tree building, nesting, sorting, edge cases
- `lib/ai.test.ts` — Chat streaming, transcript context assembly, message building
- `lib/ai-prompts.test.ts` — System prompt construction, action directives, tool context
- `lib/ai-actions.test.ts` — Action registry, icons, action lookup
- `lib/ai-tools.test.ts` — Tool registration, execution, undo, schema validation

**Component tests (8 files)**:
- `components/AudioPlayer.test.tsx` — Play/pause, speed control, time display
- `components/EditableSegment.test.tsx` — Rendering, editing, context menu, confidence display
- `components/EmptyState.test.tsx` — Empty state messaging
- `components/FolderDialog.test.tsx` — Create/rename folder dialog
- `components/RecordingBeacon.test.tsx` — Pulsing indicator visibility
- `components/RecordingControls.test.tsx` — Recording controls state
- `components/SetupBanner.test.tsx` — Engine status banners
- `components/TrialExpiredOverlay.test.tsx` — Trial expiry overlay

**App test (1 file)**:
- `App.test.tsx` — Root routing by `?window=` param

### Root script updates

| Script | Command |
|--------|---------|
| `pnpm test` | `cargo test --all && pnpm --filter @yapstack/desktop test` |
| `pnpm test:frontend` | `pnpm --filter @yapstack/desktop test` |
| `pnpm test:rust` | `cargo test --all` |
| `pnpm test:watch` | `pnpm --filter @yapstack/desktop test:watch` |
| `pnpm lint` | `cargo fmt --all -- --check && cargo clippy --all -- -D warnings && pnpm --filter @yapstack/desktop lint` |
| `pnpm check` | Full CI gate: Rust build + test + fmt + clippy + TS typecheck + ESLint + vitest |

### Dev dependencies added
- `vitest` — Test runner
- `@testing-library/react` — React component testing utilities
- `@testing-library/user-event` — User interaction simulation
- `@testing-library/jest-dom` — Custom DOM matchers
- `jsdom` — Browser environment for tests

### Key decisions

- **Factory mock pattern** — Tauri mocks are factory functions (`tauriCoreMock()`, etc.) rather than side-effect imports. `vi.mock()` calls are hoisted above all imports by vitest, so the mock module must already be importable at hoist time. Factory functions return fresh mock objects per test file, avoiding shared state leaks.
- **Store injection via `useAppStore.setState()`** — Components read from Zustand; tests set state directly rather than mocking the store module. Simpler, tests real selector logic.
- **Module-level mocks for AI** — `ai.test.ts` and `ai-tools.test.ts` mock the OpenAI SDK and store at module level because these modules create clients at import time.
- **`ResizeObserver` polyfill** — jsdom doesn't implement `ResizeObserver`, which `react-resizable-panels` requires. A no-op polyfill in `setup.ts` prevents errors.

---

## Phase 19 — Prompt Context Decay

Added silence-triggered prompt decay to prevent Whisper hallucination after extended pauses. After 5+ seconds of silence across all sources, brief ambient noise (mousepad, cough, desk bump) caused Whisper to hallucinate words from the previous conversation because `shared_prompt` — the rolling 350-char context window — persisted indefinitely. Industry standard approach: clear prompt context after N seconds of all-source silence.

### What was built

| File | Change |
|------|--------|
| `commands/live_transcription.rs` | Added `prompt_decay_silence_seconds: Option<f32>` to `LiveTranscriptionConfig` (default 5.0, 0 to disable). Input validation. Extracted `check_prompt_decay()` helper function (testable without async loop). Two new state vars in live loop: `all_silent_since: Option<Instant>`, `prompt_seeded_from_backfill: bool`. Backfill seeding guard prevents re-seeding stale backfill data after decay clears the prompt. Decay check runs every 300ms after per-source VAD loop. 5 unit tests. |
| `stores/appStore.ts` | Added `promptDecaySilenceSeconds: number` to Settings (default 5). Persist version 14 → 15 with migration. Config construction passes `prompt_decay_silence_seconds` (null when 0 to disable). |
| `components/settings/TranscriptionTab.tsx` | New "Prompt Decay" `ButtonGroupSetting` in Advanced section: Off, 3s, 5s, 10s. |

### How it works

1. Every 300ms poll cycle, after per-source VAD processing, `check_prompt_decay()` checks if all sources are silent
2. If any source is speaking, `all_silent_since` resets to `None`
3. If all silent, `all_silent_since` records when silence began (via `get_or_insert_with`)
4. Once silence exceeds `prompt_decay_silence_seconds` and `shared_prompt` is non-empty, the prompt is cleared
5. When speech resumes, the prompt rebuilds naturally from new transcription via the existing `shared_prompt.push_str(&result.text)` in `transcribe_chunk`
6. Backfill seeding is one-shot: `prompt_seeded_from_backfill` flag prevents re-seeding from the bridged prompt after decay has cleared `shared_prompt`

### Key decisions

- **Silence-triggered reset (not gradual decay)** — Matches industry standard in WhisperX, whisper_streaming, and whisper.cpp application layers. Simpler and equally effective — Whisper's decoder either gets context or doesn't.
- **5s default** — Conservative enough to not clear context during normal speech pauses (typical 0.5-2s) but catches the problematic case of long silences followed by ambient noise.
- **Testable helper** — `check_prompt_decay()` is a pure function (modulo `Instant::now`) that can be unit tested without the async live transcription loop.
- **One-shot backfill seeding** — Without the `prompt_seeded_from_backfill` guard, a cleared prompt would be immediately re-filled from stale backfill context, defeating the purpose of decay.

---

## Phase 20 — Audio Pipeline Optimization

Expert review of the transcription pipeline identified suboptimal Whisper decoder parameters.

### What was built

| File | Change |
|------|--------|
| `crates/yapstack-sidecar/src/main.rs` | `best_of: 3` → `1` (deterministic at temp 0, all 3 candidates identical — wasted ~2x decoder time). `temperature_inc: 0.0` → `0.2` (enables whisper.cpp temperature fallback: retries at 0.2, 0.4… when initial decode produces high-entropy or low-logprob output). Fixed debug log from hardcoded values to actual dynamic `best_of`/`max_tokens`/`single_segment`. |
| `CLAUDE.md` | Updated whisper-rs params, removed overlap docs, added per-source accumulated_text and prompt decay details. |
| `docs/ARCHITECTURE.md` | Fixed resampler description (linear → sinc), updated Whisper params, removed overlap references, added per-source prompt details. |
| `docs/API_REFERENCE.md` | Fixed all stale Whisper param values (best_of, temperature_inc, no_speech_thold, single_segment, max_tokens), fixed resampler description. |

### Key decisions

- **best_of:1 + temperature_inc:0.2 are complementary** — Initial decode is still deterministic (temp 0), so `best_of:1` is correct. `temperature_inc:0.2` only kicks in as a fallback when whisper.cpp detects bad output (high entropy or low logprob). Temperature progression replaces `best_of` as the diversity mechanism but only fires when needed.
- **Documentation sweep** — Removed overlap references (never implemented), corrected "linear interpolation" to "sinc interpolation" across all docs, added per-source `accumulated_text` prompting details that were missing.

---

## Phase 21 — FE Refactor + Cross-Platform Overlay Windows + Tests

Frontend decomposition, macOS NSPanel integration for overlay windows, dead code cleanup, DB type improvements, and expanded test coverage.

### Cross-Platform Overlay Windows

| File | Change |
|------|--------|
| `apps/desktop/src-tauri/src/commands/mod.rs` | New `show_overlay_panel` and `hide_overlay_panel` Tauri commands. cfg-gated: macOS uses `tauri-nspanel` `ManagerExt::get_webview_panel()` on the main thread; non-macOS falls back to `WebviewWindow` show/hide. Also added `get_autostart_enabled` / `set_autostart_enabled` via `tauri-plugin-autostart`. |
| `apps/desktop/src-tauri/src/lib.rs` | `tauri-nspanel` plugin init (macOS only). `tauri_panel!` macro defines `OverlayPanel` type. On setup, dictation + recording-indicator windows converted to NSPanels with `PanelLevel::MainMenu`, non-activating style mask, stationary + `move_to_active_space` + `full_screen_auxiliary` collection behavior. |
| `apps/desktop/src-tauri/tauri.conf.json` | Added `visibleOnAllWorkspaces: true` to dictation and recording-indicator windows (non-macOS fallback). |
| `apps/desktop/src/hooks/useDictation.ts` | `showBubble()` / `hideBubble()` now use `commands.showOverlayPanel("dictation")` / `commands.hideOverlayPanel("dictation")` instead of direct window show/hide. |
| `apps/desktop/src/hooks/useRecordingIndicator.ts` | Show/hide uses `commands.showOverlayPanel("recording-indicator")` / `commands.hideOverlayPanel("recording-indicator")`. |

### FloatingChatBar Decomposition

| File | Purpose |
|------|---------|
| `hooks/useChatMessages.ts` | Extracted chat message lifecycle: send, stream, tool execution, undo (10s timeout), DB persistence. Returns `{ messages, isStreaming, undoState, handleSend, handleUndo, handleClearChat }`. |
| `components/chat/ChatInputBar.tsx` | Extracted input UI: textarea auto-resize, file attachments (500KB limit), actions popover, keyboard shortcuts. |
| `components/chat/ContextPill.tsx` | Toggleable context source pill for transcript/notes. |
| `components/chat/ModelPickerPill.tsx` | Popover-based model selector grouped by provider. |
| `components/FloatingChatBar.tsx` | Now a thin shell — delegates to `useChatMessages` + `ChatInputBar`. |

### Dead Code Removal

Deleted components: `EmptyState.tsx`, `SessionDetailView.tsx`, `SessionSidebar.tsx`.

Removed unused DB functions from `lib/db.ts`: `listPinnedSessions`, `updateSessionFolder`, `listSessionsByFolder`, `renameFolder`, `getSessionFolderIds`.

### DB Type Improvements

Added `SessionStatus` (`"recording" | "completed"`) and `SessionType` (`"manual" | "recording"`) string literal type aliases in `lib/db.ts`, replacing raw strings in `DbSession`.

### Test Coverage Expansion

6 new/expanded test files (21 total, 275 tests, up from 16 files / 149 tests):

| File | Tests | Notes |
|------|-------|-------|
| `components/AIChatMessage.test.tsx` | 15 | Tool badges, citation chips, markdown rendering |
| `components/NoteCard.test.tsx` | 14 | Card rendering, pin state, session types |
| `hooks/useClickOutside.test.ts` | 5 | Click outside detection hook |
| `lib/ai-context.test.ts` | 14 | Context factory functions, source assembly |
| `lib/analytics.test.ts` | 26 | Event tracking, fire-and-forget guarantees |
| `lib/utils.test.ts` | 39 | Expanded from 17 — added more edge cases |

New shared test utilities in `test/helpers.ts`: factory functions `makeSession()`, `makeSegment()`, `makeFolder()`, `makeChatMessage()` with sensible defaults.

### Key Decisions

- **NSPanel for overlay windows** — macOS NSPanels don't steal focus, appear above full-screen apps, and move with the active space. Standard `alwaysOnTop` windows can't achieve all three on macOS. The `visibleOnAllWorkspaces` tauri.conf.json property provides the non-macOS equivalent.
- **Platform-agnostic commands** — `show_overlay_panel` / `hide_overlay_panel` abstract the platform difference behind cfg-gating. Frontend code doesn't need to know about NSPanels.
- **Chat hook extraction** — `useChatMessages` encapsulates all streaming/tool/undo/DB logic so `FloatingChatBar` becomes a pure UI shell. Easier to test and reason about.
- **Test helpers** — Factory functions (`makeSession`, etc.) with sensible defaults reduce boilerplate across test files and ensure consistency.

---

## Phase 22 — Dictation History + AI Context Refactor

### Dictation History Persistence
- DB migration v10: `dictation_history` table with indexes (created_at, slot_id)
- Frontend CRUD: `insertDictationHistory`, `listDictationHistory`, `getDictationHistory`, `deleteDictationHistory`, `clearDictationHistory`, `updateDictationHistorySessionId` in `lib/db.ts`
- Store: `dictationHistory` state with `loadDictationHistory`, `deleteDictationHistoryEntry`, `clearDictationHistoryEntries` actions

### Dictation Activation Modes
- Settings v16: `activationMode: "hold" | "toggle"` added to `DictationSettings`
- Toggle state machine in `useGlobalShortcuts` via module-level `toggleActiveSlots: Set<string>`
- No-input detection (3s timer → yellow pulsing bubble with `"no-input"` state)
- `yapstack:dictation-idle` custom event for toggle mode cleanup

### Dictation History UI
- `DictationHistoryList`: entries grouped by day, "Clear All" button with confirmation
- `DictationHistoryCard`: badges (slot name, AI, output action), WAV playback, context menu (delete, move to note)
- `AppSidebar`: "Dictation" navigation button sets `ListFilter { type: "dictation" }`
- `NoteCardList` routes to `DictationHistoryList` when filter is dictation

### AI Context Refactor
- `ListChatContext` type and `resolveListContext()` factory centralizes context resolution for all `ListFilter` types
- `ListContextBar` component extracted from `AppLayout` (reduced ~90 lines of duplicated context setup)
- Dictation chat context: `createDictationSources`, `getDictationSystemPrompt`, `assembleDictationContext`
- Source ID disambiguation: `"notes"` → `"session-notes"` for multi-session contexts to avoid key collisions
- `AIContextProvider`: added `placeholder` prop for context-specific input hints
- `useChatMessages`: keying by `source.id` (not `source.type`) for correct context part assembly

### Analytics
3 new events: `dictation_history_cleared`, `dictation_history_entry_deleted`, `dictation_moved_to_note`

### Tests
- `ai-context.test.ts`: +5 tests for `resolveListContext` (all 4 context types + pluralization)
- Frontend total: 275 → 294 tests

---

## Phase 23 — Stream Health Monitoring & Auto-Restart

cpal audio streams can silently die (device disconnect, macOS sleep/wake, Bluetooth headphone switching) while `is_running` stays true. The only symptom is that `write_pos` freezes, causing the live transcription loop to produce no new segments.

### What was built

| File | Change |
|------|--------|
| `yapstack-audio/src/stream.rs` | `build_capture_stream()` accepts `stream_error: &Arc<AtomicBool>`. cpal error callback stores `true` with `Release` ordering. |
| `yapstack-audio/src/mic.rs` | Added `stream_error: Arc<AtomicBool>` field. `has_stream_error()` public API. Error flag reset before each `start()`. |
| `yapstack-audio/src/system/mod.rs` | Same pattern: `stream_error` field, `has_stream_error()`, reset on start. |
| `yapstack-audio/src/manager.rs` | `mic_has_stream_error()`, `system_has_stream_error()` delegate to captures. `restart_mic(device_name)`, `restart_system_audio()` stop old stream and start new one on the existing ring buffer. 4 new tests. |
| `commands/live_transcription.rs` | `StreamHealthEvent` DTO. `check_stream_health()` async function called each poll iteration. Constants: `STREAM_STALL_THRESHOLD_SECS = 2.0`, `STREAM_RESTART_MAX_ATTEMPTS = 3`. `SourceVadState` extended with `last_seen_write_pos`, `last_write_pos_advance`, `restart_attempts`. |
| `src/lib/events.ts` | `STREAM_HEALTH` event constant + `StreamHealthEvent` type. |
| `src/hooks/useLiveTranscriptionEvents.ts` | `stream-health` listener with toast notifications (success/error) and analytics tracking. |
| `src/lib/analytics.ts` | `trackStreamHealthEvent({ source, status })` — 1 new event. |

### How it works

1. **Layer 1 — cpal error flag** (instant): Each capture stream's cpal error callback sets an `Arc<AtomicBool>` flag. Checked via `mic_has_stream_error()` / `system_has_stream_error()`.
2. **Layer 2 — write_pos watchdog** (~2s latency): `SourceVadState` tracks `last_seen_write_pos` and `last_write_pos_advance`. If `write_pos` hasn't advanced for 2 seconds, the stream is considered stalled.
3. **Restart**: `AudioManager::restart_mic()` / `restart_system_audio()` stop the old (dead) stream and start a new one on the same ring buffer — no audio data is lost from the buffer. Up to 3 attempts per source; on success, `restart_attempts` resets to 0.
4. **Frontend**: Toast notifications for restart success/failure. `stream_health_event` analytics with source and status.

### Key decisions

- **Two detection layers** — cpal error callbacks catch some failures instantly, but not all (some platforms silently stop producing samples). The write_pos watchdog catches the rest with a 2s delay.
- **Buffer preservation** — Restart creates a new cpal stream writing to the same ring buffer. The monotonic `write_pos` continues from where it was. No audio data is lost and the live transcription loop resumes seamlessly.
- **3 restart attempts** — Prevents infinite restart loops if the device is truly gone. After 3 failures, the source is abandoned and the user sees a persistent error toast.
- **Default device on restart** — The restart uses the default device (not the original device name) because the original device may have been disconnected.

---

## Phase 24 — NVIDIA Parakeet engine + Sortformer diarization

Added Parakeet-TDT-0.6b-v3 (multilingual, 25 languages) as a first-class peer to Whisper, optional Sortformer speaker diarization, and ORT-CoreML / ORT-WebGPU acceleration paths. Previously Whisper was the only transcription backend; sessions had no concept of speakers.

### What was built

| File | Purpose |
|------|---------|
| `crates/yapstack-common/src/engines.rs` | New static `engine_catalogue()` (engine kinds × supported languages × capability flags). Whisper = 99 languages + initial-prompt; Parakeet = 25 European languages + diarization. |
| `crates/yapstack-common/src/types.rs` | New `EngineKind` enum (Whisper, Parakeet). `TranscriptSegment` gains `speaker_id: Option<u8>` (skipped on serialize when `None`). `SidecarRequest::Transcribe` gains `diarization: bool` (default false). All additive — old IPC payloads still deserialize. |
| `crates/yapstack-sidecar/src/engines/{mod,whisper,parakeet,sortformer}.rs` | Extracted Whisper from monolithic `main.rs` into a `TranscriptionBackend` trait. Concrete backends in `whisper.rs` (whisper-rs + Metal) and `parakeet.rs` (parakeet-rs 0.3.5 + ORT + Sortformer post-pass). Shared text helpers (`normalize_spacing`, `sanitize_text`, `should_include_segment`) moved into `engines/mod.rs`. `main.rs` shrank from 832 → ~280 lines, now a thin IPC dispatcher with `--engine whisper\|parakeet` arg parsing. |
| `crates/yapstack-sidecar/src/engines/parakeet.rs` | Wraps `ParakeetTDT` for transcription and `Sortformer` for diarization. Resamples to 16 kHz mono before model input (parakeet-rs requires it despite the docs implying auto-resampling). Groups word-level tokens into segments at 0.5 s silence gaps + 12 s soft cap. When `opts.diarization=true`, runs Sortformer on the same audio and assigns `speaker_id` by max-overlap. CoreML / WebGPU EPs selected via `YAPSTACK_PARAKEET_ACCEL` env var with CPU fallback on load failure. |
| `crates/yapstack-transcription/src/{whisper.rs → client.rs}` | File renamed via `git mv`. `WhisperClient` → `TranscriptionClient` (alias kept for transitional compat). New `spawn(sidecar, engine, model, vad_model, sortformer_model, coreml_cache_dir)` signature; `engine()` accessor; `transcribe_with(audio, language, prompt, diarization)` for per-call diarization control. |
| `crates/yapstack-transcription/src/model.rs` | Added `ParakeetVariant::TdtV3` (multi-file ONNX bundle: `encoder-model.onnx` + `encoder-model.onnx.data` + `decoder_joint-model.onnx` + `vocab.txt`, ~600 MB total) and `SortformerVariant::V2_1` (~50 MB single file). New `ModelManager` methods: `parakeet_model_dir`, `parakeet_is_available`, `download_parakeet`, `delete_parakeet`, `ensure_parakeet`, `sortformer_model_path`, `download_sortformer`, `ensure_sortformer`, `delete_sortformer`. Multi-file Parakeet downloads loop through the variant's `files()` list with per-file SHA-256 streaming. |
| `apps/desktop/src-tauri/src/db.rs` | New `ensure_runtime_schema()` that opens the SQLite DB directly via `rusqlite` at app startup and adds `segments.speaker_id INTEGER` if missing. Idempotent — silent no-op when present. **Lives outside the migration list** because some local dev DBs have a "ghost" v11 from another branch in `_sqlx_migrations`, which makes sqlx silently refuse to apply our v12. The startup hook sidesteps the entire ordering problem. |
| `apps/desktop/src-tauri/src/lib.rs` | New `init_tracing()` wires `tracing-subscriber` so all our `info!`/`warn!`/`error!` calls (and forwarded sidecar stderr) actually surface in `pnpm tauri dev`. Default filter: `info` for our crates + `warn` for noisy deps; `RUST_LOG` overrides. Calls `db::ensure_runtime_schema()` before tauri-plugin-sql initializes. |
| `apps/desktop/src-tauri/src/commands/transcription.rs` | New DTOs: `EngineKindDto`, `ParakeetVariantDto`, `SortformerVariantDto`, `EngineDescriptorDto`, `ParakeetModelInfoDto`, `SortformerModelInfoDto`. `TranscriptSegmentDto` gains `speaker_id`. New commands: `init_transcription_client(engine, whisper_model?, parakeet_variant?, enable_diarization)` (engine-aware peer to `init_whisper_client`), `get_engine_catalogue`, `get_parakeet_models`, `download_parakeet_model`, `delete_parakeet_model`, `get_sortformer_status`, `download_sortformer_model`, `delete_sortformer_model`. Computes the CoreML cache dir under `$APP_DATA_DIR/cache/coreml/` and forwards on Parakeet init. |
| `apps/desktop/src-tauri/src/commands/live_transcription.rs` | `LiveTranscriptionConfig` gains `diarization: bool`. Live loop now branches on `client.engine()`: Parakeet calls drop `initial_prompt` (TDT decoder has no text-prompt input); Whisper unchanged. Per-segment `speaker_id` propagates into `LiveSegmentEvent` so the frontend can group by speaker. |
| `apps/desktop/src/stores/appStore.ts` | Persist version 21 → 22 migration adds `selectedEngine: "Whisper"` (upgrade-safe default), `selectedParakeetVariant: "TdtV3"`, `diarizationEnabled: false`, and a per-session `speakerNames: Record<string, Record<number, string>>` map. New actions: `loadEngineCatalogue`, `refreshParakeetModels`, `refreshSortformerStatus`, `switchEngine` (auto-downloads target model if missing), `downloadParakeetModel`, `deleteParakeetModel`, `setDiarizationEnabled` (lazily downloads Sortformer + re-inits client), `setSpeakerName`. `autoSetup` branches on `settings.selectedEngine`. |
| `apps/desktop/src/components/settings/{TranscriptionTab,ParakeetModelSection}.tsx` | Engine radio (Whisper / Parakeet — peers, no Recommended badge), conditional model picker (existing `ModelSection` for Whisper, new `ParakeetModelSection` for Parakeet), language dropdown derived from the catalogue (clamps to "auto" when current selection isn't supported by the new engine), Switch-style **diarization toggle** greyed out unless engine supports it. |
| `apps/desktop/src/components/TranscriptSegments.tsx` | New wrapper component for `ChatView`. Falls back to a flat segment list when no segment carries a `speaker_id` (Whisper sessions render unchanged). Otherwise groups consecutive same-speaker segments under a `Speaker N` header with a 4-color palette, inline-editable name (persisted to per-session `speakerNames` Zustand map). |
| `apps/desktop/src/lib/{ai,ai-prompts,ai-context}.ts` | `assembleTranscriptContext(segments, speakerNames?)` adds `(SpeakerName)` prefix per segment when diarization data is present. New `transcriptHasSpeakers()` predicate. `getSystemPrompt(directive, ..., { hasSpeakers })` injects a `SPEAKER_INSTRUCTION` paragraph telling the model to attribute statements correctly. |
| `apps/desktop/src/lib/db.ts` | `DbSegment.speaker_id?: number \| null` (optional + nullable to keep test fixtures compatible). `insertSegment` writes to the new column with `?? null` coalescing. |
| `scripts/build-sidecar.sh` | Now builds with `whisper,parakeet` everywhere; adds `metal,coreml,webgpu` on Apple targets and `cuda` on Windows. Dev mode also mirrors the binary to `target/debug/yapstack-sidecar` so iterative `pnpm build:sidecar:dev` actually takes effect on the next sidecar respawn (without that, tauri-cli only copies the binary into `target/debug/` once at app build time). |

### Dependencies added

- `parakeet-rs = "0.3"` (sidecar, optional, with `sortformer` feature) — wraps NVIDIA Parakeet TDT v3 + Sortformer ONNX models via `ort`
- `ort` (transitive via parakeet-rs) — ONNX Runtime bindings; linked statically; CoreML.framework dynamically linked on Apple targets
- `rusqlite = "0.32"` with `bundled` (desktop) — used only by `ensure_runtime_schema()` to do an idempotent ALTER TABLE outside the tauri-plugin-sql migration system
- `tracing-subscriber` (desktop, was already in workspace) — finally initialised in `lib.rs::run()`; previously every `tracing` call in the desktop crate vanished into the void

### Key decisions

- **Engines as peers, not primary/fallback.** Whisper stays the upgrade-safe default for existing users, but the Settings UI presents Whisper and Parakeet side-by-side with no "Recommended" badge — users pick consciously.
- **Engine + language form a cascade.** The catalogue lives in `yapstack-common` so backend validation and the frontend dropdown are driven by the same source of truth. Switching engines clamps the language selection to one the new engine supports (or "auto").
- **Trait-based backend abstraction in the sidecar.** `TranscriptionBackend` trait keeps `main.rs` engine-agnostic. Adding a third engine in the future means: feature flag + `engines/foo.rs` + one `EngineKind` variant. Per-engine config (Whisper VAD model, Parakeet Sortformer + CoreML cache) flows through the constructor, not the trait.
- **Sortformer is a post-pass, not a separate IPC trip.** Diarization runs on the same audio buffer right after transcription and merges speaker IDs into the produced segments by max-overlap (samples → ms → segment range). Saves a second WAV write + IPC round-trip and keeps the chunked timing aligned.
- **`speaker_id` lives outside the migration list.** sqlx-style migrations check that every applied migration is also in the registered list (checksum verified). Some local dev DBs picked up a "ghost" v11 from another feature branch; sqlx silently skips any newer migration when an unknown applied version is present, leaving `segments.speaker_id` missing and every insert failing. `ensure_runtime_schema()` does the ALTER directly via `rusqlite` at startup — idempotent, no migration list needed, self-heals on every developer's machine regardless of prior local state.
- **CoreML preflight skip.** ORT-CoreML deterministically fails to load Parakeet TDT v3 because the model ships a 2.3 GB external `.onnx.data` initializer file and CoreML's compilation step loses the model_path context. We detect `*.onnx.data` files in the model dir and silently fall back to CPU under the Auto policy, avoiding a 600 ms doomed load attempt and a noisy `ERROR ort` line on every spawn. Power users can still force CoreML via `YAPSTACK_PARAKEET_ACCEL=coreml`.
- **Per-load CPU fallback at the backend level.** parakeet-rs's built-in `error_on_failure()` only catches *runtime* op failures, not load-time ones. We wrap `ParakeetTDT::from_pretrained` with a try/CPU-retry so any chosen accelerator (CoreML or WebGPU) that fails at load surfaces a clear `WARN` and the sidecar still returns a working model.
- **Resample on our side.** parakeet-rs 0.3.5 documents auto-resampling but actually rejects anything other than 16 kHz mono with `Audio sample rate X doesn't match expected 16000`. We resample via the existing `yapstack_common::audio::resample` helper before calling `transcribe_samples` (no-op when already 16 kHz).
- **`tracing-subscriber` was the silent failure.** The desktop crate had `tracing` calls everywhere but never installed a global subscriber, so every `info!`/`warn!`/`error!` (including forwarded sidecar stderr) vanished. Adding `init_tracing()` to `run()` was a one-line fix that unlocked all the diagnostic firepower we already had.

### Empirical RTFx (single Apple Silicon machine, Parakeet TDT v3, 2-13 s chunks)

| EP | Mean RTFx | Range | Notes |
|---|---|---|---|
| CPU | ~5-7× | 4-8× | Baseline, deterministic |
| WebGPU | ~6× | 4-10× | Similar mean to CPU, higher variance |
| CoreML | n/a | n/a | Doesn't load (external `.onnx.data` issue) |

For comparison, FluidAudio (Swift + CoreML + ANE) reportedly hits 155-237× realtime on the same model on the same hardware class. The path to a real (10×+) speedup is option #2 (data-inlining + CoreML CPUAndGPU) or option #5 (Swift sidecar with FluidAudio's ANE pipeline). Both deferred — current CPU performance is already sub-second per chunk for typical recording chunk sizes.

### What's *not* in this phase

- **Live diarization** — Sortformer runs once at transcribe time, not on a streaming buffer. Live transcription with diarization toggled on still works but processes each chunk independently, so the speaker ID for the same speaker may drift between chunks. Streaming Sortformer (`Sortformer::diarize_chunk`) exists in parakeet-rs and is a follow-up.
- **Multi-engine UI for the language dropdown** — currently shows the union, would be cleaner to show only the active engine's languages with a tooltip explaining the constraint.
- **CoreML on data-inlined models** — option #2 from the matrix; would require pre-processing the model on download to merge `.onnx.data` into the `.onnx` file.
- **Swift FluidAudio sidecar** — option #5; would actually use the Apple Neural Engine for 30-100× speedups but breaks the cross-platform single-Rust-sidecar story.

---

## Phase 25 — Silero VAD + Parakeet Tuning

Replaces the RMS energy detector with Silero V5 VAD for both engines and tightens Parakeet's chunk cadence to fit its low-RTFx, non-autoregressive decoder. Branch: `feat/silero-vad-parakeet`.

> Note: an earlier draft of this phase also bundled a sidecar priority scheduler from `feat/transcription-scheduler`, but that branch never merged into main. The scheduler eventually landed independently — see Phase 32.

### What was built

| File | Change |
|---|---|
| `apps/desktop/src-tauri/src/commands/silero_vad.rs` | New. `SileroVad` (shared `silero::Session`, V5 ONNX bundled in-binary via the `silero` crate, ~2 MB) + `SileroSource` (per-source `StreamState` + VAD-only read cursor + sticky `last_probability` for empty polls). `score_stream` returns every 32 ms frame's probability — not just the last — so intra-poll speech (onset+offset inside one batch) isn't lost. `score_all` for backfill batch-scoring. Resampling delegates to `yapstack_common::audio::resample` (rubato sinc). |
| `apps/desktop/src-tauri/src/commands/live_transcription.rs` | Replaced RMS (`peek_energy_rms`) with Silero per-source scoring. `VadTuning.silence_threshold` (RMS) renamed → `speech_threshold` (probability). Parakeet tuning: dialogue-aggressive defaults (100 ms poll, 250 ms pre-roll, 0.7 offset-hysteresis ratio); Parakeet's silence window has since been raised to 500 ms (`6d66b24`) to relieve queue pressure. Whisper keeps the user's `silence_duration_ms` (300 ms poll, no pre-roll). `vad_chunk_historical_audio` now batch-scores through Silero and walks a pure state machine (`backfill_chunks_from_probabilities`) over the probability stream; `prev_chunk_end` clamp prevents pre-roll from rewinding into a previous chunk. Backfill-to-live handoff resets all per-source Silero state (`read_pos`, `stream`, `last_probability`, `earliest_next_chunk_pos`) to the post-backfill write position. |

### Key decisions

- **One Silero session, two source streams**. The `silero::Session` is `Send` but not `!Sync`, so we feed mic and system sequentially inside each poll. Per-source `StreamState` keeps their LSTM memories independent. Building two sessions would fragment acceleration and double the model load.
- **Silero replaces RMS for both engines, but only the detector changes**. Whisper keeps its tuned timing values (`silence_duration_ms`, 300 ms poll, no pre-roll); only the speech signal is different. This was an explicit memory rule: don't retune Whisper when tuning Parakeet.
- **Thresholds are V5 defaults**. `speech_threshold = 0.5`, `offset_threshold_ratio = 0.7` (→ 0.35 end threshold). Matches the upstream Python silero-vad reference.
- **Reuse the canonical resampler**. `silero_vad.rs` calls `yapstack_common::audio::resample` (the same rubato-backed sinc path used by `yapstack-sidecar` and `yapstack-audio::mixer`) rather than shipping a hand-rolled decimator. Silero is robust to mild aliasing but consistency with the workspace idiom is worth the line count.
- **Pure state-machine helper for tests**. `backfill_chunks_from_probabilities` is extracted from `vad_chunk_historical_audio` so unit tests can feed hand-crafted probability sequences without loading the ONNX model — particularly important for the pre-roll-clamp regression test, since Silero won't classify synthetic tones as speech.

### What was learned

- **Intra-poll speech events are real**. A 100 ms Parakeet poll can contain an entire short utterance; if `score_stream` returns only the trailing probability (silence), the VAD state machine never sees the onset. The fix is straightforward — return `Vec<f32>` and iterate `poll_vad` per frame — but the bug is easy to miss in review.
- **The sidecar's stdin queue is FIFO**. Multiple concurrent `transcribe_with` calls don't pipeline on the model side; they queue in order received. That made "parallel" mic/system dispatch useless for prioritization and motivated the eventual scheduler in Phase 32.
- **Backfill-to-live handoff is more than just the extraction cursor**. Resetting `cursor` + `speech_start_pos` isn't enough; the Silero read cursor, the LSTM stream state, the sticky last_probability, and `earliest_next_chunk_pos` all have to move with them or the live detector replays the backfill window.
- **Pre-roll needs the same clamp in backfill as in live**. The live loop already bounded pre-roll by `earliest_next_chunk_pos`; the backfill chunker didn't, so two utterances separated by less than ~250 ms could produce overlapping chunks.

### Test coverage

- `commands::silero_vad::tests` — bundled ONNX session loads and returns valid probabilities for silence; 32 ms frame cadence (512 samples × N → N probabilities).
- `commands::live_transcription::tests` — Silero-era `poll_vad` state-machine tests (thresholds are now probabilities); intra-poll speech onset regression; backfill-reset structural guard; no-overlap regression for the `prev_chunk_end` clamp.

### Not yet done

- Optional: reuse the SincFixedIn resampler across polls instead of rebuilding on each call (rubato's recommended streaming pattern). Current cost is dominated by Silero inference, so not urgent.
- Optional: two-tier endpointing (soft at 200 ms for interim segments, confirmed at 600 ms for finals) — Deepgram's pattern. Would reduce perceived latency but requires an interim-segment protocol between sidecar and frontend.
- Optional: adaptive noise floor for environments where the 0.5 speech probability is regularly exceeded by non-speech (e.g. loud HVAC). Silero is robust to this in practice; revisit only if users report false triggers.

---

## Phase 26 — CoreAudio device listener + bulletproofing against cpal#1175

Users reported mid-session silent death of system-audio capture when toggling AirPods / Bluetooth output, and separately a false-positive drift error repeating every few seconds for users who had explicitly picked a non-default mic. Both traced to the same upstream gap: **cpal does not auto-reroute streams when the default device changes**, and its error callback does not fire for that case either.

### Upstream status to monitor

- **[cpal#1175](https://github.com/RustAudio/cpal/issues/1175)** — "default devices don't get automatically rerouted upon disconnection" (maintainer confirmed 2026-04-22, filed against cpal master post-[PR #1003](https://github.com/RustAudio/cpal/pull/1003)). No fix merged yet. Proposed direction is a new `DeviceChanged` error-callback variant.
- Related history: [cpal#704](https://github.com/RustAudio/cpal/issues/704), [cpal#1012](https://github.com/RustAudio/cpal/issues/1012), [cpal#1030](https://github.com/RustAudio/cpal/issues/1030).

**When cpal lands a fix, delete `yapstack-audio/src/system/device_watcher.rs` and simplify `check_stream_health` in `live_transcription.rs`** — the CoreAudio listener, the `device_list_changed` signal, the settle-and-verify loop, and the `RestartReport::same_device` retry path all exist only to work around cpal#1175.

### What was built

| Area | Mechanism |
|------|-----------|
| Push signal (primary) | `DefaultDeviceWatcher` registers CoreAudio property listeners on `kAudioHardwarePropertyDefaultInputDevice`, `kAudioHardwarePropertyDefaultOutputDevice`, and `kAudioHardwarePropertyDevices`. The device-list listener fires earliest during AirPods/Bluetooth handshake on some macOS versions. |
| Settle-and-verify | Before acting on a listener event, sleep 200 ms and re-query the current default. macOS can briefly revert to the old device during the handshake; rebinding during that window would pin us to a dead device. |
| Restart reporting | `RestartReport { outcome, same_device, new_device_id }` returned by both `restart_mic` and `restart_system_audio`. `same_device == true` means the rebind landed on the pre-restart device (macOS hadn't committed yet). |
| Retry on same-device | When `same_device` is true, the health layer increments `restart_attempts` (still capped at `STREAM_RESTART_MAX_ATTEMPTS`), clears `last_restart_at` so the next poll retries immediately, and preserves `last_write_pos_advance` so Layer 2 (stall) can also fire. |
| Drift-check gate | `mic_input_drifted` skips the comparison when `bound_is_default == false` — users who explicitly pick a non-default mic no longer get a "device identity drift (listener missed)" restart fired on every 3 s poll. |

### Layer map after this phase

0. **CoreAudio listener** (push, macOS) — default-device change *or* device-list change, verified with a 200 ms settle re-query.
1. **cpal error callback** — rare on macOS for default-device change; retained for other failure modes.
2. **`write_pos` stall** — 2 s threshold, gated by 5 s cooldown (bypassed when layer 0's restart rebound to same device).
3. **Device-identity drift poll** — 3 s throttle, skipped for non-default-bound mic streams.

---

## Phase 27 — Knowledge Management: Tags, Multi-Turn Tools, Vocabulary Hints, Auto-Folder Suggestions

### Tags Infrastructure (Migration v11)

New `tags` and `session_tags` tables with `COLLATE NOCASE` uniqueness and `source` column (`manual`/`auto`/`ai`). Tag CRUD in `db.ts`, store state (`tags`, `sessionTagMap`) in `appStore.ts`, loaded on mount alongside folders.

**Design decision**: Tags are flat (no hierarchy) — folders are the primary organizational primitive. Tags are applied by the AI during summarization, not by the user during recording. This distinction is intentional: folders carry hierarchical descriptions that enrich AI context, tags are lightweight cross-cutting metadata.

### Multi-Turn Tool Execution

Replaced single-turn tool flow with a multi-turn loop in `useChatMessages.ts`. After tools execute, results are sent back as `tool`-role messages and a new streaming call fires (up to `MAX_TOOL_ROUNDS=3`). This enables phased workflows:

- Turn 1: LLM calls `add_to_folder` → receives folder description chain
- Turn 2: LLM uses folder context to call `update_title` + `tag_session` + `save_to_notes`

**Key hardening**:
- Null tool results send `"No action needed."` (OpenAI API requires a response for every `tool_call` ID)
- All tools now have explicit `result` strings (sent to LLM) distinct from `detail` (human badge)
- Abort signal checked between turns and between individual tool executions
- `ToolExecution[]` state on `ChatMessage` with per-tool status (`running`/`done`/`error`)

### New AI Tools

| Tool | Purpose |
|------|---------|
| `tag_session` | Add/remove tags. Creates tags on-the-fly. Source tracked as `"ai"`. |
| `add_to_folder` | Classify session into folder by name. Handles branch conflicts. Returns hierarchical description chain in `result`. |
| `get_folder_context` | Read-only. Returns full folder tree or specific folder's context chain. |

### Folder-First Summarization

All 4 action directives rewritten with explicit two-phase instructions:
- **Phase 1**: "Call ONLY `add_to_folder`. Do NOT call other tools yet."
- **Phase 2**: "After receiving folder context, proceed with title, tags, notes."

The folder tree (names + descriptions) is injected into the system prompt only during action triggers via `assembleFolderTreeForActions()` — not in regular freeform chat.

### Whisper Vocabulary Hints

Folder/tag names (≥4 chars) are prepended to Whisper's `initial_prompt` as comma-separated hints: `"ConsignR, ProjectAlpha. <rolling context>"`. Budget: 80 chars for vocab prefix, remainder for rolling context.

**Dynamic updates**: Vocabulary hints moved from immutable `LiveTranscriptionConfig` to `Arc<Mutex<String>>` on `LiveTranscriptionRuntime`. New `update_vocabulary_hints` Tauri command writes to the shared Arc. `transcribe_chunk()` reads fresh each chunk. Frontend pushes updates when folder chip is accepted.

**Fresh DB queries**: Both session start and dictation start query `listFolders()` + `listTags()` directly from SQLite — not from the store snapshot. This ensures newly created folders are always included.

### Auto-Folder Suggestion Chips

`FolderSuggestionTracker` in `lib/auto-tag.ts` scans live transcript segments for folder name keywords. Matching rules: names ≥4 chars, word-boundary regex, 2+ distinct mentions before suggesting. `AutoTagSuggestions` component renders inline chips below session header during recording. On acceptance: session added to folder, vocab hints rebuilt and pushed to Whisper backend.

**Folders only**: Chips suggest folder placement exclusively. Tags are applied by the AI during Phase 2 of summarization.

### Tool Execution UI

Replaced raw `*Executing: add_to_folder, tag_session...*` text with structured `ToolExecutionBlock` component. Each row shows: status icon (spinning Loader2 → Check → AlertCircle), tool-specific icon (FolderInput, Tag, FileText, etc.), human label, and detail. Rendered in a subtle bordered container above the message bubble. Both live executions and persisted `[tool:]` badges render through the same component.

### Files Changed

| File | Change |
|------|--------|
| `src-tauri/src/db.rs` | Migration v11: tags, session_tags tables |
| `src-tauri/src/commands/live_transcription.rs` | `LiveTranscriptionRuntime` wrapper, dynamic vocab hints Arc, `update_vocabulary_hints` command |
| `src-tauri/src/lib.rs` | Register new command, update state access pattern |
| `src/lib/db.ts` | Tag CRUD, `DbTag`/`DbSessionTag` interfaces |
| `src/lib/ai-tools.ts` | 3 new tools (tag_session, add_to_folder, get_folder_context), `result` field on all tools, `ToolEffect: "organization"` |
| `src/lib/ai-actions.ts` | Two-phase directives on all 4 actions |
| `src/lib/ai-prompts.ts` | `GENERAL_DIRECTIVE` updated with new tool descriptions, `folderTreeContext` param on `getSystemPromptWithToolContext` |
| `src/lib/ai-context.ts` | 6 tools in session context, `assembleFolderTreeForActions()`, expanded `ToolContext` |
| `src/lib/ai.ts` | `ToolExecution`/`ToolExecutionStatus` types, `assembleFolderTreeContext()`, `ChatMessage.toolExecutions` |
| `src/lib/transcription.ts` | New file: `buildVocabularyHints()` |
| `src/lib/auto-tag.ts` | New file: `FolderSuggestionTracker`, keyword matching (folders only) |
| `src/hooks/useChatMessages.ts` | Multi-turn loop, `ToolExecution[]` state tracking, per-tool status updates, null result handling |
| `src/hooks/useAutoTag.ts` | New file: folder suggestion hook with vocab hint push |
| `src/hooks/useDictation.ts` | Vocab hints from fresh DB |
| `src/stores/appStore.ts` | Tags state, tag actions, vocab hints from fresh DB |
| `src/components/ToolExecutionBlock.tsx` | New file: tool status rows with icons/spinners |
| `src/components/AutoTagSuggestions.tsx` | New file: folder suggestion chips |
| `src/components/AIChatMessage.tsx` | Render `ToolExecutionBlock` for live + persisted tool state |
| `src/components/NoteDetailView.tsx` | Wire `useAutoTag`, render `AutoTagSuggestions`, `"organization"` effect refresh |
| `src/components/AppLayout.tsx` | Load tags + session tags on mount |

---

## Phase 28 — Escape-to-Cancel for Dictation

### What was built

Pressing **Escape** while a Dictation is in any non-idle phase (`recording`, `transcribing`, `processing`, or post-failure `done`) fully aborts that Dictation: live transcription stops, in-flight AI is aborted, the Output action does not fire, no `dictation_history` row is written, and the streamed Session WAV is deleted. The Dictation Bubble shows a brief "Cancelled" (~450 ms) before hiding.

PRD: `docs/plans/dictation-escape-cancel.md`.

### Key decisions

- **Frontend-only feature.** No backend or IPC changes. The cancel reducer reuses existing `commands.stopLiveTranscription()` and `commands.deleteSessionWav()`. The Sidecar's currently-in-flight Chunk transcribe is allowed to finish and its result is discarded by the Lifecycle Hook on the way past — explicitly out-of-scope for V1.
- **Escape as a Global hotkey, scoped to a non-idle Dictation.** Registered in `useDictation`'s `handleStart` (after the start is accepted) via `@tauri-apps/plugin-global-shortcut`'s per-binding `register`/`unregister`, and unregistered in `setIdle`. Outside an active Dictation, Escape behaves normally for the OS / focused app. The Global registration is what makes cancel work while focus is in another app, which is the realistic dictation case for `paste` and `clipboard` Output actions.
- **Cooperative cancellation, not preemption.** The cancel reducer (`handleCancel`) takes ownership by setting `phase = "cancelling"` synchronously. Every `await` in the existing `handleStop` body and the ghost-transcription guard in `handleStart` are followed by an explicit `if (phase() === "cancelling") return;` bail-point, so the cancel path does its own teardown without double-running with the happy path.
- **Suppress, don't preempt, the Output action.** The reducer never reaches the paste/copy/note branches — there is nothing to undo, no race with the OS clipboard. AI is the one in-flight thing we *can* cancel, so the AI `AbortController` is aborted explicitly.
- **TS narrowing workaround**: a `phase = (): DictationState => stateRef.current` helper inside the effect defeats TypeScript's control-flow narrowing of `stateRef.current` after intermediate assignments (e.g. `= "transcribing"`), without which the cancel bail-points read as "no overlap with literal type" comparisons.
- **Session safety**: a regular Session and a Dictation cannot run concurrently (`LiveTranscriptionState` is single-occupancy on the Rust side), so cancel cannot affect a Session's stream. Pressing Escape during a Session is a no-op — the hotkey isn't registered.

### Trade-offs accepted

- **Settings change mid-Dictation** can cause `useGlobalShortcuts.unregisterAll()` to clobber the Dictation Escape registration. Rare and not handled — the Dictation continues normally and finishes via its hotkey or programmatic stop.
- **In-flight Sidecar Chunk** runs to completion. Bounded by VAD config (≤10 s chunks) and CPU/GPU cost is acceptable. Adding a per-request abort verb would require backend protocol churn; deferred until measurement shows it's worth it.
- **Escape consumed system-wide** while a Dictation is open. Other apps' Escape handlers don't see the keypress for the duration. Acceptable because dictation is brief and Escape pressed during a Dictation almost certainly means "cancel this Dictation."

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src/lib/events.ts` | Added `cancelled` to the `BubbleState` union |
| `apps/desktop/src/components/DictationBubble.tsx` | Added `cancelled` state config (neutral grey ring, "Cancelled" label, no pulse) |
| `apps/desktop/src/lib/analytics.ts` | New `trackDictationCancelled({ slot_id, phase, duration_ms })` |
| `apps/desktop/src/hooks/useDictation.ts` | `cancelling` phase + `handleCancel` reducer + `registerCancelHotkey` / `unregisterCancelHotkey` lifecycle + per-await bail-points in `handleStop` and the ghost-transcription guard |
| `docs/plans/dictation-escape-cancel.md` | PRD |

---

## Phase 29 — AI Tool Extensions: Transcript Editing, Note Modes, Undo Receipts

### What was built

Five expansions to the AI chat tooling layer that turn it from "summarize and tag" into "edit and refine":

1. **`replace_in_transcript`** — surgical edit of transcript text by find-and-replace. Touches the durable `segments` rows, preserves `original_text` so undo restores every touched segment.
2. **`save_to_notes` modes** — added `prepend` (TL;DR / executive summary above) and `find_replace` (substring swap inside existing notes). Prior `replace` semantics are preserved but the system prompt now nudges the model to prefer `find_replace` for "change", "fix", "edit", "update", "rename" requests instead of overwriting the whole note.
3. **Undone receipts** — undone tool calls render as grayed receipts in the chat history rather than disappearing entirely. Persisted tool calls are also cleaned up on undo so the next replay sees consistent state.
4. **Action button intent fix** — action buttons (Summarize / Key Points / etc.) no longer ask "conversation or notes?" — they pick the correct surface based on whether the source is a transcript or note context.
5. **Citation chip robustness** — citation chips now render reliably across messages even when the chat is reloaded mid-stream. `[[seg:ID]]` tokens land in HTML as `<span data-segment-ref>` elements, then the renderer reattaches click handlers per message on mount.

### Key decisions

- **Editing the transcript via the AI is durable, not a UI overlay.** `replace_in_transcript` mutates `segments.text` and stamps `edited_at`; `original_text` captures the pre-edit value once. Undo restores `original_text` per touched segment. This matches how human edits already work — there's no "AI shadow text" layer.
- **Notes modes are first-class branches, not free-form.** The schema enum is `replace | append | prepend | find_replace`, with `find` required when `find_replace` is chosen. The system prompt steers the model toward `find_replace` over `replace` whenever the user is asking for a change rather than a wholesale rewrite, because `replace` would silently nuke the user's manual edits.
- **Undone tool calls stay visible.** Hiding them was confusing — users saw a bot reply with no apparent action. Graying the receipt while keeping the row preserves provenance.

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src/lib/ai-tools.ts` | New `replace_in_transcript` tool. `save_to_notes` schema gains `prepend` + `find_replace` modes and a `find` parameter. |
| `apps/desktop/src/lib/ai-prompts.ts` | Notes guidance rewritten to prefer `find_replace` for change requests. |
| `apps/desktop/src/components/ToolExecutionBlock.tsx` | Grayed-receipt rendering for undone calls; tightened bottom padding. |
| `apps/desktop/src/hooks/useChatMessages.ts` | Persist undone tool-call state to DB so reloads see consistent receipts; tighten search filters. |

---

## Phase 30 — Resumable Sessions via `session_audio_parts`

### What was built

Each session can now consist of multiple audio parts persisted in a new `session_audio_parts` table (migration v15). Recording into an existing session appends a new part instead of overwriting; the FE concatenates parts at playback time, segments use a cumulative `audio_offset_seconds` base so timestamps stay continuous across resumes. This makes sessions genuinely resumable without losing transcript-to-audio alignment.

### Key decisions

- **DB is the durable source of truth, FE event is just a refresh hint.** The `session_audio_parts` row is inserted from Rust at finalize time *before* `session-part-ready` is emitted, so a missed FE event (window closed, force quit) can't lose the file. The FE handler now just refreshes parts from the DB.
- **One file per part, no concatenation on disk.** Files are named `{session_id}.{part_index}.{wav|mp3}` per the user's `audioExportFormat` setting. Resuming a session opens a new `SessionWavWriter` at `part_index = N` rather than rewriting `part_index = 0`. Concat happens at playback time, in the FE, via a parts-aware `seekTo`.
- **`audio_save_locations` table catches orphans.** Every directory the app has written audio into is recorded at recording start, *before* the file exists. If the run dies before the row insert, `scan_missing_audio_parts()` on next startup walks each known dir for files matching `{session_id}.{part_index}.{ext}` and reconciles the missing rows. This covers crash, force-quit, and the migration's runtime backfill.
- **Migration v15 is idempotent and safe.** Backfills `session_audio_parts` from legacy `wav_file_path` rows but does not drop the legacy columns (SQLite ALTER-DROP-in-same-tx hazard). The legacy columns remain as a duration fallback for sessions whose part rows didn't reconcile.
- **`delete_audio_files` surfaces failures.** Bulk-delete returns an error listing every path that didn't unlink, so the FE can warn or queue retries; we explicitly removed an earlier "pending deletion retry" loop because the surfaced error already gives the FE everything it needs.
- **`audio-stream://` allow-list extended at runtime.** `TrustedAudioDirs` is seeded at startup from `session_audio_parts.file_path` parents and `audio_save_locations`, then extended each time a part is finalized. User-chosen export paths (anywhere on disk) now play, while path traversal and untracked paths still 404.

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src-tauri/src/db.rs` | `session_audio_parts` table + migration v15 + `AudioPartRow` + `insert_audio_part_row` + `register_audio_save_location` + `scan_missing_audio_parts` + `reconcile_audio_parts` + runtime backfill in `ensure_runtime_schema` |
| `apps/desktop/src-tauri/src/commands/capture.rs` | New `delete_audio_files` command (validates against `TrustedAudioDirs`, surfaces failure list) |
| `apps/desktop/src-tauri/src/commands/live_transcription.rs` | `LiveTranscriptionConfig.audio_save_location` / `audio_export_format` / `mp3_bitrate` / `resume`; finalize inserts the part row + emits `session-part-ready` |
| `apps/desktop/src-tauri/src/lib.rs` | `TrustedAudioDirs` allow-list seeded at startup from parts + audio_save_locations |
| `apps/desktop/src/lib/db.ts` | `listSessionAudioParts`, parts-aware deletion, `listAllAudioPartPaths` for `clearAllSessions` |
| `apps/desktop/src/stores/appStore.ts` | Resume flow appends a new part; `onSessionPartReady` refreshes from DB |

---

## Phase 31 — Live Transcription Decomposition + Tree-shake

### What was built

A behavior-preserving cleanup pass that removed accumulated dead code, decomposed the largest functions in the live-transcription pipeline, and brought stale documentation back in sync with the code. Tracked at `docs/plans/tree-shake-cleanup.md`.

### Key decisions

- **`live_transcription_loop` was 923 lines, now ~445.** Split into named helpers (each with a specific responsibility): `build_initial_sources_and_backfill`, `seed_prompt_from_backfill`, `write_session_wav_samples`, `handle_empty_wav_flush`, `drain_in_flight_chunks`, `dispatch_final_pending_chunks`, `emit_fatal_sidecar_error`, `run_prompt_decay`, `finalize_session_wav`. Loop body now reads as orchestration over named phases.
- **`check_stream_health` was 306 lines, now a 25-line orchestrator + four layered helpers**: `evaluate_listener_signal` (Layer 0 OS push notification with settle-and-recheck for cpal#1175), `evaluate_speculative_signals` (Layers 1–3 cooldown-gated symptom detection), `attempt_source_restart` (the actual restart + outcome bookkeeping), `handle_buffer_replacement` (cursor + WAV-flush reset on a fresh buffer). Same restart policy, same per-source `restart_attempts` cap, same emitted events.
- **`transcribe_chunk` was 261 lines, now 156 lines.** Pulled `build_effective_prompt` (vocab + accumulated_text combination, with mutex-scoped vocab snapshot to keep `update_vocabulary_hints` from blocking on transcribe round trips) and `recover_from_chunk_failure` (post-error sidecar respawn handshake — `try_unwrap`, put-back, retry).
- **Dead code deleted, not just suppressed.** `BackfillChunk` struct (never instantiated), `chunk_at_silence_boundaries()` and its tests (no production caller — backfill exclusively uses `vad_chunk_historical_audio`), and the `should_stall_restart` if/else simplified to a single negated expression.
- **Frontend `useDictationEntry()` hook unifies the dictation row UI.** `DictationFeedEntry` and `DictationTrayItem` previously duplicated playing-state, audio play/pause, copy/move-to-note/delete handlers, and store wiring. The hook owns all of it; both components now focus on layout.
- **Several agent-suggested deletions were reverted under verification.** `prompt_seeded_from_backfill` is a load-bearing one-shot guard against reseeding the shared prompt every poll; `getDayLabel` is internal-and-tested; the three WAV flush thresholds (10/20/100) each serve a distinct purpose (one-shot user error, periodic warn, success-path diagnostic). Plan file documents each skip.
- **Documentation reconciliation** — CLAUDE.md, `docs/ARCHITECTURE.md`, `docs/API_REFERENCE.md`, and `docs/DEVELOPMENT.md` were updated to drop the `WhisperClient` alias and the `init_whisper_client` legacy shim (both removed long ago), bump the documented Zustand store version (22 → 23) and SQL migration count (v11 → v15), and correct the description of `db::ensure_runtime_schema()` (sweeps stale recording sessions and creates `audio_save_locations`; the `segments.speaker_id` column is added by the frontend's `getDb()` after migrations run).

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src-tauri/src/commands/live_transcription.rs` | All extractions; `should_stall_restart` simplification; dead code removal |
| `apps/desktop/src/hooks/useDictationEntry.ts` | New shared hook |
| `apps/desktop/src/components/DictationFeedEntry.tsx`, `DictationTrayItem.tsx` | Refactor to consume hook |
| `CLAUDE.md`, `docs/ARCHITECTURE.md`, `docs/API_REFERENCE.md`, `docs/DEVELOPMENT.md` | Stale-content sweep |
| `docs/plans/tree-shake-cleanup.md` | New plan tracking the refactor |

---

## Phase 32 — Transcription Scheduler (Backfill Durability)

### What was built

A single-worker priority queue in front of the sidecar lane that fixes a class of bugs where backfill audio could be silently dropped on session stop, especially under sustained live dictation. New module `commands/transcription_scheduler.rs` (~600 lines including 6 unit tests). Live transcription pipeline rewired to submit jobs through the scheduler instead of calling `TranscriptionClient::transcribe_with` directly.

### The bug being fixed

Before this phase, on session stop the live loop set a `backfill_cancel` flag that broke the backfill submitter loop at the next chunk boundary. Every backfill chunk past the cancel point was dropped without being transcribed — its audio (alive in memory as `Vec<f32>` inside the submitter task) was simply freed. Combined with no priority scheduling between live and backfill at the sidecar, sustained live dictation would starve backfill, and stop would then guarantee whatever hadn't drained got abandoned.

### Key decisions

- **Priorities `FinalFlush > Live > Backfill`, mic/system round-robin at Live.** Live work always preempts backfill at the sidecar; closing-words at stop preempt everything. Round-robin within Live keeps mic and system from starving each other in mixed-source sessions.
- **Single-owner client.** The scheduler holds the sole `Arc<TranscriptionClient>` clone for the session, and is the only task that calls `transcribe_with`. This makes sidecar respawn race-free: when a transcribe call fails, the worker drops its in-call clone and `Arc::try_unwrap` always succeeds. The previous design had to fall back to "another task still holds the Arc" branches; that's deleted along with `recover_from_chunk_failure`.
- **Backfill is no longer cancelled on stop.** The submitter walks its in-memory chunk list and submits each at `Backfill` priority. The submitter awaits each chunk's scheduler response before submitting the next (per-chunk prompt context and in-order segment emission both depend on this serial wait), so the submitter — not the scheduler — is what's actually doing the draining post-stop. On stop, the live loop awaits the submitter's join (capped at 5 min); chunks not yet submitted at that timeout are lost (they live in the submitter's stack as `Vec<f32>`, not on any durable queue). The "scheduler keeps draining backfill" framing in earlier drafts of this entry was wrong on that point. Phase 33 keeps this lifecycle and corrects the wording.
- **5-minute shutdown timeout.** Generous enough to drain a typical 5-minute backfill window on a slow sidecar; bounded so a wedged sidecar can't hang the stop path forever. If the worker timeout fires after submitter exit, the worker is aborted and the transcription client is dropped rather than returned to shared state — an aborted worker may still hold an in-flight `Arc<TranscriptionClient>` clone, so handing the same client to a new session would race the sidecar's response routing. The next session re-initializes the engine.
- **`LiveSegmentEvent` and `LiveTranscriptionPressureEvent` carry `origin: SegmentOrigin`.** Replaces the earlier `is_backfill: bool` outright — the desktop app and its sidecar deploy atomically, so there's no version skew to bridge, and `origin` is a strict superset (distinguishes `live` from `final_flush`, where the boolean could not).
- **Frontend rendering not changed in this phase.** The new fields are emitted but unconsumed by the UI; the priority change happens entirely at the backend so `pnpm check` is the only verification needed for this branch. Bucketed rendering is a separate UX concern.
- **Parakeet engine tuning untouched.** An earlier draft of this work (on a feature branch that never merged) bundled a Parakeet retuning pass (200 ms silence / 10 s max chunk). Main has since moved to 500 ms silence (`6d66b24`) and that's deliberately preserved here — engine-specific tuning is its own concern, validated independently.

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src-tauri/src/commands/transcription_scheduler.rs` | New module — see ARCHITECTURE.md § "Transcription Scheduler" for the design. Includes 6 unit tests for priority ordering, round-robin, cancel, drain. |
| `apps/desktop/src-tauri/src/commands/live_transcription.rs` | Pipeline rewired around the scheduler. `TranscriptionContext` simplified, `recover_from_chunk_failure` removed (respawn moved into scheduler), backfill cancel flag deleted, stop path waits for backfill submitter then calls `scheduler.shutdown_and_return`. `LiveSegmentEvent` gains `origin`; the prior `is_backfill: bool` is removed in favour of it. |
| `apps/desktop/src/lib/tauri.ts` | `LiveSegmentEvent` typing updated for the new fields. |
| `apps/desktop/src-tauri/src/commands/mod.rs` | Register the new module. |

---

## Phase 33 — Live-Stop Hardening and Live-Tier Protection

### What was built

A second pass on the scheduler-driven pipeline that closes the remaining "audio outlives the session boundary" and "backfill starves live" cases that Phase 32 didn't fully resolve. Branch: `feat/backfill-scheduler` (the same branch that landed Phase 32; this work was layered on before merge).

### The bugs being fixed

- **Soft stop boundary.** Phase 32 made backfill durable across stop, but the *live* path had no hard endpoint — `extract_since` and the WAV flush both read to the current ring-buffer write head, so any audio captured between the user pressing stop and the loop noticing the signal ended up in the final transcript and the session WAV. Final transcripts and final files therefore varied by host load.
- **Backfill head-of-line blocking.** The scheduler honored priority *between* jobs, but a single backfill chunk that started executing was non-preemptible — sidecar inference can't be interrupted — so a 30-second backfill chunk would block live for ~30 s of wall time even with `Live > Backfill` priority.
- **Head-drop on live overrun.** When inference fell behind real time and the live extraction window exceeded the per-chunk cap, the loop dropped the head of the extraction (the oldest audio) and re-anchored to the cap window. Audio was silently lost; pressure was reported as a "head-drop cap" counter that conflated lost audio with slow inference.
- **WAV sample-rate mismatch.** Session WAV creation always preferred whichever ring buffer happened to exist (mic-first), so a `SystemOnly` capture wrote a WAV at the mic device's nominal rate and played back too fast or too slow.
- **Lag metric clobbered backwards.** `SessionCounters::latest_completed_audio_offset_seconds` was overwritten on every successful chunk regardless of audio-time ordering; a backfill chunk for older audio landing after a live chunk for newer audio reset the counter, and the lag display reported `now − backfill_offset` instead of the true near-zero lag.

### Key decisions

- **Stop snapshots `BufferPositions` plus a short tail grace.** `stop_live_transcription` builds a `StopRequest { stop_positions, deadline }` from the ring buffer's current write positions extended by a brief tail window. Final live extraction, the WAV flush, and stream-health restarts are all bounded to that snapshot — samples written after the snapshot are ignored for the stopped session. The tail grace exists so a sentence ending in flight at the moment of stop still gets transcribed.
- **Dual stop-signal kept on purpose.** The oneshot `stop_request` payload (carrying the bounded `BufferPositions`) and a separate atomic checkpoint flag are *both* set on stop. The oneshot delivers the bounded positions to the loop body; the atomic is what cooperative paths (poll loop, drain sites) check. They are intentionally separate — unifying them would either lose the carried positions or force every checkpoint site to await the oneshot.
- **Backfill split into 5 s quanta before submission.** Every backfill chunk is sliced into ≤5 s pieces before going on the scheduler queue. A backfill job that has started executing at the sidecar still runs to completion (sidecar inference is not preemptible), but the worst-case delay a fresh `Live` job sees while a `Backfill` job is in-flight is now bounded by one quantum instead of by the full chunk.
- **Live-busy gate.** The scheduler grows an explicit `live_busy` flag the live loop sets while speech is active, live/final chunks are in flight, a force-drain backlog exists, or the stop path is dispatching bounded live tails. `Backfill` jobs queue normally during that window but only dispatch when the gate clears; `FinalFlush` and `Live` jobs ignore the gate and dispatch by priority.
- **Replace head-drop with preserved-drain.** When the extracted live audio exceeds the per-chunk cap, the loop now extracts the oldest contiguous max-size range, preserves the remaining cursor, and force-drains one chunk per source per tick without growing the scheduler queue. The chunks reach the sidecar in order and no audio is dropped. The `live_drain_backlog_chunks` / `live_drain_backlog_seconds` fields on `LiveTranscriptionStatus` and the `drain_backlog_seconds` field on the pressure event surface this catch-up state.
- **`lag_seconds` and `live_drain_backlog_seconds` are not the same thing.** Both can appear non-zero, both can be zero independently, and they answer different questions — `lag_seconds` is wall-clock minus latest committed audio offset (folds in chunking, queueing, and inference); `live_drain_backlog_seconds` is the source-local already-captured audio still queued for the live tier. ARCHITECTURE.md § "Transcription Scheduler" documents the distinction so a future maintainer doesn't unify them.
- **Session WAV sample rate keyed off `CaptureSource`.** New `AudioManager::output_sample_rate_for(source)` consults the buffer matching the configured source (`MicOnly` → mic, `SystemOnly` → system, `Mixed` → mic when present, system otherwise). The live-transcription path passes its config rather than relying on whichever buffer exists.
- **`latest_completed_audio_offset_seconds` is monotonic in audio time.** The counter is now updated with `max`, not `=`, so a late backfill chunk for older audio cannot clobber the live tier's high-water mark. The field's rustdoc states the invariant.
- **Bounded reads in `yapstack-audio`.** `AudioRingBuffer::snapshot_range(from, until)` and `AudioManager::extract_since_until(positions, limits, ...)` were added so the stop path doesn't read implicitly to the live write head. Both report `overrun: bool` when the requested start has already been overwritten.

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src-tauri/src/commands/live_transcription.rs` | Stop request now carries bounded positions + tail grace; final live extraction, WAV flush, and stream-health work are gated on the stop boundary. Live overrun replaced with preserved-drain. Backfill submitter slices into 5 s quanta and submits behind a live-busy gate. WAV creation uses `output_sample_rate_for(source)`. `SessionCounters::latest_completed_audio_offset_seconds` writes via `max`. |
| `apps/desktop/src-tauri/src/commands/transcription_scheduler.rs` | New `live_busy` gate at the scheduler level. |
| `crates/yapstack-audio/src/ring_buffer.rs` | New `snapshot_range(from, until) -> AudioRangeSnapshot { samples, start_pos, end_pos, overrun }`. |
| `crates/yapstack-audio/src/manager.rs` | New `output_sample_rate_for(source)` and `extract_since_until(positions, limits, source, mix_config) -> Option<BoundedExtraction>`. |
| `crates/yapstack-audio/src/capture.rs` | New `BoundedExtraction { samples, sample_rate, new_positions, overrun }`. |
| `crates/yapstack-audio/src/lib.rs` | Re-export the new types. |
| `crates/yapstack-audio/src/export.rs` | LAME `set_output_sample_rate` pinned to the input WAV's rate; new `test_mp3_output_sample_rate_matches_input_at_low_bitrate` regression. |
| `apps/desktop/src/lib/types.ts`, `lib/events.ts`, `components/StatusPopover.tsx`, `stores/appStore.ts`, `test/tauri-mocks.ts` | Status surface swapped from head-drop cap counter to drain-backlog fields. |
| `.github/ISSUE_TEMPLATE/_drafts/chronic_drain_backlog.md` | Drafts a follow-up ticket: under sustained slow inference the drain backlog grows unbounded; needs a chronic-drain response policy (graceful warning, eventual stop, or queue cap). Out of scope for this phase. |

### What was learned

- **A "stop" signal is two things.** A *boundary* (where in the audio stream the session ends) and a *deadline* (when the loop should stop doing work). Earlier code conflated them — the loop "stopped" by exiting, and the boundary was wherever the ring buffer happened to be at exit time. Splitting them out (the oneshot carries the boundary, the atomic carries the deadline) is what made stop deterministic.
- **Sidecar inference being non-preemptible has consequences past the scheduler queue.** Phase 32 fixed the queue ordering; Phase 33 added time-slicing because once a job dispatches, priority no longer matters until it finishes. Five seconds was picked empirically as the sweet spot — short enough to bound live delay, long enough that VAD-aligned chunk boundaries still dominate.
- **Pressure metrics that conflate two phenomena are worse than two metrics that can each be zero.** The old "drops" counter mixed "audio was lost" with "inference is slow" — both were rare individually, but the combined symptom was indecipherable. Splitting into `lag_seconds` (end-to-end delay) and `live_drain_backlog_seconds` (live-tier-specific catch-up) made each interpretable.

---

## Phase 34 — Dictation Volume Ducking (macOS) + Sidecar Crate Restructure

### What was built

Two unrelated changes that landed together in the alpha.8 cycle.

1. **Dictation volume ducking.** Opt-in setting on the Dictation tab that lowers the system output volume to a configured target while a dictation is recording, then restores it the instant the user releases the key. Settings: `dictationDuckEnabled: bool` (default false), `dictationDuckTarget: number` in `[0, 1]` (default 0.2). New backend module `apps/desktop/src-tauri/src/system_volume.rs` (mechanism, ~590 lines incl. tests) plus `commands/system_volume.rs` (Tauri surface, ~40 lines).
2. **`yapstack-sidecar` → `yapstack-transcription-sidecar` rename.** Behaviour-preserving repackaging that makes room for additional sidecar workers (e.g. an embedding sidecar) without naming ambiguity. Crate, binary, build script, and tracing target all renamed; a wrapper `scripts/build-sidecars.sh` now fans out to per-worker scripts.

### Bugs / needs being addressed

- **Ducking:** users on earphone playback have no clean way to talk over a podcast or call to dictate a quick note. Pausing media is often inconvenient, and reaching for the volume keys mid-sentence doesn't work well with hold-to-talk. Ducking gives them an automatic "duck while talking, restore on release" loop.
- **Sidecar rename:** the project now anticipates a second sidecar worker (embeddings for semantic search). Naming the existing one `yapstack-sidecar` would have made the new one's name awkward (`yapstack-embedding-sidecar` next to a generic `yapstack-sidecar`). The rename was the cheapest moment to do this — before any external integration depends on the binary name.

### Key decisions

- **Mechanism / policy split.** The `system_volume` module knows nothing about dictation; it exposes generic `apply_duck(target)` / `restore()` and a `(device_id, level)` snapshot. Dictation is the only caller today, but a future feature (e.g. duck during meetings) can reuse the mechanism without entangling the volume code with dictation state. Reflected in module names: `system_volume::apply_duck` (mechanism) vs. `apply_volume_duck` (Tauri command, also generic).
- **Snapshot the *device*, not just the level.** macOS users routinely change default output mid-session (Bluetooth headphone connect, wired DAC unplug). Restoring "the system default to level X" would silently leave the *originally* ducked device stuck at the ducked level forever. The snapshot is `(device_id, prior_level)` and restore explicitly targets that device.
- **Never raises.** If the user's current level is already at or below the target, `apply_duck` is a no-op and no snapshot is captured. Without this guard, a user dictating with the volume already at 5% would have it bumped to the 20% target on apply and *stay there* on restore.
- **Apply / restore hold the snapshot mutex across the volume set.** Concurrent ops can't interleave into a state where the snapshot is updated but the volume isn't (or vice versa) — that would either leak the ducked level forever or strand a snapshot of an already-ducked level as if it were the user's prior choice.
- **Frontend serializes apply/restore across cycles via two pending-promise refs.** A fast stop-then-start cycle (release key, immediately re-press) could otherwise interleave a `restore` and the next `apply` at the backend and strand the volume — the second `apply` would snapshot the ducked level as if it were the user's choice. The hook awaits any pending duck before invoking restore, and any pending restore before invoking duck. Restore is also awaited on the post-duck-error and slot-disappeared paths *before* `setIdle` so a rapid retry can't start a new duck on top of an unresolved restore.
- **`RunEvent::Exit` calls `restore` as a final safety net.** If the app crashes or quits while ducked, the user shouldn't have to reboot to get their volume back. Belt-and-braces in addition to the per-cycle restore in the dictation hook.
- **Errors are swallowed at the command boundary.** Volume control is a UX nicety, not load-bearing for the dictation flow — a CoreAudio failure shouldn't surface as a toast or block the recording. The mechanism still returns `Result` so internal callers can log; the Tauri surface just `warn!`s and returns `()`.
- **`build-sidecars.sh` is a wrapper, not a replacement.** Per-worker scripts (`build-transcription-sidecar.sh` today) keep their own concerns; the wrapper is for callers that want "build everything" without knowing the worker list. Existing npm scripts (`build:sidecar[:dev]`) kept as compat aliases pointing at the per-worker script.

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src-tauri/src/system_volume.rs` | New — `apply_duck`, `restore`, `(device_id, level)` snapshot, macOS CoreAudio FFI, no-op stubs for non-macOS, with property test–style coverage of the snapshot/restore lifecycle (device-swap mid-duck, already-quieter no-op, repeated-apply behaviour, restore without snapshot). |
| `apps/desktop/src-tauri/src/commands/system_volume.rs` | New — Tauri surface (`apply_volume_duck`, `restore_volume`); error-swallowing wrappers. |
| `apps/desktop/src-tauri/src/lib.rs` | Register the new module + commands; add `RunEvent::Exit` safety-net restore. |
| `apps/desktop/src-tauri/src/logging.rs` | Update tracing target name for the renamed sidecar crate. |
| `apps/desktop/src/components/settings/DictationTab.tsx` | New "Lower system volume during dictation" toggle + target slider. |
| `apps/desktop/src/hooks/useDictation.ts` | Apply/restore at the recording-phase boundaries; cycle serialization via `pendingDuckRef` / `pendingRestoreRef`; restore on error and slot-disappeared paths awaited before `setIdle`. |
| `apps/desktop/src/stores/appStore.ts` | New persisted settings: `dictationDuckEnabled`, `dictationDuckTarget`. |
| `apps/desktop/src/lib/types.ts`, `apps/desktop/src/lib/tauri.ts` | Generated bindings for the new commands. |
| `crates/yapstack-sidecar/` → `crates/yapstack-transcription-sidecar/` | Crate renamed; binary name follows. |
| `scripts/build-sidecar.sh` → `scripts/build-transcription-sidecar.sh` | Per-worker build script renamed. |
| `scripts/build-sidecars.sh` | New wrapper that fans out to per-worker build scripts. |
| `apps/desktop/src-tauri/tauri.conf.json` | `externalBin` updated for the renamed binary. |
| `.github/workflows/ci-checks.yml`, `.github/workflows/release.yml`, `scripts/build-dmg.sh` | Updated for the renamed crate / script. |
| `crates/yapstack-transcription/src/client.rs` | Tracing target renamed to `yapstack_transcription_sidecar`. |
| `README.md`, `AGENTS.md`, `docs/ARCHITECTURE.md`, `docs/API_REFERENCE.md`, `docs/AGENT_GUIDE.md`, `docs/DEVELOPMENT.md`, `docs/GLOSSARY.md`, `docs/UBIQUITOUS_LANGUAGE.md`, `docs/PRINCIPLES.md`, `docs/adr/0001-adopt-agents-md.md` | Sidecar references updated; ARCHITECTURE/API_REFERENCE/GLOSSARY also gain ducking surface; README alpha warning made generic. |

### What was learned

- **Ducking is an interaction problem, not just a system call.** The CoreAudio side is two FFI calls; the hard parts were all in the lifecycle: device swap mid-duck, hold-to-talk press/release storms, app crash mid-duck, post-restore retry races. Most of `system_volume.rs`'s test coverage and the dictation hook's promise refs exist to handle those, not the volume change itself.
- **Generic command surface paid for itself in 24 hours.** The `apply_volume_duck` / `restore_volume` commands were written generic from the start (they take `target: f32`, not `dictation_settings: ...`). When a follow-up came up about ducking during full sessions too, no backend change was needed — same commands, different policy caller.

## Phase 35 — Event-Driven Device Tracking + Tiptap UX Overhaul

### What was built

Two unrelated landings folded into the alpha.9 cycle.

1. **Event-driven device tracking + Capture auto-failover (#25).** The audio crate's Core Audio listener path moved off `AtomicBool` flag-polling onto a runtime-agnostic `DeviceEventSink`, consumed by an always-on Tauri-side `device_broker`. The broker debounces bursty Core Audio events in a 250 ms window, gates restarts on `kAudioDevicePropertyDeviceIsAlive`, and dispatches `RestartTarget::FollowDefault` restart intents through the live-transcription loop (or directly to `AudioManager` when no live session is active). New Tauri events: `devices-changed` (re-emitted on any device-list change *or* any system-default flip) and an enriched `stream-health` payload carrying `bound_device_name` for "Switched to {device}" toasts on the FE.
2. **Tiptap UX overhaul (#26).** Multi-color highlight palette (yellow/green/blue/purple/red) themed via CSS variables so highlights re-theme automatically; selection bubble menu scoped to inline marks (Notion / Linear / Novel convention); static toolbar gains Link + Code Block buttons and a heading dropdown that shows the active level; in-app shortcuts (⌘K, ⌘\\, ⌘,, ⌘1/⌘2, ⌘N, ⌘., ⌘J, ⌘D) now fire while the editor has focus; sidebar shortcut moved from ⌘B → ⌘\\ to stop fighting Tiptap's bold binding; themed checklist checkboxes; pasted markdown with fenced code blocks parses as a real code block.

### Bugs / needs being addressed

- **Device tracking:** the previous `AtomicBool` poll-and-react path required a 200 ms `thread::sleep` workaround for the AirPods/Bluetooth revert window, missed default-device flips that didn't change the device list, and bypassed its own `IsAlive` gate due to a CamelCase / lowercase mismatch in `strip_cpal_prefix`. Plugging into a Thunderbolt dock that brought a new audio interface online silently re-bound to the previous device because the failover probe order tried the stored id first. Users on AirPods that dropped mid-session got stuck with a dead binding until they restarted capture by hand.
- **Tiptap:** the selection bubble menu was duplicating block-level controls (headings, lists, blockquote, code block) that already lived in the toolbar, while the inline-mark controls in the toolbar didn't reflect active state — bold persisting on a new line was invisible. ⌘B couldn't be used for bold inside notes because the global "toggle sidebar" binding swallowed it. Pasting a code block out of a terminal landed as a flat line of text. Checklist checkboxes were unstyled native browser controls.

### Key decisions

- **Sink stays runtime-agnostic; broker owns the async surface.** The audio crate stays free of `tokio` so it remains testable headless and reusable from a non-Tauri host. The broker lives in the desktop app because it needs `AppHandle` for emitting events and a runtime to debounce. This split also means the broker can be swapped (e.g. for a Linux backend) without touching the audio crate.
- **Tri-state `DeviceLiveness` (`Alive` / `Dead` / `Absent` / `Unknown`).** The previous boolean fail-opened on "couldn't tell" — an unplugged USB mic that was missing from the device list reported `alive=true`, and subsequent `DefaultInputChanged` events were silently dropped on the explicit-pick branch. The explicit-pick branch now only skips failover on `Alive`; everything else proceeds into the `FollowDefault` probe.
- **`FollowDefault` probes default first; `PreserveBinding` probes stored id first.** The broker dispatches `FollowDefault` because the user's intent on a device-change event is "follow the OS." The in-loop watchdog keeps `PreserveBinding` because its intent on a stream error is "recover the device the user explicitly picked." Same restart entry point, two probe orders.
- **`bound_is_default` is derived from the post-restart bind id**, not preserved verbatim across restart. The previous behaviour kept the explicit-pick flag on through a successful failover to the system default, and silently dropped every subsequent default-change event. Computing from the actual resolved bind id makes the flag self-correcting.
- **Selection bubble vs. static toolbar split mirrors Notion / Linear / Novel.** Inline marks (the kind you toggle on a selection) live in the floating bubble; block formatting (the kind you apply to the whole current block) lives in the static toolbar. Avoids the "two ways to do the same thing in two places" anti-pattern and makes the bubble fast to scan.
- **Highlights are stored as CSS variable references, not literal colors.** Switching between light and dark themes re-themes existing highlights automatically. Was tempting to store the palette index as a `data-` attribute and resolve in CSS, but Tiptap's Highlight extension only round-trips a `color` attribute, so the value *is* the variable reference.
- **`LiveSessionPresent` flag, separate from inbox presence.** The broker's direct-restart fallback used inbox-presence to decide between routing through the live loop and calling `AudioManager::restart_*` directly. `stop_live_transcription` clears the inbox before the live loop's final flush completes, so a device-change in that window would replace the ring buffer while the loop was still extracting at snapshotted stop positions. The new flag is set by the spawned task at start and cleared only after scheduler shutdown, so routing during the stop tail correctly takes the loop path.

### Files Changed

| File | Change |
|------|--------|
| `crates/yapstack-audio/src/system/device_watcher.rs` | New `DeviceEventSink` trait + Core Audio property-listener wiring for input default, output default, system-output default, and device-list changes; emits `DeviceListChanged` / `DefaultInputChanged` / `DefaultOutputChanged` / `DefaultSystemOutputChanged`. |
| `crates/yapstack-audio/src/lib.rs` | Removed `AudioManager::{mic_default_changed, system_audio_default_changed, device_list_changed, mic_input_drifted, system_audio_output_drifted, live_default_input_name, live_default_output_name}` and `DefaultDeviceWatcher::take_change`; added the 4-property listener on init; `device_liveness` returns the new tri-state; loopback aggregate filtered at enumeration time. |
| `apps/desktop/src-tauri/src/device_broker.rs` | New — always-on broker. Debounces 250 ms, evaluates `DeviceLiveness`, dispatches `RestartTarget::FollowDefault`, emits `devices-changed`. |
| `apps/desktop/src-tauri/src/live_transcription.rs` | Routes broker-driven restart intents through the live loop when `LiveSessionPresent` is set; falls back to direct `AudioManager::restart_*` otherwise. Dropped the ~30 s name-comparison drift poll. |
| `apps/desktop/src-tauri/src/commands/audio.rs` | `start_mic` rejects the cpal loopback aggregate as defense-in-depth. |
| `apps/desktop/src-tauri/src/lib.rs` | Spawns the broker task; wires the new event names. |
| `crates/yapstack-common/src/types.rs` | `StreamHealthEvent` carries `bound_device_name`; `RestartTarget::FollowDefault` variant added. |
| `apps/desktop/src/stores/appStore.ts`, `apps/desktop/src/lib/tauri.ts` | Listens for `devices-changed`, replaces the cached device list, reconciles `selectedMicDeviceId` if its device disappeared, picks up refreshed `is_default` flags. Toasts on successful auto-failover using the new `bound_device_name` field. |
| `apps/desktop/src/components/notes/NoteEditor/extensions/MultiColorHighlight.ts` | New — extends Tiptap Highlight with a 5-color palette stored as CSS variable references. |
| `apps/desktop/src/components/notes/NoteEditor/Toolbar.tsx`, `BubbleMenu.tsx` | Reactive active-state via `useEditorState`; bubble menu scoped to inline marks; toolbar adds Link + Code Block + heading dropdown. Floating UI flip/shift now use the editor as boundary; `z-50` so it sits above other floating UI. |
| `apps/desktop/src/index.css` | `.tiptap-editor` themed checklist checkboxes (accent fill, contrast checkmark, focus-visible ring, first-line alignment); highlight palette CSS variables. |
| `apps/desktop/src/hooks/useGlobalShortcuts.ts` | In-app shortcuts (⌘K, ⌘\\, ⌘,, ⌘1/⌘2, ⌘N, ⌘., ⌘J, ⌘D) fire while editor focused; Escape and ⌘⌫ still defer to the editor. Sidebar default rebound to ⌘\\. |
| `docs/ARCHITECTURE.md`, `docs/UBIQUITOUS_LANGUAGE.md` | Already updated with broker / `DeviceEventSink` / `devices-changed` / `bound_device_name` surface in the PR. |
| `docs/FRONTEND.md` | Tiptap notes: multicolor highlights, scoped bubble menu, themed checklists, in-editor shortcut handling. |
| `CHANGELOG.md` | `[1.0.0-alpha.9] - 2026-05-03` block (entries cross-referenced with PR numbers). |
| `apps/desktop/src-tauri/Cargo.toml`, `apps/desktop/src-tauri/tauri.conf.json`, `Cargo.lock` | Version bumped to `1.0.0-alpha.9`. |

### What was learned

- **Event sinks beat polled flags as soon as the consumer needs to debounce or reason about ordering.** The previous `AtomicBool` design was fine when "did anything change" was the only question, but it couldn't carry *which* property changed, couldn't be debounced without losing edges, and forced the consumer to re-poll system state on every tick. The sink path lets the broker batch a Bluetooth-revert burst into one decision and route it intelligently.
- **A "couldn't tell" boolean fails open in exactly the wrong direction.** The original `is_device_alive → bool` reported `true` on lookup failure because that was the conservative default for the watchdog path (don't restart a working stream). For the explicit-pick failover branch the conservative default is the opposite (the device might be gone — let the failover proceed). The tri-state forces the call site to make that choice explicitly.
- **Tiptap's bubble menu is a UX trap when it duplicates the toolbar.** Users got into states where toggling bold from the bubble disagreed with the toolbar's active state because the toolbar wasn't subscribing to selection updates. The fix — `useEditorState` + scope split — is small once you know the convention; the trap is shipping the bubble with everything in it because Tiptap's example shows that.

## Phase 36 — Marquee/Portal Click Swallow + Footer Rebrand

### What was built

Two small landings folded into the alpha.10 cycle.

1. **Stop the transcript marquee from swallowing context-menu clicks (#28).** The whitespace-marquee `onPointerDown` on `ChatView`'s container now bails when the synthetic event came from outside the container's actual DOM subtree (`!currentTarget.contains(target)`). One guard clause.
2. **Settings → General footer rebrand (#29).** Two-character copy/link change.

### Bugs / needs being addressed

- Right-clicking a transcription segment opened the context menu correctly, but every action item — Edit, Copy, Insert into Notes, Hide, Delete — silently no-op'd. Menu stayed open; only the focus ring on the hovered item flickered off. The bug had been in main since `76c7b4e` (drag-select WIP) and only surfaced as user-visible when right-click actions started getting used — none of the previous "fixes" landed because they were diagnosing the wrong layer (we kept reaching for the `appStore` action paths when the actions were never being called at all).

### Key decisions

- **One-line guard, not a refactor.** The architecturally-correct fix is to attach the marquee listeners with native `addEventListener` on a ref'd DOM node — DOM-tree event flow naturally avoids portal cross-talk because portal children whose DOM lives in `document.body` aren't ancestors of the container in the DOM tree. Punted that to a follow-up; the guard unblocks users today and doesn't make the eventual extraction harder (the bail becomes dead code and gets deleted in the same change).
- **Why `currentTarget.contains(target)` is the right discriminator.** For a normal event bubbling within the React/DOM tree, `currentTarget` always contains `target`. For a React synthetic event originating in a portal, `target` is the actual DOM node (in `document.body`) and `currentTarget` is wherever the listener is registered (in `#root`); `contains` returns false. No allowlist of Radix selectors needed.

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src/components/ChatView.tsx` | New early-return in `handleContainerPointerDown` for synthetic events whose DOM target lives outside the container subtree (portal children). |
| `apps/desktop/src/components/settings/GeneralTab.tsx` | Footer attribution + link rebranded. |
| `CHANGELOG.md` | `[1.0.0-alpha.10] - 2026-05-04` block. |
| `apps/desktop/src-tauri/Cargo.toml`, `apps/desktop/src-tauri/tauri.conf.json`, `Cargo.lock` | Version bumped to `1.0.0-alpha.10`. |

### What was learned

- **React's portal events route through the component tree, not the DOM tree.** Any pointer/mouse handler attached as a React prop on a container that *transitively* owns a `<ContextMenu>` / `<DropdownMenu>` / `<Tooltip>` will receive synthetic events from those portals' children. If the handler's contract is "this gesture started inside my DOM subtree" — as drag-select handlers usually are — it needs an explicit `currentTarget.contains(target)` check, or it needs to be wired via native `addEventListener` instead. The latter is the pattern the rest of this codebase already uses (`useClickOutside`, the Settings/Onboarding mousedown listeners) and is the longer-term correct shape for ChatView's marquee logic.
- **`preventDefault` on a `pointerdown` for a mouse pointer suppresses the corresponding mousedown / mouseup / click events.** This is per W3C spec and is exactly why the menu items lost their click delivery: once the marquee handler called `preventDefault`, the click event Radix's MenuItem composes its `onSelect` onto never fired. Easy to forget when reading just the marquee code in isolation.

## Phase 37 — Active-Session Dictation (Concurrent Runtimes + Mic-Ownership Windows)

### What was built

A user can now hit a dictation hotkey while a recording session is in progress and get the dictated text back through the normal paste/clipboard/new-note path, without leaking the dictated audio into the session transcript or session WAV. The session keeps recording across the dictation; the user's word-in-progress at the moment they hit the hotkey is preserved.

The change is mostly an architectural refactor: the `TranscriptionScheduler` becomes a long-lived app-level singleton, the `LiveTranscriptionState` becomes two named slots with explicit lifecycle states, and a new "mic-ownership window" model coordinates which runtime owns mic samples during which intervals.

### The bugs / gap being addressed

Before this phase, hitting a dictation hotkey during a recording session failed with "transcription client not initialized" — `start_live_transcription` rejected a second call because `LiveTranscriptionState` was a single `Option<Runtime>` and the `TranscriptionClient` was extracted exclusively into the per-session scheduler at session start. This was the long-flagged "Dictation during active recording" item under "What's Not Yet Built."

Several bugs surfaced during review and were fixed in the same PR:

- Status events emitted by `emit_status` always set `session_id: None`, so the dictation hook's two-prong `source_kind + session_id` filter never matched the dictation Stopped event and every dictation waited the full 5 s grace timeout before AI/output/history.
- `start_live_transcription` unconditionally installed the device-broker `RestartIntent` sender into the global inbox, so a dictation start during a session replaced the session loop's sender — subsequent device-change restarts routed to the dictation loop and the session never received its restarts.
- Stop-time `set_live_busy(true)` always toggled the session bits regardless of source kind. A dictation finalize then cleared only its `Dictation` bit, leaving session bits stuck and blocking backfill until a future session happened to clear them.
- The defensive `dictation_owns_mic.flag` clear on session start fired unconditionally, including while a dictation runtime was mid-utterance — which unwound the session-mic suspension and let the session ingest dictation audio.
- Dictation acquired its mic ownership flag only at the spawn site, after preflight + scheduler lookup + engine-info probe + audio file plumbing. Audio captured between hotkey press and that point was still ingested by the session loop.
- A race-prone earlier design: a boolean `dictation_owns_mic` flag with edge detection on `dictation_was_active` could miss a dictation that opened-and-closed between two session polls. The closed window left dictation audio in the session transcript and WAV.
- Stop-time final flush could still transcribe dictation audio into the session if dictation cleared the flag during the session's stop tail before the session loop processed the falling edge.
- Session WAV captured dictation audio (transcript skipped it but the recorded file still played back the dictation, creating a confusing playback / transcript mismatch).
- `preflight_stream_health` could replace the mic buffer (`RestartOutcome::BufferReplaced`) during a dictation start, leaving the session loop with stale buffer positions — only the session's in-loop restart handler knows how to reset cursors + WAV `flush_positions` on buffer replacement.

### Key decisions

- **Long-lived scheduler.** `TranscriptionScheduler` is constructed once at engine init in `init_transcription_client` and stored in a new `TranscriptionSchedulerState`. Both runtimes clone `Arc<Scheduler>` from there. The scheduler owns the sole `Arc<TranscriptionClient>` for its full lifetime; engine teardown is the explicit `shutdown_transcription_client` command, which calls `scheduler.shutdown_client()` and is gated on every runtime slot being `Idle`. Replaces the per-session `shutdown_and_return` round-trip back to `TranscriptionClientState`. The legacy state is removed.

- **Routing identity is separate from priority class.** Two new dimensions, deliberately orthogonal:
  - `JobOrigin` (extended with a `Dictation` variant) is the scheduler's *priority class* — which lane a job occupies. Order is `FinalFlush > Dictation > Live (mic/system round-robin) > Backfill`.
  - `LiveSourceKind` (`Session | Dictation`) is the *routing identity* of the runtime that produced an event — what UI surface the event belongs to, what storage path it writes to.

  A dictation runtime's stop-time chunks are still legitimately `JobOrigin::FinalFlush` (priority); their `source_kind` is `Dictation` (routing). Conflating them invites real bugs — e.g. `SegmentOrigin::Dictation` was deliberately *not* added because that would invite frontend code to route on `origin`, which is wrong. Frontend filters everywhere on `source_kind`, never on `origin`. This split is now codified in PRINCIPLES.md.

- **Two-slot runtime state with explicit lifecycle.** `LiveTranscriptionState` becomes `LiveTranscriptionSlots { session, dictation }`, each `RuntimeSlot { Idle | Starting | Running | Stopping }`. The slot stays non-`Idle` for the full lifetime including finalization — the spawned task's finalizer transitions the slot back to `Idle` after the loop's tail emission completes. Same-kind double-start is rejected even during finalization, so a stop-then-fast-start can't race the prior task's tail.

- **Per-producer busy bitmask.** The scheduler's single `live_busy` boolean became insufficient with two producers. Replaced with a per-kind bitmask `{LiveMic, LiveSystem, Dictation}`. Backfill is gated unless every bit is clear; one runtime clearing its bit while another is still busy can't unblock backfill prematurely.

- **`submit()` rejects after shutdown.** `submit()` returns `Result<Receiver, SchedulerError>` and rejects with `Shutdown` once the scheduler is non-`Running`. Without this, racing `Arc<Scheduler>` clones (e.g. a session-stop path racing engine shutdown) could enqueue work into a dead worker. Taking the scheduler out of app-state alone is insufficient because Arc clones outlive the slot.

- **Mic-ownership window model — `[start, end)` intervals, not a boolean.** An earlier draft used `(flag: AtomicBool, acquired_at: AtomicU64)` with edge detection on a `dictation_was_active` per-source flag. That had a fundamental race: a dictation that opened and closed between two session polls never flipped the flag. Replaced with `DictationOwnsMic` holding an `open: Option<u64>` window and a `closed: Vec<MicWindow>` list of completed `[start, end)` intervals. Closed windows are durable — even a dictation that didn't survive a single session tick still appears as a closed window and is consumed by the next tick.

- **Dictation initializes its cursors at the window start.** A captured `dictation_start_pos` is threaded through `TranscriptionContext` into `build_initial_sources_and_backfill`. The dictation runtime's mic VAD cursor + WAV flush position both initialize at the window start instead of `current_write_pos`. Without this, audio captured between hotkey press and live-loop spawn was dropped on the floor — the session correctly skipped it (inside the open window) but dictation never read backward to the hotkey moment, so neither pipeline saw it.

- **Session loop drives off the window list, not edge detection.** Each session tick: consume any closed windows that landed since the previous tick (pre-flush pending session speech up to `window.start`, reset VAD past `window.end`); fire a one-time pre-flush for the open window if applicable; suspend mic dispatch while an open window exists OR closed windows were processed this tick (post-tick VAD output is stale relative to the post-reset state). The pre-flush preserves the user's word-in-progress at the hotkey moment; the post-window reset drops any VAD progress accumulated on dictation audio.

- **WAV muting computes precise mono-frame ranges.** `mute_dictation_window_in_wav_extraction` zeros only the samples whose mic ring-buffer positions fall inside any window — it doesn't mute whole batches. For `MicOnly` this means silence inside windows, untouched session audio outside. For `Mixed` the zeroing also wipes system audio in the overlap (loses the system audio for the dictation window — known MVP tradeoff over a more complex extract-system-only-during-window path).

- **Dictation defers to the session's in-loop watchdog for stream health.** Preflight is skipped for a dictation start during an active session; the session's watchdog already polls and routes restarts through the in-loop path that knows how to reset `SourceVadState` cursors and the session-WAV `flush_positions` on `BufferReplaced`. Without this, a preflight-driven mic-buffer replace from dictation startup would leave the session reading from stale raw-sample positions pointing into the dropped buffer.

- **Layered cleanup for the dictation flag.** Three paths cooperate so the flag is always reset:
  1. Dictation task finalizer is the canonical close: reads the final mic position, calls `dictation_owns_mic.close(end_pos)` to record an accurate `[start, end)` window, runs *before* the slot transitions to `Idle` so the session sees the closed window before any racing same-kind start could re-open one.
  2. `MicOwnershipGuard::Drop` on the runtime is the safety net for paths the finalizer doesn't reach (early errors after the window opened but before the task spawned). Calls `clear_open()` to discard the open state without recording a window — no audio was actually transcribed.
  3. Defensive `clear_for_session()` on session start, gated on the dictation slot being `Idle`. Covers the rare case where a prior dictation panic somehow escaped both the finalizer and the RAII guard.

- **Backend boundary validation.** `source_kind == Dictation` requires `MicOnly + backfill_seconds=0 + persist_audio_part=false`. Failing fast at the boundary beats letting an inconsistent dictation runtime emit segments into session storage paths.

- **Specta post-export trim.** Pre-existing trailing whitespace on JSDoc continuation lines (` * `) in `apps/desktop/src/lib/types.ts` triggered `git diff --check` after every regen. The Specta + prettier formatter combination doesn't normalize these. Added a small post-export pass in `lib.rs` that trims per line so future regens stay clean — matched to the same place the export call lives, not in a separate build step.

### What was learned

- **A boolean is the wrong model for "is this thing currently happening?" when the consumer polls at finite cadence.** The session loop polls every ~30 ms; a dictation that opens AND closes between two ticks never flips a boolean the loop observes. Closed `[start, end)` intervals are durable — the loop catches up on its next tick regardless of how fast the open phase was. The same lesson applies to any "track an event that happens between polls" problem: a list of completed events beats a flag the consumer races to read.

- **"Routing identity vs priority class" deserves to be its own concept.** When the original design wanted to add `SegmentOrigin::Dictation`, that felt natural for symmetry — but it would have invited frontend code to filter on `origin`, which is the *wrong* dimension. Origin is the scheduler's priority lane (a dictation's stop-time final flush is legitimately `FinalFlush`); routing is "what UI surface and storage path does this event belong to" (`source_kind`). Codified in PRINCIPLES.md so the next person designing a parallel pipeline doesn't conflate them.

- **A long-lived scheduler is structurally simpler than a per-session scheduler that round-trips a client through shared state.** The per-session design existed because there was only one caller of `transcribe_with` at a time. As soon as a second caller (dictation) needed to coexist, the round-trip became actively harmful — the next session's caller had to wait for the current session's `shutdown_and_return` to land back in shared state, which is unbounded under a wedged sidecar. Hoisting the scheduler removed that coupling entirely; the cleanup of `transcription-engine-dropped` and the worker-timeout-drops-client branches fell out for free.

- **`#[allow(clippy::too_many_arguments)]` is correct on framework-mandated signatures, lazy on extracted helpers.** Tauri commands need every state injection as a top-level argument because the framework wires them from the signature at registration time. There the allow is the only option. For an internal helper extracted to factor out duplication between two call sites, the right move is bundling the related arguments into a struct (`StatusProgress`) — only fall back to `#[allow]` when the args are genuinely independent borrows the helper needs `&mut` access to.

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src-tauri/src/commands/transcription.rs` | New `TranscriptionSchedulerState` (sole engine-init source of truth), `EngineConfig` for re-init matching, `MicWindow` + `DictationOwnsMic` (open + closed window list with `open()` / `close()` / `clear_open()` / `clear_for_session()` / `snapshot()`), `init_transcription_client` rewrite (idempotent on matching engine config, explicit error on mismatch), `shutdown_transcription_client` rewrite (refuses while any runtime active, calls `scheduler.shutdown_client()`). `TranscriptionClientState` deleted. |
| `apps/desktop/src-tauri/src/commands/transcription_scheduler.rs` | `JobOrigin::Dictation` priority tier, `BusyKind` per-producer bitmask, explicit `Running \| ShuttingDown \| Terminal` lifecycle, `submit()` returns `Result` + rejects when non-`Running`, new `shutdown_client` method, cached engine accessors. `shutdown_and_return` deleted. |
| `apps/desktop/src-tauri/src/commands/live_transcription.rs` | Two-slot `LiveTranscriptionState`, `LiveSourceKind` threaded through `LiveTranscriptionConfig` / `LiveSegmentEvent` / `LiveTranscriptionStatus` / `start` / `stop` / `get_status`, dictation invariant validation at the boundary, mic-ownership acquisition early in `start_live_transcription`, dictation cursor init at `dictation_start_pos`, window-driven session loop (consume closed windows + pre-flush + post-window reset), precise per-window WAV muting (`MicOnly` silence, `Mixed` overlap-zero), `dispatch_pre_window_flush` helper, final-flush mic guards via `snapshot.overlaps()`, preflight skipped for dictation-during-session, `LiveSourceKind::set_scheduler_busy` for source-kind-aware busy-bit toggling, status event source_kind filtering, `backfill-complete` suppression for dictation runtimes. The earlier `dictation_was_active: bool` per-source flag and rising/falling-edge handler are removed. |
| `apps/desktop/src-tauri/src/lib.rs` | State registrations swapped to `TranscriptionSchedulerState` + `DictationOwnsMicState`, slot-aware tray-recording check, post-Specta-export trim pass that strips trailing whitespace from `types.ts`. |
| `apps/desktop/src/hooks/useDictation.ts` | `source_kind: "dictation"` on start; segment + status listeners filter on `source_kind === "dictation"` AND matching dictation `session_id`; `stop_live_transcription("dictation")` everywhere. |
| `apps/desktop/src/hooks/useLiveTranscriptionEvents.ts` | Two-prong filter on every listener: `source_kind === "session"` AND `session_id === activeSessionId`. |
| `apps/desktop/src/stores/appStore.ts` | `source_kind: "session"` at session-start config, segment listener filters on `source_kind === "dictation"` first (with the legacy synthetic-id match kept as belt-and-suspenders), `stopLiveTranscription("session")`. |
| `apps/desktop/src/components/StatusPopover.tsx`, `apps/desktop/src/lib/tauri.ts` | `LiveSourceKind` type, status polling explicit on `"session"`. |
| `docs/ARCHITECTURE.md`, `docs/plans/shared-scheduler-dictation.md`, `CHANGELOG.md` | Long-lived scheduler + window model documented; old plan marked superseded. |

---

## Phase 38 — Frontend log bridge, buffer-replaced offset rebase, transcript hit-zone fix

### What was built

A handful of independent items that landed between Phase 37 and the `1.0.0-alpha.11` cut:

- **Unified frontend log bridge.** A new Tauri command `log_frontend(level, module, message)` routes JS log events through the existing `tracing` subscriber so they land on stderr, the rolling daily log file under `app_log_dir()`, and the in-memory ring buffer tailed by LogsPanel — alongside every native log source. The frontend bridge installs once in `main.tsx` before React mounts and captures: explicit `log.error/warn/info/debug` calls, every `console.error` / `console.warn` (forwarded; original devtools output preserved), uncaught errors via `window.onerror`, and unhandled promise rejections. Adds a "Snapshot" button in LogsPanel that captures a JS-heap snapshot on demand, plus a daemon thread on the Rust side sampling process RSS every 60 s via `memory-stats` so heap-growth analysis works on macOS where WKWebView doesn't expose `performance.memory`.
- **Buffer-replaced audio-offset rebase.** A per-source `audio_offset_anchor_seconds` on `SourceVadState` that advances on every `BufferReplaced` to `ctx.session_offset_base_seconds + session_start_instant.elapsed()`. Fixes a production bug where device-change-driven ring-buffer swaps were rewinding every post-swap chunk's `audio_offset_seconds` by however many seconds had elapsed inside the loop. The session-wide `session_offset_base_seconds` stays immutable (it represents prior-parts duration from resume) and continues to drive lag.
- **Transcript marquee hit-zone + drag perf.** Drag-select on the transcript previously bailed the moment the pointer landed on any segment bubble, so a drag starting on a bubble's top/bottom edge fell through to native text selection. The marquee now starts on any pointerdown in the container and promotes after a 6 px movement threshold; the marquee rect moved off React state and onto an imperative ref so a drag performs zero React work per pointermove.
- **Mid-session segment-action refresh.** `refreshViewSessionSegments` previously early-returned for the live session, so right-click context-menu actions wrote to the DB but didn't update `activeSessionSegments` until stop. Now refreshes both the view-session and active-session arrays when the selected session is the active one.
- **Reduce-by ducking semantics.** The dictation duck setting now lowers the *current* system volume by the slider fraction (Discord/Windows model) instead of snapping to an absolute level. Existing settings convert in the persist merge (`new = 1 − old`) to preserve effective duck strength.
- **Hour-bucket timestamps.** `formatTime` and `formatElapsed` gained an hours bucket; a third copy of the same buggy formatter inside `AIChatMessage` is removed in favor of the shared one.
- **Resize handle focus outline removed.**

### Bugs / gaps being addressed

- **Witnessed in production:** a session ran 60 min wall / 58 min audio with `final_lag_secs = 388.88 s` after two AirPods reconnect cascades; all post-swap segments were stamped ~6 min behind real session-time. Root cause: `session_start_pos` was reset to the fresh buffer's `write_pos` on `BufferReplaced` without any compensating offset bookkeeping, so the offset formula treated post-swap audio as if it had started at session-time `now − elapsed`.
- The frontend logger had three independent console paths (devtools, in-memory diagnostics, occasional `eprintln` from Rust), so a heap-growth investigation against a user's installed build had no way to read JS-side state. WKWebView's lack of `performance.memory` made a JS-only heap sampler insufficient even when the user could grant access.
- Drag-select was visibly broken in a way the transcript's owners shipped and lived with — the regression surface was specifically the bubble's top/bottom edges, which are easy to miss in casual testing.
- Mid-session segment edits/deletes/hides looked like silent no-ops because the bubble persisted on screen until `loadSessions/recovery` re-read the DB at session stop.
- Ducking-as-absolute over-quieted at low ambient volumes (50% → 20% is barely audible) and under-quieted at high ones, which read as inconsistent across users with different baseline output levels.

### Key decisions

- **Per-source anchor, immutable session base.** The session-wide offset base couldn't be mutated on `BufferReplaced` because lag is computed against it and resume relies on it staying at "prior parts duration." A per-source anchor keeps offsets correct without disturbing those invariants — the chunk offset formula now consults the anchor instead of computing from session start.
- **Surface the swapped-out buffer through `RestartReport.previous_buffer`.** The live loop atomically snapshots `new_pos_at_swap` + `new_anchor` before any await so audio captured during rescue work isn't dropped, drains only the matching source's in-flight chunk task before touching `accumulated_text`, spawns a `FinalFlush` rescue chunk from `speech_start_pos` through the normal `chunk_tasks` pipeline, appends pre-swap unflushed audio to the session WAV with resampling (MicOnly / SystemOnly; Mixed logs `buffer_replaced_wav_gap_mixed`), rebases `dictation_owns_mic` instead of clearing it, and truncates both rescues at the earliest dictation boundary so dictated audio doesn't leak into session content.
- **Frontend log bridge as a Tauri command, not a sidecar channel.** The bridge is fire-and-forget by contract — the JS caller never surfaces a failure that would itself produce an error during error reporting. Message length is capped to prevent a runaway stack trace from flooding the ring buffer. `target` is fixed at `frontend` so the subscriber filter (`frontend=debug`) is a single knob.
- **Marquee start-anywhere with movement-threshold promotion.** A pending start records on every pointerdown in the container; the marquee promotes once movement crosses the 6 px threshold. Below threshold the bubble's `onClick` runs untouched; above threshold an `onClickCapture` interceptor + one-shot `suppressNextClickRef` deterministically blocks the synthesized click. Active editing controls (contenteditable/input/textarea) are exempt from the pending path so in-edit text selection is never clobbered by marquee promotion.
- **Imperative marquee overlay, ref-driven.** ChatView keeps the rect in a ref and calls `setRect`/`hide` on the `MarqueeOverlay` via `useImperativeHandle`. The overlay div is mounted once and its style is mutated directly on each pointermove — combined with a `React.memo` on `TranscriptSegments`, a drag in the middle of the transcript performs zero React work per pointermove.
- **Reduce-by persist migration, not a settings version bump.** The duck-amount field is converted in place in the persist-merge (`new = 1 − old`) instead of bumping the settings schema version. The conversion is loss-preserving (effective duck strength is identical for the same starting volume) and avoids a migration-on-upgrade for a single-field change.

### What was learned

- **A buffer-swap is an offset-coordinate-system change.** The system has two independent ways to express "where in time is this audio": ring-buffer `write_pos` (mutates on swap) and `audio_offset_seconds` (must stay monotonic against session-time). The pre-fix code only had bookkeeping for the first, and treated the offset as a derived property of the first — which works exactly as long as the buffer never replaces underneath you. Once swap is real, the offset needs its own anchor that doesn't reset.
- **WKWebView lacks `performance.memory`.** The JS-side heap-sampling story for production diagnostics is "it works on Chromium devtools-as-debugger and nowhere else," so any heap-growth investigation against a shipped macOS build needs a Rust-side process-RSS sampler as the floor. `memory-stats` gives cross-platform RSS via `process_memory_info()` without an OS-specific dance.
- **A "click vs drag" disambiguation needs a movement threshold AND a deterministic synthesized-click suppressor.** Relying on the spec's `setPointerCapture` / pointer-event ordering is fragile under React re-renders; a one-shot ref + onClickCapture is the explicit version. Worth codifying as a pattern for any future hit-zone work in the editor.

### Files Changed

| File | Change |
|------|--------|
| `apps/desktop/src-tauri/src/commands/log_frontend.rs` (new) | Tauri command that emits a `tracing::event!` at the requested level, with `target = "frontend"` and the `module` rendered as a bracketed prefix. Caps message length. |
| `apps/desktop/src-tauri/src/lib.rs` | Registers `log_frontend` + `capture_diagnostics`; spawns the 60 s `memory-stats` RSS sampler thread on startup. |
| `apps/desktop/src/lib/logger.ts` (new), `apps/desktop/src/main.tsx` | `installFrontendLogBridge()` runs before React mounts. Hooks `console.error` / `console.warn` / `window.onerror` / `unhandledrejection`. |
| `apps/desktop/src/components/LogsPanel.tsx` | "Snapshot" button calls `captureDiagnostics`. |
| `apps/desktop/src-tauri/src/commands/live_transcription.rs` | Per-source `audio_offset_anchor_seconds`; chunk offset formula consults the anchor; on-`BufferReplaced` snapshot + drain + rescue chunk + WAV append + dictation rebase + truncation; new `marker = "buffer_replaced_rebase"` info log + `buffer_replaced_overrun` / `buffer_replaced_wav_overrun` / `buffer_replaced_wav_gap_mixed` / `buffer_replaced_drain_empty` / `buffer_replaced_sidecar_dead` warn markers. New unit tests for anchor init/reset/per-source independence/consecutive rebases/formula regression, drain-in-flight ordering, and dictation rebase + truncation invariants. |
| `apps/desktop/src/components/ChatView.tsx`, `apps/desktop/src/components/transcript/MarqueeOverlay.tsx` (new), `apps/desktop/src/components/transcript/TranscriptSegments.tsx` | Movement-threshold marquee promotion, onClickCapture + one-shot suppression, imperative overlay ref, `React.memo` on `TranscriptSegments`, hoisted constants + extracted `resetMarqueeState` helper, dedicated `pointercancel` handler. |
| `apps/desktop/src/stores/appStore.ts` | `refreshViewSessionSegments` now also refreshes `activeSessionSegments` when the selected session is the active session. |
| `apps/desktop/src/stores/appStore.ts` (duck merge), `apps/desktop/src/components/settings/DictationTab.tsx` | Reduce-by duck math + persist merge `new = 1 − old`; setting label updated to "while dictating" with a one-line subtext. |
| `apps/desktop/src/lib/format.ts`, `apps/desktop/src/components/AIChatMessage.tsx` | `formatTime`/`formatElapsed` gain an hours bucket; AIChatMessage drops its inline copy for the shared one. |
| `CHANGELOG.md`, `apps/desktop/src-tauri/Cargo.toml`, `apps/desktop/src-tauri/tauri.conf.json`, `Cargo.lock` | Version bump to `1.0.0-alpha.11`; CHANGELOG section dated and PR-cross-referenced. |

---

## Phase 39 — Windows W0: build pipeline, app hardening, first msvc compile

### What was built

The first slice of the Windows-first cross-platform rollout (ADR 0004, `docs/plans/windows-support.md`, PR #78) — the pipeline and hardening work that makes Windows builds exist at all:

- **`rust-windows` CI leg.** Every PR now compiles, clippys (`-D warnings`), and tests the whole workspace on `windows-latest` for `x86_64-pc-windows-msvc`, including a dedicated `--features webgpu` clippy pass so the sidecar's non-macOS WebGPU arm is always compiled. This is the mechanism that keeps the port from rotting — there is no local Windows dev environment; the loop is CI-compile → fix → push.
- **`build-windows` release job.** Mirrors `build-macos`: msvc sidecar build, `pnpm tauri build --target x86_64-pc-windows-msvc --bundles nsis` (NSIS only — `targets: "all"` would drag in MSI/WiX, which rejects pre-release versions like `1.0.0-alpha.N`), NSIS installer + `v1Compatible` updater zip/sig uploads, `windows-x86_64` entry in `latest.json`, and a skippable Authenticode placeholder gated at **job-level** env (a step-level `env:` is invisible to that step's own `if:`).
- **CPU-first sidecar.** The forced `cuda` feature is gone (it had no code paths and no runner could compile it). Windows builds `whisper,parakeet` — the `webgpu` feature was deliberately *not* shipped in W0 because ort links Dawn as a dynamic library (`webgpu_dawn.dll` load-time import) and nothing stages that DLL yet; flag + packaging land together in plan item #12. `AccelChoice::Auto` resolves to CPU off macOS; WebGPU is reachable only via `YAPSTACK_PARAKEET_ACCEL=webgpu` until the canary validates it (Gate C). The int8 single-file Parakeet variant is the recommended model on all hosts except CPU-only Intel macOS, and the fp32 model-size constants were corrected to HTTP-HEAD-verified actuals (the encoder blob is 2.44 GB, not 560 MB).
- **App hardening from the 8-surface Windows-readiness review:** `tauri-plugin-single-instance` registered first in the plugin chain (Windows has no Launch Services dedup; with close-to-tray, Start-Menu relaunch would spawn a second process sharing the SQLite DB, tray, and hotkeys); `core:window` minimize/toggle-maximize/close capability grants (the custom title-bar buttons render only off macOS and had no IPC permissions — dead chrome on the first canary build); per-platform tray icon (the solid-black macOS template glyph is invisible on a dark Windows taskbar; non-macOS uses the colored default window icon); `CREATE_NO_WINDOW` on both dictation spawns (`cmd /C clip`, PowerShell SendKeys).
- **CI cache hardening.** macOS Rust caches are keyed by Xcode version and `ort.pyke.io` is cached on all Rust legs (see "What was learned").

### Bugs / gaps being addressed

- A multi-agent review of every Windows-relevant code path (8 surfaces, adversarial verification per finding) found the changeset's own blockers before push: the webgpu-feature-without-DLL-staging sequencing bug, the MSI/pre-release version hard-fail, and the never-true Authenticode gate — plus the unplanned app gaps above. Findings that remain open are plan items #18–#32 in `docs/plans/windows-support.md`.
- The plan's claim "the dependency graph compiles on Windows" had never been tested; the first live msvc run proved it true up to four small lint errors — no structural problems.

### Key decisions

- **CPU-first W0, WebGPU behind an env flag.** An installable build that transcribes on CPU beats an accelerated build that fails to load. The `ort` WebGPU EP is experimental (can return wrong output), so `Auto` flips to WebGPU only after the canary proves it correct AND faster (Gate C) — never silently.
- **Draft-release canary loop.** Windows builds ship from CI only (no local Windows build env). A `v*` tag produces a **draft** GitHub release — invisible to the public — whose NSIS installer the maintainer side-loads onto a Windows canary machine for manual UAT. Publishing remains a separate explicit act.
- **Goal boards stay local.** `docs/goals/` (GoalBuddy working state) is gitignored — plans and ADRs are the committed record; boards are agent process scaffolding.

### What was learned

- **The msvc port was 4 lints away from compiling.** whisper.cpp's cmake build, cpal's WASAPI backend, and the full tauri app compiled first try; the only failures were dead-code/unreachable-pattern lints on macOS-only-constructed enum variants — the mirror image of the `cfg_attr(target_os = "macos", allow(dead_code))` idioms the codebase already had for the opposite direction.
- **`macos-latest` serves mixed runner images, and Rust caches embed absolute toolchain paths.** Cached build-script output (e.g. `objc2-exception-helper`'s `clang_rt.osx` search dir, `ort-sys`'s dylib dir under `~/Library/Caches/ort.pyke.io`) breaks relinks when restored under a different Xcode. Cache-key bumps are band-aids; keying the cache by `xcodebuild -version` and caching `ort.pyke.io` is the fix.
- **Trust but verify third-party action pins.** A `lukka/get-cmake` SHA that doesn't exist upstream fails the job at action-download time; runner images ship cmake, so the action was pure risk with no benefit.

### Files Changed

| File | Change |
|------|--------|
| `.github/workflows/ci-checks.yml` | `rust-windows` job (msvc clippy/test + webgpu clippy leg, msvc `.exe` externalBin placeholder); Xcode-fingerprinted macOS cache key; `ort.pyke.io` cache-directories. |
| `.github/workflows/release.yml` | `build-windows` job (NSIS-only, updater artifacts, checksums); `create-release` needs both platforms; `windows-x86_64` in `latest.json`; job-level-env Authenticode gate; macOS cache hardening. |
| `scripts/build-transcription-sidecar.sh` | Windows `FEATURES="whisper,parakeet"` (CPU-first; webgpu returns with Dawn staging in plan #12). |
| `crates/yapstack-transcription-sidecar/Cargo.toml`, `src/engines/parakeet.rs` | `cuda` feature removed; non-macOS `Auto` → CPU; explicit WebGPU opt-in path retained. |
| `crates/yapstack-transcription/src/model.rs` | `recommended_for_host()` → `TdtV3Int8` everywhere except CPU-only Intel macOS; size constants corrected to HEAD-verified actuals. |
| `crates/yapstack-audio/src/system/device_watcher.rs` | `to_event` allowed dead off macOS until the WASAPI watcher (plan #8) consumes it. |
| `apps/desktop/src-tauri/Cargo.toml`, `src/lib.rs` | `tauri-plugin-single-instance` (first plugin, focuses existing window); per-platform tray icon. |
| `apps/desktop/src-tauri/capabilities/default.json` | `core:window` minimize/toggle-maximize/close grants. |
| `apps/desktop/src-tauri/src/commands/dictation.rs` | `CREATE_NO_WINDOW` on the clip + PowerShell spawns. |
| `apps/desktop/src-tauri/src/commands/{permissions,system_volume}.rs` | Scoped lint allows for macOS-only-constructed variants / off-macOS-unreachable match arms. |
| `docs/adr/0004-*.md`, `docs/plans/{windows,linux}-support.md` | ADR + active Windows plan (incl. review items #18–#32) + deferred Linux gap list. |

---

## What's Not Yet Built

- **End-to-end integration tests** — capture audio, transcribe, verify text (unit + component tests now exist; integration tests still needed)
- **CI/CD pipeline** — macOS + Windows builds land in alpha.12 (rust-windows CI leg, build-windows release job); Linux builds not yet (see `docs/plans/linux-support.md`)
- **Temp file cleanup** — WAV files from capture accumulate; needs cleanup after transcription
- **Progress events during sidecar inference** — the sidecar emits `Progress` responses and the client handles them (skipping to wait for final result), but whisper-rs doesn't provide a progress callback during inference itself
- **Memory system** — Three-layer knowledge model (permanent facts, project context, daily logs) with SQLite storage, tag-based retrieval, and AI tools (`create_memory`, `update_memory`). Planned as Phase 4 of knowledge management.
- **Daily digest & knowledge gardening** — AI-driven end-of-day summarization with full knowledge gardening (extract, update, merge, promote, archive). Manual trigger with smart nudge.
- **Memory UI & vault sync** — Memory browser, backlinks panel, action item tracker, optional markdown vault sync for Obsidian interop.
- **Tag management UI** — Tag CRUD, tag picker, tag filtering in sidebar, tags in search results. Tags infrastructure exists but no dedicated management UI yet.
- **Sharing** — `shares` table exists but no sharing UI or backend logic
- **Multi-platform dictation testing** — Windows dictation paths now compile in CI but await canary UAT (paste via PowerShell SendKeys is slated for a `SendInput` rewrite, and the `cmd /C clip` clipboard write needs a Unicode-safe replacement — plan items #15/#18). Linux not yet supported.
- **Mixed-source WAV preserves system audio during dictation** — Phase 37's WAV muting zeros the (mic+system) mix sample for the dictation window, which loses the system audio for that interval. Precise impl would extract system-only audio for the overlap and substitute it. Tracked as MVP behavior in the helper rustdoc.
- **Session-stable speaker IDs** — Sortformer's chunk-local speaker IDs cause the same person to flip across speaker numbers across chunk boundaries. Diarization is force-disabled on upgrade (settings v22→23) until session-stable IDs land. The IPC + DB + sidecar plumbing is intact, so re-enabling is a one-line change once stability is solved.
