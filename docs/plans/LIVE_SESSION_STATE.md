# Live Session State — remote-recording lifecycle across synced devices

Grounded in a scout audit of the working tree (branch `feat/sync-relay-skeleton`) and a
live two-device UAT (2026-07-11). Every file:line below was re-checked against the tree.
Principle (from SYNC_REMEDIATION): build for the production user, model the states sync
actually makes reachable, add zero mechanisms the evidence doesn't force.

## Problem

Sync made a state reachable that no code models: **a session that is `status='recording'`
but recorded on another device.** Two independent gaps and one live hazard:

- **Gap 1 — the view has no remote-live concept.** `NoteDetailView` decides layout from
  `isActiveSession = selectedSessionId === activeSessionId`
  (`apps/desktop/src/components/NoteDetailView.tsx:57`), which is *strictly the local live
  recording*. `isEditable = isActiveSession || status==='completed'`
  (`NoteDetailView.tsx:159`). A synced `'recording'` row from another device is neither
  active (`:198`) nor completed (`:238`), so it falls to the fallback `ChatView`
  (`:284-289`), whose empty branch prints **"Start speaking to begin transcription"**
  (`apps/desktop/src/components/ChatView.tsx:503-511`). The recording is live on A; B shows
  the empty-local-record prompt.

- **Gap 2 — the open view doesn't live-update.** `startSyncAppliedRefresh` reloads
  sessions / folders / tags on `sync://applied`, but **not the open session's segments or
  row** (`apps/desktop/src/stores/appStore.ts:1147-1169`, coarse-by-design). Even once Gap 1
  is fixed, B's open transcript stays empty as segments merge until a manual reopen.

- **P0 HAZARD — the boot sweep is ownership-blind and destructive.**
  `close_orphaned_recordings` (`apps/desktop/src-tauri/src/db.rs:98-164`) **DELETEs** empty
  `'recording'` sessions and **finalizes** the rest, for *any* local row, keyed only on
  `status='recording'`. It runs at every boot via `ensure_runtime_schema`
  (`db.rs:61`, defined `db.rs:27`; wired at `apps/desktop/src-tauri/src/lib.rs:783`). Its
  design comment asserts *"At startup the app cannot have a real in-flight recording
  session"* (`db.rs:93-97`) — **sync falsified that invariant.** Launching B while A records
  finalizes or DELETEs A's live row, and because the sweep runs on a cr-sqlite connection the
  mutation syncs back and destroys the recording on A. This is live data-corruption today.

Legitimate origin of the empty-remote-live row: `createSession` INSERTs only `(id, source)`
and relies on the schema default `status='recording'`
(`apps/desktop/src/lib/db.ts:467-476`; default at `db.rs:457`). The store calls it in
`createAndStartSession` (`appStore.ts:1273`) *before* `startLiveTranscription`
(`appStore.ts:1312`) and before any segment — so B can validly observe
`{status='recording', 0 segments}` for a few seconds.

## Decisions

