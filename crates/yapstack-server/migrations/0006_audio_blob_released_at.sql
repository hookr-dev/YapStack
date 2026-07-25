-- SPDX-License-Identifier: AGPL-3.0-only
-- YapStack relay — relay blob GC (hardening item 5).
--
-- Adds the "became unreferenced at" timestamp that the GC sweep (gc.rs) needs to apply a
-- grace period before deleting an audio blob's object + row. `released_at` is set the moment
-- a blob's mapping-count refcount TRANSITIONS to <= 0 (the repoint-away decrement in
-- audio.rs) and cleared (back to NULL) whenever the refcount goes back UP (a new mapping /
-- repoint onto the blob). A blob is GC-eligible only when `refcount <= 0` AND
-- `released_at < now() - grace` (default grace = 7 days).
--
-- STRICTLY ADDITIVE — the empty-tables invariant does NOT apply here: audio_blobs holds real
-- rows on the live relay. This is a nullable column with NO default and NO table rewrite:
--   * ADD COLUMN of a nullable column with no default is a metadata-only change in Postgres
--     (no row rewrite, no long lock on existing data).
--   * Every PRE-EXISTING row keeps `released_at = NULL`, so it is NEVER GC-eligible until a
--     FUTURE refcount decrement (a repoint away) marks it. In particular, any pre-existing
--     refcount<=0 row (if one exists) stays untouched until such a transition — it does not
--     become instantly deletable on deploy. That is the intended conservative behaviour:
--     GC only ever acts on blobs it watched become unreferenced.

ALTER TABLE audio_blobs ADD COLUMN released_at timestamptz;

-- Sweep support: a partial index over the eligible set (refcount already dropped to <= 0),
-- ordered by release time so the per-tenant grace scan is cheap. yapstack_app inherits the
-- table's existing SELECT/UPDATE/DELETE grants for the new column (table-level GRANTs cover
-- future columns), and the new column inherits the table's FORCE ROW LEVEL SECURITY policy.
CREATE INDEX audio_blobs_released_idx
    ON audio_blobs (workspace_id, released_at)
    WHERE refcount <= 0;
