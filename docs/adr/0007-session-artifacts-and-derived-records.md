# 0007. Session artifacts, background jobs and derived records

- **Status**: Accepted
- **Date**: 2026-11-01
- **Depends on**: ADR 0006 (synced-schema evolution), which every synced table here uses.

## Context

Almost every Q4 feature after transcription is post-recording and derived: the speaker refine pass, structured summaries, daily recaps and Obsidian export.

1. **The Mixed session file is a mono mix** (`crates/yapstack-audio/src/mixer.rs`), and live ASR is already per-source. Dictation muting erases system audio too (`commands/live_transcription.rs`), and a resample failure silently keeps the mic only (`manager.rs`). Parts don't record which source they hold; resume reads the current capture setting (`stores/appStore.ts`).
2. **There is no durable post-processing runner.** The transcription scheduler is in-memory and non-preemptive. Each plan proposed its own loop.
3. **cr-sqlite merges per column, and remote writes arrive through `crsql_changes` without touching app code.** A generated document spread across columns can merge incoherently, and a "previous value" archive written by app code never sees the concurrent loser. Profile ids are device-local and must not sync.

## Decision

### 1. Session audio artifact
- Every part records its **source identity**: `session_audio_parts.channel_layout TEXT NULL` ∈ `mic | system | mic_system`. `NULL` means legacy or unknown.
  - It's a single ALTER, registered in `OUT_OF_BAND_ALTERS`, with no CHECK constraint, because older clients keep inserting parts with `NULL`. Existing rows are not backfilled, so no change is emitted for every part on every device.
  - The value comes from the capture mode of that recording run, not from the current setting.
  - Crash reconciliation writes `mic_system` for a 2-channel file, and `NULL` otherwise.
- Mixed capture records **one stereo part per run: L = mic, R = system**, at unity gain with no limiter. It's encoded as MP3 (96 kbps default for stereo) or WAV per the user setting. MicOnly and SystemOnly stay mono.
- Dictation muting applies to the mic channel only. A resample failure is an error.
- **Every playback or audio-export path is a downmix point**: the player now, and any future export or share path. Nothing downstream may assume a part is mono.
- One part still maps to one audio blob (SYNC_SPEC §4.6).
- Post-hoc passes read audio only through `prepare_session_audio(session, Mix | Mic | System)`. It decodes with `symphonia`, concatenates parts in `part_index` order and writes a 16 kHz mono temp WAV, so sidecar IPC is unchanged. Asking for a channel a part doesn't have (including a `NULL` part) is the typed error `ChannelNotRecorded`.
- Retention, when built, deletes audio files and blobs, never part rows. It must not run while a session has an active job.

### 2. Session jobs
- A **local-only** `session_jobs` table, never a CRR. States: `queued | running | done | failed | cancelled`.
  - One active job per `(kind, subject_id)`.
  - A lease handles crash recovery (`running → queued` on start, at most 3 attempts).
  - `last_error` is surfaced verbatim, with no fallback.
- **Recording runs** (local-only, never CRRs, same family and conventions as `session_jobs`). `recording_runs(id, session_id, offset_base_seconds, state capturing | finalized, refine_done_at)` and `recording_run_members(run_id, kind part | segment, member_id)`.
  - A run row is written **before capture starts**, on start and on every resume.
  - Membership is written as capture proceeds. The part member goes in with the part row (`live_transcription.rs`). The segment member is written *before* its segment through `segmentQueueTail`, so a crash leaves at most a member pointing at a segment that never landed, and refine skips it. Segments with a `NULL` key are members too.
  - `reconcile_audio_parts` (`db.rs`) recovers part rows but not ownership. Whenever it recovers a part file, it adds it to that session's latest local `capturing` run. The file is local, so it can only be this device's.
  - At boot, `capturing` runs whose session isn't live on this device are marked `finalized`. Every `finalized` run with no `refine_done_at` is enqueued idempotently, deduped by F3's one active job per `(kind, subject_id = run id)`.
  - **Why not `session_jobs` itself:** a job row exists only once work can run, but a run must exist before capture and gather members while capture is still going. Folding runs in would add a non-job "capturing" state to the runner and a per-segment rewrite of a job row. The job stays the unit of work; the run is its durable input.
