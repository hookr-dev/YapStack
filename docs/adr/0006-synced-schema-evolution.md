# 0006. Synced-schema evolution after cutover

- **Status**: Accepted
- **Date**: 2026-11-01
- **Companion**: ADR 0007 (session artifacts, jobs and derived records), which defines the first tables that use this mechanism.

## Context

Q4 adds the first synced tables since sync shipped (session artifacts and derived records, ADR 0007). The schema layer can't do that today.

1. **No way to add a synced table after cutover.**
   - `crr_migrate` CRR-ifies `SYNC_TABLES` only at cutover (`crates/yapstack-sync/src/schema.rs`).
   - On a CRR DB the migration runner refuses any SQL that names a sync table, except a single `ALTER TABLE` (`apps/desktop/src-tauri/src/db_service.rs`).
   - A failed migration is logged and skipped, then retried next launch (`db_service.rs`). Nothing surfaces it.
2. **An older peer receiving rows for a table it lacks wedges its entire pull.**
   - Sentinel rows bypass the column-gap quarantine (`quarantine.rs`), and cr-sqlite rejects the unknown table (`vendor/cr-sqlite/core/rs/core/src/changes_vtab_write.rs`).
   - The changeset rolls back and the page watermark never advances (`outbox.rs`).
   - Quarantine replay is all-or-nothing: one failing row aborts the whole replay transaction (`quarantine.rs`).
3. **Shipping the fix in a release isn't enough on its own.** Updates can be dismissed (`apps/desktop/src/components/UpdateBanner.tsx`), and the relay's pull has no version gate (`crates/yapstack-server/src/sync/pull.rs`). The relay's advisory `min_client_version` (`config.rs`) can't fill that gap: it compares release cores only and is shown only in the setup wizard (§5).
4. **Cutover drops secondary indexes.** The rebuild re-creates triggers but not indexes (`schema.rs`). On the owner's synced DB only `idx_chat_messages_send` and `idx_audio_parts_session` survive, because `lib/db.ts` re-creates them.

## Decision

### 1. Migration kinds
`Migration` (`db.rs`) gains `kind: Plain | CreateSyncTable { table } | SyncIndex`.
- **`SyncIndex`**: every statement is `CREATE [UNIQUE] INDEX IF NOT EXISTS … ON <sync_table>`, applied plainly. A non-PK `UNIQUE` index on a CRR table is refused (uniqueness is app-layer).
- **`CreateSyncTable`**: one `CREATE TABLE IF NOT EXISTS <table>` for a `SYNC_TABLES` member, then `ensure_crr(table)` (a `pub` wrapper over `transform_and_crrify`), then the table's declared indexes, then the migration is recorded.
  - The migration runner is the **only** DDL source for a new synced table. There is no `lib/db.ts` belt-and-braces CREATE.
  - A failed `CreateSyncTable` is recorded and shown in sync lane status, not only logged.
- On a non-CRR DB both kinds apply plainly; cutover converts the table later because it's in `SYNC_TABLES`.
- `SYNC_TABLES` is append-only.

### 2. Indexes survive the rebuild
- `transform_and_crrify` captures `sqlite_master` index SQL for the table, excluding `UNIQUE` indexes and `sqlite_autoindex_*`, and replays it after the RENAME.
- One `SyncIndex` migration (v18) re-declares every secondary index on a synced table with `IF NOT EXISTS`, including `idx_chat_messages_send` and `idx_audio_parts_session` and the v8 shape of `idx_chat_messages_session`.
- Invariant I-NT2: the full index set of a freshly migrated non-CRR DB equals the set after cutover plus migrations.

### 3. Boot heal
At drain start, `ensure_sync_tables_crr` converts any `SYNC_TABLES` table that exists but isn't a CRR. Its only job is to finish a run that crashed between CREATE and CRR-ify. It's idempotent, and `crsql_as_crr` backfills clocks for rows written in between.

