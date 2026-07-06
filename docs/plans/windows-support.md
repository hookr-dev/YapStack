# Plan: Windows support (this phase)

Ship a signed, auto-updating Windows build at feature parity with macOS except
where explicitly noted. This is the **active** cross-platform phase; Linux is
deferred (see [`linux-support.md`](linux-support.md) and
[ADR 0004](../adr/0004-cross-platform-rollout-windows-first.md)).

## Context

Windows is already ~80% wired at the source level. Real
`#[cfg(target_os = "windows")]` branches exist for device enumeration/resolution
(WASAPI, `wasapi:<endpoint id>` ids with FriendlyName dedup), system-audio
loopback, live-health silence handling, clipboard + paste, `CREATE_NO_WINDOW`,
and overlay/tray/shortcut fallbacks. The dependency graph compiles on Windows —
`coreaudio-sys`/`tauri-nspanel` are macOS-gated (confirmed 2026-07-01).

So the work is **not** a port. It is: (a) a release/CI pipeline that produces and
ships the artifact, (b) closing the audio-device correctness cluster that is the
known weak point, then (c) Parakeet WebGPU acceleration + UX polish.

GPU note (per ADR 0004, refined by the 2026-07-01 GPU OSINT run): "GPU" is two
independent runtimes. **Parakeet** runs on ONNX Runtime (`ort` rc.12 via
`parakeet-rs` 0.3.5) → the **WebGPU/Dawn** EP (Dawn → D3D12 on Windows) is the
same code path already shipping on macOS, so Windows GPU is wiring + packaging,
not new EP code. **Whisper** runs on whisper.cpp/GGML (`whisper-rs`) → it has no
DirectML backend and **stays CPU on Windows this phase** (Vulkan deferred;
Whisper is an older, deprecation-candidate engine — not worth the added
`VULKAN_SDK`-in-CI overhead now). DirectML and CUDA are not pursued.

Sequencing principle: get an installable CPU build first (W0), gate GA on
audio-device correctness (W1), then layer perf + polish for public GA (W2).

Sizes: **S** ≈ hours, **M** ≈ 1–2 days, **L** ≈ 3+ days.

---

## Phase W0 — Pipeline bring-up → first installable CPU build

Goal: a `windows-latest` runner produces an installable NSIS artifact running
CPU inference, exercised by CI.

1. **Drop the forced `cuda` feature; default the Windows sidecar to CPU.** — **S**
   - The build script forces `FEATURES=whisper,parakeet,cuda`
     (`scripts/build-transcription-sidecar.sh:87-88`); `cuda = ["whisper-rs/cuda"]`
     (`crates/yapstack-transcription-sidecar/Cargo.toml`) needs CUDA+cuDNN at
     compile time, which no runner provisions → the sidecar won't build.
   - Default Windows to `whisper,parakeet` (CPU) for the first installable build;
     **drop `cuda` entirely** (NVIDIA-only, needs a toolkit, and can't coexist
     with the WebGPU `ort` binary in one build). Parakeet WebGPU acceleration
     lands in W2.
   - **2026-07-02:** the `webgpu` FEATURES flag was briefly pulled forward onto
     the Windows sidecar build, then reverted to CPU-first the same day. The flag
     returns **with** #12's Dawn-DLL staging so the shipped binary can actually
     load the EP; the first installable build stays `whisper,parakeet` (CPU).

2. **Build the `x86_64-pc-windows-msvc` sidecar in the release flow.** — **M**
   - The sidecar build is invoked with no target args (`build-sidecars.sh`,
     `release.yml:65`), so only the host (macOS) triple is ever built; the
     script's Windows branch (`build-transcription-sidecar.sh:87-89`) is never
     exercised. Tauri's `externalBin` requires the target-triple-suffixed name
     (`yapstack-transcription-sidecar-x86_64-pc-windows-msvc.exe`).
   - The desktop-side locator is already correct
     (`transcription.rs:978` `find_sidecar_path` + `current_target_triple`); it
     returns `NotFound` today only because the binary is never produced.

