import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  tauriCoreMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import type { DbFolder, DbSession } from "@/lib/db";

// Capture the `sync://applied` handler so the test can simulate drain-applied events and
// assert the coarse, debounced refresh (R6 item 2).
const { listenHandlers, listenMock } = vi.hoisted(() => {
  const listenHandlers = new Map<string, (e: unknown) => void>();
  const listenMock = vi.fn(async (name: string, handler: (e: unknown) => void) => {
    listenHandlers.set(name, handler);
    return () => listenHandlers.delete(name);
  });
  return { listenHandlers, listenMock };
});

// Controllable list reads — the batched refresh (C2) reads the DB directly
// instead of fanning out to the individual load actions.
const dbReads = vi.hoisted(() => ({
  listSessions: vi.fn(),
  listFolders: vi.fn(),
  listAllSessionFolders: vi.fn(),
  listTags: vi.fn(),
  listAllSessionTags: vi.fn(),
  listDictationHistory: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock, emit: vi.fn() }));
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => ({
  toast: { info: vi.fn(), error: vi.fn(), warning: vi.fn(), success: vi.fn() },
}));
vi.mock("@/lib/db", async () => {
  const actual = await vi.importActual<typeof import("@/lib/db")>("@/lib/db");
  return { ...actual, ...dbReads };
});

import { useAppStore } from "./appStore";

function session(over: Partial<DbSession> = {}): DbSession {
  return {
    id: "s1",
    title: "One",
    created_at: "2026-01-01 00:00:00",
    updated_at: "2026-01-01 00:00:00",
    source: "MicOnly",
    status: "completed",
    duration_seconds: 10,
    total_segments: 1,
    folder_id: null,
    is_pinned: 0,
    pinned_at: null,
    session_type: "transcription",
    sort_order: 0,
    recording_device_id: null,
    ...over,
  };
}

function folder(id: string, name = id): DbFolder {
  return {
    id,
    name,
    parent_id: null,
    color: null,
    icon: null,
    sort_order: 0,
    created_at: "2026-01-01 00:00:00",
  } as DbFolder;
}

describe("startSyncAppliedRefresh (R6 item 2)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    listenHandlers.clear();
    dbReads.listSessions.mockResolvedValue([]);
    dbReads.listFolders.mockResolvedValue([]);
    dbReads.listAllSessionFolders.mockResolvedValue([]);
    dbReads.listTags.mockResolvedValue([]);
    dbReads.listAllSessionTags.mockResolvedValue([]);
    dbReads.listDictationHistory.mockResolvedValue([]);
    useAppStore.setState({
      sessions: [],
      folders: [],
      tags: [],
      dictationHistory: [],
      listFilter: { type: "all" },
      selectedSessionId: null,
      activeSessionId: null,
      editingSegmentId: null,
      noteEditingSessionId: null,
      pendingViewRefresh: false,
      syncAppliedSeq: 0,
    });
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("subscribes to sync://applied and debounces a burst into one coarse refresh", async () => {
    const unlisten = await useAppStore.getState().startSyncAppliedRefresh();

    const handler = listenHandlers.get("sync://applied");
    expect(handler).toBeTypeOf("function");

    // A backlog produces several rapid events — they must collapse into ONE refresh.
    handler!({ payload: { applied: 3, quarantined: 0, replayed: 0 } });
    handler!({ payload: { applied: 5, quarantined: 0, replayed: 0 } });
    handler!({ payload: { applied: 2, quarantined: 0, replayed: 0 } });

    // Before the debounce window elapses, nothing has refreshed yet.
    expect(dbReads.listSessions).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(1500);

    // Exactly one coarse refresh across every affected view.
    expect(dbReads.listSessions).toHaveBeenCalledTimes(1);
    expect(dbReads.listFolders).toHaveBeenCalledTimes(1);
    expect(dbReads.listAllSessionFolders).toHaveBeenCalledTimes(1);
    expect(dbReads.listTags).toHaveBeenCalledTimes(1);
    expect(dbReads.listAllSessionTags).toHaveBeenCalledTimes(1);
    // B1: dictation history rides the same batch.
    expect(dbReads.listDictationHistory).toHaveBeenCalledTimes(1);
    // Surfaces that own their own reads observe the committed batch.
    expect(useAppStore.getState().syncAppliedSeq).toBe(1);

    unlisten();
  });

  it("cancels a pending debounced refresh and tears down the subscription on unlisten", async () => {
    const unlisten = await useAppStore.getState().startSyncAppliedRefresh();
    const handler = listenHandlers.get("sync://applied")!;

    // An event arrives (schedules the debounce), then we unlisten BEFORE it fires.
    handler({ payload: { applied: 1, quarantined: 0, replayed: 0 } });
    unlisten();
    await vi.advanceTimersByTimeAsync(5000);

    expect(dbReads.listSessions).not.toHaveBeenCalled();
    expect(listenHandlers.has("sync://applied")).toBe(false);
  });

  // C1
  it("fires under the max-wait ceiling when continuous activity keeps resetting the debounce", async () => {
    const unlisten = await useAppStore.getState().startSyncAppliedRefresh();
    const handler = listenHandlers.get("sync://applied")!;

    // A peer recording live emits faster than the 1.5s trailing window, so the
    // trailing timer never gets to fire.
    for (let i = 0; i < 6; i++) {
      handler({ payload: { applied: 1, quarantined: 0, replayed: 0 } });
      await vi.advanceTimersByTimeAsync(1000);
    }

    // …the 3s ceiling fired anyway (and only for elapsed ceilings — 6s of
    // continuous activity = two windows, not six).
    expect(dbReads.listSessions).toHaveBeenCalled();
    expect(dbReads.listSessions.mock.calls.length).toBeLessThanOrEqual(2);

    unlisten();
  });
});

