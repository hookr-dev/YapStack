# Plan: Linux support (DEFERRED — documented, not scheduled)

> **Status: deferred.** Linux is **not** worked this phase — the active
> cross-platform push is Windows-only (see
> [`windows-support.md`](windows-support.md) and
> [ADR 0004](../adr/0004-cross-platform-rollout-windows-first.md)). This doc is
> the durable record of Linux's current state and the work required, so the
> deferral is a recorded decision and there is a ready pickup point when Linux is
> scheduled. Nothing here is committed for this release.

## Context

The Cargo dependency graph already compiles on Linux — `coreaudio-sys` /
`tauri-nspanel` are `[target."cfg(target_os = \"macos\")".dependencies]` and
`build.rs` is portable (confirmed 2026-07-01). But Linux has **never been built
or run**, and it is greenfield for several subsystems: system-audio, the
device-change watcher, volume-duck, and — most importantly — dictation output.
Several non-mac paths also inherit the Windows branch wholesale, which is only
sometimes correct on Linux.

## Committed scope decisions (when Linux is picked up)

Per ADR 0004:

- **Mic-only v1.** System-audio (loopback) capture is **deferred** — cpal has no
  ALSA output-loopback path; supporting it means a bespoke PulseAudio
  monitor-source / PipeWire node-capture backend. Ship mic-only first with a
  clean "system audio unavailable on Linux" state.
- **X11 is the tested path; Wayland is best-effort** with documented degradation
  (tray / global-shortcuts / non-activating overlays diverge by session type and
  compositor).
- **x86_64 only** — no ARM/musl.

## Current Linux status (as traced 2026-07-01)

| Subsystem | Linux status | Where |
|---|---|---|
| Device enumeration / default resolution | PARTIAL — ALSA hint clutter (`default`/`pulse`/`sysdefault`/`hw:`/`plughw:`) not collapsed | `device.rs:56-105` |
| Host backend | ALSA only (cpal 0.17 has no Pulse/JACK host to select) | `device.rs:53` |
| Device selection / matching | PARTIAL — name collisions across virtual PCMs; ALSA pcm_ids not stable across replug | `resolve_input_device` |
| Format / sample-rate negotiation | Compiles, UNVERIFIED at runtime (no hardware/CI) | `build_capture_stream` |
| System audio (loopback) | MISSING — `is_available()=false`, `start()` returns `PlatformNotSupported` | `system/mod.rs:42-66` |
| Device-change watcher | STUBBED-NOOP — never fires | `device_watcher.rs:287` |
| Device liveness | STUBBED-NOOP — always `Unknown` | `device.rs:444` |
| Device broker | IMPLEMENTED but INERT — parks on `rx.recv()`, no `devices-changed`, no failover | `device_broker.rs` |
| Mixer | IMPLEMENTED — Mixed degrades to mic-only (system buffer never populated) | `manager.rs:250-254` |
| Mic permission | MISSING — denied mic surfaces only as a later cpal failure | (no command) |
| Volume duck | STUBBED-NOOP — `StubController` returns `Unsupported` | `system_volume.rs:286` |
| **Dictation output (paste)** | **MISSING** — no `cfg` branch → empty body returns `Ok(())` while FE reports "pasted" | `commands/dictation.rs`, `useDictation.ts:527-533` |
| Overlay windows | PARTIAL — always-on-top only; steals focus; main window keeps native decorations (double titlebar); transparency needs a compositor | `commands/mod.rs:100-108`, `lib.rs:768-772` |
| Global shortcuts / tray | PARTIAL — needs StatusNotifier/AppIndicator host (absent on stock GNOME); Wayland-weak shortcuts | plugin init |
| Transcription sidecar | Compiles, NOT shipped (release never builds the linux triple) | — |
| Parakeet / Whisper accel | STUBBED — CPU-only, label correct ("cpu") | `parakeet.rs:417`, `whisper.rs:56` |
| Sidecar locator | IMPLEMENTED — `x86_64-unknown-linux-gnu` | `transcription.rs` |
| Model / cache | IMPLEMENTED path resolution, suboptimal fp32 variant (560 MB external data) | `model.rs:112-118` |
| Bundle / release / CI | MISSING — no `bundle.linux`, no `build-linux` job, no Linux CI Rust leg | `tauri.conf.json:105-131`, `release.yml`, `ci-checks.yml` |
| FE chrome / picker | PARTIAL — inherits the Windows non-mac branch; no empty/dupe device state | — |

