# 0005. Parakeet-only transcription engine

- **Status**: Accepted
- **Date**: 2026-11-02
- **Supersedes in part**: ADR 0004 (the "Whisper stays CPU on Windows" clause).

## Context

YapStack ships two speech engines behind one sidecar: Whisper (whisper-rs on
whisper.cpp/GGML) and Parakeet TDT v3 (parakeet-rs on ONNX Runtime). Fresh
installs have defaulted to Parakeet since settings v22, and ADR 0004 already
called Whisper "an older engine and a deprecation candidate".

Keeping Whisper costs more than it returns:

1. **Build and CI weight.** whisper-rs-sys is the only crate in the workspace
   that needs `cmake`. CI installs and checks cmake on macOS and Windows, and
   sets `-march=native` CFLAGS for whisper.cpp's i8mm kernels. The engine adds
   roughly 1,000–1,300 lines across the sidecar, the model catalogue, the Tauri
   commands, the live loop and the settings UI.
2. **Whisper-only capabilities are already inert for the default user.**
   - Vocabulary hints (folder and tag names) and the rolling prompt context
     only feed Whisper's decoder (`initial_prompt`). Parakeet ignores them.
     They are not used by the AI cleanup or summary prompts.
   - The silence and max-chunk sliders only tune Whisper's VAD profile.
     Parakeet uses a fixed profile (500 ms silence, 10 s max chunk, 250 ms
     pre-roll).
   - The language picker is a no-op for Parakeet, which auto-detects among its
     25 European languages.
   - Real per-token confidence is Whisper-only. Parakeet reports 1.0, so the
     confidence-gated hallucination tiers already never fire for Parakeet
     users.
3. **Two engines double every engine-facing change.** Accelerator labelling,
   variant upgrades, diarization and the planned Unified EN model all have to
   reason about a second engine that most users never run.

The owner ratified removal on 2026-11-02 without a Whisper benchmark row, and
explicitly accepted the loss of the languages outside Parakeet's 25.

## Decision

- **Remove Whisper completely**: the `whisper` and `metal` sidecar features,
  the `whisper-rs` dependency, the Whisper backend, its hallucination tables,
  the Whisper model catalogue (GGML sizes and the GGML Silero VAD file), the
  Whisper model commands, the Whisper UI and the CI cmake setup.
- **Keep `EngineKind` as a one-variant enum** and keep the engine catalogue
  and the sidecar `TranscriptionBackend` trait. They are the extension point
  for later models (Parakeet Unified EN first), so removing them now would mean
  re-adding them.
- **Remove the Whisper-shaped IPC knobs in the same change.**
  `Transcribe.initial_prompt` and `Transcribe.single_segment` leave the sidecar
  protocol, and the host's prompt-context subsystem (vocabulary hints, rolling
  prompt, prompt decay) is deleted with them. Vocabulary hints are not moved to
  the AI cleanup prompt: that would change what text reaches the user's AI
  provider and needs its own design.
- **`Transcribe.diarization` stays.** Live speaker labelling depends on it. A
  later IPC change replaces it with a per-stream key.
- **The sidecar refuses an unknown `--engine`** with a non-zero exit, instead
  of silently defaulting to an engine.
- **The Parakeet default is unchanged.** The int8 TDT v3 variant stays the
  default. Lighter or GPU-friendlier variants are a separate, later change that
  adds entries to the Parakeet-only catalogue.
- **Whisper users are migrated, not stranded.** Settings migration v32 (and an
  idempotent coercion on every rehydrate) rewrites a persisted
  `selectedEngine: "Whisper"` to Parakeet and drops the Whisper-only settings.
  Startup then takes the normal Parakeet first-install path: a visible download
  with progress, and on failure an error state with Retry. There is no fallback
  engine.
- **Old Whisper model files are never deleted silently.** Transcription
  settings lists them with their size and offers a one-click removal. Deleting
  user data stays a user action.
- **Language UX is honest.** The language picker becomes a read-only
  "auto-detected" list of the 25 supported languages. A persisted
  `settings.language` outside that list is kept (it becomes the primary-language
  input for Unified EN routing later) and the user sees a notice that it isn't
  supported yet.

## Consequences

- **Languages go from 99 to 25**, all auto-detected. Users who transcribed
  other languages lose that capability until a language pack ships.
- **Decoder vocabulary biasing and the rolling prompt are gone.** Neither was
  active for Parakeet users, which is every fresh install.
- **The VAD and chunk tuning sliders are removed.** They had no effect on
  Parakeet.
- **Confidence-gated hallucination tiers stay inert** until a Parakeet build
  exposes token log-probabilities. Low-confidence styling on old Whisper-era
  segments keeps working because the stored confidence is unchanged.
- **Whisper upgraders download Parakeet once** (about 670 MB for int8 on Apple Silicon and Windows;
  about 2.5 GB for the fp32 bundle Intel Macs use). An
  upgrader who is offline at first launch sees the engine error and Retry; the
  release notes call out the one-time download.
- **IPC change (host and sidecar ship in one bundle):** `EngineKind::Whisper`,
  `Transcribe.initial_prompt` and `Transcribe.single_segment` are removed.
- **Tauri command change:** the Whisper model commands and
  `update_vocabulary_hints` are removed, the Whisper-only live-config fields
  go, and a mechanism-named pair of commands lists and deletes legacy model
  files. The same pair later cleans up superseded Parakeet variants.
- **CI no longer needs cmake** on macOS or Windows, and drops the
  `-march=native` flags.
- **No database migration.** No table stores the engine.
- **ADR 0004's "Whisper stays CPU on Windows" clause no longer applies**; the
  rest of ADR 0004 stands.

## Alternatives considered

- **Keep Whisper as an opt-in pack.** Keeps the cmake toolchain, the second
  engine arm in every engine-facing change, and settings that do nothing for
  the default user. Rejected.
- **Switch runtime to transcribe.cpp.** A larger change with no product need
  today. Not pursued.
- **A multilingual model pack for non-European languages.** Deferred to its own
  decision; the one-variant `EngineKind` and the catalogue leave room for it.

## References

- [`0004-cross-platform-rollout-windows-first.md`](0004-cross-platform-rollout-windows-first.md) — the Whisper-on-Windows clause this ADR supersedes.
- [`docs/plans/windows-support.md`](../plans/windows-support.md) — where Whisper was first labelled a deprecation candidate.
- [`docs/plans/parakeet-variants.md`](../plans/parakeet-variants.md) — the Parakeet variant catalogue that becomes the only model catalogue.
