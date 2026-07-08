// YapStack Sync — frontend IPC seam + pure display logic.
//
// The crypto ceremonies (Argon2id stretch, envelope wrapping, roster signing)
// and the vault key at rest run in Rust (`yapstack-crypto` + OS keychain), NOT
// in the webview: keeping the master/vault keys out of the JS heap and out of
// localStorage is the whole point (CRYPTO_SPEC §10, arch §11.1). This module is
// the thin, hand-typed `invoke` boundary the Rust `sync` commands implement,
// plus the pure formatting helpers the key-management UI renders.
//
// The commands are hand-typed here (not via the Specta bindings in
// `lib/types.ts`) on purpose: the Rust `sync` integration ships behind an
// off-by-default cargo feature (it links the pinned cr-sqlite CRR bundle, which
// needs nightly + panic=abort). Typing the boundary by hand keeps the frontend
// compiling and testable independent of that feature build. When the feature is
// enabled the Rust command names/shapes below are the contract both sides meet.

import { invoke } from "@tauri-apps/api/core";

/** Hosted relay default. Users may paste a self-host URL instead. */
export const DEFAULT_SYNC_SERVER_URL = "https://sync.yapstack.app";

// ----- Wire types (contract with the Rust `sync` command surface) -----

/** `GET /sync/info` echo. `billingUrl` is present ONLY when the deployment
 *  advertises a control plane (hosted); self-host omits it and the UI shows no
 *  upgrade affordance (ENTITLEMENTS_SEAM §client-billing-discovery). */
export interface SyncInfo {
  serverUrl: string;
  version: string;
  billingUrl: string | null;
}

/** `sync_probe` success (T025). A version gap rides here as `versionAdvisory` — it is
 *  advisory ("update this app"), never a probe failure. */
export interface RelayProbeOk {
  engineVersion: string;
  protocolVersion: number;
  /** Elapsed request→response head, milliseconds. */
  latencyMs: number;
  /** The URL actually probed after normalization (scheme prepended when schemeless,
   *  trailing slashes stripped) — echo/persist this, not the raw input. */
  normalizedUrl: string;
  /** Present ONLY when this client is behind the relay's published minimum. */
  versionAdvisory: RelayVersionAdvisory | null;
}

export interface RelayVersionAdvisory {
  /** Minimum client version the relay publishes (§0.3). */
  minClientVersion: string;
  /** Verbatim advisory line to surface. */
  raw: string;
}

/** Typed `sync_probe` failure. The `invoke` promise REJECTS with this object (not the ok
 *  payload); `kind` discriminates and `raw` is always the verbatim detail, surfaced to the
 *  user. `unreachable` also absorbs TLS errors that rustls cannot distinguish from a plain
 *  connect failure (the verbatim chain still rides in `raw`). */
export type RelayProbeError =
  | { kind: "unreachable"; raw: string }
  | { kind: "tls-error"; raw: string }
  | { kind: "not-a-relay"; raw: string };

/**
 * Store-side connection health for the relay URL field — a SEPARATE axis from
 * the sync `phase` (§1b two-tier rule: connection health short-circuits sync
 * phase in `deriveSyncDisplay`). Driven only by explicit probes today:
 *
 *   idle     — never probed / field just edited (reset).
 *   testing  — a probe is in flight.
 *   ok       — reachable relay; carries the T025 success payload. A version gap
 *              rides as `versionAdvisory` (advisory, still saveable) — it is NOT
 *              a failure kind (the §1b sketch predates T025; T025's shape wins).
 *   unreachable / tls-error / not-a-relay — the three typed probe failures, each
 *              with the verbatim `raw` chain (must-preserve).
 */
export type RelayConnState =
  | { kind: "idle" }
  | { kind: "testing" }
  | {
      kind: "ok";
      engineVersion: string;
      protocolVersion: number;
      latencyMs: number;
      normalizedUrl: string;
      versionAdvisory: RelayVersionAdvisory | null;
    }
  | { kind: "unreachable"; raw: string }
  | { kind: "tls-error"; raw: string }
  | { kind: "not-a-relay"; raw: string };

export type SyncPhase =
  | "disconnected"
  | "connecting"
  | "connected"
  | "preparing" // crr_migrate running: "preparing your library for sync"
  | "syncing" // a push is in flight — unacked entries remain in the outbox (T024)
  | "auth_expired" // session expired; the drain stopped and needs a fresh sign-in (T023)
  | "unreachable" // drain hit a typed transport connectivity failure — relay unreachable (R3)
  | "error";