- The runner is hosted in the renderer (AI keys are renderer-side, ADR 0003). Per-feature schedulers become producers.
- Heavy compute runs as a one-shot sidecar process at lowered OS priority, cancellable. A lowest-priority scheduler lane for work on the resident model is the agreed future mechanism, but it isn't built until a consumer needs it (re-transcription, import).
- **Staleness is a property of a derived record**: its `input_hash` compared with the current inputs. It is not a job state. Input hashes cover content inputs, plus model identity for audio jobs. They exclude the AI profile and model for AI jobs.
- Auto-run device:
  - audio jobs run on the recording device;
  - recaps run only on the device where "Generate recaps on this device" is on (device-local);
  - export runs on the device with the vault.
  - Manual runs work anywhere (audio is fetched first), **except speaker refine**. Refine is part of finishing a recording run on the device that recorded it (§3), so it has no cross-device run and no wait for audio.

### 3. Derived records
- **Document tables** (`session_summaries`, `recaps`):
  - a deterministic slot id:
    - `summary:<session>:<template family>`, e.g. `builtin:summarize`; the template version goes in provenance;
    - `recap:day:<local date>` and `recap:week:<gggg-Www>`; the timezone and window bounds go inside the envelope, not in the id or a column;
  - exactly two content cells: `generated_json` (with the provenance envelope inside) and `override_json` (the user's edit);
  - an append-only versions table (`session_summary_versions(summary_id)`, `recap_versions(recap_id)`, following `note_versions(note_id)`).
    - **Every local write of either cell first inserts the new payload into it, then updates the slot.** A versions row has a unique id, `cell` (`generated | override`), `payload`, `payload_hash`, `generated_on` and an explicitly written `created_at` (never the column default).
    - **Write order, three plain statements:** (1) create the slot if it's missing, with both cells `NULL` (`INSERT … ON CONFLICT(id) DO NOTHING`); (2) insert the versions row; (3) update the slot cell. Step 1 exists because the versions table follows `note_versions` and keeps its parent FK on non-CRR DBs (`apps/desktop/src-tauri/src/db.rs`, with `foreign_keys=ON`, `db_service.rs`), so a first-generation versions insert with no slot would fail. Crash boundaries: after (1) there's an empty slot, which renders as "no record" and the job retries; after (2) there's one extra history row, which is harmless. There's no transactional command, and no SQL trigger, since triggers also fire on merged remote changes.
    - When two devices write concurrently, LWW keeps one cell value, and the other device's payload survives in its own synced versions row.
    - **Every published payload is archived; nothing is coalesced at write time.** Generations publish once. A user edit session keeps its draft in the editor and **publishes** `override_json` (versions row first) on blur, close or navigation, and at most every 60 s while it's dirty. A crash loses at most 60 s of edits. Autosave never writes the slot, so no payload ever reaches the relay without its versions row. A payload whose hash matches the slot's latest version is skipped (nothing is published).
    - **Retention:** the last 20 versions per slot, plus every generation row within 30 days of the slot's newest version. The cutoff is measured from the data, not the wall clock. Ranking is `(created_at DESC, id ASC)`; `id` is the immutable PK. On a CRR DB, only rows whose `{slot fk, cell, created_at}` columns are **proven merged** take part, as rankers or as candidates (the `dedup_notes` clock-join gate, `crates/yapstack-sync/src/uniqueness.rs`). The prune also runs only under the same clean-drained-pull condition as `cascade_gc` (`sync.rs`). A receiver sitting at the relay tip with a half-captured row (capture commits per chunk, `outbox.rs`) therefore ranks a subset of the rows a complete device ranks, so it deletes a subset of the same ids, and the deletes converge. On a non-CRR DB every row is complete, so no proof is needed.
    - **Versions rows are immutable, with exactly one exception: the delete purge (§Cascade), which rewrites `generated` payloads only.** Purge writes never append versions rows.
  - Render = `override_json ?? generated_json`. Regeneration never writes `override_json`, and replacing a user edit is an explicit user action. No `superseded_by` chains.
- **Entity tables** (`session_speakers`, `summary_templates`) use per-column LWW. **Each column has exactly one owner, user or system.**
  - Speaker id = `<session_id>:<speaker_key>`, with `speaker_key ∈ mic | system | <source>:<epoch_id>:<local_id> | <source>:<run_id>:<k>`. **Keys are minted without coordination**, so two devices never mint the same one, and a key that refine empties is never reused.
    - `epoch_id` is random (8 base32 chars), minted by the host for each diarizer instance. A new one is minted whenever state is lost: sidecar respawn, session resume (on any device, `appStore.ts`), or a new stream. `local_id` is the diarizer's own id within that instance. Refine mints a random `run_id` for clusters it can't map onto an existing key.
    - The key lives in a **new system-owned column `segments.speaker_key TEXT NULL`** (ALTER + `OUT_OF_BAND_ALTERS`, under ADR 0006). The legacy `segments.speaker_id INTEGER` (runtime ALTER, `lib/db.ts`) is left in place and no longer written. Its values are chunk-local leftovers, and every existing reader and older client treats it as a number, so storing text keys in it would mix two meanings in one column.
    - The "Speaker N" a user sees is a **render-time ordinal**: per source, in order of first appearance among the resolved keys. It's never stored.
    - **Live feed:** each source's continuous capture audio is fed to its instance in capture order (owner ruling, 2026-11-01: continuous, not VAD-trimmed), independent of the ASR scheduler. A segment gets its label from the turns of the current epoch that overlap its time range. So FinalFlush running ahead of queued live chunks (`commands/transcription_scheduler.rs`) and backfill don't affect what's fed. A segment that falls before the current epoch (its state was lost) stays `NULL` until refine.
    - **Delayed labels:** a segment is written first with `speaker_key NULL`. Its live label is applied later through `segmentQueueTail` (p95 lag about 10 s), and only while `speaker_key` is still `NULL`. Live labels are shown as provisional until refine has run.
    - **Refine scope is one recording run** (owner ruling, 2026-11-01). At each run's stop, the device that recorded it refines that run. **It selects parts and segments by the run's durable membership (§2, recording runs), never by time offsets.** Two devices can resume the same completed session before either one sees the other's state change (`appStore.ts`). Each takes its offset base from the parts it knows about (`:1862-1866`), so the time ranges of two runs can overlap. Diarizer turns map onto member segments relative to the run's own `offset_base_seconds`. A resumed session gets one refine per run, each on the device that recorded that run. No device ever refines another device's audio, and nothing waits for audio blobs to sync. The owner ruling now holds by construction: only the recording device has the run row. The job's subject is the run id. Parts carry no device attribution (`db.rs`), and the local run tables supply it without a new synced column.
      - *Per run, not per device.* Refining all of one device's parts together would link speakers across same-device resumes. But it needs per-part device attribution, which is a new synced column or local bookkeeping, and that same-device case is the one the owner accepted across devices. One rule covers both.
      - Speakers aren't linked across runs: one person can show as two speakers until the user gives both keys the same name.
    - **Refine** (after that run's stop) reconciles each refined cluster onto the existing key with the most co-assigned duration (≥ 0.6 overlap) and labels `NULL` segments. **When two existing keys turn out to be one person, it doesn't move segments.** It sets the system-owned `session_speakers.merged_into = <target key>` on the absorbed key. It rewrites a segment's `speaker_key` only to fill a `NULL` or correct a single-segment assignment. It never merges two keys that both have a user `display_name`, and never writes override columns.
    - The input hash covers the run's part ids, its segment ids and the `session_speakers` rows for the run's keys. It's re-checked at write time, as F3 jobs do, and a stale result is discarded. Run memberships are disjoint (each segment and part is written by exactly one device's run), and each run has a single writer, so two devices never write refine results for the same segments. Refine only sets `merged_into` on keys minted in its own run, so no alias crosses runs or devices. A re-run of the same run (Retry) first clears `merged_into` on its run's keys, then writes. A crash between those two steps can leave a same-device cycle, and the walk below handles it.
    - **Resolver:** effective key = `speaker_key_override ?? speaker_key ?? source`. Then **walk `merged_into`** from that key, and stop at the first key that:
      - has its own `display_name` (user intent wins over a concurrent merge);
      - has no `merged_into`; or
      - has a missing `session_speakers` row (partial arrival). Stop at the last present key.

      On revisiting a key (a cycle), stop at the lexicographically smallest key in the cycle. The label, the render ordinal and `is_self` all come from that terminal key.
    - **Identical display names render as one person** (trimmed, case-insensitive), in the speaker list, AI context and exports. That's how the user joins speakers across runs or devices. Nothing is rewritten.
  - Segment speaker identity is split by owner: `segments.speaker_key` is system-owned (written by live labelling and refine), and `segments.speaker_key_override` is user-owned (written only by user actions).
  - "This is me" is split the same way: `session_speakers.is_self_auto` (job) and `is_self_override` (user).
  - The label resolver prefers the override in both cases, and every read that labels segments returns both columns.
  - Built-in templates live in code; only user templates and forks are rows.
- **Provenance envelope** (inside `generated_json`):

  ```
  { schema, producer{kind ai|system, model, profile_label, connection_kind, prompt_version, template_version, engine},
    input_hash, generated_at, generated_on, quality{status complete|partial, …}, window?, timezone?, content }
  ```

  - Never a `profile_id`, never `failed`. Failure is local job state.
  - `engine` records the **resolved** accelerator.
- **Points** carry:
  - a citation-anchored id `p_` + base32(sha256(subject, kind, sorted segment_ids))[:10], with a deterministic ordinal suffix (`_2`, `_3` in content order) when two points in one record collide;
  - `kind ∈ point | decision | action_item | question`;
  - `segment_ids[]` as canonical UUIDs, plus the source `session_id` (the envelope also lists `source_session_ids`), so a purge still works after the segments are tombstoned;
  - for action items, `owner {speaker_key, label}` and a grounded `due`.
  - Completion state is not part of a derived record. Exporters that merge checkbox state keep checked lines whose point disappeared.
- **Cascade:**
  - Session children are registered in `CASCADE_DELETE`, and versions cascade with their slot.
  - `CASCADE_DELETE` entries for versions tables come after their parent's entry, as `note_versions` follows `notes`.
  - Recaps aren't session children, so the session's own cascade never reaches them. **One shared purge, `purge_deleted_sources(conn)`** (in `crates/yapstack-sync/src/cascade.rs`), removes deleted-session content from document records. It's a deterministic `json_each` pass keyed on each point's `session_id` and the envelope's `source_session_ids`:
    - in `generated_json` and versions `generated` payloads, points from a deleted session are removed. Narrative fields that drew on it are cleared, and the record is marked stale (`quality.purged = true`);
    - versions rows are rewritten in place, never appended. This is the single exception to versions immutability;
    - **it never writes `override_json` or versions `override` payloads.** Those are user-owned and mutable. A purge write would bump the same column version as a concurrent user edit, and cr-sqlite breaks equal versions by comparing values (`vendor/cr-sqlite/core/rs/core/src/changes_vtab_write.rs`), so the stale, purged text could beat the user's new edit. The user's text is kept, because it's their own writing. Its citations to the deleted session are opaque ids with no text, and they render as plain labels through the F5 resolver (an unresolvable reference renders as text). They're dropped on the user's next real save, the same render-time repair rule as dead note pills;
    - "deleted" means the session's delete sentinel is proven (`proven_deleted_pks`, `cascade.rs`) on a CRR DB, and the session row is absent on a non-CRR DB (nothing arrives late there). Every device computes the same output, so the LWW writes converge. On a CRR DB it touches only rows whose payload column is proven merged, so a half-arrived row is never rewritten from its synthetic default.
  - **Callers, all of them idempotent:**
    - local deletion: `deleteSession` (`apps/desktop/src/lib/db.ts`) runs the purge straight after its `DELETE`, through one Tauri command, `purge_deleted_sources`. This works with sync off; today a local delete only removes the `sessions` row and the runtime cascade runs only after a merge (`sync.rs`);
    - remote deletion: the post-merge maintenance step, beside `cascade_gc`;
    - crash recovery: app boot. The session's delete itself (the row's absence, or its sentinel) is the durable marker, so a crash between the delete and the purge heals on the next launch without a new table;
    - publication: a derived-record write re-checks its input hash before publishing (F3), and calls the purge for its slot afterwards, so a job that read a session deleted mid-run can't republish its text.
  - The stale recap regenerates on the designated device. Regeneration sends the remaining sessions to the Profile's provider, and the UI discloses that.

