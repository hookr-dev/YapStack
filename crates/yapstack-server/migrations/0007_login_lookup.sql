-- SPDX-License-Identifier: AGPL-3.0-only
-- YapStack relay — non-owner serving hardening (server-rls finding cluster).
--
-- Two changes, both keeping FORCE ROW LEVEL SECURITY on every tenant table intact:
--
--   1. yapstack_lookup_login(email): the pre-tenant login/recover paths resolve a
--      (user, workspace) mapping BEFORE any app.tenant_id context exists, so they
--      cannot read the FORCE-RLS `workspace_members` table directly under the
--      non-owner serving role (yapstack_app) — the predicate is fail-closed when
--      the guard is unset, returning zero rows and 401-ing every login. A
--      SECURITY DEFINER function owned by the migration owner performs exactly that
--      one bootstrap read with row_security off, so the tenant tables keep FORCE RLS
--      while login/recover work under the non-owner role.
--
--      Contract: returns a ROW SET so a future user-in-multiple-workspaces is
--      expressible; v1 stores each user in exactly one workspace (one
--      workspace_members row per user), so callers treat it as a single-row lookup.
--
--   2. GRANT SELECT ON _sqlx_migrations: lets the non-owner serving role read the
--      applied-migration watermark at boot so `db::migrate` can skip the Migrator
--      (which would otherwise CREATE the migrations table and need CREATE on schema
--      public) when migrations are already applied. Migrations remain an owner-only,
--      one-time step; the serving role never runs DDL.

CREATE OR REPLACE FUNCTION yapstack_lookup_login(p_email text)
RETURNS TABLE(user_id uuid, workspace_id uuid)
LANGUAGE sql
STABLE
SECURITY DEFINER
-- row_security off + a pinned search_path: this definer read must not be filtered by
-- the tenant guard and must not resolve `users`/`workspace_members` through a
-- caller-planted temp schema.
SET row_security = off
SET search_path = pg_catalog, public
AS $$
    SELECT u.id, m.workspace_id
    FROM users u
    JOIN workspace_members m ON m.user_id = u.id
    WHERE lower(u.email) = lower(p_email)
$$;

REVOKE ALL ON FUNCTION yapstack_lookup_login(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION yapstack_lookup_login(text) TO yapstack_app;

GRANT SELECT ON _sqlx_migrations TO yapstack_app;
