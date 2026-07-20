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
  | "catching_up" // outbox drained, but the PULL is still behind the relay tip (R12)
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
  /** R12: changesets still to PULL to reach the last-known relay tip (0 when caught
   *  up or the tip is unknown). Drives the "catching up (N to go)" copy. A device is
   *  "up to date" only when this is 0 AND `pendingEntries` is 0. */
  pullBehind: number;
  /** S2 — audio-upload lane, DISTINCT from changeset sync. Recordings still to
   *  seal+upload across both priorities (0 == every local recording is backed up). */
  audioUploadOutstanding: number;
  /** Of `audioUploadOutstanding`, the low-priority backfill of the existing library. */
  audioBackfillOutstanding: number;
  /** Audio blobs that failed to upload (needs attention; app-start + manual retry). */
  audioUploadFailed: number;
  /** Cumulative recordings the relay confirms it holds for this device. */
  audioUploadedTotal: number;
  /** Whether the one-time idempotent backfill walk has completed on this device. */
  audioBackfillComplete: boolean;
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

  /** S2 manual-retry for the audio-upload lane: re-arm every `failed` blob so the
   *  uploader retries it next cycle. Resolves with the count re-armed. */
  retryFailedAudioUploads: (): Promise<number> =>
    invoke("audio_retry_failed_uploads"),

  /** S3 fetch-on-demand: resolve a missing part in D2 order (cache → fetch), joining the
   *  single in-flight download (one per part; concurrent views coalesce). Poll this while a
   *  track is fetching; it returns progress or a terminal state. `NotOnServer` stays the
   *  honest "audio is on <device>" case. */
  prepareAudioPart: (partId: string): Promise<AudioPartPrepare> =>
    invoke("audio_prepare_part", { partId }),

  /** S3: cancel an in-flight fetch AND reset the part's slot (also the retry seam — a
   *  subsequent `prepareAudioPart` starts a fresh download). */
  cancelAudioPart: (partId: string): Promise<void> =>
    invoke("audio_cancel_part", { partId }),

  /** S3.5 navigate-away: drop the part ONLY if it is still admission-queued; an in-flight
   *  download completes into the cache (bounded by the global cap). Resolves with whether
   *  a queued entry was dropped. */
  releaseAudioPart: (partId: string): Promise<boolean> =>
    invoke("audio_release_part", { partId }),

  /** Fetched-audio cache footprint (bytes + file count) for the sync panel row. */
  audioCacheStats: (): Promise<AudioCacheStats> => invoke("audio_cache_stats"),

  /** Clear the fetched-audio cache (skips in-flight fetches and their temps — never
   *  corrupts a running download; source audio is never touched). Resolves with the
   *  remaining footprint. */
  audioCacheClear: (): Promise<AudioCacheStats> => invoke("audio_cache_clear"),
};

/** Fetched-audio cache footprint as reported by `audio_cache_stats` / `audio_cache_clear`. */
export interface AudioCacheStats {
  bytes: number;
  files: number;
}

/**
 * S3 dictation fold-in — FIRE-AND-FORGET enqueue of a saved dictation's WAV onto the upload
 * queue. Dictation audio has no `session_audio_parts` row; the part identity is the
 * `dictation_history.id` itself. Mirrors {@link enqueueAudioForSession}: durable + idempotent
 * + backfill-healed, so we never await or surface failures (absent on no-sync builds).
 */
export function enqueueAudioForDictation(dictationId: string): void {
  if (!dictationId) return;
  void invoke("audio_enqueue_dictation", { dictationId }).catch(() => {});
}

// ----- S3 fetch-on-demand playback state machine (pure, unit-tested) -----

/** Per-part fetch state as reported by the Rust `audio_prepare_part` command (poll). */
export type AudioPartPrepare =
  | { state: "ready"; path: string }
  | { state: "fetching"; received: number; total: number }
  // Admitted but waiting for a global download permit (cap 2, FIFO).
  | { state: "queued" }
  | { state: "not_on_server" }
  | { state: "unreachable" }
  // The cache volume positively reported insufficient space; `needed` = clean-disk budget.
  | { state: "no_space"; needed: number }
  | { state: "verification_failed" }
  | { state: "error"; message: string };

