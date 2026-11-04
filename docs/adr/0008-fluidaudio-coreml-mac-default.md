# 0008. FluidAudio (CoreML on the Neural Engine) as the Mac default engine runtime

- **Status**: Accepted
- **Date**: 2026-11-04
- **Builds on**: ADR 0005 (Parakeet-only). The model stays Parakeet TDT v3;
  only the runtime under it changes on Apple silicon.

## Context

Parakeet TDT v3 ships as the istupakov int8 ONNX export on ONNX Runtime
(parakeet-rs 0.3.5). On macOS that runs on the CPU: the export is ORT
*dynamic* int8, which the WebGPU EP can't place (19.7% of heavy ops), so Auto
resolves it to CPU under Gate G4. It is accurate and fast enough, but it keeps
the performance cores busy for the whole of every meeting. Battery is the
cost a laptop user feels.

The AE benchmark (docs/plans/asr-engine-overhaul.md, "AE results") measured
battery energy per minute of audio on Apple silicon (M5 Pro), with the same
model and corpus on every row:

| Runtime | J / audio-minute |
|---|---|
| Parakeet int8, ORT CPU (today's macOS default) | 24.2 |
| Parakeet fp16, ORT WebGPU | 21.4 |
| Parakeet f16, parakeet-rs Burn backend | 14.2 |
| **Parakeet TDT v3, FluidAudio CoreML on the Neural Engine** | **6.5** |

FluidAudio ran at 193× realtime on the same machine, so the energy win is not
bought with latency.

The alternatives:

- **The ORT CoreML EP** placed 0% of the encoder's heavy ops on the ANE with
  this export. It was never an acceleration path, and #142 removed it.
- **Burn** (parakeet-rs's pure-Rust backend) is the next most efficient row,
  but it needs a patch we would carry ourselves, and it builds with
  `panic = "abort"`, so any panic inside it kills the sidecar instead of
  failing one request.
- **ORT WebGPU fp16** saves about 12% over the CPU, which isn't worth a new
  model download.

FluidAudio (FluidInference, Apache-2.0) is a Swift package that runs Parakeet
TDT v3 as compiled CoreML models (`.mlmodelc`). Its `Encoder_v2` is an int8
per-channel re-quantization of the v3 encoder. The original 6-bit palettized
`Encoder` corrupts tokens under some right-context (FluidAudio issue #760), so
only `Encoder_v2` is acceptable. FluidAudio needs macOS 14.

## Decision

1. **On Apple silicon, Parakeet TDT v3 runs on FluidAudio** (CoreML,
   `MLComputeUnits.cpuAndNeuralEngine`), pinned at FluidAudio 0.18.2 with the
   models from `FluidInference/parakeet-tdt-0.6b-v3-coreml@7dd20fe6`. The
   encoder is always `Encoder_v2` (int8 per-channel); there is no knob.
2. **One sidecar, two runtimes.** FluidAudio links statically into the
   existing transcription sidecar through a small `@_cdecl` Swift shim. It is
   not a second process. The Parakeet backend picks its runtime from the
   model folder's format (`.mlmodelc` bundles mean CoreML, ONNX files mean
   ORT), and both runtimes decode through the same chunk path, empty-decode
   retry ladder, segment grouping and filters. The sidecar IPC is unchanged.
3. **The accelerator label stays truthful (the G4 precedent).** The sidecar
   reports `accel: "ane"` only when CoreML's `MLComputePlan` places the
   encoder's heavy ops on the Neural Engine, and `accel_partial` when some
   of them run elsewhere. It never labels a CoreML load "ane" on faith.
4. **No silent fallback.** A CoreML load or warm-up failure is a load error
   the user sees. It doesn't quietly drop to the ORT CPU engine, the same
   rule as every other engine error (surface errors, the user fixes them).
5. **The minimum macOS version becomes 14 (Sonoma).** FluidAudio needs it.
   There is no macOS 13 path that keeps the old engine: one floor, one engine
   per platform. macOS 13 users stay on the last release that supported it.
6. **The CoreML model replaces the int8 bundle on disk.** When the CoreML
   model has installed and loaded, the app deletes the int8 ONNX bundle. The
   int8 bundle is about 640 MB of dead weight beside the CoreML model, and
   nothing falls back to it.
7. **What stays on ORT:**
   - **Sortformer diarization**, on every platform. FluidAudio's diarizer is
     a different model and out of scope.
   - **Windows**, which keeps the int8 export on ORT (WebGPU with the logged
     CPU fallback, ADR 0004).
   - **Intel Macs**, which aren't shipped (best-effort only) and have no
     Neural Engine.
8. **Gate AE-G1 decides the default flip**, ratified 2026-11-03 and not
   relaxed after the fact. FluidAudio becomes the default only if, in an
   app-style re-score (the real live and backfill chunkers, not whole files):
   - its WER is at most the shipped int8's WER + 1.0 pt absolute on every
     gating slice;
   - it produces zero empty or truncated outputs beyond int8's;
   - word timestamps are kept.

## Consequences

- **The work ships as a PR series**, each passing `pnpm check`:
  1. This ADR.
  2. The macOS 14 floor (`minimumSystemVersion`, `MACOSX_DEPLOYMENT_TARGET`,
     README, release notes).
  3. The engine behind a `fluidaudio` cargo feature: the Swift shim, a
     `ParakeetRuntime { Ort, CoreMl }` selection, the truthful ANE label, and
     a macOS CI job that builds and tests the feature on a pinned Xcode 26.
  4. Shipping the feature on aarch64. The first ANE compile takes much longer
     than an ORT load, so the host gives a CoreML load (and the first engine
     query after it) a larger timeout budget.
  5. The catalog entry, with a sha256 per file, plus the download and the
     int8 deletion. An older-chip (M1/M2) run happens before this lands.
  6. The UI: a "Neural Engine" accelerator label, a "preparing model" state
     when a load takes more than about 3 s, and pruning of stale ANE caches.
     The copy needs owner approval.
- **The build needs Swift on macOS.** A `fluidaudio` build runs `swift build`
  from the sidecar's build script and fetches FluidAudio through SwiftPM at the
  version in `Package.resolved`. CI pins Xcode 26 and caches SwiftPM. Windows
  and Linux builds never touch Swift.
- **The first load after an OS update is slow.** CoreML compiles the models
  for the ANE once per OS build and caches the result under
  `~/Library/Caches/<process>/com.apple.e5rt.e5bundlecache/<OS build>`. Old
  builds' caches are left behind until PR 6 prunes them.
- **Mac and Windows run different runtimes of the same model.** Transcripts
  can differ slightly between platforms. AE-G1 bounds how much worse the Mac
  can be; it says nothing about matching Windows word for word.
- **macOS 13 is no longer supported.** That is a public, user-visible change,
  and the release notes say so.
- **parakeet-rs and ORT stay in the Mac build** for Sortformer, so the Mac
  bundle doesn't shrink by the size of ORT.
