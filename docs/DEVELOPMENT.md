# Development Guide

## Prerequisites

- Rust stable (see `rust-toolchain.toml`)
- Node.js >= 22
- pnpm
- cmake (only needed for `--features whisper`)

### macOS-specific
- Xcode Command Line Tools (provides clang/clang++)
- `brew install cmake` (for whisper feature)

## Setup

```bash
pnpm install
```

## Common Commands

### Development
```bash
pnpm tauri dev                    # Run the full app (frontend + backend)
pnpm dev:frontend                 # Vite dev server only (no Tauri)
```

### Build
```bash
cargo build --all                 # Build all Rust crates
cargo build -p yapstack-audio        # Build a specific crate
cargo build --release --all       # Release build
```

### Test
```bash
pnpm test                         # Run all tests (Rust + frontend)
pnpm test:frontend                # Frontend only (vitest)
pnpm test:rust                    # Rust only (cargo test --all)
pnpm test:watch                   # Frontend watch mode

# Targeted Rust tests
cargo test -p yapstack-audio                 # Single crate
cargo test -p yapstack-audio -- ring_buffer  # Specific module
cargo test -p yapstack-audio -- --ignored    # Hardware-dependent tests
```

### Lint & Format
```bash
pnpm lint                         # Rust fmt + clippy + ESLint (all at once)
pnpm typecheck                    # TypeScript type checking

# Granular
cargo fmt --all -- --check        # Check Rust formatting
cargo fmt --all                   # Fix Rust formatting
cargo clippy --all -- -D warnings # Rust lints
pnpm --filter @yapstack/desktop lint         # Frontend linting only
```

### Full Verification (run before committing)
```bash
pnpm check
# Expands to: cargo build --all && cargo test --all && cargo fmt --all -- --check
#   && cargo clippy --all -- -D warnings && pnpm --filter @yapstack/desktop check
#   (which runs tsc --noEmit && eslint . && vitest run)
```

## Feature Flags

### yapstack-transcription-sidecar
```bash
# Single engines
cargo build -p yapstack-transcription-sidecar --features whisper                # Whisper only (requires cmake)
cargo build -p yapstack-transcription-sidecar --features parakeet               # Parakeet only (no cmake needed; pulls ort + parakeet-rs)

# Add accelerators
cargo build -p yapstack-transcription-sidecar --features whisper,metal          # Whisper + Metal
cargo build -p yapstack-transcription-sidecar --features parakeet,coreml        # Parakeet + ORT-CoreML EP (Apple)
cargo build -p yapstack-transcription-sidecar --features parakeet,webgpu        # Parakeet + ORT-WebGPU EP

# Standard apple build (what scripts/build-transcription-sidecar.sh produces on macOS)
cargo build -p yapstack-transcription-sidecar --features whisper,parakeet,metal,coreml,webgpu

# Default features
cargo build -p yapstack-transcription-sidecar                                   # = whisper + parakeet
```

| Feature | Purpose |
|---------|---------|
| `whisper` | Whisper transcription via whisper-rs. Requires cmake. Without it, the sidecar still compiles but every `--engine whisper` request returns "engine 'whisper' not compiled in this build". |
| `metal` | Metal GPU acceleration for whisper-rs (Apple). |
| `parakeet` | Parakeet TDT v3 via parakeet-rs + ONNX Runtime. Also enables Sortformer speaker diarization (parakeet-rs ships it under the `sortformer` sub-feature, which `parakeet` enables). |
| `coreml` | Adds the ORT-CoreML execution provider for Parakeet on Apple. Strict superset of `parakeet`. The Parakeet backend's "Auto" policy will skip CoreML at load time if the model dir contains external `.onnx.data` files (known ORT bug); set `YAPSTACK_PARAKEET_ACCEL=coreml` to force-attempt. |
| `webgpu` | Adds the ORT-WebGPU execution provider for Parakeet (Metal under the hood on macOS, Dawn → D3D12 on Windows; shipping on macOS, planned for Windows behind `YAPSTACK_PARAKEET_ACCEL=webgpu` per ADR 0004). Whisper stays CPU on Windows. Set `YAPSTACK_PARAKEET_ACCEL=webgpu` to opt in. |

