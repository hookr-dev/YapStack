import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import { invoke } from "@tauri-apps/api/core";
import { useAppStore } from "@/stores/appStore";
import { TooltipProvider } from "@/components/ui/tooltip";
import { SyncStatusGlyph } from "./SyncStatusGlyph";
import type { RelayConnState, SyncStatus } from "@/lib/sync";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

const invokeMock = vi.mocked(invoke);

function baseStatus(over: Partial<SyncStatus> = {}): SyncStatus {
  return {
    phase: "connected",
    serverUrl: "https://sync.yapstack.app",
    email: "user@example.com",
    deviceFingerprint: "AAAA-BBBB-CCCC-DDDD",
    roster: [],
    vaultKeyEpoch: null,
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
    ...over,
  };
}

/** Seed store + make the capability probe resolve the same status, so the async
 *  probe re-sets an identical value (no visual change) rather than clobbering. */
function seed({
  status,
  conn = { kind: "idle" },
  email = "user@example.com",
  syncEnabled = true,
}: {
  status: SyncStatus | null;
  conn?: RelayConnState;
  email?: string | null;
  syncEnabled?: boolean;
}) {
  invokeMock.mockImplementation(async (cmd: string) =>
    cmd === "sync_status" ? status : null,
  );
  useAppStore.setState({
    syncStatus: status,
    relayConn: conn,
    syncConfig: {
      ...useAppStore.getState().syncConfig,
      email,
      syncEnabled,
    },
  });
}

function renderGlyph() {
  return render(
    <TooltipProvider>
      <SyncStatusGlyph />
    </TooltipProvider>,
  );
}

/** Render + let the one-shot capability probe settle (so its async setSyncStatus
 *  runs inside act and never leaks into the next test). */
async function renderSettled() {
  const utils = renderGlyph();
  await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("sync_status"));
  return utils;
}

beforeEach(() => {
  vi.clearAllMocks();
  invokeMock.mockResolvedValue(null);
  useAppStore.setState({
    syncStatus: null,
    relayConn: { kind: "idle" },
    syncConfig: {
      ...useAppStore.getState().syncConfig,
      email: null,
      syncEnabled: false,
    },
  });
});

