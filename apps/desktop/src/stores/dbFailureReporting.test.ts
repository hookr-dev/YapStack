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

/**
 * Session-start DB helpers. Kept apart from `segmentDbMocks` because the suite-wide
 * `beforeEach` rejects everything in that group, and the busy-flag tests need a
 * controllable resolve as well as a reject.
 */
const { sessionDbMocks } = vi.hoisted(() => ({
  sessionDbMocks: {
    createSession: vi.fn(),
    deleteSession: vi.fn(),
    listSessions: vi.fn(),
    listFolders: vi.fn(),
    listTags: vi.fn(),
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
  return { ...actual, ...segmentDbMocks, ...sessionDbMocks };
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

/**
 * Starting a session (the live sync-drain bug).
 *
 * `createAndStartSession`'s first act is a DB write, which can wait out the whole
 * write-lock retry budget while the sync drain catches up. Before this the button had
 * no busy state (it just looked dead), the global-shortcut path had no catch at all
 * (unhandled rejection, user sees nothing), and the tray path only console.error'd.
 * These pin the two halves of the fix: a single-flighting `creatingSession` flag, and
 * one friendly toast reached from every entry point because the catch lives in the
 * action itself.
 */
const BUSY_TOAST =
  "Couldn't start a session — the database is busy syncing. Try again in a moment.";

describe("createAndStartSession surfaces failures and marks itself busy", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.spyOn(console, "error").mockImplementation(() => {});
    sessionDbMocks.createSession.mockResolvedValue(undefined);
    sessionDbMocks.deleteSession.mockResolvedValue(undefined);
    sessionDbMocks.listSessions.mockResolvedValue([]);
    sessionDbMocks.listFolders.mockResolvedValue([]);
    sessionDbMocks.listTags.mockResolvedValue([]);
    // Preconditions the action guards on — without these it fails before the DB write.
    useAppStore.setState({
      enginePhase: "ready",
      captureStatus: {
        state: "Capturing",
        mic_active: true,
        system_audio_active: false,
        error_message: null,
      },
      activeSessionId: null,
      creatingSession: false,
    });
  });

  it("holds `creatingSession` for the duration of the opening DB write", async () => {
    let releaseWrite = () => {};
    sessionDbMocks.createSession.mockReturnValue(
      new Promise<void>((resolve) => {
        releaseWrite = () => resolve();
      }),
    );

    const pending = useAppStore.getState().createAndStartSession();
    // The flag has to be visible synchronously — that is the whole point: the
    // button shows a spinner instead of looking dead for the length of the drain.
    expect(useAppStore.getState().creatingSession).toBe(true);

    releaseWrite();
    await pending;
    expect(useAppStore.getState().creatingSession).toBe(false);
  });

  it("single-flights: a second click while the write is stuck is a no-op", async () => {
    let releaseWrite = () => {};
    sessionDbMocks.createSession.mockReturnValue(
      new Promise<void>((resolve) => {
        releaseWrite = () => resolve();
      }),
    );

    const first = useAppStore.getState().createAndStartSession();
    await useAppStore.getState().createAndStartSession();

    expect(sessionDbMocks.createSession).toHaveBeenCalledTimes(1);
    releaseWrite();
    await first;
  });

  it("turns write-lock contention into a plain-language toast, not raw SQLite", async () => {
    sessionDbMocks.createSession.mockRejectedValue("database is locked");

    await useAppStore.getState().createAndStartSession();

    expect(toastMock.error).toHaveBeenCalledWith(BUSY_TOAST, {
      id: "create-session-failed",
    });
    // The raw message still reaches the db-scoped log seam.
    expect(logMock.error).toHaveBeenCalledWith(
      "createAndStartSession failed: database is locked",
      "db",
    );
  });

  it("keeps the honest detail for a failure that waiting will not fix", async () => {
    sessionDbMocks.createSession.mockRejectedValue({
      kind: "Sqlite",
      message: "no such column: recording_device_id",
    });

    await useAppStore.getState().createAndStartSession();

    expect(toastMock.error).toHaveBeenCalledWith(
      "Couldn't start a session: no such column: recording_device_id",
      { id: "create-session-failed" },
    );
  });

  it("never rejects — the shortcut and tray paths fire and forget", async () => {
    sessionDbMocks.createSession.mockRejectedValue("database is locked");

    await expect(
      useAppStore.getState().createAndStartSession(0, "shortcut"),
    ).resolves.toBeUndefined();
  });

  it("clears `creatingSession` and the backfill affordance after a failure", async () => {
    sessionDbMocks.createSession.mockRejectedValue("database is locked");

    await useAppStore.getState().createAndStartSession(30, "tray");

    expect(useAppStore.getState().creatingSession).toBe(false);
    expect(useAppStore.getState().backfillActive).toBe(false);
  });
});
