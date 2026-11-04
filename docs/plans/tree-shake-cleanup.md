# Tree-shake Cleanup

**Status:** executed. Merged to main in 65df3a9 (`refactor/tree-shake-cleanup`). Kept as history; line references may be stale.

Working branch: `refactor/tree-shake-cleanup`

## Problem Statement

Multiple refactor passes (engine abstraction, session-resume-parts, MP3 export, dictation, parakeet+sortformer) have accumulated dead code, defensive branches that protect against impossible states, monolithic functions that obscure control flow, and stale documentation. The migrations and persistence layers stay (closed-beta continuity), but the **active code tree** needs clear branches, named-for-purpose functions, and no dangling leaves.

## Solution

A series of small, mechanical commits that each leave `pnpm check` green. No behavior changes; identical end-to-end. Where a function is split, the split is internal — public surface unchanged.

## Decision Document

- **Modules touched**: live transcription loop, silero VAD helpers, audio stream creation, session WAV state, dictation UI components, `appStore.ts` exports, CLAUDE.md, `docs/ARCHITECTURE.md`.
- **Modules NOT touched**: SQL migrations, Zustand persist migrations 1→23, sidecar IPC protocol, engine abstraction (`TranscriptionBackend` trait), audio capture core, AI tool registry.
- **Public-surface invariants**: no Tauri command signatures change, no DTO shapes change, no DB schema changes.
- **Defensive code preserved**: `TrustedAudioDirs` reconciliation, `close_orphaned_recordings`, `audio_save_locations` runtime table, `segmentQueueTail` serialization, legacy chat-message compat, stream-restart cap and cooldown.
- **Verification gate**: every commit runs `pnpm check` clean. Behavior is identical end-to-end.

## Out of Scope

- Schema or migration changes (SQLite or Zustand). Closed-beta continuity prohibits.
- Dropping the `shares` table, `sessions.wav_file_path`/`wav_duration_seconds` columns, or consolidating `ensure_runtime_schema` — all require migrations and are deferred.
- Any behavior change in VAD, hallucination filtering, prompt context, prompt decay, or stream restart policy.
- AI chat / tool calling — already clean.
- Adding new tests or coverage.
- Performance optimizations beyond what falls out naturally from extraction.

## Testing Decisions

- A good test exercises external behavior: a live recording produces the same segments/WAV/MP3 outputs as before. No tests should pin internal extracted-helper signatures.
- Existing Rust tests under `crates/yapstack-audio` and `apps/desktop/src-tauri` are the safety net for Phases 4–5. Run `cargo test --all` after each extraction.
- Frontend behavior covered by `vitest`; component consolidation in Phase 6 should leave existing dictation tests passing.
- No new tests required — refactor is behavior-preserving by construction. If a test pins a removed internal symbol, that test is updated as part of the same commit.

## Phases / Commits

Each commit leaves the codebase working (`pnpm check` green).

### Phase 1 — Confirmed dead code

1. Delete `BackfillChunk<'a>` struct. **DONE** — `VadBackfillChunk` is the live one.
2. Delete `chunk_at_silence_boundaries()` and its tests. **DONE** — backfill exclusively uses `vad_chunk_historical_audio()`.
3. ~~Delete `prompt_decay_reset()`~~. Skipped — function does not exist (audit error).
4. ~~Delete `prompt_seeded_from_backfill` field~~. Skipped — load-bearing one-shot guard for backfill prompt seeding; without it, the shared prompt would be reseeded every poll iteration.
5. ~~Drop `getDayLabel` export~~. Skipped — used internally by `groupSessionsByDay` and tested directly.
6. **DONE** — Removed "alias retained for one release" and `WhisperClient`/`WhisperClientState` references from CLAUDE.md (rename has fully shipped).
7. **DONE** — Corrected the `db::ensure_runtime_schema()` description, store version (22 → 23), DB schema version (v11 → v15), and `init_whisper_client` references.

### Phase 2 — Defensive code that can't fire

8. **DONE** — Collapsed `should_stall_restart` to a single negated boolean expression.
9. ~~Drop `if sample_rate == 0` guard~~. Skipped — guard does not exist (audit error).
10. ~~Audit `finalize_wav_only()` paths~~. Skipped — both call sites are reachable (empty-recording cleanup at session end, WAV-format finalize) and the `fallback_text` panic-recovery path is a legitimate defensive measure for tokio-task panics.

### Phase 3 — Branch consolidation (no logic change)

11. ~~Reconcile the two WAV flush thresholds.~~ Skipped — `ERROR_THRESHOLD = 10` (one-shot user error event), `WARNING_INTERVAL = 20` (periodic log), `DIAGNOSTIC_INTERVAL = 100` (success-path log) each serve a distinct purpose.
12. ~~Inline the dictation-skip branch in segment persistence.~~ Skipped — already a single early return in `onLiveSegment` at `appStore.ts`.

### Phase 4 — `live_transcription_loop` decomposition

The loop was 923 lines, now ~445. Extracted (each its own commit):

13. **DONE** — `finalize_session_wav` (post-loop final flush + finalize + DB part-row insert + part-ready emit).
14. **DONE** — `seed_prompt_from_backfill` (one-shot backfill prompt seeding).
15. **DONE** — `write_session_wav_samples` + `handle_empty_wav_flush` (the two arms of the per-tick WAV flush).
16. **DONE** — `drain_in_flight_chunks` + `dispatch_final_pending_chunks` (post-loop chunk-task drain and Phase 3 final dispatch).
17. **DONE** — `emit_fatal_sidecar_error` + `run_prompt_decay` (per-tick fatal-error emit and decay check).
18. **DONE** — `build_initial_sources_and_backfill` (pre-loop source-state setup with backfill rewind).
19. ~~Single `LiveLoopState` struct~~. Skipped — the parameters threaded through are already each cohesive (`audio_state`, `ctx`, `session`, source slice). A wrapper struct would just rename the threading without simplifying it.

### Phase 5 — `check_stream_health` decomposition

20. **DONE** — Split the 306-line function into `evaluate_listener_signal`, `evaluate_speculative_signals`, `attempt_source_restart`, `handle_buffer_replacement`. Top-level is now a 25-line orchestrator that scans sources, gathers a reason from the layered helpers, and delegates the restart.

### Phase 6 — Dictation UI consolidation

21. **DONE** — Extracted `useDictationEntry()` hook with the playing/audioRef state, store wiring, and copy/play/move-to-note/delete handlers. Both `DictationFeedEntry` and `DictationTrayItem` consume it.

### Phase 7 — Naming / clarity sweep

22. **DONE** (no-op for Rust) — All Rust identifiers were already updated in the live-transcription path (`transcription_client`, `TranscriptionClientState`, etc.). The only remaining `whisper`/`Whisper` identifiers refer legitimately to the Whisper engine.
23. **DONE** — Extracted `build_effective_prompt` and `recover_from_chunk_failure` from `transcribe_chunk`, which shrank from 261 → 156 lines.

### Phase 8 — Final docs reconciliation

24. **DONE** — Updated `docs/ARCHITECTURE.md`, `docs/API_REFERENCE.md`, `docs/DEVELOPMENT.md` to drop the `WhisperClient` alias references, the `init_whisper_client` legacy shim, and the stale store/migration version numbers; corrected the `db::ensure_runtime_schema()` description.
