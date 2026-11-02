# Release runbook

How to cut and ship a YapStack release. Owned end-to-end by a maintainer with access to the `release` GitHub Actions environment and the Tauri minisign signing secret.

## What a release is

A release is a git tag of the form `vX.Y.Z` (or `vX.Y.Z-prerelease`) on `main`. Pushing the tag triggers [`.github/workflows/release.yml`](../.github/workflows/release.yml), which:

1. Waits for the CI Quality Checks (Rust Format, Rust macOS, Rust Windows, Frontend) on the tagged commit and stops the release unless all of them passed. The tag must point at a commit that was pushed to `main`.
2. Builds the macOS Apple Silicon target with code signing + notarization, and the Windows x64 NSIS installer. Both include the `sync` feature.
3. Generates `latest.json` (Tauri auto-updater manifest) referencing the signed macOS and Windows updater bundles.
4. Creates a **draft** GitHub Release with the DMG, the Windows installer, the updater bundles + minisign signatures, `latest.json`, and SHA-256 checksums attached.

The draft is the maintainer's last review gate. Nothing ships to existing installs until the draft is published — `latest.json` only gets served from the release URL once the draft is no longer a draft.

## Versioning

- **Semantic versioning.** `MAJOR.MINOR.PATCH`, with `-alpha.N` / `-beta.N` / `-rc.N` prereleases.
- We are currently on the `1.0.0-alpha.N` series. Bump `N` for each alpha cut. Drop the `-alpha.N` suffix only when promoting to `1.0.0`.
- The Tauri minisign **public key** in [`apps/desktop/src-tauri/tauri.conf.json`](../apps/desktop/src-tauri/tauri.conf.json) is **never** rotated as part of a release. Rotating it invalidates the auto-updater for every existing install.

## Files that carry a version

When bumping the release version, edit all of:

| File | What to change |
|---|---|
| [`apps/desktop/src-tauri/Cargo.toml`](../apps/desktop/src-tauri/Cargo.toml) | `version = "X.Y.Z"` (top of file) |
| [`apps/desktop/src-tauri/tauri.conf.json`](../apps/desktop/src-tauri/tauri.conf.json) | `"version": "X.Y.Z"` |
| [`Cargo.lock`](../Cargo.lock) | Auto-updates on the next `cargo` invocation; commit the resulting diff |
| [`CHANGELOG.md`](../CHANGELOG.md) | Rename `## [Unreleased]` → `## [X.Y.Z] - YYYY-MM-DD` and start a fresh empty `[Unreleased]` block above it |

`apps/desktop/package.json` carries an unrelated `"version"` field that does **not** track release versions. Do not touch it as part of a release cut.

## Cutting a release

This is the path used for every release the project has shipped so far. PR title convention is `chore: prepare vX.Y.Z` (see commits `8891757`, `e302676`, `709ceee`, `5205d8a` for prior cuts).