describe("SyncStatusGlyph — visibility", () => {
  it("renders nothing when the user never engaged sync", () => {
    seed({ status: null, email: null, syncEnabled: false });
    const { container } = renderGlyph();
    expect(container.firstChild).toBeNull();
    // Never engaged → never polls.
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("hides and stops polling on a no-sync build (missing sync_status command)", async () => {
    seed({ status: null });
    invokeMock.mockRejectedValue(new Error("Command sync_status not found"));
    const { container } = renderGlyph();
    await waitFor(() => expect(container.firstChild).toBeNull());
    // Capability probe fired exactly once; missing-command stops rescheduling.
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("sync_status");
  });
});

describe("SyncStatusGlyph — icon + tone per state", () => {
  it("caught-up → CloudCheck, muted", async () => {
    seed({ status: baseStatus() });
    const { container } = await renderSettled();
    const icon = container.querySelector(".lucide-cloud-check");
    expect(icon).toBeTruthy();
    expect(icon).toHaveClass("text-muted-foreground");
  });

  it("syncing over the motion gate → RefreshCw, active, spins", async () => {
    seed({ status: baseStatus({ phase: "syncing", pendingEntries: 5 }) });
    const { container } = await renderSettled();
    const icon = container.querySelector(".lucide-refresh-cw");
    expect(icon).toBeTruthy();
    expect(icon).toHaveClass("text-primary");
    expect(icon).toHaveClass("animate-spin");
  });

  it("syncing under the motion gate → RefreshCw but no animation", async () => {
    seed({ status: baseStatus({ phase: "syncing", pendingEntries: 2 }) });
    const { container } = await renderSettled();
    const icon = container.querySelector(".lucide-refresh-cw");
    expect(icon).toBeTruthy();
    expect(icon).not.toHaveClass("animate-spin");
  });

  it("pending idle backlog → Cloud + dot, muted", async () => {
    seed({ status: baseStatus({ phase: "connected", pendingEntries: 2 }) });
    const { container } = await renderSettled();
    // cloud-dot composes a base Cloud with a bg-current dot.
    expect(container.querySelector(".lucide-cloud")).toBeTruthy();
    expect(container.querySelector(".bg-current")).toBeTruthy();
  });

  it("auth expired → CloudAlert, amber", async () => {
    seed({ status: baseStatus({ phase: "auth_expired" }) });
    const { container } = await renderSettled();
    const icon = container.querySelector(".lucide-cloud-alert");
    expect(icon).toBeTruthy();
    expect(icon).toHaveClass("text-amber-600");
  });

  it("unreachable relay → CloudOff, amber", async () => {
    seed({
      status: baseStatus(),
      conn: { kind: "unreachable", raw: "connection refused" },
    });
    const { container } = await renderSettled();
    const icon = container.querySelector(".lucide-cloud-off");
    expect(icon).toBeTruthy();
    expect(icon).toHaveClass("text-amber-600");
  });

  it("caught-up WITH unreadable changesets → CloudAlert, amber, never animates (the FINDING)", async () => {
    // §11.3 crypto-quarantine honesty: a caught-up device with undecryptable peer changesets
    // must render the amber cloud-alert warning, NEVER the plain-green CloudCheck.
    seed({ status: baseStatus({ phase: "connected", pendingEntries: 0, cryptoQuarantined: 2 }) });
    const { container } = await renderSettled();
    const icon = container.querySelector(".lucide-cloud-alert");
    expect(icon).toBeTruthy();
    expect(icon).toHaveClass("text-amber-600");
    expect(icon).not.toHaveClass("animate-spin");
    // Categorically not the green up-to-date glyph.
    expect(container.querySelector(".lucide-cloud-check")).toBeNull();
  });

  it("error → CloudAlert, destructive, never animates", async () => {
    seed({ status: baseStatus({ phase: "error", lastError: "boom" }) });
    const { container } = await renderSettled();
    const icon = container.querySelector(".lucide-cloud-alert");
    expect(icon).toBeTruthy();
    expect(icon).toHaveClass("text-destructive");
    expect(icon).not.toHaveClass("animate-spin");
  });
});

describe("SyncStatusGlyph — interaction", () => {
  it("click requests the Sync settings tab and navigates to settings", async () => {
    const setSettingsRequest = vi.fn();
    const navigateTo = vi.fn();
    seed({ status: baseStatus() });
    useAppStore.setState({ setSettingsRequest, navigateTo });

    await renderSettled();
    await userEvent.click(screen.getByRole("button"));

    expect(setSettingsRequest).toHaveBeenCalledWith("sync");
    expect(navigateTo).toHaveBeenCalledWith("settings");
  });
});

describe("SyncStatusGlyph — app-wide polling", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("polls at the 2s syncing cadence via refreshSyncStatus and stops on unmount", async () => {
    vi.useFakeTimers();
    const refreshSyncStatus = vi.fn().mockResolvedValue(undefined);
    seed({ status: baseStatus({ phase: "syncing", pendingEntries: 5 }) });
    useAppStore.setState({ refreshSyncStatus });

    const { unmount } = renderGlyph();

    // Flush the immediate capability probe (direct sync_status invoke).
    await vi.advanceTimersByTimeAsync(1);
    expect(refreshSyncStatus).not.toHaveBeenCalled();

    // Still nothing well before the 2s syncing cadence elapses.
    await vi.advanceTimersByTimeAsync(1500);
    expect(refreshSyncStatus).not.toHaveBeenCalled();

    // Fires past ~2s — and before the 5s idle cadence would have — proving the
    // tighter syncing cadence is in effect.
    await vi.advanceTimersByTimeAsync(1000);
    expect(refreshSyncStatus).toHaveBeenCalledTimes(1);

    unmount();
    await vi.advanceTimersByTimeAsync(10000);
    // Cleanup cleared the pending timer — no further polls after unmount.
    expect(refreshSyncStatus).toHaveBeenCalledTimes(1);
  });
});
