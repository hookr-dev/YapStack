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
import type { SyncStatus } from "@/lib/sync";

// Capture the `sync://applied` handler (as in syncAppliedRefresh.test.ts).
const { listenHandlers, listenMock } = vi.hoisted(() => {
  const listenHandlers = new Map<string, (e: unknown) => void>();
  const listenMock = vi.fn(async (name: string, handler: (e: unknown) => void) => {
    listenHandlers.set(name, handler);
    return () => listenHandlers.delete(name);
  });
  return { listenHandlers, listenMock };
});

// Controllable db reads. Everything else in @/lib/db stays real (pure helpers such as
// isRecordingStale run for the resume guard).
const {
  getSessionMock,
  getSessionSegmentsMock,
  listSessionAudioPartsMock,
  getNoteMock,
} = vi.hoisted(() => ({
  getSessionMock: vi.fn(),
  getSessionSegmentsMock: vi.fn(),
  listSessionAudioPartsMock: vi.fn(),
  getNoteMock: vi.fn(),
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
  return {
    ...actual,
    getSession: getSessionMock,
    getSessionSegments: getSessionSegmentsMock,
    listSessionAudioParts: listSessionAudioPartsMock,
    getNote: getNoteMock,
    listSessions: vi.fn().mockResolvedValue([]),
    listFolders: vi.fn().mockResolvedValue([]),
    listAllSessionFolders: vi.fn().mockResolvedValue([]),
    listTags: vi.fn().mockResolvedValue([]),
    listAllSessionTags: vi.fn().mockResolvedValue([]),
    listDictationHistory: vi.fn().mockResolvedValue([]),
  };
});

import { useAppStore } from "./appStore";
import { commands } from "@/lib/tauri";
import { toast } from "sonner";

function dbTs(deltaSec: number): string {
  return new Date(Date.now() + deltaSec * 1000)
    .toISOString()
    .replace("T", " ")
    .replace(/\.\d+Z$/, "");
}

function makeSession(over: Partial<DbSession> = {}): DbSession {
  return {
    id: "open",
    title: "T",
    created_at: dbTs(-30),
    updated_at: dbTs(-30),
    source: "MicOnly",
    status: "recording",
    duration_seconds: null,
    total_segments: 0,
    folder_id: null,
    is_pinned: 0,
    pinned_at: null,
    session_type: "transcription",
    sort_order: 0,
    recording_device_id: null,
    ...over,
  };
}

function makeSegment(id: string): DbSegment {
  return {
    id,
    session_id: "open",
    source: "Mic",
    text: "hi",
    audio_offset_seconds: 0,
    chunk_duration_seconds: 1,
    confidence: 1,
    created_at: dbTs(-5),
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
  } as DbSegment;
}

function syncStatus(over: Partial<SyncStatus> = {}): SyncStatus {
  return {
    phase: "connected",
    serverUrl: "https://relay.example",
    email: "o@e.com",
    deviceFingerprint: "ME",
    roster: [],
    vaultKeyEpoch: null,
    rosterFingerprint: null,
    syncEnabled: true,
    lastError: null,
    billingUrl: null,
    pendingEntries: 0,
    pendingBytes: 0,
    ackedThisSession: 0,
    lastSuccess: null,
    pullBehind: 0,
    cryptoQuarantined: 0,
    audioUploadOutstanding: 0,
    audioBackfillOutstanding: 0,
    audioUploadFailed: 0,
    audioUploadedTotal: 0,
    audioBackfillComplete: false,
    ...over,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  listenHandlers.clear();
  listSessionAudioPartsMock.mockResolvedValue([]);
  getNoteMock.mockResolvedValue(null);
  useAppStore.setState({
    noteEditingSessionId: null,
    editingSegmentId: null,
    pendingViewRefresh: false,
    remoteNoteUpdate: null,
    viewSessionParts: [],
    viewSessionSegments: [],
  });
});

describe("refreshOpenViewSession (D4 live refresh, Gap 2)", () => {
  it("reloads the open non-active session's row + segments without bumping noteRefreshCounter", async () => {
    const segs = [makeSegment("g1"), makeSegment("g2")];
    getSessionMock.mockResolvedValue(makeSession());
    getSessionSegmentsMock.mockResolvedValue(segs);
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      editingSegmentId: null,
      viewSession: null,
      viewSessionSegments: [],
      noteRefreshCounter: 7,
    });

    await useAppStore.getState().refreshOpenViewSession();

    expect(getSessionSegmentsMock).toHaveBeenCalledWith("open");
    expect(useAppStore.getState().viewSessionSegments).toEqual(segs);
    expect(useAppStore.getState().viewSession).not.toBeNull();
    // Normative: must NOT bump noteRefreshCounter (would remount/reload the note editor).
    expect(useAppStore.getState().noteRefreshCounter).toBe(7);
  });

  it("does NOT reload when the open session is the local active one", async () => {
    useAppStore.setState({
      selectedSessionId: "live",
      activeSessionId: "live",
      editingSegmentId: null,
    });
    await useAppStore.getState().refreshOpenViewSession();
    expect(getSessionSegmentsMock).not.toHaveBeenCalled();
  });

  it("respects the edit-in-progress signal (skips while editingSegmentId is set)", async () => {
    getSessionMock.mockResolvedValue(makeSession());
    getSessionSegmentsMock.mockResolvedValue([makeSegment("g1")]);
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      editingSegmentId: "seg-under-edit",
      viewSessionSegments: [],
    });
    await useAppStore.getState().refreshOpenViewSession();
    expect(getSessionSegmentsMock).not.toHaveBeenCalled();
    expect(useAppStore.getState().viewSessionSegments).toEqual([]);
  });

  it("fires from a debounced sync://applied batch", async () => {
    vi.useFakeTimers();
    try {
      getSessionMock.mockResolvedValue(makeSession());
      getSessionSegmentsMock.mockResolvedValue([makeSegment("g1")]);
      useAppStore.setState({
        selectedSessionId: "open",
        activeSessionId: null,
        editingSegmentId: null,
      });
      const unlisten = await useAppStore.getState().startSyncAppliedRefresh();
      listenHandlers.get("sync://applied")!({
        payload: { applied: 1, quarantined: 0, replayed: 0 },
      });
      await vi.advanceTimersByTimeAsync(1500);
      expect(getSessionSegmentsMock).toHaveBeenCalledWith("open");
      unlisten();
    } finally {
      vi.useRealTimers();
    }
  });

  // B3
  it("reloads the open session's audio parts so an appended remote part surfaces", async () => {
    const parts = [
      { id: "p1", session_id: "open", part_index: 0 } as never,
      { id: "p2", session_id: "open", part_index: 1 } as never,
    ];
    getSessionMock.mockResolvedValue(makeSession());
    getSessionSegmentsMock.mockResolvedValue([]);
    listSessionAudioPartsMock.mockResolvedValue(parts);
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      viewSessionParts: [],
    });

    await useAppStore.getState().refreshOpenViewSession();

    expect(listSessionAudioPartsMock).toHaveBeenCalledWith("open");
    expect(useAppStore.getState().viewSessionParts).toHaveLength(2);
  });
});

