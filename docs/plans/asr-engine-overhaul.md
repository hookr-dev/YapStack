# Plan: ASR engine overhaul — benchmark, fix the quant, drop Whisper, adopt Parakeet Unified EN

**Status:** in progress (updated 2026-11-03). Proposed 2026-10-22; gates G1, G3 and G4 ratified 2026-10-31 (G4 at 100% placement); AE-G1 added 2026-11-03. Phase 0 harness is `tools/asr-bench`. Phase 1's truthful EP shipped (strict WebGPU, measured placement table, int8 on CPU on macOS). Phase 2 shipped (ADR 0005, #131, #132). The AE results below replace the macOS default question: FluidAudio (CoreML/ANE) is the planned Mac default. Phases 3–4 not started.
**Research:** `docs/plans/research/asr-engine-2026-10/README.md` (gitignored).

## Context

YapStack has two engines today: Whisper (whisper-rs, Metal) and Parakeet TDT v3 (parakeet-rs 0.3.5, ort rc.12). Fresh installs already default to Parakeet. Whisper is labelled a "deprecation candidate" in ADR 0004 and in windows-support.md:27. Three findings drive this plan.

1. **The shipped quant probably can't use the GPU.**
   - Apple Silicon and Windows run istupakov `encoder-model.int8.onnx`. That file is ORT *dynamic* quantization: DynamicQuantizeLinear ×223, MatMulInteger ×217, ConvInteger ×77.
   - The ORT WebGPU EP registers none of those ops, and neither does the CoreML EP. The quantized matmuls and convs therefore very likely fall back to CPU, with copies in between.
   - This fits the logged RTFx: WebGPU ≈6× vs CPU ≈5–7× (IMPLEMENTATION_LOG:970).
   - macOS Auto registers WebGPU *soft*, so the UI can report "webgpu" while the work runs on CPU (parakeet.rs:497-508).
   - None of this is profiled yet.
2. **Dynamic int8 may cost accuracy on quiet input.** One community card (FLEURS-ru, n=40) reports WER of int8 7.3% vs weight-only int4 5.5% vs fp32 5.4%. Our own int8 WER sanity check was never run; it is a pre-GA item at windows-support.md:221.
3. **A better same-size model now exists.**
   - nvidia/parakeet-unified-en-0.6b (2026-04) scores a 5.91 mean WER offline vs 6.34 for v3, and 6.29 when streaming at 1.12 s latency. It outputs punctuation and capitalisation.
   - It is English-only and uses the NVIDIA Open Model License.
   - parakeet-rs 0.4.x loads it. 0.4.x also brings Nemotron-3 Diarization (Sortformer v3: streaming, up to 8 speakers), Nemotron streaming, and an experimental pure-Rust Burn backend.

Removing Whisper costs:
- 74 languages: 99 → Parakeet's 25 European languages.
- Decoder-level vocabulary biasing: folder/tag hints and rolling prompt context (live_transcription.rs:5006).
- Pre-v22 upgraders are still on Whisper and need a migration.

Nothing else is lost:
- Intel Macs aren't shipped.
- Windows Parakeet already runs on WebGPU.
- Word timestamps are Parakeet-only.
- No fallback path depends on Whisper.

## Goals

1. Measure, on the owner's hardware and audio, which Parakeet variant and EP gives the best accuracy and speed. Then make it the default, and make the EP label truthful.
2. Remove Whisper completely: code, deps, CI cmake, model catalog, UI and docs.
3. Upgrade to parakeet-rs 0.4.x and adopt Parakeet Unified EN as the English model, if it passes its gate.

## Non-goals (deferred follow-ons, each its own plan)

- **Qwen3-ASR 1.7B multilingual pack** for non-European languages. Needs a runtime decision (transcribe.cpp); Phase 0 collects informational numbers only.
- **Unified or Nemotron *streaming* mode** for live captions and dictation, replacing VAD chunking. This is an architectural change to the live loop.
- **Granite Speech 4.1** post-meeting high-accuracy re-transcription pass.
- **CoreML/ANE-native runtime:** a FluidAudio Swift sidecar, or CoreML EP with an fp16 MLProgram.
- **Diarization rework on Sortformer v3**, which belongs to the product-direction plan. Phase 3 only makes it available.
- **Real Parakeet confidence:** it is hard-coded 1.0 today (parakeet.rs:202).
- **Decoder-level vocabulary biasing** for Parakeet.
- **Performance cleanups:** raw-PCM IPC instead of a temp WAV per chunk, deduplicating the two ORT copies, TEN VAD, frontend code-splitting.

