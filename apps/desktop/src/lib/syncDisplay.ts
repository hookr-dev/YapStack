// Unified sync-display derivation — the single source of truth both the Settings
// → Sync badge (T027) and the app-wide ambient sidebar glyph (T028) render from,
// so the two surfaces can never disagree (plan §3: "recompute from live status;
// never latch"). Pure and fully testable — no Tauri, no store access.
//
// The precedence is the binding two-tier rule from SYNC_PAGE_REDESIGN.md §1b:
// connection health short-circuits sync phase. "Up to date" is impossible while
// unreachable, while `lastError` is set, or while anything is pending.

import {
  formatCatchingUp,
  formatLastSynced,
  formatSyncProgress,
  type RelayConnState,
  type SyncStatus,
} from "./sync";

/**
 * Backlog size at/under which the syncing glyph does NOT animate. Our capture
 * batches at ~5s drain cycles, so a handful of entries is a routine blip, not a
 * sustained sync — animating it would strobe the sidebar (plan §3 motion gate,
 * HIG/Linear). Motion needs `pendingEntries > SMALL_N` OR a >2s continuous
 * in-flight window. Exported so T027/T028 can label the boundary if needed.
 */
export const SMALL_N = 3;

/** Continuous-syncing window (ms) that also unlocks motion, independent of N. */
export const SYNC_MOTION_MS = 2000;

/** The mutually-exclusive display states (plan §3 table; `catching-up` added R12;
 *  `quarantined` added for the §11.3 crypto-quarantine honesty fix). */
export type SyncDisplayState =
  | "off"
  | "caught-up"
  | "quarantined"
  | "syncing"
  | "catching-up"
  | "pending"
  | "auth-expired"
  | "unreachable"
  | "error";

export type SyncDisplayTone = "muted" | "active" | "amber" | "destructive";

/** Icon key from the single agreed lucide map (plan §3). Tone disambiguates the
 *  two `cloud-alert` uses (amber auth-expired vs destructive error). */
export type SyncDisplayIcon =
  | "cloud"
  | "cloud-check"
  | "refresh"
  | "cloud-alert"
  | "cloud-off"
  | "cloud-dot";

export interface SyncDisplay {
  state: SyncDisplayState;
  label: string;
  tone: SyncDisplayTone;
  icon: SyncDisplayIcon;
  /** Animate the glyph — true ONLY for `syncing` past the motion gate. */
  motion: boolean;
  tooltip: string;
}

export interface SyncDisplayInput {
  /** Probe-derived relay connection health (§1b axis, separate from phase). */
  conn: RelayConnState;
  /** Live status snapshot; null before the first fetch. */
  status: SyncStatus | null;
  /** Durable signed-in handle (syncConfig.email != null). */
  signedIn: boolean;
  /** Whether the user has opted into (and migrated for) sync. */
  syncEnabled: boolean;
  /** Injected clock for deterministic tests; defaults to now. */
  now?: Date;
  /** Epoch ms the current `syncing` window began; unlocks motion past
   *  SYNC_MOTION_MS even for a small backlog. Undefined = time-gate off. */
  syncingSince?: number;
}

