import { render, screen } from "@testing-library/react";
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
});