### 4. Speech-engine boundary (with the planned ADR 0005, Parakeet-only)
- `EngineKind` collapses to Parakeet, but the sidecar keeps the narrow `TranscriptionBackend` trait. Unified EN is the second implementation.
- **Diarization is live per source, plus a refine pass after stop** (owner ruling, 2026-10-31). `Transcribe.diarization: bool` (`crates/yapstack-common/src/types.rs`) is **kept** through the Whisper removal. The speaker IPC change replaces it with an optional stream key `{ session_id, source, epoch_id }` (the continuous-feed shape is settled in that PR), which is ask-first and goes to the owner once the spike passes. The sidecar holds one stateful Sortformer per stream key, fed through `feed`, and drops it at session end or when a new epoch starts. Dictation never uses it.
- Live and refine share one sidecar diarizer wrapper (`StreamDiarizer`: `feed`, `flush`, offset mapping from fed audio back to chunk time). Refine runs it through the one-shot `diarize` subcommand, once per channel over the run's parts (§3), using `prepare_session_audio(Mic | System)`, or `Mix` for `NULL` or mono parts. Both paths go through the same mapping module (§3, entity tables).
- **Live runs on both sources** (owner ruling, 2026-11-01, after the spike measured +1.59 GB RSS for two v2.1 instances). The 400 MB target is follow-up work (SP-MEM) and doesn't block live labels. Any fp16 lever needs a CPU-RSS check, because fp16 on CPU has been measured at 4.2 GB.
- The engine catalogue `variant_id` plus the model sha is the model identity in job input hashes and provenance.
- parakeet-rs moves to 0.3.8 + ort rc.13 together with the Sortformer v3 model in one change. 0.4.x waits on a silero replacement.
- Deferred until re-transcription is planned: `sessions.transcript_engine`, and the rules that re-transcription is user-initiated only, previewed, never replaces edited, hidden or deleted segments, and soft-deletes replaced ones.