describe("coarse refresh commit (C2/C3/B4)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    dbReads.listSessions.mockResolvedValue([]);
    dbReads.listFolders.mockResolvedValue([]);
    dbReads.listAllSessionFolders.mockResolvedValue([]);
    dbReads.listTags.mockResolvedValue([]);
    dbReads.listAllSessionTags.mockResolvedValue([]);
    dbReads.listDictationHistory.mockResolvedValue([]);
    useAppStore.setState({
      sessions: [],
      folders: [],
      tags: [],
      dictationHistory: [],
      listFilter: { type: "all" },
      selectedSessionId: null,
      activeSessionId: null,
      syncAppliedSeq: 0,
    });
  });

  // C2
  it("commits every list in a single set() so the UI moves once", async () => {
    dbReads.listSessions.mockResolvedValue([session()]);
    dbReads.listFolders.mockResolvedValue([folder("f1")]);
    dbReads.listDictationHistory.mockResolvedValue([]);
    const seen: number[] = [];
    const unsub = useAppStore.subscribe((s) => seen.push(s.syncAppliedSeq));

    await useAppStore.getState().syncAppliedCoarseRefresh();
    unsub();

    // One commit for the whole batch (the open-view refresh is a no-op here —
    // nothing is open).
    expect(seen).toEqual([1]);
    expect(useAppStore.getState().sessions).toHaveLength(1);
    expect(useAppStore.getState().folders).toHaveLength(1);
  });

  // C3
  it("keeps object identity for rows that did not change", async () => {
    const first = session({ id: "s1", title: "One" });
    const second = session({ id: "s2", title: "Two" });
    useAppStore.setState({ sessions: [first, second] });

    // A remote edit touches s2 only; s1 comes back as a fresh (equal) object.
    dbReads.listSessions.mockResolvedValue([
      session({ id: "s1", title: "One" }),
      session({ id: "s2", title: "Two (renamed)" }),
    ]);

    await useAppStore.getState().syncAppliedCoarseRefresh();

    const after = useAppStore.getState().sessions;
    expect(after[0]).toBe(first); // unchanged card keeps identity → no remount
    expect(after[1]).not.toBe(second);
    expect(after[1].title).toBe("Two (renamed)");
  });

  it("returns the very same array when nothing changed at all", async () => {
    const rows = [session({ id: "s1" })];
    useAppStore.setState({ sessions: rows });
    dbReads.listSessions.mockResolvedValue([session({ id: "s1" })]);

    await useAppStore.getState().syncAppliedCoarseRefresh();

    expect(useAppStore.getState().sessions).toBe(rows);
  });

  // B4
  it("resets a folder filter whose folder was deleted on another device", async () => {
    useAppStore.setState({
      folders: [folder("f1"), folder("f2")],
      listFilter: { type: "folder", folderId: "f2" },
    });
    dbReads.listFolders.mockResolvedValue([folder("f1")]);

    await useAppStore.getState().syncAppliedCoarseRefresh();

    expect(useAppStore.getState().listFilter).toEqual({ type: "all" });
  });

  it("leaves a folder filter alone while its folder still exists", async () => {
    useAppStore.setState({
      folders: [folder("f1")],
      listFilter: { type: "folder", folderId: "f1" },
    });
    dbReads.listFolders.mockResolvedValue([folder("f1")]);

    await useAppStore.getState().syncAppliedCoarseRefresh();

    expect(useAppStore.getState().listFilter).toEqual({
      type: "folder",
      folderId: "f1",
    });
  });

  // B1
  it("reloads dictation history on the batch", async () => {
    dbReads.listDictationHistory.mockResolvedValue([
      { id: "d1", text: "hi" } as never,
    ]);

    await useAppStore.getState().syncAppliedCoarseRefresh();

    expect(useAppStore.getState().dictationHistory).toHaveLength(1);
  });
});
