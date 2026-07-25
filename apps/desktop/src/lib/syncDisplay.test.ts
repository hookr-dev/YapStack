import { describe, it, expect } from "vitest";
import {
  deriveSyncDisplay,
  SMALL_N,
  type SyncDisplayInput,
} from "./syncDisplay";
import type { RelayConnState, SyncStatus } from "./sync";

const NOW = new Date("2026-07-07T12:00:00Z");

function status(partial: Partial<SyncStatus> = {}): SyncStatus {
  return {
    phase: "connected",
    serverUrl: "https://sync.yapstack.app",
    email: "a@b.com",
    deviceFingerprint: "ABCD-2345-EFGH-6789",
    roster: [],
    vaultKeyEpoch: 0,
    rosterFingerprint: null,
    syncEnabled: true,
    lastError: null,
    billingUrl: null,
    pendingEntries: 0,
    pendingBytes: 0,
    ackedThisSession: 0,
    lastSuccess: null,
    pullBehind: 0,
    cryptoQuarantined: 0,
    audioUploadOutstanding: 0,
    audioBackfillOutstanding: 0,
    audioUploadFailed: 0,
    audioUploadedTotal: 0,
    audioBackfillComplete: false,
    ...partial,
  };
}

const IDLE: RelayConnState = { kind: "idle" };

function derive(partial: Partial<SyncDisplayInput>): ReturnType<typeof deriveSyncDisplay> {
  return deriveSyncDisplay({
    conn: IDLE,
    status: status(),
    signedIn: true,
    syncEnabled: true,
    now: NOW,
    ...partial,
  });
}

describe("deriveSyncDisplay — off", () => {
  it("is off when signed out (even with everything else 'live')", () => {
    const d = derive({
      signedIn: false,
      conn: { kind: "unreachable", raw: "x" },
      status: status({ phase: "syncing", pendingEntries: 5 }),
    });
    expect(d.state).toBe("off");
    expect(d.label).toBe("Sync is off");
    expect(d.icon).toBe("cloud");
    expect(d.tone).toBe("muted");
    expect(d.motion).toBe(false);
  });

  it("is off when sync is disabled", () => {
    expect(derive({ syncEnabled: false }).state).toBe("off");
  });

  it("does NOT render unreachable while signed out — off wins", () => {
    const d = derive({ signedIn: false, conn: { kind: "tls-error", raw: "cert" } });
    expect(d.state).toBe("off");
  });
});

describe("deriveSyncDisplay — connection health short-circuits sync phase", () => {
  it.each(["unreachable", "tls-error", "not-a-relay"] as const)(
    "%s connection beats syncing → unreachable",
    (kind) => {
      const d = derive({
        conn: { kind, raw: "boom" },
        status: status({ phase: "syncing", pendingEntries: 99 }),
      });
      expect(d.state).toBe("unreachable");
      expect(d.label).toBe("Can't reach relay");
      expect(d.tone).toBe("amber");
      expect(d.icon).toBe("cloud-off");
      expect(d.motion).toBe(false);
    },
  );

  it("tooltip echoes the host from the server URL", () => {
    const d = derive({
      conn: { kind: "unreachable", raw: "refused" },
      status: status({ serverUrl: "https://relay.example.com:8443/base" }),
    });
    expect(d.tooltip).toBe("Can't reach relay.example.com:8443");
  });

  it("R3: drain-level phase=='unreachable' renders amber unreachable even when the probe is ok", () => {
    // A relay that died MID-SESSION: the last probe still reads "ok", but the drain
    // classified a connect/timeout at the transport into phase=="unreachable". This must
    // show the amber "Can't reach relay" state, NOT the destructive red "error", and it
    // must beat a set lastError (which carries the verbatim network error).
    const d = derive({
      conn: {
        kind: "ok",
        engineVersion: "0.16.3",
        protocolVersion: 1,
        latencyMs: 42,
        normalizedUrl: "https://relay.example.com",
        versionAdvisory: null,
      },
      status: status({
        phase: "unreachable",
        serverUrl: "https://relay.example.com",
        lastError: "relay unreachable: error sending request",
      }),
    });
    expect(d.state).toBe("unreachable");
    expect(d.label).toBe("Can't reach relay");
    expect(d.tone).toBe("amber");
    expect(d.icon).toBe("cloud-off");
    expect(d.tooltip).toBe("Can't reach relay.example.com");
  });

  it("an ok connection with a version advisory does NOT change state (still caught-up)", () => {
    const d = derive({
      conn: {
        kind: "ok",
        engineVersion: "0.16.3",
        protocolVersion: 1,
        latencyMs: 42,
        normalizedUrl: "https://sync.yapstack.app",
        versionAdvisory: { minClientVersion: "0.20.0", raw: "update this app" },
      },
      status: status({ phase: "connected", pendingEntries: 0 }),
    });
    expect(d.state).toBe("caught-up");
  });
});

