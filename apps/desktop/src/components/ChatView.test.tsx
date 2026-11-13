import { forwardRef, memo, type ComponentProps } from "react";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
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
import {
  installVirtualLayout,
  type VirtualLayout,
} from "@/test/virtual-layout";

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

// Rows are ROW_PX tall unless a test overrides one by segment id. That is
// a little taller than the estimate, so rows shift as they get measured.
const ROW_PX = 50;
const VIEWPORT_PX = 600;
const heights = new Map<string, number>();
let layout: VirtualLayout;

beforeEach(() => {
  heights.clear();
  layout = installVirtualLayout({
    viewportHeight: VIEWPORT_PX,
    viewportWidth: 400,
    heightOf: (row) => {
      const id = row
        .querySelector("[data-segment-row-id]")
        ?.getAttribute("data-segment-row-id");
      return (id && heights.get(id)) || ROW_PX;
    },
  });
});

afterEach(() => layout.uninstall());

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

  it("does not start an edit when the menu is dismissed with Escape", async () => {
    render(view([seg(0), seg(1)]));
    openMenuOn("Segment 1");
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(screen.queryAllByRole("menuitem")).toHaveLength(0),
    );
    // Let the close's deferred focus restore run.
    await act(() => new Promise((r) => setTimeout(r, 0)));
    expect(screen.getByText("Segment 1")).toHaveAttribute(
      "contenteditable",
      "false",
    );
    expect(useAppStore.getState().editingSegmentId).toBeNull();
  });

  describe("touch long-press", () => {
    // Radix's trigger opens on a 700 ms touch/pen hold.
    const LONG_PRESS_MS = 700;

    beforeEach(() => vi.useFakeTimers());
    afterEach(() => vi.useRealTimers());

    function longPress(el: Element) {
      fireEvent.pointerDown(el, { pointerType: "touch", button: 0 });
      act(() => {
        vi.advanceTimersByTime(LONG_PRESS_MS + 100);
      });
    }

    it("opens the menu for the pressed row", () => {
      render(view([seg(0), seg(1)]));
      longPress(screen.getByText("Segment 1"));
      fireEvent.click(screen.getByRole("menuitem", { name: "Delete" }));
      expect(useAppStore.getState().deleteSegment).toHaveBeenCalledWith(
        "seg-1",
      );
    });

    it("opens nothing on blank space after a right-click on a row", () => {
      const { container } = render(view([seg(0), seg(1)]));
      openMenuOn("Segment 1");
      fireEvent.keyDown(screen.getAllByRole("menuitem")[0], { key: "Escape" });
      act(() => {
        vi.runOnlyPendingTimers();
      });
      expect(screen.queryAllByRole("menuitem")).toHaveLength(0);

      const list = container.querySelector(
        "[data-segment-row-id]",
      )!.parentElement!;
      longPress(list);
      fireEvent.contextMenu(list);
      expect(screen.queryAllByRole("menuitem")).toHaveLength(0);
    });
  });
});

describe("ChatView range selection across the backfill split", () => {
  it("ranges across backfill and live rows after a prepend", () => {
    const live = [seg(4), seg(5)];
    const extra = { backfillActive: true, backfillBoundarySeconds: 20 };
    const r = render(view(live, extra));
    r.rerender(view([seg(2), seg(3), ...live], extra));
    fireEvent.click(screen.getByText("Segment 2"), { metaKey: true });
    fireEvent.click(screen.getByText("Segment 5"), { shiftKey: true });
    expect([...useAppStore.getState().selectedSegmentIds].sort()).toEqual([
      "seg-2",
      "seg-3",
      "seg-4",
      "seg-5",
    ]);
  });
});

const range = (from: number, to: number) =>
  Array.from({ length: to - from }, (_, k) => seg(from + k));
const maxScrollTop = () =>
  layout.viewport().scrollHeight - layout.viewport().clientHeight;
const rowEl = (id: string) =>
  document.querySelector<HTMLElement>(`[data-segment-row-id="${id}"]`);

async function mount(
  segments: DbSegment[],
  extra?: Partial<ComponentProps<typeof ChatView>>,
) {
  const r = render(view(segments, extra));
  await layout.settle();
  return r;
}

