-- SPDX-License-Identifier: AGPL-3.0-only
-- Proof of possession at login (CRYPTO_SPEC §3.2 amendment A10, SYNC_SPEC §15.K).
--
-- `POST /auth/login/begin` issues a random 32-byte nonce bound to the (lower-cased) email;
-- `login/finish` and `recover` consume it with an Ed25519 signature by the DEVICE key over
-- `LP("yapstack.login.pop.v1", nonce, client_id, ed25519_pub, email_lower)`, which the relay
-- verifies against the STORED public key when the device row exists (so a password holder
-- can no longer mint a token for another device's `client_id`) and against the presented
-- key when the device is new. Pre-tenant (login runs before any tenant context), so this
-- table is NOT RLS-scoped, like `users`. Rows are single-use (deleted on consume) and
-- pruned after 10 minutes; a nonce is valid for 5.
CREATE TABLE login_nonces (
    nonce        bytea PRIMARY KEY,
    email_lower  text NOT NULL,
    issued_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX login_nonces_issued_idx ON login_nonces (issued_at);
GRANT SELECT, INSERT, DELETE ON login_nonces TO yapstack_app;

-- Pre-A10 refresh families were minted WITHOUT proof of possession: a password holder
-- could have bound one to another device's client_id. Revoke every device-bound family so
-- each device signs in again — with proof — on its next refresh. Clients already handle a
-- revoked family (the drain lands on "Sign in again").
UPDATE refresh_tokens SET revoked = true WHERE client_id IS NOT NULL AND NOT revoked;