describe("deriveSyncDisplay — auth expired", () => {
  it("auth_expired maps to auth-expired", () => {
    const d = derive({ status: status({ phase: "auth_expired" }) });
    expect(d.state).toBe("auth-expired");
    expect(d.label).toBe("Sign in again to sync");
    expect(d.tone).toBe("amber");
    expect(d.icon).toBe("cloud-alert");
  });

  it("auth_expired beats a set lastError (never conflated with a sync error)", () => {
    const d = derive({
      status: status({ phase: "auth_expired", lastError: "boom" }),
    });
    expect(d.state).toBe("auth-expired");
  });
});

describe("deriveSyncDisplay — error", () => {
  it("phase error → error", () => {
    const d = derive({ status: status({ phase: "error", lastError: "kaput" }) });
    expect(d.state).toBe("error");
    expect(d.label).toBe("Sync error — needs attention");
    expect(d.tone).toBe("destructive");
    expect(d.icon).toBe("cloud-alert");
    expect(d.motion).toBe(false);
  });

  it("a set lastError beats syncing (error wins over an in-flight push)", () => {
    const d = derive({
      status: status({ phase: "syncing", pendingEntries: 10, lastError: "net" }),
    });
    expect(d.state).toBe("error");
  });
});

describe("deriveSyncDisplay — syncing + motion gate", () => {
  it("syncing with a small backlog does not animate", () => {
    const d = derive({
      status: status({ phase: "syncing", pendingEntries: SMALL_N, pendingBytes: 0 }),
    });
    expect(d.state).toBe("syncing");
    expect(d.tone).toBe("active");
    expect(d.icon).toBe("refresh");
    expect(d.motion).toBe(false);
    expect(d.label).toBe("Syncing — 3 items remaining");
  });

  it("animates once the backlog exceeds SMALL_N", () => {
    const d = derive({
      status: status({ phase: "syncing", pendingEntries: SMALL_N + 1 }),
    });
    expect(d.motion).toBe(true);
  });

  it("animates for a small backlog once it has been syncing continuously > 2s", () => {
    const d = derive({
      status: status({ phase: "syncing", pendingEntries: 1 }),
      syncingSince: NOW.getTime() - 3000,
    });
    expect(d.motion).toBe(true);
  });

  it("does not animate for a small backlog just-started (< 2s)", () => {
    const d = derive({
      status: status({ phase: "syncing", pendingEntries: 1 }),
      syncingSince: NOW.getTime() - 500,
    });
    expect(d.motion).toBe(false);
  });
});

