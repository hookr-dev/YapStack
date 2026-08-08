import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
import type { DbSession, DbSegment } from "@/lib/db";
import type { SyncStatus } from "@/lib/sync";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => ({
  toast: { info: vi.fn(), error: vi.fn(), warning: vi.fn(), success: vi.fn() },
}));

import { useAppStore } from "@/stores/appStore";
import { commands } from "@/lib/tauri";
import { invoke } from "@tauri-apps/api/core";
import type { AudioPartPrepare } from "@/lib/sync";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { AudioPart } from "@/components/AudioPlayer";
import type { DbAudioPart } from "@/lib/db";
import {
  NoteDetailView,
  isRemoteLiveSession,
  selectRemoteRecordingView,
  canMarkSessionCompleted,
  selectAudioAvailability,
  assembleTrack,
} from "./NoteDetailView";

// A SQLite `datetime('now')`-shaped UTC timestamp `deltaSec` seconds from real now.
// Staleness (LIVE_SESSION_STATE D5) compares the heartbeat to Date.now(), so the
// render-branch fixtures MUST be relative to now — a fixed calendar date would read
// permanently stale and mis-route every "Live" case to "Interrupted".
function dbTs(deltaSec: number): string {
  return new Date(Date.now() + deltaSec * 1000)
    .toISOString()
    .replace("T", " ")
    .replace(/\.\d+Z$/, "");
}

function makeSession(over: Partial<DbSession> = {}): DbSession {
  return {
    id: "s-remote",
    title: "Meeting notes",
    created_at: dbTs(-5),
    updated_at: dbTs(-5),
    source: "MicOnly",
    status: "recording",
    duration_seconds: null,
    total_segments: 0,
    folder_id: null,
    is_pinned: 0,
    pinned_at: null,
    session_type: "transcription",
    sort_order: 0,
    recording_device_id: "PEER",
    ...over,
  };
}

