-- SPDX-License-Identifier: AGPL-3.0-only
-- YapStack relay — Gate 5 / T011: R2 snapshot-bootstrap metadata.
--
-- A SEED device publishes its whole library ONCE as an encrypted, compact DB snapshot
-- so a JOIN device can bootstrap from it instead of replaying ~434k per-cell changesets
-- (T003 measured the owner's 41 MB DB at ~434k initial changes/device). The relay stays
-- BLIND: the snapshot ciphertext is content-addressed by its CIPHERTEXT sha256 and
-- presigned directly to object storage (exactly like an audio blob, §9) — no bytes flow
-- through the relay. This table holds only opaque routing metadata.
--
-- `baseline_seq` is the tenant `changeset_seq` the seed had synced through when it made
-- the snapshot; a joining device resumes INCREMENTAL pull from there instead of
-- re-pulling everything the snapshot already carries.

CREATE TABLE snapshots (
    workspace_id       uuid NOT NULL REFERENCES workspaces(id),
    generation         bigint NOT NULL,
    ciphertext_sha256  bytea NOT NULL,   -- 32-byte hash of the CIPHERTEXT (never plaintext)
    size_bytes         bigint NOT NULL,  -- client-declared, pinned by the presigned policy
    baseline_seq       bigint NOT NULL DEFAULT 0,
    created_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, generation)
);
-- Same fail-closed FORCE-RLS pooling guard as every other tenant-scoped table (0001).
SELECT yapstack_apply_rls('snapshots');