## Completion list (for the future Linux phase)

Sizes: **S** ≈ hours, **M** ≈ 1–2 days, **L** ≈ 3+ days.

### L0 — Pipeline + bundle (reuses the Windows scaffolding)

1. **`build-linux` release job + `bundle.linux` config.** — **L**
   ubuntu-22.04 with webkit2gtk-4.1 / gtk-3 / alsa / openssl / patchelf /
   librsvg / cmake; add a `bundle.linux` block to `tauri.conf.json:105-131`
   (`deb.depends` for ALSA/webkit/openssl, appimage, desktop category).
   `release.yml` currently has only `build-macos`.
2. **Build + ship the `x86_64-unknown-linux-gnu` sidecar (CPU)**, add
   `linux-x86_64` to `latest.json` (`release.yml:156-162`), add a `linux-latest`
   CI Rust leg + generalize the externalBin placeholder
   (`ci-checks.yml:66-68`). — **M**

### L1 — Core feature parity (mic-only)

3. **Linux dictation paste.** — **L**
   `clipboard_paste` (`commands/dictation.rs:24-112`) has only `macos` and
   `windows` `cfg` branches for both the clipboard write and the auto-paste, so
   on Linux the body compiles out to the trailing `Ok(())` at line 111 while the
   FE reports "pasted" (`useDictation.ts:527-533`). (The `#[cfg(unix)]` blocks at
   `dictation.rs:118+` are test-only helpers, not a runtime paste path — don't
   mistake them for Linux coverage.)
   X11 (`xclip`/`xsel` + `xdotool`) as the tested path; Wayland (`wl-copy` +
   `wtype`/`ydotool`) best-effort. **Return a `CommandError` when the tool is
   absent** so the FE reports failure rather than a false success.
4. **Device-change watcher + `device_liveness` via PipeWire (or Pulse).** — **L**
   Same silent-no-op stubs as Windows (`device_watcher.rs:287`, `device.rs:444`);
   broker never fires (`device_broker.rs`). Subscribe to node/default changes and
   map to `DeviceEvent`.
5. **Clean "system audio unavailable on Linux" state.** — **S**
   Loopback deferred: surface a distinct unsupported state instead of the raw
   `PlatformNotSupported` error (`system/mod.rs:42-66`). Mixed already degrades to
   mic-only gracefully.
6. **Filter ALSA enumeration clutter.** — **M**
   `device.rs:56-105` doesn't collapse `default`/`pulse`/`sysdefault`/`hw:`/
   `plughw:`; prefer one canonical entry per card, drop non-openable virtual PCMs.
7. **Mic-permission handling.** — **M** (shared concept with Windows).

### L2 — Chrome polish

8. **Strip native decorations on Linux.** — **S**
   `set_decorations(false)` is `cfg(target_os = "windows")` only
   (`lib.rs:768-772`) and `titleBarStyle: "Overlay"` is macOS-only, so the WM
   draws a native title bar around the custom `TitleBar` (double chrome). Extend
   to `cfg(not(target_os = "macos"))`.
9. **Non-activating overlays.** — **M**
   X11 override-redirect / `_NET_WM_WINDOW_TYPE`; Wayland best-effort; transparency
   needs a compositor.
10. **Document tray / global-shortcut degradation** on Wayland / stock GNOME. — **S**

## Deferred beyond even the Linux mic-only phase

- **System-audio loopback on Linux** — PulseAudio monitor-source / PipeWire
  node-capture backend (a full new capture path). This is the item that would
  later bring Linux to macOS/Windows parity.
- **Linux GPU acceleration** — `parakeet-rs/webgpu` could target Vulkan via Dawn;
  otherwise Linux stays CPU-only. Optional.

## Sources

- OS-specific code trace, 2026-07-01 (6-area parallel trace + adversarial verify).
- `crates/yapstack-audio/src/{device.rs,manager.rs,system/{mod.rs,device_watcher.rs}}`
- `apps/desktop/src-tauri/src/{device_broker.rs,lib.rs,system_volume.rs,commands/{dictation.rs,mod.rs,transcription.rs}}`
- `apps/desktop/src/hooks/useDictation.ts`
- `apps/desktop/src-tauri/tauri.conf.json`, `.github/workflows/{release.yml,ci-checks.yml}`
