# Architecture

## Overview

YapStack is a desktop app for real-time audio capture and transcription. Built with Tauri v2 (Rust backend) + React 19 (TypeScript frontend). Audio is captured via `cpal`, stored in lock-free ring buffers, exported to WAV, and transcribed by a sidecar process that supports two engines as first-class peers: **Whisper** (whisper-rs, 99 languages) and **NVIDIA Parakeet TDT v3** (parakeet-rs + ONNX Runtime, 25 European languages, optional Sortformer speaker diarization). The user picks the engine in Settings; the sidecar is spawned with `--engine whisper|parakeet` and dispatches IPC requests through a `TranscriptionBackend` trait so adding a third engine is one feature flag plus one impl. Sessions and segments are persisted to SQLite through a repo-owned command layer (`db_service`, single writer + reader pool); when the optional `sync` feature is enabled the same database is CRDT-tracked (cr-sqlite) and synchronized end-to-end-encrypted through a self-hosted blind relay — see `CRYPTO_SPEC.md` for the cryptographic design and `self-hosting.md` for running the relay.

## Workspace Layout

```
yapstack/
├── Cargo.toml                  # Workspace root
├── crates/
│   ├── yapstack-common/           # Shared types, config, IPC protocol
│   ├── yapstack-audio/            # Audio capture, ring buffers, mixing, WAV export
│   ├── yapstack-transcription/    # Model management, sidecar client
│   ├── yapstack-transcription-sidecar/  # Standalone Whisper/Parakeet inference binary
│   ├── yapstack-crypto/           # E2E primitives: AEAD envelopes, KDFs, roster signing, KATs
│   ├── yapstack-sync/             # Sync engine: CRDT capture/merge, outbox, audio blobs, transport
│   ├── yapstack-server/           # Self-hostable blind relay (axum + Postgres + MinIO)
│   └── yapstack-entitlements/     # Typed quantitative limits (Limit::Unlimited | Max) + admin contract
├── apps/
│   └── desktop/
│       ├── src/                # React frontend
│       └── src-tauri/          # Tauri backend (commands, state management)
└── scripts/
    ├── build-sidecars.sh                  # Wrapper: builds every sidecar worker
    ├── build-transcription-sidecar.sh     # Build the transcription sidecar for target triples
    └── download-test-fixtures.sh
```

## Crate Dependency Graph

```
yapstack-desktop
├── yapstack-audio
│   └── yapstack-common
├── yapstack-transcription
│   └── yapstack-common
└── yapstack-common

yapstack-transcription-sidecar (standalone binary)
└── yapstack-common
```

`yapstack-transcription-sidecar` is a separate binary, not linked into the desktop app. It communicates via JSON-line IPC over stdin/stdout.

## Module Responsibilities

### yapstack-common
Shared types used across all crates. No business logic.
- `audio.rs` — `deinterleave_to_mono()`, `resample()` (rubato sinc, no-op when from_rate == to_rate). Used by `yapstack-audio` internally, `live_transcription.rs` directly, and the Parakeet/Sortformer backends in `yapstack-transcription-sidecar`.
- `config.rs` — `AppConfig`, `AudioConfig` (only `capture_history_seconds`), `TranscriptionConfig` with serde
- `types.rs` — Domain types (`CaptureState`, `CaptureSource`, `PermissionStatus`, `EngineKind`, etc.). `TranscriptSegment` carries an optional `speaker_id` for Parakeet+Sortformer output. IPC protocol (`SidecarRequest`, `SidecarResponse`) is engine-agnostic; `Transcribe` carries an optional `diarization: bool`.
- `engines.rs` — Static `engine_catalogue()` exposing `EngineDescriptor { kind, display_name, languages, supports_diarization, supports_initial_prompt }` per engine. Single source of truth for engine capabilities + supported languages, consumed by both backend validation and the frontend cascading dropdowns (via the `get_engine_catalogue` Tauri command).

### yapstack-audio
All audio capture and processing. Core of the application.
- `ring_buffer.rs` — Lock-free SPSC ring buffer for real-time audio. `UnsafeCell` + atomics. Producer API is zero-alloc (safe for audio callbacks). Consumer API allocates snapshots, including bounded `snapshot_range()` reads for hard stop boundaries.
- `device.rs` — Device enumeration via `cpal`
- `mic.rs` — Microphone capture stream management. `stream_error: Arc<AtomicBool>` for cpal error propagation.
- `system/` — System audio capture (macOS via cpal loopback on default output device, Windows stub, fallback `Unavailable`)
- `manager.rs` — `AudioManager` orchestrates mic + system capture and exposes the position-based extraction API (`buffer_positions`, `extract_since`, `extract_since_until`, `extract_sources_since`, `peek_energy_rms`) consumed by the live-transcription loop. Stream restart (`restart_mic`, `restart_system_audio`) preserves ring buffers across device rebinds.
- `mixer.rs` — Pure functions for mixing mic + system audio with gain/normalization
- `export.rs` — WAV file export via `hound` (16-bit PCM, f32 clamping). `SessionWavWriter` for incremental streaming WAV during live sessions.
- `capture.rs` — Data types: `BufferPositions`, `SeparateExtraction` (the position-based extraction shapes consumed by the live-transcription loop)
- `error.rs` — `AudioError` enum with `From` impls for cpal and hound errors