function makeSegment(id: string, text: string, createdAt: string = dbTs(-3)): DbSegment {
  return {
    id,
    session_id: "s-remote",
    source: "System",
    text,
    audio_offset_seconds: 0,
    chunk_duration_seconds: 1,
    confidence: 1,
    created_at: createdAt,
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
    roster: [{ fingerprint: "PEER", isSelf: false, pending: false, label: "Windows" }],
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

function openRemote(over: Partial<DbSession> = {}, segments: DbSegment[] = []) {
  useAppStore.setState({
    selectedSessionId: "s-remote",
    activeSessionId: null,
    viewSession: makeSession(over),
    viewSessionSegments: segments,
    viewSessionParts: [],
    sessions: [],
    syncStatus: syncStatus(),
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  // jsdom does not implement scrollIntoView; ChatView's stick-to-bottom uses it.
  Element.prototype.scrollIntoView = vi.fn();
});

describe("isRemoteLiveSession selection (D3)", () => {
  const me = "ME";
  it("foreign-owned recording, not active → remote-live", () => {
    expect(
      isRemoteLiveSession(
        { status: "recording", recording_device_id: "PEER" },
        false,
        me,
      ),
    ).toBe(true);
  });
  it("own-owned recording → NOT remote-live (own crash / Interrupted, slice 4)", () => {
    expect(
      isRemoteLiveSession({ status: "recording", recording_device_id: "ME" }, false, me),
    ).toBe(false);
  });
  it("NULL-owner recording → NOT remote-live (legacy)", () => {
    expect(
      isRemoteLiveSession({ status: "recording", recording_device_id: null }, false, me),
    ).toBe(false);
  });
  it("completed foreign session → NOT remote-live", () => {
    expect(
      isRemoteLiveSession(
        { status: "completed", recording_device_id: "PEER" },
        false,
        me,
      ),
    ).toBe(false);
  });
  it("locally-active recording → NOT remote-live (local live branch wins)", () => {
    expect(
      isRemoteLiveSession({ status: "recording", recording_device_id: "PEER" }, true, me),
    ).toBe(false);
  });
  it("NULL myFingerprint (sync off) → never remote-live", () => {
    expect(
      isRemoteLiveSession(
        { status: "recording", recording_device_id: "PEER" },
        false,
        null,
      ),
    ).toBe(false);
  });
});

describe("NoteDetailView remote-live rendering (D3)", () => {
  it("renders the '● Live on <label>' badge and streamed transcript, read-only", () => {
    openRemote({}, [makeSegment("g1", "hello from A")]);
    render(<NoteDetailView />);
    expect(screen.getByText("Live on Windows")).toBeInTheDocument();
    expect(screen.getByText("hello from A")).toBeInTheDocument();
    // The empty-local-record prompt (Gap 1 bug) must NOT be reachable here.
    expect(
      screen.queryByText("Start speaking to begin transcription"),
    ).not.toBeInTheDocument();
    // No write affordances: no resume/stop/delete menu, no editor.
    expect(screen.queryByText("Resume recording")).not.toBeInTheDocument();
    expect(screen.queryByText("Delete session")).not.toBeInTheDocument();
  });

  it("falls back to a waiting message (not 'Start speaking') at zero segments", () => {
    openRemote({}, []);
    render(<NoteDetailView />);
    expect(screen.getByText("Live on Windows")).toBeInTheDocument();
    expect(screen.getByText(/Waiting for Windows to start speaking/)).toBeInTheDocument();
    expect(
      screen.queryByText("Start speaking to begin transcription"),
    ).not.toBeInTheDocument();
  });

  it("labels an unknown owner 'another device'", () => {
    useAppStore.setState({
      selectedSessionId: "s-remote",
      activeSessionId: null,
      viewSession: makeSession({ recording_device_id: "UNKNOWN" }),
      viewSessionSegments: [],
      viewSessionParts: [],
      sessions: [],
      syncStatus: syncStatus(),
    });
    render(<NoteDetailView />);
    expect(screen.getByText("Live on another device")).toBeInTheDocument();
  });
});

describe("selectRemoteRecordingView (slice 4: fresh → live, stale → interrupted)", () => {
  const me = "ME";
  const freshSeg = [makeSegment("s1", "x", dbTs(-10))];
  const staleSeg = [makeSegment("s1", "x", dbTs(-600))];

  it("fresh foreign owner → remote-live", () => {
    expect(
      selectRemoteRecordingView(
        makeSession({ recording_device_id: "PEER" }),
        freshSeg,
        false,
        me,
      ),
    ).toBe("remote-live");
  });

  it("stale foreign owner → interrupted", () => {
    expect(
      selectRemoteRecordingView(
        makeSession({ recording_device_id: "PEER", created_at: dbTs(-600) }),
        staleSeg,
        false,
        me,
      ),
    ).toBe("interrupted");
  });

  it("own-owned recording → interrupted (own crash, never renders live)", () => {
    expect(
      selectRemoteRecordingView(
        makeSession({ recording_device_id: "ME" }),
        freshSeg,
        false,
        me,
      ),
    ).toBe("interrupted");
  });

  it("NULL-owner recording → interrupted (legacy, never renders live)", () => {
    expect(
      selectRemoteRecordingView(
        makeSession({ recording_device_id: null }),
        freshSeg,
        false,
        me,
      ),
    ).toBe("interrupted");
  });

  it("completed foreign → null (not a remote-recording row)", () => {
    expect(
      selectRemoteRecordingView(
        makeSession({ status: "completed", recording_device_id: "PEER" }),
        freshSeg,
        false,
        me,
      ),
    ).toBe(null);
  });

  it("locally active → null (local live branch wins)", () => {
    expect(
      selectRemoteRecordingView(
        makeSession({ recording_device_id: "PEER" }),
        freshSeg,
        true,
        me,
      ),
    ).toBe(null);
  });

  it("sync off (NULL fingerprint) → null (single-device unchanged, D7)", () => {
    expect(
      selectRemoteRecordingView(
        makeSession({ recording_device_id: "PEER" }),
        freshSeg,
        false,
        null,
      ),
    ).toBe(null);
  });

  it("flips interrupted → remote-live when a fresh segment arrives (D4 recompute)", () => {
    const s = makeSession({
      recording_device_id: "PEER",
      created_at: dbTs(-600),
    });
    // A long silence: only an old heartbeat → interrupted.
    expect(selectRemoteRecordingView(s, staleSeg, false, me)).toBe("interrupted");
    // The recorder resumes; a freshly-merged segment (new heartbeat) flips it back.
    expect(
      selectRemoteRecordingView(
        s,
        [...staleSeg, makeSegment("s2", "resumed", dbTs(-2))],
        false,
        me,
      ),
    ).toBe("remote-live");
  });
});

describe("canMarkSessionCompleted visibility matrix (escape hatch, Q1)", () => {
  const me = "ME";
  const freshSeg = [makeSegment("s1", "x", dbTs(-10))];
  const staleSeg = [makeSegment("s1", "x", dbTs(-600))];

  it("fresh-foreign → hidden (a live session can never be marked completed)", () => {
    expect(
      canMarkSessionCompleted(
        makeSession({ recording_device_id: "PEER" }),
        freshSeg,
        false,
        me,
      ),
    ).toBe(false);
  });

  it("stale-foreign → shown (dead device / stranded session)", () => {
    expect(
      canMarkSessionCompleted(
        makeSession({ recording_device_id: "PEER", created_at: dbTs(-600) }),
        staleSeg,
        false,
        me,
      ),
    ).toBe(true);
  });

  it("own stale → hidden (owner-only boot sweep finalizes it, D6)", () => {
    expect(
      canMarkSessionCompleted(
        makeSession({ recording_device_id: "ME", created_at: dbTs(-600) }),
        staleSeg,
        false,
        me,
      ),
    ).toBe(false);
  });

  it("NULL-owner stale → hidden (legacy; sweep finalizes)", () => {
    expect(
      canMarkSessionCompleted(
        makeSession({ recording_device_id: null, created_at: dbTs(-600) }),
        staleSeg,
        false,
        me,
      ),
    ).toBe(false);
  });

  it("completed foreign → hidden", () => {
    expect(
      canMarkSessionCompleted(
        makeSession({
          status: "completed",
          recording_device_id: "PEER",
          created_at: dbTs(-600),
        }),
        staleSeg,
        false,
        me,
      ),
    ).toBe(false);
  });

  it("sync off (NULL fingerprint) → hidden", () => {
    expect(
      canMarkSessionCompleted(
        makeSession({ recording_device_id: "PEER", created_at: dbTs(-600) }),
        staleSeg,
        false,
        null,
      ),
    ).toBe(false);
  });

  it("same-hardware re-pair: this device's own pre-re-pair row is foreign-to-self + stale → shown", () => {
    // A credential clear / fresh install mints a NEW fingerprint ("ME-NEW"); the device's
    // own old rows still carry the OLD self-fingerprint ("ME-OLD"), so they now read as
    // foreign and, being stale, expose the same escape hatch — the only recovery under
    // owner-only finalization (D6). foreign+stale is the whole predicate, so re-pair needs
    // no special-casing.
    expect(
      canMarkSessionCompleted(
        makeSession({ recording_device_id: "ME-OLD", created_at: dbTs(-600) }),
        staleSeg,
        false,
        "ME-NEW",
      ),
    ).toBe(true);
  });
});

function openInterrupted(over: Partial<DbSession> = {}, segments: DbSegment[] = []) {
  useAppStore.setState({
    selectedSessionId: "s-remote",
    activeSessionId: null,
    viewSession: makeSession({ created_at: dbTs(-600), ...over }),
    viewSessionSegments: segments,
    viewSessionParts: [],
    sessions: [],
    syncStatus: syncStatus(),
  });
}

describe("NoteDetailView interrupted rendering + escape hatch (slice 4)", () => {
  it("renders 'Interrupted on <label>' (no live pulse) for a stale foreign row", () => {
    openInterrupted({ recording_device_id: "PEER" }, [
      makeSegment("g1", "older text", dbTs(-600)),
    ]);
    render(<NoteDetailView />);
    expect(screen.getByText("Interrupted on Windows")).toBeInTheDocument();
    expect(screen.queryByText("Live on Windows")).not.toBeInTheDocument();
    expect(
      screen.queryByText("Start speaking to begin transcription"),
    ).not.toBeInTheDocument();
    // Transcript still renders (read-only follow-along of what was captured).
    expect(screen.getByText("older text")).toBeInTheDocument();
  });

  it("confirm dialog names the device and invokes the plain LWW mark-completed action", async () => {
    const markSpy = vi.fn();
    openInterrupted({ recording_device_id: "PEER" }, []);
    useAppStore.setState({ markSessionCompleted: markSpy });
    render(<NoteDetailView />);

    await userEvent.click(screen.getByRole("button", { name: "Session actions" }));
    await userEvent.click(await screen.findByText("Mark completed"));
    // Exact Q1 dialog copy, naming the device.
    expect(
      await screen.findByText(
        /This session appears interrupted on Windows\. Mark it completed\?/,
      ),
    ).toBeInTheDocument();
    // Confirm → the escape-hatch action fires with this session id.
    await userEvent.click(screen.getByRole("button", { name: "Mark completed" }));
    expect(markSpy).toHaveBeenCalledWith("s-remote");
  });

  it("hides the escape hatch for an own-crashed row (Interrupted, no actions menu)", () => {
    openInterrupted({ recording_device_id: "ME" }, []);
    render(<NoteDetailView />);
    expect(screen.getByText("Interrupted")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Session actions" }),
    ).not.toBeInTheDocument();
  });

  it("hides the escape hatch for a legacy NULL-owner row", () => {
    openInterrupted({ recording_device_id: null }, []);
    render(<NoteDetailView />);
    expect(screen.getByText("Interrupted")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Session actions" }),
    ).not.toBeInTheDocument();
  });
});

describe("selectAudioAvailability (honest player selection)", () => {
  const parts: AudioPart[] = [
    { src: "a.wav", duration: 1 },
    { src: "b.wav", duration: 2 },
  ];

  it("no parts → available, empty (nothing to render)", () => {
    expect(selectAudioAvailability([], null)).toEqual({
      playableParts: [],
      unavailable: false,
    });
  });

  it("unresolved check → optimistic full player", () => {
    expect(selectAudioAvailability(parts, null)).toEqual({
      playableParts: parts,
      unavailable: false,
    });
  });

  it("length mismatch (stale check) → optimistic full player", () => {
    expect(selectAudioAvailability(parts, [true])).toEqual({
      playableParts: parts,
      unavailable: false,
    });
  });

  it("all present → full player", () => {
    expect(selectAudioAvailability(parts, [true, true])).toEqual({
      playableParts: parts,
      unavailable: false,
    });
  });

  it("all missing → unavailable, no playable parts", () => {
    expect(selectAudioAvailability(parts, [false, false])).toEqual({
      playableParts: [],
      unavailable: true,
    });
  });

  it("mixed (some missing) → unavailable all-or-nothing (timeline honesty)", () => {
    expect(selectAudioAvailability(parts, [true, false])).toEqual({
      playableParts: [],
      unavailable: true,
    });
  });
});

function makePart(over: Partial<DbAudioPart> = {}): DbAudioPart {
  return {
    id: "p0",
    session_id: "s-remote",
    part_index: 0,
    file_path: "/peer/audio/s-remote.0.wav",
    format: "wav",
    duration_seconds: 12,
    sample_rate: 16000,
    created_at: "2026-07-15 12:00:00",
    ...over,
  } as DbAudioPart;
}

describe("assembleTrack (S3 D2 src resolution)", () => {
  const url = (p: string) => `stream://${p}`;
  const p0 = makePart({ id: "p0", file_path: "/local/a.wav" });
  const p1 = makePart({ id: "p1", part_index: 1, file_path: "/local/b.wav" });

  it("uses local file_path for present parts (same-device fast path)", () => {
    const r = assembleTrack([p0, p1], [true, true], {}, url);
    expect(r.allPlayable).toBe(true);
    expect(r.parts).toEqual([
      { src: "stream:///local/a.wav", duration: 12 },
      { src: "stream:///local/b.wav", duration: 12 },
    ]);
  });

  it("stays optimistic (local) when presence is unchecked (null)", () => {
    const r = assembleTrack([p0], null, {}, url);
    expect(r.allPlayable).toBe(true);
    expect(r.parts[0].src).toBe("stream:///local/a.wav");
  });

  it("uses the fetched cache path for a missing-but-cached part", () => {
    const r = assembleTrack([p0, p1], [true, false], { p1: "/cache/p1.wav" }, url);
    expect(r.allPlayable).toBe(true);
    expect(r.parts[1].src).toBe("stream:///cache/p1.wav");
  });

  it("is not all-playable while a missing part has no cache yet", () => {
    const r = assembleTrack([p0, p1], [true, false], {}, url);
    expect(r.allPlayable).toBe(false);
    expect(r.parts[1].src).toBe("");
  });
});

function openCompleted(parts: DbAudioPart[]) {
  useAppStore.setState({
    selectedSessionId: "s-remote",
    activeSessionId: null,
    viewSession: makeSession({
      status: "completed",
      recording_device_id: "PEER",
    }),
    viewSessionSegments: [makeSegment("g1", "recorded on the peer")],
    viewSessionParts: parts,
    sessions: [],
    syncStatus: syncStatus(),
  });
}

/** Route the mocked `invoke` per command: prepare responses by part id, everything else
 *  resolves benignly. Returns the mock for call assertions. */
function mockSyncInvoke(byPart: Record<string, AudioPartPrepare>) {
  const m = vi.mocked(invoke);
  m.mockImplementation(async (cmd, args) => {
    if (cmd === "audio_prepare_part") {
      const id = String((args as Record<string, unknown>)?.partId ?? "");
      return byPart[id] ?? { state: "queued" };
    }
    if (cmd === "audio_release_part") return false;
    return null;
  });
  return m;
}

const prepareCalls = () =>
  vi
    .mocked(invoke)
    .mock.calls.filter(([cmd]) => cmd === "audio_prepare_part")
    .map(([, args]) => (args as Record<string, unknown>)?.partId);

describe("NoteDetailView audio auto-fetch (S3.5, honest player rendering)", () => {
  it("arms the fetch BY ITSELF when a synced session with missing audio opens", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "fetching", received: 25, total: 100 } });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    // No click: the progress bar appears on its own and the idle affordance never shows.
    expect(await screen.findByText("Fetching… 25%")).toBeInTheDocument();
    expect(screen.queryByText(/click to fetch/)).not.toBeInTheDocument();
    await waitFor(() => expect(prepareCalls()).toContain("p0"));
    // The session view submits in the HIGH class (outranks background dictation prefetches).
    expect(vi.mocked(invoke)).toHaveBeenCalledWith("audio_prepare_part", {
      partId: "p0",
      highPriority: true,
    });
    // The transcript still renders read-through; seeking stays withheld until fetched.
    expect(screen.getByText("recorded on the peer")).toBeInTheDocument();
    // Cancel affordance is present while fetching.
    expect(
      screen.getByRole("button", { name: "Cancel fetch" }),
    ).toBeInTheDocument();
  });

  it("submits multi-part fetches in part_index order and shows 'part K of M' copy", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false, false]);
    mockSyncInvoke({
      p0: { state: "ready", path: "/cache/p0.wav" },
      p1: { state: "fetching", received: 50, total: 100 },
    });
    openCompleted([
      makePart({ id: "p0" }),
      makePart({ id: "p1", part_index: 1, file_path: "/peer/audio/s-remote.1.wav" }),
    ]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    // K = first in-flight part's ordinal among missing (p1 → 2), M = total missing (2).
    // 75% = the landed part 1 (a full unit) plus half of part 2, over both parts.
    expect(await screen.findByText("Fetching part 2 of 2 — 75%")).toBeInTheDocument();
    // Ordered submission: p0 was prepared before p1 on the first tick.
    const calls = prepareCalls();
    expect(calls.indexOf("p0")).toBeLessThan(calls.indexOf("p1"));
  });

  it("renders the queued 'waiting' state while the global cap holds a part back", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "queued" } });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    expect(await screen.findByText("Waiting to fetch…")).toBeInTheDocument();
  });

  it("does NOT arm on a remote-LIVE session, then fires when D4 flips it to completed", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "fetching", received: 1, total: 100 } });
    // Foreign `recording` session with a (missing) part: remote-live branch, no fetch.
    useAppStore.setState({
      selectedSessionId: "s-remote",
      activeSessionId: null,
      viewSession: makeSession({ status: "recording", recording_device_id: "PEER" }),
      viewSessionSegments: [],
      viewSessionParts: [makePart()],
      sessions: [],
      syncStatus: syncStatus(),
    });
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    expect(await screen.findByText("Live on Windows")).toBeInTheDocument();
    await waitFor(() => expect(commands.audioFilesExist).toHaveBeenCalled());
    expect(prepareCalls()).toHaveLength(0);

    // The D4 live-refresh flips the row to completed while the view stays open → the arm
    // predicate re-evaluates and the fetch fires unprompted.
    useAppStore.setState({
      viewSession: makeSession({ status: "completed", recording_device_id: "PEER" }),
    });
    await waitFor(() => expect(prepareCalls()).toContain("p0"));
  });

  it("cancel (X) suppresses auto-fetch and offers the manual re-start affordance", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "fetching", received: 25, total: 100 } });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    // fireEvent (not userEvent): the resizable-panels lib's global pointer handlers
    // are not jsdom-safe, and this interaction only needs the click itself.
    fireEvent.click(await screen.findByRole("button", { name: "Cancel fetch" }));
    // Suppressed: back to the idle affordance (auto-fetch must not instantly re-arm).
    expect(
      await screen.findByText("Audio is on Windows — click to fetch"),
    ).toBeInTheDocument();
    expect(vi.mocked(invoke)).toHaveBeenCalledWith("audio_cancel_part", {
      partId: "p0",
    });
  });

  it("verification_failed offers a Retry that clears the slot and re-prepares", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "verification_failed" } });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    expect(await screen.findByText("Audio failed verification")).toBeInTheDocument();
    const callsBefore = prepareCalls().length;
    // Retry clears the sticky tamper terminal (cancel → slot removed) and re-prepares.
    fireEvent.click(screen.getByRole("button", { name: "Retry fetch" }));
    await waitFor(() =>
      expect(vi.mocked(invoke)).toHaveBeenCalledWith("audio_cancel_part", {
        partId: "p0",
      }),
    );
    await waitFor(() => expect(prepareCalls().length).toBeGreaterThan(callsBefore));
  });

  it("renders the self-healing auth-expired copy (no Retry — the drain reconnects)", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "auth_expired" } });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    expect(
      await screen.findByText("Sync session expired — reconnecting…"),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Retry fetch" }),
    ).not.toBeInTheDocument();
  });

  it("surfaces the no_space terminal with the need-~X copy and a Retry", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({
      p0: { state: "no_space", needed: 2 * 1024 * 1024 * 1024 },
    });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    expect(
      await screen.findByText("Not enough disk space (need ~2.0 GB)"),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Retry fetch" })).toBeInTheDocument();
  });

  it("labels an unknown recording device 'another device' (on_device re-probe state)", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "not_on_server" } });
    openCompleted([makePart()]);
    useAppStore.setState({
      viewSession: makeSession({
        status: "completed",
        recording_device_id: "GHOST",
      }),
    });
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    // The source device hasn't uploaded yet: honest on-device copy (re-probed on the slow
    // cadence — the fetch self-starts when the upload lands).
    expect(
      await screen.findByText("Audio is on another device"),
    ).toBeInTheDocument();
  });

  it("falls back to the disabled 'Audio is on <label>' bar when sync is OFF", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    openCompleted([makePart()]);
    // Sync disabled → nothing to fetch from; the honest disabled bar remains, no fetch.
    useAppStore.setState({ syncStatus: syncStatus({ syncEnabled: false }) });
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    expect(await screen.findByText("Audio is on Windows")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Audio unavailable" }),
    ).toBeDisabled();
    expect(prepareCalls()).toHaveLength(0);
  });

  it("keeps the normal player (no unavailable copy) when the file is present", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([true]);
    openCompleted([makePart({ file_path: "/local/audio/s-remote.0.wav" })]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    await waitFor(() =>
      expect(commands.audioFilesExist).toHaveBeenCalledWith([
        "/local/audio/s-remote.0.wav",
      ]),
    );
    expect(screen.queryByText(/Audio is on/)).not.toBeInTheDocument();
    expect(prepareCalls()).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// Audio-row stability. Everything below is about what the row does BETWEEN the
// honest states above: while the presence probe is unresolved, while a re-probe
// runs under an in-flight fetch, and while progress crosses a part boundary.
// ---------------------------------------------------------------------------

const audioRow = () => document.querySelector("[data-audio-row='placeholder']");
const playerEl = () => document.querySelector("audio[data-session-audio]");
const releaseCalls = () =>
  vi
    .mocked(invoke)
    .mock.calls.filter(([cmd]) => cmd === "audio_release_part")
    .map(([, args]) => (args as Record<string, unknown>)?.partId);

describe("NoteDetailView audio row — probe stability", () => {
  it("holds an inert placeholder while the FIRST presence probe is unresolved", async () => {
    // A probe that never answers: the mount tick must commit to nothing.
    vi.mocked(commands.audioFilesExist).mockReturnValue(new Promise(() => {}));
    mockSyncInvoke({ p0: { state: "fetching", received: 10, total: 100 } });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    await waitFor(() => expect(commands.audioFilesExist).toHaveBeenCalled());
    // Not the player (it would be yanked away if the bytes are elsewhere)…
    expect(playerEl()).toBeNull();
    // …and not the fetch bar (no fetch is known to be needed yet).
    expect(screen.queryByText(/Fetching/)).not.toBeInTheDocument();
    expect(screen.queryByText(/click to fetch/)).not.toBeInTheDocument();
    expect(screen.queryByText(/Audio is on/)).not.toBeInTheDocument();
    // The space is reserved instead, so nothing below it moves when it resolves.
    expect(audioRow()).not.toBeNull();
    expect(prepareCalls()).toHaveLength(0);
  });

  it("resolves the placeholder into the real player once the probe answers", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([true]);
    mockSyncInvoke({});
    openCompleted([makePart({ file_path: "/local/a.wav" })]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    await waitFor(() => expect(playerEl()).not.toBeNull());
    expect(audioRow()).toBeNull();
  });

  it("a window-focus re-probe mid-fetch holds the bar and never releases the queued parts", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false, false]);
    mockSyncInvoke({
      p0: { state: "fetching", received: 40, total: 100 },
      p1: { state: "queued" },
    });
    openCompleted([
      makePart({ id: "p0" }),
      makePart({ id: "p1", part_index: 1, file_path: "/peer/audio/s-remote.1.wav" }),
    ]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    // (0.4 + 0) / 2 parts = 20%.
    expect(await screen.findByText("Fetching part 1 of 2 — 20%")).toBeInTheDocument();
    const probesBefore = vi.mocked(commands.audioFilesExist).mock.calls.length;
    expect(releaseCalls()).toHaveLength(0);

    // The user clicks into another app and back. This used to blank the presence
    // result, which unmounted the bar, tore down the poll effect (releasing p1's
    // queue slot) and restarted the progress at 0.
    fireEvent(window, new Event("focus"));
    await waitFor(() =>
      expect(vi.mocked(commands.audioFilesExist).mock.calls.length).toBeGreaterThan(
        probesBefore,
      ),
    );

    // The bar is still the bar, at the same progress…
    expect(screen.getByText("Fetching part 1 of 2 — 20%")).toBeInTheDocument();
    expect(audioRow()).toBeNull();
    expect(screen.queryByText(/click to fetch/)).not.toBeInTheDocument();
    // …and p1 kept its place in the queue.
    expect(releaseCalls()).toHaveLength(0);
  });

  it("does not restart the probe when a live refresh replaces the parts array in place", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "fetching", received: 30, total: 100 } });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    expect(await screen.findByText("Fetching… 30%")).toBeInTheDocument();
    const probesBefore = vi.mocked(commands.audioFilesExist).mock.calls.length;

    // D4 live refresh: a NEW array holding the SAME part rows.
    await act(async () => {
      useAppStore.setState({ viewSessionParts: [makePart()] });
    });

    expect(screen.getByText("Fetching… 30%")).toBeInTheDocument();
    expect(audioRow()).toBeNull();
    expect(vi.mocked(commands.audioFilesExist).mock.calls.length).toBe(probesBefore);
    expect(releaseCalls()).toHaveLength(0);
  });
});

