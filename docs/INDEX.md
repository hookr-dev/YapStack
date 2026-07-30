# Documentation index

Fast router for humans and AI agents. One line per doc — load this file first when orienting.

## Top-level

- [`README.md`](../README.md) — project overview, install, platform support, license.
- [`AGENTS.md`](../AGENTS.md) — canonical AI agent instructions (build/test commands, permission boundaries, conventions).
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) — contributor workflow for humans and agents.
- [`CODE_OF_CONDUCT.md`](../CODE_OF_CONDUCT.md) — Contributor Covenant 2.1.
- [`SECURITY.md`](../SECURITY.md) — vulnerability disclosure policy.
- [`CHANGELOG.md`](../CHANGELOG.md) — release notes (Keep a Changelog format).
- [`LICENSING.md`](../LICENSING.md) — the licensing covenant: AGPL-3.0-only, no CLA, limits-not-features, self-host is the max tier, zero phone-home.
- [`TRADEMARK.md`](../TRADEMARK.md) — name/marks policy (code freedom absolute; brand confusion restricted).
- [`DCO`](../DCO) — Developer Certificate of Origin 1.1 (contribution terms; no CLA).

## Architecture & API

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — data flow between crates, ring buffer, sidecar IPC, live transcription pipeline, AI chat tool calling, frontend component tree, analytics.
- [`API_REFERENCE.md`](API_REFERENCE.md) — exact function signatures, struct fields, error variants, Tauri command shapes. Read before adding or modifying public APIs.
- [`GLOSSARY.md`](GLOSSARY.md) — domain terms (session, segment, part, dictation, diarization, etc.).
- [`CRYPTO_SPEC.md`](CRYPTO_SPEC.md) — normative cryptographic design for E2E sync: envelopes, AAD discipline, key hierarchy, device roster, KAT vectors. Read before any sync/crypto work.
- [`self-hosting.md`](self-hosting.md) — run the sync relay yourself: install, hardening, TLS, backups.
- [`../deploy/README.md`](../deploy/README.md) — relay day-two operations quick reference (start/stop, env knobs, keep-host-awake, backup).

## Development

- [`DEVELOPMENT.md`](DEVELOPMENT.md) — build issues, feature flags, sidecar compilation, test infra, model paths.
- [`FRONTEND.md`](FRONTEND.md) — Tailwind tokens, shadcn inventory, framework stack, shortcuts, UX patterns.
- [`PRINCIPLES.md`](PRINCIPLES.md) — design, testing, and coding posture. Read before refactoring.
- [`AGENT_GUIDE.md`](AGENT_GUIDE.md) — navigation tips and common task recipes for AI agents.
- [`LINEAR_TICKETS.md`](LINEAR_TICKETS.md) — ticket structure for agent pickup.
- [`RELEASE.md`](RELEASE.md) — release runbook: version bump locations, CHANGELOG roll, tag/push, signing, draft publish, hotfix path.

## Subsystems & integrations

- [`AI_CONTEXT.md`](AI_CONTEXT.md) — AI chat context flow, tool registry + how to add a tool.
- [`LOCAL_LLM.md`](LOCAL_LLM.md) — llama.cpp, LM Studio, Ollama integration.

## History & decisions

- [`IMPLEMENTATION_LOG.md`](IMPLEMENTATION_LOG.md) — phase-by-phase build history. Use to understand *why* something was built a certain way.
- [`adr/`](adr/) — architecture decision records (append-only).

## Cross-platform

- [`plans/windows-support.md`](plans/windows-support.md) — **active** Windows rollout plan (pipeline → audio-device correctness gate → Parakeet WebGPU accel + polish).
- [`plans/linux-support.md`](plans/linux-support.md) — Linux state + gap list, **deferred** (documented, not scheduled).
- [`adr/0004-cross-platform-rollout-windows-first.md`](adr/0004-cross-platform-rollout-windows-first.md) — decision: Windows first, Linux deferred; Parakeet WebGPU/Dawn GPU (Whisper stays CPU); Linux mic-only + X11/Wayland-best-effort.

## Plans (transient)

- [`plans/`](plans/) — historical implementation plans, mostly archived. Browse only when researching prior approaches.