### First adopters (each lands in its own PR)
- `session_audio_parts.channel_layout` (ALTER)
- `session_jobs` (local)
- `session_speakers` (entity, with system-owned `merged_into`), `segments.speaker_key_override` and `segments.speaker_key` (ALTERs)
- `session_summaries` + `session_summary_versions` (document)
- `recaps` + `recap_versions` (document, with `period`)
- `summary_templates` (entity)

`segments.speaker_id` stays a runtime ALTER (ADR 0002 reasoning), as a legacy column that's no longer written.

## Consequences

- Derived data never blocks capture or dictation.
- Contributors writing a derived feature implement a job kind (inputs, hash, executor) and choose a record class. They don't write a scheduler, add provenance or status columns, or archive prior values by hand.
- Versions tables are bounded by publish cadence (one row per generation, at most one per 60 s of editing) and by the 20-version / 30-day retention.
- Live speaker labels are provisional. Ids can change at a run's stop, when refine reconciles that run's epochs; user names and overrides survive.
- A person in runs recorded on different devices (or in separate runs on one device) shows as two speakers until the user names both the same.
- Mixed recordings cost about 1.5× disk and relay storage (43 MB/h vs 29 MB/h). Old builds play new stereo parts hard-panned. Historic and crash-recovered mono parts can't be channel-split, so diarization keeps a mono-mix path for them.
- Trade-offs accepted:
  - Local-only job state means two devices can occasionally generate the same record. They converge on the slot, and both payloads are kept in versions.
  - A recap on a device that isn't designated shows "includes a deleted session" until the designated device regenerates it. The purged text is already gone.

