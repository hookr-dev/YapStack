# Sync Remediation Plan — one path, tree-shaken

Grounded in a three-scout audit of HEAD 90a1401 (surface inventory / engine + vendored
cr-sqlite source analysis / dev-vs-prod classification of the T019–T030 chain).
Principle: build for the typical production user; stop carrying this dev environment's
history in the product.

## 1. The canonical flow (what a production user does)

```
install → Settings → Sync → relay URL (probe: ✓ Connected · saved)
       → sign up (first device) or sign in (later device)
       → later device: approved via fingerprint ceremony on an existing device
       → Enable sync  ──  ONE code path for every device:
            crr_migrate a copy of the library → drain loop
            each cycle: capture→chunk→encrypt→push  AND  pull→decrypt→merge
       → steady state: "Up to date · synced Xm ago" / ambient glyph
```

No device roles. No seed/join branching. No snapshot bootstrap. A fresh second device
starts with pull_watermark=0 and pulls full history; a populated independent second
device does the same and the merge is a **lossless union**.

**Why this is safe (audited, not assumed):** cr-sqlite `as_crr` backfills pre-existing
rows at col_version=1 with a real db_version (vendor .../backfill.rs:107-117), and the
equal-version tiebreak compares value then site_id (changes_vtab_write.rs:59-115).
Silent loss therefore requires two devices editing a row with the SAME primary key.
All 12 synced tables use TEXT UUID PKs (2 composite-UUID junctions; no fixed-PK
singletons are CRR'd) — independent devices cannot collide. The only lossy case is
**shared ancestry**: a library manually copied between machines before sync, then
edited on both.

**v1 posture on shared ancestry:** documented limitation ("don't copy libraries
between machines — sign in and sync instead"), enforced by nothing, mitigated by
nothing. `reconcile.rs` and the snapshot server routes stay in the tree (they're the
future "migrate an existing library" feature) but leave the product surface entirely.
An empirical lossiness test (same UUID, divergent edits, bidirectional merge) gets
added to the engine suite so the limitation is pinned by a failing-by-design
assertion, not folklore.

## 2. Core fixes (the actual bugs, all small)

- **F1 — decouple pull from push** in `drain_once` (outbox.rs:472: `push_outbox().await?`
  fully precedes the pull loop). Each cycle attempts both; each error is preserved
  independently. Cherry-pick from stash `T031-partial-seed-join-routing` where usable.
- **F2 — surface drain failures**: after N consecutive failed cycles (stash has
  DRAIN_FAIL_SURFACE_THRESHOLD=2 + a DrainHealth variant), the verbatim error reaches
  the sync panel + glyph as a distinct failing state. Today's "syncing (but nothing
  moves)" silence is the bug that cost us a debugging session.
- **F3 — un-wire the dead bootstrap**: `sync_enable` remains THE enable path;
  `sync_seed`/`sync_join` are unregistered from the command builder (code parked
  behind the future migration feature, or deleted — see tree-shake).
- **F4 — Windows round-3 retest** after F1/F2 ship in a new artifact: with errors
  surfaced, either it syncs or it tells us exactly why.

## 3. Tree-shake (delete; zero production installs exist, so migration shims serve nobody)

| Item | Evidence | Action |
|---|---|---|
| `sync_info` cmd + `SyncInfoDto` + `SyncInfoResponse` + `.info` wrapper | superseded by `sync_probe`; 0 callers | delete |
| `sync_seed` / `sync_join` registration (no TS wrapper, no UI) | never invoked | unregister; park or delete bodies with `reconcile.rs` kept |
| `sync_device_list` cmd + `.deviceList` | roster arrives via `sync_status`; 0 callers | delete |
| `sync_wrap_secret` cmd + `.wrapSecret` | future AI seam, unused | delete (recreate when the AI feature lands) |
| `repair_oversized_entries` (outbox.rs:236-337) | repairs pre-T021 poison entries no production user can have; owner's outbox fully acked | delete; KEEP the push guard (`SyncError::Oversized`) |
| Legacy `session-v1` migration (sync.rs:402-458) + `migrate_identity_from_session` (:675-698) | serve pre-upgrade installs; none exist | delete |
| Stale module doc (sync.rs:22-31 claims commands unregistered) | contradicts lib.rs:378-391 | rewrite |
| `isValidRecoveryCode` TS export | test-only | fold into test or delete |
| SSE `/sync/stream`, audio routes | mounted, zero client callers | leave server-side (harmless, phase-2 seams) but mark in code comments |

Estimated: several hundred LOC out of sync.rs (3,305 today), 3 commands off the
surface, both migration shims gone.

## 4. Hardening batch (debt triage — one worker task, from the consolidated 15-item list)

Ship-blocking (do now):
- **HIGH** refresh-rotation crash window (sync.rs:1069-1091): crash between relay
  rotation and local persist = device locked out → treat first-refresh 401 after
  restart as clean re-login prompt, never a hot-loop.
- **MED** chmod 0600 `sync-session.enc` on unix; **MED** bind `vault_key_epoch` into
  the session-store AAD (anti-rollback); **MED** `busy_timeout`/`BEGIN IMMEDIATE` on
  outbox write txs; **MED** drain-level connectivity plumbed into the `unreachable`
  display state (closes the TODO(T02x) pair; a relay that dies mid-session shows
  amber "can't reach", not red "error").

Defer (tracked, not v1): zeroization pass, nonce-uniqueness/mid-migration tests,
per-device last-seen/version (needs relay), fingerprint word/emoji presentation,
recovery-ceremony UX, tray/dock co-habitation, "attention" bucket.

## 5. Deploy/ops hygiene (self-hoster story vs this machine)

- Rebuild the local relay image (the 5 MiB `PUSH_BODY_LIMIT` from T023 is committed
  but the running container predates it).
- `deploy/.env.example` documents the real knobs; the LAN IP + moved MinIO ports
  (9010/9011) in the local `.env` are machine-specific and stay out of docs/defaults.
- Server changesets table is append-only with no GC — acceptable for v1 (ciphertext
  rows, per-tenant), noted as a phase-2 compaction item (a future snapshot feature is
  the natural anchor).

## 6. Honest gap vs the goal oracle

**Audio round-trip is NOT wired**: server audio routes exist, client transport has no
audio calls. The goal oracle says "audio round-trips." Owner decision required:
wire audio blob sync now (new scope) or amend the oracle to data-sync-v1 with audio
as the immediate follow-on. The plan assumes the latter but this is explicitly the
owner's call, not self-certified.

## 6b. Original-spec conformance (what we keep, defer, and why neither breaks it)

The World B pillars from the architecture doc are untouched and non-negotiable:
cr-sqlite on-device merge; blind encrypted relay (ciphertext only, per-tenant
commit-ordered seq); XChaCha20-Poly1305 + CRYPTO_SPEC AAD discipline; Ed25519 device
roster with the approval ceremony; refresh rotation with reuse detection; RLS +
non-owner DB role; self-host zero-outbound; no plaintext server-side ever. Nothing in
this plan weakens any of these — the tree-shake removes *unwired surface*, not spec
mechanisms.

Spec deliverables re-scoped with rationale:
- **Snapshot seed/join (spec R1/R2)**: designed for the two-populated-DBs day-one
  path. The audit showed its *correctness* role applies only to shared-ancestry
  libraries; for independent libraries (UUID PKs) full-history union is exactly
  correct. R1/R2 therefore become the future "migrate an existing library" +
  cold-start-bandwidth feature, not the enable path. `reconcile.rs` + snapshot
  routes are retained as that seam. If the owner's own two machines turn out to be
  shared-ancestry, the UAT will exercise this question honestly — the lossiness test
  (§1) pins the semantics either way.
- **Audio round-trip**: spec'd, server routes exist, client not wired — §6 owner
  decision. Not silently descoped.
- **SSE stream**: spec'd as a liveness optimization; polling drain is spec-compliant;
  stays a phase-2 seam.

## 6c. Remaining goal-oracle checklist (what "done" still requires)

From goal.md — none of this is waived by the remediation:
1. **§15 verification matrix green**: CRDT convergence proptest, schema-desync +
   quarantine-replay integration, no-plaintext-on-server grep, crypto round-trip
   Rust↔JS (KAT §13), RLS metadata isolation, refresh rotation/reuse detection.
   R-final includes an audit pass mapping each matrix row to its existing test (most
   landed in T001–T012; verify, don't assume) — plus the new tests from R2.
2. **Owner two-device UAT** (human gate, never self-certified): record on A appears
   on B; offline edits merge sanely; audio per the §6 decision.
3. **Covenant PR legal read** (human gate) before any merge to main.
4. Board `full_outcome_complete: true` only after 1–3.

## 7. Execution order (each slice reversible, gated on full checks)

1. **R1** tree-shake + stale docs (§3) — pure deletion, big surface win.
2. **R2** core fixes F1–F3 (+ engine tests: push-fails-pull-proceeds; empirical
   shared-ancestry lossiness pin).
3. **R3** hardening batch (§4 ship-blocking) — security-adjacent → Judge review.
4. **R4** relay image rebuild + push + CI artifact → Windows round-3 (F4).
5. Two-device oracle completion (record on A → appears on B, offline edits merge,
   audio per §6 decision).

## What we deliberately do NOT build

Role detection at enable. Snapshot bootstrap. Legacy self-heals. Migration shims for
installs that don't exist. Dev-relay-state workarounds in product code. Every one of
these was momentum from debugging this environment; none serves the production user.

---

## Addendum (2026-07-08, owner-ratified): Option A′ — CRR cutover with backend swap

Live two-device UAT exposed that the drain syncs the CRR *copy* and nothing bridges
copy↔live (UI reads live; post-enable local writes never captured). Owner decision:
**A′ — the CRR database becomes the app's live database**, implemented by replacing
tauri-plugin-sql's backend with repo-owned rusqlite-backed commands exposing the same
execute/select JS API (plugin has no extension seam; forking re-risks the T022
finalize-on-close abort; a bidirectional bridge = a second sync engine, rejected).
Safety fact: extension-less writes to CRR tables fail loudly (trigger → missing fn →
rollback) — no silent-corruption window.

Stages: **A1** spike (rusqlite command pool + per-connection crsql init + clean
finalize under concurrency/shutdown; WAL-checkpoint + file-swap mechanics — on a COPY,
never live) → **A2** backend swap (Rust db command module, frontend db.ts shim,
migration runner moves in-repo) → **A3** enable-time cutover + disable rollback +
migrations via crsql_begin/commit_alter → **A4** two-device UAT round 4.