If the engine the sidecar was spawned with isn't compiled in (e.g. `--engine parakeet` on a `--features whisper` build), every IPC request returns `engine 'X' not compiled in this build`. The dispatcher in `main.rs` is engine-agnostic and unconditional.

### Runtime env vars (sidecar)

- **`YAPSTACK_PARAKEET_ACCEL=auto|cpu|coreml|webgpu`** — selects the Parakeet ORT execution provider. Default `auto`: uses CoreML when compiled in *and* the model has no external `.onnx.data` initializers, otherwise CPU. Empirical RTFx on Apple Silicon for Parakeet TDT v3, 2-13 s chunks: CPU 4-8×, WebGPU 4-9× (similar mean, higher variance). CoreML doesn't load for TDT v3 (external-data bug); use Auto to silently fall through.
- **`RUST_LOG`** — standard tracing override. Default sidecar filter is `info,yapstack_transcription_sidecar=debug`; default desktop filter is `info` for our crates + `warn` for noisy upstream deps. Example: `RUST_LOG=debug,ort=warn npm run dev`.

System audio capture is always compiled in via cpal loopback — no feature flag needed. Available on macOS (CoreAudio) and Windows (WASAPI). On Linux, `SystemAudioCapture::is_available()` returns false at runtime. On Windows, WASAPI loopback delivers zero packets during system silence (no audio playing) — this is normal and handled by the stream health watchdog.

## Building the Sidecar

The sidecar is a standalone binary that must be built separately and placed in `apps/desktop/src-tauri/binaries/` for Tauri to bundle it.

```bash
# Build every sidecar worker for the current platform
./scripts/build-sidecars.sh

# Build only the transcription sidecar for specific targets
./scripts/build-transcription-sidecar.sh aarch64-apple-darwin x86_64-apple-darwin
```

This builds with `--release --features whisper` and copies the binary to the binaries directory with the Tauri naming convention: `yapstack-transcription-sidecar-{target-triple}`.

The `tauri.conf.json` has `externalBin` configured to bundle `binaries/yapstack-transcription-sidecar`.

## Building the DMG

```bash
# Standard DMG build (builds sidecar + Tauri app + packages DMG)
./scripts/build-dmg.sh
```

### Environment Variables for Builds

| Variable | Required | Set by | Purpose |
|----------|----------|--------|---------|
| `APTABASE_KEY` | Yes (prod builds) | `.env` (local) or GitHub Secrets (CI) | Analytics API key, read at compile time via `option_env!()` |
| `APPLE_SIGNING_IDENTITY` | Yes (macOS DMG) | `.env` (local) or GitHub Secrets (CI) | Code signing identity for macOS |

Copy `.env.example` to `.env` and fill in your values for local builds.

## Workspace Dependencies

All shared dependencies are defined in the root `Cargo.toml` under `[workspace.dependencies]`. Crates reference them with `{ workspace = true }` and can add extra features:

```toml
# In a crate's Cargo.toml:
tokio = { workspace = true, features = ["process"] }  # adds features on top of workspace definition
```

Current workspace deps: `cpal`, `serde`, `serde_json`, `thiserror`, `tokio`, `tracing`, `hound`, `tempfile`, `reqwest`, `sha2`, `futures-util`, `tracing-subscriber`.

## TypeScript Type Generation

Rust types tagged with `#[specta::specta]` on Tauri commands are automatically exported to `apps/desktop/src/lib/types.ts` via `tauri-specta` during debug builds. This file is auto-generated and excluded from `tsconfig.json` type checking.

The DTO pattern keeps `specta::Type` out of library crates:
- Domain types in `yapstack-common`, `yapstack-audio`, etc. use only `serde`
- DTOs in `commands/*.rs` derive `specta::Type` and implement `From<DomainType>`

## Frontend Dependencies