/** Host for the "Can't reach <host>" tooltip; falls back to the raw URL. */
function hostOf(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/**
 * Derive the one display object every sync surface renders. Precedence (binding,
 * plan §1b + R12 + §11.3 crypto-quarantine): off → unreachable → auth-expired → error →
 * syncing (push) → catching-up (pull behind) → pending → { caught-up | quarantined }.
 * Connection failure short-circuits BEFORE any sync phase, "catching up" outranks "up to
 * date" so a pull backlog can never masquerade as green, and — at the caught-up slot only —
 * a nonzero crypto-quarantine count turns the ONLY green state into an amber "Synced — N
 * unreadable" warning, so an unreadable/tampered peer changeset can never hide behind plain
 * green. It sits at the caught-up slot (not higher) deliberately: unreachable/auth/error are
 * more urgent and actionable; syncing/catching-up are active transitions whose spinner is
 * honest and settle back to THIS amber when they finish; pending is a distinct non-green idle
 * state that resolves and then reveals the warning. The count is durable, so deferring it
 * behind those transient/idle states never loses it — it only ever replaces the green.
 */
export function deriveSyncDisplay(input: SyncDisplayInput): SyncDisplay {
  const { conn, status, signedIn, syncEnabled, syncingSince } = input;
  const now = input.now ?? new Date();

  // 1. Signed-out or sync disabled — nothing to say.
  if (!signedIn || !syncEnabled) {
    return {
      state: "off",
      label: "Sync is off",
      tone: "muted",
      icon: "cloud",
      motion: false,
      tooltip: "Sync is off",
    };
  }

  const serverUrl = status?.serverUrl ?? "";
  const phase = status?.phase ?? null;
  const pendingEntries = status?.pendingEntries ?? 0;
  const pendingBytes = status?.pendingBytes ?? 0;
  const pullBehind = status?.pullBehind ?? 0;
  const cryptoQuarantined = status?.cryptoQuarantined ?? 0;
  const lastError = status?.lastError ?? null;
  const lastSuccess = status?.lastSuccess ?? null;
  const hasError = lastError != null && lastError !== "";

  // 2. Connection health short-circuits sync phase (§1b two-tier rule). The
  //    three typed probe failures all render as "unreachable". We derive this
  //    ONLY from probe results OR the drain's typed connectivity phase — never
  //    from parsing `lastError` strings. (R3: the drain now classifies a
  //    connect/timeout failure at the transport into phase=="unreachable", so a
  //    relay that dies mid-session feeds this amber state without string-matching
  //    the verbatim error.)
  if (
    conn.kind === "unreachable" ||
    conn.kind === "tls-error" ||
    conn.kind === "not-a-relay" ||
    phase === "unreachable"
  ) {
    return {
      state: "unreachable",
      label: "Can't reach relay",
      tone: "amber",
      icon: "cloud-off",
      motion: false,
      tooltip: serverUrl ? `Can't reach ${hostOf(serverUrl)}` : "Can't reach relay",
    };
  }

  // 3. Expired session — never conflated with a wrong/unreachable server.
  if (phase === "auth_expired") {
    return {
      state: "auth-expired",
      label: "Sign in again to sync",
      tone: "amber",
      icon: "cloud-alert",
      motion: false,
      tooltip: "Sign in again to sync",
    };
  }

  // 4. Hard error — a failed phase OR any set lastError. Beats syncing so a
  //    stale/failed push never masquerades as progress, and gates out
  //    "Up to date" (plan §1b: no up-to-date while lastError is set).
  if (phase === "error" || hasError) {
    return {
      state: "error",
      label: "Sync error — needs attention",
      tone: "destructive",
      icon: "cloud-alert",
      motion: false,
      tooltip: "Sync error — needs attention",
    };
  }

  // 5. Actively pushing.
  if (phase === "syncing") {
    const overN = pendingEntries > SMALL_N;
    const overTime =
      syncingSince != null && now.getTime() - syncingSince > SYNC_MOTION_MS;
    return {
      state: "syncing",
      label: `Syncing — ${formatSyncProgress(pendingEntries, pendingBytes)}`,
      tone: "active",
      icon: "refresh",
      motion: overN || overTime,
      tooltip: `Syncing — ${formatSyncProgress(pendingEntries, pendingBytes)}`,
    };
  }

  // 6. Outbox drained, but the PULL is still behind the relay tip (R12): a device is
  //    actively catching up on peer changesets and must NOT show the green "Up to date".
  //    Renders as an active-sync visual (same refresh glyph + motion as `syncing`) with
  //    honest "(N to go)" copy. The backend sets phase=="catching_up"; we ALSO fall in
  //    here on any pullBehind > 0 as a belt-and-suspenders guard so a phase/count mismatch
  //    can never leak a false "up to date".
  if (phase === "catching_up" || pullBehind > 0) {
    return {
      state: "catching-up",
      label: formatCatchingUp(pullBehind),
      tone: "active",
      icon: "refresh",
      motion: true,
      tooltip: formatCatchingUp(pullBehind),
    };
  }

  // 7. Idle with a PUSH backlog queued.
  if (pendingEntries > 0) {
    const noun = pendingEntries === 1 ? "change" : "changes";
    return {
      state: "pending",
      label: `${pendingEntries} to sync`,
      tone: "muted",
      icon: "cloud-dot",
      motion: false,
      tooltip: `${pendingEntries} ${noun} to sync`,
    };
  }

  // 8. Fully drained, reachable, no error, no push backlog, caught up on the pull — but one
  //    or more PEER changesets are cryptographically UNREADABLE (§11.3 crypto-quarantine: a
  //    decrypt/decode failure this device stored durably and ADVANCED past, so the watermark
  //    still reached the tip). We are honestly caught up AND carrying a standing warning, so we
  //    render amber "Synced — N unreadable" (the cloud-alert idiom) instead of the plain-green
  //    "Up to date". This is the FINDING fix: the count must never read as green. It replaces
  //    ONLY the green caught-up state (see the precedence note above), never a busier state.
  if (cryptoQuarantined > 0) {
    const noun = cryptoQuarantined === 1 ? "changeset" : "changesets";
    const label = `Synced — ${cryptoQuarantined} ${noun} unreadable`;
    return {
      state: "quarantined",
      label,
      tone: "amber",
      icon: "cloud-alert",
      motion: false,
      tooltip: label,
    };
  }

  // 9. Fully drained, reachable, no error, no unreadable changesets, AND caught up on the pull
  //    → up to date. (Strictly gated: we can only reach here with pendingEntries == 0 AND
  //    !hasError AND pullBehind == 0 AND cryptoQuarantined == 0.)
  const rel = formatLastSynced(lastSuccess, now.getTime());
  return {
    state: "caught-up",
    label: rel ? `Up to date · synced ${rel}` : "Up to date",
    tone: "muted",
    icon: "cloud-check",
    motion: false,
    tooltip: rel ? `Up to date · synced ${rel}` : "Up to date",
  };
}