describe("NoteDetailView audio row — fetch-state presentation", () => {
  it("never flashes 'Fetching…' when the part is already in the cache", async () => {
    // The prepare resolves ready on the first tick. Anything the UI says about
    // fetching in between is a lie it has to take back.
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "ready", path: "/cache/p0.wav" } });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    await waitFor(() => expect(prepareCalls()).toContain("p0"));
    expect(screen.queryByText(/Fetching/)).not.toBeInTheDocument();
    // Give the label debounce more than its window to (not) fire.
    await new Promise((r) => setTimeout(r, 300));
    expect(screen.queryByText(/Fetching/)).not.toBeInTheDocument();
    // The cached part plays.
    await waitFor(() => expect(playerEl()).not.toBeNull());
  });

  it("enters the player with the view transition when it replaces a fetch bar", async () => {
    let ready = false;
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd) => {
      if (cmd === "audio_prepare_part") {
        return ready
          ? { state: "ready", path: "/cache/p0.wav" }
          : { state: "fetching", received: 20, total: 100 };
      }
      return null;
    });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    expect(await screen.findByText("Fetching… 20%")).toBeInTheDocument();

    ready = true;
    await waitFor(() => expect(playerEl()).not.toBeNull(), { timeout: 3000 });
    expect(playerEl()?.parentElement?.className).toContain("view-enter");
  });

  it("gives a local session's player no entrance transition (it was never absent)", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([true]);
    mockSyncInvoke({});
    openCompleted([makePart({ file_path: "/local/a.wav" })]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    await waitFor(() => expect(playerEl()).not.toBeNull());
    expect(playerEl()?.parentElement?.className).not.toContain("view-enter");
  });

  it("keeps the rendered percent non-decreasing when a poll reports less", async () => {
    // Two ticks: the second one reports LOWER progress (a re-probe that lost the
    // byte count). The bar must hold, not rewind.
    let tick = 0;
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd) => {
      if (cmd === "audio_prepare_part") {
        tick += 1;
        return tick <= 1
          ? { state: "fetching", received: 80, total: 100 }
          : { state: "fetching", received: 5, total: 100 };
      }
      return null;
    });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    expect(await screen.findByText("Fetching… 80%")).toBeInTheDocument();
    await waitFor(() => expect(tick).toBeGreaterThan(1), { timeout: 3000 });
    // The low reading landed; the display is still at the high-water mark.
    expect(screen.getByText("Fetching… 80%")).toBeInTheDocument();
    expect(screen.queryByText("Fetching… 5%")).not.toBeInTheDocument();
  });

  it("releases the ratchet when the backend re-queues the part (re-download from 0)", async () => {
    // Distinct from the test above: that one is a stale reading the bar should ignore;
    // this one is a real restart the bar must follow down, or it sits frozen at 80%
    // for the whole re-download.
    let tick = 0;
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd) => {
      if (cmd === "audio_prepare_part") {
        tick += 1;
        // 80% … then the part is re-queued and starts over.
        return tick <= 1
          ? { state: "fetching", received: 80, total: 100 }
          : { state: "fetching", received: 0, total: 100 };
      }
      return null;
    });
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    expect(await screen.findByText("Fetching… 80%")).toBeInTheDocument();
    await waitFor(() => expect(tick).toBeGreaterThan(1), { timeout: 3000 });
    await waitFor(() =>
      expect(screen.getByText("Fetching…")).toBeInTheDocument(),
    );
    expect(screen.queryByText("Fetching… 80%")).not.toBeInTheDocument();
  });

  it("shows an animated (not dead) track while a part waits for a download permit", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    mockSyncInvoke({ p0: { state: "queued" } });
    openCompleted([makePart()]);
    const { container } = render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );
    await screen.findByText("Waiting to fetch…");
    expect(container.querySelector(".animate-command-loading")).not.toBeNull();
  });
});
