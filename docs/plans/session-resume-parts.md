# Session Resume — Parts Model

**Status:** executed. Merged in 29c4485 (`feat/session-resume-parts`); `session_audio_parts` is the durable source of truth (see ARCHITECTURE.md, "Multi-part playback and seeking").

## Context

The first pass at session resume (commits `f3eca93` through the simplify pass on this branch) made MP3 the *mutable source of truth* — each resume decoded the prior MP3 back to PCM, appended new audio, and re-encoded the whole concatenated thing. That design failed code review on three P1 grounds (ignores persisted audio path, duration math wrong, destructive ops happen mid-startup) and one architectural ground (lossy generation loss on every resume; couples decoding into the live recording path).

This plan replaces that design entirely. Recording becomes append-only at the *part* level: each recording run produces its own immutable file, and a session's audio is the conceptual concatenation of its parts. No file is ever decoded, mutated, or deleted as part of the resume path.

## Architecture

A session owns an ordered sequence of **audio parts**, each part being one finalized recording run in its native format (WAV or MP3 per the user's setting at the time the part was recorded).

```
session
  ├── part 0:  {session_id}.0.mp3   (12.4 s, 48 kHz, mp3)   ← original recording
  ├── part 1:  {session_id}.1.mp3   ( 3.2 s, 48 kHz, mp3)   ← first resume
  ├── part 2:  {session_id}.2.wav   ( 8.7 s, 44.1 kHz, wav) ← second resume after format change
  └── part N: ...
```

Properties:
- **Append-only**: existing parts are never mutated. New runs add a new part.
- **No decoding** anywhere in the live recording path. `minimp3` is dropped.
- **Heterogeneous formats acceptable**: a session's parts can mix WAV and MP3 if the user changed the export format between runs.
- **Session duration** = `SUM(parts.duration_seconds)`.
- **Resume's `session_offset_base_seconds`** = the same SUM at resume time. Segment offsets stay continuous.

## DB schema

```sql
-- Migration v12: session audio parts + drop legacy single-file columns

CREATE TABLE session_audio_parts (
  id TEXT PRIMARY KEY,                          -- UUID
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  part_index INTEGER NOT NULL,                  -- 0-based, contiguous, monotonic per session
  file_path TEXT NOT NULL,
  format TEXT NOT NULL CHECK (format IN ('wav','mp3')),
  duration_seconds REAL NOT NULL,
  sample_rate INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE (session_id, part_index)
);
CREATE INDEX idx_audio_parts_session ON session_audio_parts(session_id, part_index);

-- Backfill: every existing session with a wav_file_path becomes one part_0 row.
INSERT INTO session_audio_parts (id, session_id, part_index, file_path, format, duration_seconds, sample_rate, created_at)
SELECT
  lower(hex(randomblob(16))),
  id,
  0,
  wav_file_path,
  CASE WHEN wav_file_path LIKE '%.mp3' THEN 'mp3' ELSE 'wav' END,
  COALESCE(wav_duration_seconds, 0),
  48000,                                        -- not preserved on the old row; default
  COALESCE(updated_at, created_at)
FROM sessions
WHERE wav_file_path IS NOT NULL;

-- Drop legacy single-file columns. SQLite ≥3.35 supports DROP COLUMN.
ALTER TABLE sessions DROP COLUMN wav_file_path;
ALTER TABLE sessions DROP COLUMN wav_duration_seconds;
```

`sessions.duration_seconds` stays — recomputed from the parts SUM at finalize. No DTO shim is needed because the parts-aware AudioPlayer reads parts directly.

`sample_rate=48000` is a default for backfilled rows. The parts-aware player only uses sample rate for diagnostics; for new parts, it's whatever the live loop used.

## Recording flow

1. **New session**: backend records to `{session_id}.0.wav`. On stop:
   - if `audio_export_format = mp3`: encode → `{session_id}.0.mp3`, delete the WAV, insert part with `format=mp3`.
   - if `audio_export_format = wav`: keep the WAV, insert part with `format=wav`.
2. **Resume**: backend records to `{session_id}.{N}.wav` where N = `MAX(part_index) + 1`. Same finalize logic, insert part N. **No prior file is ever read or modified.**

The live loop's WAV writer setup branch becomes a thin lookup: derive the next part index from the parts table, name the output file accordingly. No `is_resume` branch in the WAV writer itself — it just creates a new file.

`session_offset_base_seconds` is computed by the frontend (cumulative SUM of part durations) and passed in `LiveTranscriptionConfig::resume`.

## Backend contract

Replace the current two-field marker with one structured optional:

```rust
pub struct LiveTranscriptionConfig {
    // ... existing fields ...
    pub resume: Option<ResumeConfig>,
}

pub struct ResumeConfig {
    pub part_index: u32,                  // the new part's index = prior_count
    pub offset_base_seconds: f32,         // SUM of prior parts' durations
}
```

Drop `resume_of_session_id` and `resume_existing_audio_seconds` entirely. The session_id is already in the config; the offset comes in explicitly. Impossible to construct a self-inconsistent payload.

The Rust audio crate after this PR:
- **Keeps**: `SessionWavWriter::new`, `SessionWavWriter::write_samples`, `SessionWavWriter::finalize_as_mp3`, `SessionWavWriter::finalize_wav_only`, `convert_wav_to_mp3`, `validate_mp3_bitrate`.
- **Removes**: `SessionWavWriter::open_for_append`, `decode_mp3_to_wav`, `prepare_wav_for_append`, `mono_16bit_spec` helper, all their tests, the `minimp3` dependency.

## `session-wav-ready` event becomes `session-part-ready`

Emit one part-ready event per recording run with: `{ session_id, part_index, file_path, format, duration_seconds, sample_rate }`. The frontend handler inserts a parts row and updates the session's `duration_seconds = SUM(parts.duration_seconds)`.

This replaces the current `session-wav-ready` + `updateSessionWavPath` flow. The session row's `duration_seconds` becomes derived state, kept in sync at every part insert.

## Frontend flow

### `appStore.resumeSession(sessionId)`

1. Sync guards (engine ready, capture active, no other live session).
2. `getSession(sessionId)` — verify completed and at least one part exists.
3. `listSessionAudioParts(sessionId)` — for `nextPartIndex` and `offset_base_seconds = SUM(durations)`.
4. `Promise.all([listFolders(), listTags(), getSessionSegments(sessionId)])` — for vocab hints + segment hydration.
5. Detach AudioPlayer's media element (`audio.src = ""` then unmount via state) so the frontend isn't holding any audio file open before backend setup. **Belt and suspenders** — the parts model means we don't write to any prior file, but cleaning up the playback handle is still right because we're transitioning from "viewing" to "recording".
6. Call `commands.startLiveTranscription({ ..., resume: { part_index: N, offset_base_seconds: O } })`.
7. **On error**: surface toast, return. No DB rollback needed because we never flipped status to `'recording'` first.
8. **On success**: `markSessionRecording(sessionId)` (DB) + `set({ activeSessionId, activeSessionSegments: existingSegments, activeSessionStartTime, ... })` in one batch.

The `markSessionRecording` call moves *after* backend success, so a failed startup leaves the row at `'completed'` with all parts intact. No corruption window.

### `completeSession` no longer uses wall-clock

Frontend `completeSession(sessionId)` now does:

```sql
UPDATE sessions
SET status = 'completed',
    duration_seconds = (SELECT COALESCE(SUM(duration_seconds), 0) FROM session_audio_parts WHERE session_id = $1),
    total_segments = (SELECT COUNT(*) FROM segments WHERE session_id = $1 AND deleted_at IS NULL),
    updated_at = datetime('now')
WHERE id = $1
```

The wall-clock `Date.now() - activeSessionStartTime` math goes away. Duration is always the truth on disk.

## Parts-aware AudioPlayer

State machine in `AudioPlayer`:

- Props: `parts: Array<{ src: string; duration: number }>` (replaces `src: string; duration: number`).
- Internal state: `partIndex: number`, plus the existing `isPlaying`, `currentTime` (now scoped to the active part).
- `globalCurrentTime = SUM(parts[0..partIndex-1].duration) + currentTime`.
- `globalDuration = SUM(parts.duration)`.
- On `<audio>`'s `ended`: if `partIndex < parts.length - 1`, increment, set `<audio>` src to next part, `play()`. Otherwise stop.
- On seek (slider value `t`): map `t → (newPartIndex, partTime)` via cumulative durations. If `newPartIndex !== partIndex`, swap `<audio>` src and seek inside the new audio on `loadedmetadata`. Otherwise just set `<audio>.currentTime = partTime`.
- The slider, time displays, and play/pause continue to operate on global time. The `<audio>` element is the single playback engine, swapping its `src` between parts.

Cross-part seek has a small load gap (network + metadata fetch on `audio-stream://`). Imperceptible for a handful of parts; if it ever becomes noticeable for many-part sessions, MediaSource API is a future optimization.

The Resume button stays as a leftmost control (red Mic icon) on the player toolbar, with the same `canResumeSession` gating.

## Files affected

- `apps/desktop/src-tauri/src/db.rs` — migration v12 (table + backfill + drop legacy columns).
- `apps/desktop/src-tauri/src/commands/live_transcription.rs` — replace `is_resume` WAV-writer branch with part-index-aware file naming; new `ResumeConfig` shape; new `session-part-ready` event payload.
- `apps/desktop/src-tauri/src/commands/capture.rs` — `delete_session_wav` becomes `delete_session_audio` taking a parts list (or session_id with backend resolving from DB).
- `apps/desktop/src-tauri/src/lib.rs` — `audio-stream://` continues to serve individual files; no change needed for the parts-aware player approach.
- `crates/yapstack-audio/src/export.rs` — drop `open_for_append`, `decode_mp3_to_wav`, `prepare_wav_for_append`, `mono_16bit_spec`, all their tests.
- `crates/yapstack-audio/Cargo.toml` + workspace `Cargo.toml` — drop `minimp3` dep.
- `apps/desktop/src/lib/db.ts` — `DbSession` loses `wav_file_path` / `wav_duration_seconds`; add `DbAudioPart` type and CRUD (`listSessionAudioParts`, `insertAudioPart`); rewrite `completeSession` to derive from parts; `canResumeSession` now checks `parts.length > 0`.
- `apps/desktop/src/lib/types.ts` — regenerated specta bindings.
- `apps/desktop/src/stores/appStore.ts` — `resumeSession` reorders status flip to post-success, computes offset base from parts, hydrates segments; `live-transcription-status` → handles new `session-part-ready` event; `createAndStartSession` writes the first part on completion (probably via the backend event handler).
- `apps/desktop/src/components/AudioPlayer.tsx` — parts-aware playback state machine.
- `apps/desktop/src/components/NoteDetailView.tsx` — passes `parts` array to `AudioPlayer` (computed from `session_audio_parts` table); same Resume button gating.
- `apps/desktop/src/components/SessionHeader.tsx` — Resume dropdown item unchanged in behavior.
- `apps/desktop/src/hooks/useLiveTranscriptionEvents.ts` — handles new event name + payload.

## What disappears (net code reduction)

- `minimp3` workspace + crate dep + 5 transitive deps (`minimp3-sys`, `cc`, etc.).
- `decode_mp3_to_wav` and its tests.
- `prepare_wav_for_append` and its tests.
- `SessionWavWriter::open_for_append` and its tests.
- `mono_16bit_spec`.
- The `is_resume` branch in `start_live_transcription`'s WAV-writer setup.
- `resume_of_session_id` and `resume_existing_audio_seconds` config fields.
- `markSessionRecording` rollback path (no rollback needed when status flip is post-success).
- `wav_file_path` and `wav_duration_seconds` columns on `sessions`.

The architecture is genuinely smaller, not just shifted.

## Edge cases

- **Audio location change between parts**: each part stores its absolute `file_path`. A user who changes `audio_save_location` doesn't break old parts — they live where they were written.
- **Session deletion**: FK cascade removes parts rows; backend deletes each part file by iterating the parts list.
- **Crashed mid-recording**: existing crash-recovery sweep in `db.rs` flips stale `'recording'` rows to `'completed'`. The half-written WAV for the current part is orphaned; a follow-up sweep can match orphan files to sessions and either insert as a part or delete. Deferred to a follow-up issue if needed.
- **Format change between resumes**: each part records in the format current at *its* recording time. AudioPlayer plays each part with its own `<audio>` element, so format is per-part transparent.
- **Sample-rate change between captures**: each part stores its own sample rate. Player doesn't care; plays each as-is.
- **Resume after a session with no parts** (edge of edges): backfill handles it for legacy data; new sessions always have at least one part after first stop. Resume guard requires `parts.length > 0`.
- **All P1 issues from the prior code review evaporate by construction**:
  - "ignores persisted audio path" → backend never reads prior files.
  - "duration only the resumed run" → derived from parts SUM, never wall-clock.
  - "mutates/deletes only artifact mid-startup" → no destructive ops anywhere on resume.

## Test plan

**Rust audio crate** (unchanged tests minus the deleted ones — `SessionWavWriter::new` + `convert_wav_to_mp3` keep their existing coverage).

**Rust desktop tests**:
- New: part-index naming honored — start session, stop, assert file at `{session_id}.0.{ext}`. Resume, stop, assert file at `{session_id}.1.{ext}`.
- New: `session-part-ready` emitted with correct fields per stop.
- New: resume rejected when another live session is running (existing single-occupancy gate covers it).

**Frontend tests**:
- New: `canResumeSession` returns false for a session with zero parts.
- New: `appStore.resumeSession` does not mark recording on backend error.
- New: `appStore.resumeSession` parallel fetches (folders + tags + segments + parts) — verify at most one round trip per resource.
- New: `AudioPlayer` parts-aware state machine — global time mapping across parts, seek across parts, `ended` advances to next part.
- Existing: `useDictation` Escape-cancel tests, all existing component tests pass unchanged.

**Manual verification**:
- Record 30 s in MP3 mode → stop. Verify single part on disk.
- Resume, speak 10 s, stop. Verify two parts. Player plays seamlessly across them.
- Resume again, speak 5 s, stop. Three parts. Seek to 35 s in player — lands in part 1. Seek to 42 s — lands in part 2.
- Change export format to WAV mid-session (between resumes). Verify mixed-format parts play correctly.
- Delete session — all part files removed.
- Migrate from a pre-PR DB containing a single MP3 session — backfill creates part 0, resume produces part 1, no decoding occurred.

## Phasing

**One PR.** This branch (`feat/session-resume-parts`) builds the parts model on top of checkpoint `2862f4a`. The previous design's destructive primitives (decode, append, prepare) get deleted; what's kept and reworked:
- `canResumeSession` predicate (gated on parts.length > 0 instead of wav_file_path).
- `appStore.resumeSession` action (reordered status flip, parts-derived offset, parallel fetches).
- AudioPlayer Resume button (props change to `onResume` + `parts`).
- SessionHeader dropdown item (unchanged in behavior).

At the end the branch's net diff vs `main`'s pre-resume state is: parts table + parts-aware player + new resume command shape. Cleaner than the checkpoint by every measure: lines of code, dependencies, code paths, failure modes.

## Verification commands

- `cargo test -p yapstack-audio` — must still pass without the deleted modules.
- `cargo test -p yapstack-desktop --lib` — new resume + parts tests.
- `pnpm typecheck && pnpm --filter @yapstack/desktop lint`.
- `pnpm test:frontend`.
- `cargo fmt --check` and `git diff --check` (the prior review flagged trailing whitespace + unformatted Rust).
- Manual flow above on real microphone capture.

## Out of scope for this PR

- Pause-during-recording (different UX, distinct state).
- Single-file audio export action (stitches parts into one file). Future feature; the parts model makes it trivial when wanted.
- MediaSource-API gapless playback. Premature; sequential `<audio>` swap is fine for ≤ a handful of parts.
- Per-part editing/deletion (delete part 1, keep parts 0 and 2). Future.
- Cloud sync of parts across machines.
- Orphan-file sweep on app start (a partially-written WAV from a crashed recording). Punt to a follow-up if it becomes a real problem.
