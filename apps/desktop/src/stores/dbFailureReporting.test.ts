import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";

/**
 * Honest DB errors (live Windows bug hunt).
 *
 * The right-click segment actions used to end in
 * `console.error(...) + toast.error("Failed to …")`, which threw the underlying
 * SQLite message away at the UI layer. These pin the two seams that make it
 * visible again:
 *   1. the toast carries the underlying message, so a screenshot is diagnosable;
 *   2. `log.error(..., "db")` fires, so the failure reaches the Rust `tracing`
 *      subscriber's rolling log file even with devtools closed.
 */

const DB_ERROR = "no such column: hidden";

/** Every segment DB helper rejects with a realistic rusqlite message. */
const { segmentDbMocks } = vi.hoisted(() => ({
  segmentDbMocks: {
    updateSegmentText: vi.fn(),
    softDeleteSegment: vi.fn(),
    toggleSegmentHidden: vi.fn(),
    softDeleteSegments: vi.fn(),
    setSegmentsHidden: vi.fn(),
  },
}));

const { logMock } = vi.hoisted(() => ({
  logMock: { error: vi.fn(), warn: vi.fn(), info: vi.fn(), debug: vi.fn() },
}));

const { toastMock } = vi.hoisted(() => ({
  toastMock: {
    info: vi.fn(),
    error: vi.fn(),
    warning: vi.fn(),
    success: vi.fn(),
  },
}));

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => ({ toast: toastMock }));
vi.mock("@/lib/logger", () => ({
  log: logMock,
  installFrontendLogger: vi.fn(),
  captureDiagnostics: vi.fn(),
  safeStringify: (v: unknown) => String(v),
  formatArgs: (a: readonly unknown[]) => a.map(String).join(" "),
}));
vi.mock("@/lib/db", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/db")>();
  return { ...actual, ...segmentDbMocks };
});

import { useAppStore } from "./appStore";

/** The five store actions the right-click segment menu drives. */
const SEGMENT_ACTIONS: ReadonlyArray<{
  name: string;
  toastPrefix: string;
  run: () => Promise<void>;
}> = [
  {
    name: "editSegmentText",
    toastPrefix: "Failed to edit segment",
    run: () => useAppStore.getState().editSegmentText("seg-1", "new text"),
  },
  {
    name: "deleteSegment",
    toastPrefix: "Failed to delete segment",
    run: () => useAppStore.getState().deleteSegment("seg-1"),
  },
  {
    name: "toggleSegmentHidden",
    toastPrefix: "Failed to toggle segment visibility",
    run: () => useAppStore.getState().toggleSegmentHidden("seg-1"),
  },
  {
    name: "deleteSegments",
    toastPrefix: "Failed to delete segments",
    run: () => useAppStore.getState().deleteSegments(["seg-1", "seg-2"]),
  },
  {
    name: "setSegmentsHidden",
    toastPrefix: "Failed to update segment visibility",
    run: () => useAppStore.getState().setSegmentsHidden(["seg-1"], true),
  },
];

describe("segment DB failures are surfaced, not swallowed", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // Tauri rejects a Rust `Err(String)` with the bare string, not an Error.
    for (const fn of Object.values(segmentDbMocks)) {
      fn.mockRejectedValue(DB_ERROR);
    }
    // Silence the deliberate devtools passthrough.
    vi.spyOn(console, "error").mockImplementation(() => {});
  });

  it.each(SEGMENT_ACTIONS)(
    "$name puts the underlying error in the toast",
    async ({ toastPrefix, run }) => {
      await run();
      expect(toastMock.error).toHaveBeenCalledWith(
        `${toastPrefix}: ${DB_ERROR}`,
        undefined,
      );
    },
  );

  it.each(SEGMENT_ACTIONS)(
    "$name routes the failure through the db-scoped log seam",
    async ({ name, run }) => {
      await run();
      expect(logMock.error).toHaveBeenCalledWith(
        `${name} failed: ${DB_ERROR}`,
        "db",
      );
    },
  );

  it("clears the in-flight edit marker even when the write fails", async () => {
    await useAppStore.getState().editSegmentText("seg-1", "new text");
    expect(useAppStore.getState().editingSegmentId).toBeNull();
  });

  it("truncates a pathologically long error in the toast but logs it whole", async () => {
    const long = `constraint failed: ${"x".repeat(400)}`;
    segmentDbMocks.softDeleteSegment.mockRejectedValue(long);

    await useAppStore.getState().deleteSegment("seg-1");

    const [shown] = toastMock.error.mock.calls[0] as [string];
    expect(shown.length).toBeLessThanOrEqual("Failed to delete segment: ".length + 120);
    expect(shown.startsWith("Failed to delete segment: constraint failed: xxx")).toBe(true);
    expect(shown.endsWith("…")).toBe(true);
    // The log seam keeps the full message — truncation is a toast concern only.
    expect(logMock.error).toHaveBeenCalledWith(`deleteSegment failed: ${long}`, "db");
  });

  it("unwraps a CommandError object rather than printing [object Object]", async () => {
    segmentDbMocks.softDeleteSegment.mockRejectedValue({
      kind: "Sqlite",
      message: "database is locked",
    });

    await useAppStore.getState().deleteSegment("seg-1");

    expect(toastMock.error).toHaveBeenCalledWith(
      "Failed to delete segment: database is locked",
      undefined,
    );
  });

  it("leaves the success path free of error toasts and error logs", async () => {
    segmentDbMocks.softDeleteSegment.mockResolvedValue(undefined);

    await useAppStore.getState().deleteSegment("seg-1");

    expect(toastMock.error).not.toHaveBeenCalled();
    expect(logMock.error).not.toHaveBeenCalled();
  });
});
