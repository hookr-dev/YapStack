import { render, screen, cleanup } from "@testing-library/react";
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
