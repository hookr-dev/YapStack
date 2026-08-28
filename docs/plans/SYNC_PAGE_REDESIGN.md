# Sync Page Redesign — Plan v1

Grounded in a 10-target OSINT pass (Obsidian LiveSync, Joplin, Bitwarden, Standard
Notes, Syncthing, Nextcloud desktop, Immich, shadcn/ui forms + settings corpus, and
ambient-indicator conventions across Dropbox/OneDrive/iCloud/Notion/Linear/Figma),
verified against primary sources and cross-checked against this repo. Full research
briefs: `docs/plans/research/sync-page-uiux/` (local, gitignored).

Scope: the Settings → Sync tab + a new app-wide ambient sync glyph. Builds on the
T023/T024 backend fields (`phase` incl. `syncing`/`auth_expired`, `pending_entries`,
`pending_bytes`, `acked_this_session`, `last_success`). No relay-side changes required
except none; one Tauri command widens its error type.

---

## 0. Decisions ratified by research (with the one needing owner sign-off)

1. **⚠️ OWNER SIGN-OFF — probe trigger.** The ask was "know if we have a valid
   connection as soon as we input one." Every researched product avoids per-keystroke
   probing (each probe is a network beacon to a user-controlled host; UX literature
   calls keystroke validation "yelling"). **Default adopted here:** an explicit
   **Test connection** button as the guaranteed path, which *also* auto-fires once on
   blur/paste when the URL parses as valid (single-flight, ~600 ms debounce,
   cancel-on-edit, never per-keystroke). Blur/paste ≈ "the moment they finish entering
   a URL." Ratify or override.
2. **No react-hook-form/zod.** Verified: shadcn ships no probe concept; routing a
   network probe through schema validation fights the verbatim-error rule and is
   community-documented fragile. The probe is a store action with its own typed enum.
   Optionally vendor shadcn's presentational `Field`/`FieldError` (zero runtime dep,
   `role="alert"` a11y, accepts a verbatim error string as plain children). Defer
   RHF+zod until a real multi-field form (signup/recovery redesign) forces it.
3. **Version check is advisory, never blocking** (Immich, Nextcloud both converged
   here). Direction corrected against our code: the server publishes
   `min_client_version`, so a mismatch means **"update this app"**, not "update the
   server."
