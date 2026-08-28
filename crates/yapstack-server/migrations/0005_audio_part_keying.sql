-- SPDX-License-Identifier: AGPL-3.0-only
-- YapStack relay — audio round-trip S1 (D6 + D8).
--
-- Re-key audio_objects from (workspace_id, session_id) to (workspace_id, part_id): blobs
-- are addressed by the `session_audio_parts` row UUID (`part_id`), NOT by
-- (session_id, part_index). The CRR rebuild strips UNIQUE(session_id, part_index)
-- (schema.rs:457-476), so (session_id, part_index) can COLLIDE across devices; a CSPRNG
-- UUIDv4 row id cannot. `session_id` is retained as a metadata column for future
-- per-session listing (indexed), never as the key.
--
-- ADDITIVE + SAFE under the load-bearing invariant: audio_objects is EMPTY in every
-- deployment (the audio routes have zero client callers until S1 ships), so there is no
-- data to migrate — a DROP + CREATE is a no-op on rows. DROP cascades the old table's RLS
-- policy and grants; yapstack_apply_rls re-applies them fail-closed (matching 0002).

DROP TABLE audio_objects;

CREATE TABLE audio_objects (
    workspace_id       uuid NOT NULL REFERENCES workspaces(id),
    part_id            uuid NOT NULL,   -- session_audio_parts.id (CSPRNG UUIDv4, D6); simple-format accepted
    session_id         uuid NOT NULL,   -- metadata only: future per-session blob listing
    ciphertext_sha256  bytea NOT NULL,  -- the blob this part references (content-addressed on audio_blobs)
    created_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, part_id)
);
CREATE INDEX audio_objects_session_idx ON audio_objects (workspace_id, session_id);
SELECT yapstack_apply_rls('audio_objects');
