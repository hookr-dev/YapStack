<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# YapStack Licensing & Covenant

> **STATUS: RATIFIED.** Ratified by the project owner (2026-07-27) as the project's
> intended policy, together with the accompanying `TRADEMARK.md` and `DCO`
> (ENTITLEMENTS_SEAM.md decision 10). This document is not legal advice; independent
> counsel review is recommended before launching a commercial hosted service or
> filing trademark registration.

YapStack is **free and open-source software licensed under
[`AGPL-3.0-only`](LICENSE)**. Every crate and package in this repository inherits that
license (`[workspace.package] license = "AGPL-3.0-only"`, `package.json`, root
`LICENSE`). There is **no `ee/` directory, no dual-licensed file, and no compile-time
"enterprise" variant** — one public repository, one artifact.

This document is a **covenant**: a good-faith, durable statement of how we will and
will not build the commercial side of YapStack. It exists so that self-hosters,
contributors, and forkers can rely on the shape of the project.

## 1. Limits only, never features

The hosted service and the open-source relay enforce **quantitative limits only**
(storage bytes, upload bytes per period, share count, device count). The limit
vocabulary in `crates/yapstack-entitlements` is **`Limit::Unlimited | Limit::Max(u64)`
and nothing else** — there is no feature flag, allowlist, or capability gate in the
type system. Gating a *feature* would therefore require a public, diffable change to
this repository, which we pledge not to make. **No feature is ever withheld from the
open-source build.**

## 2. Self-host is feature-complete and unlimited, forever

A self-hosted YapStack relay is **the maximum tier**. When the optional `[limits]`
configuration section is absent — the default — every limit resolves to `Unlimited`
and the tenant state is always `Active`. Self-hosters get the complete product,
including open metering (the usage endpoint), with no artificial caps. We will never
ship a change that makes self-host a degraded tier.

## 3. Billing lives in a separate program

Payment, plan catalogs, prices, dunning, and provisioning live in a **separate,
private control-plane program** (`yapstack-cloud`) that communicates with this relay
only over a **published HTTP admin contract** (see `crates/yapstack-entitlements`
OpenAPI). The relay has **zero runtime dependency** on that control plane: if the
control plane is down, tenants keep their last-known limits. No commercial endpoint,
plan name, or price is hardcoded in this repository.

## 4. Zero phone-home

A self-hosted relay makes **no outbound network calls except to the object storage
and database the operator configures** (its own infrastructure). It does not check
in, does not fetch entitlements, does not report telemetry, and cannot be remotely
disabled. Limits are **pushed in** to the relay's own database by the operator's
control plane; the relay never reaches out.

## 5. No removal of shipped open-source surface

Once a capability ships in the open-source relay, sync runtime, or clients, we will
**not remove it or move it behind the commercial control plane** in a later release.
The open surface only grows.

## 6. AGPL-3.0-only, DCO, no CLA, no rug-pull

Contributions are accepted under the **Developer Certificate of Origin 1.1** (see
[`DCO`](DCO)) — **there is no Contributor License Agreement.** Because we do **not**
collect copyright assignment or a relicensing grant, **we cannot unilaterally
relicense contributors' work under a proprietary license.** This is a structural
no-rug-pull guarantee: the project stays AGPL-3.0-only. Forks of the server (or any
crate) share their modifications back under AGPL, including the network-use clause
(§13).

## 7. The hosted binary is this repository, at a digest

The hosted YapStack relay runs an image **built by public CI from a public git tag**.
The running build digest is exposed at `GET /version`. We publish this as an
operational **binary-transparency covenant** (public tags, digest in `/version`, no
unpublished patches to the relay), **never as a security proof.** The security claim
stands on the protocol itself: the server is assumed hostile and still cannot read
your content, by construction (see `docs/CRYPTO_SPEC.md`). Reproducible builds and
attestation are welcome future work, not a precondition of this covenant.

## 8. What this is not

This covenant governs the relay and clients in this repository. It does not license
the YapStack **name or marks** — see [`TRADEMARK.md`](TRADEMARK.md). It is not a
warranty; the software is provided under the AGPL "as is."
