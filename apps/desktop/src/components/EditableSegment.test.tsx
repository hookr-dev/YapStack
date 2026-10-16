import { render, screen, fireEvent } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
import { EditableSegment } from "./EditableSegment";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { DbSegment } from "@/lib/db";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

function makeSegment(overrides?: Partial<DbSegment>): DbSegment {
  return {
    id: "seg-1",
    session_id: "s1",
    source: "Mic",
    text: "Hello world",
    audio_offset_seconds: 65,
    chunk_duration_seconds: 5,
    confidence: 0.9,
    created_at: "2024-01-01",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  useAppStore.setState({
    editSegmentText: vi.fn(),
    deleteSegment: vi.fn(),
    toggleSegmentHidden: vi.fn(),
    editingSegmentId: null,
  });
});

describe("EditableSegment", () => {
  it("renders segment text", () => {
    render(<EditableSegment segment={makeSegment()} />);
    expect(screen.getByText("Hello world")).toBeInTheDocument();
  });

  it("formats timestamp correctly", () => {
    render(<EditableSegment segment={makeSegment({ audio_offset_seconds: 65 })} />);
    expect(screen.getByText("1:05")).toBeInTheDocument();
  });

  it("applies low opacity for low confidence", () => {
    render(
      <EditableSegment segment={makeSegment({ confidence: 0.3 })} />,
    );
    const bubble = screen.getByText("Hello world");
    expect(bubble.className).toContain("opacity-60");
  });

  it("does not apply line-through for hidden segment", () => {
    render(
      <TooltipProvider>
        <EditableSegment segment={makeSegment({ hidden: 1 })} />
      </TooltipProvider>,
    );
    const bubble = screen.getByText("Hello world");
    expect(bubble.className).not.toContain("line-through");
  });

  it("applies opacity-60 for hidden segment", () => {
    render(
      <TooltipProvider>
        <EditableSegment segment={makeSegment({ hidden: 1 })} />
      </TooltipProvider>,
    );
    const bubble = screen.getByText("Hello world");
    expect(bubble.closest("[class*='opacity-60']")).toBeInTheDocument();
  });

  it("renders EyeOff icon for hidden segment", () => {
    render(
      <TooltipProvider>
        <EditableSegment segment={makeSegment({ hidden: 1 })} />
      </TooltipProvider>,
    );
    expect(screen.getByLabelText("Hidden from AI and exports")).toBeInTheDocument();
  });

  it("renders nothing for empty text", () => {
    const { container } = render(
      <EditableSegment segment={makeSegment({ text: "   " })} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("shows edited indicator when edited", () => {
    render(
      <EditableSegment segment={makeSegment({ edited_at: "2024-01-02" })} />,
    );
    expect(screen.getByText(/edited/)).toBeInTheDocument();
  });

  it("enters edit mode on click", async () => {
    render(<EditableSegment segment={makeSegment()} />);
    const bubble = screen.getByText("Hello world");
    await userEvent.click(bubble);
    expect(bubble).toHaveAttribute("contenteditable", "true");
  });

  it("does not enter edit mode in readOnly", async () => {
    render(<EditableSegment segment={makeSegment()} readOnly />);
    const bubble = screen.getByText("Hello world");
    await userEvent.click(bubble);
    expect(bubble).not.toHaveAttribute("contenteditable", "true");
  });

  // --- D4 edit-in-progress guard for the WHOLE open-edit window
  // (LIVE_SESSION_STATE.md "Edit-in-progress under live refresh"). The store-visible
  // `editingSegmentId` signal is what `refreshOpenViewSession` consults to skip a reload.

  it("sets the edit-in-progress guard when a segment enters editing (focus)", async () => {
    render(<EditableSegment segment={makeSegment()} />);
    const bubble = screen.getByText("Hello world");
    await userEvent.click(bubble);
    expect(bubble).toHaveAttribute("contenteditable", "true");
    // The whole-window guard opens at edit start, not just during the async save.
    expect(useAppStore.getState().editingSegmentId).toBe("seg-1");
  });

  it("clears the guard on blur with no committed change", async () => {
    render(<EditableSegment segment={makeSegment()} />);
    const bubble = screen.getByText("Hello world");
    await userEvent.click(bubble);
    expect(useAppStore.getState().editingSegmentId).toBe("seg-1");
    // Unchanged text on blur → editSegmentText is not called; the component clears.
    fireEvent.blur(bubble);
    expect(useAppStore.getState().editingSegmentId).toBeNull();
    expect(useAppStore.getState().editSegmentText).not.toHaveBeenCalled();
  });

  it("clears the guard on Escape (cancel)", async () => {
    render(<EditableSegment segment={makeSegment()} />);
    const bubble = screen.getByText("Hello world");
    await userEvent.click(bubble);
    expect(useAppStore.getState().editingSegmentId).toBe("seg-1");
    fireEvent.keyDown(bubble, { key: "Escape" });
    expect(useAppStore.getState().editingSegmentId).toBeNull();
  });

  it("clears the guard on unmount mid-edit (no stuck guard blocking refreshes)", async () => {
    const { unmount } = render(<EditableSegment segment={makeSegment()} />);
    const bubble = screen.getByText("Hello world");
    await userEvent.click(bubble);
    expect(useAppStore.getState().editingSegmentId).toBe("seg-1");
    unmount();
    expect(useAppStore.getState().editingSegmentId).toBeNull();
  });

  it("leaves the guard set on a committing blur so editSegmentText owns clearing it", async () => {
    render(<EditableSegment segment={makeSegment()} />);
    const bubble = screen.getByText("Hello world");
    await userEvent.click(bubble);
    // Change the text, then blur: editSegmentText runs and (in production) re-sets then
    // clears the guard across its async write — the component must NOT clear it here.
    bubble.textContent = "Changed text";
    fireEvent.blur(bubble);
    expect(useAppStore.getState().editSegmentText).toHaveBeenCalledWith(
      "seg-1",
      "Changed text",
    );
    expect(useAppStore.getState().editingSegmentId).toBe("seg-1");
  });

  it("keeps refreshOpenViewSession a no-op while a focused edit holds the guard", async () => {
    // An open non-active session that would otherwise be reloaded by the D4 refresh.
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      viewSessionSegments: [],
    });
    render(<EditableSegment segment={makeSegment()} />);
    const bubble = screen.getByText("Hello world");
    await userEvent.click(bubble);
    // The focus-driven guard is exactly what refreshOpenViewSession consults.
    expect(useAppStore.getState().editingSegmentId).toBe("seg-1");
    await useAppStore.getState().refreshOpenViewSession();
    // The open edit suppressed the reload: no view state mutated.
    expect(useAppStore.getState().viewSessionSegments).toEqual([]);
  });
});
