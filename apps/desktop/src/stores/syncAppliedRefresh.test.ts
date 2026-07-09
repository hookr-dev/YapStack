import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  tauriCoreMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";

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

import { useAppStore } from "./appStore";

describe("startSyncAppliedRefresh (R6 item 2)", () => {
  beforeEach(() => {
    listenHandlers.clear();
    listenMock.mockClear();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function stubReloads() {
    const spies = {
      loadSessions: vi.fn().mockResolvedValue(undefined),
      loadFolders: vi.fn().mockResolvedValue(undefined),
      loadSessionFolders: vi.fn().mockResolvedValue(undefined),
      loadTags: vi.fn().mockResolvedValue(undefined),
      loadSessionTags: vi.fn().mockResolvedValue(undefined),
    };
    useAppStore.setState(spies);
    return spies;
  }

  it("subscribes to sync://applied and debounces a burst into one coarse refresh", async () => {
    const spies = stubReloads();
    const unlisten = await useAppStore.getState().startSyncAppliedRefresh();

    const handler = listenHandlers.get("sync://applied");
    expect(handler).toBeTypeOf("function");

    // A backlog produces several rapid events — they must collapse into ONE refresh.
    handler!({ payload: { applied: 3, quarantined: 0, replayed: 0 } });
    handler!({ payload: { applied: 5, quarantined: 0, replayed: 0 } });
    handler!({ payload: { applied: 2, quarantined: 0, replayed: 0 } });

    // Before the debounce window elapses, nothing has refreshed yet.
    expect(spies.loadSessions).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(1500);

    // Exactly one coarse refresh across every affected view.
    expect(spies.loadSessions).toHaveBeenCalledTimes(1);
    expect(spies.loadFolders).toHaveBeenCalledTimes(1);
    expect(spies.loadSessionFolders).toHaveBeenCalledTimes(1);
    expect(spies.loadTags).toHaveBeenCalledTimes(1);
    expect(spies.loadSessionTags).toHaveBeenCalledTimes(1);

    unlisten();
  });

  it("cancels a pending debounced refresh and tears down the subscription on unlisten", async () => {
    const spies = stubReloads();
    const unlisten = await useAppStore.getState().startSyncAppliedRefresh();
    const handler = listenHandlers.get("sync://applied")!;

    // An event arrives (schedules the debounce), then we unlisten BEFORE it fires.
    handler({ payload: { applied: 1, quarantined: 0, replayed: 0 } });
    unlisten();
    await vi.advanceTimersByTimeAsync(1500);

    expect(spies.loadSessions).not.toHaveBeenCalled();
    expect(listenHandlers.has("sync://applied")).toBe(false);
  });
});
