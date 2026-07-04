<p align="center">
  <img src="apps/desktop/src-tauri/icons/icon.png" alt="YapStack" width="128" height="128" />
</p>

<h1 align="center">YapStack</h1>

<p align="center">
  Real-time audio capture & transcription for your desktop. On-device. Open source.
</p>

<p align="center">
  <a href="https://github.com/hookr-dev/YapStack/actions/workflows/ci.yml"><img src="https://github.com/hookr-dev/YapStack/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/hookr-dev/YapStack/releases"><img src="https://img.shields.io/github/v/release/hookr-dev/YapStack?include_prereleases&sort=semver" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-AGPL_v3-blue.svg" alt="License: AGPL v3"></a>
  <a href="CONTRIBUTING.md"><img src="https://img.shields.io/badge/PRs-welcome-brightgreen.svg" alt="PRs welcome"></a>
</p>

---

> [!WARNING]
> **YapStack is in alpha.** Officially supported on macOS (Apple Silicon recommended). Builds on Intel Macs and Windows are experimental — see [Platform Support](#platform-support) below. Schema, APIs, and on-disk formats may still change between releases; pin a specific version for any serious use, and expect to read the [`CHANGELOG`](CHANGELOG.md) before upgrading.

> [!NOTE]
> **Vibe-coded with AI assistance.** YapStack is built using AI pair-programming as a first-class part of the workflow, and we plan to keep iterating that way. Contributions are welcome — your prompts, enhancements, modifications, and PRs back into the project. Bring whatever tools you want; we care about correctness, design clarity, and tests, not provenance.

YapStack is a privacy-first desktop app that captures mic and system audio, transcribes it locally using [Whisper](https://github.com/ggerganov/whisper.cpp) or [Parakeet TDT](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3), and organizes everything into searchable, editable notes. All processing happens on-device — nothing leaves your machine.

## Highlights

### Never miss a word

Always-on ring buffer (up to 5 min) captures audio before you hit record. Start a session and rewind to grab what you missed. Backfill transcribes retroactively while live transcription continues in parallel.

### Real-time transcription

Choose between **Whisper** (Metal-accelerated, broad language support) and **Parakeet TDT v3** (NVIDIA, faster on Apple Silicon via WebGPU + int8). Per-source VAD, hallucination filtering, two-tier prompt context for continuity. All on-device.

### Speaker diarization

Optional multi-speaker labeling via Sortformer (Parakeet only). Rename `Speaker 1` / `Speaker 2` to whatever fits the meeting.

### Full audio capture

Sessions stream to WAV incrementally. Play back at 6 speeds (0.5×–2×) with seeking. Click any transcript timestamp to jump to that moment.

### Voice dictation

Global shortcut-driven dictation with deep customization:

- **Multiple slots** — Named slots with custom global keybinds.
- **Activation modes** — Hold-to-talk or toggle.
- **AI processing** — Per-slot system prompts to transform speech (e.g. "Clean & Focus", "Create Spec").
- **Output actions** — Paste into active app, copy to clipboard, or create a new note.
- **Status bubble** — Floating overlay (listening → transcribing → processing → done).
- **History** — Past dictations grouped by day with audio replay.

### AI session chat

Per-session chat with tool calling: rename, pin, save to notes, tag, organize into folders — each with 10s undo. AI cites transcript segments as clickable timestamp chips.

### Rich notes editor

Tiptap split-pane editor alongside the transcript. Version history with restore.

### Mic + system audio

Capture mic, system audio, or both. Independent per-source VAD. Stream health monitoring with auto-restart (up to 3 attempts).

### Organization & search

Folders with icons and colors, pinning, drag-and-drop sorting, Cmd+K search across sessions, notes, and segments.

### Desktop integration

System tray with quick actions. Fully customizable global shortcuts. Recording indicator overlay. Close-to-minimize.

## Platform Support

| Platform              | Status                  | Notes |
|-----------------------|-------------------------|-------|
| macOS (Apple Silicon) | ✅ Officially supported | Primary target. Metal acceleration for Whisper, WebGPU + int8 Parakeet. |
| macOS (Intel)         | ⚠️ Best-effort           | Builds, but reduced performance and limited testing. |
| Windows               | 🧪 Experimental          | **Not officially supported.** CI/CD does not produce Windows builds. Parakeet GPU uses WebGPU/Dawn (shipping on macOS; planned for Windows behind `YAPSTACK_PARAKEET_ACCEL=webgpu` per ADR 0004) and Whisper stays CPU; you may build and run locally at your own discretion. Official Windows support is planned for a future release. |
| Linux                 | ❌ Not yet               | No current build target. |

## Getting Started

### Prerequisites

- **Rust** ≥ 1.77.2 — install via [rustup](https://rustup.rs)
- **Node.js** ≥ 22
- **pnpm** — `corepack enable && corepack prepare pnpm@latest-10 --activate`
- **cmake** — required only if building with the `whisper` feature flag

### Setup

```bash
pnpm install
```

### Development

```bash
# Full app (Tauri + Vite)
pnpm tauri dev

# Frontend only
pnpm --filter @yapstack/desktop dev
```

### Build

```bash
pnpm tauri build
```

### Testing

```bash
# Everything
pnpm check          # Rust build + test + fmt + clippy + TS typecheck + ESLint + vitest

# Selective
pnpm test           # Rust + frontend tests
pnpm test:frontend  # Vitest only
pnpm test:rust      # cargo test --all
pnpm lint           # Rust fmt + clippy + ESLint
pnpm typecheck      # TypeScript type checking
```

## Architecture

Tauri v2 app with a Rust backend and React frontend. Five crates handle distinct concerns:

| Crate | Role |
|-------|------|
| `yapstack-common` | Shared types, config, audio utilities |
| `yapstack-audio` | Lock-free ring buffers, mic/system capture, WAV export |
| `yapstack-transcription` | Model management, sidecar IPC |
| `yapstack-transcription-sidecar` | Standalone Whisper / Parakeet inference binary |
| `yapstack-desktop` | Tauri command layer, live transcription controller |

Detailed docs under [`docs/`](docs/):

- [`ARCHITECTURE.md`](docs/ARCHITECTURE.md) — data flow, crates, IPC, state, analytics.
- [`API_REFERENCE.md`](docs/API_REFERENCE.md) — Rust library + Tauri command signatures.
- [`DEVELOPMENT.md`](docs/DEVELOPMENT.md) — build, feature flags, test setup.
- [`FRONTEND.md`](docs/FRONTEND.md) — Tailwind tokens, shadcn inventory, framework stack, shortcuts, UX patterns.
- [`AI_CONTEXT.md`](docs/AI_CONTEXT.md) — AI chat context, tool registry + how to add a tool.
- [`PRINCIPLES.md`](docs/PRINCIPLES.md) — design, testing, and coding posture.
- [`LOCAL_LLM.md`](docs/LOCAL_LLM.md) — llama.cpp, LM Studio, Ollama integration.
- [`IMPLEMENTATION_LOG.md`](docs/IMPLEMENTATION_LOG.md) — phase-by-phase build history.

## Tech Stack

- [Tauri v2](https://v2.tauri.app) — native desktop shell
- [Rust](https://www.rust-lang.org) — audio capture, transcription orchestration
- [React 19](https://react.dev) + TypeScript — frontend UI
- [SQLite](https://sqlite.org) — session, note, and folder persistence
- [Whisper.cpp](https://github.com/ggerganov/whisper.cpp) — on-device speech recognition
- [Parakeet TDT v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) — NVIDIA TDT speech recognition (alternative engine)
- [ONNX Runtime](https://onnxruntime.ai) — Parakeet inference + WebGPU/CoreML execution
- [Tiptap](https://tiptap.dev) — rich text editor

## Contributing

PRs welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full workflow, [`docs/PRINCIPLES.md`](docs/PRINCIPLES.md) for design and testing posture, and [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) for build details (including local Windows builds for the curious).

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md). Security issues should be reported per [`SECURITY.md`](SECURITY.md), not as public issues.

## License

YapStack is licensed under the [GNU Affero General Public License v3.0](LICENSE). You can use, modify, and redistribute it under those terms; if you run YapStack as a network service, you must make your modifications available under the same license.
