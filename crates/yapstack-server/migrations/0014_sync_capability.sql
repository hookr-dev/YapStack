-- SPDX-License-Identifier: AGPL-3.0-only
-- Peer sync-capability evidence (ADR 0006 §5).
--
-- Every pull reports the client's sync capability level (`X-YapStack-Sync-Capability`;
-- no header = 0). The relay records it on the caller's existing `devices` row, beside
-- `platform`: advisory index metadata like `label`, never content. `capability_seen_at`
-- is NULL until the device's first pull after this migration.
ALTER TABLE devices ADD COLUMN IF NOT EXISTS sync_capability int NOT NULL DEFAULT 0;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS capability_seen_at timestamptz;

-- The workspace floor. Not on `workspaces` (the app role has only SELECT, INSERT there,
-- and it is the non-RLS registry) and not on `tenant_seq` (push holds that row FOR UPDATE,
-- so pulls would queue behind pushes). One row per tenant, bootstrapped by the relay with
-- `INSERT ... ON CONFLICT DO NOTHING` before each lock, exactly like `tenant_seq`.
--   required_sync_capability:  pulls below it are refused; push batches above it are refused.
--   requested_sync_capability: the highest level a raise has asked for (nudges stragglers).
-- Both only ever rise (GREATEST). Lock order: this row first, then `tenant_seq`.
CREATE TABLE tenant_sync_capability (
    workspace_id               uuid PRIMARY KEY REFERENCES workspaces(id),
    required_sync_capability   int NOT NULL DEFAULT 0,
    requested_sync_capability  int NOT NULL DEFAULT 0
);
SELECT yapstack_apply_rls('tenant_sync_capability');
