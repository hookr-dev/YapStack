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
} from "@/test/tauri-mocks";
import { useAppStore } from "@/stores/appStore";
import type { DbSegment } from "@/lib/db";
import { BulkActionsBar } from "./BulkActionsBar";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@tauri-apps/plugin-sql", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("@aptabase/tauri", () => ({
  trackEvent: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

const writeTextMock = vi.fn().mockResolvedValue(undefined);

function seg(
  id: string,
  source: "Mic" | "System",
  text: string,
  offset: number,
): DbSegment {
  return {
    id,
    session_id: "s1",
    source,
    text,
    audio_offset_seconds: offset,
    chunk_duration_seconds: 1,
    confidence: 1,
    created_at: "",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
  };
}

const SEGMENTS: DbSegment[] = [
  seg("a", "System", "Hi there", 0),
  seg("b", "Mic", "Hello back", 5),
];

beforeEach(() => {
  vi.clearAllMocks();
  writeTextMock.mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    get: () => ({ writeText: writeTextMock }),
  });
  useAppStore.setState({
    selectedSegmentIds: new Set(["a", "b"]),
    clearSegmentSelection: vi.fn(),
    deleteSegments: vi.fn(),
    setSegmentsHidden: vi.fn(),
  });
});

afterEach(() => cleanup());

describe("BulkActionsBar", () => {
  it("renders one Copy action, not the old per-speaker split", () => {
    render(<BulkActionsBar segments={SEGMENTS} readOnly={false} />);
    expect(screen.getByRole("button", { name: /copy/i })).toBeInTheDocument();
    // The my-side/their-side split moved to the session-level Export menu.
    expect(screen.queryByText("Copy my side")).not.toBeInTheDocument();
    expect(screen.queryByText("Copy their side")).not.toBeInTheDocument();
    expect(screen.queryByText("Copy all")).not.toBeInTheDocument();
  });

  it("copies the selected segments as attributed markdown", async () => {
    render(<BulkActionsBar segments={SEGMENTS} readOnly={false} />);
    await userEvent.click(screen.getByRole("button", { name: /copy/i }));
    expect(writeTextMock).toHaveBeenCalledWith(
      "**Them:** Hi there\n\n**Me:** Hello back",
    );
  });

  it("renders nothing when no segments are selected", () => {
    useAppStore.setState({ selectedSegmentIds: new Set<string>() });
    const { container } = render(
      <BulkActionsBar segments={SEGMENTS} readOnly={false} />,
    );
    expect(container).toBeEmptyDOMElement();
  });
});