export interface DeviceRosterEntry {
  /** `SHA-256(ed25519_pub)` → base32 4×4, per CRYPTO_SPEC §7.5.5. */
  fingerprint: string;
  /** True for the device this app instance is running on. */
  isSelf: boolean;
  /** Pending devices are awaiting approval by an existing device. */
  pending: boolean;
  label: string | null;
}

export interface SyncStatus {
  phase: SyncPhase;
  serverUrl: string;
  /** Signed-in account email, null when signed out. */
  email: string | null;
  /** This device's fingerprint (base32 4×4), null before first enrollment. */
  deviceFingerprint: string | null;
  /** Signed device roster with per-entry pending/self flags. */
  roster: DeviceRosterEntry[];
  /** `vault_key_epoch` of the current signed roster (§7.4 anti-rollback). */
  vaultKeyEpoch: number | null;
  /** Roster fingerprint for out-of-band epoch comparison (§7.5.5). */
  rosterFingerprint: string | null;
  /** True once crr_migrate has run and the drain is live. */
  syncEnabled: boolean;
  /** Last connection / auth error surfaced verbatim (never auto-routed). */
  lastError: string | null;
  billingUrl: string | null;
  /** Unacked outbox entries still to push (0 == up to date). Drives the
   *  "Syncing — N remaining" indicator (T024). */
  pendingEntries: number;
  /** Total ciphertext bytes of those unacked entries (base64 upload is ~4/3). */
  pendingBytes: number;
  /** Entries acked since the current drain thread started (this session). */
  ackedThisSession: number;
  /** RFC3339 of the last fully-drained-and-reachable moment; null before the
   *  first successful drain. Rendered relative to now ("synced 2m ago"). */
  lastSuccess: string | null;
}

export interface SignupRequest {
  serverUrl: string;
  email: string;
  password: string;
}

/** Returned ONCE from signup. The recovery code is shown exactly once and the
 *  UI MUST force the user to record it before proceeding (§6.1). */
export interface SignupResult {
  /** `AAAA-BBBB-…` 8×4 base32 recovery code (§6.1). Never persisted in JS. */
  recoveryCode: string;
  deviceFingerprint: string;
}

export interface LoginBeginResult {
  /** True when the served `salt_enc` differs from this known device's cached
   *  baseline — a hostile-relay salt-substitution signal (§3.2 C3). The UI
   *  MUST refuse to proceed and surface the alert. */
  saltMismatch: boolean;
}

// ----- IPC boundary (implemented by the Rust `sync` feature) -----

export const syncCommands = {
  /** Typed relay probe (T025): reachability / TLS / not-a-relay are distinct classes and a
   *  version gap is advisory metadata on success. Normalizes the URL, enforces a 5s budget,
   *  and applies the 2xx-sentinel check (protocol_version + engine_version) so a bare proxy
   *  200 never reads as connected. Resolves with `RelayProbeOk`; REJECTS with a
   *  `RelayProbeError` the caller discriminates on `kind`. This is the only relay-metadata
   *  call; `billingUrl` rides on `sync_status`. */
  probe: (serverUrl: string): Promise<RelayProbeOk> =>
    invoke("sync_probe", { serverUrl }),

  status: (): Promise<SyncStatus> => invoke("sync_status"),

  /** Create account: derive keys, generate + wrap the vault key, store it in the
   *  OS keychain, self-enroll the epoch-0 roster (§3.2 C2). Returns the
   *  one-time recovery code for forced capture. */
  signup: (req: SignupRequest): Promise<SignupResult> =>
    invoke("sync_signup", { req }),

  /** Round 1 of two-round login (§3.2): send email, fetch `salt_enc`, run the
   *  known-device salt-mismatch check. */
  loginBegin: (serverUrl: string, email: string): Promise<LoginBeginResult> =>
    invoke("sync_login_begin", { serverUrl, email }),

  /** Round 2: derive `auth_key`, authenticate, unwrap the vault key, bootstrap
   *  the roster. New devices land `pending` until approved (§7.5). */
  loginFinish: (password: string): Promise<SyncStatus> =>
    invoke("sync_login_finish", { password }),

  /** Recover access with the base32 recovery code when the password is lost. */
  recover: (
    serverUrl: string,
    email: string,
    recoveryCode: string,
  ): Promise<SyncStatus> =>
    invoke("sync_recover", { serverUrl, email, recoveryCode }),

  /** Enable sync: run crr_migrate ONCE against a `.backup` COPY of the live DB,
   *  gated on SYNC_SCHEMA_VERSION, then start the drain. Long-running;
   *  drives the "preparing your library for sync" state. */
  enable: (): Promise<SyncStatus> => invoke("sync_enable"),

  /** Approve a pending device by fingerprint: bump the roster counter, re-assert
   *  the live `vault_key_epoch`, re-sign, upload (§7.5.3). */
  approveDevice: (fingerprint: string): Promise<SyncStatus> =>
    invoke("sync_approve_device", { fingerprint }),

  signOut: (): Promise<void> => invoke("sync_sign_out"),
};