3. **Add a `build-windows` release job.** — **L**
   - `release.yml:14-117` has only `build-macos`. Add `windows-latest` with MSVC
     + cmake, build the sidecar (step 2), `pnpm tauri build --target
     x86_64-pc-windows-msvc`, and upload the NSIS installer + updater artifacts.
   - `create-release` must `needs: [build-macos, build-windows]` and download
     both artifact sets.

4. **Add `windows-x86_64` to the updater manifest + collect the Windows updater
   artifact.** — **M**
   - `latest.json` is hardcoded to `darwin-aarch64` (`release.yml:156-162`).
     `createUpdaterArtifacts` is globally on, but only the macOS `.app.tar.gz` is
     collected/signed (`release.yml:107-113`). Collect and sign the Windows
     updater `.zip`/`.sig` and add the `windows-x86_64` key.

5. **Add a `windows-latest` CI Rust leg + generalize the externalBin
   placeholder.** — **M**
   - Rust clippy/test runs only on `macos-latest` (`ci-checks.yml:29`), so every
     `cfg(windows)` branch and the WASAPI-COM `#[cfg_attr(target_os = "windows",
     ignore)]` tests are never compiled. Add a Windows Rust leg so the port
     can't rot.
   - The CI externalBin placeholder is macOS-suffix-only
     (`ci-checks.yml:66-68`); generalize it to the msvc `.exe` suffix or the
     Windows leg fails Tauri's externalBin existence check.

6. **Make broker inertness observable (interim safety net).** — **S**
   - Until W1 lands the real watcher, have `DefaultDeviceWatcher::new()` return
     `Err`/warn off macOS (`device_watcher.rs:287`) so a parked device broker is
     logged rather than silently doing nothing. Removed/replaced by W1.

---

## Phase W1 — Audio-device correctness (**GATES GA**)

This is the known weak point. GA does not ship until every item here is closed.

7. **Resolve the WASAPI COM-cleanup question. — GATE.** — **M**
   - Six tests are `#[cfg_attr(target_os = "windows", ignore)] // WASAPI COM
     cleanup crashes on CI` (`crates/yapstack-audio/src/device.rs:472,482,490`,
     `crates/yapstack-audio/src/manager.rs:1466,1483,1878`). They do **not** all
     build a stream: the three `device.rs` `resolve_*` tests only **enumerate**
     devices (they never construct a cpal stream); only the `manager.rs` restart
     tests **build and drop** a WASAPI stream wrapped in `SendStream` /
     `unsafe impl Send` (`stream.rs:28-47`) on a manager thread (`mic.rs`,
     `system/mod.rs`). And `manager.rs:1824
     test_start_capture_already_running_preserves_state` runs **un-ignored** on
     Windows while exercising the same `start_capture` path an ignored test
     avoids — so a blanket "stream construction crashes" story is already partly
     falsified.
   - New evidence (2026-07-02): cpal 0.17.3's WASAPI backend initializes COM as
     `COINIT_APARTMENTTHREADED` (STA) **per thread**, which directly contradicts
     `stream.rs:19-20`'s unsafe-`Send` justification claiming `COINIT_MULTITHREADED`
     (MTA). That strengthens the cross-thread-COM-drop hypothesis and means the
     investigation must cover the **enumeration-only** COM paths too, not just
     stream drop.
   - The "crash" is still an **unproven** cross-thread-COM hypothesis that could
     also be a CI-teardown artifact. On a real Windows host: run the ignored
     tests un-ignored; stress enumerate + create/drop across threads under App
     Verifier. If it reproduces, confine WASAPI stream construction + drop — and,
     if enumeration is implicated, enumeration too — to one dedicated
     COM-initialized thread with a matching apartment model. **Do not ship
     Windows until these pass un-ignored.**

