<p align="center">
  <img src="apps/desktop/src-tauri/icons/icon.png" alt="YapStack" width="128" height="128" />
</p>

<h1 align="center">YapStack</h1>

<p align="center">
  Local-first transcription and notes for your desktop.<br/>
  Your audio and transcripts never leave your machine — sync, if you want it, runs through a relay you host that can't read a word.
</p>

<p align="center">
  <a href="https://github.com/hookr-dev/YapStack/actions/workflows/ci.yml"><img src="https://github.com/hookr-dev/YapStack/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/hookr-dev/YapStack/releases"><img src="https://img.shields.io/github/v/release/hookr-dev/YapStack?include_prereleases&sort=semver" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-AGPL--3.0--only-blue.svg" alt="License: AGPL-3.0-only"></a>
</p>

---

> [!WARNING]
> **YapStack is in alpha.** Officially supported on macOS (Apple Silicon recommended); Windows and Intel Macs are experimental — see [Platform Support](#platform-support). Schema and on-disk formats may change between releases; read the [`CHANGELOG`](CHANGELOG.md) before upgrading.

<!-- TODO(owner): add a screenshot or short GIF here — docs/assets/ -->

YapStack captures mic and system audio, transcribes it on your device with [Whisper](https://github.com/ggerganov/whisper.cpp) or [Parakeet TDT](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3), and organizes everything into searchable, editable notes — with AI chat, voice dictation, and full audio playback. It works completely offline. Multi-device sync is **optional and self-hosted**: an end-to-end-encrypted relay that stores only ciphertext.

## What leaves your machine

Honesty first — the exact list:

| When | What goes where |
|---|---|
| **Normal use** | Nothing content-related, ever. Audio, transcripts, and notes are processed and stored locally. Official builds send anonymous usage analytics ([Aptabase](https://aptabase.com): feature-usage counts, app version, OS, locale — never audio, transcripts, notes, or user identifiers; can be disabled in Settings → General, and builds compiled without the analytics key send none). The app also checks the release feed for updates and downloads transcription models from Hugging Face on first use. |
| **AI features (optional)** | If you connect a cloud AI provider, the text you send it goes to that provider. Point it at a local model ([llama.cpp, LM Studio, Ollama](docs/LOCAL_LLM.md)) and nothing leaves. |
| **Sync (optional, off by default)** | Encrypted changesets and encrypted audio go to the relay **you** run. See the table below for exactly what that relay can and cannot see. |

## Features

| | |
|---|---|
| **Never miss a word** | Always-on ring buffer (up to 5 min) lets you start a recording retroactively; backfill transcribes what you almost missed while live transcription continues. |
| **On-device transcription** | Whisper (Metal) or Parakeet TDT v3 (WebGPU + int8), per-source VAD, hallucination filtering. |
| **Speaker diarization** | Optional multi-speaker labeling (Sortformer, Parakeet only), with renameable speakers. |
| **Full audio** | Sessions stream to disk; playback at 0.5×–2× with seeking; click any transcript timestamp to jump. |
| **Voice dictation** | Global-shortcut dictation slots with per-slot AI prompts, paste/copy/note output, and replayable history. |
| **AI session chat** | Per-session chat with tool calling (rename, tag, organize, save to notes — each with undo) that cites transcript segments as clickable timestamps. |
| **Notes** | Tiptap split-pane editor beside the transcript, version history with restore. |
| **Organization** | Folders, pinning, drag-and-drop, Cmd+K search across sessions, notes, and segments. |
| **Desktop-native** | System tray, customizable global shortcuts, recording indicator, close-to-minimize. |

## Install

Download the latest build from [**Releases**](https://github.com/hookr-dev/YapStack/releases) (macOS `.dmg`, experimental Windows `-setup.exe`).

Building from source: see [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md). Short version: `pnpm install && pnpm dev` (this builds the transcription sidecars first — required), `pnpm check` runs the full test gate.

## Platform Support

| Platform | Status | Notes |
|---|---|---|
| macOS (Apple Silicon) | ✅ Officially supported | Primary target. Metal Whisper, WebGPU + int8 Parakeet. |
| macOS (Intel) | ⚠️ Best-effort | Builds, reduced performance, limited testing. |
| Windows | 🧪 Experimental | CI publishes experimental installers with each release. Parakeet runs CPU int8 by default (WebGPU opt-in via `YAPSTACK_PARAKEET_ACCEL=webgpu`); Whisper is CPU. See [ADR 0004](docs/adr/0004-cross-platform-rollout-windows-first.md). |
| Linux | ❌ Not yet | No current build target. |

## Sync & self-hosting (optional)

Sync is **off by default** — at build time (a compile feature) *and* at runtime (you explicitly enable it in Settings). When you turn it on, your devices sync through a relay you host yourself (a small Docker stack: server + Postgres + MinIO). The design goal is simple: **the relay is assumed hostile and still can't read your content.**

| | |
|---|---|
| **The relay never sees** | Your transcripts, notes, titles, audio bytes, AI prompts, your password, or any unwrapped key. Everything is sealed with XChaCha20-Poly1305 on your device before upload. A CI test scans every column of the relay database for a plaintext canary and fails the build if one ever appears. |
| **The relay does see** | Metadata: that your workspace syncs and when, the number/size/timing of encrypted blobs, your device count, device names, account email, and IP. Encrypted content is opaque; its shape and timing are not. |
| **A hostile relay can** | Deny service — withhold, delay, or delete your encrypted data. It cannot decrypt it, forge a device approval, or silently alter content: tampering fails authentication and is quarantined, never applied. Known v1 limitations and the planned OPAQUE upgrade are documented in [`docs/CRYPTO_SPEC.md`](docs/CRYPTO_SPEC.md). |

New devices join via an explicit fingerprint-verification ceremony approved from an already-trusted device. Audio syncs as encrypted blobs and is fetched on demand. Losing both your password **and** your recovery code makes your synced data unrecoverable — by design; there is no server-side reset.

> **Status**: official releases include sync going forward; releases published before this documentation landed were built without it (build from source with `--features sync` if you're on one). See [`docs/self-hosting.md`](docs/self-hosting.md) to run the relay and [`docs/CRYPTO_SPEC.md`](docs/CRYPTO_SPEC.md) for the full cryptographic design.

## The covenant

[`LICENSING.md`](LICENSING.md) is a durable, structural promise about how the commercial side will and won't work:

- **AGPL-3.0-only, DCO, no CLA** — we never collect relicensing rights, so the project *cannot* be rug-pulled to a proprietary license.
- **Limits, never features** — any hosted tier can only differ in quantitative limits; no feature is ever withheld from the open-source build.
- **Self-host is the maximum tier, forever** — a self-hosted relay defaults to unlimited everything.
- **Zero phone-home** — the relay's only outbound calls are to the storage and database *you* configure. It cannot be remotely disabled.

The YapStack name and marks are covered separately by [`TRADEMARK.md`](TRADEMARK.md) — code freedom is absolute; brand confusion is what's restricted.

## Contributing

PRs welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md), [`docs/PRINCIPLES.md`](docs/PRINCIPLES.md), and [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md). Contributions are accepted under the [DCO](DCO); the full gate is `pnpm check`. This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md); report security issues per [`SECURITY.md`](SECURITY.md), not as public issues.

YapStack is built with AI pair-programming as a first-class part of the workflow. Bring whatever tools you like — we care about correctness, design clarity, and tests, not provenance.

## Architecture

Tauri v2 (Rust backend, React 19 frontend), nine workspace crates spanning audio capture, on-device transcription, the E2E crypto layer, the sync engine, and the self-hostable relay server. Start at [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); the full documentation map is [`docs/INDEX.md`](docs/INDEX.md).

## License

[AGPL-3.0-only](LICENSE). Use, modify, and redistribute freely; if you run a modified YapStack as a network service, you must share your modifications under the same license. Name and marks: [`TRADEMARK.md`](TRADEMARK.md).