/**
 * Aggregate track-level fetch state (a track = the ordered parts of ONE session/dictation).
 * Playback is all-or-nothing (global-time seeking spans every part), so the track is only
 * `ready` when every missing part is cached. Distinct honest terminals, never silent.
 */
export type TrackFetch =
  | { kind: "ready" } // all (missing) parts are now local — build the player
  | {
      kind: "fetching";
      percent: number;
      received: number;
      total: number;
      /** 1-based ordinal (among the missing parts) of the first in-flight part — the K in
       *  "Fetching part K of M". */
      currentPart: number;
      /** Total missing parts — the M. */
      totalParts: number;
    }
  | { kind: "queued" } // admitted, no part started yet (global cap busy)
  | { kind: "on_device" } // at least one part has no server copy — can't assemble YET (auto re-probed)
  | { kind: "unreachable" } // can't reach the sync server
  | { kind: "no_space"; needed: number } // disk precheck: not enough space on the cache volume
  | { kind: "verification_failed" } // a part failed to decrypt/verify (tamper signal)
  | { kind: "error"; message: string };

/**
 * Reduce the per-part prepare states (of the MISSING parts only, in part_index order —
 * locally-present parts are excluded by the caller) into one track state. Precedence puts
 * the states that BLOCK assembly first: verification failure → unreachable → no_space →
 * generic error → still fetching → queued → any part with no server copy (`on_device`).
 * `on_device` ranks BELOW fetching/queued (auto-fetch S3.5): a part the server lacks is
 * re-probed on a slow cadence while the rest keep downloading, and the fetch self-starts
 * when the source device's upload lands — so mid-download progress stays the honest
 * headline. An empty input means nothing is missing, i.e. `ready`. Pure so the player and
 * its tests share one source of truth.
 */
export function deriveTrackFetch(parts: AudioPartPrepare[]): TrackFetch {
  if (parts.length === 0) return { kind: "ready" };
  if (parts.some((p) => p.state === "verification_failed")) {
    return { kind: "verification_failed" };
  }
  if (parts.some((p) => p.state === "unreachable")) return { kind: "unreachable" };
  const noSpace = parts.find((p) => p.state === "no_space");
  if (noSpace && noSpace.state === "no_space") {
    return { kind: "no_space", needed: noSpace.needed };
  }
  const err = parts.find((p) => p.state === "error");
  if (err && err.state === "error") return { kind: "error", message: err.message };
  const firstFetching = parts.findIndex((p) => p.state === "fetching");
  if (firstFetching >= 0) {
    let received = 0;
    let total = 0;
    for (const p of parts) {
      if (p.state === "fetching") {
        received += Math.max(0, p.received);
        total += Math.max(0, p.total);
      }
    }
    const percent =
      total > 0 ? Math.min(100, Math.max(0, Math.round((received / total) * 100))) : 0;
    return {
      kind: "fetching",
      percent,
      received,
      total,
      currentPart: firstFetching + 1,
      totalParts: parts.length,
    };
  }
  if (parts.some((p) => p.state === "queued")) return { kind: "queued" };
  // No fetch in flight and a part has no server copy: waiting for the source device's
  // upload (the caller re-probes on the ON_DEVICE_REPROBE_MS cadence).
  if (parts.some((p) => p.state === "not_on_server")) return { kind: "on_device" };
  return { kind: "ready" };
}

/** "Fetching 42%" / "Fetching…" copy for the in-progress bar. */
export function formatFetchProgress(percent: number): string {
  if (!Number.isFinite(percent) || percent <= 0) return "Fetching…";
  return `Fetching ${Math.min(100, Math.round(percent))}%`;
}

/**
 * Aggregate in-progress copy: single-part tracks read "Fetching… N%", multi-part tracks
 * "Fetching part K of M — N%" (K = the first in-flight part's ordinal among the missing
 * parts, M = total missing). While the percent is unknown (total not yet declared) the
 * percent suffix is omitted.
 */
export function formatTrackFetchLabel(track: {
  percent: number;
  currentPart: number;
  totalParts: number;
}): string {
  const pct =
    Number.isFinite(track.percent) && track.percent > 0
      ? `${Math.min(100, Math.round(track.percent))}%`
      : null;
  if (track.totalParts > 1) {
    const base = `Fetching part ${track.currentPart} of ${track.totalParts}`;
    return pct ? `${base} — ${pct}` : `${base}…`;
  }
  return pct ? `Fetching… ${pct}` : "Fetching…";
}

