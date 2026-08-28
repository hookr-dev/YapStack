// SPDX-License-Identifier: AGPL-3.0-only
#![forbid(unsafe_code)]
//! # yapstack-entitlements
//!
//! The entitlements seam from `docs/plans/ENTITLEMENTS_SEAM.md`. The relay hard-
//! enforces exactly the numbers a [`TenantLimitSource`] returns; the vocabulary is
//! *quantitative only* ([`Limit::Unlimited`] | [`Limit::Max`]) so gating a *feature*
//! is impossible without a public, diffable schema change (decision 6).
//!
//! Two sources ship in-repo and both flow through the same enforcement path:
//! [`AllowAll`] (self-host default — every field unlimited) and [`StoredLimits`]
//! (reads `tenant_limits`; a missing row falls open to the deployment `[limits.default]`,
//! or [`Limit::Unlimited`] when unset). Selection is by config value only — no
//! compiled-in private impl (decision 1/6, AGPL one-artifact covenant).
//!
//! **DTO placement (T007 decision).** The admin PUT / usage GET wire types live in
//! this crate ([`wire`]), because ENTITLEMENTS_SEAM.md makes the admin+usage OpenAPI
//! part of *this* seam contract. The sync-protocol DTOs (changeset push/pull) remain
//! destined for `yapstack-common` per architecture §14 and are a T008 deliverable.

pub mod source;
pub mod types;
pub mod wire;

/// Tenant identity. Tenant = workspace from the first migration (decision 5); a solo
/// user is a degenerate 1-member workspace, so `tenant_id == workspace_id` everywhere.
pub type TenantId = uuid::Uuid;

pub use source::{AllowAll, LimitDefaults, StoredLimits, TenantLimitSource};
pub use types::{Limit, TenantLimits, TenantState, WarnThresholds};
