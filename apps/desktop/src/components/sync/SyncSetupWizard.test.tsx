import { act, render, screen, cleanup, waitFor, within } from "@testing-library/react";
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
import type { DeviceRosterEntry, RelayConnState, SyncStatus } from "@/lib/sync";
import { SyncSetupWizard } from "./SyncSetupWizard";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => {
  const toast = Object.assign(vi.fn(), {
    success: vi.fn(),
    error: vi.fn(),
    dismiss: vi.fn(),
  });
  return { toast };
});
import { toast } from "sonner";

const invokeMock = vi.mocked(invoke);

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

function makeStatus(overrides: Partial<SyncStatus> = {}): SyncStatus {
  return {
    phase: "connected",
    serverUrl: "https://relay.example.com",
    email: "user@example.com",
    deviceFingerprint: "AAAABBBBCCCCDDDD",
    roster: [SELF],
    vaultKeyEpoch: 0,
    rosterFingerprint: "EEEEFFFFGGGGHHHH",
    syncEnabled: false,
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
    audioBackfillComplete: true,
    ...overrides,
  };
}

const OK_CONN = (url: string): RelayConnState => ({
  kind: "ok",
  engineVersion: "0.16.3",
  protocolVersion: 1,
  latencyMs: 42,
  normalizedUrl: url,
  versionAdvisory: null,
});

/** Store-driven setup. `setSyncStatus` stays REAL so a step that receives a status
 *  (login finish, enable) drives the next step through the store like production. */
function setup({
  serverUrl = "",
  email = null as string | null,
  status = null as SyncStatus | null,
  relayConn = { kind: "idle" } as RelayConnState,
} = {}) {
  useAppStore.setState({
    syncConfig: { serverUrl, syncEnabled: false, email, deviceFingerprint: null },
    syncStatus: status,
    relayConn,
    refreshSyncStatus: vi.fn().mockResolvedValue(undefined),
    probeRelay: vi.fn().mockResolvedValue(undefined),
    resetProbe: vi.fn(),
  });
}

function renderWizard(start: "auto" | "sign-in" = "auto") {
  const onOpenChange = vi.fn();
  render(<SyncSetupWizard open onOpenChange={onOpenChange} start={start} />);
  return { onOpenChange, dialog: () => screen.getByRole("dialog") };
}

beforeEach(() => {
  vi.clearAllMocks();
  invokeMock.mockResolvedValue(null);
});
afterEach(() => cleanup());

describe("SyncSetupWizard — resume point", () => {
  it("starts on the intro when nothing is configured", () => {
    setup();
    const { dialog } = renderWizard();
    expect(within(dialog()).getByText("Your notes on every device")).toBeInTheDocument();
  });

  it("starts on the server step when a server is already saved", () => {
    setup({ serverUrl: "https://relay.example.com" });
    const { dialog } = renderWizard();
    expect(within(dialog()).getByLabelText("Server address")).toHaveValue(
      "https://relay.example.com",
    );
  });

  it("starts on turn-on for a signed-in, approved device", () => {
    setup({ email: "user@example.com", status: makeStatus() });
    const { dialog } = renderWizard();
    expect(within(dialog()).getByRole("button", { name: "Turn on sync" })).toBeInTheDocument();
  });

  it("starts on approval for a signed-in device that is still pending", () => {
    setup({
      email: "user@example.com",
      status: makeStatus({ roster: [{ ...SELF, pending: true }, PEER] }),
    });
    const { dialog } = renderWizard();
    expect(within(dialog()).getByText("Approve this device")).toBeInTheDocument();
  });
});

describe("SyncSetupWizard — server step", () => {
  it("probes when the field settles and continues once the probe saved the address", async () => {
    setup();
    const { dialog } = renderWizard();
    await userEvent.click(within(dialog()).getByRole("button", { name: /Get started/ }));
    const input = within(dialog()).getByLabelText("Server address");
    await userEvent.type(input, "https://relay.example.com");
    await userEvent.tab();
    await waitFor(() =>
      expect(useAppStore.getState().probeRelay).toHaveBeenCalledWith("https://relay.example.com"),
    );
    // Not yet: the probe has not come back.
    expect(within(dialog()).queryByText("Your account")).not.toBeInTheDocument();

    // The store's probe resolves + persists (what the real probeRelay does on success).
    act(() => {
      useAppStore.setState({
        relayConn: OK_CONN("https://relay.example.com"),
        syncConfig: { ...useAppStore.getState().syncConfig, serverUrl: "https://relay.example.com" },
      });
    });
    expect(within(dialog()).getByText(/Connected · 42 ms/)).toBeInTheDocument();
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    expect(within(dialog()).getByText("Your account")).toBeInTheDocument();
  });

  it("shows the verbatim failure with a use-anyway escape hatch", async () => {
    setup({ serverUrl: "" });
    const { dialog } = renderWizard();
    await userEvent.click(within(dialog()).getByRole("button", { name: /Get started/ }));
    await userEvent.type(within(dialog()).getByLabelText("Server address"), "https://bad.example");
    act(() => {
      useAppStore.setState({
        relayConn: { kind: "unreachable", raw: "connection refused (os error 61)" },
      });
    });
    expect(within(dialog()).getByText("Can't reach that server")).toBeInTheDocument();
    expect(within(dialog()).getByRole("alert").textContent).toBe("connection refused (os error 61)");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Use this address anyway/ }));
    expect(useAppStore.getState().syncConfig.serverUrl).toBe("https://bad.example");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    expect(within(dialog()).getByText("Your account")).toBeInTheDocument();
  });
});