Key frontend dependencies beyond React 19 + Vite:
- **Zustand** — State management with `persist` middleware for settings (version 23, with migrations — see `docs/API_REFERENCE.md` for the full v0→v23 history)
- **tauri-plugin-sql** — SQLite via Tauri plugin. Session, segment, note, folder persistence in `src/lib/db.ts`
- **sonner** — Toast notifications for user feedback
- **shadcn/ui** — Component library (button, card, tabs, select, popover, alert-dialog, dialog, dropdown-menu, context-menu, tooltip, slider, input, textarea, command, sheet, etc.)
- **Tauri API** — `@tauri-apps/api` for IPC, events, and path resolution
- **Tiptap** — Rich text editor (`@tiptap/react`, `@tiptap/starter-kit`, `@tiptap/extension-placeholder`, `@tiptap/pm`) for notes
- **react-resizable-panels** — Split pane layout for transcript + notes side-by-side
- **@dnd-kit** — Drag-and-drop (`@dnd-kit/core`, `@dnd-kit/sortable`, `@dnd-kit/utilities`) for session → folder organization
- **cmdk** — Command palette (Cmd+K search)
- **vaul** — Drawer component
- **Radix UI** — context-menu, dialog, tooltip, dropdown-menu, slider, collapsible (via shadcn wrappers)
- **openai** — OpenAI SDK (v6) for AI chat completions and tool calling. Used with `dangerouslyAllowBrowser: true`.
- **marked** — Markdown to HTML conversion for AI-generated notes content
- **react-markdown** — Renders AI chat message content as markdown in `AIChatMessage`
- **@tauri-apps/plugin-global-shortcut** — Global keyboard shortcuts (work when app is unfocused)
- **@tauri-apps/plugin-dialog** — Native file/folder picker dialogs
- **@tauri-apps/plugin-fs** — File system access from frontend
- **@tauri-apps/plugin-opener** — Open files/URLs with system default handlers
- **@aptabase/tauri** — Privacy-first analytics (no user IDs, no fingerprinting). Typed wrapper in `src/lib/analytics.ts`.
- **lucide-react** — Icon library used throughout the UI

## Frontend Architecture

### Component Tree
```
App (routes by ?window= param: main → MainApp, dictation → DictationBubble, recording-indicator → RecordingIndicator)
├── MainApp (close-to-minimize: hides on X, Cmd+Q still exits)
│   └── TooltipProvider
│       └── DndContext
│           └── AppLayout
│               ├── AppSidebar (navigation, folders, create session/note)
│               │   ├── FolderItem (draggable folder with context menu)
│               │   └── RecordingBeacon (pulsing indicator during capture)
│               ├── Main content
│               │   ├── StatusBar + Search/AI buttons
│               │   ├── SetupBanner (engine status)
│               │   ├── NoteCardList (grid of session/note cards)
│               │   │   ├── NoteCard (draggable, with pin/folder badges)
│               │   │   └── DictationHistoryList (when filter = dictation)
│               │   │       └── DictationFeedEntry (per-row, uses useDictationEntry hook)
│               │   ├── DictationTrayItem (sidebar tray; also uses useDictationEntry)
│               │   ├── NoteDetailView (session detail)
│               │   │   ├── SessionHeader (title, badges, actions dropdown)
│               │   │   ├── AudioPlayer (play/pause, seek, speed control — parts-aware seeking)
│               │   │   ├── AutoTagSuggestions (folder suggestion chips during recording)
│               │   │   ├── ResizablePanelGroup (split pane)
│               │   │   │   ├── ChatView (transcript bubbles, supports diarized speaker grouping)
│               │   │   │   │   └── EditableSegment (context menu: edit, copy, hide, delete; also used as read-only bubble)
│               │   │   │   └── NoteEditor (Tiptap rich text with toolbar)
│               │   │   │       └── NoteHistoryPanel (version snapshots)
│               │   │   └── AIContextProvider (context-dependent AI setup)
│               │   │       └── FloatingChatBar (AI chat overlay in notes pane)
│               │   │           └── AIChatMessage (tool badges, citations, markdown)
│               │   └── SettingsPanel (tabbed settings)
│               │       ├── AudioTab
│               │       ├── TranscriptionTab
│               │       ├── GeneralTab (theme, audio save location, recording indicator toggle, clear sessions)
│               │       ├── ShortcutsTab (keybind viewer/editor with capture mode)
│               │       └── DictationTab (enable/disable, dynamic slot config)
│               ├── ListContextBar (AI chat for non-session views)
│               └── SearchCommand (Cmd+K palette via cmdk)
├── DictationBubble (separate transparent window, always-on-top)
│   └── YapStackIcon (SVG mask-based icon component)
└── RecordingIndicator (separate transparent window, always-on-top, 56×120, click → open main)
```

