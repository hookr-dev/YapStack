import {
  act,
  render,
  screen,
  cleanup,
  waitFor,
  within,
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
import type { SyncStatus, RelayConnState, DeviceRosterEntry } from "@/lib/sync";
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
      { fingerprint: "AAAABBBBCCCCDDDD", isSelf: true, pending: false, label: null, platform: null },
    ],
    vaultKeyEpoch: 0,
    rosterFingerprint: "EEEEFFFFGGGGHHHH",
    syncEnabled: true,
    lastError: null,
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
      deviceFingerprint: null,
    },
    syncStatus: status,
    relayConn,
    settingsRequest: null,
    refreshSyncStatus: vi.fn().mockResolvedValue(undefined),
    probeRelay: vi.fn().mockResolvedValue(undefined),
    resetProbe: vi.fn(),
    setSyncConfig: vi.fn(),
    setSyncStatus: vi.fn(),
  });
}

beforeEach(() => vi.clearAllMocks());
afterEach(() => cleanup());


const SELF: DeviceRosterEntry = {
  fingerprint: "AAAABBBBCCCCDDDD",
  isSelf: true,
  pending: false,
  label: "Mac Studio",
  platform: "macos",
};
const PEER: DeviceRosterEntry = {
  fingerprint: "PPPPQQQQRRRRSSSS",
  isSelf: false,
  pending: false,
  label: "Work Laptop",
  platform: "windows",
};

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

  it("renders the not-in-this-build placeholder instead of the setup hero", () => {
    setup({ status: missing() });
    render(<SyncTab />);
    expect(screen.getByText("Not in this build")).toBeInTheDocument();
    expect(screen.getByText("docs/self-hosting.md")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Set up sync" })).not.toBeInTheDocument();
    // Not surfaced as a "Connection problem" — the server is not the issue.
    expect(screen.queryByText("Connection problem")).not.toBeInTheDocument();
  });

  it("keeps the real UI for an actual status failure", () => {
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
    expect(screen.queryByText("Not in this build")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Set up sync" })).toBeInTheDocument();
  });
});