### 4. Table-readiness quarantine
- The quarantine defers a row when its **table is not a CRR locally** (absent, or present but not converted), **sentinel rows included**, or when its column is unknown. `is_crr` is cached per changeset.
- Replay walks the stash in `id` order and applies **one row per savepoint**. A row that still fails stays stashed and is counted. Lane status shows "N changes waiting for an app update" and "M stashed changes failed to apply".
- A non-deferrable failure still surfaces as `pull_error`. Real corruption stays an honest stop; nothing is auto-skipped.
- `crr_migrate`, `cascade_gc` and `enforce_uniqueness` tolerate absent tables.
- Invariant I-NT1: a row for a table that isn't a CRR locally never fails a changeset.

### 5. Release gate: peer capability evidence
The relay's `min_client_version` can't serve as the gate. `client_is_older` compares release cores only, so every alpha build compares equal (`sync.rs`). The advisory is computed only in `sync_probe` (`sync.rs`) and shown only in the setup wizard, and pull never stops. So the gate uses evidence that comes from the peers themselves.
1. **Capability level.** An integer the client sends on every pull (`X-YapStack-Sync-Capability`). Level 1 = "has the table-readiness quarantine". A client that sends no header is level 0.
2. **Where it's recorded.** The relay records the level and a timestamp on the caller's existing `devices` row, as additive columns beside `platform`. It's advisory index metadata like `label`; the relay holds no content. None of the synced client tables is per-device, and a new synced table would need this very gate, so the relay roster is the only existing record that fits. The workspace levels live on a per-tenant RLS row (step 5).
3. **Release order.** Release N ships the quarantine and the capability header; the relay is deployed first. The first `CreateSyncTable` ships in N+1 or later (`docs/RELEASE.md`).
4. **The gate is on publication, not on the write.** Rows can exist before a device ever syncs, and cutover resets the push watermark to 0 (`apps/desktop/src-tauri/src/sync.rs`), so a never-synced device's whole history is captured at once, and the drain pushes before it pulls (`crates/yapstack-sync/src/outbox.rs`). So the check sits in the push path, where every route to the relay passes: normal capture, cutover, an account switch and an outbox restored with the DB.
   - Each table has a **sync capability level**, declared next to its `SYNC_TABLES` entry: 0 for the 12 cutover tables, and the release-gate level for every `CreateSyncTable` table.
   - At capture, each outbox entry stores the highest level among its rows, in a local column `_yapstack_outbox.required_sync_capability` (`INTEGER NOT NULL DEFAULT 0`, added idempotently by `ensure_outbox_table`, `outbox.rs`). Entries captured by older builds can't hold rows for new tables, so 0 is correct for them.
   - **The relay enforces it.** Every push request declares the highest level among its batch's entries (`X-YapStack-Batch-Capability`; no header means 0, which covers older builds). Before taking the `tenant_seq` lock (`yapstack-server/src/sync/push.rs`), push locks the tenant's floor row `FOR SHARE` and refuses with 409 `sync_capability_floor` (returning the current floor) when the batch level is above it. So a gated row is accepted only against the floor the relay holds *at that moment*. That stays true after a relay restore or re-provision that dropped the floor back to 0, because the client's persisted copy never authorizes a push.
   - `push_outbox` uploads in `client_seq` order. **Its batch planner (`outbox.rs`, grouped by size today) also cuts a batch wherever the entry level changes**, so every batch has a single level and the ungated prefix ahead of a gated entry still ships. On that 409 it tries the raise in step 5 and retries once. If the raise is refused, push stops with "waiting for <device label> to update" in lane status, and pull carries on. Nothing is skipped or reordered.
   - The client persists the last floor the relay returned, per workspace id (an account switch starts again from 0). It's **UX only**: it decides whether a feature is enabled (step 6) and never authorizes publication.