### Hooks
| Hook | Purpose |
|------|---------|
| `useAutoSetup` | One-time engine initialization on mount |
| `useCaptureEvents` | Listens to backend `capture-status` and `buffer-info` events |
| `useLiveTranscriptionEvents` | Handles `live-transcription-segment`, `live-transcription-status`, `backfill-complete`, `session-part-ready`, `session-wav-error`, `live-transcription-warning`, `stream-health` events |
| `useCreateSession` | Derives `canCreate` from engine + capture state, provides `handleNew(useBackfill)` |
| `useDownloadProgress` | Listens to `model-download-progress` events |
| `useKeyboardShortcuts` | In-app keyboard shortcuts via capture-phase keydown (mounted in AppLayout). 11 actions. |
| `useGlobalShortcuts` | Global shortcuts via `@tauri-apps/plugin-global-shortcut` (mounted in App.tsx). Handles both static global shortcuts and dynamic dictation slot bindings. Re-registers when bindings or slots change. |
| `useDictation` | Voice dictation lifecycle with hold-to-talk and toggle modes (mounted in App.tsx, main window only). State machine: idle → recording → transcribing → processing → done. Controls dictation bubble window. Includes no-input detection and history persistence. |
| `useRecordingIndicator` | Shows/hides recording indicator overlay based on recording state + main window focus + setting. Positions at middle-right of screen. Listens for click-to-open events. Mounted in App.tsx (MainApp). |
| `useTrayEvents` | Listens for tray menu Tauri events (`tray:new-session`, `tray:new-session-all`, `tray:stop-session`). Guards on engine/capture state. Mounted in AppLayout. |
| `useClickOutside` | Detects clicks outside a ref element. Used by FloatingChatBar for collapse-on-click-outside. |

### Settings Persistence
Settings are stored via Zustand's `persist` middleware with `localStorage`. Schema versioned (currently **v23**). See `docs/API_REFERENCE.md` § "Settings Persistence (Zustand)" for the full migration history (v0→v23).

## Project Structure Conventions

### Error Handling
- Library crates use `thiserror` enums
- Tauri commands return `Result<_, CommandError>` — a unified tagged union (`commands/error.rs`) with kinds: `Audio`, `Transcription`, `NotInitialized`, `InvalidInput`, `NotFound`, `Internal`
- `From` impls on `CommandError` for `AudioError`, `TranscriptionError`, `std::io::Error`

### Testing

**Rust tests**: Unit tests in `#[cfg(test)] mod tests` within each source file. Hardware-dependent tests (device enumeration, mic capture) are `#[ignore]`d. Tests use `tempfile` for temporary directories/files.

**Frontend tests** (338 tests across 23 files): vitest + `@testing-library/react` + `@testing-library/user-event` + jsdom.

