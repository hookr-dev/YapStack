# AGENTS.md

Canonical instruction file for AI coding agents working on YapStack. Read by **all major agents** (Codex, Claude Code, Copilot, Cursor, Aider, Windsurf, Zed, Junie) per the [agents.md spec](https://agents.md/).

`CLAUDE.md`, `.github/copilot-instructions.md`, and `.cursor/rules/main.mdc` are stubs that point here. Update **this file**; the stubs follow.

## Quick orientation

If you have one question, the answer is probably here. If not, follow the doc router below.

- **Goal**: real-time on-device audio capture + transcription on macOS and Windows.
- **Engine**: Parakeet TDT v3 (NVIDIA, 25 European languages, WebGPU on macOS and Windows with CPU fallback). Whisper was removed per ADR 0005.
- **Stack**: Tauri v2 (Rust backend) + React 19/TypeScript frontend, SQLite via a repo-owned rusqlite command backend, sidecar binary for inference IPC over JSON-line stdin/stdout.
- **Verification**: `pnpm check` is the local gate. Run it before declaring work done. CI also runs Rust checks on Windows, which a Mac can't reproduce; see [Keeping CI green](#keeping-ci-green).
- **License**: AGPL-3.0. New code stays open under the same license.

## Doc router (read these on demand)

- [`docs/INDEX.md`](docs/INDEX.md) — fastest router; one-line description per doc.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — data flow, crates, IPC, state, analytics. Cross-cutting concerns.
- [`docs/API_REFERENCE.md`](docs/API_REFERENCE.md) — exact signatures, struct fields, error variants, Tauri command shapes.
- [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) — build issues, feature flags, sidecar compilation, test infrastructure, model paths.
- [`docs/FRONTEND.md`](docs/FRONTEND.md) — Tailwind tokens, shadcn inventory, framework stack, shortcuts, UX patterns.
- [`docs/AI_CONTEXT.md`](docs/AI_CONTEXT.md) — AI chat context flow, tool registry + how to add a tool.
- [`docs/PRINCIPLES.md`](docs/PRINCIPLES.md) — design, testing, and coding posture. Read before refactoring.
- [`docs/LOCAL_LLM.md`](docs/LOCAL_LLM.md) — llama.cpp, LM Studio, Ollama integration.
- [`docs/IMPLEMENTATION_LOG.md`](docs/IMPLEMENTATION_LOG.md) — phase-by-phase build history. Use to understand *why*.
- [`docs/GLOSSARY.md`](docs/GLOSSARY.md) — domain terms (session, segment, part, dictation, diarization).
- [`docs/AGENT_GUIDE.md`](docs/AGENT_GUIDE.md) — navigation and common task recipes for agents.
- [`docs/LINEAR_TICKETS.md`](docs/LINEAR_TICKETS.md) — ticket structure for agent pickup.
- [`docs/RELEASE.md`](docs/RELEASE.md) — release runbook (version-bump locations, CHANGELOG roll, tag/push, signing, hotfix path).
- [`docs/adr/`](docs/adr/) — architecture decision records (append-only).

## Build & test commands

```bash
# Full verification — gates merge
pnpm check                                 # Rust build + test + fmt + clippy + TS typecheck + ESLint + vitest

# Test
pnpm test                                  # Rust + frontend
pnpm test:frontend                         # vitest
pnpm test:rust                             # cargo test --all
pnpm test:watch                            # vitest watch

# Targeted Rust tests
cargo test -p yapstack-audio                       # single crate
cargo test -p yapstack-audio -- ring_buffer        # specific module
cargo test -p yapstack-audio -- --ignored          # hardware-dependent

# Lint
pnpm lint                                  # cargo fmt + clippy + ESLint
pnpm typecheck                             # tsc

# Feature-flag transcription sidecar builds
cargo build -p yapstack-transcription-sidecar --features parakeet                               # parakeet-rs + ort + Sortformer
cargo build -p yapstack-transcription-sidecar --features parakeet,webgpu                        # + ORT-WebGPU EP (what ships on Windows)
cargo build -p yapstack-transcription-sidecar --features parakeet,webgpu,fluidaudio             # + CoreML/ANE via FluidAudio (what ships on Apple silicon; Xcode 26, ADR 0008)

# Sidecar release/dev build (copies into apps/desktop/src-tauri/binaries/ + mirrors target/debug/,
# then smoke-launches the staged copies on macOS)
./scripts/build-sidecars.sh
./scripts/build-sidecars.sh --dev

# Force a Parakeet ORT EP at runtime (overrides Auto)
YAPSTACK_PARAKEET_ACCEL=cpu|webgpu pnpm tauri dev

# Frontend only
pnpm --filter @yapstack/desktop dev

# Full app. Every `tauri dev` rebuilds the dev sidecar first (beforeDevCommand), then starts vite.
pnpm dev                                   # = pnpm tauri dev
pnpm tauri dev -f sync                     # with the desktop sync feature
pnpm tauri build

# DMG packaging
./scripts/build-dmg.sh
```

## Permission boundaries

Agent should respect these without being asked.

### Always OK (no confirmation needed)
- Reading any file in the repo.
- Running tests, lints, typechecks, formatters.
- Editing source files inside a single logical task scope.
- Adding tests.
- Updating `CHANGELOG.md` under `## [Unreleased]`.
- Creating new branches off `main`.
- Committing locally on a non-`main` branch.

### Ask first
- **Database schema migrations** (`apps/desktop/src-tauri/src/db.rs` versioned migrations, frontend `getDb()` runtime patches). Schema bumps require an [ADR](docs/adr/) and a migration plan.
- **Sidecar IPC protocol changes** (`crates/yapstack-common/src/types.rs` `SidecarRequest`/`SidecarResponse`). Forward/backward compatibility matters across mismatched sidecar/host versions.
- **Tauri command surface changes** (add/remove/rename in `apps/desktop/src-tauri/src/commands/`). These regenerate TS bindings via specta.
- **Dependency upgrades** beyond patch level. Especially `tauri`, `parakeet-rs`, `ort`, `react`.
- **Branch-protection or repo-settings changes** via `gh api`.
- **Force-push** or any operation that rewrites published history.
- **Public-facing copy** in README, CHANGELOG release entries, in-app onboarding.

### Never (refuse and surface)
- Committing secrets, API keys, `.env`, `.p12`, or any credential blob.
- `git push --force` to `main` or any tag without explicit user confirmation **for that specific operation**.
- Skipping hooks (`--no-verify`) or signature checks (`--no-gpg-sign`).
- Modifying `LICENSE`.
- Touching the Tauri minisign **public key** in `tauri.conf.json` — breaks the auto-updater for existing installs.
- Deleting `v0.1.0` or any historical release tag.
- Posting content to external services (Slack, Linear, GitHub Discussions) without explicit per-action user direction.
- Generating or auto-filling commit messages without user input on substantive changes.

## Coding posture (short version)

Full version in [`docs/PRINCIPLES.md`](docs/PRINCIPLES.md).

- **No speculative abstractions.** Three similar lines beats a premature helper. Don't design for hypothetical future requirements.
- **Default to no comments.** Add one only when the *why* is non-obvious (hidden constraint, subtle invariant, workaround for a specific bug).
- **Trust internal code.** Validate at system boundaries (user input, external APIs) only.
- **One logical change per PR.** Tighten the diff before submitting.
- **For UI changes**, exercise the feature in a browser before reporting done. Type-checks and tests verify code correctness, not feature correctness.
- **Tests are not optional** for behaviour changes. Add a test that fails before your fix and passes after.

## Changelog discipline

`CHANGELOG.md` is a first-class artifact and **must be updated in the same PR/commit** that makes a user-visible change. "User-visible" means anything a downstream consumer notices: new features, API renames, schema migrations, behaviour changes, perf wins they can feel, bug fixes worth calling out, dependency upgrades that affect compatibility.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) under `## [Unreleased]`:

