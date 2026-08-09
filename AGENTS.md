# AGENTS.md

Canonical instruction file for AI coding agents working on YapStack. Read by **all major agents** (Codex, Claude Code, Copilot, Cursor, Aider, Windsurf, Zed, Junie) per the [agents.md spec](https://agents.md/).

`CLAUDE.md`, `.github/copilot-instructions.md`, and `.cursor/rules/main.mdc` are stubs that point here. Update **this file**; the stubs follow.

## Quick orientation

If you have one question, the answer is probably here. If not, follow the doc router below.

- **Goal**: real-time on-device audio capture + transcription, macOS-first.
- **Engines**: Whisper (Metal, broad language) and Parakeet TDT v3 (NVIDIA, faster on Apple Silicon via WebGPU + int8). Pick at runtime.
- **Stack**: Tauri v2 (Rust backend) + React 19/TypeScript frontend, SQLite via `tauri-plugin-sql`, sidecar binary for inference IPC over JSON-line stdin/stdout.
- **Verification**: `pnpm check` is the single command that gates merge. Run it before declaring work done.
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
cargo build -p yapstack-transcription-sidecar --features whisper                                # whisper-rs (needs cmake)
cargo build -p yapstack-transcription-sidecar --features parakeet                               # parakeet-rs + ort + Sortformer
cargo build -p yapstack-transcription-sidecar --features parakeet,coreml                        # + ORT-CoreML EP
cargo build -p yapstack-transcription-sidecar --features parakeet,webgpu                        # + ORT-WebGPU EP
cargo build -p yapstack-transcription-sidecar --features whisper,parakeet,metal,coreml,webgpu   # full Apple

# Sidecar release/dev build (copies into apps/desktop/src-tauri/binaries/ + mirrors target/debug/)
./scripts/build-sidecars.sh
./scripts/build-sidecars.sh --dev

# Force a Parakeet ORT EP at runtime (overrides Auto)
YAPSTACK_PARAKEET_ACCEL=cpu|coreml|webgpu pnpm tauri dev

# Frontend only
pnpm --filter @yapstack/desktop dev

# Full app
pnpm tauri dev
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
- **Dependency upgrades** beyond patch level. Especially `tauri`, `whisper-rs`, `parakeet-rs`, `ort`, `react`.
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
- **Windows** is unsupported officially; Parakeet GPU is WebGPU/Dawn, shipping on both macOS and Windows — non-macOS `Auto` attempts WebGPU since Gate C passed (2026-07-05, ADR 0004), with an explicit logged CPU fallback on EP-init failure and `YAPSTACK_PARAKEET_ACCEL=cpu` as the escape hatch. Whisper stays CPU on Windows. Don't delete platform-specific branches without an ADR.
- **Tauri secret signing**: the `tauri.conf.json` `pubkey` is the **public** half of an Ed25519 minisign keypair. The private key lives in GitHub Actions Secrets. Do not regenerate without user direction — rotation invalidates auto-update for every existing install.