function expectFullyVisible(id: string) {
  const el = rowEl(id);
  expect(el, `${id} is mounted`).not.toBeNull();
  expect(el!.closest("[data-index]"), `${id} is a virtual row`).not.toBeNull();
  const rect = el!.getBoundingClientRect();
  expect(rect.top).toBeGreaterThanOrEqual(0);
  expect(rect.bottom).toBeLessThanOrEqual(VIEWPORT_PX);
}

describe("ChatView virtualized transcript", () => {
  it.each([100, 4554, 10000])(
    "mounts a bounded number of rows for %i segments",
    async (n) => {
      const { container } = await mount(range(0, n));
      const mounted = layout.mountedIndexes();
      expect(mounted.length).toBeGreaterThan(0);
      expect(mounted.length).toBeLessThanOrEqual(50);
      expect(container.querySelectorAll("*").length).toBeLessThan(500);
    },
  );

  it("opens at the latest line", async () => {
    await mount(range(0, 200));
    expect(maxScrollTop()).toBeGreaterThan(0);
    expect(layout.viewport().scrollTop).toBe(maxScrollTop());
    expectFullyVisible("seg-199");
  });

  it("rides the bottom on a pinned append, also when the new row remeasures taller", async () => {
    const base = range(0, 100);
    const r = await mount(base);
    r.rerender(view([...base, seg(100)]));
    await layout.settle();
    expect(layout.viewport().scrollTop).toBe(maxScrollTop());
    expectFullyVisible("seg-100");

    heights.set("seg-100", 140);
    act(() => layout.resize());
    await layout.settle();
    expect(layout.viewport().scrollTop).toBe(maxScrollTop());
    expectFullyVisible("seg-100");
  });

  it("leaves a scrolled-up reader in place on append", async () => {
    const base = range(0, 100);
    const r = await mount(base);
    await layout.scrollTo(1000);
    r.rerender(view([...base, seg(100)]));
    await layout.settle();
    expect(layout.viewport().scrollTop).toBe(1000);
  });

  it.each([
    ["at the top", {}],
    [
      "above the backfill divider",
      { backfillActive: true, backfillBoundarySeconds: 50 * 5 },
    ],
  ] as const)(
    "keeps the row being read in place when 50 rows land %s",
    async (_label, extra) => {
      const r = await mount(range(50, 150), extra);
      await layout.scrollTo(2000);
      const before = rowEl("seg-90")!.getBoundingClientRect().top;
      r.rerender(view(range(0, 150), extra));
      await layout.settle();
      expect(rowEl("seg-90")!.closest("[data-index]")).not.toBeNull();
      const after = rowEl("seg-90")!.getBoundingClientRect().top;
      expect(Math.abs(after - before)).toBeLessThanOrEqual(0.5);
    },
  );

  it("keeps following when a row above the viewport shrinks while pinned", async () => {
    const base = range(0, 100);
    const r = await mount(base);
    const above = [...document.querySelectorAll("[data-segment-row-id]")].find(
      (el) => el.getBoundingClientRect().bottom < 0,
    );
    expect(above).toBeDefined();
    heights.set(above!.getAttribute("data-segment-row-id")!, 20);
    act(() => layout.resize());
    await layout.settle();
    expect(layout.viewport().scrollTop).toBe(maxScrollTop());

    r.rerender(view([...base, seg(100)]));
    await layout.settle();
    expect(layout.viewport().scrollTop).toBe(maxScrollTop());
    expectFullyVisible("seg-100");
  });

  async function startPlayback(segments: DbSegment[], time: number) {
    const r = await mount(segments, { currentPlaybackTime: 0, onTimestampClick });
    act(() => useAppStore.setState({ isPlaying: true }));
    r.rerender(view(segments, { currentPlaybackTime: time, onTimestampClick }));
    await layout.settle();
    return r;
  }

  it("scrolls a far-away active segment into view on a playback seek", async () => {
    await startPlayback(range(0, 300), 5 * 5);
    expectFullyVisible("seg-5");
  });

  it("centres an unmounted active segment on Jump to current", async () => {
    await startPlayback(range(0, 300), 5 * 5);
    // Past the follow's own scroll, so the next one reads as the user's.
    await layout.settle(200);
    await layout.scrollTo(maxScrollTop());
    expect(rowEl("seg-5")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: /jump to current/i }));
    await layout.settle();
    expectFullyVisible("seg-5");
    const rect = rowEl("seg-5")!.getBoundingClientRect();
    expect(
      Math.abs((rect.top + rect.bottom) / 2 - VIEWPORT_PX / 2),
    ).toBeLessThanOrEqual(30);
  });

  // 10,000px up from the bottom of a 500-row list: seg-495 is far outside
  // the rendered range there.
  async function scrollFarAway() {
    await layout.scrollTo(maxScrollTop() - 10_000);
    const mounted = layout.mountedIndexes();
    expect(mounted.filter((i) => i < 400).length).toBeGreaterThan(10);
  }

  it("keeps an edited row mounted, and its edit guard, after scrolling 10,000px away", async () => {
    await mount(range(0, 500));
    await userEvent.click(screen.getByText("Segment 495"));
    expect(screen.getByText("Segment 495")).toHaveAttribute("contenteditable", "true");

    await scrollFarAway();
    expect(screen.getByText("Segment 495")).toHaveAttribute("contenteditable", "true");
    expect(useAppStore.getState().editingSegmentId).toBe("seg-495");
  });

  it("keeps the edit guard while a committed edit is still saving after scrolling away", async () => {
    let finishSave!: () => void;
    useAppStore.setState({
      editSegmentText: vi.fn((segmentId: string) => {
        useAppStore.setState({ editingSegmentId: segmentId });
        return new Promise<void>((resolve) => {
          finishSave = () => {
            useAppStore.setState({ editingSegmentId: null });
            resolve();
          };
        });
      }),
    });
    await mount(range(0, 500));
    const bubble = screen.getByText("Segment 495");
    await userEvent.click(bubble);
    bubble.textContent = "Segment 495 edited";
    fireEvent.blur(bubble);
    expect(useAppStore.getState().editSegmentText).toHaveBeenCalledWith(
      "seg-495",
      "Segment 495 edited",
    );

    await scrollFarAway();
    expect(rowEl("seg-495")).not.toBeNull();
    expect(useAppStore.getState().editingSegmentId).toBe("seg-495");

    await act(async () => finishSave());
    await layout.settle();
    expect(rowEl("seg-495")).toBeNull();
    expect(useAppStore.getState().editingSegmentId).toBeNull();
  });

  it("selects every row between the drag's top and bottom, including rows scrolled out during the drag", async () => {
    await mount(range(0, 100));
    const list = screen.getByRole("list").parentElement!;
    list.setPointerCapture ??= () => {};
    list.hasPointerCapture ??= () => false;
    list.releasePointerCapture ??= () => {};

    const startTop = layout.viewport().scrollTop;
    const startY = rowEl("seg-95")!.getBoundingClientRect().top + 10;
    fireEvent.pointerDown(screen.getByText("Segment 95"), {
      button: 0,
      clientX: 300,
      clientY: startY,
      pointerId: 1,
    });
    fireEvent.pointerMove(list, { clientX: 300, clientY: startY - 40, pointerId: 1 });

    await layout.scrollTo(startTop - 3000);
    expect(rowEl("seg-95")).toBeNull();
    fireEvent.pointerMove(list, { clientX: 300, clientY: 200, pointerId: 1 });
    // The row under the pointer at release (or, in a gap, the one below it).
    const endIndex = Math.min(
      ...[...document.querySelectorAll<HTMLElement>("[data-index]")]
        .filter((el) => el.getBoundingClientRect().bottom > 200)
        .map((el) => Number(el.getAttribute("data-index"))),
    );
    fireEvent.pointerUp(list, { clientX: 300, clientY: 200, pointerId: 1 });

    const expected = range(endIndex, 96).map((s) => s.id).sort();
    expect(expected.length).toBeGreaterThan(50);
    expect([...useAppStore.getState().selectedSegmentIds].sort()).toEqual(expected);
  });

  it("exposes rows as a list with each row's place in the whole transcript", async () => {
    await mount(range(0, 100));
    const items = screen.getAllByRole("listitem");
    const last = items[items.length - 1];
    expect(last).toHaveAttribute("aria-setsize", "100");
    expect(last).toHaveAttribute("aria-posinset", "100");
  });
});
