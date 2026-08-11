import { render, screen, cleanup, fireEvent, waitFor } from "@testing-library/react";
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
import type { DbSegment, DbSession } from "@/lib/db";
import { SessionHeader } from "./SessionHeader";
import { toast } from "sonner";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@tauri-apps/plugin-sql", () => tauriSqlMock());
vi.mock("@tauri-apps/plugin-opener", () => tauriOpenerMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("@aptabase/tauri", () => ({
  trackEvent: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

const { updateSessionTitleMock, getSessionMock } = vi.hoisted(() => ({
  updateSessionTitleMock: vi.fn(),
  getSessionMock: vi.fn(),
}));
vi.mock("@/lib/db", async () => {
  const actual = await vi.importActual<typeof import("@/lib/db")>("@/lib/db");
  return {
    ...actual,
    updateSessionTitle: updateSessionTitleMock,
    getSession: getSessionMock,
  };
});

function makeSession(overrides?: Partial<DbSession>): DbSession {
  return {
    id: "session-1",
    title: "Test Session",
    created_at: "2026-01-01T00:00:00",
    updated_at: "2026-01-01T00:00:00",
    source: "MicOnly",
    status: "completed",
    duration_seconds: 120,
    total_segments: 2,
    folder_id: null,
    is_pinned: 0,
    pinned_at: null,
    session_type: "transcription",
    sort_order: 0,
    recording_device_id: null,
    ...overrides,
  };
}

function seg(id: string, source: "Mic" | "System", hidden = 0): DbSegment {
  return {
    id,
    session_id: "session-1",
    source,
    text: source === "Mic" ? "mine" : "theirs",
    audio_offset_seconds: source === "Mic" ? 1 : 0,
    chunk_duration_seconds: 1,
    confidence: 1,
    created_at: "",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  updateSessionTitleMock.mockResolvedValue(undefined);
  getSessionMock.mockResolvedValue(null);
  // Radix menu items call scrollIntoView on open; jsdom lacks it.
  HTMLElement.prototype.scrollIntoView = vi.fn();
  useAppStore.setState({
    deleteSession: vi.fn(),
    activeSessionId: null,
    navigateTo: vi.fn(),
    loadSessions: vi.fn(),
    resumeSession: vi.fn(),
    stopActiveSession: vi.fn(),
    liveTranscriptionActive: false,
    sessionStopping: false,
    viewSessionParts: [],
    folderTree: [],
    sessionFolderMap: {},
    toggleSessionFolder: vi.fn(),
    removeSessionFromAllFolders: vi.fn(),
    sessionMetaRefreshCounter: 0,
    sessionMetaRefreshSessionId: null,
  });
});

afterEach(() => cleanup());

// The copy formatting is covered by export.test.ts and the clipboard-write path
// by BulkActionsBar.test.tsx; here we verify the session-level gating — that the
// Export submenu appears only when the session has visible transcript content.
describe("SessionHeader — Copy / Export transcript", () => {
  it("offers Copy and Export transcript when the session has visible segments", async () => {
    render(
      <SessionHeader
        session={makeSession()}
        segments={[seg("a", "System"), seg("b", "Mic")]}
      />,
    );
    await userEvent.click(
      screen.getByRole("button", { name: "Session actions" }),
    );
    expect(await screen.findByText("Copy transcript")).toBeInTheDocument();
    expect(screen.getByText("Export transcript")).toBeInTheDocument();
  });

  it("hides both when there are no segments", async () => {
    render(<SessionHeader session={makeSession()} segments={[]} />);
    await userEvent.click(
      screen.getByRole("button", { name: "Session actions" }),
    );
    // Menu is open (Delete is always present for a non-recording session)…
    expect(await screen.findByText("Delete session")).toBeInTheDocument();
    // …but Copy/Export are absent with nothing to export.
    expect(screen.queryByText("Copy transcript")).not.toBeInTheDocument();
    expect(screen.queryByText("Export transcript")).not.toBeInTheDocument();
  });

  it("hides both when every segment is hidden", async () => {
    render(
      <SessionHeader
        session={makeSession()}
        segments={[seg("a", "System", 1), seg("b", "Mic", 1)]}
      />,
    );
    await userEvent.click(
      screen.getByRole("button", { name: "Session actions" }),
    );
    expect(await screen.findByText("Delete session")).toBeInTheDocument();
    expect(screen.queryByText("Copy transcript")).not.toBeInTheDocument();
    expect(screen.queryByText("Export transcript")).not.toBeInTheDocument();
  });
});

// A3: the title input is an edit surface over ONE last-writer-wins cell, so a
// sync-down must not reset it under the user's fingers — and a save that
// supersedes a remote rename must say so instead of overwriting it silently.
describe("SessionHeader — title edit vs. remote rename (sync-UX A3)", () => {
  async function startEditing(title = "Old title") {
    const view = render(<SessionHeader session={makeSession({ title })} />);
    await userEvent.dblClick(screen.getByText(title));
    return view;
  }

  it("keeps the typed title when a remote rename merges in mid-edit", async () => {
    const { rerender } = await startEditing();
    const input = screen.getByRole("textbox");
    await userEvent.clear(input);
    await userEvent.type(input, "My new title");

    // A sync-applied refresh replaces viewSession with the peer's rename.
    rerender(<SessionHeader session={makeSession({ title: "Peer title" })} />);

    expect(screen.getByRole("textbox")).toHaveValue("My new title");
  });

  it("adopts the remote title on blur when the user changed nothing", async () => {
    const { rerender } = await startEditing();
    rerender(<SessionHeader session={makeSession({ title: "Peer title" })} />);

    fireEvent.blur(screen.getByRole("textbox"));

    await waitFor(() => expect(screen.getByText("Peer title")).toBeTruthy());
    // No local change ⇒ no write ⇒ the peer's rename survives.
    expect(updateSessionTitleMock).not.toHaveBeenCalled();
  });

  it("saves the local title but announces that it superseded a remote rename", async () => {
    const { rerender } = await startEditing();
    const input = screen.getByRole("textbox");
    await userEvent.clear(input);
    await userEvent.type(input, "My new title");
    rerender(<SessionHeader session={makeSession({ title: "Peer title" })} />);

    fireEvent.blur(screen.getByRole("textbox"));

    await waitFor(() =>
      expect(updateSessionTitleMock).toHaveBeenCalledWith(
        "session-1",
        "My new title",
      ),
    );
    expect(toast.info).toHaveBeenCalledWith(
      "Title updated on another device — your version was saved",
    );
  });

  it("saves silently when no remote rename happened", async () => {
    await startEditing();
    const input = screen.getByRole("textbox");
    await userEvent.clear(input);
    await userEvent.type(input, "My new title");

    fireEvent.blur(input);

    await waitFor(() =>
      expect(updateSessionTitleMock).toHaveBeenCalledWith(
        "session-1",
        "My new title",
      ),
    );
    expect(toast.info).not.toHaveBeenCalled();
  });
});

// Pattern B (local-write misattribution): a LOCAL same-machine rename (the AI
// `update_title` tool via handleToolsExecuted, a dictation rename) moves the row
// under an open title edit exactly like a remote rename would, but it announces
// itself by bumping sessionMetaRefreshCounter for that session — the sync path
// never does. The title save must yield to it (no clobber, no "another device"
// toast), while a genuine remote rename (no bump) still toasts + LWW-wins.
describe("SessionHeader — title edit vs. LOCAL rename (misattribution)", () => {
  it("yields to a local session-meta rename: no clobber, no 'another device' toast", async () => {
    const { rerender } = render(
      <SessionHeader session={makeSession({ title: "Old title" })} />,
    );
    await userEvent.dblClick(screen.getByText("Old title"));
    const input = screen.getByRole("textbox");
    await userEvent.clear(input);
    await userEvent.type(input, "My draft");

    // A LOCAL AI `update_title` runs: NoteDetailView.handleToolsExecuted bumps the
    // session-scoped meta signal for THIS session and reloads the row to the AI's
    // title. Model exactly that: raise the signal, then the row prop moves.
    useAppStore.getState().incrementSessionMetaRefresh("session-1");
    rerender(<SessionHeader session={makeSession({ title: "AI Title" })} />);

    fireEvent.blur(screen.getByRole("textbox"));

    // FAILS on the pre-fix (session-blind) logic: it LWW-clobbers the AI rename
    // with "My draft" and toasts "another device". The fix yields to the local
    // write — no write, no toast, and the AI title survives.
    await waitFor(() => expect(screen.getByText("AI Title")).toBeTruthy());
    expect(updateSessionTitleMock).not.toHaveBeenCalled();
    expect(toast.info).not.toHaveBeenCalledWith(
      expect.stringContaining("another device"),
    );
  });

  it("still toasts + LWW-wins for a genuine remote rename (no local bump)", async () => {
    // Same shape, but NO sessionMetaRefresh bump — this is a peer edit that
    // arrived via sync, which never bumps the counter.
    const { rerender } = render(
      <SessionHeader session={makeSession({ title: "Old title" })} />,
    );
    await userEvent.dblClick(screen.getByText("Old title"));
    const input = screen.getByRole("textbox");
    await userEvent.clear(input);
    await userEvent.type(input, "My draft");

    rerender(<SessionHeader session={makeSession({ title: "Peer title" })} />);

    fireEvent.blur(screen.getByRole("textbox"));

    await waitFor(() =>
      expect(updateSessionTitleMock).toHaveBeenCalledWith(
        "session-1",
        "My draft",
      ),
    );
    expect(toast.info).toHaveBeenCalledWith(
      "Title updated on another device — your version was saved",
    );
  });
});

// Pattern D (commit-after-navigation): handleSaveTitle refreshes viewSession
// AFTER updateSessionTitle + getSession resolve. If the user opened a different
// session during that async gap, committing the renamed row would show the wrong
// session's title. The post-await selectedSessionId===session.id recheck prevents it.
describe("SessionHeader — title save vs. navigate-away (commit-after-navigation)", () => {
  function deferred<T>() {
    let resolve!: (v: T) => void;
    const promise = new Promise<T>((r) => (resolve = r));
    return { promise, resolve };
  }
  const flush = () => new Promise((r) => setTimeout(r, 0));

  it("does not commit the renamed row after the user navigated to another session", async () => {
    const sentinel = makeSession({ id: "session-1", title: "sentinel view" });
    useAppStore.setState({ selectedSessionId: "session-1", viewSession: sentinel });
    const rowRead = deferred<DbSession>();
    getSessionMock.mockReturnValue(rowRead.promise);

    render(<SessionHeader session={makeSession({ title: "Old title" })} />);
    await userEvent.dblClick(screen.getByText("Old title"));
    const input = screen.getByRole("textbox");
    await userEvent.clear(input);
    await userEvent.type(input, "My new title");
    fireEvent.blur(input);

    // Let updateSessionTitle + loadSessions settle so we're parked on getSession.
    await waitFor(() => expect(getSessionMock).toHaveBeenCalled());
    await flush();

    // User opens a different session while getSession is in flight.
    useAppStore.setState({ selectedSessionId: "other-session" });

    rowRead.resolve(makeSession({ id: "session-1", title: "My new title" }));
    await flush();

    // The open view is another session; the renamed row must NOT be committed.
    expect(useAppStore.getState().viewSession).toBe(sentinel);
  });

  it("commits the renamed row when the view has not moved", async () => {
    useAppStore.setState({ selectedSessionId: "session-1", viewSession: null });
    const renamed = makeSession({ id: "session-1", title: "My new title" });
    getSessionMock.mockResolvedValue(renamed);

    render(<SessionHeader session={makeSession({ title: "Old title" })} />);
    await userEvent.dblClick(screen.getByText("Old title"));
    const input = screen.getByRole("textbox");
    await userEvent.clear(input);
    await userEvent.type(input, "My new title");
    fireEvent.blur(input);

    await waitFor(() =>
      expect(useAppStore.getState().viewSession).toBe(renamed),
    );
  });
});