describe("deriveSyncDisplay — catching up (R12, pull behind)", () => {
  it("phase catching_up maps to the active syncing visual with (N to go) copy", () => {
    const d = derive({
      status: status({
        phase: "catching_up",
        pendingEntries: 0,
        pullBehind: 1650,
      }),
    });
    expect(d.state).toBe("catching-up");
    expect(d.tone).toBe("active");
    expect(d.icon).toBe("refresh");
    expect(d.motion).toBe(true);
    expect(d.label).toBe("Syncing — catching up (1650 changes to go)");
    expect(d.tooltip).toBe("Syncing — catching up (1650 changes to go)");
  });

  it("singular copy for a single changeset behind", () => {
    const d = derive({
      status: status({ phase: "catching_up", pullBehind: 1 }),
    });
    expect(d.label).toBe("Syncing — catching up (1 change to go)");
  });

  it("a pull backlog is NOT 'up to date' even with an empty outbox and no error (the R12 bug)", () => {
    // The exact regression: outbox drained (pendingEntries 0), no lastError — the OLD
    // code showed the green "Up to date". A positive pullBehind must beat caught-up.
    const d = derive({
      status: status({
        phase: "connected", // even if the phase lags, the count guards it
        pendingEntries: 0,
        lastError: null,
        pullBehind: 42,
      }),
    });
    expect(d.state).toBe("catching-up");
    expect(d.label).toBe("Syncing — catching up (42 changes to go)");
  });

  it("connectivity health STILL outranks catching up (precedence preserved)", () => {
    const d = derive({
      conn: { kind: "unreachable", raw: "refused" },
      status: status({ phase: "catching_up", pullBehind: 900 }),
    });
    expect(d.state).toBe("unreachable");
  });

  it("auth-expired outranks catching up", () => {
    const d = derive({
      status: status({ phase: "auth_expired", pullBehind: 900 }),
    });
    expect(d.state).toBe("auth-expired");
  });

  it("a set lastError (failing) outranks catching up", () => {
    const d = derive({
      status: status({ phase: "catching_up", pullBehind: 900, lastError: "boom" }),
    });
    expect(d.state).toBe("error");
  });

  it("a PUSH backlog (syncing) takes precedence over pull catch-up", () => {
    const d = derive({
      status: status({ phase: "syncing", pendingEntries: 5, pullBehind: 900 }),
    });
    expect(d.state).toBe("syncing");
  });

  it("caught-up ONLY once pullBehind reaches 0", () => {
    const d = derive({
      status: status({ phase: "connected", pendingEntries: 0, pullBehind: 0 }),
    });
    expect(d.state).toBe("caught-up");
  });
});

describe("deriveSyncDisplay — crypto-quarantine honesty (§11.3, the FINDING)", () => {
  it("caught-up with unreadable changesets is amber 'Synced — N unreadable', NEVER plain green", () => {
    // The exact FINDING: outbox drained, pull caught up, no error — the OLD code showed the
    // green "Up to date" while peer changesets were undecryptable. A nonzero quarantine count
    // must replace the green with an amber attention state.
    const d = derive({
      status: status({
        phase: "connected",
        pendingEntries: 0,
        pullBehind: 0,
        lastError: null,
        cryptoQuarantined: 3,
      }),
    });
    expect(d.state).toBe("quarantined");
    expect(d.tone).toBe("amber");
    expect(d.icon).toBe("cloud-alert");
    expect(d.motion).toBe(false);
    expect(d.label).toBe("Synced — 3 changesets unreadable");
    expect(d.tooltip).toBe("Synced — 3 changesets unreadable");
    // Belt-and-suspenders: it is categorically not the green caught-up.
    expect(d.state).not.toBe("caught-up");
    expect(d.icon).not.toBe("cloud-check");
  });

  it("singular copy for a single unreadable changeset", () => {
    const d = derive({
      status: status({ phase: "connected", pendingEntries: 0, cryptoQuarantined: 1 }),
    });
    expect(d.label).toBe("Synced — 1 changeset unreadable");
  });

  it("a zero count is still plain-green caught-up (no false warning)", () => {
    const d = derive({
      status: status({ phase: "connected", pendingEntries: 0, cryptoQuarantined: 0 }),
    });
    expect(d.state).toBe("caught-up");
    expect(d.icon).toBe("cloud-check");
    expect(d.tone).toBe("muted");
  });

  it("connectivity health (unreachable) outranks the quarantine warning", () => {
    const d = derive({
      conn: { kind: "unreachable", raw: "refused" },
      status: status({ cryptoQuarantined: 5 }),
    });
    expect(d.state).toBe("unreachable");
  });

  it("auth-expired outranks the quarantine warning", () => {
    const d = derive({
      status: status({ phase: "auth_expired", cryptoQuarantined: 5 }),
    });
    expect(d.state).toBe("auth-expired");
  });

  it("a set lastError (error) outranks the quarantine warning", () => {
    const d = derive({
      status: status({ phase: "connected", cryptoQuarantined: 5, lastError: "boom" }),
    });
    expect(d.state).toBe("error");
  });

  it("an active push (syncing) outranks the quarantine warning (transient spinner, still honest)", () => {
    const d = derive({
      status: status({ phase: "syncing", pendingEntries: 5, cryptoQuarantined: 5 }),
    });
    expect(d.state).toBe("syncing");
  });

  it("R12 coexistence: an active pull (catching-up) outranks the quarantine warning", () => {
    // Quarantine + a still-behind pull: the active catch-up spinner wins; once the pull
    // settles (pullBehind 0) the very next derivation surfaces the amber warning instead of
    // green — the count is durable, so nothing is lost by deferring behind the active pull.
    const d = derive({
      status: status({ phase: "catching_up", pullBehind: 900, cryptoQuarantined: 5 }),
    });
    expect(d.state).toBe("catching-up");
  });

  it("a push backlog (pending) outranks the quarantine warning (distinct non-green idle state)", () => {
    // pending is not green either, and it resolves; once the outbox drains the amber warning
    // surfaces. The invariant we protect is only 'never plain green while count > 0'.
    const d = derive({
      status: status({ phase: "connected", pendingEntries: 4, cryptoQuarantined: 5 }),
    });
    expect(d.state).toBe("pending");
  });

  it("R12 coexistence, settled: quarantine + caught-up pull (pullBehind 0) → amber, not green", () => {
    const d = derive({
      status: status({
        phase: "connected",
        pendingEntries: 0,
        pullBehind: 0,
        cryptoQuarantined: 2,
      }),
    });
    expect(d.state).toBe("quarantined");
    expect(d.tone).toBe("amber");
    expect(d.label).toBe("Synced — 2 changesets unreadable");
  });
});

