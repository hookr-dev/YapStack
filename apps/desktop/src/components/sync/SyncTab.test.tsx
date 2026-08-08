import {
  act,
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
vi.mock("@/lib/db-backend", () => tauriSqlMock());
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
    pullBehind: 0,
    cryptoQuarantined: 0,
    audioUploadOutstanding: 0,
    audioBackfillOutstanding: 0,
    audioUploadFailed: 0,
    audioUploadedTotal: 0,
    // Healthy default: a backfill walk has covered this device's library at least once.
    // Tests that care about the incomplete-walk caveat opt in explicitly.
    audioBackfillComplete: true,
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

describe("SyncTab — build without the sync feature", () => {
  /** What `refreshSyncStatus` parks in the store when `sync_status` is not a
   *  registered command: phase "error" + Tauri's verbatim rejection string. */
  const missing = () =>
    makeStatus({
      phase: "error",
      email: null,
      syncEnabled: false,
      roster: [],
      lastError: "Command sync_status not found",
    });

  it("renders the not-in-this-build placeholder instead of the dead controls", () => {
    setup({ status: missing() });
    render(<SyncTab />);
    expect(screen.getByText("Sync isn’t included in this build")).toBeInTheDocument();
    expect(screen.getByText("docs/self-hosting.md")).toBeInTheDocument();
    expect(screen.getByText("--features sync")).toBeInTheDocument();
    // None of the sync controls (all of which would fail) are rendered.
    expect(screen.queryByText("Relay server")).not.toBeInTheDocument();
    expect(screen.queryByText("Account")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Test connection" }),
    ).not.toBeInTheDocument();
    // Not surfaced as a "Connection problem" — the relay is not the issue.
    expect(screen.queryByText("Connection problem")).not.toBeInTheDocument();
  });

  it("keeps the real UI (and the verbatim error) for an actual status failure", () => {
    setup({
      status: makeStatus({
        phase: "error",
        email: null,
        syncEnabled: false,
        roster: [],
        lastError: "relay unreachable: connection refused",
      }),
    });
    render(<SyncTab />);
    expect(
      screen.queryByText("Sync isn’t included in this build"),
    ).not.toBeInTheDocument();
    expect(screen.getByText("Relay server")).toBeInTheDocument();
    expect(screen.getByText("Connection problem")).toBeInTheDocument();
    expect(
      screen.getByText("relay unreachable: connection refused"),
    ).toBeInTheDocument();
  });
});

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

describe("SyncTab — audio backup card (S2)", () => {
  it("is hidden when there is no audio activity", () => {
    setup({ status: makeStatus() });
    render(<SyncTab />);
    expect(screen.queryByText("Audio backup")).not.toBeInTheDocument();
  });

  it("shows an in-flight upload count with the library-backfill nuance", () => {
    setup({
      status: makeStatus({
        audioUploadOutstanding: 10,
        audioBackfillOutstanding: 4,
      }),
    });
    render(<SyncTab />);
    expect(screen.getByText("Audio backup")).toBeInTheDocument();
    expect(
      screen.getByText("Backing up 10 recordings (4 from your existing library)"),
    ).toBeInTheDocument();
  });

  it("shows an all-backed-up resting line", () => {
    setup({ status: makeStatus({ audioUploadedTotal: 7 }) });
    render(<SyncTab />);
    expect(screen.getByText("7 recordings backed up")).toBeInTheDocument();
  });

  it("surfaces failures with a Retry that re-arms the lane", async () => {
    const { invoke } = await import("@tauri-apps/api/core");
    vi.mocked(invoke).mockResolvedValue(3);
    setup({ status: makeStatus({ audioUploadFailed: 2 }) });
    render(<SyncTab />);
    expect(
      screen.getByText("2 recordings failed to back up"),
    ).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Retry/ }));
    await waitFor(() =>
      expect(vi.mocked(invoke)).toHaveBeenCalledWith(
        "audio_retry_failed_uploads",
      ),
    );
  });

  it("is not shown before sync is enabled", () => {
    setup({
      status: makeStatus({ syncEnabled: false, audioUploadOutstanding: 5 }),
    });
    render(<SyncTab />);
    expect(screen.queryByText("Audio backup")).not.toBeInTheDocument();
  });

  it("says so when the library walk never finished, instead of claiming all-backed-up", () => {
    // The boot-contention case the owner hit: the walk stepped over parts that are
    // therefore in NO queue and NO count. Nothing here is outstanding or failed.
    setup({
      status: makeStatus({
        audioUploadedTotal: 42,
        audioBackfillComplete: false,
      }),
    });
    render(<SyncTab />);
    expect(screen.queryByText("42 recordings backed up")).not.toBeInTheDocument();
    expect(
      screen.getByText("Some audio not yet backed up — retries next launch"),
    ).toBeInTheDocument();
    // No Retry: `audio_retry_failed_uploads` re-pends rows already IN the queue, and a
    // skipped part has no row. Only the next launch's walk can pick it up.
    expect(screen.queryByRole("button", { name: /Retry/ })).not.toBeInTheDocument();
  });

  it("collapses to a resting line — not an unmount — once the counts fall back to zero", () => {
    // Unmounting a card from the MIDDLE of the stack jumps every card below it while
    // the user is reading them.
    setup({ status: makeStatus({ audioUploadOutstanding: 2 }) });
    const { rerender } = render(<SyncTab />);
    expect(screen.getByText("Backing up 2 recordings")).toBeInTheDocument();

    act(() => {
      useAppStore.setState({ syncStatus: makeStatus() });
    });
    rerender(<SyncTab />);

    expect(screen.queryByText("Backing up 2 recordings")).not.toBeInTheDocument();
    expect(screen.getByText("Audio backup — nothing waiting")).toBeInTheDocument();
  });

  it("animates in rather than popping into the stack", () => {
    setup({ status: makeStatus({ audioUploadOutstanding: 1 }) });
    const { container } = render(<SyncTab />);
    const card = screen.getByText("Audio backup").closest("[data-slot='card']");
    expect(card?.className).toContain("view-enter");
    expect(container).toBeTruthy();
  });
});