// ----- Pure display / parsing helpers (unit-tested, no Tauri) -----

/**
 * True only when the deployment advertised a billing control plane. The upgrade
 * UI is gated on this so self-host builds never show a "buy" affordance
 * (ENTITLEMENTS_SEAM: OSS/self-host advertises no billing_url).
 */
export function shouldShowUpgrade(info: Pick<SyncInfo, "billingUrl">): boolean {
  return typeof info.billingUrl === "string" && info.billingUrl.length > 0;
}

/**
 * Normalize user-entered recovery-code / fingerprint input: strip hyphens and
 * whitespace, uppercase. Base32 alphabet is case-insensitive (§6.1).
 */
export function normalizeCode(input: string): string {
  return input.replace(/[\s-]/g, "").toUpperCase();
}

/**
 * Group an already-base32 string into `groups` blocks of 4 joined by `-`.
 * Recovery code → 8 groups (32 chars, §6.1); device/roster fingerprint → 4
 * groups (16 chars, §7.5.5). Extra trailing chars (defensive) are appended
 * as a final short group rather than dropped.
 */
export function groupBase32(base32: string, groups: number): string {
  const clean = normalizeCode(base32);
  const wanted = clean.slice(0, groups * 4);
  const parts: string[] = [];
  for (let i = 0; i < wanted.length; i += 4) {
    parts.push(wanted.slice(i, i + 4));
  }
  const rest = clean.slice(groups * 4);
  if (rest) parts.push(rest);
  return parts.join("-");
}

/** Recovery code display grouping: 8 groups of 4 (§6.1). */
export function formatRecoveryCode(base32: string): string {
  return groupBase32(base32, 8);
}

/** Device / roster fingerprint display grouping: 4 groups of 4 (§7.5.5). */
export function formatFingerprint(base32: string): string {
  return groupBase32(base32, 4);
}

/**
 * Validate that a normalized recovery-code entry is a plausible 32-char base32
 * string (RFC 4648 uppercase alphabet). Used by the recovery-confirm step to
 * gate the "I saved it" flow before we let the user proceed.
 */
export function isValidRecoveryCode(input: string): boolean {
  const clean = normalizeCode(input);
  return clean.length === 32 && /^[A-Z2-7]+$/.test(clean);
}

/** True when a pasted/typed server URL is a usable http(s) origin. */
export function isValidServerUrl(url: string): boolean {
  try {
    const u = new URL(url.trim());
    return u.protocol === "http:" || u.protocol === "https:";
  } catch {
    return false;
  }
}

/**
 * Compact "N items remaining" line for the in-flight push indicator (T024). The
 * byte figure is folded in only once it is meaningfully large so the line reads
 * naturally for a small handful of changes ("Syncing — 2 items remaining") vs a
 * big initial sync ("Syncing — 137 items remaining · 68.0 MB").
 */
export function formatSyncProgress(
  pendingEntries: number,
  pendingBytes: number,
): string {
  const noun = pendingEntries === 1 ? "item" : "items";
  const head = `${pendingEntries} ${noun} remaining`;
  // Only append a size once it clears ~1 MiB — below that it is noise.
  if (pendingBytes >= 1024 * 1024) {
    return `${head} · ${formatBytes(pendingBytes)}`;
  }
  return head;
}

/** Human byte size (MB/GB) for the sync backlog line. Base-1024, one decimal. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 MB";
  const mb = bytes / (1024 * 1024);
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * Relative "last synced" phrasing from an RFC3339 timestamp, for the "Up to date"
 * line. Returns "just now" under a minute, then minutes / hours / days. `null`
 * (never synced) yields an empty string so the caller can omit the suffix.
 */
export function formatLastSynced(iso: string | null, now: number = Date.now()): string {
  if (!iso) return "";
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "";
  const secs = Math.max(0, Math.round((now - then) / 1000));
  if (secs < 60) return "just now";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.round(hours / 24);
  return `${days}d ago`;
}