describe("SyncSetupWizard — sign in", () => {
  function atAccount() {
    setup({ serverUrl: "https://relay.example.com", relayConn: OK_CONN("https://relay.example.com") });
    const w = renderWizard();
    return w;
  }

  it("runs the two-round login and lands on approval when this device is pending", async () => {
    const { dialog } = atAccount();
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "sync_login_begin") return { saltMismatch: false };
      if (cmd === "sync_login_finish")
        return makeStatus({ roster: [{ ...SELF, pending: true }, PEER] });
      return null;
    });
    await userEvent.type(within(dialog()).getByLabelText("Email"), "user@example.com");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("sync_login_begin", {
        serverUrl: "https://relay.example.com",
        email: "user@example.com",
      }),
    );
    await userEvent.type(within(dialog()).getByLabelText(/Password for/), "hunter22");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Sign in/ }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("sync_login_finish", { password: "hunter22" }),
    );
    expect(await within(dialog()).findByText("Approve this device")).toBeInTheDocument();
    expect(within(dialog()).getByText("AAAA-BBBB-CCCC-DDDD")).toBeInTheDocument();
    expect(within(dialog()).getByText(/On Work Laptop, open Settings/)).toBeInTheDocument();
  });

  it("after a recovery sign-in, offers a new password before moving on", async () => {
    const { dialog } = atAccount();
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "sync_login_begin") return { saltMismatch: false };
      if (cmd === "sync_recover") return makeStatus({ roster: [SELF, PEER] });
      if (cmd === "sync_change_password") return undefined;
      return null;
    });
    await userEvent.type(within(dialog()).getByLabelText("Email"), "user@example.com");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    await userEvent.click(
      await within(dialog()).findByRole("button", { name: /Forgot your password/ }),
    );
    await userEvent.type(
      within(dialog()).getByLabelText("Recovery code"),
      "ABCD-EFGH-IJKL-MNOP-QRST-UVWX-YZ23-4567",
    );
    await userEvent.click(within(dialog()).getByRole("button", { name: /Recover/ }));
    expect(await within(dialog()).findByText("Set a new password")).toBeInTheDocument();
    await userEvent.type(within(dialog()).getByLabelText("New password"), "fresh-secret-1");
    await userEvent.type(
      within(dialog()).getByLabelText("Confirm new password"),
      "fresh-secret-1",
    );
    await userEvent.click(within(dialog()).getByRole("button", { name: "Change password" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("sync_change_password", {
        currentPassword: null,
        newPassword: "fresh-secret-1",
      }),
    );
    // Recovered as an approved member → lands on turn-on.
    expect(
      await within(dialog()).findByRole("heading", { name: "Turn on sync" }),
    ).toBeInTheDocument();
  });

  it("hard-stops on a salt mismatch and never shows the password field", async () => {
    const { dialog } = atAccount();
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    invokeMock.mockImplementation(async (cmd: string) =>
      cmd === "sync_login_begin" ? { saltMismatch: true } : null,
    );
    await userEvent.type(within(dialog()).getByLabelText("Email"), "user@example.com");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    expect(await within(dialog()).findByText("Stopped for your safety")).toBeInTheDocument();
    expect(within(dialog()).queryByLabelText(/Password/)).not.toBeInTheDocument();
  });

  it("lands on turn-on for an already-approved device, and Turn on enables sync", async () => {
    const { dialog } = atAccount();
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "sync_login_begin") return { saltMismatch: false };
      if (cmd === "sync_login_finish") return makeStatus({ roster: [SELF, PEER] });
      if (cmd === "sync_enable") return makeStatus({ syncEnabled: true, roster: [SELF, PEER] });
      return null;
    });
    await userEvent.type(within(dialog()).getByLabelText("Email"), "user@example.com");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    await userEvent.type(await within(dialog()).findByLabelText(/Password for/), "hunter22");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Sign in/ }));
    const turnOn = await within(dialog()).findByRole("button", { name: "Turn on sync" });
    // Joining an account with peers: the copy says things download here.
    expect(within(dialog()).getByText(/download here/)).toBeInTheDocument();
    await userEvent.click(turnOn);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("sync_enable"));
    expect(await within(dialog()).findByText("Sync is on")).toBeInTheDocument();
    expect(within(dialog()).getByText(/keeps in step with your other device/)).toBeInTheDocument();
  });

  it("surfaces a turn-on failure verbatim and stays on the step", async () => {
    setup({ email: "user@example.com", status: makeStatus() });
    const { dialog } = renderWizard();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "sync_enable") throw new Error("cutover failed: disk full");
      return null;
    });
    await userEvent.click(within(dialog()).getByRole("button", { name: "Turn on sync" }));
    expect(await within(dialog()).findByText("cutover failed: disk full")).toBeInTheDocument();
    expect(within(dialog()).getByRole("button", { name: "Turn on sync" })).toBeInTheDocument();
  });
});