| # | Decision | Rationale / evidence |
|---|---|---|
| D1 | **Ownership-gate the boot sweep.** Interim (ships first, no schema dep): the sweep **NEVER DELETEs**; it **finalizes nothing on a CRR-prepared DB** and finalizes only stale rows on a **never-CRR-prepared** DB (pure DB-level check — see sweep rules). This *narrows but does NOT close* cross-device finalization for rows stale-at-receiver during a sync gap — the actual P0 fix is the column + final rule (D2, sequencing step 3). Final (post-column): sweep skips *foreign* rows entirely, deletes only owned-or-null-and-sync-off empties, finalizes the rest. | P0 corruption is live: the finalize UPDATE at `db.rs:126-136` sets `status='completed'` on a CRR connection (the sweep runs via `open_managed` at `db.rs:61`), so it LWW-propagates the `'completed'` write back onto A mid-recording. An interim that still finalized stale rows on a CRR-prepared DB would *re-open the same path* (B boots, its heartbeat check sees A's row stale-at-receiver via synced `max(segments.created_at)`, finalizes → LWW syncs back) and would contradict D6 (owner-only finalization). So the honest interim finalizes only on a DB that was never CRR-prepared. Migration precedes the *final* sweep at boot (`lib.rs:729` `DbService::open`→`run_migrations` before `lib.rs:783` `ensure_runtime_schema`), so it can read the new column. |
| D2 | **Add synced column `sessions.recording_device_id TEXT NULL`** = the recorder's `device_fingerprint`. Written at `createSession` and `markSessionRecording`. NULL = legacy/pre-attribution **or** sync-not-configured. | `device_fingerprint` is already frontend-exposed (`apps/desktop/src/lib/sync.ts:117`) and roster-labelable (`sync.ts:101-109`), so "Live on \<label\>" needs no new plumbing. NULL degrades to exactly today's single-device semantics (see D1 final rule). |
| D3 | **Remote-live view branch** in `NoteDetailView` when `status==='recording' && recording_device_id && recording_device_id !== myFingerprint`: read-only follow-along, live badge, synced segments render as they arrive, **all** write affordances hidden. | Closes Gap 1. `isActiveSession` (`NoteDetailView.tsx:57`) can't express "recorded elsewhere"; a dedicated branch is the minimal fix. |
| D4 | **Close Gap 2** by extending `startSyncAppliedRefresh`: on each debounced applied batch, if an open session exists that is **not** the local active one, also reload its `viewSession` row + `viewSessionSegments`. The reload **must not clobber an in-progress local edit** — see the normative *Edit-in-progress* requirement below. | `appStore.ts:1147-1169` already debounces; the incremental cost is one session row + its segments, only while a note is open. Cheaper than fine-grained per-row invalidation the coarse refresh deliberately omits. |
| D5 | **Liveness = fresh heartbeat**, heartbeat = `max(segments.created_at)` (fallback `session.created_at` at 0 segments), threshold **3 min**. Stale → "Interrupted on \<label\>", still read-only. | `insertSegment` touches only `segments` (`db.ts:803-820`); `sessions.updated_at` is boundary-only (`completeSession` `db.ts:496-516`, `markSessionRecording` `db.ts:523-532`, title). So the segment stream *is* the heartbeat — zero extra sync writes (see D8). |
| D6 | **Finalization authority is owner-only in v1.** A non-owner never writes `status`. The owner's boot sweep (now ownership-gated) finalizes its own crashed sessions. | `sync://applied` is a merge, not a command; letting B finalize A's row is a second writer. Accepted consequence: a permanently-dead device leaves a stuck "interrupted" row — recoverable via the manual "mark completed" escape hatch (resolved Q1). |
| D7 | **`myFingerprint` sourced from `SyncStatus.deviceFingerprint`** (`sync.ts:117`); NULL before enrollment / sync-off. When NULL, every comparison in D3 is false → no remote-live branch → single-device behavior unchanged. | Column stays NULL when sync unconfigured; the frontend already holds the fingerprint. |
| D8 | **No per-append heartbeat column.** Arriving segments are the heartbeat. | Adding a `last_heartbeat` write per segment would be one extra CRR write per append — pure sync overhead for information the segment rows already carry. |

## Schema & sync-evolution

`recording_device_id` is added the R11-proven way for a synced column:

1. **New migration** in `migrations()` (`apps/desktop/src-tauri/src/db.rs:445`), a single
   `ALTER TABLE sessions ADD COLUMN recording_device_id TEXT`. `run_migrations`
   (`apps/desktop/src-tauri/src/db_service.rs:500-501`) auto-wraps a lone sync-table `ALTER`
   through `crsql_alter` on a CRR DB, backfilling `col_version=1`; on a non-CRR DB (fresh
   install pre-cutover, or `no-sync` build) it applies plainly (`db_service.rs:490-499`).
2. **Register in `OUT_OF_BAND_ALTERS`** (`crates/yapstack-sync/src/schema.rs:314-322`) so a
   device that CRRified at the base schema version picks the column up via
   `apply_out_of_band_alters` (`schema.rs:327-342`) at cutover
   (`apps/desktop/src-tauri/src/sync.rs:1570`) and at boot self-heal (`sync.rs:2822`).
   `apply_out_of_band_alters` is idempotent (skips existing columns / non-CRR tables).

Receivers ahead of schema quarantine the unknown-column changeset and replay it after the
column lands (`crates/yapstack-sync/src/outbox.rs:490`, `crates/yapstack-sync/src/quarantine.rs`)
— automatic, no bespoke handling.

**Writers.** `createSession` (`db.ts:467`) gains a `recordingDeviceId: string | null` arg,
passed from `createAndStartSession` (`appStore.ts:1273`) reading
`get().syncStatus.deviceFingerprint` (null when sync off). `markSessionRecording`
(`db.ts:523`) likewise stamps the resuming device's fingerprint on the resume transition, so
a resumed session re-attributes to whoever resumed it.

## Lifecycle state machine

`me` = the viewing device's fingerprint. `owner` = `recording_device_id`.
`fresh` = `now − heartbeat < 3 min`. Rendered state of the **open** session:

| status | owner | heartbeat | isActive | Rendered on this device | Writes |
|---|---|---|---|---|---|
| recording | — | — | **true** | Local live recording (existing active branch, `NoteDetailView.tsx:198`) | full |
| recording | other | fresh | **true** | **Resume-race loser** — this device *is* locally recording (`isActiveSession` true) but LWW kept a **foreign** `recording_device_id`. Renders as the local live branch; the foreign owner is the reconciliation hazard, not the render. | full (local); LWW may reassign `owner` |
| recording | `me` | any | false | **Interrupted (own, crashed)** — read-only; sweep will finalize at next boot | none (owner-finalize only) |
| recording | other | fresh | false | **● Live on \<label\>** — read-only follow-along, segments stream in | none |
| recording | other | stale | false | **Interrupted on \<label\>** — read-only. Also the **same-hardware re-pair** state: after a credential clear / fresh install this device's own old rows are now foreign-to-self and land here; reclaimable only via the explicit "mark completed" action (Q1), never automatically. | none |
| recording | NULL | any | false | **Interrupted (legacy)** — read-only; finalize-eligible, never a false "live" | none |
| completed | — | — | false | Completed transcription (`:238`); Resume per `canResumeSession` (`db.ts:48-66`) | edit / resume |

Transitions:

- **Start (A):** A `createSession` → `{recording, owner=A, 0 seg}` syncs to B. B renders
  "● Live on A" immediately (no more "Start speaking"). Segments append → B's transcript
  fills live via D4.
- **Crash (A):** A dies mid-recording. Row stays `{recording, owner=A}`. B: heartbeat goes
  stale after 3 min → "Interrupted on A". A reboots → sweep sees an *owned* `'recording'`
  row → finalizes to `completed` → syncs → both show completed.
- **Stall vs crash:** a long pause without new segments looks identical to a crash after
  3 min. Acceptable: both render read-only "interrupted"; a later segment (recorder resumes)
  flips B back to "live" on the next applied batch. No write is ever wrongly enabled.
- **Resume (A):** completed → `markSessionRecording` sets `{recording, owner=A}` → B shows
  "● Live on A" again.
- **Resume race (both devices resume one completed session):** `resumeSession`
  (`appStore.ts:1352`) guards only *local* state — `liveTranscriptionActive || sessionStopping
  || activeSessionId` (`appStore.ts:1362`) — and its status check (`appStore.ts:1380`) sees
  `'completed'` on **both** racers during the sync gap, so two devices can each resume the same
  session; LWW then keeps one `recording_device_id`. **Guard:** the existing
  `status !== 'completed'` refusal (`appStore.ts:1380`) is *already* the operative guard and
  is strictly stronger — once A's `recording` write has synced, B's resume is refused
  regardless of owner or freshness. The specced **fresh-foreign-owner** check (`owner &&
  owner !== me && fresh`) adds nothing to the race window and is retained as
  **defense-in-depth only** (belt-and-suspenders, and it yields a truthful "live on
  \<label\>" message instead of a generic refusal). **If the race still lands** (both passed
  the status check while both saw `completed`):
  the LWW loser becomes the state-machine row `{recording, owner=other, isActive=true}`.
  Accepted blast radius, stated explicitly: **interleaved segments in one session** (no crash,
  repairable by edit/delete) **and interleaved audio parts** — both racers compute the same
  `offsetBaseSeconds` from the pre-race parts (`appStore.ts:1401`) and write **colliding
  `part_index` values** (`appStore.ts:1429`), and `completeSession` SUMs *both* racers' part
  durations (`db.ts:504-510`), so the session ends with an **inflated duration and overlapping
  playback**; audio parts have **no user delete affordance**, so the part-level overlap is not
  user-repairable in v1. Still **no crash and no data loss**, no stuck lifecycle. v1 does not
  attempt to split or arbitrate the two segment/part streams (Non-goal: multi-writer).
- **Same-hardware re-pair (`fresh install` / credential clear):** the device mints a new
  keypair, so `device_fingerprint = SHA-256(ed25519_pub)` base32 (`sync.rs:3181`; fresh-install
  mint noted `sync.rs:150-151`) changes. Its own prior `'recording'` rows now satisfy
  `foreign` (owner is the *old* self-fingerprint) and, being stale, render "Interrupted on
  \<label\>" **permanently** under owner-only finalization (D6) — the boot sweep will never
  touch them because they read as foreign. This is the user's own data left unreclaimable
  automatically; recovery is the explicit "mark completed" owner action folded into Q1, never
  an automatic re-owning rule.
- **Legacy/NULL:** pre-attribution rows never carry a fingerprint, so they can never render
  "live" (D3 requires truthy `owner`) — they read as interrupted and are finalize-eligible
  **once stale**. Post-rollout, live recording *always* writes `owner`, so a NULL
  `'recording'` row is a pre-attribution crash; during rollout a not-yet-updated peer can
  still be live with NULL owner, which is why the final rule finalizes NULL rows only when
  `stale` (see sweep rules).
- **Clock skew (D5 caveat):** `segments.created_at` is the *recorder's* clock; freshness
  compares it to the *viewer's* wall clock. On LAN/self-host (NTP) skew is seconds.
  Bounded failure modes: viewer ahead by δ → a live session may read "interrupted"
  (δ − 3 min early); viewer behind by δ → a dead session reads "live" up to δ longer.
  Both are cosmetic — neither loses data nor enables a write. A monotonic/logical heartbeat
  is deferred (D8: the segment stream is already the signal, adding a logical clock is a new
  sync write for no v1 payoff).

## UI spec — remote-live & interrupted states

Follows `docs/FRONTEND.md` + the frontend-design workflow (invoke `/frontend-design` before
writing the component).

- **New branch** in `NoteDetailView`, ordered *before* the fallback (`:284`), covering **all
  non-active `'recording'` rows** so the "Start speaking" fallback is no longer reachable for
  them:
  - `owner && owner !== me && fresh` → **RemoteLive**.
  - else (`owner === me`, `owner === null`, or stale) → **Interrupted** (same read-only
    shell, different badge copy).
- **RemoteLive:** transcript-only, read-only `ChatView` fed `viewSessionSegments`; a live
  badge `● Live on {label}` where `label` = roster entry `.label` matched by fingerprint
  (`sync.ts:101-109`) else `"another device"`. Hidden: record/resume/edit/delete, the notes
  editor's write affordances, `FloatingChatBar` send. Segments auto-render as D4 refreshes
  fire.
- **Interrupted:** identical read-only shell; badge `Interrupted on {label}` (or `Interrupted`
  for NULL/legacy). No "live" pulse.
- Sidebar list may later surface the same `● Live on {label}` glyph; out of this tranche
  (Q3-adjacent) — spec only the open-note surface here. RemoteLive scrolling per resolved Q3:
  stick-to-bottom with user-scroll override, inheriting `ChatView`'s existing behavior
  (`ChatView.tsx:159-163`).

## Edit-in-progress under live refresh (D4 normative)

D4's applied-batch reload of `viewSessionSegments` fires on a device that may be *editing* a
segment of the same open session. The editor is **uncontrolled**: `EditableSegment` renders
`contentEditable` and holds the in-flight text in the DOM, not in store state
(`EditableSegment.tsx:178-206`; save on blur via `editSegmentText`, `EditableSegment.tsx:75-83,
:202`), and `NoteEditor` takes `refreshKey={noteRefreshCounter}` as a **prop**
(`NoteDetailView.tsx:191/228/274`; prop declared `NoteEditor.tsx:264-268`) consumed by a
**content-reload effect** (`NoteEditor.tsx:332-351`) that calls `editor.commands.setContent` —
it is *not* a React key and no remount occurs. A naive reload (a `noteRefreshCounter` bump
re-running that effect, or a segment-list refresh replacing the edited node) would discard the
open edit or drop an in-flight `editSegmentText`.

**Normative requirements:**
- The D4 reload **must preserve in-progress edit state**: it must not remount or overwrite the
  DOM of a segment currently being edited, and must not drop an in-flight `editSegmentText`
  write. Concretely, the applied-batch reload must be a no-op (or deferred) for the segment
  under active edit — e.g. skip the reload while an edit is open, or reconcile without
  remounting the editing node. It must **not** bump `noteRefreshCounter` for the open session
  while an edit is in progress. *Implementation hint (spec stays mechanism-agnostic):*
  `isEditing` is component-local state inside `EditableSegment`, so suppressing the reload
  during an open edit requires a store-visible edit-in-progress signal the D4 refresh can
  consult.
- A **same-segment concurrent edit** (this device and a remote device both edit one segment)
  **resolves by LWW** at the CRR layer. The local editor must **reconcile to the stored winner
  on blur / next refresh**: on blur the local save runs (its write competes in LWW like any
  other); on the following refresh the segment re-renders to the LWW-winning stored text.
  Named outcome: **the last write wins and the local editor snaps to the stored winner** — a
  losing local edit is not silently preserved in the DOM, and no crash or duplicate segment
  results.

## Sweep rules (interim + final)

`me` = `device_fingerprint` (**NULL when sync unconfigured**). `empty` = no `segments` AND no
`session_audio_parts` rows. `stale` = `heartbeat` older than the D5 threshold.
`foreign` = `recording_device_id IS NOT NULL AND recording_device_id != me`.

**Interim (ships first, no schema dependency):**
- **DELETE:** none. The sweep never deletes.
- **FINALIZE** (`status='recording'` → `completed`) only when the **DB was never
  CRR-prepared** — a pure DB-level check: no crsql clock shadow tables exist
  (`is_crr(conn, "sessions")` false, `crates/yapstack-sync/src/schema.rs:212`; already used
  app-side at `apps/desktop/src-tauri/src/db_service.rs:576`) — and then only where `stale`.
  **On a CRR-prepared DB the interim finalizes nothing** and defers all finalization to the
  final rule (sequencing step 3). The predicate is deliberately **not**
  `device_fingerprint IS NULL`: a credential-clear / fresh keychain over a *retained* CRR DB
  has a NULL fingerprint while the DB still holds foreign synced rows, and the fingerprint
  predicate would let the interim finalize a peer's stale-at-receiver live row and propagate
  the `'completed'` after re-enrollment. The DB-level check cannot be fooled that way.
  Rationale for the CRR-prepared refusal: on a synced DB, `max(segments.created_at)` is the
  *recorder's* clock delivered by sync, so a receiver B that boots into a sync gap can read
  A's genuinely-live row as stale-at-receiver and — if it finalized — would write
  `'completed'` at `db.rs:126-136` and LWW-propagate it back onto A mid-recording (the exact
  P0 path) while also breaching D6 (owner-only finalization). So the interim never finalizes
  on any DB that has ever synced.
- **Honest residual (CRR-prepared interim):** genuine *local* crash cleanup is **not**
  performed by the interim on a CRR-prepared DB — those `'recording'` rows stay open until
  the final rule (step 3) ships and finalizes them ownership-gated. Bounded: this is a
  cosmetic "still shows recording" until the column lands, never data loss. Local-crash
  finalization *is* still performed by the interim on a never-CRR-prepared DB (single-device
  semantics unchanged).

**Boot-ordering hazard (must be stated).** `start_drain_if_enabled` (`lib.rs:757`) runs
*before* `ensure_runtime_schema` (`lib.rs:783`), which runs the sweep (`db.rs:61`). The sweep
therefore executes on its own `open_managed` connection while the drain is already pushing/
pulling on its dedicated runtime — the sweep races the boot drain. **Decision: the final
sweep tolerates the race and need not move before drain start.** It writes only
owner-legitimate finalizations of the device's *own* crashed rows (always crashes at the
owner's own boot, per D6), which are correct regardless of drain interleaving, and it runs on
the same CRR path (`open_managed`) so its writes are captured and synced normally. The
interim finalizes nothing on a CRR-prepared DB, so it introduces no racing write at all.

**Final (after `recording_device_id` ships):**
- **Never touch `foreign` rows** (D6 — non-owner has no finalization authority).
- **DELETE** empties that are *not foreign* **and** delete-eligible:
  `status='recording' AND empty AND ( me IS NULL OR recording_device_id = me )`.
  (Sync-off: `me IS NULL` → today's delete-empties semantics. Sync-on: only own empties.)
  A NULL-owner row on a sync-*on* device is **never** delete-eligible — it may be another
  device's legacy row that would sync the DELETE back.
- **FINALIZE** the rest that are *not foreign*:
  `status='recording' AND NOT foreign AND NOT (deleted above)`, with one extra condition on
  the NULL-owner branch: **NULL-owner rows must also be `stale`**. Version-skew rationale:
  during rollout a not-yet-updated peer still records live with a NULL owner, and an updated
  device would otherwise finalize that live row once. Owned rows are always crashes at the
  owner's own boot (no staleness needed); NULL **stale** rows are pre-attribution crashes
  (can't be live — see state machine) → finalize is safe.

SQL-level ownership predicate for both the DELETE and UPDATE `WHERE`:
`recording_device_id IS NULL OR recording_device_id = ?me` (parameterized with `me`; when
`me` is NULL, bind NULL and gate the DELETE branch on `me IS NULL` as above). Foreign rows
match neither.

## Sequencing (each slice reversible)

1. **P0 sweep gate — interim** (`db.rs:98-164` only): never-delete + finalize-nothing-on-a-
   CRR-prepared-DB (finalize stale only when the DB was never CRR-prepared, per the DB-level
   `is_crr` check). Ships independently and neutralizes the sweep's destructive writes on
   every DB that has ever synced; it does **not** close cross-device finalization for rows
   stale-at-receiver during a sync gap, and it defers all CRR-prepared local-crash
   finalization to step 3. The actual P0 close is the column + final rule (step 3).
2. **Schema column + writers** (`db.rs:445` migration, `schema.rs:314` register, `db.ts:467`
   + `db.ts:523` writers, `appStore.ts:1273` fingerprint pass-through).
3. **Sweep final rules** replace the interim gate now the column exists.
4. **Remote-live view + live segment refresh** (`NoteDetailView` new branch, D4 in
   `appStore.ts:1147-1169`).
5. **Staleness rendering** (heartbeat threshold, interrupted copy).

## Verification

| Slice | Test |
|---|---|
| Sweep gating (interim + final) | Rust unit over the matrix: {owned, foreign, NULL} × {empty, has-segments} × {fresh, stale} × {sync-on, sync-off} × (interim only) {CRR-prepared, never-CRR-prepared} — assert foreign never mutated, no cross-device DELETE, NULL-on-sync-on never deleted, sync-off empties still deleted; **interim:** zero finalize on any CRR-prepared DB (including NULL-fingerprint-after-credential-clear over a retained CRR DB), stale-only finalize on never-CRR-prepared. |
| Column sync + quarantine-replay | Engine-level (`yapstack-sync`): CRRify at base schema, apply a changeset carrying `recording_device_id`, assert quarantine then replay after `apply_out_of_band_alters` (`schema.rs:327`, `quarantine.rs`); value converges. |
| Remote-live view branch | Component test: `{status:'recording', recording_device_id: 'B', me:'A'}` renders RemoteLive read-only with badge + no write affordances; stale variant renders Interrupted; `owner===me` and NULL variants render Interrupted, never "Start speaking". |
| Live segment refresh (D4) | Store test: emit `sync://applied` with an open non-active session; assert `viewSessionSegments` reloads (and does **not** when the open session is the local active one). |
| **Stale-at-receiver, live-at-source must NOT be finalized by the receiver** | A records >3 min; a sync gap makes B's synced `max(segments.created_at)` read stale-at-receiver; B boots. Assert **B does not finalize A's row** and no `'completed'` LWW-propagates back to A — **including when B's fingerprint is NULL** (credential-clear / fresh keychain over a retained CRR DB awaiting re-enrollment). **Satisfied by the final rule** (foreign rows never touched, D6). The **honest interim explicitly does not satisfy this by finalizing** — on any CRR-prepared DB it runs no finalize at all (PM preference (a)); because the gate is the DB-level `is_crr` check (`schema.rs:212`), not the fingerprint, a never-CRR-prepared DB *cannot contain* another device's synced rows, so **no cross-device finalize can occur in the interim window** — exactly. |
| Follow-along (integration) | Two-device UAT script: A records; B opens the session within seconds → sees "● Live on A" (not "Start speaking"); transcript fills as A speaks; A stops → B shows completed; A crashes (kill) → B shows "Interrupted on A" after 3 min → A reboots → both completed. |

## Non-goals (v1)

- Cross-device takeover / resume of a session recorded elsewhere.
- Per-append heartbeat column (D8 — segments arriving *is* the heartbeat; zero extra sync
  writes).
- Audio follow-along (a separate tranche; this covers transcript only).
- Multi-writer / simultaneous recording of one session.

## Resolved questions (owner-delegated PM decisions)

1. **Manual finalize escape hatch — YES, "mark completed", visibility-gated to interrupted
   only.** Covers both stranding paths under owner-only finalization (D6): a device that dies
   and never returns, and same-hardware re-pair (re-fingerprint makes own old rows
   foreign-to-self; sweep never touches them). **Affordance:** an action in the session's
   context/overflow menu, shown **only** when `{status='recording', foreign owner, stale}`;
   confirm dialog names the device — *"This session appears interrupted on \<label\>. Mark it
   completed?"*. Rationale: (a) automatic reclaim stays rejected — a relay gap makes
   false-stale indistinguishable from dead, and only the human holds the missing fact (is the
   other machine actually recording?); (b) visibility-gating makes misuse structurally
   impossible on fresh-live sessions and adds zero clutter elsewhere; (c) a worst-case
   mis-click during a relay gap is provably self-correcting: the recorder's isActive branch
   takes render precedence over synced status (state machine row 1), `insertSegment` never
   checks status (`db.ts:802-819`), and `completeSession` rewrites status + totals at stop
   (`db.ts:496-516`) — LWW converges to the recorder's final values, no recording lost.
   Residual: only the already-specced resume-race blast radius if someone *also* resumes the
   mis-completed session, discouraged by the dialog wording.
2. **Staleness threshold — flat 3 minutes, NOT chunk-cadence-derived.** Rationale: (a)
   cadence is the wrong model — `max_chunk_seconds` is user-configurable up to ~25-30 s
   (`apps/desktop/src-tauri/src/commands/live_transcription.rs:6416,6429`), and VAD/silence
   gating means a quiet stretch emits **no** segments; the dominant heartbeat-gap source is
   silence + relay latency, both unbounded by cadence, so deriving from cadence is false
   precision. (b) Cadence-derivation would require the receiver to know the recorder's config,
   which is not synced — a new dependency for a label. (c) A false "Interrupted" is bounded to
   cosmetics **by construction**: no write decision keys off cross-device staleness anywhere
   in this spec (the escape hatch is human-confirmed, the sweep is owner-only), so a mislabel
   during a long silence self-heals on the next segment. 3 min > drain interval +
   conversational lulls, yet small enough that a dead session is labeled within minutes.
   Threshold is a named constant; revisit only if UAT shows silence-driven flapping.
3. **Follow-along auto-scroll — stick-to-bottom with user-scroll override, reusing the
   existing local live-view behavior.** `ChatView` already implements exactly this
   ("Stick-to-bottom: follow new segments unless the user has scrolled away",
   `ChatView.tsx:159-163`; `userScrolled` override). Rationale: (a) the remote-live view
   answers "what is being said right now" — same intent as the local live view, so behavioral
   consistency between the two live modes *is* the design; (b) the mechanism already exists
   and is battle-tested locally — the follow-along inherits it rather than introducing a
   second scroll policy; (c) the override preserves read-back, the classic failure of naive
   pinning.
