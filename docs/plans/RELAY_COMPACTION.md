<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Relay history compaction — design for ratification

> **Status (2026-09-16): PROPOSAL, not implemented.** This is slice 5 of the plan in
> [`../SYNC_SPEC.md`](../SYNC_SPEC.md) §17 (§15.F). It is written for the owner to ratify
> before any code, because the join path replaces a device's live database file with a
> snapshot — the same class of action as the enable-time cutover, with the same
> data-loss stakes. Each open question below carries a recommendation; a "ratified" mark
> next to each is the go signal.

## Problem

The relay's `changesets` table is append-only. The owner's two-device relay holds about
12 000 changesets (≈ 490 MB) after the audio-part duplication bug (fixed in
`77d2f2a`), and it only grows: a fresh device replays every changeset since the
account's first push, a re-enabled device after an account switch pulls from zero, and
the relay's storage is never reclaimed. Nothing is incorrect today — the dense cursor and
idempotent merge make the replay safe — it is slow and unbounded.

## What exists already (the seam)

- Relay: `POST /snapshot/presign {sha256, size, generation, baseline_seq}` and
  `GET /snapshot` (presigned, existence-checked, content-addressed, choke-metered) —
  `crates/yapstack-server/src/snapshot.rs`, table `snapshots(workspace_id, generation,
  ciphertext_sha256, size_bytes, baseline_seq)`.
- Engine: `snapshot::produce_snapshot_bytes` (a `VACUUM INTO` copy of a CRR database),
  `snapshot::write_snapshot_file`, `snapshot::resite_as_fresh_peer` (demotes the seed's
  rows to a peer ordinal and mints a fresh local `site_id`), `crypto::SnapshotCipher`
  (AAD binds tenant, generation and **epoch** — CRYPTO_SPEC §5.2), transport
  `put_snapshot` / `get_snapshot` (with `MockRelay` support).
- Client: none. No command produces or installs a snapshot; every join replays history.

## Design

### D1 — Per-device pull acknowledgement (new)

`GET /sync/pull?since=&limit=&ack=` gains an optional `ack`: the caller's durable pull
watermark. The relay records `devices.pull_ack_seq = GREATEST(pull_ack_seq, ack)` for the
token's `client_id` (active devices only). This is the relay's only knowledge of how far
each device has merged, and it is plaintext metadata (a number), not content.

### D2 — Retention floor (new)

`tenant_seq.floor_seq` (default 0) = the highest pruned `changeset_seq`. Pull with
`since < floor_seq` returns no changes and `floor_seq` in the response (a new
`PullResponse.floor_seq`, default 0 for older relays); the client must bootstrap from a
snapshot (D5). `completeness` reports `contiguous = (max - floor == count)`, and the
per-client tails move to a durable `devices.push_tail_seq` maintained on every push, so
the G1 counter-regression guard survives pruning.

### D3 — Who publishes the snapshot, when (recommendation: any caught-up active device, on a size trigger)

The drain, on a cycle where the outbox is empty, `pull_behind == 0`, no crypto quarantine
and no schema quarantine are pending, and the relay reports more than **N = 2 000**
changesets above the floor, produces a snapshot of its own live CRR database:
`VACUUM INTO` a scratch file, **strip every local-only table** (`_yapstack_*`,
`_sqlx_migrations` stays, FTS shadow tables are dropped and rebuilt on install), encrypt
under the current epoch, `put_snapshot(generation = previous + 1, baseline_seq = its pull
watermark)`, then call D4. At most one attempt per 24 h per device; a failure is logged,
never surfaced as a sync error (compaction is an optimisation).

