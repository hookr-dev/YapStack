-- SPDX-License-Identifier: AGPL-3.0-only
-- YapStack relay — initial schema (Gate 3 / T007).
--
-- Workspace-grain tenancy from the first migration (ENTITLEMENTS_SEAM.md decision 5,
-- architecture §4.2): tenant = workspace, and a solo user is a degenerate 1-member
-- workspace. tenant_id == workspace_id EVERYWHERE. Teams later is additive (more
-- members + shared-vault wrapping), never a re-tenancy of these predicates.
--
-- The relay is a BLIND store: every content column is ciphertext (bytea) or opaque
-- metadata. RLS protects *metadata isolation*; the content is encrypted regardless.
--
-- RLS pooling guards (architecture §5): every tenant-scoped table is
-- `ENABLE ROW LEVEL SECURITY` + `FORCE ROW LEVEL SECURITY` (so even the table owner
-- is subject), with a transaction-local guard set via
-- `set_config('app.tenant_id', <uuid>, true)` and read back with
-- `current_setting('app.tenant_id', true)`. The predicate FAILS CLOSED when the
-- setting is unset/empty: `nullif(current_setting('app.tenant_id', true), '')::uuid`
-- is NULL, so the `= NULL` comparison hides every row. The application connects as a
-- NON-OWNER role (`yapstack_app`) so it can never bypass RLS.

-- ---------------------------------------------------------------------------
-- Non-owner application role. Created idempotently; the app MUST connect as this
-- role (never as the migration owner). FORCE RLS below also covers the owner as
-- belt-and-suspenders.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'yapstack_app') THEN
        CREATE ROLE yapstack_app NOLOGIN;
    END IF;
END
$$;

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- ---------------------------------------------------------------------------
-- Identity bootstrap tables (accessed in the PRE-tenant auth path, before an
-- app.tenant_id context exists: login looks a user up by email before any tenant is
-- known). These are NOT RLS-scoped; isolation here is by unique(email) + scoped
-- queries. All *content* tables below ARE RLS-forced.
-- ---------------------------------------------------------------------------
CREATE TABLE workspaces (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name        text NOT NULL DEFAULT '',
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id                          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email                       text NOT NULL,
    -- CRYPTO_SPEC §3.1: the server stores a SECOND hash of auth_key, never auth_key,
    -- never the password. verifier = Argon2id(auth_key, server_salt, m=19456,t=2,p=1).
    verifier                    bytea NOT NULL,
    server_salt                 bytea NOT NULL,
    -- The client's Argon2 salt (§2.1); not secret, server may store it.
    salt_enc                    bytea NOT NULL,
    -- Committing-envelope wraps of the vault key (§4.2). Inert without the password /
    -- recovery code — the server hands them out but can never unwrap them.
    wrapped_vault_key_password  bytea NOT NULL,
    wrapped_vault_key_recovery  bytea NOT NULL,
    created_at                  timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_email_lower_key ON users (lower(email));

-- ---------------------------------------------------------------------------
-- Helper: apply the standard RLS pooling guard to a tenant-scoped table whose
-- tenant column is `workspace_id`.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION yapstack_apply_rls(tbl regclass) RETURNS void AS $$
BEGIN
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', tbl);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', tbl);
    EXECUTE format(
        'CREATE POLICY tenant_isolation ON %s USING '
        || '(workspace_id = nullif(current_setting(''app.tenant_id'', true), '''')::uuid) '
        || 'WITH CHECK '
        || '(workspace_id = nullif(current_setting(''app.tenant_id'', true), '''')::uuid)',
        tbl);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO yapstack_app', tbl);
END;
$$ LANGUAGE plpgsql;

