// SPDX-License-Identifier: AGPL-3.0-only
//! Changeset relay (architecture §7): push, pull, completeness/anti-entropy, and the
//! SSE wakeup stream. The relay stores opaque ciphertext and a dense, per-tenant,
//! COMMIT-ORDERED `changeset_seq`; it never decrypts.

pub mod completeness;
pub mod pull;
pub mod push;
pub mod stream;