*Alternative rejected:* a designated "seed" device (the 2026-07 owner decision "Mac
seeds"). Any active device holds the full converged state once caught up; a role adds a
setting and a failure mode.

### D4 — Prune rule (recommendation: sweep-first, never below any active device)

`POST /sync/compact {generation}` (active caller, re-authenticated? — no: it deletes only
what every device has already merged, so a stolen token can at worst force a bootstrap on
a laggard; **not** re-authenticated). The relay computes
`floor_new = min(baseline_seq(generation), MIN(pull_ack_seq) over ACTIVE devices)` —
a device that never acknowledged (NULL) counts as 0 and blocks pruning; pending and
revoked devices are ignored. It deletes `changesets WHERE changeset_seq <= floor_new`,
sets `tenant_seq.floor_seq`, drops snapshot generations older than the newest two, and
returns `{floor_seq}`. Because every active device is at or past the floor, **no active
device ever bootstraps**; only fresh installs, account switches, and long-off devices do.

*Alternative rejected:* "one-time re-seed" (force every device to bootstrap once). It
would touch every device's live file for a one-off gain and re-introduces the
shared-ancestry hazard.

### D5 — Bootstrap install on the client (the risky part)

Trigger: pull returns `floor_seq > since`. Preconditions, all checked before touching the
file: outbox empty (everything pushed and acked), no in-flight audio seal, drain lanes
stopped, a snapshot present whose `baseline_seq >= floor_seq` and whose epoch equals the
session epoch (a snapshot sealed under an older epoch is refused; the publisher then
re-seeds). Steps, reusing the cutover machinery (`cutover_with_fault`'s journal, backup,
fsync and rollback discipline):

1. Download to a temp file, decrypt segment... (whole-blob committing envelope today;
   sizes are tens of MB), verify the generation AAD, write the staging DB.
2. Open staging with `CrsqlDb`; `resite_as_fresh_peer`; delete `_yapstack_*` rows;
   **carry over local-only tables from the current live DB** (`audio_save_locations`,
   `chat_context_settings`, `pending_audio_deletions`, the audio upload queue, the
   fetch-cache bookkeeping) via `ATTACH`; rebuild FTS; `apply_out_of_band_alters` +
   `heal_stale_crr_triggers`; run the in-repo migration list so the staging schema
   matches this build.
3. Set `pull_watermark = baseline_seq`, `push_watermark = max_local_db_version`
   (resited rows are a peer's history and are never re-pushed), `bound_tenant_id`.
4. Journaled swap: backup → rename → reopen → oracle. The oracle is **not** row-count
   equality (counts differ by design) but: `PRAGMA integrity_check`, every synced table
   present, and the local-only tables' row counts equal to the pre-swap counts.
5. Restart the lanes; the first pull from `baseline_seq` re-acquires anything newer,
   including this device's own echoes (own-echo merge, `179121b`).

A device with an **independently populated library** (the owner's original Windows case)
goes through the same path: its enable pushes its whole library first (push watermark 0),
then, if the relay's floor is above 0, it bootstraps and pulls its own rows back. The
lossless-union property holds because its rows are on the relay before the file is
replaced; the precondition "outbox empty" is what guarantees it.

### D6 — Quota and metering

Deleting changesets would not decrement `tenant_usage.storage_bytes` — unlike audio-blob
GC, which does decrement it since the simplification pass (SYNC_SPEC §7.2, §14 D21);
changeset metering would stay monotonic here because pruned history is replaced by a
snapshot of comparable size. Snapshot uploads are metered like audio. On self-host this
is invisible (AllowAll). Hosted policy is deferred with the hosted relay.

### D7 — Truncation recovery and hostile relay

A relay that lies about `floor_seq` (raises it) can force a bootstrap; the bootstrap
still verifies the snapshot's AEAD/AAD (epoch, generation, tenant) so it cannot inject
content, and the device's local data was pushed before the swap. A relay that prunes
below a device's ack (bug or malice) leaves that device unable to pull; it is surfaced as
a distinct "history unavailable — re-sync needed" state, never a silent "up to date".
Completeness' `contiguous` covers `(floor, max]`.

## Open questions → recommendations (mark each ratified)

| # | Question | Recommendation |
|---|---|---|
| Q1 | Sweep-first (prune only below every active device) or one-time re-seed? | **Sweep-first** (D4). |
| Q2 | Who publishes snapshots? | **Any caught-up active device**, size-triggered, ≤ 1/day/device (D3). |
| Q3 | Trigger threshold N and snapshot retention | **N = 2 000** changesets above floor; keep the **newest two** generations. |
| Q4 | Does compaction require re-authentication? | **No** (it only removes what every active device has). |
| Q5 | Quota metering | Prune does not decrement `storage_bytes` (audio GC does); document (D6). |
| Q6 | Truncation recovery UX | Distinct "history unavailable" state + automatic bootstrap when preconditions hold; a manual "Re-sync this device" button otherwise (D7). |
| Q7 | Bootstrap oracle | `integrity_check` + local-only row-count equality, never synced-row equality (D5.4). |

## Verification (before any UAT)

- Relay: prune never deletes above `MIN(pull_ack_seq)`; floor reported on pull; tails
  survive prune; snapshot generations retained/dropped as specified; compact refused for
  pending/revoked callers.
- Engine (MockRelay): bootstrap preconditions; install carries local-only tables and
  loses nothing (row-count oracle); resited snapshot is never re-pushed; own echoes
  re-acquired after install; an epoch-mismatched snapshot is refused.
- Desktop: journaled swap rollback on every fault-injection point (reuse the cutover
  harness); lanes stopped/restarted around the swap.
- Owner UAT (human gate): a fresh install joins the ~12 000-changeset account via a
  snapshot; the two existing devices are never bootstrapped; audio fetch still works.

## Sequence

1. Relay: ack, floor, durable tails, compact endpoint + tests.
2. Engine: bootstrap install + tests against MockRelay.
3. Desktop: publish trigger, bootstrap wiring, UX states.
4. Independent review of 1–3 (data-loss and hostile-relay lenses), then owner UAT.