- **Added** — new features.
- **Changed** — behaviour changes to existing features.
- **Deprecated** — soon-to-be-removed features.
- **Removed** — deletions.
- **Fixed** — bug fixes worth surfacing.
- **Security** — vulnerability fixes.

When cutting a release, rename `[Unreleased]` to the new version with the date (`## [1.0.0-alpha.6] - YYYY-MM-DD`) and start a fresh empty `[Unreleased]` block above it.

**Skip the changelog** for: pure refactors, internal test changes, formatting, doc-only edits that don't affect APIs, dependency bumps that don't change behaviour. When in doubt, add an entry — it's cheap.

A missing changelog entry on a user-visible PR is a review-blocking gap, like a missing test.

## Commit and PR conventions

- **Commit subject**: lowercase, imperative (`fix: ...`, `feat: ...`, `chore: ...`, `refactor: ...`, `docs: ...`, `test: ...`).
- **Body**: optional, but use it for non-obvious *why*.
- **Co-author trailer for AI-assisted commits** (required when applicable):
  ```
  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
  ```
  Adjust the model line for the actual agent. **Never use `Signed-off-by:` for an AI** — that is a DCO legal certification a model cannot give. The contributor remains the author of record.
- **PR description**: a human writes the *why*. Bullet diffs from a tool are fine; narrative must come from the contributor.
- **Verification**: every PR description must state which command was run and that it passed (`pnpm check`).

## Pre-commit / pre-push checklist

Run through this before declaring a task done.

- [ ] `pnpm check` is green.
- [ ] `CHANGELOG.md` updated under `## [Unreleased]`, or change is genuinely not user-visible.
- [ ] No new files in `.gitignore`-blocked categories accidentally tracked (`.env`, `*.db`, `*.p12`, etc.).
- [ ] No personal paths, hostnames, or API tokens in diffs.
- [ ] Tests added for behaviour changes.
- [ ] Public API changes have matching `docs/` updates.
- [ ] If touched a schema migration: ADR added.
- [ ] If touched sidecar IPC: forward/backward compatibility considered.

