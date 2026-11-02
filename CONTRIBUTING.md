# Contributing to YapStack

Thanks for your interest. YapStack is built openly with AI-assisted development as a first-class part of the workflow. **Humans and AI agents are both first-class contributors here** — we care about correctness, design clarity, and tests, not provenance.

## Quickstart (humans and agents)

```bash
git clone https://github.com/hookr-dev/YapStack.git
cd YapStack
pnpm install
pnpm tauri dev          # full app
pnpm check              # gate command — must pass before opening a PR
```

Prerequisites: Rust per [`rust-toolchain.toml`](rust-toolchain.toml) (installed automatically by [rustup](https://rustup.rs)), Node.js 24, pnpm (`corepack enable && corepack prepare pnpm@latest-10 --activate`), cmake (only if building with the `whisper` feature flag).

## Where to start

| You want to... | Read this |
|---|---|
| Understand the codebase | [`docs/INDEX.md`](docs/INDEX.md) — one-line router |
| Find a good first issue | [Issues with `good first issue`](https://github.com/hookr-dev/YapStack/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) |
| Submit an AI-agent-friendly ticket | [`docs/LINEAR_TICKETS.md`](docs/LINEAR_TICKETS.md) |
| Operate as an AI agent | [`AGENTS.md`](AGENTS.md), [`docs/AGENT_GUIDE.md`](docs/AGENT_GUIDE.md) |
| Understand build & test | [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) |
| Understand design posture | [`docs/PRINCIPLES.md`](docs/PRINCIPLES.md) |

## Ways to contribute

- **Bug reports** — [bug report issue](https://github.com/hookr-dev/YapStack/issues/new?template=bug_report.yml). Include reproduction steps, platform, and relevant logs.
- **Feature requests** — [feature request](https://github.com/hookr-dev/YapStack/issues/new?template=feature_request.yml). State the *problem* before the solution.
- **Agent-ready tasks** — [agent-ready task](https://github.com/hookr-dev/YapStack/issues/new?template=agent_ready_task.yml). Structured for autonomous AI execution; works for humans too.
- **Pull requests** — see workflow below.
- **Documentation** — typos, clarifications, examples. PR with no prior discussion needed.

## Pull request workflow

1. **Fork** the repo (once we're public).
2. **Branch** from `main`. Naming: `feat/<short-name>`, `fix/<short-name>`, `chore/<short-name>`, `refactor/<short-name>`, `docs/<short-name>`.
3. **Make one focused change.** One logical change per PR.
4. **Run `pnpm check` locally** before opening the PR. CI additionally runs the Rust checks on Windows; see [`AGENTS.md` § Keeping CI green](AGENTS.md#keeping-ci-green).
5. **Update [`CHANGELOG.md`](CHANGELOG.md)** under `## [Unreleased]` if your change is user-visible. See [`AGENTS.md` § Changelog discipline](AGENTS.md#changelog-discipline) for what counts.
6. **Open the PR.** The template will prompt for summary, motivation, and a test plan.

CI must be green before review. Failing PRs go back in the queue.

## Definition of done

A change is done when **all** of the following hold:

- [ ] `pnpm check` passes locally and in CI.
- [ ] Tests cover behaviour changes (a test that fails before, passes after).
- [ ] `CHANGELOG.md` updated for user-visible changes.
- [ ] Public-API surface changes have matching docs updates.
- [ ] No new files in `.gitignore`-blocked categories accidentally tracked (`.env`, `*.db`, `*.p12`, etc.).
- [ ] Schema migrations carry an [ADR](docs/adr/).
- [ ] Sidecar IPC changes preserve forward + backward compatibility (or carry an ADR explaining the break).
- [ ] PR description states the *why* (humans write narrative; bullet diffs from a tool are fine).

## AI-Assisted Contributions

We welcome contributions made with AI assistance. We don't track which model you used or how much. We do require:

### Disclosure (when applicable)

When an AI agent meaningfully participated in writing the change, add a `Co-Authored-By:` trailer on the commit:

```
Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

Adjust the model line for the agent you used. The trailer is a contributor-attribution signal, not a legal certification.

**Never use `Signed-off-by:` for an AI.** That trailer is a Developer Certificate of Origin certification a model cannot give. The contributor of record (you) is always human.

### Accountability

You are accountable for code you submit, regardless of how it was written. That means:

- You ran `pnpm check` locally and watched it pass.
- You read the diff and can answer "why did you choose this approach?" without re-prompting your model.
- You exercised UI changes in a real browser, not just trusted the type-checker.
- You're available to address review feedback.

PRs that fail accountability checks (contributor can't explain their own code, didn't run verification, ghosted on review) will be closed regardless of code quality.

### What agents do well here

- Targeted bug fixes with clear acceptance criteria.
- Mechanical refactors (rename, signature changes, API surface widening).
- Test scaffolding for existing behaviour.
- Doc updates that follow the established structure.
- Boilerplate (new Tauri command, new shadcn component scaffold).

### What agents do badly here

- Cross-cutting design decisions (pick a transcription engine, redesign the live pipeline).
- Anything in the [`AGENTS.md` "Ask first"](AGENTS.md#ask-first) list without explicit human approval.
- Ambiguous tickets — agents will pick a direction and ship a 2000-line PR. Tighten the spec before assigning. See [`docs/LINEAR_TICKETS.md`](docs/LINEAR_TICKETS.md).
- Public-facing copy without human review (README, in-app onboarding, release notes).

## Coding conventions

- See [`docs/PRINCIPLES.md`](docs/PRINCIPLES.md) for the full design and testing posture.
- Default to **no comments**. Add one only when the *why* is non-obvious (hidden constraint, subtle invariant, workaround).
- **No speculative abstractions.** Three similar lines beat a premature helper.
- **Trust internal code.** Validate at system boundaries (user input, external APIs) only.
- **Small diffs.** Tighten before submitting.

## Verification commands

| When | Run |
|---|---|
| Before every PR | `pnpm check` |
| Iterating on Rust | `cargo test -p <crate>` or `cargo clippy --all` |
| Iterating on frontend | `pnpm test:watch` |
| Just types/lint | `pnpm typecheck && pnpm lint` |

Full command list: [`AGENTS.md` § Build & test commands](AGENTS.md#build--test-commands).

## Scope boundaries — what we will and won't accept

**Will**:
- Bug fixes with tests.
- New features that don't introduce another transcription engine, audio backend, or LLM provider abstraction.
- Performance improvements with `live_pressure` telemetry showing the win.
- Doc improvements that follow the established structure.
- Platform-support fixes for macOS Apple Silicon and Windows x64 (supported) and Intel Macs (best-effort).

**Won't**:
- New transcription engines without an [ADR](docs/adr/) demonstrating why Whisper + Parakeet aren't enough.
- New abstraction layers ("provider interface", "engine plugin system") for hypothetical future engines.
- Platform-specific changes that break another supported platform. Linux desktop support isn't a target yet.
- Changes that touch the Tauri minisign **public key** in `tauri.conf.json` — breaks every existing install's auto-updater.

## Reporting security issues

Do **not** report security issues via public GitHub issues. See [`SECURITY.md`](SECURITY.md) for the disclosure process.

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md). By participating, you agree to its terms.

## License

By contributing, you agree your contributions are licensed under the same [GNU Affero General Public License v3.0](LICENSE) that covers the project.
