import { render, screen } from "@testing-library/react";
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
import { NoteDetailView, isRemoteLiveSession } from "./NoteDetailView";

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
