-- SPDX-License-Identifier: AGPL-3.0-only
-- YapStack relay — Gate 4 / T008: audio blob dedup + refcount and admin replay nonces.
--
-- The changeset relay reuses the tables already defined in 0001 (changesets,
-- tenant_seq, tenant_usage, tenant_limits). This migration adds the audio side and the
-- admin-request replay guard.
--
-- Every tenant-scoped table added here gets the SAME fail-closed FORCE-RLS pooling
-- guard as 0001 (via yapstack_apply_rls, defined in 0001 and persisted in the DB).

-- ---------------------------------------------------------------------------
-- audio_blobs: within-tenant dedup on the CIPHERTEXT sha256 (architecture §9). The
-- relay stores no bytes — only the metadata that a blob with this hash exists, its
-- declared size, and a REFCOUNT of how many sessions reference it. Dedup + soft-delete
-- GC use the refcount so a shared blob is never orphaned while another session still
-- points at it (the GC job that deletes refcount=0 blobs from object storage is a T010+
-- stub; the table is designed here).
-- ---------------------------------------------------------------------------
CREATE TABLE audio_blobs (
    workspace_id       uuid NOT NULL REFERENCES workspaces(id),
    ciphertext_sha256  bytea NOT NULL,   -- 32-byte hash of the CIPHERTEXT (never plaintext)
    size_bytes         bigint NOT NULL,  -- client-declared, pinned by the presigned policy
    refcount           bigint NOT NULL DEFAULT 0,
    created_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, ciphertext_sha256)
);
SELECT yapstack_apply_rls('audio_blobs');

-- audio_objects: maps a session to the blob hash it references, so GET /audio/{session}
-- can resolve to an object key and redirect. One row per (tenant, session). A session
-- re-record repoints to a new hash (old blob refcount--, new refcount++).
CREATE TABLE audio_objects (
    workspace_id       uuid NOT NULL REFERENCES workspaces(id),
    session_id         uuid NOT NULL,
    ciphertext_sha256  bytea NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, session_id)
);
SELECT yapstack_apply_rls('audio_objects');

-- ---------------------------------------------------------------------------
-- admin_nonces: replay guard for the control-plane admin API (ENTITLEMENTS_SEAM.md
-- wire contract — Ed25519 signature over body + timestamp + monotonic nonce, ~5 min
-- window). This is a CROSS-TENANT control-plane concern, not tenant content, so it is
-- intentionally NON-RLS (like the identity bootstrap tables in 0001). Uniqueness of
-- `nonce` (the PK) is what makes a replayed request fail (INSERT conflict).
-- ---------------------------------------------------------------------------
CREATE TABLE admin_nonces (
    nonce    text PRIMARY KEY,
    seen_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX admin_nonces_seen_at_idx ON admin_nonces (seen_at);
GRANT SELECT, INSERT, DELETE ON admin_nonces TO yapstack_app;
