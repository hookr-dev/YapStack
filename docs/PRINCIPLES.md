# Principles

Design, testing, and coding posture for YapStack. Short and opinionated. When you're unsure whether to add something, this doc should tell you.

---

## Design

**DTO boundary at the Tauri layer.** Library crates (`yapstack-common`, `yapstack-audio`, `yapstack-transcription`, `yapstack-transcription-sidecar`) use `serde` only. `specta::Type` lives in `apps/desktop/src-tauri/src/commands/` — each command defines a DTO with a `From<DomainType>` impl. Do not pull `specta` or `tauri` into the `yapstack-*` crates; it couples business logic to the command shell and bloats compile times.

**Lock-free audio path.** The capture hot path (mic / system) writes into an SPSC ring buffer with `Release`/`Acquire` orderings on a monotonic `write_pos`. Never introduce a mutex on this path. Consumers read via `snapshot_since(pos)` — if you need coordination, add a position counter, not a lock.

**Per-engine tuning.** VAD thresholds, chunk sizes, and hallucination filters are **per-engine** on the backend (`profile_for(EngineKind)` in the live loop). Parakeet is the only engine today (ADR 0005); if another engine is added, it gets its own profile rather than retuning Parakeet's values. The hallucination filter keeps only canned outros in the always-reject tier and drops short fillers only at low confidence, so a real end-of-turn "thank you" survives.

**Frontend state has one cache: the DB.** Zustand owns UI state (view, filters, selection, settings). SQLite owns persisted entities (sessions, segments, notes, folders, chat, dictation). There is no query cache — no React Query, no SWR. If you're reading the same data in two components, lift into the store or into a single parent. Adding a second caching layer in the frontend is a refactor, not a feature.

**`tauri-plugin-sql` is authoritative.** Don't maintain parallel in-memory mirrors of DB state. If you need derived data, select it fresh or cache a minimal projection in the store — don't duplicate the row.

**Segment writes serialize through one queue.** Backfill and live transcription both emit segments. `onLiveSegment` in `stores/appStore.ts` serializes through `segmentQueueTail` (a promise chain) to prevent concurrent writes from racing on the same session. Don't bypass this queue; extend it.

**Routing identity is not priority class.** The scheduler's `JobOrigin` (`Live | Dictation | Backfill | FinalFlush`) names a *priority lane* — which queue position a job occupies. The live event's `LiveSourceKind` (`Session | Dictation`) names a *routing identity* — which UI surface the event belongs to and which storage path it writes to. They're orthogonal. A dictation runtime's stop-time final flush is legitimately `JobOrigin::FinalFlush` (priority) with `source_kind: Dictation` (routing). Frontend filtering must use `source_kind`, never `origin`; conflating them silently routes dictation segments into the session transcript when a dictation hits the FinalFlush lane. When you add a parallel pipeline, ask explicitly which dimension each new variant belongs to before adding it.

**Track event ranges, not flags, when consumers poll.** A boolean is the wrong model for "is this thing currently happening?" when the consumer reads at finite cadence. The session loop polls every ~30 ms; an event that opens AND closes between two ticks never flips a boolean the loop observes. `DictationOwnsMic` uses durable `[start, end)` windows precisely because the earlier boolean-with-edge-detection lost sub-poll dictations entirely. The same shape applies whenever you have an event-with-duration that another component needs to react to in retrospect.

**Tokens, not hex.** Frontend styling consumes `--background`, `--primary`, etc. from `apps/desktop/src/index.css`. No inline hex values in components. If you need a new color, add a token first — see [`FRONTEND.md`](./FRONTEND.md).

**Don't persist speculative state.** Split-pane resize, scroll position, ephemeral UI toggles stay in memory. Only persist settings the user would miss across restarts. Every new persisted field is a migration risk — a `Zustand` store bump *and* a potential SQLite migration.

**Feature flags over back-compat shims.** When an old code path is replaced, delete it cleanly. Don't leave `// removed` breadcrumbs, renamed-with-underscore stubs, or dual implementations guarded by env vars that never flip. For truly staged rollouts, use a real feature flag and schedule a cleanup follow-up.

---

## Testing

**`pnpm check` is the gate.** Runs Rust build + test + fmt + clippy + TypeScript typecheck + ESLint + Vitest. If it fails, the PR isn't ready.

**Rust:** `cargo test --all` via `pnpm test:rust`. Hardware-dependent tests are marked `#[ignore]` and run explicitly with `cargo test -- --ignored`. Don't mark a flaky test `#[ignore]` to hide it — fix it or delete it.

**Frontend:** `vitest@4` + `@testing-library/react`. Tauri commands are stubbed via `apps/desktop/src/test/tauri-mocks.ts`. Add to the mocks when you add a command — a test that relies on an unmocked command will hang on `invoke()`.