## Alternatives considered

- **Two files per part (stems):** rejected. It breaks part-to-blob identity, the upload-queue key and reconciliation.
- **Lossless masters:** rejected. ×12 storage; on non-CRR DBs, FLAC also hits the `format` CHECK.
- **Opus:** rejected for now. A new C dependency, and no symphonia decoder.
- **An explicit `legacy_unknown` value backfilled into existing parts:** rejected. One CRR change per row per device, for no information gain over `NULL`.
- **A Rust-hosted job runner:** rejected. Keys are renderer-side.
- **A synced job table:** rejected. Device-specific facts, cross-device RPC, changeset churn.
- **Per-feature schedulers:** rejected. Three crash stories.
- **`superseded_by` version chains:** rejected. They fork under concurrent generation.
- **Versions archiving the prior value:** rejected. Remote writes bypass app code, so the concurrent loser is lost.
- **Slot + versions in one transaction through a new Tauri command, or via SQL triggers:** rejected. Versions-first ordering is enough; triggers fire on merges.
- **Coalescing autosaves at write time:** rejected. An unflushed autosave reaches the relay without a versions row, so a concurrent winner can erase it.
- **Purging `override_json` in place:** rejected. It competes with concurrent user edits at an equal column version.
- **Purge only inside `cascade_gc`:** rejected. That runs only after a merge, so a device with sync off would keep deleted-session text.
- **Raw diarizer ids as speaker keys:** rejected. They restart on respawn or resume and arrive out of order.
- **A stored ordinal `N = max(speaker_id) + 1`:** rejected. It reuses keys a refine merge emptied, which rebinds old names and overrides to a new person, and two devices allocating after a cross-device resume collide on the PK.
- **Refine on the session's current recording device, or refining another device's audio:** rejected (owner ruling). It needs a takeover rule and a wait for audio blobs to sync.
- **Refine moving segments on a merge:** rejected. It races a concurrent rename of the absorbed key; a system-owned alias doesn't.
- **Text keys stored in `segments.speaker_id`:** rejected. Old readers treat it as an integer, and it holds legacy values.
- **Removing `Transcribe.diarization` with Whisper:** rejected. Live diarization needs the field until the stream key replaces it.
- **Regenerating a recap inside the purge:** rejected. AI output isn't deterministic, so the purge wouldn't converge. Removal plus a stale mark does.
- **`is_self` as one column written by both the job and the user:** rejected. The same two-owner merge hazard as `speaker_assigned_by`.
- **`speaker_assigned_by` next to a job-written `speaker_id`:** rejected. The two columns merge independently.
- **Provenance as columns:** rejected. They interleave under per-column LWW.
- **Timezone in the recap slot id:** rejected. The same local day would fork across devices.
- **Template version in the summary slot id:** rejected. A template bump would orphan user overrides.
- **Seeded built-in templates:** rejected. Every device emits changesets on every update.
- **Text-hash point ids:** rejected. Unstable across rewording.

## References

- `docs/SYNC_SPEC.md` §4, §7; `docs/plans/AUDIO_ROUND_TRIP.md` D1/D6; ADR 0002, 0003, 0006; ADR 0005 (planned).
- `crates/yapstack-sync/src/{cascade,audio}.rs`; `apps/desktop/src-tauri/src/db.rs`; `crates/yapstack-audio/src/{manager,mixer,export}.rs`; `apps/desktop/src-tauri/src/commands/{live_transcription,transcription_scheduler}.rs`.