describe("SyncSetupWizard — create account", () => {
  it("forces the recovery code to be acknowledged and typed back before finishing", async () => {
    setup({ serverUrl: "https://relay.example.com", relayConn: OK_CONN("https://relay.example.com") });
    const { dialog, onOpenChange } = renderWizard();
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    await userEvent.click(within(dialog()).getByRole("tab", { name: "Create account" }));
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "sync_signup")
        return { recoveryCode: "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567", deviceFingerprint: "AAAABBBBCCCCDDDD" };
      return null;
    });
    await userEvent.type(within(dialog()).getByLabelText("Email"), "new@example.com");
    await userEvent.type(within(dialog()).getByLabelText("Password"), "longenough");
    await userEvent.type(within(dialog()).getByLabelText("Confirm password"), "longenough");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Create account/ }));

    expect(await within(dialog()).findByText("Save your recovery code")).toBeInTheDocument();
    expect(within(dialog()).getByText("ABCD-EFGH-IJKL-MNOP-QRST-UVWX-YZ23-4567")).toBeInTheDocument();
    // Locked: no close button, and Escape is refused with a toast.
    expect(within(dialog()).queryByRole("button", { name: "Close" })).not.toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    expect(onOpenChange).not.toHaveBeenCalledWith(false);

    const cont = within(dialog()).getByRole("button", { name: /Continue/ });
    expect(cont).toBeDisabled();
    await userEvent.click(within(dialog()).getByRole("checkbox"));
    await userEvent.click(cont);

    expect(within(dialog()).getByText("Type it back to confirm")).toBeInTheDocument();
    const finish = within(dialog()).getByRole("button", { name: /Finish/ });
    expect(finish).toBeDisabled();
    await userEvent.type(within(dialog()).getByRole("textbox"), "abcd-efgh-ijkl-mnop-qrst-uvwx-yz23-4567");
    expect(finish).toBeEnabled();
    await userEvent.click(finish);
    await waitFor(() => expect(useAppStore.getState().refreshSyncStatus).toHaveBeenCalled());
    expect(toast.error).not.toHaveBeenCalled();
  });
});

describe("SyncSetupWizard — approval step", () => {
  it("polls status and moves on by itself once the other device approves", async () => {
    vi.useFakeTimers();
    try {
      setup({
        email: "user@example.com",
        status: makeStatus({ roster: [{ ...SELF, pending: true }, PEER] }),
      });
      const { dialog } = renderWizard();
      expect(within(dialog()).getByText("Approve this device")).toBeInTheDocument();
      await vi.advanceTimersByTimeAsync(3100);
      expect(useAppStore.getState().refreshSyncStatus).toHaveBeenCalled();

      act(() => {
        useAppStore.setState({ syncStatus: makeStatus({ roster: [SELF, PEER] }) });
      });
      await vi.advanceTimersByTimeAsync(0);
      expect(within(dialog()).getByRole("button", { name: "Turn on sync" })).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("SyncSetupWizard — expired session re-login", () => {
  it("starts on the account step with the email locked and finishes after sign-in", async () => {
    setup({ serverUrl: "https://relay.example.com", email: "user@example.com", status: makeStatus({ phase: "auth_expired", syncEnabled: true }) });
    const { dialog } = renderWizard("sign-in");
    expect(within(dialog()).getByText("Sign in again")).toBeInTheDocument();
    const email = within(dialog()).getByLabelText("Email");
    expect(email).toHaveValue("user@example.com");
    expect(email).toBeDisabled();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "sync_login_begin") return { saltMismatch: false };
      if (cmd === "sync_login_finish") return makeStatus({ syncEnabled: true });
      return null;
    });
    await userEvent.click(within(dialog()).getByRole("button", { name: /Continue/ }));
    await userEvent.type(await within(dialog()).findByLabelText(/Password for/), "hunter22");
    await userEvent.click(within(dialog()).getByRole("button", { name: /Sign in/ }));
    expect(await within(dialog()).findByText("You're signed in")).toBeInTheDocument();
  });
});