5. **The relay raises the floor atomically, serialized against pulls and pushes.** `begin_tenant_tx` runs at READ COMMITTED (`yapstack-server/src/db.rs`), and pull takes no lock today (`sync/pull.rs`). So a level check and a later read in the same pull could straddle a raise.
   - **Pull** (and `GET /snapshot`, `/snapshot/presign`, `lib.rs`, which hand out the same content) starts its transaction by locking the tenant's floor row `FOR SHARE`. It then records the caller's header and `capability_seen_at`, checks the level, and reads, all in that one transaction. `capability_seen_at` is stamped on every authenticated pull, refused ones included. To keep every pull from writing `devices`, the stamp is a conditional `UPDATE`: it runs only when the reported level changed, `capability_seen_at` is more than 1 h old, **or `capability_seen_at IS NULL`** (an existing device's first pull, headerless or not, is always stamped, so a level-0 client that keeps pulling counts in the 30-day query). One hour is negligible against the 30-day window, and a level change is still recorded at once.
   - **Raise:** `PUT required_sync_capability = L` locks the same row `FOR UPDATE`, records the caller's own header (so a device raising at cutover never blocks itself), and then reads `devices`. Every non-revoked device whose `capability_seen_at` falls in the last 30 days must report at least `L`. If that holds, it sets `required_sync_capability = GREATEST(required_sync_capability, L)` and returns the new value. A pull that passed the old check has therefore either committed its read before the raise, or waits and re-checks after it.
   - **On failure** the relay answers 409 with each blocking device's `client_id`, label, `platform` and `capability_seen_at`, and records `requested_sync_capability = GREATEST(requested_sync_capability, L)` on the floor row. The client never decides eligibility from a roster it fetched earlier.
   - **Where the levels live.** Not on `workspaces`: the relay role has only `SELECT, INSERT` there (`crates/yapstack-server/migrations/0001_initial.sql`). Row locks and the raise need `UPDATE`, and `workspaces` is the non-RLS registry (`gc.rs`), so granting `UPDATE` on it would let one tenant rewrite another's floor. Not on `tenant_seq` either: push holds that row `FOR UPDATE` (`push.rs`), so every pull would wait on every push. Both levels go on a new per-tenant table `tenant_sync_capability (workspace_id PK, required_sync_capability int NOT NULL DEFAULT 0, requested_sync_capability int NOT NULL DEFAULT 0)`, under `yapstack_apply_rls` (`0001_initial.sql`, which grants the app role the row privileges inside RLS). It's bootstrapped with `INSERT … ON CONFLICT DO NOTHING` before each lock, the way `tenant_seq` is (`push.rs`).
   - Lock order is always the floor row first, then `tenant_seq` (push only), so there's no lock cycle.
   - A test under the non-owner role (beside `tests/rls_nonowner_flow.rs`) shows that lock, raise and bootstrap all work as `yapstack_app`, and that one tenant can't read or raise another's floor.
   - An interleaving integration test (Postgres) holds a pull between its check and its read while a raise and a gated push run, and asserts the pull never returns the gated changeset.
6. **The feature-level write gate is UX only.** On a sync-enabled device, a feature that writes a gated table is enabled once the last floor the relay returned is at least its level. If it isn't, the device tries the raise (online only) and names any blocking device. Without this gate, one generated summary would hold up all of the device's outbound sync. Devices that aren't syncing write freely; step 4 catches their rows when sync is enabled.
7. **Stragglers.** The relay refuses pulls from devices below the workspace level with "update required". **A device offline longer than 30 days must update before it can sync. Its pull stops and it shows "update required", rather than wedging silently.**
   - A straggler that keeps syncing never ages out, because its own pulls refresh `capability_seen_at`, and updates can be dismissed (`UpdateBanner.tsx`). So every pull response carries `required` and `requested` levels. A device below `requested` shows "Another device needs you to update YapStack" in lane status, through the advisory path in step 9. It's a nudge, not a block. The blocking holder's UI names the device and offers the existing roster revoke.
   - Ghost rows: a reinstall mints a new `client_id` (`apps/desktop/src-tauri/src/sync.rs`), so the old row lingers until it ages out. The 409's `client_id`, `platform` and last-seen time let the user recognise it and revoke it from the same prompt.
8. **A device enabling sync later** sends its level on its first pull. Below the required level it's refused, and `sync_enable` shows "update required" before any merge. If its own captured history holds gated rows and the floor is still low, step 4 holds its push until the raise succeeds.
9. **The advisory stays advisory.** `min_client_version` is compared prerelease-aware, refreshed on every drain cycle, persisted as last-seen and shown in lane status. The relay's default floor becomes a prerelease value, so alpha builds aren't nagged.
10. `SYNC_SCHEMA_VERSION` is never bumped for additive tables.

### 6. Replay ordering and the write turnstile
Each replayed row runs in its own savepoint, under the process `WriteTurnstile` (taken before `BEGIN`). When a row fails, every later stashed row for the same `(table, pk)` is skipped for that pass. So a column row can never apply over a delete sentinel that failed.

## Consequences

- A new synced table is routine: a `CreateSyncTable` migration, a `SYNC_TABLES` entry, a `CASCADE_DELETE` entry if it's a child, and a SYNC_SPEC §4.1 line.
- SYNC_SPEC §4.1, §4.3, §6.4, §11, §12 (I-NT1, I-NT2) and §14 get updated.
- Every release that adds a table depends on the previous release's tolerance and capability header. The relay must be deployed with the capability columns first, and that includes self-hosted relays.
- A device can hold up new-table features for every peer until it updates or is 30 days absent. The feature names the device, and revoking it from the roster also clears it.
- A device offline for more than 30 days has to update before it can sync again.
- A device that wrote gated rows before enabling sync can't upload anything until the floor rises. Its outbox stays in order behind the first gated entry, lane status names the blocking device (with revoke), and the blocking device is told to update. Splitting the outbox to push around held entries was rejected: it reorders `client_seq`.
- Restoring indexes is local-only and changes no data or clocks.

## Alternatives considered

- **Bump `SYNC_SCHEMA_VERSION` per table:** rejected. Wholesale AAD quarantine across skew.
- **Infer "new table" from SQL text:** rejected. Implicit; an explicit kind is deterministic.
- **Pre-create future tables:** rejected. Speculative.
- **A frontend `CREATE TABLE IF NOT EXISTS` as a backstop:** rejected. It's a second DDL source; the runner is repo-owned (`db.rs`).
- **Rely on release N alone:** rejected. Updates can be dismissed, and pull isn't version-gated.
- **Gate on the relay's `min_client_version`:** rejected. It compares release cores only, so alphas compare equal, and nothing stops pull.
- **A synced client-side capability table:** rejected. Creating it needs the gate it would implement.
- **Gate the first local write only:** rejected. Rows written before sync, cutover's watermark reset and restored outboxes all reach the relay without passing it.
- **Authorize the raise from a cached `/devices` list:** rejected. The roster can change between fetch and raise, and an offline device can't know it. The relay re-checks eligibility atomically.
- **Trust the client's persisted floor for publication:** rejected. A restored or re-provisioned relay can sit below it. The relay checks each push instead.
- **Pull-time level check without a lock:** rejected. Under READ COMMITTED it races the raise.
- **Capability inside changesets:** rejected. An idle device pushes nothing, so it would never report.
- **Hard-block every old client by version at the relay:** rejected. A block keyed to an observed capability level only affects devices that would actually wedge.
- **All-or-nothing replay:** rejected. One bad row would strand every other stashed change.

## References

- `docs/SYNC_SPEC.md` §4, §6.4, §11, D5.
- `crates/yapstack-sync/src/{schema,quarantine,cascade,outbox}.rs`; `apps/desktop/src-tauri/src/{db.rs,db_service.rs,sync.rs}`; `crates/yapstack-server/src/{config.rs,db.rs,devices.rs,lib.rs,sync/pull.rs,sync/push.rs}`, `crates/yapstack-server/migrations/0009_device_platform.sql`; `crates/yapstack-common/src/write_turnstile.rs` (#102).