describe("SyncTab — not set up", () => {
  it("signed out: the hero and nothing to configure", () => {
    setup();
    render(<SyncTab />);
    expect(screen.getByText("Your notes on every device")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Set up sync" })).toBeInTheDocument();
    expect(screen.queryByLabelText(/server/i)).not.toBeInTheDocument();
    expect(screen.queryByText("Devices")).not.toBeInTheDocument();
  });

  it("Set up sync opens the wizard on its intro", async () => {
    setup({ serverUrl: "" });
    render(<SyncTab />);
    await userEvent.click(screen.getByRole("button", { name: "Set up sync" }));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByRole("button", { name: /Get started/ })).toBeInTheDocument();
  });

  it("a signed-in device that never turned sync on is offered 'Continue setup'", async () => {
    setup({
      email: "user@example.com",
      status: makeStatus({ syncEnabled: false, roster: [SELF] }),
    });
    render(<SyncTab />);
    expect(screen.getByText(/Signed in as user@example.com/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Continue setup" }));
    // Resumes at the turn-on step, not the intro.
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByRole("button", { name: "Turn on sync" })).toBeInTheDocument();
  });

  it("a device waiting for approval says so and resumes at the approval step", async () => {
    setup({
      email: "user@example.com",
      status: makeStatus({
        syncEnabled: false,
        rosterFingerprint: "EEEEFFFFGGGGHHHH",
        vaultKeyEpoch: 3,
        roster: [{ ...SELF, pending: true }, PEER],
      }),
    });
    render(<SyncTab />);
    expect(screen.getByText(/waiting for one of your other devices/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Continue setup" }));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText("Approve this device")).toBeInTheDocument();
    expect(within(dialog).getByText("AAAA-BBBB-CCCC-DDDD")).toBeInTheDocument();
    expect(within(dialog).getByText(/Security code EEEE-FFFF-GGGG-HHHH · key v3/)).toBeInTheDocument();
    expect(within(dialog).getByText(/On Work Laptop, open Settings/)).toBeInTheDocument();
  });

  it("a 'sync-setup' deep link opens the wizard on mount and clears itself", async () => {
    setup();
    const setSettingsRequest = vi.fn();
    useAppStore.setState({ settingsRequest: "sync-setup", setSettingsRequest });
    render(<SyncTab />);
    await waitFor(() => expect(screen.getByRole("dialog")).toBeInTheDocument());
    expect(setSettingsRequest).toHaveBeenCalledWith(null);
  });
});

describe("SyncTab — on", () => {
  it("shows devices as named tiles with this-device marked, plus Add a device", () => {
    setup({ email: "user@example.com", status: makeStatus({ roster: [SELF, PEER] }) });
    render(<SyncTab />);
    expect(screen.getByText("Mac Studio")).toBeInTheDocument();
    expect(screen.getByText("This device")).toBeInTheDocument();
    expect(screen.getByText("Work Laptop")).toBeInTheDocument();
    expect(screen.getByText("Windows")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Add a device/ })).toBeInTheDocument();
    // No hero, no setup.
    expect(screen.queryByRole("button", { name: "Set up sync" })).not.toBeInTheDocument();
  });

  it("a requesting device is an amber tile whose Review opens the ceremony with both codes", async () => {
    setup({
      email: "user@example.com",
      status: makeStatus({
        rosterFingerprint: "EEEEFFFFGGGGHHHH",
        vaultKeyEpoch: 2,
        roster: [SELF, { ...PEER, pending: true }],
      }),
    });
    render(<SyncTab />);
    expect(screen.getByText("Wants to join")).toBeInTheDocument();
    // Named once — the request tile, not also an approved tile.
    expect(screen.getAllByText("Work Laptop")).toHaveLength(1);
    await userEvent.click(screen.getByRole("button", { name: "Review" }));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText("PPPP-QQQQ-RRRR-SSSS")).toBeInTheDocument();
    expect(within(dialog).getByText(/Security code EEEE-FFFF-GGGG-HHHH · key v2/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Approve device" })).toBeDisabled();
  });

  it("Add a device opens the guide with this account's server and email", async () => {
    setup({
      email: "user@example.com",
      status: makeStatus({ roster: [SELF], serverUrl: "https://relay.example.com" }),
    });
    render(<SyncTab />);
    await userEvent.click(screen.getByRole("button", { name: /Add a device/ }));
    const sheet = screen.getByRole("dialog");
    expect(within(sheet).getByText("https://relay.example.com")).toBeInTheDocument();
    expect(within(sheet).getByText("user@example.com")).toBeInTheDocument();
    expect(within(sheet).getByText(/Approve it from here/)).toBeInTheDocument();
  });

  it("shows the server host and account as quiet rows", () => {
    setup({
      email: "user@example.com",
      status: makeStatus({ roster: [SELF], serverUrl: "https://relay.example.com" }),
    });
    render(<SyncTab />);
    expect(screen.getByText("relay.example.com")).toBeInTheDocument();
    expect(screen.getByText("user@example.com")).toBeInTheDocument();
  });

  it("an expired sign-in offers Sign in, which opens the wizard with the email locked", async () => {
    setup({
      email: "user@example.com",
      status: makeStatus({ phase: "auth_expired", roster: [SELF] }),
    });
    render(<SyncTab />);
    expect(screen.getByText(/Your sign-in expired/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText("Sign in again")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Email")).toHaveValue("user@example.com");
    expect(within(dialog).getByLabelText("Email")).toBeDisabled();
  });

  it("routes sign-out through a confirmation", async () => {
    setup({ email: "user@example.com", status: makeStatus({ roster: [SELF] }) });
    render(<SyncTab />);
    await userEvent.click(screen.getByRole("button", { name: "Sign out" }));
    const dialog = screen.getByRole("alertdialog");
    expect(within(dialog).getByText("Sign out of sync on this device?")).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("button", { name: "Sign out" }));
    await waitFor(() =>
      expect(useAppStore.getState().setSyncStatus).toHaveBeenCalledWith(null),
    );
  });

  it("surfaces a verbatim connection error under the heading", () => {
    setup({
      email: "user@example.com",
      status: makeStatus({ roster: [SELF], lastError: "relay unreachable: connection refused" }),
    });
    render(<SyncTab />);
    expect(screen.getByText("Connection problem")).toBeInTheDocument();
    expect(screen.getByText("relay unreachable: connection refused")).toBeInTheDocument();
  });
});

describe("SyncTab — audio backup row (S2)", () => {
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
    expect(screen.getByText("Nothing waiting")).toBeInTheDocument();
  });

  it("animates in rather than popping into the stack", () => {
    setup({ status: makeStatus({ audioUploadOutstanding: 1 }) });
    const { container } = render(<SyncTab />);
    const row = screen.getByText("Audio backup").closest(".view-enter");
    expect(row).toBeTruthy();
    expect(container).toBeTruthy();
  });
});

describe("SyncTab — downloaded-audio row (S3.5)", () => {
  async function mockCache(bytes: number, files: number) {
    const { invoke } = await import("@tauri-apps/api/core");
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_cache_stats") return { bytes, files };
      if (cmd === "audio_cache_clear") return { bytes: 0, files: 0 };
      return null;
    });
    return vi.mocked(invoke);
  }

  it("shows the downloaded-audio row once the cache holds fetched audio", async () => {
    await mockCache(5 * 1024 * 1024, 3);
    setup({ email: "user@example.com", status: makeStatus() });
    render(<SyncTab />);
    expect(await screen.findByText(/5\.0 MB from your other devices/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Clear" })).toBeInTheDocument();
  });

  it("is hidden while the cache is empty", async () => {
    const invoke = await mockCache(0, 0);
    setup({ email: "user@example.com", status: makeStatus() });
    render(<SyncTab />);
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("audio_cache_stats"),
    );
    expect(screen.queryByText("Downloaded audio")).not.toBeInTheDocument();
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
      within(dialog).getByText("Clear downloaded audio?"),
    ).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("button", { name: "Clear" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("audio_cache_clear"),
    );
    // The refreshed (now empty) footprint hides the row.
    await waitFor(() =>
      expect(screen.queryByText("Downloaded audio")).not.toBeInTheDocument(),
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
      expect(screen.queryByText("Downloaded audio")).not.toBeInTheDocument();

      // A background fetch lands while the panel sits open.
      bytes = 2 * 1024 * 1024;
      await vi.advanceTimersByTimeAsync(5000);
      expect(screen.getByText(/2\.0 MB from your other devices/)).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("animates in rather than popping into the stack", async () => {
    await mockCache(5 * 1024 * 1024, 3);
    setup({ email: "user@example.com", status: makeStatus() });
    render(<SyncTab />);
    const row = (await screen.findByText("Downloaded audio")).closest(".view-enter");
    expect(row).toBeTruthy();
  });
});