-- workspace_members: attaches users to workspaces. tenant column = workspace_id.
CREATE TABLE workspace_members (
    workspace_id  uuid NOT NULL REFERENCES workspaces(id),
    user_id       uuid NOT NULL REFERENCES users(id),
    role          text NOT NULL DEFAULT 'owner',
    added_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
SELECT yapstack_apply_rls('workspace_members');

-- devices: one row per install (§7.1). client_id is a fresh UUIDv4 per install.
CREATE TABLE devices (
    client_id     uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces(id),
    user_id       uuid NOT NULL REFERENCES users(id),
    ed25519_pub   bytea NOT NULL,
    label         text NOT NULL DEFAULT '',
    state         text NOT NULL DEFAULT 'active',   -- active | stale | retired (§6.5)
    added_at      timestamptz NOT NULL DEFAULT now(),
    last_seen     timestamptz NOT NULL DEFAULT now()
);
SELECT yapstack_apply_rls('devices');

-- device_roster: the opaque signed roster (§7.3), one per tenant. The server stores
-- {device_list, signature} verbatim and cannot forge it (no vault key). counter and
-- vault_key_epoch are the anti-rollback watermarks (§7.4).
CREATE TABLE device_roster (
    workspace_id     uuid PRIMARY KEY REFERENCES workspaces(id),
    device_list      jsonb NOT NULL,
    signature        bytea NOT NULL,
    counter          bigint NOT NULL,
    vault_key_epoch  bigint NOT NULL,
    updated_at       timestamptz NOT NULL DEFAULT now()
);
SELECT yapstack_apply_rls('device_roster');

-- refresh_tokens: rotation + reuse detection per device family (architecture §5/§10).
-- A presented token already `rotated_at`/`replaced_by` => REUSE => revoke the whole
-- family_id.
CREATE TABLE refresh_tokens (
    id            uuid PRIMARY KEY,               -- == JWT jti
    workspace_id  uuid NOT NULL REFERENCES workspaces(id),
    user_id       uuid NOT NULL REFERENCES users(id),
    client_id     uuid,
    family_id     uuid NOT NULL,
    issued_at     timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL,
    rotated_at    timestamptz,
    replaced_by   uuid,
    revoked       boolean NOT NULL DEFAULT false
);
CREATE INDEX refresh_tokens_family_idx ON refresh_tokens (family_id);
SELECT yapstack_apply_rls('refresh_tokens');

-- tenant_limits: pushed by the control plane (ENTITLEMENTS_SEAM). tenant_id ==
-- workspace_id. A NULL limit column = explicit unlimited; a MISSING row = deployment
-- default (resolved in yapstack-entitlements).
CREATE TABLE tenant_limits (
    tenant_id            uuid PRIMARY KEY REFERENCES workspaces(id),
    plan                 text NOT NULL DEFAULT 'default',
    storage_bytes        bigint,
    upload_bytes_period  bigint,
    share_count          bigint,
    device_count         bigint,
    period_start         timestamptz,
    warn_storage         double precision,
    warn_upload          double precision,
    state                text NOT NULL DEFAULT 'active',   -- active | grace | frozen
    grace_until          timestamptz,
    updated_at           timestamptz NOT NULL DEFAULT now()
);
-- RLS uses tenant_id as the tenant column here (not workspace_id); apply directly.
ALTER TABLE tenant_limits ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_limits FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_limits USING
    (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK
    (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON tenant_limits TO yapstack_app;

-- tenant_usage: open metering counters (ENTITLEMENTS_SEAM decision 4). T008 wires the
-- choke point that increments these.
CREATE TABLE tenant_usage (
    tenant_id            uuid PRIMARY KEY REFERENCES workspaces(id),
    storage_bytes        bigint NOT NULL DEFAULT 0,
    upload_bytes_period  bigint NOT NULL DEFAULT 0,
    share_count          bigint NOT NULL DEFAULT 0,
    device_count         bigint NOT NULL DEFAULT 0,
    window_start         timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE tenant_usage ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_usage FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_usage USING
    (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK
    (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON tenant_usage TO yapstack_app;

-- tenant_seq: the per-tenant commit-ordered counter row for changeset_seq (locked
-- security default: "per-tenant commit-ordered counter row … never bigserial"). T008
-- performs the SELECT ... FOR UPDATE increment at commit; the table is defined here.
CREATE TABLE tenant_seq (
    workspace_id  uuid PRIMARY KEY REFERENCES workspaces(id),
    next_seq      bigint NOT NULL DEFAULT 1
);
SELECT yapstack_apply_rls('tenant_seq');

-- changesets: opaque ciphertext blobs. changeset_seq is a DENSE, per-tenant,
-- commit-ordered cursor assigned at commit from tenant_seq (T008). (client_id,
-- client_seq) is the push idempotency key. schema_version/engine_version here are the
-- server's UNTRUSTED plaintext hints (CRYPTO_SPEC §5.4) — routing only, never a trust
-- decision; the authority is the AAD-bound version inside the ciphertext.
CREATE TABLE changesets (
    workspace_id    uuid NOT NULL REFERENCES workspaces(id),
    changeset_seq   bigint NOT NULL,
    client_id       uuid NOT NULL,
    client_seq      bigint NOT NULL,
    ciphertext      bytea NOT NULL,
    schema_version  integer NOT NULL,
    engine_version  integer NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, changeset_seq),
    UNIQUE (workspace_id, client_id, client_seq)
);
SELECT yapstack_apply_rls('changesets');

-- shares: opaque committing-envelope ciphertext (CRYPTO_SPEC §8); the key lives only
-- in the URL fragment and never reaches the server.
CREATE TABLE shares (
    share_id      uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces(id),
    ciphertext    bytea NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
SELECT yapstack_apply_rls('shares');

GRANT USAGE ON SCHEMA public TO yapstack_app;
GRANT SELECT, INSERT, UPDATE ON users TO yapstack_app;
GRANT SELECT, INSERT ON workspaces TO yapstack_app;
