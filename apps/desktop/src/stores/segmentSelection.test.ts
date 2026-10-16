import { describe, it, expect, beforeEach } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import { vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

import { useAppStore } from "./appStore";

const ORDER = ["a", "b", "c", "d", "e", "f"];

function ids(): string[] {
  return Array.from(useAppStore.getState().selectedSegmentIds);
}

function anchor(): string | null {
  return useAppStore.getState().lastSelectedSegmentId;
}

beforeEach(() => {
  useAppStore.setState({
    selectedSegmentIds: new Set<string>(),
    lastSelectedSegmentId: null,
  });
});

describe("toggleSegmentSelected — canonical Finder/Linear behavior", () => {
  it("first shift-click with no anchor selects just that item and seeds the anchor", () => {
    useAppStore.getState().toggleSegmentSelected("c", "range", ORDER);
    expect(ids()).toEqual(["c"]);
    expect(anchor()).toBe("c");
  });

  it("second shift-click extends the range from the original anchor", () => {
    useAppStore.getState().toggleSegmentSelected("b", "range", ORDER);
    useAppStore.getState().toggleSegmentSelected("e", "range", ORDER);
    expect(ids().sort()).toEqual(["b", "c", "d", "e"]);
  });

  it("anchor stays fixed across consecutive shift-clicks", () => {
    useAppStore.getState().toggleSegmentSelected("b", "range", ORDER);
    useAppStore.getState().toggleSegmentSelected("e", "range", ORDER);
    expect(anchor()).toBe("b");
  });

  it("shift-click can shrink the range — replaces, not unions", () => {
    useAppStore.getState().toggleSegmentSelected("b", "range", ORDER);
    useAppStore.getState().toggleSegmentSelected("e", "range", ORDER);
    // User decides they only wanted b..c — shift-click on c should shrink.
    useAppStore.getState().toggleSegmentSelected("c", "range", ORDER);
    expect(ids().sort()).toEqual(["b", "c"]);
  });

  it("shift-click backwards from the anchor selects the reversed range", () => {
    useAppStore.getState().toggleSegmentSelected("e", "range", ORDER);
    useAppStore.getState().toggleSegmentSelected("b", "range", ORDER);
    expect(ids().sort()).toEqual(["b", "c", "d", "e"]);
    expect(anchor()).toBe("e");
  });

  it("cmd-click toggle moves the anchor to the toggled item", () => {
    useAppStore.getState().toggleSegmentSelected("b", "toggle", ORDER);
    useAppStore.getState().toggleSegmentSelected("d", "toggle", ORDER);
    expect(ids().sort()).toEqual(["b", "d"]);
    expect(anchor()).toBe("d");
  });

  it("shift-click after a stale anchor falls back to single-item selection", () => {
    useAppStore.setState({
      selectedSegmentIds: new Set(["zz"]),
      lastSelectedSegmentId: "zz",
    });
    useAppStore.getState().toggleSegmentSelected("c", "range", ORDER);
    expect(ids()).toEqual(["c"]);
    expect(anchor()).toBe("c");
  });

  it("setSegmentAnchor records a bare-click anchor that shift-click ranges from", () => {
    // Simulate a bare click on segment "c" (e.g., entering edit mode).
    useAppStore.getState().setSegmentAnchor("c");
    expect(anchor()).toBe("c");
    // The user then shift-clicks segment "f" — range from c through f.
    useAppStore.getState().toggleSegmentSelected("f", "range", ORDER);
    expect(ids().sort()).toEqual(["c", "d", "e", "f"]);
    expect(anchor()).toBe("c");
  });

  it("clearSegmentSelection drops both the set and the anchor", () => {
    useAppStore.getState().toggleSegmentSelected("b", "range", ORDER);
    useAppStore.getState().toggleSegmentSelected("e", "range", ORDER);
    useAppStore.getState().clearSegmentSelection();
    expect(ids()).toEqual([]);
    expect(anchor()).toBeNull();
  });
});