1. **Confirm `main` is green.** Check that CI passed on the `main` commit you will tag (the release workflow refuses to build otherwise), then `git pull origin main && pnpm check` locally. CI doesn't cover everything `pnpm check` does on your machine, so run both.
2. **Create the prep branch.** `git checkout -b chore/prepare-vX.Y.Z`.
3. **Bump the version files** (table above). Run `cargo build -p yapstack-desktop` once to let `Cargo.lock` regenerate.
4. **Roll the CHANGELOG.**
   - Cross-reference each `[Unreleased]` entry with the PR it came from (`(#NN)` at end of line). Use `git log --oneline TAG..HEAD` to enumerate.
   - Rename the section header from `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` (today's date, ISO 8601).
   - Insert a fresh empty `## [Unreleased]` block above the new dated section.
5. **Sweep the docs.** For any user-visible feature in this release, check whether the relevant doc still accurately describes the surface:
   - [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — for cross-cutting changes (data flow, IPC, state).
   - [`docs/API_REFERENCE.md`](API_REFERENCE.md) — for added/removed/renamed Tauri commands or struct fields.
   - [`docs/GLOSSARY.md`](GLOSSARY.md) and [`docs/UBIQUITOUS_LANGUAGE.md`](UBIQUITOUS_LANGUAGE.md) — for new domain terms.
   - [`docs/FRONTEND.md`](FRONTEND.md) — for new UI surfaces, shortcuts, or patterns.
   - [`docs/IMPLEMENTATION_LOG.md`](IMPLEMENTATION_LOG.md) — append a new `## Phase N — <title>` entry covering "what was built / bugs being addressed / key decisions / files changed / what was learned." This is the project's design-history record; future-you will read it to understand *why*.
   - [`README.md`](../README.md) — only if install / platform support / first-run flow changed. The alpha warning is intentionally generic — leave it alone.
6. **Open the prep PR.** Title `chore: prepare vX.Y.Z (#NN)`. Description should at minimum:
   - Name the user-visible PRs folded into this cut (with `#NN` links).
   - Confirm `pnpm check` passed.
   - Call out any docs that were swept.
7. **Merge to `main`.** Standard squash merge.
8. **Tag and push.**
   ```bash
   git checkout main
   git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
   The push triggers the release workflow. **Tag-pushing is the trigger; do not also create the GitHub Release manually.**
9. **Watch the workflow.** `gh run watch` on the latest release run. It takes ~15–25 min on macos-latest including signing + notarization. If it fails, fix forward (do not delete the tag — see "Hotfix and recovery").
10. **Review the draft release.** When the workflow completes, a draft Release exists for the tag with the DMG, the `.app.tar.gz` + `.sig`, `latest.json`, and `checksums-sha256.txt` attached. Open it and:
    - Verify the auto-generated release notes look right (GitHub generates them from PRs since the previous tag). Edit prose into the body — the CHANGELOG entry for this version is the source of truth; copying it in is encouraged.
    - Sanity-check that `latest.json`'s `version` matches the tag and the URL points at the uploaded `.app.tar.gz`.
    - Download the DMG, install it on a clean macOS Apple Silicon machine, and exercise the golden path (start a session, dictation, system audio capture, verify auto-update points at the new version).
11. **Publish the draft.** From the GitHub Releases UI, uncheck "Set as a pre-release" only if this is a stable (non-alpha/beta/rc) release; otherwise leave it as a pre-release. Click **Publish release**. Existing installs running the auto-updater will pick up `latest.json` on their next check.

## Releases that add a synced table

A peer that receives rows for a synced table it doesn't have yet must hold them in its schema quarantine, not stop syncing (ADR 0006 §4, SYNC_SPEC §6.4). Builds before that tolerance existed stall their pull on such a row. So **no release may introduce a new synced table (a `SYNC_TABLES` entry) unless the previous release already contained the table-readiness quarantine.** Adding one also needs the peer capability gate in ADR 0006 §5.

## Verification before publish

Don't skip these — the auto-updater silently pushes whatever `latest.json` references to every install.

- [ ] DMG installs cleanly on macOS Apple Silicon and launches.
- [ ] First-run permission prompts (microphone, screen recording for system audio) still surface correctly.
- [ ] A session records, transcribes, and saves; the WAV plays back.
- [ ] Dictation works (mic-only, with and without volume ducking if enabled).
- [ ] Auto-updater check from the previous version → this version succeeds end-to-end (pre-publish: side-load the new build via DMG; post-publish: verify the in-app updater can step from the previous tag's install).
- [ ] `latest.json` `signature` field is non-empty (means `TAURI_SIGNING_PRIVATE_KEY` was wired correctly).
- [ ] `checksums-sha256.txt` matches what `shasum -a 256 *.dmg` reports locally on the downloaded artifact.

## Hotfix and recovery

- **The build failed mid-workflow.** Push a fix-forward commit to `main` and re-tag with the *next* patch number — don't reuse the failed tag. The previous failed run produced no artifacts, so there's nothing to clean up.
- **The build succeeded but the draft is wrong** (notes typo, missing asset, regression caught during review). Edit the draft directly. Do **not** publish until corrected. If the artifact is bad, delete the draft, fix forward, re-tag.
- **A bad release was published.** Pull the release back to "draft" or delete it. Cut a new patch release immediately — `latest.json` only points at the most-recent published release, so the auto-updater will steer existing installs to the fix on their next check (24h cadence by default). Note in the new release notes that the prior version was withdrawn.
- **Never delete a published tag.** Even a withdrawn release keeps its tag for forensic history. Deleting tags breaks `git describe`, the changelog cross-references, and confuses anyone bisecting an issue.
- **Never `git push --force` to `main`** as part of a release.

## Canary and test builds

Two manual workflows produce installers without cutting a release:

- **Release → Run workflow** on a branch builds only the Windows installer (the Windows canary).
- **Sync Test Build → Run workflow** builds macOS and/or Windows from any ref.

Both run in the `canary` environment, **without** the Tauri updater signing key, and pass `--config src-tauri/tauri.no-updater.conf.json` so no updater artifacts are produced. Install them by hand; they can't be served to existing installs. A signed + notarized macOS test build (`mac_signed`) needs the `release` environment, so it only runs from refs that environment allows.

## Required secrets

Configured as **environment secrets on the `release` GitHub Actions environment**, not as repository secrets: repository secrets are readable by every workflow on every branch. Restrict the environment's deployment refs to `main` and `v*` tags. Do not commit these or echo them in build logs.

| Secret | Purpose |
|---|---|
| `APPLE_CERTIFICATE` | Base64 of the Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | Password for the `.p12` |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID` | Apple ID for notarization submission |
| `APPLE_PASSWORD` | App-specific password (not the Apple ID password) |
| `APPLE_TEAM_ID` | Apple Developer team ID |
| `TAURI_SIGNING_PRIVATE_KEY` | Tauri minisign **private** key (paired with the public key in `tauri.conf.json`) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password for the minisign private key |
| `APTABASE_KEY` | Analytics key embedded in the build. Canary and test builds use it too, so it can stay a repository secret. |

If any of these are unset or invalid, the workflow will fail at the signing or notarization step. The fix is in repo settings — not in the workflow file.

## Public-facing copy

CHANGELOG entries, release notes, and any in-app onboarding copy that ships with the release count as **public-facing copy** under [`AGENTS.md` § Permission boundaries](../AGENTS.md#ask-first). A human writes these. Tooling can suggest, but the release-cut commit's copy is on the maintainer's name.