8. **Implement the WASAPI default-device-change watcher.** — **L** —
   **LANDED 2026-07-05** (`feat/wasapi-device-watcher`): `IMMNotificationClient`
   via the `windows` crate's `#[implement]`, one client per watcher kind on a
   dedicated MTA COM thread (register → park → unregister on drop). Role
   filtering: `eConsole` only (one physical change = one event, not three);
   `DefaultSystemOutput` never fires on Windows (no separate alerts route —
   the console render endpoint is covered by `Output`). Supersedes W0 #6
   (broker-inertness interim net). Pending: rust-windows CI compile + canary
   hot-swap validation.
   - `DefaultDeviceWatcher` is a silent no-op off macOS (`device_watcher.rs:287`):
     `new()` returns `Ok`, the sink is stored but never invoked. cpal does not
     auto-reroute, so unplugging the active mic or switching default output
     produces **no `devices-changed` and no auto-failover**, and the device
     broker parks forever on `rx.recv()` (`device_broker.rs`).
   - Implement via WASAPI `IMMDeviceEnumerator` + `IMMNotificationClient`
     (`OnDefaultDeviceChanged` / `OnDeviceAdded` / `OnDeviceRemoved`) mapped to
     the existing `DeviceEvent` variants the broker already consumes.
   - **Design consideration:** once #8 lands, Windows system-loopback has **no
     symptom-based backup detection** — `should_stall_restart` disables the stall
     watchdog for System on Windows by design, so a silently-dead loopback has no
     fallback trigger. The `IMMNotificationClient` watcher becomes the only
     detector; weigh whether that is sufficient or a loopback-specific health
     check is still needed.

