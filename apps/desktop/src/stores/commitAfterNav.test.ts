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

// Controllable db reads. The commit-after-navigation actions (openSession,
// markSessionCompleted, createManualNote, recoverActiveSession) fan out through
// these; the tests drive their resolution ORDER / interleaving to reproduce the
// stale-commit race each guard defends against.
const dbMocks = vi.hoisted(() => ({
  getSession: vi.fn(),
  getSessionSegments: vi.fn(),
  listSessionAudioParts: vi.fn(),
  listSessions: vi.fn(),
  markSessionCompleted: vi.fn(),
  createManualSession: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}), emit: vi.fn() }));
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("@aptabase/tauri", () => ({ trackEvent: vi.fn().mockResolvedValue(undefined) }));
vi.mock("sonner", () => ({
  toast: { info: vi.fn(), error: vi.fn(), warning: vi.fn(), success: vi.fn() },
}));
vi.mock("@/lib/db", async () => {
  const actual = await vi.importActual<typeof import("@/lib/db")>("@/lib/db");
  return { ...actual, ...dbMocks };
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

/** Flush queued microtasks + a real macrotask so awaited reads reach their park. */
async function flush() {
  await new Promise((r) => setTimeout(r, 0));
}

beforeEach(() => {
  vi.clearAllMocks();
  dbMocks.getSessionSegments.mockResolvedValue([]);
  dbMocks.listSessionAudioParts.mockResolvedValue([]);
  dbMocks.listSessions.mockResolvedValue([]);
  dbMocks.markSessionCompleted.mockResolvedValue(undefined);
  dbMocks.createManualSession.mockResolvedValue(undefined);
  useAppStore.setState({
    selectedSessionId: null,
    activeSessionId: null,
    viewSession: null,
    viewSessionSegments: [],
    viewSessionParts: [],
    currentView: "note-list",
    sessions: [],
  });
});

describe("openSession — out-of-order commit (Pattern D)", () => {
  // Two overlapping opens: A starts first but its reads park; B starts second
  // and commits fully; then A's parked read resolves LAST. Without the latest-
  // request token, A commits its reads on top of B → selectedSessionId="A" while
  // the user asked for B last. The LATER open must win.
  it("lets the later openSession win when the earlier one resolves last", async () => {
    const aRead = deferred<DbSession>();
    dbMocks.getSession.mockImplementation((id: string) =>
      id === "A" ? aRead.promise : Promise.resolve(session(id)),
    );

    // (1) Open A — parks on getSession("A").
    const openA = useAppStore.getState().openSession("A");
    // (2) Open B — resolves immediately and commits B.
    await useAppStore.getState().openSession("B");
    expect(useAppStore.getState().selectedSessionId).toBe("B");

    // (3) A's parked read finally resolves — must NOT clobber B.
    aRead.resolve(session("A"));
    await openA;

    expect(useAppStore.getState().selectedSessionId).toBe("B");
    expect(useAppStore.getState().viewSession?.id).toBe("B");
  });

  // Happy path unaffected: a lone open still commits.
  it("commits normally for a single open", async () => {
    dbMocks.getSession.mockImplementation((id: string) =>
      Promise.resolve(session(id)),
    );
    await useAppStore.getState().openSession("A");
    expect(useAppStore.getState().selectedSessionId).toBe("A");
    expect(useAppStore.getState().viewSession?.id).toBe("A");
  });

  // Residual closed: the seq token is now shared with navigateTo, so a competing
  // navigation during the open's parked read supersedes the stale open.
  it("lets a navigateTo during the open's await win (open does not re-open)", async () => {
    const aRead = deferred<DbSession>();
    dbMocks.getSession.mockImplementation((id: string) =>
      id === "A" ? aRead.promise : Promise.resolve(session(id)),
    );
    const openA = useAppStore.getState().openSession("A");
    // User navigates to the note list while A's reads are in flight.
    useAppStore.getState().navigateTo("note-list", undefined);
    expect(useAppStore.getState().currentView).toBe("note-list");
    // A's parked read resolves LAST — must NOT re-open note-detail/A.
    aRead.resolve(session("A"));
    await openA;
    expect(useAppStore.getState().currentView).toBe("note-list");
    expect(useAppStore.getState().selectedSessionId).toBe(null);
  });
});

describe("markSessionCompleted — navigate-away during getSession (Pattern D)", () => {
  it("does not overwrite viewSession after the user navigated to another session", async () => {
    useAppStore.setState({ selectedSessionId: "A", viewSession: session("A") });
    const rowRead = deferred<DbSession>();
    dbMocks.getSession.mockReturnValue(rowRead.promise);

    // (1) Mark A completed — passes the pre-await selectedSessionId===id check,
    //     then parks on getSession("A").
    const p = useAppStore.getState().markSessionCompleted("A");
    await flush();

    // (2) User opens B while getSession("A") is in flight.
    useAppStore.setState({ selectedSessionId: "B", viewSession: session("B") });

    // (3) The parked read resolves with A's row.
    rowRead.resolve(session("A"));
    await p;

    // The open view is B; A's row must not have been committed over it.
    expect(useAppStore.getState().viewSession?.id).toBe("B");
  });

  it("commits the refreshed row when the view has not moved", async () => {
    useAppStore.setState({ selectedSessionId: "A", viewSession: session("A") });
    dbMocks.getSession.mockResolvedValue(session("A"));
    await useAppStore.getState().markSessionCompleted("A");
    expect(useAppStore.getState().viewSession?.id).toBe("A");
  });
});

describe("createManualNote — navigate-away during create (Pattern D)", () => {
  it("does not force-navigate to the new note when the user moved away mid-create", async () => {
    useAppStore.setState({ selectedSessionId: "A", viewSession: session("A") });
    const noteRead = deferred<DbSession>();
    // getSession here is the read of the just-created note.
    dbMocks.getSession.mockReturnValue(noteRead.promise);

    const p = useAppStore.getState().createManualNote("New");
    await flush(); // let dbCreateManualSession + listSessions settle; park on getSession

    // User navigates to B while the note's getSession is in flight.
    useAppStore.setState({ selectedSessionId: "B", viewSession: session("B") });

    noteRead.resolve(session("new-note"));
    await p;

    // The just-created note must NOT yank the user off B.
    expect(useAppStore.getState().selectedSessionId).toBe("B");
    expect(useAppStore.getState().viewSession?.id).toBe("B");
  });

  it("navigates to the new note on the normal (no-navigation) path", async () => {
    dbMocks.getSession.mockResolvedValue(session("new-note"));
    await useAppStore.getState().createManualNote("New");
    // The store commits its own freshly-minted session id (crypto.randomUUID),
    // not the getSession echo — so we assert navigation happened, not the id.
    expect(useAppStore.getState().selectedSessionId).toBeTruthy();
    expect(useAppStore.getState().selectedSessionId).not.toBe("A");
    expect(useAppStore.getState().currentView).toBe("note-detail");
    expect(useAppStore.getState().viewSession?.id).toBe("new-note");
  });
});

describe("recoverActiveSession — navigate-away during boot recovery (Pattern D)", () => {
  it("restores the active session but does not steal a view the user opened", async () => {
    const read = deferred<DbSession>();
    // Park the getSession leg of the Promise.all.
    dbMocks.getSession.mockReturnValue(read.promise);
    dbMocks.getSessionSegments.mockResolvedValue([segment("s", "REC")]);
    dbMocks.listSessionAudioParts.mockResolvedValue([]);

    const p = useAppStore.getState().recoverActiveSession("REC");
    await flush();

    // User opens a different session during the async recovery gap.
    useAppStore.setState({ selectedSessionId: "USER", viewSession: session("USER") });

    read.resolve(session("REC"));
    await p;

    // Active recording state is always restored…
    expect(useAppStore.getState().activeSessionId).toBe("REC");
    // …but the forced note-detail navigation must not stomp the user's choice.
    expect(useAppStore.getState().selectedSessionId).toBe("USER");
    expect(useAppStore.getState().viewSession?.id).toBe("USER");
  });

  it("navigates to the recovered session on a clean boot (no user navigation)", async () => {
    dbMocks.getSession.mockResolvedValue(session("REC"));
    dbMocks.getSessionSegments.mockResolvedValue([]);
    dbMocks.listSessionAudioParts.mockResolvedValue([]);

    await useAppStore.getState().recoverActiveSession("REC");

    expect(useAppStore.getState().activeSessionId).toBe("REC");
    expect(useAppStore.getState().selectedSessionId).toBe("REC");
    expect(useAppStore.getState().currentView).toBe("note-detail");
  });
});