4. **Self-hosted URL stays first-class** (must-preserve) — the *control* stays visible
   in its card; only the *result detail* collapses (LiveSync's collapse-on-success).
   We deliberately diverge from Standard Notes/Bitwarden, which bury the URL.
5. **Failed probe never blocks Save** — "Save anyway" escape hatch (LiveSync's
   "Continue anyway"), because over-strict validation traps self-hosters behind odd
   proxies.
6. **Single adaptive page, not a wizard** — we're a tab inside a Settings dialog with
   a short journey; states become distinct card compositions.
7. **Color discipline:** steady state is muted/monochrome; saturated color only for
   attention (amber) and hard error (red). A green ✓ appears only in the transient
   test-connection result line, never as a durable badge.

## 1. Connection model (Rust + store)

### 1a. Widen the probe command's error type
`sync_info` (apps/desktop/src-tauri/src/sync.rs:1396) currently returns
`Result<SyncInfoDto, String>`, collapsing all failures into one string. Widen to a
typed result. reqwest exposes `.is_timeout()`/`.is_connect()`; rustls surfaces cert
errors distinctly (verify the exact variant during implementation).

```rust
enum SyncProbeError {
  Unreachable { raw: String },     // DNS / refused / timeout (5s budget)
  TlsError    { raw: String },     // cert / handshake
  NotARelay   { raw: String },     // 2xx but JSON missing sentinel fields
  VersionMismatch { min_client_version: String, raw: String }, // advisory
}
```

- **Sentinel check (critical):** a 200 counts as a relay only if the body parses as
  `SyncInfoResponse` with `protocol_version` + `engine_version` present
  (crates/yapstack-server/src/routes.rs:42 — there is **no** `server_version` field;
  earlier drafts inventing "relay v1.4.2" copy are wrong). Otherwise any proxy
  returning 200 reads as "connected" (Nextcloud's `installed` sentinel / Immich's
  `{"res":"pong"}` lesson).
- Probe also measures latency; success payload = `{ engine_version,
  protocol_version, latency_ms }`.
- Normalize before probing: prepend `https://` when schemeless, strip trailing
  slashes. TLS is never silently downgraded — an http retry is an explicit user
  choice.

### 1b. Store: connection state is a separate enum from sync phase
```ts
type RelayConnState =
  | { kind: "idle" } | { kind: "testing" }
  | { kind: "ok"; engineVersion: string; protocolVersion: number; latencyMs: number }
  | { kind: "unreachable" | "not-a-relay" | "tls-error"; raw: string }
  | { kind: "version-mismatch"; minClientVersion: string; raw: string }; // advisory
```
The two-tier rule (Nextcloud/Syncthing): **connection health short-circuits sync
phase**. One derivation helper feeds every surface so they can never disagree:

```
deriveSyncDisplay(conn, phase, pendingEntries, lastSuccess, lastError):
  conn not ok (post-signin)   → "Can't reach relay"        [attention/amber]
  phase == auth_expired       → "Sign in again"            [attention/amber]
  phase == error              → "Sync error"               [destructive]
  phase == syncing            → "Syncing — N remaining"    [active]
  idle && pending > 0         → "N to sync"                [muted]
  idle && pending == 0 && !lastError → "Up to date · synced 3m ago" [muted]
```
"Up to date" is strictly gated on zero pending AND no error (Syncthing #7046;
Nextcloud empty-error-set gate). `last_success` renders relative with absolute time
in a tooltip. No ETA — we have no throughput history; `N remaining · X MB` is honest.

## 2. Page composition — 4 stacked Cards replace the flat Separator stack

```
[pinned verbatim-error Alert when lastError]        (unchanged, must-preserve)

Card: Relay server        CardAction = ConnectionBadge (health enum)
  desc: "Point YapStack at a blind relay. The server never sees your plaintext."
  URL Input + [Test connection]  (+ auto-probe once on blur/paste; §0.1)
  collapse-on-success result (LiveSync <details open={!ok}>):
    ok  → one muted line: "✓ Connected — engine v0.16.3 · protocol v1 · 42 ms"
    err → auto-expanded: distinct icon+copy per kind + verbatim raw (role=alert)
          + [Save anyway]
  signed-in: URL renders read-only as a confirmed identity line (host only);
  editing requires explicit unlock via AlertDialog ("Changing your relay server
  signs out this device and stops syncing. Local data is untouched.")

Card: Account
  signed-out → Sign in / Create account (dialogs unchanged)
  signed-in  → email + this-device fingerprint + Sign out (moved to Advanced)
  auth_expired → warm "Sign in again" CTA (URL stays locked — never conflate
  expired session with wrong server)

Card: Devices (signed-in only)
  pending-approval Alert (attention tone, ceremony UNCHANGED) + roster rows
  (fingerprint mono, this-device/pending badges). Per-device last-seen/version
  are relay-dependent follow-ons — not in v1.

Card: Enable sync (signed-in && !enabled — first-run only)
  "Prepares a copy of your library…" + Enable button
  during initial big sync: determinate <Progress> capped at 99% while
  pending_bytes > 0 (Syncthing "never 100% until done");
  total = pending at session start (acked_this_session + pending_entries)

[steady-state status line when enabled]  ← deriveSyncDisplay text
[Upgrade card — only when billing_url]   (unchanged)
[Advanced Collapsible — subdued, not a red danger zone]
  Sign out · (future: remove device, disable & wipe) — each AlertDialog-gated
```

First-run (signed-out, unconfigured) shows **two cards** (Relay + Account), not the
full stack. No nested sidebar nav, no ContentSection scroll region — those are for
5+ page-level categories.

## 3. App-wide ambient glyph (sidebar settings row)

Rendered from the same `deriveSyncDisplay` output. Distinct silhouettes, not
color-alone; the icon pre-diagnoses the fix (OneDrive rule).

| state | icon (lucide) | tone | motion | tooltip | click |
|---|---|---|---|---|---|
| off (signed-out/disabled) | — (no glyph) or `Cloud` muted | muted | none | "Sync is off" | Settings → Sync |
| caught up | `Cloud` w/ check (CloudCheck if avail; else Cloud) | muted, low-contrast | none | "Up to date · synced 3m ago" | Settings → Sync |
| syncing (gated) | `RefreshCw` | primary, subtle | slow spin | "Syncing — N remaining" | Settings → Sync |
| pending (queued, idle) | `Cloud` + tiny dot | muted | none | "N changes to sync" | Settings → Sync |
| auth expired | `CloudAlert` | amber | none | "Sign in again to sync" | Sync (login) |
| unreachable | `CloudOff` | amber | none | "Can't reach <host>" | Sync (server field) |
| error | `CloudAlert` | destructive | none — never animate an error | "Sync error — needs attention" | Sync (error Alert) |

- **Motion gate:** animate only when `phase == syncing && (pending_entries > N ||
  in-flight > ~2 s)` so drain-cycle blips don't strobe the sidebar (HIG/Linear).
  Note: our capture batches at drain cycles (~5 s), so the strobing risk is bursts,
  not keystrokes — the gate still applies.
- **Never let unreachable render the calm cloud** (Nextcloud #6196 — the #1
  real-world ambient failure). Recompute from live status; never latch (Joplin #3447).
- Escalation ladder: glyph → tooltip → in-panel verbatim Alert → modal only for
  destructive confirmations, never for sync errors (Figma rule).
- Icon availability: verify `CloudAlert`/`CloudCheck` exist in our lucide-react
  version; otherwise compose `Cloud` + badge dot. This table is the **single agreed
  icon map** — sidebar glyph and in-panel badge must both use it.

## 4. Implementation slices (each reversible, each `pnpm check`-gated)

- **T025 — probe backend:** widen `sync_info` to typed errors + latency; sentinel
  check; URL normalization; unit tests per error class. (Rust only.)
- **T026 — status derivation + store:** `RelayConnState` in the zustand store,
  `deriveSyncDisplay` helper + tests; wire probe action (button + one-shot
  blur/paste, single-flight cancel-on-edit).
- **T027 — page recomposition:** the 4-card layout above; Relay card w/
  collapse-on-success result + Save-anyway; locked-URL steady state + unlock
  AlertDialog; sign-out confirm. (Frontend-design workflow applies: read
  docs/FRONTEND.md + docs/PRINCIPLES.md first.)
- **T028 — ambient glyph:** sidebar settings-row indicator per §3 + motion gate +
  click-through to the Sync tab.
- Follow-ons (explicitly out of v1): per-device last-seen/version (needs relay),
  OS tray/dock integration (app already owns a TrayIcon for dictation — decide
  co-habitation separately), fingerprint *presentation* upgrade (Signal/Matrix/
  Keybase-style word/emoji encoding — research gap flagged by the critic),
  recovery-code ceremony UX (1Password Emergency-Kit precedent), "attention"
  third bucket for done-but-N-unmerged.

## 5. Traps the research says to avoid (binding for implementers)

- Don't probe per-keystroke; don't beacon on every blur — one-shot + explicit button.
- Don't route the probe through zod/RHF or any schema validator.
- Don't hard-block on version mismatch; don't silently downgrade TLS.
- Don't let "Up to date" render while unreachable or while lastError is set.
- Don't overload one label for signed-out vs unreachable vs auth-expired vs error.
- Don't animate errors; don't show 100% before truly done; don't fake ETAs.
- Don't add nested settings nav; don't build a loud red danger zone.
- Verbatim error strings always reach the user (must-preserve; `raw` field + Alert).