## Phase 0 — Benchmark harness (no product change)

**Where:** `tools/asr-bench/`, a standalone Cargo project with its own `[workspace]` and lockfile, excluded from the root workspace.
- **Why standalone:** it must link parakeet-rs 0.4.x / ort rc.15 alongside the app's 0.3.5 / rc.12. `ort-sys` declares `links`, so both versions cannot share one lockfile.
- **Current-app baselines** (Whisper, Parakeet on 0.3.5) come from driving the *built* sidecar binary over its existing JSON-line IPC. The baseline is then exactly what ships.
- **New variants** run in-process via parakeet-rs 0.4.x.
- **Optional rows:** transcribe.cpp runs through its CLI.

### Matrix

| ID | Model / variant | Runtime / EP | Purpose |
|---|---|---|---|
| A1–A3 | TDT v3 int8 (istupakov, current) | sidecar 0.3.5: CPU / WebGPU (as shipped, soft) / WebGPU strict | Baseline. Proves or disproves the CPU fallback. |
| A4 | TDT v3 int8 | CoreML EP | Completeness |
| B | TDT v3 fp16, single file (converted from istupakov fp32, `keep_io_types`) | WebGPU, CoreML | Main GPU candidate |
| C | TDT v3 int4 weight-only (`MatMulNBits`, efederici) | CPU, WebGPU | Size and quiet-input candidate |
| D | TDT v3 fp32 | CPU | Accuracy ceiling / reference |
| E | Parakeet Unified EN 0.6B, offline mode (fp32 export; plus fp16/int4 single-file conversions) | CPU, WebGPU | Gate G2 |
| W1–W2 | Whisper Small (current default), Whisper Medium | sidecar, Metal | Gate G3 baseline |
| X1 | TDT v3 + Unified EN, GGUF Q8_0 | transcribe.cpp, Metal | Informational: runtime alternative |
| X2 | TDT v3 fp32 | parakeet-rs Burn, Metal | Informational: ORT-free path |
| X3 | Qwen3-ASR 1.7B + 0.6B (GGUF) | transcribe.cpp, Metal | Informational: language-pack decision |

Rows X1–X3 never gate this plan.

### Corpus

All corpus data lives in a gitignored cache (`tools/asr-bench/.cache/`).

1. **Owner audio (gating).**
   - About 30 minutes from real sessions, with mic and system parts kept separate.
   - Reference transcripts are bootstrapped from row D output and then hand-corrected by the owner.
   - Include English plus any other European languages the owner actually uses.
2. **Quiet-input slice (gating).**
   - The owner clips re-levelled to −25 and −35 dBFS peak.
   - At least one genuine laptop-mic recording.
3. **Production-chunking replay (gating).** Owner sessions are fed through the real Silero VAD chunker profile (500 ms silence, 10 s max, 250 ms pre-roll; live_transcription.rs:1109-1120). This measures WER the way live transcription produces it, boundary effects included.
4. **Public subsets (sanity).**
   - LibriSpeech test-other (200 utterances), AMI-IHM and Earnings-22 subsets.
   - FLEURS: about 50 utterances each for en, de, fr, es, it, pl, plus the weak languages sv and el.
5. **Negative set.** Silence, music, keyboard noise. Measures hallucinated or non-empty output.

### Metrics

- **Accuracy:**
  - WER and CER. One shared text normaliser across all rows (Open-ASR/Whisper-style).
  - Hallucination rate on the negative set.
- **Speed:**
  - RTFx overall.
  - Per-chunk latency p50/p95 for 2–10 s chunks.
  - Cold start: model load plus first inference.
- **Resources:**
  - Peak RSS.
  - Energy (mJ per audio-minute) via `sudo powermetrics`. The owner runs this step.
- **EP placement:** ORT verbose session logs give the node count per EP and the number of partitions, per row.
- **Hardware:** the owner's Apple Silicon Mac (record the chip).
  - Windows: a CI job on a CPU runner covers correctness and CPU RTFx only.
  - Windows WebGPU numbers come from canary UAT. This follows the CI-only Windows workflow.

**Output:** CSV plus a markdown report in `docs/plans/research/asr-bench-<date>/` (gitignored). The headline table goes into ADR 0005.

### Gates (ratified before the run; not relaxed after)