### yapstack-transcription
Model management and sidecar client. No whisper-rs / parakeet-rs dependencies — those live entirely in the sidecar.
- `model.rs` — `ModelManager` manages three model families with HuggingFace download + streaming SHA-256 verification:
  - **Whisper** (single ggml file): `ModelSize { Tiny, Base, Small, Medium }`, `download`, `verify_checksum`, `delete`, `list_all`.
  - **Whisper VAD** (Silero, ~885KB, auto-downloaded): `vad_model_path`, `download_vad_model`, `ensure_vad_model`.
  - **Parakeet** (multi-file ONNX bundle in `models_dir/parakeet-<variant>/`): `ParakeetVariant::TdtV3`, `parakeet_model_dir`, `parakeet_is_available` (checks all required files), `download_parakeet` (loops over the variant's `files()` list with per-file SHA verify), `delete_parakeet`, `ensure_parakeet`.
  - **Sortformer** (single ONNX file, ~50MB): `SortformerVariant::V2_1`, `sortformer_model_path`, `download_sortformer`, `ensure_sortformer`.
- `client.rs` — `TranscriptionClient` spawns the sidecar process and communicates via JSON-line IPC. `spawn()` takes `engine: EngineKind`, `model_path`, `vad_model_path?` (Whisper-only), `sortformer_model_path?` (Parakeet-only), and `coreml_cache_dir?` (Parakeet-only). `transcribe_with(audio, language, prompt, diarization)` exposes the per-call diarization flag. `respawn()` re-spawns the sidecar preserving engine + all paths + `next_id`.
- `error.rs` — `TranscriptionError` with variants for download, sidecar, timeout, IO, HTTP

### yapstack-transcription-sidecar
Standalone binary. Reads JSON-line requests from stdin, writes responses to stdout. Logging goes to stderr.

**CLI**: `yapstack-transcription-sidecar [--engine whisper|parakeet] [--model PATH] [--vad-model PATH] [--sortformer-model PATH] [--coreml-cache-dir PATH]`

**Architecture**: `engines/mod.rs` defines a `TranscriptionBackend` trait + shared text-cleanup helpers. Concrete impls in `engines/whisper.rs` (whisper-rs + flash attention + Silero VAD) and `engines/parakeet.rs` (ParakeetTDT + Sortformer post-pass + ORT EP selection). `main.rs` is a thin IPC dispatcher that picks one backend at startup and forwards every request through the trait. When the engine the sidecar was spawned with isn't compiled in (e.g. `--engine parakeet` on a `--features whisper` build), every IPC request returns `engine 'X' not compiled in this build`.

**Whisper backend (`engines/whisper.rs`)**: Loads whisper-rs model with flash attention, converts audio to 16kHz mono (stereo→mono averaging + sinc interpolation resampling via rubato), transcribes WAV files with optional `initial_prompt` for context continuity, filters hallucination segments via `should_include_segment()`.
- Whisper params: `greedy(best_of=1)`, `no_context(true)`, `logprob_thold(-1.0)`, `max_tokens(100/200)`, `suppress_blank(true)`, `suppress_nst(true)`, `temperature_inc(0.2)`, `no_speech_thold(0.45)`, `single_segment(adaptive)`
- **Silero VAD**: Optional `--vad-model` arg enables voice activity detection as whisper.cpp preprocessing (threshold 0.5, min_speech 250ms, min_silence 100ms, pad 30ms). Skips non-speech audio before decoding, reducing hallucinations.
- Hallucination filtering: drops empty text, special tokens (`[BLANK_AUDIO]`, `[MUSIC]`), low-confidence segments (< 0.4), excessive word/phrase repetition (3+ consecutive identical words with punctuation-normalized detection via `normalize_for_repetition()`), and 47 known filler/hallucination patterns ("thank you", "thanks for watching", "yeah", "um", "so", etc.) at marginal confidence (< 0.6).

**Parakeet backend (`engines/parakeet.rs`)**: Loads `ParakeetTDT::from_pretrained(model_dir, exec_config)` from a directory containing `encoder-model.onnx` + `encoder-model.onnx.data` + `decoder_joint-model.onnx` + `vocab.txt`. Resamples audio to 16 kHz mono via `yapstack_common::audio::resample` before calling `transcribe_samples` (parakeet-rs 0.3.5 rejects other rates despite its docs). Token-level timestamps grouped into segments at 0.5 s silence gaps + 12 s soft cap. Same `should_include_segment` post-filter as Whisper (mostly a no-op for Parakeet which doesn't hallucinate the same patterns).
- **ORT execution provider** selected at startup via `YAPSTACK_PARAKEET_ACCEL=auto|cpu|coreml|webgpu` env var. `auto` uses CoreML when compiled in *and* the model dir contains no external `.onnx.data` files (TDT v3 *does* — so Auto falls through to CPU on this model). Per-load CPU fallback at the backend level catches CoreML/WebGPU load failures and degrades silently — the sidecar never returns "no model loaded".
- **Sortformer post-pass** (when `Transcribe.diarization=true` and the sidecar was spawned with `--sortformer-model`): runs Sortformer on the same resampled 16 kHz audio, returns `Vec<SpeakerSegment>` with sample-range + `speaker_id`. `assign_speakers()` maps speaker ranges onto produced transcript segments by maximum-overlap (samples → ms → segment range overlap) and populates `TranscriptSegment.speaker_id`. Sortformer model is loaded lazily on first diarization request to avoid the cost when diarization is off.

### yapstack-desktop (Tauri app)
Tauri commands layer. Thin wrappers that convert between domain types and DTOs (with `specta::Type` for TypeScript generation).
- `commands/error.rs` — `CommandError` tagged union with 6 error kinds (`Audio`, `Transcription`, `NotInitialized`, `InvalidInput`, `NotFound`, `Internal`). All Tauri commands return `Result<_, CommandError>`. `From` impls for `AudioError`, `TranscriptionError`, `std::io::Error`.
- `commands/audio.rs` — Device listing, capture start/stop, capture status, buffer info, `peek_capture_energy` (RMS readout used by the recording beacon). `MixConfigDto::sanitized()` validates gain values at the command boundary.
- `commands/capture.rs` — Audio cleanup commands only (`delete_audio_files` for per-part cleanup, `delete_session_wav` for legacy session-glob cleanup). Session lifecycle and audio finalization are owned by `commands/live_transcription.rs`.
- `commands/transcription.rs` — Model management, transcription, sidecar lifecycle. Locks released before async I/O.
- `commands/live_transcription.rs` — Real-time transcription controller with per-source VAD (`SourceVadState`), concurrent backfill processing (Silero-driven `vad_chunk_historical_audio` shares boundary choices with the live loop), prompt context windowing, prompt decay, stream health monitoring with auto-restart. Shared `TranscriptionContext` struct for immutable config passing. Clones an `Arc<TranscriptionScheduler>` from app-level state at session start; the scheduler outlives every session. Each runtime carries a `LiveSourceKind` (`Session` or `Dictation`) that's threaded onto every emitted event so the frontend routes by source kind, not by string-matching session ids. Two-slot runtime state (`session`, `dictation`) with `Idle | Starting | Running | Stopping` lifecycle so the same kind can't double-start during finalization. Streams session WAV incrementally via `SessionWavWriter`.
- `commands/transcription_scheduler.rs` — Long-lived single-worker priority queue in front of the sidecar lane (`FinalFlush > Dictation > Live (mic/system round-robin) > Backfill`, backfill gated while *any* producer bit in `{LiveMic, LiveSystem, Dictation}` is set). Constructed once at engine init and shared by every live runtime. Sole caller of `TranscriptionClient::transcribe_with`, which makes sidecar respawn race-free. `submit()` returns `Result<Receiver, SchedulerError>` and rejects with `Shutdown` once terminal so racing `Arc<Scheduler>` clones can't enqueue into a dead worker. See "Transcription Scheduler" below.
- `commands/dictation.rs` — `clipboard_paste` command for voice dictation output. Writes text to system clipboard via `pbcopy` (macOS) or `clip` (Windows), optionally triggers auto-paste via `osascript` keystroke simulation.
- `commands/system_volume.rs` — `apply_volume_duck`, `restore_volume`. Generic system-output volume control wrapping the `system_volume` mechanism module. Policy of *when* to duck lives in callers (currently dictation only); the commands themselves know nothing about dictation.
- `system_volume.rs` (mechanism, not a command module) — Snapshots `(device_id, level)` on first apply so restore targets the *originally* ducked device even if the default output changes mid-session (AirPods connect, USB DAC unplug). Only ever lowers — never raises — so a user already below the target is left alone. macOS uses CoreAudio FFI against `kAudioHardwareServiceDeviceProperty_VirtualMainVolume`; Windows / Linux are no-op `Unsupported` stubs behind the same trait.
- `commands/mod.rs` — `show_overlay_panel`, `hide_overlay_panel` commands (cfg-gated: macOS uses NSPanel via `tauri-nspanel`, non-macOS falls back to WebviewWindow show/hide). `get_autostart_enabled`, `set_autostart_enabled` commands via `tauri-plugin-autostart`.

## Data Flow

### Audio Capture
```
Microphone/System Audio
    → cpal callback (real-time thread, zero-alloc)
    → AudioRingBuffer.write() (lock-free, Release ordering)
    → AudioRingBuffer.snapshot() (app thread, Acquire ordering)
    → Vec<f32> samples
```

### Session Capture
```
start_live_transcription(config with session_id, audio_save_location?, audio_export_format)
    → Resolves the audio dir: audioSaveLocation if set, else $APP_DATA_DIR/audio/
    → Computes part_index from existing session_audio_parts rows for this session
      (0 for fresh sessions; N when resuming a session that already has parts)
    → Creates SessionWavWriter at $AUDIO_DIR/{session_id}.{part_index}.wav
    → Every 300ms: extract_since() from ring buffer → append to WAV file
    → On stop:
        1. Snapshot BufferPositions immediately and extend them by a short
           bounded tail grace, then bound all final reads to
           [last_flush_positions, stop_positions). Samples written after
           that boundary are ignored for this session.
        2. Final flush, then finalize per audioExportFormat:
           • format = "wav" → keep the WAV
           • format = "mp3" → re-encode at mp3Bitrate (lame) and DELETE the source WAV
        3. Register parent dir with TrustedAudioDirs so the audio-stream:// handler can serve it
        4. If config.persist_audio_part is true (default for real sessions),
           insert a session_audio_parts row from Rust (durable source of truth).
           Dictation passes false here because its synthetic session_id has no
           sessions row — the path is recorded against dictation_history instead.
        5. Emit "session-part-ready" with { session_id, part_index, file_path, format,
           duration_seconds, sample_rate }
        6. Empty recordings (0 samples written) emit "session-wav-error" instead and the
           file is deleted

No audio lost regardless of session length. Each Resume produces a new part; the FE
concatenates parts in part_index order for playback and seeking. There is no
separate "instant capture" or `start_session`/`end_session` Tauri surface — every
recording (including dictation) goes through the live transcription pipeline against
its own session id.
```

### Transcription
```
Frontend: init_transcription_client(engine, whisper_model?, parakeet_variant?, enable_diarization)
    → Validates the engine's selected model is on disk; returns NotFound if not
    → For Parakeet+diarization, also ensure_sortformer() (auto-downloads ~50MB Sortformer ONNX)
    → For Whisper, ensure_vad_model() (auto-downloads ~885KB Silero ONNX)
    → Resolves CoreML cache dir under $APP_DATA_DIR/cache/coreml/ (Parakeet only)
    → spawns yapstack-transcription-sidecar with: --engine, --model, [--vad-model | --sortformer-model + --coreml-cache-dir]
    → JSON-line IPC over stdin/stdout

Live transcription loop (one chunk):
    → TranscriptionClient.transcribe_with(audio_path, language, prompt, diarization)
    → sends {"type":"transcribe", ..., diarization} to sidecar stdin
    → sidecar dispatches via TranscriptionBackend trait:

      Whisper backend:
        → loads WAV (any sample rate / channel count)
        → converts to mono (averaging channels) + resamples to 16kHz (sinc via rubato) if needed
        → runs whisper inference with optional initial_prompt
        → segments[*].speaker_id = None

      Parakeet backend:
        → loads WAV at native rate
        → resamples to 16kHz mono via yapstack_common::audio::resample
        → calls ParakeetTDT.transcribe_samples (CoreML/WebGPU/CPU per env var)
        → groups word-level tokens into segments at silence gaps
        → if diarization=true: also runs Sortformer on the same audio,
          assigns speaker_id by max-overlap onto produced segments
        → drops opts.initial_prompt silently (TDT decoder has no prompt input)

    → returns {"type":"transcription", text, segments[* speaker_id], duration_ms}
    → TranscriptionResult { text, segments, duration_ms }
```

### Live Transcription
```
Frontend: start_live_transcription(config)  // config.source_kind selects slot
    → Validates source-kind invariants (Dictation must be MicOnly,
      backfill_seconds=0, persist_audio_part=false) and same-kind double-
      start is rejected unless the target slot is Idle.
    → Clones Arc<TranscriptionScheduler> from app-level TranscriptionSchedulerState
      (the scheduler is constructed once at init_transcription_client
      and outlives every session — see "Transcription Scheduler" below).
    → If source_kind == Dictation: snapshots the current mic write_pos into
      DictationOwnsMic.acquired_at, then sets DictationOwnsMic.flag = true.
      The session's mic-side live loop reads this on its next tick, flushes
      any pending session speech up to acquired_at as a Live chunk (rising
      edge), then suspends mic processing for the dictation window.
    → Creates TranscriptionContext (immutable: scheduler handle, app_handle, config, start time, dictation_owns_mic)
    → If config.session_id set: creates SessionWavWriter at $AUDIO_DIR/{session_id}.{part_index}.wav
      (audioSaveLocation if set, else $APP_DATA_DIR/audio/; part_index resumes from existing parts)
    → Spawns async task with two concurrent tracks:

    Track 1 — Backfill (if backfill_seconds > 0):
        1. Rewind cursors by backfill_seconds from current write_pos
        2. Extract historical audio per source into owned Vec<f32> (no longer
           reads the ring buffer after this snapshot — backfill is durable
           against ring-buffer churn for the rest of the session)
        3. Run vad_chunk_historical_audio() over each source — same Silero
           state machine the live loop uses, so backfill and live share
           boundary choices and there's no quality gap at the seam
        4. Skip entirely silent windows (no chunk emitted)
        5. Split each historical chunk into the fixed backfill quantum before
           scheduler submission. Sidecar inference is not preemptible, so this
           bounds the worst-case live delay from an already-running backfill job.
        6. Submit each chunk to the scheduler at JobOrigin::Backfill
           priority, interleaved across sources (chunk 0 all sources,
           chunk 1, etc.), and *await each chunk's scheduler response*
           before submitting the next. Per-chunk prompt context and
           in-order segment emission both depend on this serial wait, so
           the submitter is what's actually doing the draining. The
           scheduler still admits Backfill jobs only when it has no
           FinalFlush/Live work queued and the live loop reports idle.
        7. Emit "live-segment" events with origin="backfill"
        8. Emit "backfill-complete" event when the submitter finishes (or
           immediately on session start if effective backfill is 0 — for a
           short ring buffer or a resumed session — so the FE's
           "backfill in progress" affordance always clears)
        9. Stop does *not* cancel the submitter; the live loop waits up to
           5 min for it to drain naturally. Chunks not yet submitted at
           that timeout are lost (they live in the submitter's stack as
           Vec<f32>, not on any durable queue), and the abort handle is
           the last-resort escape hatch for a wedged submitter.

    Track 2 — Live VAD loop (runs concurrently; per-call diarization gated on
              ctx.config.diarization, initial_prompt gated on
              ctx.engine_profile.uses_initial_prompt):
        1. Poll every tuning.poll_interval (Whisper 300ms; Parakeet 100ms)
        2. Single lock: per-source extract_source_audio() for Silero +
           extract_since() for WAV flush. Raw mono samples, not RMS.
        3. Outside the lock:
            a. Write WAV samples (disk I/O doesn't hold AudioManager)
            b. Feed each source's samples through the shared Silero V5
               session via score_stream() — returns one probability per
               32 ms frame, maintained across polls by per-source LSTM
               state in SileroSource
        4. poll_vad() per source, once per Silero frame — state machine
           iterates every probability so intra-poll speech isn't missed.
           Best-action summary (ForceChunk > Chunk > None) picked per tick.
        5. On VadAction::Chunk (silence detected), ForceChunk (max duration),
           or force-drain after a prior max-size slice:
            a. Extract the oldest contiguous max-size range from the source.
               If more already-captured audio remains, preserve the source
               cursor and drain one chunk per source without growing the
               scheduler queue. Status/pressure events report the remaining
               drain backlog so slow-sidecar catch-up is visible in the UI.
            b. write_wav_to_temp() → temp WAV
            c. Submit JobRequest to the scheduler at JobOrigin::Live
            d. Emit "live-segment" event with segments + metadata + origin
        6. On stop: set the live-busy scheduler gate, skip normal VAD polling,
           idle cursor advancement, stream-health restarts, and unbounded WAV
           flushing. Wait only for the bounded tail grace, copy final source
           tails only up to the stop boundary, then exit the loop.

    → After loop exits (cleanup):
        1. Copy final pending live chunks from [speech_start_pos, stop_positions)
           into memory before waiting on in-flight tasks.
        2. Bounded final WAV flush from [flush_positions, stop_positions)
        3. Drain in-flight chunk tasks (15 s — they hold no
           Arc<TranscriptionClient> clones under the scheduler, so no abort
           phase is needed)
        4. Submit the copied final chunks at JobOrigin::FinalFlush so closing
           words outrank pending Live and Backfill work, then clear the
           live-busy gate so copied backfill can drain.
        5. Finalize per audioExportFormat (WAV kept / MP3 re-encoded + WAV deleted)
        6. Register the parent dir as trusted; if config.persist_audio_part is true (default),
           insert the session_audio_parts row from Rust. Dictation passes false here and stores
           the path on dictation_history instead. Then emit "session-part-ready" with
           { session_id, part_index, file_path, format, duration_seconds, sample_rate }
           (or "session-wav-error" on empty recordings)
        7. Wait for the backfill submitter task to finish (capped at 5 min).
           The submitter awaits each chunk's scheduler response before
           submitting the next, so this is a wait for actual transcription,
           not for an enqueue loop. Chunks not yet submitted at submitter
           abort time are not on any durable queue and are lost.
        8. Slot transitions to Idle. The scheduler is long-lived and
           keeps running across sessions / dictations; engine-level
           shutdown is the explicit `shutdown_transcription_client`
           command (gated on every runtime slot being Idle), which
           calls `scheduler.shutdown_client()` to drain the worker and
           tear down the inner TranscriptionClient.
    → Per-source VAD: SourceVadState tracks mic/system independently
    → Prompt context: per-source accumulated_text (up to 1000 chars) truncated to prompt_context_chars (default 350) for Whisper initial_prompt
    → Prompt decay: clears both shared_prompt and per-source accumulated_text after prompt_decay_silence_seconds (default 5s) of
      all-source silence. Prevents stale context from causing hallucination after long pauses.
      Backfill seeding is one-shot (prompt_seeded_from_backfill guard prevents re-seeding after decay).
    → Stopped via oneshot channel from stop_live_transcription()
```

### Transcription Scheduler

`commands/transcription_scheduler.rs` is a single-worker priority queue in
front of the sidecar lane. The sidecar processes IPC requests serially on the
model side, so any two `transcribe_with` calls land in stdin FIFO order with
no way to prefer fresh live speech over historical backfill. The scheduler
fixes that.

**Priorities** (highest first):
1. `FinalFlush` — closing-words chunks emitted at session stop.
2. `Live` — real-time chunks during the session, with strict mic/system
   round-robin so neither source can starve the other in mixed mode.
3. `Backfill` — historical audio, drained only when the higher tiers are idle
   and the live loop has cleared the live-busy gate.

**Single-owner client.** The scheduler owns the sole `Arc<TranscriptionClient>`
clone for the duration of a session, which makes sidecar respawn race-free:
when a transcribe call fails and the sidecar is dead, the worker's
`Arc::try_unwrap` always succeeds because no other task holds a clone. The
older design had to fall back to "another task still holds the Arc, retry on
next chunk" branches — that's gone.

**Live-busy gate.** The live loop sets a scheduler guard while speech is
active, live/final chunks are in flight, a force-drain backlog exists, or stop
finalization is copying/dispatching bounded live tails. `Backfill` jobs remain
queued during that window even if the sidecar worker becomes free; `FinalFlush`
and `Live` jobs still dispatch immediately by priority.

**Stop semantics.** `stop_live_transcription()` carries a `StopRequest` with
the current `BufferPositions` extended by a short tail grace. The live loop
stops normal ingestion and stream health work, waits only for that bounded
tail window, copies final live tails from the stop-bounded ranges, flushes the
session WAV only up to the same positions, drains in-flight live tasks, then
submits the copied tails at `FinalFlush` priority. Only after those bounded
live tasks finish does it clear the live-busy gate so the backfill submitter
can drain. The live loop then awaits the backfill submitter's join (capped at
5 min); the submitter awaits each chunk's scheduler response before submitting
the next, so this wait is a wait for actual transcription to finish, not for
an enqueue loop to drain. Anything still in the submitter's `Vec<f32>` chunk
list at the join timeout is lost — there is no durable scheduler queue that
holds chunks the submitter never submitted. After the submitter exits,
the scheduler keeps running — it's now a long-lived app-level singleton
that outlives every session, so there's no client round-trip at session
end. The session simply finalizes its WAV, releases its busy bits via
the per-producer bitmask, and the slot transitions to `Idle`. Engine
shutdown happens via the explicit `shutdown_transcription_client`
command, which calls `scheduler.shutdown_client()` to drain the worker
and tear down the inner `TranscriptionClient`; that command is gated on
both runtime slots being `Idle`. Backfill audio captured into the
submitter's `Vec<f32>` is durable against ring-buffer churn for as long
as the submitter is alive; ring-buffer audio written after the bounded
stop tail is ignored for the stopped session.

**Concurrent dictation.** Hitting a dictation hotkey while a session is
recording opens a *mic-ownership window* in `DictationOwnsMic`: the
runtime captures the current mic ring-buffer write position as
`start_pos`, threads it into the dictation runtime's
`TranscriptionContext::dictation_start_pos`, and the dictation runtime
initializes its mic VAD cursor + WAV flush position there so audio
captured between hotkey press and live-loop spawn is included. The
dictation task's finalizer reads the final mic write position, calls
`close(end_pos)` to record a closed `[start, end)` window, and the slot
transitions to `Idle`. The session loop drives off the window list (not
edge-detection on a boolean), so a fast dictation that opens-and-closes
between two session polls still appears as a closed window and is
consumed by the next tick's window-handling block. The session's mic
dispatch is suspended while an open window exists; the WAV writer mutes
samples whose ring-buffer positions overlap any window. `MicOnly` mutes
to silence; `Mixed` zeros the overlap (loses system audio for the
dictation duration, documented as MVP behavior); `SystemOnly` is never
affected. The `JobOrigin::Dictation` priority tier puts dictation chunks
ahead of session live chunks in the scheduler queue.

**Job submission API.** `scheduler.submit(JobRequest { origin, source,
wav_path, language, initial_prompt, diarization })` returns a
`oneshot::Receiver<JobOutcome>`. `JobOutcome` carries the
`TranscriptionResult` (or a `SchedulerError`) and the worker's `wall_ms`
for pressure telemetry.

**Pressure metrics.** The status surface exposes two related-but-distinct
numbers. `lag_seconds` is wall-clock minus the latest committed audio offset
and folds in *every* source of delay — chunking, scheduler queueing, sidecar
warmup, inference. `live_drain_backlog_seconds` is the source-local
already-captured audio that hadn't been dispatched yet at the moment a Live
chunk was sent; non-zero specifically attributes the slowness to the live tier
force-draining preserved audio because the sidecar is slower than realtime.
One can be zero while the other isn't, so both are kept.

### Stream Health Monitoring

Two coordinated paths recover from a Stream that's gone bad: a *symptom-based* in-loop watchdog for stream errors and stalls, and an *event-driven* device broker for OS device-change auto-failover.

**In-loop watchdog (`check_stream_health()`, every 300 ms tick):**
1. **cpal error callback flag** (instant) — each capture stream stores an `Arc<AtomicBool>` (`stream_error`) the cpal error callback flips on any stream error. Checked via `mic_has_stream_error()` / `system_has_stream_error()`.
2. **`write_pos` stall watchdog** (~2 s latency) — `SourceVadState` tracks `last_seen_write_pos` and `last_write_pos_advance`. Stall when `write_pos` hasn't advanced for `STREAM_STALL_THRESHOLD_SECS` (2.0 s).

**Watchdog fields on `SourceVadState`**:
- `last_seen_write_pos: usize`, `last_write_pos_advance: Instant`, `restart_attempts: u32`.

**Device broker (`device_broker` module, always-on):**
- Owns the receiving end of `yapstack_audio::system::device_watcher::DeviceEventSink`. The audio crate stays sync and runtime-agnostic; the broker lives in the Tauri layer because it needs `tokio` and `AppHandle`.
- Listens on four Core Audio properties: `kAudioHardwarePropertyDevices`, `kAudioHardwarePropertyDefaultInputDevice`, `kAudioHardwarePropertyDefaultOutputDevice`, `kAudioHardwarePropertyDefaultSystemOutputDevice`.
- Coalesces bursty events in a 250 ms debounce window. On flush:
  - `DeviceListChanged` *or any default-* kind → re-enumerate, emit `devices-changed` (FE replaces its device list, reconciles `selectedMicDeviceId` if its device disappeared, and picks up refreshed `is_default` flags).
  - `DefaultInputChanged` → if Mic is following the system default OR the explicit pick is no longer alive, dispatch a Mic restart. Otherwise the explicit pick stays.
  - `DefaultOutputChanged` / `DefaultSystemOutputChanged` → coalesce both into one System audio restart.
- Dispatch is `kAudioDevicePropertyDeviceIsAlive`-gated (one re-check at +250 ms) to absorb the AirPods/Bluetooth revert window — replaces the previous unconditional 200 ms `thread::sleep` workaround.
- Routing: prefers a `RestartIntent` channel into the live-transcription loop when a session is active, so `SourceVadState.cursor`, Silero stream state, and the session-WAV flush position are reset by the loop's existing bookkeeping. Falls back to `AudioManager::restart_*` directly when no live loop is running but a non-live Capture is.

**Restart behavior**: up to `STREAM_RESTART_MAX_ATTEMPTS` (3) attempts per Source. `AudioManager::restart_mic()` / `restart_system_audio()` stop the old stream and start a new one on the same ring buffer when format matches — no audio data is lost. On `Mixed` Capture, terminal restart-failed on either Source stops both Sources fail-fast.

**Events**: emits `"stream-health"` (`StreamHealthEvent { source, status, message, bound_device_name? }`) and `"devices-changed"` (`AudioDeviceInfoDto[]`). Status values: `"restarted"` (success), `"restart_failed"` (will retry), `"restart_abandoned"` (max attempts reached). The FE turns successful restarts into "Switched to {bound_device_name}" toasts.

## State Management

### Backend (Rust)
Five `Arc<Mutex<T>>` (or `Arc<T>`) states managed by Tauri:
- `AudioManagerState` — audio capture lifecycle, ring buffers, sessions
- `ModelManagerState` — model download/delete/list (Whisper + Parakeet + Sortformer)
- `TranscriptionSchedulerState` — `Arc<Mutex<Option<InitializedEngine>>>`, the single source of truth for engine initialization. The scheduler owns the `Arc<TranscriptionClient>` for its full lifetime as the sole `transcribe_with` caller; both the session and dictation runtimes clone `Arc<TranscriptionScheduler>` from here. `init_transcription_client` is idempotent on a matching engine config; engine swap is an explicit two-step (`shutdown_transcription_client` first, then re-init).
- `LiveTranscriptionState` — `Arc<Mutex<LiveTranscriptionSlots>>` with `session` and `dictation` slots, each carrying explicit `Idle | Starting | Running | Stopping` lifecycle. Same-kind double-start is rejected even during finalization.
- `DictationOwnsMicState` — `Arc<DictationOwnsMic>` (mutex-protected window list). Records the open dictation window's start position and a list of closed `[start, end)` windows. Drives session mic suspension, WAV muting, and dictation-runtime cursor initialization.

Startup also runs `db::ensure_runtime_schema()` *before* tauri-plugin-sql wires up — it opens the SQLite DB directly via `rusqlite`, sweeps stale `recording`-status sessions left by a prior crash, and creates the `audio_save_locations` table used by reconciliation on next startup. The Parakeet+Sortformer `segments.speaker_id INTEGER` column is added separately by the frontend's `getDb()` after migrations run, sidestepping a "ghost" v11 entry that some local dev DBs picked up from another branch.

### Frontend (TypeScript)
- **State**: Zustand store (`stores/appStore.ts`) with persisted settings (**version 23**, migrations for schema changes). Settings include capture source, **selected engine** (`"Whisper" | "Parakeet"`, peers — Whisper is the upgrade-safe default), Whisper model size, **selected Parakeet variant**, **diarization enabled**, **per-session `speakerNames` map** (renames `Speaker N` → custom labels, persisted client-side), language, VAD params, prompt context, prompt decay silence, theme, sidebar state, buffer size, AI settings, shortcut bindings, audio save location, dictation settings, `showRecordingIndicator`. The store also caches the engine catalogue (`engineCatalogue: EngineDescriptorDto[]`) and Parakeet/Sortformer download status (`parakeetModels`, `sortformerStatus`) loaded on `autoSetup`.
- **Persistence**: SQLite via `tauri-plugin-sql` (`lib/db.ts`) for sessions, segments, notes, note versions, folders, session_folders (many-to-many), chat messages, dictation history, tags, session_tags, FTS5 search tables, and `session_audio_parts`. DB file at app data dir. 15 SQL migration versions; `segments.speaker_id INTEGER` is added by the frontend's `getDb()` after migrations run.
- **Type generation**: Specta-generated types in `src/lib/types.ts` (auto-generated, excluded from tsconfig). Tauri command wrappers in `src/lib/tauri.ts`.
- **Serialization queue**: `onLiveSegment` uses a promise queue (`segmentQueueTail`) to prevent concurrent backfill + live events from racing on DB writes.
- **Hooks**: `useAutoSetup` (engine init), `useCaptureEvents` (backend status push), `useLiveTranscriptionEvents` (segment/phase/backfill/session-part-ready/session-wav-error events), `useCreateSession` (session creation guard), `useDownloadProgress` (model download), `useKeyboardShortcuts` (in-app capture-phase keydown in AppLayout), `useGlobalShortcuts` (Tauri global-shortcut plugin in App.tsx), `useDictation` (hold-to-talk or toggle mode voice dictation lifecycle in App.tsx; also registers Escape as a global hotkey while non-idle for cancel), `useDictationEntry` (per-row state for `DictationFeedEntry` and `DictationTrayItem` — copy/play/move-to-note/delete handlers), `useRecordingIndicator` (show/hide floating overlay when recording + unfocused, in App.tsx), `useTrayEvents` (listen for tray menu actions, in AppLayout), `useChatMessages` (chat message lifecycle: send, stream, tool execution, undo, DB persistence).
- **Navigation**: Three views: `"note-list"` | `"note-detail"` | `"settings"`. `ListFilter` supports `{ type: "all" | "pinned" | "folder" | "dictation", folderId?: string }`.
- **Views**: `AppLayout` → `AppSidebar` + main content (`NoteCardList` | `NoteDetailView` | `SettingsPanel`). `NoteDetailView` handles multiple layouts:
  - **Active recording**: `SessionHeader` + `AutoTagSuggestions` + resizable split pane (`ChatView` left with backfill indicator, `NoteEditor` right). Stop / pause-style controls live in `SessionHeader`; the floating `RecordingIndicator` window provides the cross-app stop affordance via `useRecordingIndicator`.
  - **Completed transcription**: `SessionHeader` + `AudioPlayer` + resizable split pane (`ChatView` left, `NoteEditor` + `NoteHistoryPanel` right)
  - **Manual notes**: `SessionHeader` + full-width `NoteEditor` + `NoteHistoryPanel`
- **Settings UI**: Tabbed (`AudioTab`, `TranscriptionTab`, `GeneralTab`, `ShortcutsTab`, `DictationTab`). `TranscriptionTab` renders an **engine → model → language cascade**: engine radio (Whisper/Parakeet, peers), conditional model picker (`ModelSection` for Whisper sizes, new `ParakeetModelSection` for Parakeet variants), language dropdown derived from `engineCatalogue` (clamps to "auto" when current language isn't in the new engine's supported set), Switch-style **diarization toggle** (greyed out unless engine supports it; first enable lazily downloads Sortformer + re-inits the client). `GeneralTab` manages theme, audio save location, and session clearing. `DictationTab` manages dictation enable/disable, dynamic slot configuration (name, keybind, output action, AI prompt), and slot add/delete.
- **Transcript view (`TranscriptSegments.tsx`)**: Wrapper around `EditableSegment` rendered by `ChatView`. Falls back to a flat segment list when no segment carries a `speaker_id` (Whisper sessions render unchanged). Otherwise groups consecutive same-speaker segments under a `Speaker N` header with a 4-color palette. Speaker headers are inline-editable; renames persist to per-session `speakerNames` Zustand map.
- **AI prompts**: `ai-prompts.ts::getSystemPrompt(directive, ..., { hasSpeakers })` injects a `SPEAKER_INSTRUCTION` paragraph telling the model to attribute statements correctly when the active session has diarization data. `ai.ts::assembleTranscriptContext(segments, speakerNames?)` adds `(SpeakerName)` prefixes per segment.
- **Global features**: `SearchCommand` (Cmd+K fuzzy search via `cmdk`), drag-and-drop sessions into folders via `@dnd-kit`.
- **AI Chat**: `AIContextProvider` wraps `FloatingChatBar` to provide context-dependent AI chat. `FloatingChatBar` delegates chat logic to the `useChatMessages` hook and input UI to `ChatInputBar` (with `ContextPill` and `ModelPickerPill` sub-components in `components/chat/`). Factory functions in `ai-context.ts` assemble sources/tools/prompts for session vs. multi-session (folder) contexts. `resolveListContext()` centralizes context resolution for non-session views (all, pinned, folder, dictation). `ListContextBar` renders the floating chat bar for list-level contexts. See [AI Chat & Tool Calling](#ai-chat--tool-calling) below.
- **Dictation**: `DictationBubble` renders in a separate Tauri window. `YapStackIcon` provides a reusable SVG mask-based icon component. See [Dictation](#dictation) below.
- **Recording Indicator**: `RecordingIndicator` renders in a separate transparent always-on-top window (56×120). Shows a pulsing YapStack icon with drag handle when recording and main window is unfocused. Clicking the icon opens the main window to the active session. Controlled by `useRecordingIndicator` hook (uses `commands.showOverlayPanel("recording-indicator")` / `hideOverlayPanel`). Toggleable via `showRecordingIndicator` setting.
- **System Tray**: Enhanced tray menu with dynamic items based on capture/recording state. Menu: Open YapStack → Status (Idle/Listening/Recording) → Start/Stop Listening (disabled during recording) → New Session + backfill submenu → Stop Session → Quit. Tray icon uses a dedicated monochrome PNG template (`tray-icon.png`). Events dispatched via Tauri `emit`/`listen` to `useTrayEvents` hook.
- **Close-to-minimize**: Main window `closeRequested` event is intercepted — window hides instead of closing. Cmd+Q still exits (OS-level). Tray "Quit" calls `app.exit(0)`.
- **Cross-platform overlay windows**: Dictation and recording-indicator windows are converted to NSPanels on macOS via `tauri-nspanel` (`#[cfg(target_os = "macos")]`). NSPanel config: `PanelLevel::MainMenu`, non-activating (`StyleMask::nonactivating_panel`), stationary + `move_to_active_space` + `full_screen_auxiliary` collection behavior. On non-macOS: `visibleOnAllWorkspaces: true` in `tauri.conf.json` + standard WebviewWindow show/hide. Frontend uses `commands.showOverlayPanel(label)` / `commands.hideOverlayPanel(label)` — platform-agnostic.

### Primitives

The persisted domain model. All writes flow through `tauri-plugin-sql` via `lib/db.ts`; the DB file lives in the app data dir.

- **Sessions** (`sessions`) — capture + transcription root. Owns a title, pinned flag, start/end timestamps, zero-or-more `session_audio_parts` rows (one per recording run; resumed sessions append a new part), and optional note via `notes.session_id` FK. `session_type` distinguishes `"transcription"` (recorded + transcribed) from `"manual"` (no transcript — note-only).
- **Segments** (`segments`) — transcript rows keyed by session, with timestamps, text, and an optional `speaker_id`. Writes serialize through the `segmentQueueTail` promise queue to prevent backfill / live races. `speaker_id` is added by the frontend's `getDb()` after migrations run (outside the migration list — see API_REFERENCE schema note).
- **Notes** (`notes`) + **note versions** (`note_versions`) — rich-text body (Tiptap HTML). Versions persist the history for restore. One note per session.
- **Folders** (`folders`) — nested via `parent_id` (root has null). Carry `icon`, `color`, `description`. The `description` feeds the AI multi-session system prompt as an organizational layer (see `AI_CONTEXT.md`).
- **Session ↔ folder junction** (`session_folders`) — many-to-many. A session can live in multiple folders; branch conflicts detected by `findBranchConflicts()` in `lib/folder-tree.ts`.
- **Chat messages** (`chat_messages`) — per-session and per-list history (session, folder, all, pinned, dictation). Keyed by `chatContextKey()`.
- **Dictation history** (`dictation_history`) — per-slot entries with audio path, transcript, and AI-processed output.
- **Tags** (`tags`, `session_tags`) — flat, AI-applied metadata layered on top of folders. Migration v11 added `tags` (id, name, color, created_at) and `session_tags` (session_id, tag_id, source `manual`/`ai`, confidence, created_at). Folders remain the primary organizational primitive; tags are deliberately non-hierarchical and don't carry descriptions. See [`AI_CONTEXT.md`](./AI_CONTEXT.md) § Tags for the design rationale.

### See also

- [`FRONTEND.md`](./FRONTEND.md) — Tailwind tokens, shadcn inventory, framework stack, keyboard shortcuts, UX interaction language.
- [`AI_CONTEXT.md`](./AI_CONTEXT.md) — AI chat context surfaces, tool registry, how to add a new tool, folder hierarchy → prompt mapping, tags schema and auto-folder suggestions.
- [`PRINCIPLES.md`](./PRINCIPLES.md) — Design, testing, and coding posture.

## AI Chat & Tool Calling

### Overview

The AI chat system uses OpenAI-compatible APIs (OpenAI, OpenRouter, or custom endpoints) with **function/tool calling** to let the AI directly mutate session data. This creates a two-way bridge: the AI reads transcript + notes + folder context, and writes back title changes, note updates, pin state, folder classification, and tags.

The system is designed around two self-contained registries — **actions** (prompt directives with phased tool chaining) and **tools** (mutations with undo and structured results) — so that extending the AI requires minimal file changes.

### Architecture

```
User triggers action (quick action button) or freeform message
    → useChatMessages.handleSend(actionDef?: ActionDefinition)
    → Resolve directive: actionDef.directive ?? GENERAL_DIRECTIVE
    → Assemble context from enabled sources (transcript, notes)
    → If action + session context: inject folder tree via assembleFolderTreeForActions()
    → buildSystemPrompt(directive, contextParts, attachments)
        → getSystemPromptWithToolContext(directive, transcript, notes, attachments, sessionMeta, folderTree?)
    → Multi-turn tool execution loop (up to MAX_TOOL_ROUNDS=3):
        → streamChatWithTools(client, model, messages, tools)
            → OpenAI streaming API with tool schemas
            → yields: token events (streamed text) + tool_call events (accumulated)
        → If tool_calls present:
            → Update ToolExecution[] state (status: "running" → per-tool spinners in UI)
            → For each tool_call: executeTool(name, args, toolContext)
                → Mutates DB (title, notes, pin, folders, tags)
                → Returns ExecutedTool with undoData + result string
                → Update ToolExecution status → "done" (checkmark) or "error"
            → Build tool-role messages with result strings
            → Append assistant + tool messages to conversation
            → Loop: make new streaming call with full conversation
        → If no tool_calls: break (LLM finished)
    → Prepend [tool:name] badge lines to message content (DB persistence format)
    → Persist to chat_messages DB
    → Post-tool refresh via onToolsExecuted → getToolEffects() → refresh by effect category
    → Show toast with 10s undo window
```

### File Structure

| File | Responsibility |
|------|---------------|
| `lib/ai-actions.ts` | Self-contained action definitions — each carries phased `directive` string with tool chaining instructions |
| `lib/ai-tools.ts` | Self-contained tool registry — schema + executor + undo + `affects` + `result` per tool. Ten tools. |
| `lib/ai-prompts.ts` | Prompt assembly: `GENERAL_DIRECTIVE`, `getSystemPrompt(directive, ...)`, citation/notes guidance |
| `lib/ai-context.ts` | Context provider types, factory functions for session/multi-session context assembly, `assembleFolderTreeForActions()` |
| `lib/ai.ts` | OpenAI client, streaming (with and without tools), context assembly, `ToolExecution` types, markdown→HTML via `marked` |
| `lib/transcription.ts` | Whisper-facing utilities: `buildVocabularyHints()` for folder/tag name injection |
| `lib/auto-tag.ts` | `FolderSuggestionTracker` — keyword matching for folder suggestion chips during recording |
| `components/AIContextProvider.tsx` | React context provider, source toggle state management |
| `components/FloatingChatBar.tsx` | Chat UI shell, delegates chat logic to `useChatMessages` hook and input to `ChatInputBar` |
| `components/chat/ChatInputBar.tsx` | Input textarea, file attachments, action buttons, context/model pills |
| `components/chat/ContextPill.tsx` | Toggleable context source pill (transcript, notes) |
| `components/chat/ModelPickerPill.tsx` | Popover-based model selector grouped by provider |
| `hooks/useChatMessages.ts` | Chat message lifecycle: send, stream, multi-turn tool execution, undo, DB persistence |
| `hooks/useAutoTag.ts` | Folder suggestion hook — processes live segments, manages suggestion state, pushes vocab updates |
| `components/AIChatMessage.tsx` | Message rendering with `ToolExecutionBlock` and citation chips |
| `components/ToolExecutionBlock.tsx` | Tool execution status rows — per-tool icon, spinner/checkmark, label, detail |
| `components/AutoTagSuggestions.tsx` | Inline folder suggestion chips below session header during recording |

### Action Definitions (`ai-actions.ts`)

Actions are self-contained `ActionDefinition` objects. Each carries its own system prompt directive — no external lookup table needed.

```typescript
interface ActionDefinition {
  id: string;              // e.g. "summarize", "key-points", or a custom ID
  label: string;           // Display label in the action menu
  description: string;     // Tooltip / secondary text
  icon: LucideIcon;        // Icon component
  requiresTranscript?: boolean;  // Hide from manual (notes-only) sessions
  directive: string;       // The full system prompt directive for this action
}
```

Built-in actions (all use **two-phase tool chaining**):

| Action | Phase 1 (classify) | Phase 2 (write) | Behavior |
|--------|-------------------|-----------------|----------|
| `summarize` | `search_folders` → `add_session_to_folder` | `update_title` + `tag_session` + `save_to_notes` | Structured summary informed by folder context. `requiresTranscript: true`. |
| `key-points` | `search_folders` → `add_session_to_folder` | `tag_session` + `save_to_notes` (append) | 5-15 bullet points, appended to existing notes |
| `action-items` | `search_folders` → `add_session_to_folder` | `tag_session` + `save_to_notes` (append) | Numbered action items, appended |
| `meeting-minutes` | `search_folders` → `add_session_to_folder` | `update_title` + `tag_session` + `save_to_notes` | Professional minutes format. `requiresTranscript: true`. |

Phase 1 directives explicitly say "Do NOT call other tools yet — wait for folder context results." This forces the multi-turn loop to execute, sending the folder description chain back before Phase 2.

When no action is selected (freeform chat), `GENERAL_DIRECTIVE` from `ai-prompts.ts` is used as the fallback directive. Freeform chat does NOT inject the folder tree — it's action-only context.

`getActionsForSession(sessionType)` filters actions — manual sessions hide `requiresTranscript` actions (summarize, meeting-minutes) since there's no transcript to process.

`AIActionType` is `string` (not a closed union), so custom action IDs pass typecheck without modification.

**Adding a custom action** requires only creating an `ActionDefinition` object with a `directive` string and passing it via the `actions` prop on `AIContextProvider`. No core files need modification.

### Tool Registry (`ai-tools.ts`)

Tools are self-contained `ToolDefinition` objects registered at module load. Each has:
- **`schema`** — OpenAI `ChatCompletionTool` (JSON Schema for the function parameters)
- **`execute(args, ctx)`** — Performs the mutation, captures previous state for undo, returns `ExecutedTool`
- **`undo(undoData, ctx)`** — Reverts the mutation using captured state
- **`affects`** — Array of `ToolEffect` categories for declarative post-execution refresh

```typescript
type ToolKind = "read" | "mutate";  // gates Undo eligibility + "Session updated" toast
type ToolEffect = "session-meta" | "notes" | "organization" | "transcript";

interface ToolDefinition {
  kind: ToolKind;
  schema: ChatCompletionTool;
  execute: (args, ctx: ToolContext) => Promise<ExecutedTool | null>;
  undo: (undoData, ctx: { sessionId: string }) => Promise<void>;  // no-op for kind: "read"
  affects?: ToolEffect[];
}

interface ExecutedTool {
  name: string;
  label: string;
  detail: string;            // Human-facing badge text
  observation?: string;      // Text the LLM sees as the tool result for multi-turn chains
  toolCallId?: string;       // OpenAI tool_call ID for multi-turn
  result?: string;           // Mirrored into observation when omitted (legacy compat)
  undoData?: unknown;        // Required for kind: "mutate"
}
```

`ToolContext` is itself a discriminated union — `SessionToolContext { scope: "session", ... }` for single-session chats and `RetrievalToolContext { scope: "retrieval", allowedSessionIds }` for folder/pinned/all chats. Mutating tools narrow with `requireSessionContext(ctx)` so the multi-session path never has to fabricate empty `sessionId`/`currentTitle` values.

Built-in tools (registered in `ai-tools.ts`):

| Tool | Params | Effects | What it does |
|------|--------|---------|-------------|
| `update_title` | `{ title: string }` | `session-meta` | Sets session title (max 80 chars). Undo restores previous title. |
| `save_to_notes` | `{ content: string, mode: "replace" \| "append" \| "prepend" \| "find_replace", find?: string }` | `notes` | Converts markdown → HTML via `marked`. `replace` overwrites, `append` adds below with separator, `prepend` adds above with separator, `find_replace` does a surgical substring swap (requires `find`, plain-text replacement only — markdown won't render). Undo restores previous note content. |
| `pin_session` | `{ pinned: boolean }` | `session-meta` | Pins/unpins session (only toggles if state differs). Undo restores previous pin state. |
| `tag_session` | `{ add: string[], remove?: string[] }` | `organization` | Adds/removes tags. Creates new tags on-the-fly if they don't exist. Source tracked as `"ai"`. |
| `search_folders` | `{ query?: string }` | (none) | Read-only. Returns folder tree (or matches against `query`) with descriptions and `folder_id`s for the LLM to choose from. Phase 1 of folder-aware actions. |
| `add_session_to_folder` | `{ folder_id: string }` | `organization` | Classifies session into a folder by id (chosen from `search_folders` results). Handles branch conflicts. Returns the folder's hierarchical description chain in `result` so Phase 2 of two-phase actions sees the parent context. |
| `search_sessions` | `{ query: string, filters: { folder_id: string \| null, pinned: boolean \| null } \| null, limit?: number }` | (none) | Read-only. FTS5 search across session titles, segment text, and notes. `filters` is `null` for an unfiltered search or an object with both keys (each may be null individually). |
| `search_dictations` | `{ query: string, limit?: number }` | (none) | Read-only. FTS5 search across `dictation_history`. |
| `get_session_context` | `{ session_ids: string[], scope: "segments" \| "notes" \| "summary" \| "all" }` | (none) | Read-only. Expands a list of `session_ids` returned by `search_sessions` into the requested artifact (transcript chunks, notes, both, or a future summary). Hard-capped at 5 ids per call. When the chat context carries `allowedSessionIds`, out-of-scope ids are rejected. |
| `replace_in_transcript` | `{ find: string, replace: string, case_sensitive: boolean }` | `transcript` (refresh segments) | Edits segment text in the durable DB rows — fixes typos / mis-transcriptions surgically. Capped at 50 affected segments per call. Undo restores the pre-call text on every touched segment. |

**Post-tool refresh** uses `getToolEffects(toolNames)` to determine what to refresh based on `affects` metadata:
- `"session-meta"` → refresh sessions list + viewSession
- `"notes"` → increment note refresh counter
- `"organization"` → refresh session folders, session tags, and tags list
- `"transcript"` → refresh segments (used by `replace_in_transcript` so the live transcript view picks up the durable edits)

**Adding a new tool** requires only a `registerTool({...})` call in `ai-tools.ts` with an `affects` tag. No changes needed in `FloatingChatBar.tsx`, `NoteDetailView.tsx`, or `AIChatMessage.tsx`.

### Tool Context

Before executing tools, `useChatMessages` fetches a `ToolContext` via the provider's `getToolContext()`:
```typescript
// Single-session chat:
{ scope: "session", sessionId, currentTitle, currentNote, isPinned, segments?, tags?, folderNames?, folderIds?, allowedSessionIds? }
// Folder / pinned / all chat:
{ scope: "retrieval", allowedSessionIds }
```
Re-fetched each turn of the multi-turn loop so mutations from previous turns are reflected. `tags`, `folderNames`, `folderIds` are populated via parallel DB queries (`getSessionTagIds`, `listTags`, `listAllSessionFolders`). `segments` are used by `save_to_notes` to convert `[[seg:ID]]` citations to interactive `<span>` elements and by `replace_in_transcript` to plan per-segment edits.

### Prompt Assembly (`ai-prompts.ts`)

The prompt system is **directive-first** — the caller passes a `directive: string` (from an `ActionDefinition` or `GENERAL_DIRECTIVE`) rather than an action ID.

```typescript
getSystemPrompt(directive: string, transcriptText, noteText, attachments) → string
getSystemPromptWithToolContext(directive: string, ..., sessionMeta, folderTreeContext?) → string
```

Assembly layers:
1. **Directive** — the action-specific system prompt (from `ActionDefinition.directive` or `GENERAL_DIRECTIVE`)
2. **Citation instruction** — appended only when transcript is present (instructs `[[seg:ID]]` format)
3. **Notes guidance** — always appended (instructs append vs. replace mode)
4. **Context sections** — `## Session Transcript`, `## Notes`, `## Attached Files` (only when non-empty)
5. **Session metadata** — current title, pin state, notes presence
6. **Folder tree** (actions only) — full folder hierarchy with descriptions, injected as a `"folder-tree"` context part by `useChatMessages` when an action is triggered. Built by `assembleFolderTreeForActions()` → `assembleFolderTreeContext()`. Not included in regular freeform chat to keep prompts lean.

For multi-session contexts, `getMultiSessionSystemPrompt(sessionsContext, attachments, folderContext?)` builds a read-only prompt with optional `FolderContextLayer[]` hierarchy (folder name + description pairs from root to leaf).

### Streaming with Tool Calls

### Multi-Turn Tool Execution

`streamChatWithTools()` is an async generator that yields typed `StreamEvent`s:
- `{ type: "token", content }` — Text delta from the model (rendered incrementally)
- `{ type: "tool_calls", calls }` — Accumulated tool calls (emitted after stream ends, before `done`)
- `{ type: "done" }` — Stream complete

The consumer (`useChatMessages`) wraps this in a **multi-turn loop** (up to `MAX_TOOL_ROUNDS=3`):

1. Stream a response — accumulate tokens and tool calls
2. If tool calls present: execute them, build `tool`-role messages with results, append to conversation
3. Make a new streaming call with the extended conversation
4. Repeat until the LLM produces text without tool calls, or max rounds hit

This enables **phased tool chaining**: the LLM calls `search_folders` in turn 1, picks a `folder_id`, calls `add_session_to_folder` in turn 2, receives the folder context chain as a tool result, then uses that context to write an informed summary in turn 3.

**Tool execution state** is tracked via `ToolExecution[]` on `ChatMessage`:
```typescript
type ToolExecutionStatus = "running" | "done" | "error";
interface ToolExecution { name: string; label: string; detail?: string; status: ToolExecutionStatus; }
```

Each tool gets a `"running"` entry when the turn starts (showing a spinner in the UI), then transitions to `"done"` (checkmark) or `"error"` as execution completes. Updates are per-tool, not per-turn — the user sees individual tools resolve one by one.

**Null tool results**: When `executeTool` returns null (no-op, e.g., title already matches), a `"No action needed."` result is still sent as the tool-role message. The OpenAI API requires a response for every `tool_call` ID.

**Abort propagation**: The abort signal is checked between turns and between individual tool executions within a turn.

### Undo System

Every tool execution captures `undoData` (the previous state). After tools execute:
1. A toast appears: "Session updated: Title, Notes [Undo]"
2. The undo window lasts 10 seconds (`UNDO_TIMEOUT_MS`)
3. Clicking Undo calls `undoToolCalls()` which reverses tools in LIFO order
4. The `[tool:...]` badge lines are stripped from the persisted message
5. Post-tool refresh fires via `onToolsExecuted` → `getToolEffects()`

### Transcript Citations

Segments are passed to the AI with IDs embedded in the timestamp: `[seg:abc123 0:30] Hello everyone...`. The system prompt instructs the AI to cite segments using `[[seg:ID]]` markers. In `AIChatMessage`, these are parsed and rendered as:
- Clickable pill buttons showing the timestamp (e.g., "1:15")
- Tooltip on hover showing the segment text preview (first 80 chars)
- Click seeks audio to that segment's `audio_offset_seconds` via `handleSeek`

In saved notes (via `save_to_notes` tool), citations are converted to `<span data-segment-ref>` elements that the Tiptap `SegmentReference` extension renders as interactive pills.

### Tool Badge Persistence

When tools execute, badge lines are prepended to the assistant message content for DB persistence:
```
[tool:add_session_to_folder] Added to "ConsignR"
[tool:update_title] Q1 Budget Planning
[tool:tag_session] Added: meeting, planning
[tool:save_to_notes] Notes saved

Here's a summary of your session...
```
These persist in `chat_messages` DB. On load, `AIChatMessage.parseToolBadges()` converts them to `ToolExecution[]` (all `status: "done"`). During live streaming, the structured `toolExecutions` field on `ChatMessage` takes precedence — the component renders `ToolExecutionBlock` with per-tool status (spinning loader → checkmark). Both live and persisted messages render through the same `ToolExecutionBlock` component.

### Context Provider Pattern

`AIContextProvider` wraps `FloatingChatBar` and provides `AIContextValue` via React context. Factory functions in `ai-context.ts` create sources, tools, actions, and system prompt builders for different contexts:

- **Session context** (`createSessionSources`, `createSessionTools`, `createSessionSystemPromptBuilder`) — Single session with toggleable transcript and notes sources. All ten tools available. Actions filtered by `getActionsForSession(sessionType)`. `ToolContext.scope = "session"`.
- **Multi-session context** (`createMultiSessionSources`, `createMultiSessionTools`, `createMultiSessionSystemPromptBuilder`) — Folder / pinned / all chat. Exposes the four retrieval tools (`search_sessions`, `get_session_context`, `search_folders`, `search_dictations`); mutating tools are intentionally absent because they need a single `sessionId`. `allowedSessionIds` pins retrieval to the chat's filter. `ToolContext.scope = "retrieval"`.
- **Dictation context** (`createDictationSources`, `createDictationTools`, `createDictationSystemPromptBuilder`) — Dictation history chat. Exposes `search_dictations` only — folder/session retrieval tools are deliberately *not* surfaced because dictation lives in its own table and reusing the multi-session toolset would have leaked unrelated session search into a dictation-scoped chat.
- **List context** — `resolveListContext(filter, sessions, folders)` returns a `ListChatContext` (contextKey, sources, tools, systemPromptBuilder, placeholder) for any `ListFilter` type. `ListContextBar` renders the `AIContextProvider` + `FloatingChatBar` for non-session views.

Sources are toggleable via `toggleSource()` — users can enable/disable transcript and notes context independently. The `contextKey` (from `chatContextKey()`) determines chat message identity — switching sessions or folders resets the chat history. `AIContextValue` includes a `placeholder` string for context-specific input hints.

`SystemPromptBuilder` takes `(directive: string, contextParts, attachments)` — the directive is resolved by `FloatingChatBar` from the `ActionDefinition` before calling the builder.

### Markdown → HTML

Notes content in Tiptap is HTML. The AI generates markdown. Conversion uses `marked` (the `markdownToBasicHtml` function). Prompts explicitly instruct the AI to use `**bold text**` for section labels instead of `#` headings, because Tiptap renders `<h1>`/`<h2>` tags disproportionately large in the notes editor.

### Provider Compatibility

The system uses the OpenAI SDK with configurable `baseURL`, supporting:
- **OpenAI** (direct) — Full tool calling support
- **OpenRouter** — Routes to Claude, GPT, etc. with OpenAI-compatible tool calling
- **Custom** — Any OpenAI-compatible endpoint (e.g., local Ollama, LM Studio)

OpenRouter requests include `HTTP-Referer` and `X-Title` headers per their requirements.

### Extensibility

The system is designed so that user-created actions and tools require minimal file changes:

**Custom action** — just data:
```typescript
const customAction: ActionDefinition = {
  id: "my-analysis",
  label: "My Analysis",
  description: "Custom analysis prompt",
  icon: Sparkles,
  directive: "You are a specialized analyst that...",
};
// Pass via actions={[...builtIns, ...customActions]} to AIContextProvider
```

**Custom tool** — one `registerTool()` call in `ai-tools.ts`:
```typescript
registerTool({
  affects: ["session-meta"],
  schema: { type: "function", function: { name: "my_tool", ... } },
  execute: async (args, ctx) => { /* ... */ },
  undo: async (undoData, ctx) => { /* ... */ },
});
// Automatically picked up by getRegisteredTools() and getToolEffects()
```

### Key Design Decisions

1. **Self-contained action definitions** — Each `ActionDefinition` carries its own `directive` string. No separate lookup table or closed enum. Custom actions are just data objects.
2. **`AIActionType` is `string`** — Not a closed union. Any action ID passes typecheck. Built-in IDs documented in JSDoc.
3. **Directive-first prompt assembly** — `getSystemPrompt` takes a `directive: string`, not an action ID. The caller resolves the directive from the action definition. This decouples prompt construction from the action registry.
4. **Tool effect categories** — `affects: ToolEffect[]` on each `ToolDefinition` declares what side effects a tool has. `getToolEffects()` aggregates effects from executed tools. Post-tool refresh is driven by effects, not hardcoded tool names.
5. **Multi-turn tool calling** — Tool results are sent back to the model as `tool`-role messages, enabling phased workflows. The LLM classifies a session into a folder (turn 1), receives the folder context chain, then writes an informed summary (turn 2). Capped at `MAX_TOOL_ROUNDS=3` to prevent runaway loops. Each tool returns a `result` string (for the LLM) distinct from `detail` (for the human-facing badge).
6. **Modular tool registry** — Tools are self-contained objects with schema + executor + undo + affects. Adding a tool touches one file. The registry pattern avoids a switch statement in the execution path.
7. **Undo via captured state** — Each tool captures the previous value (not a diff). This makes undo reliable even if other mutations happen between execute and undo.
8. **Badge lines in message content** — Tool metadata is stored as `[tool:name] detail` prefix lines in the DB message content. This is a simple, persistent format that survives reload without a separate DB column or join table.
9. **`marked` for markdown→HTML** — Replaced a hand-rolled line parser with `marked` for correct handling of all markdown edge cases (nested lists, inline formatting, etc.).
10. **Bold instead of headings for notes** — AI prompts explicitly forbid `#` headings because Tiptap's StarterKit renders them oversized in the notes editor. `**Bold**` labels provide visual structure without layout problems.
11. **Segment IDs in transcript context** — The `[seg:ID timestamp]` format lets the AI reference specific segments. Citations are rendered as interactive chips, not raw text. The AI is instructed to cite selectively.
12. **Modular AI context via factory functions** — Session and multi-session contexts use the same `FloatingChatBar` component but different sources/tools/prompts, assembled via factory functions in `ai-context.ts`. Adding a new context type (e.g., cross-folder analysis) only requires new factory functions.

## Keyboard Shortcuts

### Two-Tier Architecture

Shortcuts are split into two tiers with different mechanisms:

**In-app shortcuts** (`useKeyboardShortcuts`, mounted in `AppLayout`):
- Capture-phase `keydown` listener on `document`
- `mod+key` format (where `mod` = Cmd on macOS, Ctrl on Windows)
- Suppressed when input/textarea/contenteditable is focused
- 11 actions across Recording, Navigation, and Editor categories

**Global shortcuts** (`useGlobalShortcuts`, mounted in `App.tsx`):
- `@tauri-apps/plugin-global-shortcut` — work even when app is unfocused
- `CmdOrCtrl+Key` format (Tauri's platform-independent modifier)
- 4 static shortcuts (new session, new session with backfill, stop recording, new note) + dynamic dictation shortcuts from `dictation.slots`
- `focusWindow()` brings app to front before executing actions

### Registry (`lib/shortcuts.ts`)

Static `SHORTCUTS` array defines 17 shortcuts across 5 categories (`ShortcutCategory`): Recording, Navigation, Editor, General, Dictation. Each `ShortcutDefinition` has `id`, `label`, `description`, `category`, `defaultBinding`, optional `isGlobal` and `isDictation` flags.

`SHORTCUT_MAP` provides fast lookup by ID. `getBinding(id, overrides)` returns the user override or default binding.

### Override System

`shortcutBindings: Record<string, string>` in persisted settings maps shortcut IDs to custom bindings. `ShortcutsTab` provides a capture-mode UI for rebinding. A shared `shortcutCaptureActive` ref prevents shortcuts from firing while the user is rebinding.

### Binding Conversion

- `eventToBinding(e: KeyboardEvent)` → `"mod+shift+k"` format (in-app)
- `eventToGlobalBinding(e: KeyboardEvent)` → `"CommandOrControl+Shift+N"` format (Tauri global)
- `formatShortcutDisplay(binding)` → platform-specific symbols (⌘⇧N vs Ctrl+Shift+N)

### Custom Events

Decoupled side effects via custom DOM events:
- `yapstack:toggle-chat` — Toggles the floating chat bar
- `yapstack:toggle-search` — Toggles the command palette
- `yapstack:confirm-delete-session` — Triggers delete confirmation dialog
- `yapstack:dictation-start` — Starts dictation recording (with `{ slotId }` detail)
- `yapstack:dictation-stop` — Stops dictation recording
- `yapstack:dictation-cancel` — Aborts the active Dictation regardless of phase (dispatched by the Escape Global hotkey while a Dictation is non-idle; see "Cancellation" below)
- `yapstack:dictation-idle` — Signals dictation has returned to idle (used for toggle mode cleanup)

## Dictation

### Overview

Voice dictation via global shortcuts. Supports two activation modes: hold-to-talk (press and hold to record, release to stop) and toggle (press once to start, press again to stop). Dictation operates independently of the main recording feature but shares the `TranscriptionClient` (V1 limitation: unavailable during active recording).

### Activation Modes

`DictationActivationMode: "hold" | "toggle"` — configurable in settings.

- **Hold mode** (default): Global shortcut Pressed → start recording, Released → stop and transcribe. Natural for short dictation.
- **Toggle mode**: First press → start recording, second press → stop and transcribe. Toggle state tracked via module-level `toggleActiveSlots: Set<string>` in `useGlobalShortcuts`. On stop, dispatches `yapstack:dictation-idle` for cleanup after processing completes.

### No-Input Detection

When recording exceeds 3 seconds with no speech detected (transcription returns empty), the dictation bubble shows a `"no-input"` state (yellow ring, pulsing animation) before returning to idle.

### Dictation History

Dictation entries are persisted to SQLite via the `dictation_history` table (DB migration v10). Each entry stores: slot metadata (name, AI config, output action), input/output text, optional WAV file path and duration, and optional session ID correlation.

- **DB CRUD**: `insertDictationHistory`, `listDictationHistory`, `getDictationHistory`, `deleteDictationHistory`, `clearDictationHistory`, `updateDictationHistorySessionId` in `lib/db.ts`
- **Store**: `dictationHistory` state with `loadDictationHistory`, `deleteDictationHistoryEntry`, `clearDictationHistoryEntries` actions
- **WAV correlation**: On the `session-part-ready` event, matches by UUID to update the history entry's `wav_file_path`/`wav_duration_seconds` (and `session_id` when the dictation slot's output action is `new-note`).
- **UI**: `DictationHistoryList` (grouped by day, clear all button) + `DictationFeedEntry` (full-width entries in the history list — badges for slot/AI/action, audio playback, context menu with delete/move-to-note). The compact `DictationTrayItem` variant appears in the tray/popover surface; both share state via the `useDictationEntry` hook.
- **Navigation**: "Dictation" button in `AppSidebar` sets `ListFilter { type: "dictation" }`. `NoteCardList` routes to `DictationHistoryList` when filter is dictation.

### Dynamic Slots

Dictation uses configurable slots (not a fixed count). Each `DictationSlot` has:
- `id` (UUID), `name`, `enabled` flag
- `aiEnabled` + `prompt` — optional AI post-processing via OpenAI-compatible API
- `outputAction: "paste" | "clipboard" | "new-note"` — determines where transcribed text goes

Slots are stored in `dictation.slots` in persisted settings. Keybinds are stored in the existing `shortcutBindings` map (keys: `global.dictation-{slotId}`). `useGlobalShortcuts` subscribes to both `shortcutBindings` and `dictation.slots` for re-registration.

### State Machine

`useDictation` hook (mounted in `App.tsx`, main window only) manages the lifecycle:

```
idle → recording → transcribing → processing → done → idle
  │      │              │               │
  │      └──────────────┴───────────────┴────► cancelling → idle  (Escape)
  ↑                                                ↓
(key press)                                  (450ms display)
```

1. **recording**: Key pressed → validate (dictation enabled, engine ready, not in active recording) → start a live transcription against a synthetic per-dictation session id (reuses the live VAD / chunk / streaming-WAV pipeline) → show bubble, start timing
2. **transcribing**: Key released → `stopLiveTranscription()` finalizes the part → wait up to 1.5 s for the loop's `session-part-ready` event → collect the live segments accumulated during recording into the dictation transcript
3. **processing**: If `slot.aiEnabled && slot.prompt` → call AI API with transcription + system prompt
4. **done**: Route output based on `slot.outputAction`:
   - `"paste"`: `clipboard_paste(text, true)` — copies to clipboard + auto-pastes via osascript
   - `"clipboard"`: `clipboard_paste(text, false)` — copies to clipboard only
   - `"new-note"`: Creates a manual session, saves transcription as notes, opens in main window
5. **cancelling**: User pressed Escape → see "Cancellation" below. Suppresses the Output action and the Dictation history write before returning to `idle`.

### Cancellation

Pressing **Escape** while a Dictation is in any non-idle phase (`recording`, `transcribing`, `processing`, or post-failure `done`) fully aborts the Dictation. Implemented entirely in the frontend — no backend changes.

- **Hotkey scope**: `useDictation` registers Escape as a Global hotkey via `@tauri-apps/plugin-global-shortcut` only while a Dictation is non-idle, and unregisters it in `setIdle`. While idle, Escape behaves as the OS / focused app would normally handle it. This is what makes cancel work while the user is focused in another app (the realistic dictation case for `paste` and `clipboard` Output actions).
- **Cancel reducer** (`handleCancel`): takes ownership by setting `phase = "cancelling"` synchronously, then runs a single linear teardown: abort the AI `AbortController`, resolve the stop-deferred so any waiting `handleStop` unblocks, emit the `cancelled` Bubble state, call `commands.stopLiveTranscription()`, wait up to 1.5 s for the part to finalize (`session-part-ready`), delete it via `commands.deleteAudioFiles`, hold the Bubble for 450 ms, hide, idle.
- **Cooperative bail-points**: every `await` in `handleStop` and the ghost-transcription guard in `handleStart` are followed by `if (phase() === "cancelling") return;`, so a cancel that arrives mid-stop bails cleanly without double-running teardown.
- **What is suppressed**: the Output action (`paste` / `clipboard` / `new-note`) does not fire, no `dictation_history` row is written, the finalized session part is deleted, and toggle-mode state is cleared via the existing `dictation-idle` event.
- **What still happens**: the Sidecar's currently-in-flight Chunk transcribe is allowed to finish; its result is discarded by the cancel reducer (no per-request abort IPC). Capture itself is app-wide and stays running, so the next Dictation can start immediately.
- **Scope**: a regular Session and a Dictation cannot run concurrently (`LiveTranscriptionState` is single-occupancy on the Rust side), so cancel cannot affect a Session's stream. Pressing Escape during a Session does nothing dictation-related — the hotkey isn't registered.

### Dictation Bubble Window

Separate Tauri window (`?window=dictation`) configured as:
- 220×64px, transparent, no decorations, no shadow, always-on-top, skip taskbar, no focus steal
- Positioned at bottom-center of screen during dictation
- `DictationBubble` component shows state-dependent visuals: red ring (recording), blue (transcribing), purple (processing), green (done), yellow pulsing (no-input), grey (cancelled)
- `showBubble()` uses `commands.showOverlayPanel("dictation")`, `hideBubble()` uses `commands.hideOverlayPanel("dictation")` — platform-agnostic (macOS: NSPanel, others: WebviewWindow)
- `App.tsx` routes to `DictationBubble` when `?window=dictation` is detected, avoiding hook violations

### Backend Command

`clipboard_paste(text, auto_paste)` in `commands/dictation.rs`:
- Writes text to clipboard via `pbcopy` (macOS) / `clip` (Windows)
- If `auto_paste`: waits 50ms, then simulates Cmd+V via `osascript` (macOS)

## Analytics

Privacy-first usage analytics via [Aptabase](https://aptabase.com) — no user IDs, no fingerprinting. The Aptabase Tauri plugin automatically attaches app version (from `tauri.conf.json`), OS, and locale to every event.

### Setup

- **Rust**: `tauri-plugin-aptabase` registered in `lib.rs` with `env!("APTABASE_KEY")` (compile-time). On `RunEvent::Exit`, flushes pending events via `flush_events_blocking()`.
- **Frontend**: `@aptabase/tauri` JS SDK. All calls go through `src/lib/analytics.ts` — typed wrappers around `trackEvent()`, fire-and-forget (no awaiting).
- **ACL**: `aptabase:allow-track-event` in `capabilities/default.json`.
- **Build**: `APTABASE_KEY` sourced from `.env` (local) or GitHub Secrets (CI). Optional for dev — analytics silently disabled when unset.

### Event Taxonomy (~35 events)

| Category | Events | Key Props |
|----------|--------|-----------|
| App lifecycle | `app_launched`, `app_exited` | capture_source, model_size, theme, ai_provider, dictation_slot_count |
| Sessions | `session_created`, `session_stopped`, `session_deleted`, `sessions_cleared`, `manual_note_created` | source, trigger (sidebar/tray/shortcut), duration_seconds, segment_count |
| Dictation | `dictation_started`, `dictation_completed`, `dictation_failed`, `dictation_cancelled`, `dictation_slot_created/deleted/configured`, `dictation_history_cleared`, `dictation_history_entry_deleted`, `dictation_moved_to_note` | slot_id, duration_ms, ai_processed, output_action, error_reason, phase (cancelled-from) |
| AI chat | `chat_message_sent`, `chat_tool_executed`, `chat_tool_undone`, `chat_cleared` | context, action_id, tool_name |
| Navigation | `search_used`, `folder_created`, `session_pinned/unpinned`, `session_moved_to_folder` | — |
| Shortcuts | `shortcut_used` | shortcut_id |
| Model/Engine | `model_downloaded/deleted/switched`, `engine_error` | model_size, from_size, error, phase |
| Settings | `setting_changed`, `ai_provider_changed`, `ai_connection_tested` | setting_name, new_value, provider, success |
| Content | `audio_playback_started`, `segment_edited`, `segment_hidden` | duration_seconds |
| Stream health | `stream_health_event` | source, status |

### Design Principles

- **No content tracking**: Never log transcript text, note content, or AI messages. Only structural metadata (counts, durations, IDs).
- **Booleans as 0/1**: Aptabase props only support strings and numbers — no booleans.
- **Truncation**: Error messages capped at 100 chars to avoid PII leakage.
- **Fire-and-forget**: Analytics never blocks UI or throws. All `trackEvent` calls are caught silently.
- **Single file**: Adding a new event = add a typed export to `analytics.ts` + one call at the integration point. No other files need to change.

## Ring Buffer Design

The `AudioRingBuffer` is the performance-critical component:
- **Single-producer, single-consumer** (SPSC) pattern
- **Producer** (audio callback): writes via `UnsafeCell`, stores `write_pos` with `Release`
- **Consumer** (app thread): loads `write_pos` with `Acquire`, reads from buffer
- **Monotonic counter**: `write_pos` never resets (wraps via modulo), enabling `snapshot_since()`
- **Bounded reads**: `snapshot_range(from, until)` reads only the requested half-open cursor range, caps to committed samples, and reports `overrun` when the ring buffer has already overwritten the requested start. Hard-stop finalization and bounded live draining use this instead of implicitly reading to the current write head.
- **Per-buffer format**: Each ring buffer is created with its device's native sample rate and channel count. `start_mic()` does not mutate `AudioManager.config` — the buffer carries its own format. All extraction methods read each buffer's `sample_rate()` / `channels()` and deinterleave multi-channel data to mono before mixing or WAV export.
- **Default config**: `capture_history_seconds` (Rust default 180, frontend default 300 via `bufferMaxSeconds`). Devices typically operate at 48kHz (mono for mic, stereo for system audio on macOS). Ring buffer size is calculated from the actual device rate. `DEFAULT_SAMPLE_RATE` (16kHz) in `yapstack_common::config` is a fallback when no buffer is active.

## IPC Protocol (Sidecar)

JSON-line protocol over stdin/stdout. Each message is a single JSON object followed by `\n`.

**Requests** (tagged union via `#[serde(tag = "type")]`):
- `{"type":"transcribe", "id": 1, "audio_path": "/path/to/file.wav", "language": "en", "initial_prompt": "prior context..."}`
- `{"type":"load_model", "id": 2, "model_path": "/path/to/ggml-small.bin"}`
- `{"type":"shutdown"}`

**Responses**:
- `{"type":"transcription", "id": 1, "text": "...", "segments": [...], "duration_ms": 500}`
- `{"type":"model_loaded", "id": 2}`
- `{"type":"error", "id": 1, "message": "..."}`
- `{"type":"progress", "id": 1, "percent": 0.5}`

## Platform Support

| Feature | macOS | Windows (x64 MSVC) | Linux |
|---------|-------|---------------------|-------|
| Microphone capture | cpal (CoreAudio) | cpal (WASAPI) | cpal (ALSA) |
| System audio capture | cpal loopback (always available) | cpal loopback (WASAPI) | Unavailable |
| Whisper transcription | whisper.cpp (Metal) | whisper.cpp | whisper.cpp |
| Entitlements | audio-input, screen-capture | N/A | N/A |

### Windows WASAPI notes

- **System audio loopback** uses WASAPI event-driven loopback capture. When no application is producing audio, the loopback stream delivers zero packets — this is expected WASAPI behavior, not a stream error.
- The stream health watchdog skips write-position stall detection for system audio on Windows to avoid false restart storms during silence. System streams rely solely on the cpal error callback flag for failure detection.
- Microphone privacy settings on Windows 10/11 can block desktop app access. Capture-start failures surface as `CaptureState::Error` with an error message visible in the title bar and via toast notification.

## Audio Playback

Session audio is stored under each session's tracked audio directory as one file per part: `{session_id}.{part_index}.wav` (or `.mp3` if the session was finalized in MP3 format). The default audio dir is `$APP_DATA_DIR/audio/`; users can override it via the `audioSaveLocation` setting, in which case each session's parts live there. Playback uses a custom URI scheme protocol:

```
audio-stream://localhost/{filename}
```

### Why a custom protocol?
The `convertFileSrc()` asset protocol URL (`asset://localhost/...`) works for `<audio>` elements in production builds but fails cross-origin in dev mode (`localhost:5173` vs `asset.localhost`). The custom `audio-stream` protocol is registered in `lib.rs` and serves audio files from any directory listed in `TrustedAudioDirs`. The trust list is seeded on startup from `session_audio_parts.file_path` parents and the `audio_save_locations` table, then extended at recording finalize time.

### Range request support
The protocol supports HTTP `Range` headers for audio seeking. Returns `206 Partial Content` for range requests with `Content-Range` header, or `200` with full file for non-range requests. Content type is set per extension (`audio/wav` for `.wav`, `audio/mpeg` for `.mp3`). Security: only files inside a trusted dir are served, and path traversal is rejected.

### Multi-part playback and seeking
`session_audio_parts` is the durable source of truth for which files belong to a session. The frontend reads parts in `part_index` order, and playback / timestamp seeking treats them as a single continuous timeline by routing through a parts-aware `seekTo` (segments carry their per-session offset, not per-file). On a session resume, the new part's `audio_offset_seconds` base is set to the cumulative duration of prior parts.

### Playback sync
`AudioPlayer` provides time updates via `requestAnimationFrame`. `ChatView` highlights the active segment by matching `currentPlaybackTime` against `audio_offset_seconds`. Clicking a segment timestamp seeks the audio via the parts-aware `handleSeek`.

## Key Design Decisions

1. **Sidecar process for Whisper** — Isolates heavy native dependency (whisper.cpp/cmake) from the main app binary. Crash isolation. Can be updated independently.
2. **Lock-free ring buffer** — Audio callbacks cannot block. SPSC with atomics avoids mutex contention.
3. **DTO layer in Tauri commands** — Domain types don't derive `specta::Type`. DTOs add the TypeScript generation derive and convert via `From` impls.
4. **Feature flags for optional native deps** — `whisper` and `metal` on yapstack-transcription-sidecar keep the default build lightweight. System audio is always compiled in via cpal loopback (no feature flag).
5. **16-bit PCM WAV export** — WAV files are written at the configured capture source's output rate (mic for `MicOnly`, system for `SystemOnly`, mic-rate mixed output for `Mixed` when mic is present). The sidecar resamples to 16kHz mono before Whisper inference. 16-bit quantization is sufficient (-96 dB noise floor).
6. **Session tracking via write_pos** — Sessions record the ring buffer's monotonic write position at start and stop. `SessionWavWriter` streams audio incrementally to disk every poll, and stop-time tails are flushed with `snapshot_range()` so post-stop samples never enter the finalized part.
7. **Custom URI scheme for audio** — `audio-stream://` protocol serves session audio (WAV or MP3) from any allow-listed directory with range request support, avoiding cross-origin issues with the default asset protocol during development.
8. **`session_audio_parts` is the durable source of truth for session audio** — Each session has zero or more part rows (`part_index = 0, 1, 2…`); when `LiveTranscriptionConfig.persist_audio_part` is true (default for actual sessions) the row is inserted from Rust at finalize time *before* `session-part-ready` is emitted, so a missed FE event can't lose the file. Dictation flips the flag off — its synthetic per-utterance id has no `sessions.id` row, so the path goes onto `dictation_history` instead and the parts table only ever holds rows that round-trip to a real session. Resuming a session appends a new part rather than overwriting; the FE concatenates parts at playback time. `audio_save_locations` tracks every directory the app has ever written into so reconciliation on next startup can recover orphans even if the row insert was missed.
9. **`useDictation` registers Escape as a global hotkey only while non-idle** — Escape cancels an in-flight Dictation, suppresses the Output action, deletes the streamed audio, and skips the `dictation_history` write. The hotkey is unregistered when idle so it doesn't override the focused app's normal Escape handling.
