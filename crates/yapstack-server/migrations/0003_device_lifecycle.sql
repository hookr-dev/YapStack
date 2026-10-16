-- SPDX-License-Identifier: AGPL-3.0-only
-- YapStack relay — Gate 5 / T010d: auth-lifecycle, device-authorization, recovery.
--
-- Additive to 0001/0002. Adds:
--   * recovery-code AUTH columns on `users` (second-hash verifier + salt, §6.2) so
--     `POST /auth/recover` can authenticate a recovery code EXACTLY like the password
--     verifier (§3.1) before serving `wrapped_vault_key_recovery`. No password-
--     equivalent is stored: `recovery_verifier` is a second hash of a client-derived
--     recovery auth key, discarded after hashing — same posture as `verifier`.
--   * `devices.status` (pending|active) so a NEW device that authenticates via password
--     login is enrolled as PENDING (authenticated, but NOT yet a signed-roster sync
--     peer). Promotion to `active` happens only when an approving device uploads a new
--     signed roster naming it (§7.5).
--
-- The roster anti-rollback `counter` + `vault_key_epoch` columns already exist on
-- `device_roster` (0001) — no schema change needed for §7.4; the handler enforces
-- monotonicity on the plaintext integer under a `FOR UPDATE` row lock.
--
-- The relay stays BLIND: `device_list`/`signature` are opaque; the server reads only the
-- integer `counter`/`vault_key_epoch` and the client-supplied active-device metadata.

-- ---------------------------------------------------------------------------
-- Recovery-code auth columns (§6.2). NULLABLE so the ADD is safe on any existing
-- rows; `POST /auth/signup` always populates them, and `POST /auth/recover` treats a
-- NULL verifier as a uniform auth failure (still computing a dummy hash to avoid a
-- timing oracle). Identity bootstrap table — intentionally NON-RLS like the rest of
-- `users` (looked up by email before any tenant context exists).
-- ---------------------------------------------------------------------------
ALTER TABLE users ADD COLUMN IF NOT EXISTS recovery_salt     bytea;
ALTER TABLE users ADD COLUMN IF NOT EXISTS recovery_verifier bytea;

-- ---------------------------------------------------------------------------
-- Pending-device state (§7.5). `state` (0001) remains the §6.5 lifecycle
-- (active|stale|retired); `status` is the ORTHOGONAL authorization gate
-- (pending|active). DEFAULT 'active' keeps the signup self-enrolled first device active
-- and every pre-existing row active; new password-login enrollments insert 'pending'.
-- ---------------------------------------------------------------------------
ALTER TABLE devices
    ADD COLUMN IF NOT EXISTS status text NOT NULL DEFAULT 'active';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'devices_status_check'
    ) THEN
        ALTER TABLE devices
            ADD CONSTRAINT devices_status_check CHECK (status IN ('pending', 'active'));
    END IF;
END
$$;

-- `devices`, `device_roster`, `users` already carry the correct RLS posture from 0001
-- (devices/device_roster FORCE-RLS via yapstack_apply_rls; users NON-RLS identity
-- bootstrap). No new tenant tables are introduced, so no new RLS policy is required.