// A2
describe("remote delete of the OPEN session (sync-UX A2)", () => {
  it("clears the open view, returns to the list and says why", async () => {
    getSessionMock.mockResolvedValue(null); // row vanished in the merge
    getSessionSegmentsMock.mockResolvedValue([]);
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      currentView: "note-detail",
      viewSession: makeSession(),
      viewSessionSegments: [makeSegment("g1")],
      viewSessionParts: [{ id: "p1" } as never],
    });

    await useAppStore.getState().refreshOpenViewSession();

    const s = useAppStore.getState();
    expect(s.currentView).toBe("note-list");
    expect(s.selectedSessionId).toBeNull();
    expect(s.viewSession).toBeNull();
    expect(s.viewSessionSegments).toEqual([]);
    expect(s.viewSessionParts).toEqual([]);
    expect(toast.info).toHaveBeenCalledWith(
      "Session was deleted on another device",
    );
  });

  it("leaves a still-present session open (no false positive)", async () => {
    getSessionMock.mockResolvedValue(makeSession());
    getSessionSegmentsMock.mockResolvedValue([]);
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      currentView: "note-detail",
    });

    await useAppStore.getState().refreshOpenViewSession();

    expect(useAppStore.getState().currentView).toBe("note-detail");
    expect(useAppStore.getState().selectedSessionId).toBe("open");
    expect(toast.info).not.toHaveBeenCalled();
  });
});

