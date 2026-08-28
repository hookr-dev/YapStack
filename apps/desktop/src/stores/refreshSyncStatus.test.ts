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

import { invoke } from "@tauri-apps/api/core";
import { useAppStore } from "./appStore";
import { type SyncStatus } from "@/lib/sync";

const invokeMock = vi.mocked(invoke);

function connectedStatus(over: Partial<SyncStatus> = {}): SyncStatus {
  return {
    phase: "connected",
    serverUrl: "https://relay.example",
    email: "owner@example.com",
    deviceFingerprint: "AAAA-BBBB-CCCC-DDDD",
    roster: [],
    vaultKeyEpoch: 3,
    rosterFingerprint: "EEEE-FFFF-GGGG-HHHH",
    syncEnabled: true,
    lastError: null,
    billingUrl: "https://pay.example",
    pendingEntries: 4,
    pendingBytes: 512,
    ackedThisSession: 9,
    lastSuccess: "2026-01-01T00:00:00Z",
    pullBehind: 2,
    cryptoQuarantined: 3,
    audioUploadOutstanding: 1,
    audioBackfillOutstanding: 1,
    audioUploadFailed: 1,
    audioUploadedTotal: 7,
    audioBackfillComplete: false,
    ...over,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.spyOn(console, "error").mockImplementation(() => {});
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

describe("refreshSyncStatus — failed fetch preserves the last-known snapshot", () => {
  it("keeps a non-zero cryptoQuarantined across a status-fetch failure", async () => {
    useAppStore.setState({ syncStatus: connectedStatus() });
    invokeMock.mockRejectedValue(new Error("ipc blip"));

    await useAppStore.getState().refreshSyncStatus();

    const { syncStatus } = useAppStore.getState();
    expect(syncStatus?.phase).toBe("error");
    expect(syncStatus?.lastError).toBe("ipc blip");
    // §11.3: a non-zero quarantine count is a potential tamper signal and is never
    // auto-dismissed — one failed poll must not blank the standing warning row.
    expect(syncStatus?.cryptoQuarantined).toBe(3);
    // Every other carried-forward counter survives too (T024).
    expect(syncStatus?.pendingEntries).toBe(4);
    expect(syncStatus?.pullBehind).toBe(2);
    expect(syncStatus?.audioUploadFailed).toBe(1);
    expect(syncStatus?.billingUrl).toBe("https://pay.example");
  });

  it("falls back to zero values on a first-ever failure with no previous status", async () => {
    invokeMock.mockRejectedValue(new Error("ipc blip"));

    await useAppStore.getState().refreshSyncStatus();

    const { syncStatus, syncConfig } = useAppStore.getState();
    expect(syncStatus?.phase).toBe("error");
    expect(syncStatus?.cryptoQuarantined).toBe(0);
    expect(syncStatus?.roster).toEqual([]);
    // Identity fields come from syncConfig, not from the zero-value constant.
    expect(syncStatus?.serverUrl).toBe("https://relay.example");
    expect(syncStatus?.email).toBe("owner@example.com");
    // The error snapshot bypasses setSyncStatus, so the persisted mirror is untouched.
    expect(syncConfig.serverUrl).toBe("https://relay.example");
    expect(syncConfig.email).toBe("owner@example.com");
  });
});
