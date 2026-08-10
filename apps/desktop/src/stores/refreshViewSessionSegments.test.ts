import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  tauriCoreMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import type { DbSession, DbSegment } from "@/lib/db";

// Controllable db reads. `refreshViewSessionSegments` and `openSession` both
// fan out through these — the test drives their resolution ORDER to reproduce
// the stale-commit race.
const dbReads = vi.hoisted(() => ({
  getSession: vi.fn(),
  getSessionSegments: vi.fn(),
  listSessionAudioParts: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}), emit: vi.fn() }));
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

function session(id: string): DbSession {
  return {
    id,
    title: id,
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
  } as DbSession;
}

function segment(id: string, sessionId: string): DbSegment {
  return {
    id,
    session_id: sessionId,
    text: `seg-${id}`,
    start_time: 0,
    end_time: 1,
    speaker: null,
    segment_index: 0,
    is_hidden: 0,
    created_at: "2026-01-01 00:00:00",
  } as unknown as DbSegment;
}

/** A promise the test resolves by hand, to park an in-flight read. */
function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

const A_SEGS = [segment("a-seg", "A")];
const B_SEGS = [segment("b-seg", "B")];

describe("refreshViewSessionSegments — stale-commit race after a session switch", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAppStore.setState({
      selectedSessionId: "A",
      activeSessionId: null,
      viewSession: session("A"),
      viewSessionSegments: A_SEGS,
      viewSessionParts: [],
      editingSegmentId: null,
      noteEditingSessionId: null,
    });
  });

  // A segment edit on session A issues a slow write, then a refresh. The user
  // clicks session B before the refresh's getSessionSegments(A) resolves.
  // openSession(B) commits {viewSession: B, viewSessionSegments: B}. Then the
  // parked getSessionSegments(A) resolves and — with no post-await recheck —
  // clobbers viewSessionSegments back to A's transcript while viewSession is B.
  it("does not commit session A's segments after openSession(B) has switched the view", async () => {
    const aSegsDeferred = deferred<DbSegment[]>();

    // getSessionSegments("A") parks until the test releases it; every other id
    // (i.e. openSession's read of B) resolves immediately.
    dbReads.getSessionSegments.mockImplementation((id: string) =>
      id === "A" ? aSegsDeferred.promise : Promise.resolve(B_SEGS),
    );
    dbReads.getSession.mockImplementation((id: string) =>
      Promise.resolve(session(id)),
    );
    dbReads.listSessionAudioParts.mockResolvedValue([]);

    // (1) A refresh for the currently-selected session A begins and parks on its
    //     getSessionSegments("A") await.
    const refreshPromise = useAppStore.getState().refreshViewSessionSegments();

    // (2) The user switches to session B; openSession commits B fully.
    await useAppStore.getState().openSession("B");
    expect(useAppStore.getState().selectedSessionId).toBe("B");
    expect(useAppStore.getState().viewSessionSegments).toEqual(B_SEGS);

    // (3) The parked A read finally resolves.
    aSegsDeferred.resolve(A_SEGS);
    await refreshPromise;

    // The view is B, so its segments must remain B's. On the current tree the
    // refresh commits A's segments unconditionally → viewSession=B paired with
    // viewSessionSegments=A (a mismatched header/transcript). This assertion
    // FAILS on the unmodified tree.
    expect(useAppStore.getState().viewSession?.id).toBe("B");
    expect(useAppStore.getState().viewSessionSegments).toEqual(B_SEGS);
  });
});
