import {
  render,
  screen,
  cleanup,
  waitFor,
  within,
  fireEvent,
} from "@testing-library/react";
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
  tauriOpenerMock,
} from "@/test/tauri-mocks";
import { useAppStore } from "@/stores/appStore";
import type { SyncStatus, RelayConnState } from "@/lib/sync";
import { SyncTab } from "./SyncTab";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@tauri-apps/plugin-sql", () => tauriSqlMock());
vi.mock("@tauri-apps/plugin-opener", () => tauriOpenerMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

function makeStatus(overrides: Partial<SyncStatus> = {}): SyncStatus {
  return {
    phase: "connected",
    serverUrl: "https://relay.example.com",
    email: "user@example.com",
    deviceFingerprint: "AAAABBBBCCCCDDDD",
    roster: [
      { fingerprint: "AAAABBBBCCCCDDDD", isSelf: true, pending: false, label: null },
    ],
    vaultKeyEpoch: 0,
    rosterFingerprint: "EEEEFFFFGGGGHHHH",
    syncEnabled: true,
    lastError: null,
    billingUrl: null,
    pendingEntries: 0,
    pendingBytes: 0,
    ackedThisSession: 0,
    lastSuccess: null,
    ...overrides,
  };
}

/** Drive the component purely from store state; stub the actions so the mount
 *  status-refresh never overwrites the fixture. */
function setup({
  status = null,
  relayConn = { kind: "idle" } as RelayConnState,
  serverUrl = "https://sync.yapstack.app",
  email = null as string | null,
}: {
  status?: SyncStatus | null;
  relayConn?: RelayConnState;
  serverUrl?: string;
  email?: string | null;
} = {}) {
  useAppStore.setState({
    syncConfig: {
      serverUrl,
      syncEnabled: false,
      email,
      lastChangesetSeq: 0,
      deviceFingerprint: null,
    },
    syncStatus: status,
    relayConn,
    refreshSyncStatus: vi.fn().mockResolvedValue(undefined),
    probeRelay: vi.fn().mockResolvedValue(undefined),
    resetProbe: vi.fn(),
    setSyncConfig: vi.fn(),
    setSyncStatus: vi.fn(),
  });
}

beforeEach(() => vi.clearAllMocks());
afterEach(() => cleanup());

describe("SyncTab", () => {
  it("signed-out shows only the Relay + Account cards", () => {
    setup();
    render(<SyncTab />);
    expect(screen.getByText("Relay server")).toBeInTheDocument();
    expect(screen.getByText("Account")).toBeInTheDocument();
    expect(screen.queryByText("Devices")).not.toBeInTheDocument();
    expect(screen.queryByText("Enable sync")).not.toBeInTheDocument();
  });

  it("clicking Test connection probes the entered URL", async () => {
    setup();
    render(<SyncTab />);
    await userEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(useAppStore.getState().probeRelay).toHaveBeenCalledWith(
      "https://sync.yapstack.app",
    );
  });

  it("auto-probes once on blur when the URL is dirty + valid", async () => {
    setup();
    render(<SyncTab />);
    const input = screen.getByLabelText("Relay server URL");
    await userEvent.clear(input);
    await userEvent.type(input, "https://relay.example.com");
    fireEvent.blur(input);
    await waitFor(() =>
      expect(useAppStore.getState().probeRelay).toHaveBeenCalledWith(
        "https://relay.example.com",
      ),
    );
  });

  it("renders the collapsed connected line on probe success", () => {
    setup({
      relayConn: {
        kind: "ok",
        engineVersion: "0.16.3",
        protocolVersion: 1,
        latencyMs: 42,
        normalizedUrl: "https://relay.example.com",
        versionAdvisory: null,
      },
    });
    render(<SyncTab />);
    expect(
      screen.getByText("engine v0.16.3", { exact: false }),
    ).toBeInTheDocument();
    expect(screen.getByText("42 ms", { exact: false })).toBeInTheDocument();
  });

  it("shows an explicit 'saved' cue once the probed URL is persisted", () => {
    // A signed-out successful probe auto-persists the normalized URL into syncConfig, so
    // savedUrl === normalizedUrl → the muted line must read "· saved" (owner asked for an
    // explicit save cue for the relay connection).
    setup({
      serverUrl: "https://relay.example.com",
      relayConn: {
        kind: "ok",
        engineVersion: "0.16.3",
        protocolVersion: 1,
        latencyMs: 7,
        normalizedUrl: "https://relay.example.com",
        versionAdvisory: null,
      },
    });
    render(<SyncTab />);
    expect(screen.getByText("saved", { exact: false })).toBeInTheDocument();
  });

  it("omits the 'saved' cue when the probed URL was not persisted", () => {
    // normalizedUrl differs from the persisted serverUrl (e.g. a probe of a different URL
    // that has not been saved) → no "saved" suffix.
    setup({
      serverUrl: "https://sync.yapstack.app",
      relayConn: {
        kind: "ok",
        engineVersion: "0.16.3",
        protocolVersion: 1,
        latencyMs: 7,
        normalizedUrl: "https://relay.example.com",
        versionAdvisory: null,
      },
    });
    render(<SyncTab />);
    expect(screen.queryByText("saved", { exact: false })).not.toBeInTheDocument();
  });

  it("renders the verbatim raw error + Save anyway on probe failure", () => {
    setup({
      relayConn: {
        kind: "unreachable",
        raw: "error trying to connect: tcp connect error: Connection refused (os error 61)",
      },
    });
    render(<SyncTab />);
    expect(screen.getByText("Can't reach server")).toBeInTheDocument();
    expect(screen.getByRole("alert").textContent).toBe(
      "error trying to connect: tcp connect error: Connection refused (os error 61)",
    );
    expect(
      screen.getByRole("button", { name: "Save anyway" }),
    ).toBeInTheDocument();
  });

  it("locks the URL as a host identity line when signed in", () => {
    setup({
      email: "user@example.com",
      status: makeStatus(),
      serverUrl: "https://relay.example.com",
    });
    render(<SyncTab />);
    expect(screen.getByText("relay.example.com")).toBeInTheDocument();
    expect(screen.queryByLabelText("Relay server URL")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Change server/ }),
    ).toBeInTheDocument();
  });

  it("shows a warm re-login CTA when the session is expired", () => {
    setup({
      email: "user@example.com",
      status: makeStatus({ phase: "auth_expired" }),
    });
    render(<SyncTab />);
    expect(
      screen.getByText(/sign in again to resume syncing/i),
    ).toBeInTheDocument();
  });

  it("routes sign-out through the Advanced AlertDialog confirmation", async () => {
    setup({
      email: "user@example.com",
      status: makeStatus(),
    });
    render(<SyncTab />);
    await userEvent.click(screen.getByRole("button", { name: "Advanced" }));
    await userEvent.click(
      screen.getByRole("button", { name: "Sign out" }),
    );
    const dialog = screen.getByRole("alertdialog");
    expect(
      within(dialog).getByText("Sign out of sync on this device?"),
    ).toBeInTheDocument();
    // Confirm → handleSignOut awaits sync_sign_out then clears status.
    await userEvent.click(within(dialog).getByRole("button", { name: "Sign out" }));
    await waitFor(() =>
      expect(useAppStore.getState().setSyncStatus).toHaveBeenCalledWith(null),
    );
  });
});
