import { render, screen, fireEvent } from "@testing-library/react";
import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import { useAppStore } from "@/stores/appStore";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { DbSession } from "@/lib/db";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("@dnd-kit/core", () => ({
  useDraggable: () => ({
    attributes: {},
    listeners: {},
    setNodeRef: vi.fn(),
    isDragging: false,
  }),
}));
vi.mock("@/lib/folder-tree", () => ({
  buildFolderTree: () => [],
  getDisplayFolders: () => [],
}));

import { NoteCard } from "./NoteCard";

function makeSession(overrides?: Partial<DbSession>): DbSession {
  return {
    id: "session-1",
    title: "Test Session",
    created_at: new Date(Date.now() - 30 * 60 * 1000).toISOString().replace("Z", ""),
    updated_at: new Date().toISOString().replace("Z", ""),
    source: "MicOnly",
    status: "completed",
    duration_seconds: 120,
    total_segments: 5,
    folder_id: null,
    is_pinned: 0,
    pinned_at: null,
    session_type: "transcription",
    sort_order: 0,
    ...overrides,
  };
}

function renderNoteCard(session: DbSession) {
  return render(
    <TooltipProvider>
      <NoteCard session={session} />
    </TooltipProvider>,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  useAppStore.setState({
    selectedSessionId: null,
    activeSessionId: null,
    openSession: vi.fn(),
    deleteSession: vi.fn(),
    togglePin: vi.fn(),
    folders: [],
    sessionFolderMap: {},
    toggleSessionFolder: vi.fn(),
    removeSessionFromAllFolders: vi.fn(),
  });
});

describe("NoteCard", () => {
  it("renders the session title", () => {
    renderNoteCard(makeSession({ title: "My Recording" }));
    expect(screen.getByText("My Recording")).toBeInTheDocument();
  });

  it("renders 'Untitled' when title is empty", () => {
    renderNoteCard(makeSession({ title: "" }));
    expect(screen.getByText("Untitled")).toBeInTheDocument();
  });

  it("shows relative time for recent sessions", () => {
    // 30 minutes ago should show "30m ago"
    const thirtyMinAgo = new Date(Date.now() - 30 * 60 * 1000)
      .toISOString()
      .replace("Z", "");
    renderNoteCard(makeSession({ created_at: thirtyMinAgo }));
    expect(screen.getByText("30m ago")).toBeInTheDocument();
  });

  it("shows segment count when total_segments > 0", () => {
    renderNoteCard(makeSession({ total_segments: 12 }));
    expect(screen.getByText(/12 segments/)).toBeInTheDocument();
  });

  it("hides segment count when total_segments is 0", () => {
    renderNoteCard(makeSession({ total_segments: 0 }));
    expect(screen.queryByText(/segments/)).not.toBeInTheDocument();
  });

  it("shows pin icon when session is pinned", () => {
    const { container } = renderNoteCard(
      makeSession({ is_pinned: 1 }),
    );
    // The Pin icon from lucide-react renders as an SVG. The Pin icon inside the
    // card header row (not in context menu) is a small 3x3 icon.
    const pinIcons = container.querySelectorAll("svg.lucide-pin");
    expect(pinIcons.length).toBeGreaterThanOrEqual(1);
  });

  it("does not show pin icon when session is not pinned", () => {
    const { container } = renderNoteCard(
      makeSession({ is_pinned: 0 }),
    );
    // Within the card content area (not context menu), there should be no Pin icon
    const cardContent = container.querySelector(".cursor-pointer");
    const pinIcons = cardContent?.querySelectorAll("svg.lucide-pin") ?? [];
    expect(pinIcons.length).toBe(0);
  });

  it("shows recording indicator (pulsing dot) when session is active", () => {
    useAppStore.setState({ activeSessionId: "session-1" });
    const { container } = renderNoteCard(makeSession({ id: "session-1" }));
    const pulsingDot = container.querySelector(".animate-pulse");
    expect(pulsingDot).toBeInTheDocument();
  });

  it("does not show recording indicator for inactive sessions", () => {
    useAppStore.setState({ activeSessionId: "other-session" });
    const { container } = renderNoteCard(
      makeSession({ id: "session-1", status: "completed" }),
    );
    const pulsingDot = container.querySelector(".animate-pulse");
    expect(pulsingDot).not.toBeInTheDocument();
  });

  it("shows Mic icon for recording session type", () => {
    const { container } = renderNoteCard(
      makeSession({ session_type: "transcription" }),
    );
    const micIcon = container.querySelector("svg.lucide-mic");
    expect(micIcon).toBeInTheDocument();
  });

  it("shows PenLine icon for manual session type", () => {
    const { container } = renderNoteCard(
      makeSession({ session_type: "manual" }),
    );
    const penIcon = container.querySelector("svg.lucide-pen-line");
    expect(penIcon).toBeInTheDocument();
  });

  it("applies selected styles when session matches selectedSessionId", () => {
    useAppStore.setState({ selectedSessionId: "session-1" });
    const { container } = renderNoteCard(makeSession({ id: "session-1" }));
    const cardDiv = container.querySelector("[class*='border-ring']");
    expect(cardDiv).toBeInTheDocument();
    expect(cardDiv?.className).toContain("bg-accent");
  });

  it("does not apply selected styles when session is not selected", () => {
    useAppStore.setState({ selectedSessionId: "other-session" });
    const { container } = renderNoteCard(makeSession({ id: "session-1" }));
    const cardDiv = container.querySelector("[class*='border-ring']");
    expect(cardDiv).toBeNull();
  });

  it("calls openSession when the card is clicked", () => {
    const openSession = vi.fn();
    useAppStore.setState({ openSession });
    renderNoteCard(makeSession({ id: "session-42", title: "Click Me" }));
    fireEvent.click(screen.getByText("Click Me"));
    expect(openSession).toHaveBeenCalledWith("session-42");
  });

});
