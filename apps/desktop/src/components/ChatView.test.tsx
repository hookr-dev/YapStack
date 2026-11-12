import { forwardRef, memo, type ComponentProps } from "react";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
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
import type { DbSegment } from "@/lib/db";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

// Counts renders per segment id. The spy is memoized with the same shallow
// prop comparison as the real row, so a count above 1 means a row received
// a changed prop.
const rowRenders = new Map<string, number>();
vi.mock("@/components/EditableSegment", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("@/components/EditableSegment")>();
  const Real = actual.EditableSegment;
  type Props = ComponentProps<typeof Real>;
  const Spy = memo(
    forwardRef<HTMLDivElement, Props>(function Spy(props, ref) {
      const id = props.segment.id;
      rowRenders.set(id, (rowRenders.get(id) ?? 0) + 1);
      return <Real {...props} ref={ref} />;
    }),
  );
  return { ...actual, EditableSegment: Spy };
});

import { useAppStore } from "@/stores/appStore";
import { ChatView } from "./ChatView";
import { TooltipProvider } from "@/components/ui/tooltip";

Element.prototype.scrollIntoView = () => {};

function seg(i: number, overrides?: Partial<DbSegment>): DbSegment {
  return {
    id: `seg-${i}`,
    session_id: "s1",
    source: i % 2 === 0 ? "Mic" : "System",
    text: `Segment ${i}`,
    audio_offset_seconds: i * 5,
    chunk_duration_seconds: 5,
    confidence: 0.9,
    created_at: "2024-01-01",
    chunk_index: i,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
    ...overrides,
  };
}

const onTimestampClick = () => {};

function view(
  segments: DbSegment[],
  extra?: Partial<ComponentProps<typeof ChatView>>,
) {
  return (
    <TooltipProvider>
      <ChatView sessionId="s1" segments={segments} isEditable {...extra} />
    </TooltipProvider>
  );
}

beforeEach(() => {
  rowRenders.clear();
  useAppStore.setState({
    selectedSegmentIds: new Set(),
    lastSelectedSegmentId: null,
    editingSegmentId: null,
    deleteSegment: vi.fn(),
    toggleSegmentHidden: vi.fn(),
    editSegmentText: vi.fn(),
  });
});

describe("ChatView live append", () => {
  it.each([
    ["live recording", {}],
    ["with a timestamp handler", { onTimestampClick }],
    [
      "during backfill",
      { backfillActive: true, backfillBoundarySeconds: 2 * 5 },
    ],
  ] as const)(
    "does not re-render existing rows when a segment is appended (%s)",
    (_label, extra) => {
      const base = Array.from({ length: 6 }, (_, i) => seg(i));
      const r = render(view(base, extra));
      expect(rowRenders.get("seg-0")).toBe(1);

      act(() => {
        r.rerender(view([...base, seg(6)], extra));
      });

      for (const s of base) expect(rowRenders.get(s.id)).toBe(1);
      expect(rowRenders.get("seg-6")).toBe(1);
    },
  );

  it("shift-click range-selects using the order at click time", async () => {
    const base = Array.from({ length: 3 }, (_, i) => seg(i));
    const r = render(view(base));
    r.rerender(view([...base, seg(3), seg(4)]));

    await userEvent.click(screen.getByText("Segment 1"));
    // Leave the edit the bare click opened before range-selecting.
    fireEvent.keyDown(screen.getByText("Segment 1"), { key: "Escape" });
    fireEvent.click(screen.getByText("Segment 4"), { shiftKey: true });

    expect([...useAppStore.getState().selectedSegmentIds].sort()).toEqual([
      "seg-1",
      "seg-2",
      "seg-3",
      "seg-4",
    ]);
  });
});

function openMenuOn(text: string) {
  fireEvent.contextMenu(screen.getByText(text));
}

function menuItemLabels() {
  return screen.getAllByRole("menuitem").map((el) => el.textContent);
}

describe("ChatView segment context menu", () => {
  it("lists the editable actions in order", () => {
    render(view([seg(0), seg(1)]));
    openMenuOn("Segment 1");
    expect(menuItemLabels()).toEqual([
      "Edit",
      "Copy",
      "Insert into Notes",
      "Hide",
      "Delete",
    ]);
  });

  it("offers only Copy and Insert into Notes when read-only", () => {
    render(view([seg(0)], { isEditable: false }));
    openMenuOn("Segment 0");
    expect(menuItemLabels()).toEqual(["Copy", "Insert into Notes"]);
  });

  it("labels a hidden segment's toggle Unhide and targets that segment", async () => {
    render(view([seg(0), seg(1, { hidden: 1 })]));
    openMenuOn("Segment 1");
    expect(menuItemLabels()).toContain("Unhide");
    await userEvent.click(screen.getByRole("menuitem", { name: "Unhide" }));
    expect(useAppStore.getState().toggleSegmentHidden).toHaveBeenCalledWith(
      "seg-1",
    );
  });

  it("opens from the empty part of a row, not only the bubble", () => {
    render(view([seg(0)]));
    const row = screen
      .getByText("Segment 0")
      .closest("[data-segment-row-id]") as HTMLElement;
    fireEvent.contextMenu(row);
    expect(menuItemLabels()).toContain("Copy");
  });

  it("does not open on blank space between rows", () => {
    const { container } = render(view([seg(0)]));
    const list = container.querySelector(
      "[data-segment-row-id]",
    )!.parentElement!;
    fireEvent.contextMenu(list);
    expect(screen.queryAllByRole("menuitem")).toHaveLength(0);
  });

  it("deletes the right-clicked segment", async () => {
    render(view([seg(0), seg(1)]));
    openMenuOn("Segment 0");
    await userEvent.click(screen.getByRole("menuitem", { name: "Delete" }));
    expect(useAppStore.getState().deleteSegment).toHaveBeenCalledWith("seg-0");
  });

  it("copies the right-clicked segment's text", async () => {
    const user = userEvent.setup();
    render(view([seg(0), seg(1)]));
    openMenuOn("Segment 1");
    await user.click(screen.getByRole("menuitem", { name: "Copy" }));
    await expect(navigator.clipboard.readText()).resolves.toBe("Segment 1");
  });

  it("dispatches an insert-segment-ref event for Insert into Notes", async () => {
    const listener = vi.fn();
    window.addEventListener("yapstack:insert-segment-ref", listener);
    render(view([seg(0), seg(13)]));
    openMenuOn("Segment 13");
    await userEvent.click(
      screen.getByRole("menuitem", { name: "Insert into Notes" }),
    );
    window.removeEventListener("yapstack:insert-segment-ref", listener);
    expect(listener).toHaveBeenCalledTimes(1);
    expect((listener.mock.calls[0][0] as CustomEvent).detail).toEqual({
      segmentId: "seg-13",
      timestamp: "1:05",
      offsetSeconds: 65,
    });
  });

  it("puts the right-clicked segment into edit mode", async () => {
    render(view([seg(0), seg(1)]));
    openMenuOn("Segment 1");
    await userEvent.click(screen.getByRole("menuitem", { name: "Edit" }));
    // The edit starts once the menu has closed and released focus.
    await waitFor(() =>
      expect(screen.getByText("Segment 1")).toHaveAttribute(
        "contenteditable",
        "true",
      ),
    );
    expect(document.activeElement).toBe(screen.getByText("Segment 1"));
    expect(screen.getByText("Segment 0")).toHaveAttribute(
      "contenteditable",
      "false",
    );
    expect(useAppStore.getState().editingSegmentId).toBe("seg-1");
  });
});
