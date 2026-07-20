import { render, screen, waitFor } from "@testing-library/react";
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
import { TooltipProvider } from "@/components/ui/tooltip";
import type { AudioPart } from "@/components/AudioPlayer";
import type { DbAudioPart } from "@/lib/db";
import {
  NoteDetailView,
  isRemoteLiveSession,
  selectAudioAvailability,
  assembleTrack,
} from "./NoteDetailView";

function makeSession(over: Partial<DbSession> = {}): DbSession {
  return {
    id: "s-remote",
    title: "Meeting notes",
    created_at: "2026-07-15 12:00:00",
    updated_at: "2026-07-15 12:00:00",
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

function makeSegment(id: string, text: string): DbSegment {
  return {
    id,
    session_id: "s-remote",
    source: "System",
    text,
    audio_offset_seconds: 0,
    chunk_duration_seconds: 1,
    confidence: 1,
    created_at: "2026-07-15 12:00:05",
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

describe("NoteDetailView audio availability (honest player rendering)", () => {
  it("renders a disabled control + 'Audio is on <label>' when the file is absent", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    openCompleted([makePart()]);
    render(
      <TooltipProvider>
        <NoteDetailView />
      </TooltipProvider>,
    );

    // S3: with sync ENABLED the honest missing state offers fetch-on-demand (not a dead
    // disabled control) — an enabled "fetch" affordance labelled by the source device.
    expect(
      await screen.findByText("Audio is on Windows — click to fetch"),
    ).toBeInTheDocument();
    const btn = screen.getByRole("button", { name: "Fetch and play audio" });
    expect(btn).not.toBeDisabled();
    // The transcript still renders read-through; playback seeking stays withheld until fetched.
    expect(screen.getByText("recorded on the peer")).toBeInTheDocument();
  });

  it("falls back to the disabled 'Audio is on <label>' bar when sync is OFF", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    openCompleted([makePart()]);
    // Sync disabled → nothing to fetch from; the honest disabled bar remains.
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
  });

  it("labels an unknown recording device 'another device'", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
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
    expect(
      await screen.findByText("Audio is on another device — click to fetch"),
    ).toBeInTheDocument();
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
  });
});