// A1c + A1b (store side)
describe("open note sync-down (sync-UX A1)", () => {
  beforeEach(() => {
    getSessionMock.mockResolvedValue(makeSession());
    getSessionSegmentsMock.mockResolvedValue([]);
  });

  it("publishes the open session's note row when no note edit is in progress", async () => {
    getNoteMock.mockResolvedValue({ content: "<p>from peer</p>" });
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      noteEditingSessionId: null,
      noteRefreshCounter: 4,
    });

    await useAppStore.getState().refreshOpenViewSession();

    expect(useAppStore.getState().remoteNoteUpdate).toMatchObject({
      sessionId: "open",
      content: "<p>from peer</p>",
    });
    // Normative: sync never bumps the note refresh counter.
    expect(useAppStore.getState().noteRefreshCounter).toBe(4);
  });

  it("does not re-publish identical content (no editor churn)", async () => {
    getNoteMock.mockResolvedValue({ content: "<p>same</p>" });
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      noteEditingSessionId: null,
    });

    await useAppStore.getState().refreshOpenViewSession();
    const first = useAppStore.getState().remoteNoteUpdate;
    await useAppStore.getState().refreshOpenViewSession();

    expect(useAppStore.getState().remoteNoteUpdate).toBe(first);
  });

  it("does NOT read or publish note content while the note is being edited", async () => {
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      noteEditingSessionId: "open",
      remoteNoteUpdate: null,
    });

    await useAppStore.getState().refreshOpenViewSession();

    expect(getNoteMock).not.toHaveBeenCalled();
    expect(useAppStore.getState().remoteNoteUpdate).toBeNull();
    // Transcript state still refreshed — only the note is held back.
    expect(getSessionSegmentsMock).toHaveBeenCalledWith("open");
    // …and the skipped note publication is remembered.
    expect(useAppStore.getState().pendingViewRefresh).toBe(true);
  });
});

// B5
describe("skipped-refresh catch-up (sync-UX B5)", () => {
  it("remembers a refresh skipped by the segment-edit guard and drains it on clear", async () => {
    getSessionMock.mockResolvedValue(makeSession());
    getSessionSegmentsMock.mockResolvedValue([makeSegment("g1")]);
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      editingSegmentId: "seg-under-edit",
      viewSessionSegments: [],
    });

    await useAppStore.getState().refreshOpenViewSession();
    expect(getSessionSegmentsMock).not.toHaveBeenCalled();
    expect(useAppStore.getState().pendingViewRefresh).toBe(true);

    // Closing the edit window is the catch-up point.
    useAppStore.getState().setEditingSegmentId(null);
    await vi.waitFor(() =>
      expect(useAppStore.getState().viewSessionSegments).toHaveLength(1),
    );
    expect(useAppStore.getState().pendingViewRefresh).toBe(false);
  });

  it("drains when the note-editing window closes (shared mechanism)", async () => {
    getSessionMock.mockResolvedValue(makeSession());
    getSessionSegmentsMock.mockResolvedValue([]);
    getNoteMock.mockResolvedValue({ content: "<p>peer</p>" });
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      noteEditingSessionId: "open",
      remoteNoteUpdate: null,
    });

    await useAppStore.getState().refreshOpenViewSession();
    expect(useAppStore.getState().pendingViewRefresh).toBe(true);

    useAppStore.getState().setNoteEditingSessionId(null);
    await vi.waitFor(() =>
      expect(useAppStore.getState().remoteNoteUpdate).toMatchObject({
        content: "<p>peer</p>",
      }),
    );
  });

  it("does not drain while another guard is still open", async () => {
    useAppStore.setState({
      selectedSessionId: "open",
      activeSessionId: null,
      editingSegmentId: "seg",
      noteEditingSessionId: "open",
      pendingViewRefresh: true,
    });

    useAppStore.getState().setNoteEditingSessionId(null);

    expect(useAppStore.getState().pendingViewRefresh).toBe(true);
    expect(getSessionSegmentsMock).not.toHaveBeenCalled();
  });
});

