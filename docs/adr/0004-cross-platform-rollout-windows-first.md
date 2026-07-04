# 0004. Cross-platform rollout: Windows first, Linux deferred

- **Status**: Accepted
- **Date**: 2026-07-01

## Context

YapStack ships **macOS-only** today. The release pipeline
(`.github/workflows/release.yml`) has a single `build-macos` job and
`create-release` `needs: [build-macos]`; CI Rust checks
(`.github/workflows/ci-checks.yml`) run only on `macos-latest`; and the
auto-updater manifest emits only a `darwin-aarch64` entry.

A full OS-specific code trace (2026-07-01) established two things that reshape
the rollout:

1. **The dependency graph already compiles on all three platforms.**
   `coreaudio-sys` and `tauri-nspanel` are declared under
   `[target."cfg(target_os = \"macos\")".dependencies]` in **both**
   `crates/yapstack-audio/Cargo.toml` and `apps/desktop/src-tauri/Cargo.toml`
   — not unconditionally — and `build.rs` is portable (`tauri_build::build()`
   only). There is **no Cargo-level compile break** on Windows/Linux. Every
   blocker lives in packaging/CI/output-injection, not the Cargo graph.

2. **Windows and Linux are at very different readiness.** Windows is ~80% wired
   at the source level — real `#[cfg(target_os = "windows")]` branches exist for
   device enumeration/resolution, WASAPI loopback, live-health silence handling,
   clipboard + paste, `CREATE_NO_WINDOW`, and overlay/tray/shortcut fallbacks.
   Linux is greenfield for system-audio, device-change watching, volume-duck,
   and dictation output.

The remaining Windows work is a bounded pipeline slice plus a specific
**audio-device correctness cluster** (the known weak point): the device-change
watcher is a silent no-op off macOS, `device_liveness` always returns `Unknown`,
and six WASAPI tests are `#[cfg_attr(target_os = "windows", ignore)]` behind an
unproven "COM cleanup crashes on CI" hypothesis.

## Decision

- **This phase ships Windows only.** Linux is fully documented
  ([`docs/plans/linux-support.md`](../plans/linux-support.md)) and deferred; no
  Linux work happens this phase. The Windows plan is
  [`docs/plans/windows-support.md`](../plans/windows-support.md).
- **Windows GA is gated on the audio-device correctness cluster**, not just on
  producing an artifact. Specifically: verify/resolve the WASAPI COM-cleanup
  question on real hardware, and land the WASAPI device-change watcher +
  `device_liveness` (shipped as a pair).
- **Windows GPU acceleration = WebGPU/Dawn for Parakeet only.** Parakeet uses
  the `ort` WebGPU execution provider (Dawn → D3D12 on Windows) — the *same code
  path already shipping on macOS* (Dawn → Metal), so it reuses proven code and
  keeps the Linux door open (Dawn → Vulkan). `ort` 2.0.0-rc.12 + `parakeet-rs`
  0.3.5 already expose the `webgpu` feature, so this is wiring + packaging, not
  new EP code. Requires switching non-Apple hosts to the **int8** Parakeet
  variant (no external `.onnx.data`) so the EP load path is unblocked — int8
  also de-risks *every* EP and cuts the model download ~4× (~2.5 GB → ~670 MB).
  **Whisper stays CPU on Windows this phase.** whisper.cpp/GGML has no DirectML
  backend (its cross-vendor GPU path is Vulkan, `whisper-rs/vulkan`), and Whisper
  is an older engine and a deprecation candidate — the added Vulkan wiring +
  `VULKAN_SDK`-in-CI overhead is not justified now. **DirectML and CUDA for
  Parakeet are not pursued:** DirectML is a Windows-only, maintenance-mode
  ("legacy") API that WebGPU strictly dominates for our cross-vendor need, and
  CUDA is NVIDIA-only and cannot coexist with the WebGPU `ort` binary in one
  build. *(This refines the initial "DirectML" framing after the GPU OSINT run,
  2026-07-01 — see References.)*
- **When Linux is picked up (a later phase):** mic-only v1 — system-audio
  loopback is deferred (cpal has no ALSA output-loopback path); **X11 is the
  tested path with Wayland best-effort** and documented degradation for
  tray / global-shortcuts / overlays.
- **ARM and musl targets are out of scope.** `current_target_triple`
  (`apps/desktop/src-tauri/src/commands/transcription.rs`) resolves one triple
  per OS (x86_64); no `aarch64-*` or `*-musl` sidecar naming this cycle.

## Consequences

- **Windows becomes the second shipping platform** and proves the reusable
  release/CI/updater/sidecar-matrix scaffolding that Linux will later inherit.
- **The device-change watcher + liveness stubs are treated as GA blockers on
  Windows**, not deferred debt. Until they land, the broker silently parks on
  `rx.recv()` with no `devices-changed` and no auto-failover; W0 adds an
  observable warning for the interim degraded state so it isn't mistaken for
  working.
- **WebGPU/Dawn pulls the int8 Parakeet variant switch into the Windows scope.**
  This is additive perf work sequenced after the CPU pipeline + audio
  correctness, i.e. it gates public GA but not an internal build. The `ort`
  WebGPU EP is flagged experimental (can return wrong output, not just crash), so
  flipping it on by default is itself gated on a golden-transcript + RTFx
  smoke-test on real NVIDIA/AMD/Intel Windows GPUs; until then it ships CPU-default
  behind `YAPSTACK_PARAKEET_ACCEL=webgpu`.
- **Linux stays honestly labeled as unbuilt.** The documented state and gap
  list exist so the deferral is a recorded decision, not an oversight; the plan
  doc is the pickup point when Linux is scheduled. When Linux ships, a follow-up
  ADR should record any scope changes (loopback backend, Wayland decisions).

## References

- [`docs/plans/windows-support.md`](../plans/windows-support.md) — Windows phase plan.
- [`docs/plans/linux-support.md`](../plans/linux-support.md) — Linux deferred state + gap list.
- `.github/workflows/release.yml`, `.github/workflows/ci-checks.yml` — macOS-only pipeline.
- `crates/yapstack-audio/Cargo.toml`, `apps/desktop/src-tauri/Cargo.toml` — macOS-gated native deps (confirmed portable).
- `crates/yapstack-audio/src/system/device_watcher.rs` — the macOS-only device-change watcher + non-macOS stub.
- GPU acceleration OSINT run (2026-07-01): 12-target primary-source trace of `ort` 2.0.0-rc.12, `parakeet-rs` 0.3.5, `whisper-rs` 0.15.1, whisper.cpp/GGML, sherpa-onnx, and comparable Tauri apps (Handy, Vibe). Key findings: "GPU" is two runtimes (Parakeet=ONNX/ort, Whisper=GGML); DirectML never applies to whisper.cpp; `webgpu` + `directml` are already wired features in the pinned crates; WebGPU/Dawn is one cross-vendor code path across macOS/Windows/Linux. Grounds the WebGPU-for-Parakeet decision above.