describe("deriveSyncDisplay — pending", () => {
  it("idle with a backlog → pending", () => {
    const d = derive({ status: status({ phase: "connected", pendingEntries: 2 }) });
    expect(d.state).toBe("pending");
    expect(d.label).toBe("2 to sync");
    expect(d.tone).toBe("muted");
    expect(d.icon).toBe("cloud-dot");
    expect(d.tooltip).toBe("2 changes to sync");
  });

  it("singular noun in the tooltip for one change", () => {
    const d = derive({ status: status({ phase: "connected", pendingEntries: 1 }) });
    expect(d.tooltip).toBe("1 change to sync");
  });
});

describe("deriveSyncDisplay — caught-up gating", () => {
  it("caught-up requires pending == 0 AND no lastError", () => {
    const d = derive({
      status: status({ phase: "connected", pendingEntries: 0, lastError: null }),
    });
    expect(d.state).toBe("caught-up");
    expect(d.icon).toBe("cloud-check");
    expect(d.tone).toBe("muted");
  });

  it("is NOT caught-up while lastError is set (falls to error)", () => {
    const d = derive({
      status: status({ phase: "connected", pendingEntries: 0, lastError: "x" }),
    });
    expect(d.state).toBe("error");
  });

  it("is NOT caught-up while pending > 0 (falls to pending)", () => {
    const d = derive({
      status: status({ phase: "connected", pendingEntries: 4 }),
    });
    expect(d.state).toBe("pending");
  });

  it("renders relative last-synced when available", () => {
    const d = derive({
      status: status({
        phase: "connected",
        pendingEntries: 0,
        lastSuccess: "2026-07-07T11:57:00Z",
      }),
    });
    expect(d.label).toBe("Up to date · synced 3m ago");
  });

  it("omits the suffix when never synced", () => {
    const d = derive({ status: status({ phase: "connected", lastSuccess: null }) });
    expect(d.label).toBe("Up to date");
  });
});

describe("deriveSyncDisplay — null status guard", () => {
  it("does not crash when status is null (neutral caught-up)", () => {
    const d = derive({ status: null });
    expect(d.state).toBe("caught-up");
    expect(d.label).toBe("Up to date");
  });
});