describe("SyncTab — fetched-audio cache row (S3.5)", () => {
  async function mockCache(bytes: number, files: number) {
    const { invoke } = await import("@tauri-apps/api/core");
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_cache_stats") return { bytes, files };
      if (cmd === "audio_cache_clear") return { bytes: 0, files: 0 };
      return null;
    });
    return vi.mocked(invoke);
  }

  it("shows 'Fetched audio: N MB' once the cache holds fetched audio", async () => {
    await mockCache(5 * 1024 * 1024, 3);
    setup({ email: "user@example.com", status: makeStatus() });
    render(<SyncTab />);
    expect(await screen.findByText("Fetched audio: 5.0 MB")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Clear" })).toBeInTheDocument();
  });

  it("is hidden while the cache is empty", async () => {
    const invoke = await mockCache(0, 0);
    setup({ email: "user@example.com", status: makeStatus() });
    render(<SyncTab />);
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("audio_cache_stats"),
    );
    expect(screen.queryByText(/Fetched audio:/)).not.toBeInTheDocument();
  });

  it("clears only through the confirm dialog, then hides the emptied row", async () => {
    const invoke = await mockCache(5 * 1024 * 1024, 3);
    setup({ email: "user@example.com", status: makeStatus() });
    render(<SyncTab />);
    await userEvent.click(await screen.findByRole("button", { name: "Clear" }));
    // No clear before the confirm.
    expect(invoke).not.toHaveBeenCalledWith("audio_cache_clear");
    const dialog = screen.getByRole("alertdialog");
    expect(
      within(dialog).getByText("Clear fetched audio?"),
    ).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("button", { name: "Clear" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("audio_cache_clear"),
    );
    // The refreshed (now empty) footprint hides the row.
    await waitFor(() =>
      expect(screen.queryByText(/Fetched audio:/)).not.toBeInTheDocument(),
    );
  });

  it("picks the cache up on the panel's idle poll, not only on mount", async () => {
    // Auto-fetch fills this cache from any view. Refreshing only on mount meant the
    // row materialised on the NEXT visit to Settings, shoving the cards below it.
    let bytes = 0;
    const { invoke } = await import("@tauri-apps/api/core");
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_cache_stats") return { bytes, files: bytes > 0 ? 1 : 0 };
      return null;
    });
    setup({ email: "user@example.com", status: makeStatus() });
    vi.useFakeTimers();
    try {
      render(<SyncTab />);
      await vi.advanceTimersByTimeAsync(0);
      expect(invoke).toHaveBeenCalledWith("audio_cache_stats");
      expect(screen.queryByText(/Fetched audio:/)).not.toBeInTheDocument();

      // A background fetch lands while the panel sits open.
      bytes = 2 * 1024 * 1024;
      await vi.advanceTimersByTimeAsync(5000);
      expect(screen.getByText("Fetched audio: 2.0 MB")).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("animates in rather than popping into the stack", async () => {
    await mockCache(5 * 1024 * 1024, 3);
    setup({ email: "user@example.com", status: makeStatus() });
    render(<SyncTab />);
    const card = (await screen.findByText("Fetched audio: 5.0 MB")).closest(
      "[data-slot='card']",
    );
    expect(card?.className).toContain("view-enter");
  });
});