Test infrastructure files:
- `src/test/setup.ts` — Global setup: `@testing-library/jest-dom` matchers + `ResizeObserver` polyfill (needed by `react-resizable-panels` in jsdom)
- `src/test/tauri-mocks.ts` — Factory functions for Tauri API mocks. The full set: `tauriCoreMock`, `tauriEventMock`, `tauriWindowMock`, `tauriDpiMock`, `tauriWebviewWindowMock`, `tauriSqlMock`, `tauriDialogMock`, `tauriFsMock`, `tauriOpenerMock`, `tauriGlobalShortcutMock`, `tauriPathMock`, `tauriCommandsMock`. Factories return fresh mock objects — `vi.mock()` calls must be in the test file itself (vitest hoisting requirement).
- `src/test/match-media.ts` — `setupMatchMedia()` polyfill for `window.matchMedia` (jsdom doesn't implement it)
- `vitest.config.ts` — jsdom environment, `@/` path alias, setup file

Testing patterns:
- **Store injection**: Component tests use `useAppStore.setState()` to set store state before rendering
- **Module-level mocks**: AI library tests (`ai.test.ts`, `ai-tools.test.ts`) use `vi.mock()` at module level for OpenAI SDK and store imports
- **Factory mocks**: Tauri mocks use factory functions (not side-effect imports) because `vi.mock()` is hoisted above imports — factories ensure each test file gets fresh mocks

### File Organization
- One module per file (no nested modules except `system/`)
- `lib.rs` declares modules and re-exports key types
- `error.rs` in each crate defines the crate's error enum

## Ignored Tests

Some tests require hardware and are ignored in CI:
- `device::tests::test_default_input_device` — needs an audio input device
- `device::tests::test_list_input_devices` — needs an audio input device
- `mic::tests::test_start_stop_capture` — needs microphone access

Run them locally with:
```bash
cargo test -p yapstack-audio -- --ignored
```

## Models Directory

Whisper models are stored in the app's data directory under `models/`:
- macOS: `~/Library/Application Support/dev.yapstack.app/models/`
- Whisper models: `ggml-tiny.bin`, `ggml-base.bin`, `ggml-small.bin`, `ggml-medium.bin` — downloaded from `huggingface.co/ggerganov/whisper.cpp`
- VAD model: `ggml-silero-v6.2.0.bin` (~885KB) — auto-downloaded on the first `init_transcription_client` call (Whisper engine path) from `huggingface.co/ggml-org/whisper-vad`. Used by whisper.cpp for voice activity detection preprocessing.

## Temp File Cleanup

`export::write_wav_to_temp()` creates temp files with prefix `yapstack_capture_` that persist after creation. The caller (transcription pipeline) is responsible for cleanup. These files accumulate in the system temp directory if not cleaned up.

## Session Audio Storage

Session audio is persisted as one file per part in each session's tracked audio directory: `{session_id}.{part_index}.{wav|mp3}`. The default directory is `$APP_DATA_DIR/audio/`; users can override it per-session via the `audioSaveLocation` setting. `session_audio_parts` (migration v15) is the durable source of truth for which files belong to a session — its rows are inserted from Rust at finalize time *before* the `session-part-ready` event fires.

For live transcription sessions, the file is streamed incrementally during recording via `SessionWavWriter` (every 300 ms the loop extracts new audio from the ring buffer and appends to the file). This ensures no audio is lost for sessions longer than the ring buffer capacity (180s). When the session stops:

1. The streamed WAV is finalized in the user's `audioExportFormat` — kept as WAV, or re-encoded at `mp3Bitrate` and the source WAV is deleted (MP3 path).
2. The `session_audio_parts` row is inserted from Rust.
3. The parent dir is registered with `TrustedAudioDirs` so playback can serve from it.
4. `session-part-ready` is emitted with `{ session_id, part_index, file_path, format, duration_seconds, sample_rate }`. Empty recordings emit `session-wav-error` instead and the empty file is deleted.

Resuming a session opens a new `SessionWavWriter` at `part_index = N` (cumulative duration of prior parts becomes the segment offset base), so timestamps stay continuous across resumes. Files are not concatenated on disk; the FE concatenates parts at playback time via a parts-aware `seekTo`.

`delete_session_wav` is the legacy session-glob cleanup path used by `clearAllSessions` and for sessions that pre-date `session_audio_parts`; per-part cleanup is `delete_audio_files(paths)`, which validates each path against `TrustedAudioDirs` and surfaces failures (the FE can warn or queue retries).

The `audio-stream://` custom URI scheme protocol (registered in `lib.rs`) serves these files to the frontend `<audio>` element with range request support for seeking. Allow-list is seeded at startup from `session_audio_parts.file_path` parents and `audio_save_locations`, then extended at finalize time.
