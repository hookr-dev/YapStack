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

/** Flush queued microtasks + a real macrotask so drain-scheduled reloads settle. */
async function flush() {
  await new Promise((r) => setTimeout(r, 0));
}

const NEW_SEGS = [segment("a-seg-edited", "A")];

describe("refreshViewSessionSegments — edit-in-progress guard (editingSegmentId)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    dbReads.getSessionSegments.mockResolvedValue(NEW_SEGS);
    dbReads.getSession.mockImplementation((id: string) =>
      Promise.resolve(session(id)),
    );
    dbReads.listSessionAudioParts.mockResolvedValue([]);
  });

  // A user has a segment open in the contentEditable (editingSegmentId set) when
  // the replace_in_transcript AI tool finishes and fires a transcript refresh.
  // The fresh (AI-edited) segment set must NOT replace the array underneath the
  // open edit — that swap remounts the bubble and drops the in-progress text.
  it("defers instead of committing while a segment is being edited", async () => {
    useAppStore.setState({
      selectedSessionId: "A",
      activeSessionId: null,
      viewSession: session("A"),
      viewSessionSegments: A_SEGS,
      viewSessionParts: [],
      editingSegmentId: "a-seg",
      noteEditingSessionId: null,
      pendingViewRefresh: false,
    });

    await useAppStore.getState().refreshViewSessionSegments();

    // Pre-fix: the refresh commits NEW_SEGS unconditionally, clobbering the
    // in-progress edit → this assertion FAILS on the unmodified tree.
    expect(useAppStore.getState().viewSessionSegments).toBe(A_SEGS);
    // The skip is remembered so the batch is not silently lost (B5).
    expect(useAppStore.getState().pendingViewRefresh).toBe(true);
  });

  // The active (live) session routes through activeSessionSegments, and
  // refreshOpenViewSession early-returns for it — so the drain MUST re-run
  // refreshViewSessionSegments, otherwise the deferred batch never lands.
  it("drains the deferred refresh onto the active view when the edit closes", async () => {
    useAppStore.setState({
      selectedSessionId: "A",
      activeSessionId: "A",
      viewSession: session("A"),
      activeSessionSegments: A_SEGS,
      viewSessionSegments: [],
      viewSessionParts: [],
      editingSegmentId: "a-seg",
      noteEditingSessionId: null,
      pendingViewRefresh: false,
    });

    // (1) A concurrent refresh arrives mid-edit → deferred, active view untouched.
    await useAppStore.getState().refreshViewSessionSegments();
    expect(useAppStore.getState().activeSessionSegments).toBe(A_SEGS);
    expect(useAppStore.getState().pendingViewRefresh).toBe(true);

    // (2) The edit window closes → the guard-clear drains the skipped refresh.
    useAppStore.getState().setEditingSegmentId(null);
    await flush();

    expect(useAppStore.getState().activeSessionSegments).toEqual(NEW_SEGS);
    expect(useAppStore.getState().pendingViewRefresh).toBe(false);
  });
});