| Gate | Decides | Pass condition |
|---|---|---|
| **G1** Default variant + EP (ratified 2026-10-31) | Phase 1 | Among rows A–C, take the fastest row (p95 chunk latency, then energy) whose WER is ≤ D (fp32) + 0.3 pt absolute on *every* gating slice (owner, quiet, chunk-replay). |
| **G4** EP honesty (ratified 2026-10-31 at 100%) | Phase 1 | An accelerator EP is default only if 100% of heavy (MatMul/Conv) ops are placed on it. Any heavy op off the accelerator forces CPU as the default for that platform. |
| **G3** Whisper drop (ratified 2026-10-31; passed, ADR 0005) | Phase 2 | The G1 winner's WER ≤ W2 (Whisper Medium) on the owner and chunk-replay slices. The owner explicitly accepts the loss of the languages outside the 25. |
| **G2** Unified EN (not yet ratified) | Phase 4 | Unified (best variant) WER ≤ G1 winner − 0.3 pt on owner English and chunk-replay, *and* not worse on the quiet slice. Latency within 1.25× of the G1 winner. |
| **AE-G1** Mac efficiency default (added 2026-11-03) | FluidAudio as Mac default | WER ≤ shipped int8 + 1.0 pt absolute on *every* gating slice, zero empty or truncated outputs, and word timestamps kept. **Owner ruling 2026-11-04:** "zero empty or truncated" means no more than the shipped int8 on the same replayed chunks (ADR 0008). |

Phase 0 also closes the pre-GA "in-house int8 WER sanity-check" (windows-support.md:221).

## AE results (2026-11-03)

Battery energy per audio-minute, Apple Silicon:

| Engine | J / audio-min |
|---|---|
| Parakeet int8, ORT CPU (current macOS default) | 24.2 |
| Parakeet fp16, ORT WebGPU | 21.4 |
| Parakeet f16, Burn | 14.2 |
| FluidAudio, CoreML/ANE | 6.5 (193× RTFx) |

- **Burn rejected.** It needs our own patch to maintain, and its `panic=abort` kills the sidecar on any panic.
- **ORT CoreML EP:** 0% of heavy ops placed on it. It is not an acceleration path.
- **FluidAudio is the planned Mac default** ([ADR 0008](../adr/0008-fluidaudio-coreml-mac-default.md)), gated on AE-G1 (see Gates) in an app-style re-score (live chunking, not whole files) after #138 (forced cuts at a pause) and #139 (empty-decode retry).
- **macOS 14 floor:** the minimum macOS version is raised to 14 directly, with no older-OS fallback path.

### AE-G1 replay baseline (2026-11-04)