**Integration tests touching migrations hit a real SQLite.** Do not mock `tauri-plugin-sql` for migration logic. Past incident: a mocked test passed while the prod migration silently broke `segments` because the mock tolerated a column order the real sqlx-migrate step rejected. The runtime schema-repair hook in `db::ensure_runtime_schema()` exists *because* mock/prod divergence has bitten us — don't repeat the experiment.

**Don't mock the sidecar in transcription tests.** The IPC protocol is where correctness lives. If you need to test a backend in isolation, spawn the real sidecar with a tiny model or fixture WAV. The `#[ignore]`-gated hardware tests exist for exactly this.

**Test what changes behavior.** Snapshot tests on UI trees are noise — they fail on innocuous refactors and pass on real regressions. Prefer interaction tests (`userEvent` + assertions on DOM state) over snapshots.

---

## Coding

**No speculative abstractions.** Three similar lines is better than a premature helper. Write the third occurrence before you extract the abstraction — you'll know more then.

**No error handling for cases that can't happen.** Validate at system boundaries: user input, external APIs, sidecar IPC, DB schema. Trust internal code and framework guarantees. Adding `try/catch` around internal calls obscures real bugs.

**Feature-gated code for missing feature flags is an error, not a fallback.** If the sidecar is built without the `parakeet` feature, every request returns a clear `no transcription engine compiled in` error (the `main.rs` dispatcher does this today), and an unknown `--engine` value exits at startup. Don't silently fall back to another engine — that hides the misconfiguration.

**Comments explain WHY, not WHAT.** Code says what. Comments explain hidden constraints, workarounds for specific bugs, surprising invariants. Do not write `// used by X` — that belongs in the commit message or PR description and rots as the codebase evolves.

**Delete, don't deprecate.** If a function is unused, remove it. Don't rename with a leading underscore, don't add `/** @deprecated */`, don't add a re-export. Unused code is dead weight; git history is the archive.

**Prefer dedicated tools over raw shell.** In agent prompts and workflows: use `Read`, `Edit`, `Write` instead of `cat`, `sed`, `echo`. Same discipline applies in scripts — a Rust/TS helper beats a bash one-liner for anything non-trivial.

**Branch posture: merge before cleanup.** For cleanup passes that span open PRs, merge the in-flight PRs first, branch off `main`, apply simplifications in one commit, and open one follow-up PR. Splitting a cleanup across three branches-of-branches always hurts more than it helps.

**Commits: create new, don't amend.** If a pre-commit hook fails, the commit didn't happen — `--amend` would modify the *previous* commit. Fix, re-stage, create a new commit. Don't `--no-verify` to bypass a failing hook; fix the underlying issue.

---

## Documentation

**Docs earn their place by capturing what code cannot.** Intent, invariants, design rationale, terminology, and orientation. Code is the source of truth for *what* the system is; docs explain *why* it is that way and *what must remain true* across implementations.

**Do not mirror code structure.** If a paragraph would need to be updated every time a struct gains or loses a field — or every time a function is renamed — it has drifted into capturing structure. Push that content to the type definition (rustdoc, JSDoc, TS types) and link from prose.

**Specifically, keep out of prose Markdown:**
- Field-by-field struct / interface listings.
- Function signatures and full type definitions in code blocks.
- Step-by-step procedures that mirror function bodies.
- "Files Changed" rows that restate the diff line-by-line.

**Specifically, keep in prose Markdown:**
- The *contract* a component provides — inputs, outputs, ordering guarantees, ownership rules.
- The reason a design exists at all — what was rejected and why.
- Invariants that must survive any reasonable refactor.
- Domain terms (Glossary). The names we use in tickets and conversations.
- Routing — "to understand X, read this file; to use it, see this rustdoc."

**The smell test.** Before saving a doc edit, re-read it and ask: *would this still be true and useful if someone refactored the implementation without changing observable behaviour?* If no, you're capturing code structure instead of intent — trim or push to code.

**API_REFERENCE.md is contracts, not signatures.** Name a type, describe its behavioural contract in 1–3 sentences, link to the source. The rustdoc on the type is the source of truth for fields and methods.

**IMPLEMENTATION_LOG.md is ADR-flavoured, not changelog-flavoured.** Each phase entry captures what bug or need motivated the work, what decisions were considered and rejected, what was learned. Append-only — if a later phase supersedes an earlier one, add a forward-pointer rather than rewriting. If an entry describes work that never actually merged, fix it; don't leave aspirational claims as historical record.

---

## Monorepo

**YapStack is one repo.** Server, mobile, web, shared packages all live here — no multi-repo splits. Cross-concern refactors should land as a single PR that updates everything atomically; there is no "types package" that needs to be published and consumed across repos.

---

## When in doubt

- If you're adding a file, ask whether an existing one can absorb the change.
- If you're adding a dependency, ask whether three lines of code would do.
- If you're adding a layer of indirection, ask whether the caller can just do the thing.
- If you're writing a comment longer than two lines, ask whether the code itself can be clearer.

Measure twice, cut once.
