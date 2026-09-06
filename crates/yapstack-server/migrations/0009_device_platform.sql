-- SPDX-License-Identifier: AGPL-3.0-only
-- Device platform: the operating system a device reported when it enrolled
-- ("macos" | "windows" | "linux"), so the devices UI can tell devices apart at a
-- glance. Advisory index metadata exactly like `label` — client-supplied, never read
-- by the relay for any decision. Additive; existing rows read as unknown ('').
ALTER TABLE devices ADD COLUMN IF NOT EXISTS platform text NOT NULL DEFAULT '';
