import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => ({
  toast: { info: vi.fn(), error: vi.fn(), warning: vi.fn(), success: vi.fn() },
}));

import { useAppStore } from "./appStore";
import { type SyncStatus } from "@/lib/sync";

/** A signed-out / disconnected status DTO as the Rust `sync_status` returns it: empty
 *  serverUrl, null email, syncEnabled false. */
function disconnectedStatus(): SyncStatus {
  return {
    phase: "disconnected",
    serverUrl: "",
    email: null,
    deviceFingerprint: null,
    roster: [],
    vaultKeyEpoch: null,
    rosterFingerprint: null,
    syncEnabled: false,
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
  };
}

/** A signed-in status DTO carrying real handles. */
function signedInStatus(over: Partial<SyncStatus> = {}): SyncStatus {
  return {
    ...disconnectedStatus(),
    phase: "connected",
    serverUrl: "https://relay.example",
    email: "owner@example.com",
    deviceFingerprint: "AAAA-BBBB-CCCC-DDDD",
    syncEnabled: true,
    ...over,
  };
}

beforeEach(() => {
  // A user who previously configured a relay and signed in — the persisted syncConfig
  // the bug was wiping.
  useAppStore.setState((s) => ({
    syncStatus: null,
    syncConfig: {
      ...s.syncConfig,
      serverUrl: "https://relay.example",
      email: "owner@example.com",
      syncEnabled: true,
      deviceFingerprint: "AAAA-BBBB-CCCC-DDDD",
    },
  }));
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("setSyncStatus — non-destructive config mirror", () => {
  it("a signed-out poll does NOT clobber the persisted serverUrl or email", () => {
    useAppStore.getState().setSyncStatus(disconnectedStatus());

    const { syncConfig, syncStatus } = useAppStore.getState();
    // The transient signed-out status is still reflected in syncStatus (live UI state)...
    expect(syncStatus?.phase).toBe("disconnected");
    // ...but the PERSISTED relay URL and email survive the poll.
    expect(syncConfig.serverUrl).toBe("https://relay.example");
    expect(syncConfig.email).toBe("owner@example.com");
  });

  it("explicit sign-out still clears the email (its own path), keeping the relay URL", () => {
    // handleSignOut's flow: null the status, then clear email via setSyncConfig.
    useAppStore.getState().setSyncStatus(null);
    useAppStore.getState().setSyncConfig({ email: null, syncEnabled: false });

    const { syncConfig } = useAppStore.getState();
    expect(syncConfig.email).toBeNull();
    expect(syncConfig.syncEnabled).toBe(false);
    // Relay URL deliberately preserved so the user can sign back in.
    expect(syncConfig.serverUrl).toBe("https://relay.example");
  });

  it("a signed-in poll updates serverUrl + email + syncEnabled normally", () => {
    // Start from defaults so we can prove the signed-in values are mirrored in.
    useAppStore.setState((s) => ({
      syncConfig: {
        ...s.syncConfig,
        serverUrl: "https://old.example",
        email: null,
        syncEnabled: false,
        deviceFingerprint: null,
      },
    }));

    useAppStore.getState().setSyncStatus(
      signedInStatus({
        serverUrl: "https://new.example",
        email: "new@example.com",
      }),
    );

    const { syncConfig } = useAppStore.getState();
    expect(syncConfig.serverUrl).toBe("https://new.example");
    expect(syncConfig.email).toBe("new@example.com");
    expect(syncConfig.syncEnabled).toBe(true);
    expect(syncConfig.deviceFingerprint).toBe("AAAA-BBBB-CCCC-DDDD");
  });

  it("a signed-out poll after sign-out does not resurrect a stale email", () => {
    // Sign out first (email cleared), then a disconnected poll arrives.
    useAppStore.getState().setSyncConfig({ email: null, syncEnabled: false });
    useAppStore.getState().setSyncStatus(disconnectedStatus());

    const { syncConfig } = useAppStore.getState();
    // email stays cleared (nullish DTO email does not overwrite, and prev is already null).
    expect(syncConfig.email).toBeNull();
    // serverUrl still preserved.
    expect(syncConfig.serverUrl).toBe("https://relay.example");
  });
});