describe("markSessionCompleted escape-hatch action (LIVE_SESSION_STATE Q1)", () => {
  it("flips the row to completed and reloads the open view as completed", async () => {
    getSessionMock.mockResolvedValue(
      makeSession({ status: "completed", recording_device_id: "PEER" }),
    );
    useAppStore.setState({
      selectedSessionId: "open",
      viewSession: makeSession({
        status: "recording",
        recording_device_id: "PEER",
      }),
      sessions: [],
    });

    await useAppStore.getState().markSessionCompleted("open");

    // Reloaded the open view row (the LWW write already landed on the db mock).
    expect(getSessionMock).toHaveBeenCalledWith("open");
    expect(useAppStore.getState().viewSession?.status).toBe("completed");
    // No error toast on the happy path.
    expect(toast.error).not.toHaveBeenCalled();
  });
});

describe("resume guard (resume-race defense-in-depth)", () => {
  function armCaptureReady() {
    useAppStore.setState({
      enginePhase: "ready",
      captureStatus: {
        state: "Capturing",
        mic_active: true,
        system_audio_active: false,
        error_message: null,
      },
      liveTranscriptionActive: false,
      sessionStopping: false,
      activeSessionId: null,
    });
  }

  it("refuses resume with a truthful 'live on <label>' message for a fresh foreign owner", async () => {
    armCaptureReady();
    useAppStore.setState({
      syncStatus: syncStatus({
        roster: [
          { fingerprint: "PEER", isSelf: false, pending: false, label: "Windows" },
        ],
      }),
    });
    getSessionMock.mockResolvedValue(
      makeSession({ status: "recording", recording_device_id: "PEER", created_at: dbTs(-10) }),
    );
    getSessionSegmentsMock.mockResolvedValue([]); // heartbeat = created_at → fresh

    await useAppStore.getState().resumeSession("open");

    expect(toast.error).toHaveBeenCalledWith("This session is live on Windows.");
    expect(commands.startLiveTranscription).not.toHaveBeenCalled();
  });

  it("falls back to 'another device' when the owner is not in the roster", async () => {
    armCaptureReady();
    useAppStore.setState({ syncStatus: syncStatus({ roster: [] }) });
    getSessionMock.mockResolvedValue(
      makeSession({ status: "recording", recording_device_id: "PEER", created_at: dbTs(-10) }),
    );
    getSessionSegmentsMock.mockResolvedValue([]);

    await useAppStore.getState().resumeSession("open");

    expect(toast.error).toHaveBeenCalledWith("This session is live on another device.");
    expect(commands.startLiveTranscription).not.toHaveBeenCalled();
  });

  it("a stale foreign owner falls through to the generic refusal (no takeover)", async () => {
    armCaptureReady();
    useAppStore.setState({ syncStatus: syncStatus() });
    getSessionMock.mockResolvedValue(
      makeSession({
        status: "recording",
        recording_device_id: "PEER",
        created_at: dbTs(-600), // >3 min → stale
      }),
    );
    getSessionSegmentsMock.mockResolvedValue([]);

    await useAppStore.getState().resumeSession("open");

    expect(toast.error).toHaveBeenCalledWith(
      "This session is not in a state that can be resumed.",
    );
    expect(commands.startLiveTranscription).not.toHaveBeenCalled();
  });
});