Corpus `librispeech-replay-v1`: 105 clips per slice (10 for drift) built from LibriSpeech test-clean:400 + test-other:200. Only numbers from this corpus compare with these. Reproduce with `tools/asr-bench`: `replay-corpus`, then `replay --baseline int8:auto` ([README](../../tools/asr-bench/README.md#app-style-replay-ae-g1)). These numbers were taken with the scratch harness that became `replay`. The corpus `replay-corpus` builds is byte-identical to it, and `replay` cuts identical `live` and `rewind` chunks. Its `backfill` cut differs slightly: it calls the app's `vad_chunk_historical_audio`, which also joins a sub-2 s last piece to the one before it (#144), and the scratch harness skipped that step. 1–12 clips per slice end differently (long 937 → 932 chunks, long44 933 → 928, mono 890 → 881, drift 932 → 932), so a fresh backfill run won't match these rows exactly; re-baseline both engines in one run. int8 ran on ORT CPU, FluidAudio (`Encoder_v2`) on CoreML with Auto. Both scored the same replayed chunks; zero sidecar errors.

| Slice | Mode | Chunks | int8 WER % | FA WER % | Δ pt | int8 empty / truncated | FA empty / truncated |
|---|---|---|---|---|---|---|---|
| long | live | 555 | 4.11 | 3.74 | −0.37 | 4 / 6 | 2 / 1 |
| long44 | live | 554 | 4.46 | 3.97 | −0.49 | 7 / 7 | 3 / 2 |
| mono | live | 486 | 5.97 | 5.77 | −0.20 | 7 / 17 | 0 / 10 |
| drift | live | 543 | 4.35 | 3.98 | −0.38 | 8 / 7 | 1 / 4 |
| long | backfill | 937 | 5.94 | 6.42 | +0.48 | 8 / 13 | 2 / 5 |
| long44 | backfill | 933 | 6.40 | 6.70 | +0.30 | 16 / 20 | 3 / 5 |
| mono | backfill | 890 | 10.01 | 8.01 | −2.00 | 27 / 39 | 0 / 12 |
| drift | backfill | 932 | 6.20 | 6.50 | +0.30 | 13 / 8 | 1 / 4 |

FA is within +0.5 pt on every slice, and 2.0 pt better on monologue backfill. It has fewer empty chunks and truncated clips on all 8 slices: AE-G1 passes on WER and empty/truncated. The word-timestamp check from that run wasn't kept; `replay` now reports it per slice (timestamp-bad column).

Backfill edge fixes (#144: no split piece under 2 s, the rewind's open tail handed to live), int8 on ORT CPU, same corpus:

| Slice | Mode | Before (WER % / empty / truncated) | After |
|---|---|---|---|
| long | backfill | 6.57 / 60 / 17 | 5.94 / 8 / 13 |
| long44 | backfill | 7.03 / 63 / 23 | 6.40 / 16 / 20 |
| mono | backfill | 10.42 / 63 / 38 | 10.01 / 27 / 39 |
| drift | backfill | 6.89 / 68 / 9 | 6.20 / 13 / 8 |
| long | rewind | 7.94 / 53 / 17 | 5.46 / 9 / 9 |
| mono | rewind | 11.46 / 54 / 35 | 7.73 / 10 / 17 |

"Before" is the chunking before #144, which no longer exists in the code; only the "After" column reproduces from `main` (`--mode backfill,rewind`).

## Phase 1 — Fix quant and EP on the current stack (parakeet-rs 0.3.5)

Driven by G1/G4. No dependency upgrade.

1. **Truthful EP.**
   - Make the macOS Auto WebGPU registration strict, or report the placement actually observed.
   - Fall back to CPU explicitly and log it, as Windows already does.
2. **Default variant switch.**
   - Add the G1 winner as a `ParakeetVariant` (e.g. `TdtV3Fp16` or `TdtV3Int4`) in `crates/yapstack-transcription/src/model.rs`, plus its DTO (`commands/transcription.rs`) and labels.
   - **Hosting:** converted files must be downloadable. Pin third-party repos by `resolve/<commit>` plus sha256, or host under a project HF org (*ask first: public publishing*).
3. **Migration.**
   - autoSetup downloads the new variant with visible progress.
   - The old variant stays until the new one loads successfully, and is deleted only afterwards.
   - This uses the existing settings migration path in `appStore.ts`; no DB schema change.
4. **Warmup:** one silent inference after model load, so the first live chunk doesn't pay cold-start cost.
5. **Housekeeping:** remove the stale 35 MB `binaries/yapstack-embedding-sidecar` (it isn't in externalBin).
6. **CHANGELOG** (Changed / Fixed) and tests for variant selection and the EP fallback label.

## Phase 2 — Drop Whisper (ADR 0005)

**Prerequisites:** G3 passed, and ADR `docs/adr/0005-parakeet-only.md` written and approved. CONTRIBUTING.md:131 and AGENTS.md:171 require an ADR for engine changes and platform-branch removal.

1. **Rust / build.**
   - Remove `whisper` and `metal` features and the `whisper-rs` dep from `crates/yapstack-transcription-sidecar/Cargo.toml`, and the dead `whisper` feature in `crates/yapstack-transcription/Cargo.toml`.
   - Delete `engines/whisper.rs`. Update `engines/mod.rs` (cfg gates, `initial_prompt`/`single_segment`, about 25 `include_w` tests) and the `main.rs` default engine and `--vad-model`.
   - Remove `ModelSize`, the Whisper model URLs and the ggml Silero VAD from `model.rs`.
   - Remove the Whisper hallucination lists (`hallucination.rs:27-70`), keeping the generic filters.
2. **IPC** (*ask first*). Remove `EngineKind::Whisper` (`yapstack-common/src/types.rs`) and the Whisper catalogue entry (`engines.rs`).
   - Host and sidecar ship in one bundle.
   - The sidecar returns a clear error for an unknown engine rather than panicking.
3. **Tauri commands** (*ask first*).
   - Remove the Whisper model commands (`get_available_models`, `download_model`, `delete_model`), `ModelSizeDto`, `EngineKindDto::Whisper` and the `whisper_model` config field.
   - This regenerates `lib/types.ts`.
4. **Frontend.**
   - Delete `ModelSection.tsx`, the engine picker and the Whisper-only sliders (`TranscriptionTab.tsx:205-225, 322-347`).
   - Remove the store Whisper paths (`appStore.ts`: autoSetup, download/switch, `switchEngine`).
   - Remove `buildVocabularyHints` decoder use. Hints keep flowing to the AI cleanup and summary prompts.
   - Fix the "Correct obvious Whisper misrenderings" prompt text (`appStore.ts:188`).
5. **Settings migration.** Force `engine = Parakeet` for every remaining Whisper user, and trigger the Parakeet download with progress.
   - Old model files are deleted automatically (owner ruling 2026-11-03).
6. **Language UX.**
   - Replace the no-op language picker (Parakeet ignores it; parakeet.rs:157) with an honest "Supported languages" list.
   - Show a notice that languages outside the 25 aren't supported yet.
7. **CI.**
   - Drop `brew install cmake`, the Windows cmake checks, and the `-march=native` i8mm CFLAGS (`ci-checks.yml:30-46, 99-103`; `release.yml:181-185`; `sync-test-build.yml`).
   - Simplify the build-sidecar feature lists (`scripts/build-transcription-sidecar.sh:73-91`).
8. **Docs.** Update AGENTS.md (build commands), README (*ask first: public copy*), DEVELOPMENT, ARCHITECTURE, API_REFERENCE, UBIQUITOUS_LANGUAGE, CONTRIBUTING and the bug-report template.
   - Add a CHANGELOG `Removed` entry.

## Phase 3 — Upgrade parakeet-rs 0.4.x + ort rc.15 (*ask first: beyond-patch dependency upgrade*)

1. **Bump both crates together.** The sidecar's direct `ort` dep, used for strict WebGPU registration, must stay pinned to what parakeet-rs pins. It must use `parakeet_rs::ort` and the single shared ORT environment, because ORT allows one per process.
2. **Re-run Phase 0 rows** for the G1 winner on 0.4.x. Results must be at parity (WER ±0.1 pt, latency within 10%).
3. **Verify packaging.** Check the Dawn/WebGPU dylib bundling (`libwebgpu_dawn.dylib`, `tauri.conf.json:125`) and Windows canary, since the ort RC bump can change native artifacts.
4. **Sortformer v3** becomes available: streaming, 8 speakers. Adopting it is the diarization follow-on, not this phase.

## Phase 4 — Parakeet Unified EN (gated by G2)

1. **Model variant.** Add `ParakeetVariant::UnifiedEn`, using the bench-chosen precision.
   - The available ONNX export (bobNight) is fp32 with an external `.onnx.data`, which blocks GPU EPs (model.rs:91-95).
   - So it ships as a single-file fp16/int4 conversion, or runs on CPU, per the bench.
2. **Language-aware selection.** Setting "Primary language: English" uses Unified EN; other European languages use TDT v3.
   - A bilingual user can keep both, at about 2× the disk.
   - This gives the language setting real meaning.
3. **Offline mode only.** It is a drop-in replacement in the existing VAD-chunk loop. Streaming is the deferred follow-on.
4. **Licence.** The NVIDIA Open Model License needs a legal read before shipping. Add the NOTICE/attribution files.
5. **CHANGELOG** `Added`.

## PR sequence

Each PR passes `pnpm check`.

1. `tools/asr-bench` harness (Phase 0). Then the bench run and the owner's gate review.
2. Truthful EP, variant switch, warmup, housekeeping (Phase 1).
3. ADR 0005 and Whisper removal (Phase 2).
4. parakeet-rs / ort upgrade (Phase 3).
5. Unified EN (Phase 4).

## Ask-first checklist (per AGENTS.md)

- [x] Ratify gates G1, G3, G4 before the bench runs (2026-10-31; G2 still open)
- [ ] ADR 0005 (Parakeet-only)
- [ ] Sidecar IPC change (`EngineKind::Whisper` removal)
- [ ] Tauri command surface removal (Whisper model commands)
- [ ] Dependency upgrades: parakeet-rs 0.3.5→0.4.x, ort rc.12→rc.15
- [ ] Hosting converted model files publicly (if not using pinned third-party repos)
- [ ] Public copy: README, release notes, in-app language notice
- [ ] Legal read: NVIDIA Open Model License (Unified EN)

## Owner inputs

- About 30 minutes of representative session audio, plus correction of the bootstrapped reference transcripts.
- One genuine quiet laptop-mic recording.
- Run the `powermetrics` energy step (needs sudo).
- Chip model of the benchmark Mac.

## Risks

- **Community conversions:** efederici int4, the bobNight Unified export, and our own fp16. Exports can carry subtle errors.
  - Mitigation: the bench validates each against the fp32 reference, and downloads are pinned by commit and sha256.
- **Fixing the EP label may expose a slower real GPU path.** If so, G4 defaults to CPU, which is the honest outcome.
- **The Windows WebGPU path can't be benchmarked locally.** It is covered by canary UAT.
- **Losing decoder vocabulary biasing** is a regression for users with folder/tag vocabularies. Mitigation: hints continue in the AI cleanup; decoder biasing is a follow-on.