## Shared environment notes

- **macOS Apple Silicon** is the primary target. Code must work there.
- **Intel Mac** is best-effort: don't break it, don't optimize for it.
- **Windows (x64)** is supported: CI runs clippy and tests on Windows, and releases ship a Windows installer. Parakeet GPU is WebGPU/Dawn, shipping on both macOS and Windows — non-macOS `Auto` attempts WebGPU since Gate C passed (2026-07-05, ADR 0004), with an explicit logged CPU fallback on EP-init failure and `YAPSTACK_PARAKEET_ACCEL=cpu` as the escape hatch. Don't delete platform-specific branches without an ADR.
- **Linux** is not a target yet (the relay server is the exception: it is deployed on Linux).
- **Worktrees**: a fresh worktree needs `pnpm install` and the sidecar staged in `apps/desktop/src-tauri/binaries/` (`./scripts/build-sidecars.sh --dev`) before `pnpm check`. Each worktree's `target/` grows to 10–20 GB, so delete it after merge. A worktree nested inside the main checkout (e.g. `.claude/worktrees/`) can't build `yapstack-sync` (so `pnpm check` and `--features sync` both fail): cargo walks up from `vendor/cr-sqlite/core/rs/bundle_static` to the outer checkout's workspace. Append a temporary empty `[workspace]` to `vendor/cr-sqlite/core/rs/bundle_static/Cargo.toml` and revert it before committing.
- **Tauri secret signing**: the `tauri.conf.json` `pubkey` is the **public** half of an Ed25519 minisign keypair. The private key lives in GitHub Actions Secrets. Do not regenerate without user direction — rotation invalidates auto-update for every existing install.

## Keeping CI green

CI (`.github/workflows/ci-checks.yml`) runs more than `pnpm check` does on one machine:

| Job | Runner | What it runs |
|---|---|---|
| Rust Format | Ubuntu | `cargo fmt --all -- --check` |
| Frontend | Ubuntu, Node 24 | ESLint, typecheck, vitest (pnpm 10.28.2) |
| Rust (macos-latest) | macOS | `cargo clippy --all -- -D warnings`, `cargo test --all` |
| Rust sync feature (macos-latest) | macOS, plus `nightly-2023-10-05` | `cargo clippy -p yapstack-desktop --features sync --all-targets -- -D warnings`, `cargo test -p yapstack-desktop --features sync` |
| Rust fluidaudio feature (macos-26) | macOS, Xcode 26.6 pinned | `cargo clippy -p yapstack-transcription-sidecar --features webgpu,fluidaudio --all-targets -- -D warnings`, `cargo test` with the same features, a `minos 14.0` check on the sidecar |
| Rust (windows-latest) | Windows | `cargo clippy --all -- -D warnings`, clippy on the sidecar with `--features webgpu`, `cargo test --all` |

- **Windows-only code compiles only in CI.** Anything behind `#[cfg(windows)]` / `cfg(target_os = "windows")` is never built on a Mac. Review those paths by reading them, keep them small, and expect the Windows leg to be the first to catch mistakes.
- **The Rust toolchain is pinned** in `rust-toolchain.toml`. Bump it in its own PR; new releases routinely add clippy lints, and CI runs clippy with `-D warnings`.
- **The desktop `sync` feature has its own CI job** ("Rust sync feature (macos-latest)"); `pnpm check` doesn't build it. If you touch sync integration in the desktop crate, run `cargo test -p yapstack-desktop --features sync` locally first (it needs `rustup toolchain install nightly-2023-10-05` for the vendored cr-sqlite bundle).
- **Tauri npm/Rust versions must align.** `scripts/check-tauri-versions.mjs` (run in the Rust Format job) fails when an `@tauri-apps/*` npm package and its Rust crate resolve to different major.minor versions in the lockfiles, which `tauri build` rejects. Run `node scripts/check-tauri-versions.mjs` locally after bumping a Tauri plugin on either side.
- **Only Server Integration is path-filtered.** `server-integration.yml` runs only when `crates/yapstack-server/`, `crates/yapstack-sync/`, `crates/yapstack-crypto/`, `Cargo.lock`, `Cargo.toml`, `rust-toolchain.toml`, `deploy/` or the workflow itself change. `ci.yml` has no paths filter: it runs on every push to a PR targeting `main` and every push to `main`, and a new PR push cancels that PR's in-flight run.
- **Tests must be deterministic.** Don't assert on thread timing or lock races; SQLite's busy handler isn't fair, so a race that passes locally can starve on a slower runner. Use hooks, channels, or explicit ordering instead.
- **Never make CI green by retrying, sleeping, or `#[ignore]`-ing a failing test.** Find the cause. If a test is genuinely flaky and blocking, say so and ask before quarantining it.
- **Dependabot PRs fail for real reasons too.** Check whether the failure reproduces on `main` before blaming the bump, and vice versa.
