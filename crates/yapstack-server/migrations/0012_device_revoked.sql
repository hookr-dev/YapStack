-- SPDX-License-Identifier: AGPL-3.0-only
-- Device removal (SYNC_SPEC §15.D).
--
-- An approving device may re-sign the roster WITHOUT a member and name it in
-- `revoked_devices` on `PUT /devices/roster`. The relay marks that row `revoked`, revokes
-- its refresh-token families, and the write gate (active callers only) refuses its
-- remaining access token; reads stop when the token expires. A revoked device that signs
-- in again with the password is re-enrolled PENDING and must be approved anew. This is
-- relay-side eviction only: a removed device keeps what it already synced (E2E cannot
-- take that back; retiring its key is vault-key rotation, CRYPTO_SPEC §4.4, deferred).
-- Widens the status CHECK; additive, no row rewrite.
ALTER TABLE devices DROP CONSTRAINT IF EXISTS devices_status_check;
ALTER TABLE devices
    ADD CONSTRAINT devices_status_check CHECK (status IN ('pending', 'active', 'revoked'));