9. **Implement `device_liveness` for Windows.** — **M** — **LANDED
   2026-07-05** (same branch, shipped paired with #8):
   `IMMDeviceEnumerator::GetDevice` + `GetState == DEVICE_STATE_ACTIVE`
   (per-call COM init, RPC_E_CHANGED_MODE-aware); `E_NOTFOUND` → `Absent`,
   other inactive states → `Dead`. `strip_cpal_prefix` gained the `wasapi:`
   arm and the pinned pass-through test was inverted. Pending: CI compile +
   canary validation.
   - Always returns `Unknown` off macOS (`device.rs:445`), so the broker can't
     preserve an explicit still-alive non-default mic on a default change.
     Latent today (masked by the no-op watcher), it becomes a live correctness
     gap the moment #8 lands — **ship it paired with #8.**
   - Implement via `IMMDeviceEnumerator::GetDevice` + `GetState ==
     DEVICE_STATE_ACTIVE`.
   - Implementation detail: `strip_cpal_prefix` (`device_broker.rs:422-430`)
     strips only `coreaudio:` — it needs a `wasapi:` arm so Windows ids reach
     `device_liveness`'s lookup path. The unit test at `device_broker.rs:573`
     currently **pins the wrong pass-through**
     (`strip_cpal_prefix("wasapi:something") == "wasapi:something"`) and must be
     inverted when #9 lands.

10. **Harden device matching against duplicate FriendlyNames.** — **M**
    - ID-first resolution is reliable, but the name fallback (reached on ID-miss
      after replug) collides across duplicate WASAPI FriendlyNames in
      `resolve_input_device` / restart-candidate ordering. Disambiguate (e.g.
      pair name with endpoint id / instance) so a replugged device rebinds to the
      right endpoint.

11. **Add a mic-permission preflight.** — **M**
    - No explicit mic-permission concept; access is triggered lazily by cpal
      opening the first stream. On Windows the per-app Mic privacy setting
      (Win10/11), when denied, makes WASAPI return silence / fail on open with no
      preflight or guidance — surfaces only later as empty transcription. Add a
      preflight + a clear "enable microphone access" path.

---

## Phase W2 — Acceleration + polish → public GA

12. **WebGPU execution provider for Parakeet (reuse the macOS Dawn path) + int8
    variant.** — **L** — **LANDED 2026-07-05** (`feat/parakeet-webgpu-windows`):
    webgpu feature + full DLL staging (the pyke wgpu dist ships
    `webgpu_dawn.dll` + `dxcompiler.dll` + `dxil.dll`; the build-script glob
    caught all three), `tauri.windows.conf.json` bundles `binaries/*.dll` to
    the install root, and the explicit opt-in registers the EP with
    `error_on_failure` via `with_custom_configure` (parakeet-rs's own arm is
    soft; macOS Auto keeps shipping soft behavior). Canary confirmed the EP
    initializes and transcribes with `YAPSTACK_PARAKEET_ACCEL=webgpu`.
    Remaining from this item: the in-house WER check (below) is still open.
    - `auto_exec_config` returns `None` off macOS
      (`crates/yapstack-transcription-sidecar/src/engines/parakeet.rs:417`), so
      Parakeet is CPU on Windows. **Extend non-macOS `AccelChoice::Auto` to
      `WebGpu`, reusing the existing `ExecutionProvider::WebGPU` arm** already
      shipping on macOS (Dawn → D3D12 on Windows). No new EP code — `ort` rc.12 +
      `parakeet-rs` 0.3.5 already expose `webgpu`; the gap is wiring + packaging.
      This replaces the `parakeet.rs:406` "Windows CUDA arrives in a follow-up"
      TODO.
    - Build features: **`whisper,parakeet,webgpu`**. Never add `cuda` alongside
      `webgpu` — pyke's resolver joins them to an unmatched tag and silently
      downloads the CPU-only `ort` binary (no compile error). This `webgpu` flag
      was briefly pulled onto the Windows build on 2026-07-02, then reverted to
      CPU-first the same day; it returns **with** the Dawn-DLL staging below so
      the shipped binary can load the EP — a `webgpu` build that can't find
      `webgpu_dawn.dll` at runtime is worse than an honest CPU build.
    - Ship the **int8 single-file Parakeet variant** (istupakov
      `encoder-model.int8.onnx` + `decoder_joint-model.int8.onnx`, ~670 MB, no
      external `.onnx.data`). The fp32 TdtV3's ~2.44 GB external blob
      (`encoder-model.onnx.data`, 2,435,420,160 bytes;
      `crates/yapstack-transcription/src/model.rs:112-118`) blocks the EP load
      path; int8 also cuts the download ~4×.
    - **WER decision (recorded deviation):** the int8 default for Windows hosts
      **shipped ahead of** the in-house WER sanity-check, justified by published
      int8-vs-fp32 benchmarks. Gate C canary UAT covers transcription quality
      before GA. The in-house WER check vs fp32 **remains a pre-GA item** — do not
      treat "int8 default" as WER-validated until it clears.
    - **EP hardening (load-bearing for Gate C):** set `ORT_SEQUENTIAL` +
      `DisableMemPattern` on the session; register the EP with `error_on_failure`
      so `ort` **cannot** silently fall back to CPU while still reporting
      `accel=webgpu` — that silent fallback would invalidate the #13 / Gate C
      canary A/B (it would be comparing CPU vs CPU under a WebGPU label). Assert
      the EP actually registered, surface a real accel error (per "surface AI
      errors, never silently fall back"), and report the Parakeet accel label
      honestly.
    - **Packaging:** `ort` **statically links** its ONNX Runtime — there is **no
      separate `onnxruntime.dll`** to stage. What must ship is **every dll in the
      pyke wgpu dist** the sidecar loads at runtime: `webgpu_dawn.dll` plus its
      possible `dxcompiler.dll` / `dxil.dll` companions. Stage them next to the
      sidecar `.exe` via a Windows `bundle.resources` glob — generalize the
      existing macOS `bundle.macOS.frameworks: ["binaries/libwebgpu_dawn.dylib"]`
      pattern (`tauri.conf.json`). `copy-dylibs` is dev-only and does not package
      for shipping.
    - Largest single Windows item; land after W0/W1 so there's a shippable
      internal CPU build first. Gates public GA, not internal builds.

13. **WebGPU hardware-validation gate — GATES GPU-on-by-default.** — **M** —
    **GATE C PASSED 2026-07-05:** the canary's explicit verdict — WebGPU is
    **correct and clearly faster than CPU** on real Windows hardware via the
    strict opt-in path. Non-macOS `Auto` now attempts WebGPU with strict EP
    registration; EP-init failure routes through `load_model`'s explicit CPU
    fallback (logged `live_accel_fallback`, honest `accel=cpu` label), so
    GPU-less machines degrade visibly, not silently.
    `YAPSTACK_PARAKEET_ACCEL=cpu` is the escape hatch. **Residual for GA:**
    validated on the canary's GPU vendor only — broader NVIDIA/AMD/Intel
    coverage remains a GA consideration, mitigated by the honest-fallback
    design and the env escape hatch.
    - The `ort` WebGPU EP is flagged **experimental** (can return *wrong*
      transcripts, not just crash) with an open macOS multi-thread crash
      (onnxruntime #27592). Before flipping Parakeet WebGPU on by default: run a
      golden-transcript + RTFx smoke-test on real NVIDIA / AMD / Intel Windows
      GPUs — confirm it is **correct AND faster than CPU** on the TDT
      transducer's dynamic shapes — and serialize ORT session creation/inference.
      Until validated, ship CPU-default with WebGPU behind
      `YAPSTACK_PARAKEET_ACCEL=webgpu`. *(Whisper accel-label fix at `whisper.rs:56`
      is deferred with Whisper GPU — Whisper stays CPU on Windows, which the
      current label reports correctly.)*
    - This A/B is only meaningful if #12's EP hardening (`error_on_failure`)
      prevents a silent CPU fallback under a `webgpu` label; without it the
      canary would compare CPU vs CPU and report a false "no speedup".

14. **Give overlays non-activating / all-spaces behavior.** — **M**
    - The NSPanel behavior (nonactivating, all-spaces, fullscreen-auxiliary) is
      macOS-only (`lib.rs:776-802`); the Windows fallback is `set_always_on_top`
      + show/hide (`commands/mod.rs:100-108`), so the dictation bubble **steals
      focus** (defeating its purpose before paste) and won't float over
      fullscreen. Apply `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` via
      `SetWindowLongPtr` on the HWND.

15. **Replace PowerShell SendKeys auto-paste with `SendInput`.** — **M**
    - `clipboard_paste` on Windows cold-starts PowerShell + `System.Windows.Forms`
      per paste (`dictation.rs:95-108`) — hundreds of ms, policy-fragile,
      unreliable against elevated/game windows. Use direct `SendInput` /
      `keybd_event` via the `windows` crate.

16. **Add Authenticode signing.** — **DESCOPED (operator decision,
    2026-07-05): no certificate, and none planned.** Windows ships unsigned.
    - Accepted consequences: SmartScreen "More info → Run anyway" on first
      install (document this in the README/release notes when Windows builds
      go public) and some AV false-positive risk on unsigned executables.
    - NOT affected: the auto-updater — Tauri's updater verifies the minisign
      signature (`TAURI_SIGNING_PRIVATE_KEY`), not Authenticode, so updates
      remain integrity-checked and functional.
    - The gated signing placeholder in `release.yml` (job-level
      `WINDOWS_SIGN_CERT` env, permanently-skipped step) stays as-is: it costs
      nothing, cannot fire without the secret, and preserves the wiring if a
      certificate ever materializes.

17. **Frontend Windows polish.** — **M**
    - Ctrl/Alt modifier glyphs instead of hardcoded ⌘/⌥; non-macOS permission
      copy (no "System Settings > Privacy > Screen Recording"); empty/duplicate
      device-list states in the picker; decide the Live Insights overlay
      fallback-or-disable. Note this is **not** a silent no-op: the insight
      window is declared unconditionally in `tauri.conf.json` and
      `show_overlay_panel` has a working non-macOS branch, so on Windows the
      overlay **does** show (`useInsightOverlayController.ts:111-117` only guards
      the panel call) — but, like the dictation bubble, it **steals focus** until
      #14. The work item is the fallback-or-disable decision: keep it, gate it
      behind #14's non-activating fix, or disable it off macOS.

---

## Additions from the 2026-07-02 review

An 8-surface adversarial code review (2026-07-02, with adversarial verification)
confirmed the gaps below and closed four of them in this changeset.

**Already landed in this changeset (2026-07-02):** single-instance plugin (a
second launch focuses the running app instead of starting a duplicate);
`core:window` capability grants for the overlay windows; `CREATE_NO_WINDOW` on
the dictation-side process spawns (no console flash); per-platform tray icon. On
the W0 pipeline items (#3/#16), the `build-windows` job now builds `--bundles
nsis` and the Authenticode signing step is env-guarded so it skips cleanly when
the cert secret is absent.

**Open items:**

18. **Unicode-safe clipboard write on Windows.** — **S** — *(W2, land with #15;
    gates non-English dictation)*
    - `clip.exe` receives raw `text.as_bytes()` over the pipe
      (`dictation.rs:55-75`), which it decodes as the console **OEM codepage**
      (no `CF_UNICODETEXT` path), so any non-ASCII dictation is mangled. Replace
      with a Unicode-safe clipboard write (`SetClipboardData(CF_UNICODETEXT, …)`
      via the `windows` crate). #15 only replaces the *paste* half — this is the
      *copy* half and independently gates non-English dictation correctness.

19. **Decide the Windows default hotkey scheme.** — **M** — *(W2, decision +
    remap)*
    - The `Ctrl+Shift+D/C/X` defaults (`appStore.ts:218-236`,
      `lib/shortcuts.ts:32-56`) steal ubiquitous shortcuts via exclusive
      `RegisterHotKey`, and the `CommandOrControl+Alt+*` variants collide with
      AltGr on international layouts. Pick a Windows-appropriate default set and
      remap.

20. **Fix Win-key capture conflation.** — **S** — *(W2)*
    - `eventToGlobalBinding` folds `metaKey` into `CommandOrControl`
      (`shortcuts.ts:283`), so capturing e.g. `Win+X` silently rebinds it as
      `Ctrl+X`. Keep the Windows/Super modifier distinct when capturing on
      Windows.

21. **Hide/disable or implement the volume duck on Windows.** — **S–M** — *(W2)*
    - The volume duck is a silent no-op stub off macOS (`system_volume.rs:87-90`)
      while `DictationTab` still shows live duck controls
      (`DictationTab.tsx:419-454`). Either hide/disable the control on Windows or
      implement it via `ISimpleAudioVolume`.

22. **Align the stored device name with the matching key.** — **S** — *(W1, fix
    with #10)*
    - Restart-by-name stores the WASAPI `DeviceDesc` (`mic.rs:63-66`) but matching
      resolves on `FriendlyName` (`device.rs:250-263`), so restart-by-name is
      silently dead on Windows. Fix alongside #10's FriendlyName disambiguation.

23. **Decide `%LOCALAPPDATA%` for machine-local caches.** — **M** — *(W2,
    decision)*
    - 2.5+ GB of models/audio/DB land in `%APPDATA%` **Roaming**
      (`lib.rs:557-586`), which roams with the user profile. Decide a
      `%LOCALAPPDATA%` migration for the machine-local caches (models, audio)
      versus what legitimately roams.

24. **Clamp off-screen window restore to a monitor.** — **S** — *(W2)*
    - `App.tsx:70-80` restores raw saved coords with no monitor-intersection
      clamp; Windows (unlike AppKit) won't pull an off-screen window back on
      screen, so a saved position on a now-absent monitor is unreachable. Clamp to
      the intersection of the available monitors.

25. **Canary: confirm hidden-window dictation survives WebView2 throttling.** —
    **M** — *(W1-gate-adjacent canary check; may escalate)*
    - `backgroundThrottling: "disabled"` is WKWebView-only. On WebView2 the
      tray-hidden main window — which runs the dictation state machine — is
      subject to Chromium hidden-tab timer throttling. The canary must observe
      hidden-window dictation; if throttled, set `IsSuspendOnHiddenEnabled` /
      disable throttling, or move the orchestration out of the hidden webview.

26. **Configure the WebView2 install mode.** — **S** — *(W2, decision)*
    - `bundle.windows` leaves WebView2 at the default `downloadBootstrapper` — an
      install-time network dependency. Decide `embedBootstrapper` /
      `offlineInstaller` for offline installs.

27. **Declare a minimum Windows version.** — **S** — *(W0/W2, decision +
    installer check)*
    - The main app links `ort` via `silero` with load-time `DirectML.dll` /
      `dxcore.dll` imports (Win10 1903+ floor) and requires WebView2, but
      `bundle.windows` has no analog of macOS `minimumSystemVersion`. Declare the
      floor and enforce it in the installer.

28. **Use the work area for the dictation-bubble bottom margin.** — **S** — *(W2)*
    - The bubble's 30px bottom margin is measured from the full monitor extent
      (`useDictation.ts:24-26,55-58`), which on Windows sits inside the taskbar
      band. Measure from the monitor **work area** instead.

29. **Handle `\\?\` verbatim paths in the trusted-audio-dir compare.** — **S–M**
    — *(W2)*
    - The canonicalize-then-compare for trusted audio dirs (`lib.rs:237-252`,
      `db.rs:231,247`) breaks against Windows `\\?\` verbatim paths, so a
      legitimately trusted dir can fail the compare. Normalize the verbatim prefix
      before comparing.

30. **Adopt Windows tray-icon conventions.** — **S** — *(W2)*
    - The tray uses the `show_menu_on_left_click` default with no
      `on_tray_icon_event` handler and no tooltip — non-idiomatic on Windows,
      where left-click typically opens the app and a tooltip is expected. Add a
      left-click open handler + tooltip.

31. **Record the clipboard-retention privacy decision.** — **S** — *(W2,
    decision)*
    - Every dictation clobbers the clipboard and is retained by Win+V / cloud
      clipboard history. Restore-after-paste is racy; deciding **not** to restore
      is acceptable, but the privacy trade-off must be explicitly recorded rather
      than left implicit.

32. **Debounce/upstream the hold-to-dictate Released poll.** — **S** — *(W2,
    note)*
    - global-hotkey 0.7.0's `GetAsyncKeyState` release-detection loop busy-spins a
      core per held press — functional, but a battery/thermal cost. Debounce
      locally or push a fix upstream.

---

## Critical path to a shipping Windows build

W0 (pipeline, CPU) → **W1 gate** (COM-cleanup verified + watcher/liveness landed).
Parakeet WebGPU acceleration (#12, gated on-by-default by the #13 hardware
smoke-test) and Authenticode (#16) are the gate between an internal build and
public GA.

## Explicitly out of scope this phase

Linux (all) · **Whisper GPU on Windows** (stays CPU — Vulkan deferred; Whisper is
an older, deprecation-candidate engine, not worth the `whisper-rs/vulkan` +
`VULKAN_SDK`-in-CI overhead now) · **DirectML & CUDA for Parakeet** (WebGPU/Dawn
covers cross-vendor GPU; DirectML is sunset/Windows-only, CUDA is NVIDIA-only and
can't share the WebGPU `ort` binary) · ARM/musl triples (`current_target_triple`
is x86_64-per-OS, `transcription.rs`) · reconciling
`check_system_audio_permission` vs the real macOS TCC grant.

## Sources

- OS-specific code trace, 2026-07-01 (6-area parallel trace + adversarial verify).
- GPU acceleration OSINT run, 2026-07-01 (12-target primary-source trace of `ort`
  rc.12 / `parakeet-rs` 0.3.5 / `whisper-rs` 0.15.1 / whisper.cpp / sherpa-onnx +
  Handy/Vibe precedent). Anchors the WebGPU-for-Parakeet decision.
- `.github/workflows/release.yml`, `.github/workflows/ci-checks.yml`
- `apps/desktop/src-tauri/tauri.conf.json`
- `scripts/build-transcription-sidecar.sh`, `scripts/build-sidecars.sh`
- `crates/yapstack-audio/src/{device.rs,manager.rs,stream.rs,system/device_watcher.rs}`
- `apps/desktop/src-tauri/src/{device_broker.rs,lib.rs,commands/{dictation.rs,transcription.rs,mod.rs}}`
- `crates/yapstack-transcription-sidecar/src/engines/{parakeet.rs,whisper.rs}`
- `crates/yapstack-transcription/src/model.rs`