/** "Not enough disk space (need ~X)" copy for the no_space terminal. */
export function formatNoSpace(needed: number): string {
  return `Not enough disk space (need ~${formatBytes(needed)})`;
}

/** Slow cadence for re-probing a track the server doesn't (fully) hold yet. */
export const ON_DEVICE_REPROBE_MS = 30_000;

/**
 * Poll scheduling for the auto-fetch effect: fast (600ms) while anything is downloading or
 * waiting for a permit, slow (30s) while the server lacks a blob (still-uploading re-probe —
 * the fetch self-starts when the upload lands), and stop (`null`) on ready + the terminals
 * that need a user action (retry / free space). Pure so the cadence is unit-testable.
 */
export function nextPollDelayMs(track: TrackFetch): number | null {
  switch (track.kind) {
    case "fetching":
    case "queued":
      return 600;
    case "on_device":
      return ON_DEVICE_REPROBE_MS;
    default:
      return null;
  }
}

/**
 * S2 producer seam — FIRE-AND-FORGET enqueue of a finalized session's audio parts onto
 * the durable upload queue (NORMAL priority). Recording is NEVER blocked by this and the
 * queue is durable, so we intentionally do not await or surface failures here: on a
 * default (no-sync) build the command is absent and `invoke` rejects with "command not
 * found", which we swallow; when sync is off the command is a harmless no-op. The backfill
 * walk re-enqueues on next enable, so a dropped call is self-healing.
 */
export function enqueueAudioForSession(sessionId: string): void {
  if (!sessionId) return;
  void invoke("audio_enqueue_session", { sessionId }).catch(() => {});
}

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

/**
 * Copy for the PULL-side catch-up line (R12): the device has drained its own outbox
 * but is still applying peer changesets. Counts CHANGESETS (commit-ordered batches),
 * not individual cells — labelled "to go" so it never reads as an item/byte count. A
 * non-positive/degenerate count falls back to the plain active-sync phrasing.
 */
export function formatCatchingUp(behind: number): string {
  if (!Number.isFinite(behind) || behind <= 0) return "Syncing — catching up";
  const noun = behind === 1 ? "change" : "changes";
  return `Syncing — catching up (${behind} ${noun} to go)`;
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

// ----- S2 audio-upload lane display -----

export type AudioBackupState = "hidden" | "uploading" | "failed" | "complete";

export interface AudioBackupDisplay {
  /** `hidden` when there is nothing to show (nothing outstanding, failed, or ever
   *  uploaded) — the card is omitted entirely. */
  state: AudioBackupState;
  /** The single steady-state line rendered in the card. */
  label: string;
}

type AudioLaneFields = Pick<
  SyncStatus,
  | "audioUploadOutstanding"
  | "audioBackfillOutstanding"
  | "audioUploadFailed"
  | "audioUploadedTotal"
>;

/**
 * Derive the audio-upload lane's steady-state line for the Sync panel. Precedence:
 * failures (needs attention) → in-flight uploads (with the library-backfill nuance) →
 * an "all backed up" resting state → hidden. Pure so the panel and its tests share one
 * source of truth. Distinct from the changeset `deriveSyncDisplay` — audio is its own lane.
 */
export function deriveAudioBackup(s: AudioLaneFields): AudioBackupDisplay {
  const out = Math.max(0, s.audioUploadOutstanding);
  const back = Math.max(0, s.audioBackfillOutstanding);
  const failed = Math.max(0, s.audioUploadFailed);
  const done = Math.max(0, s.audioUploadedTotal);

  if (failed > 0) {
    const noun = failed === 1 ? "recording" : "recordings";
    return { state: "failed", label: `${failed} ${noun} failed to back up` };
  }
  if (out > 0) {
    const noun = out === 1 ? "recording" : "recordings";
    const base = `Backing up ${out} ${noun}`;
    if (back >= out) {
      return { state: "uploading", label: `${base} from your existing library` };
    }
    if (back > 0) {
      return {
        state: "uploading",
        label: `${base} (${back} from your existing library)`,
      };
    }
    return { state: "uploading", label: base };
  }
  if (done > 0) {
    const noun = done === 1 ? "recording" : "recordings";
    return { state: "complete", label: `${done} ${noun} backed up` };
  }
  return { state: "hidden", label: "" };
}
