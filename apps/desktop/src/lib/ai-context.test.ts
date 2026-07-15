import { describe, it, expect, vi } from "vitest";

vi.mock("@/lib/db", () => ({
  getSessionSegments: vi.fn().mockResolvedValue([
    {
      id: "s1",
      session_id: "session-1",
      source: "Mic",
      text: "Hello",
      audio_offset_seconds: 0,
      chunk_duration_seconds: 5,
      confidence: 0.9,
      created_at: "",
      chunk_index: 0,
      original_text: null,
      edited_at: null,
      deleted_at: null,
      hidden: 0,
    },
  ]),
  getNote: vi.fn().mockResolvedValue(null),
  getSession: vi.fn().mockResolvedValue(null),
  getSessionsByIds: vi.fn().mockResolvedValue([]),
  listDictationHistory: vi.fn().mockResolvedValue([]),
  getSessionTagIds: vi.fn().mockResolvedValue([]),
  listAllSessionFolders: vi.fn().mockResolvedValue([]),
  listTags: vi.fn().mockResolvedValue([]),
  listFolders: vi.fn().mockResolvedValue([]),
}));
vi.mock("@/lib/db-backend", () => ({
  default: { load: vi.fn() },
}));

import {
  createSessionSources,
  createMultiSessionSources,
  createSessionTools,
  createMultiSessionTools,
  resolveListContext,
} from "./ai-context";
import type { DbSession, DbFolder, DbDictationHistory } from "./db";

describe("createSessionSources", () => {
  it("returns transcript and notes sources for recording type", () => {
    const sources = createSessionSources("session-1", 5, "transcription");
    expect(sources.length).toBe(2);
    expect(sources[0].type).toBe("transcript");
    expect(sources[1].type).toBe("notes");
  });

  it("returns only notes source for manual type", () => {
    const sources = createSessionSources("session-1", 0, "manual");
    expect(sources.length).toBe(1);
    expect(sources[0].type).toBe("notes");
  });

  it("transcript source is enabled by default", () => {
    const sources = createSessionSources("session-1", 0, "transcription");
    expect(sources[0].enabled).toBe(true);
  });

  it("transcript source is toggleable", () => {
    const sources = createSessionSources("session-1", 0, "transcription");
    expect(sources[0].toggleable).toBe(true);
  });

  it("notes source is toggleable", () => {
    const sources = createSessionSources("session-1", 0, "transcription");
    expect(sources[1].toggleable).toBe(true);
  });

  it("transcript summary includes segment count", () => {
    const sources = createSessionSources("session-1", 12, "transcription");
    expect(sources[0].summary).toBe("12 segments");
  });

  it("transcript assembler calls DB and returns formatted text", async () => {
    const sources = createSessionSources("session-1", 1, "transcription");
    const result = await sources[0].assembler();
    expect(result).toContain("[seg:s1 0:00] (You) Hello");
  });
});

describe("createSessionTools", () => {
  it("returns tool IDs for session context", () => {
    const tools = createSessionTools("session-1");
    expect(tools.availableToolIds).toContain("update_title");
    expect(tools.availableToolIds).toContain("save_to_notes");
    expect(tools.availableToolIds).toContain("pin_session");
  });

});

describe("createMultiSessionSources", () => {
  it("returns a single compact candidates source", () => {
    const sources = createMultiSessionSources([], 0);
    expect(sources.length).toBe(1);
    expect(sources[0].type).toBe("sessions");
  });

  it("sessions source is not toggleable", () => {
    const sources = createMultiSessionSources([], 0);
    expect(sources[0].toggleable).toBe(false);
  });

  it("includes summary with session count", () => {
    const sources = createMultiSessionSources(["s1", "s2"], 2);
    expect(sources[0].summary).toContain("2");
  });
});

describe("createMultiSessionTools", () => {
  it("exposes retrieval-only tools for multi-session context", () => {
    const tools = createMultiSessionTools(["s1", "s2"]);
    expect(tools.availableToolIds).toContain("search_sessions");
    expect(tools.availableToolIds).toContain("get_session_context");
    expect(tools.availableToolIds).toContain("search_folders");
    // No mutating tools at this scope — they need a single sessionId.
    expect(tools.availableToolIds).not.toContain("update_title");
    expect(tools.availableToolIds).not.toContain("save_to_notes");
    expect(tools.availableToolIds).not.toContain("pin_session");
    expect(tools.availableToolIds).not.toContain("tag_session");
    expect(tools.availableToolIds).not.toContain("add_session_to_folder");
  });

  it("threads allowedSessionIds through getToolContext", async () => {
    const tools = createMultiSessionTools(["s1", "s2"]);
    expect(tools.getToolContext).not.toBeNull();
    const ctx = await tools.getToolContext!();
    expect(ctx.allowedSessionIds).toEqual(["s1", "s2"]);
  });
});

// --- resolveListContext ---

function makeSession(overrides: Partial<DbSession> = {}): DbSession {
  return {
    id: "s1",
    title: "Test",
    created_at: "",
    updated_at: "",
    source: "Mic",
    status: "completed",
    duration_seconds: null,
    total_segments: 0,
    folder_id: null,
    is_pinned: 0,
    pinned_at: null,
    session_type: "transcription",
    sort_order: 0,
    recording_device_id: null,
    ...overrides,
  };
}

function makeDictation(overrides: Partial<DbDictationHistory> = {}): DbDictationHistory {
  return {
    id: "d1",
    slot_id: "slot1",
    slot_name: "Dictation 1",
    input_text: "hello",
    output_text: "Hello.",
    ai_enabled: 0,
    ai_prompt: null,
    output_action: "paste",
    wav_file_path: null,
    wav_duration_seconds: null,
    session_id: null,
    created_at: "",
    ...overrides,
  };
}

const emptyDeps = {
  sessions: [] as DbSession[],
  sessionFolderMap: {} as Record<string, string[]>,
  folders: [] as DbFolder[],
  dictationHistory: [] as DbDictationHistory[],
};

describe("resolveListContext", () => {
  it('"all" returns global contextKey with a single compact candidates source', () => {
    const sessions = [makeSession({ id: "a" }), makeSession({ id: "b" })];
    const result = resolveListContext({ type: "all" }, { ...emptyDeps, sessions });
    expect(result.contextKey).toBe("global");
    expect(result.sources).toHaveLength(1);
    expect(result.sources[0].id).toBe("sessions");
    expect(result.placeholder).toBe("Ask about 2 sessions...");
  });

  it('"pinned" filters to pinned sessions only', () => {
    const sessions = [
      makeSession({ id: "a", is_pinned: 1 }),
      makeSession({ id: "b", is_pinned: 0 }),
    ];
    const result = resolveListContext({ type: "pinned" }, { ...emptyDeps, sessions });
    expect(result.contextKey).toBe("pinned");
    expect(result.sources[0].label).toBe("1 Sessions");
    expect(result.placeholder).toBe("Ask about 1 pinned session...");
  });

  it('"folder" filters sessions by folder membership and passes folder layers', () => {
    const sessions = [makeSession({ id: "a" }), makeSession({ id: "b" })];
    const folders: DbFolder[] = [
      { id: "f1", name: "Work", parent_id: null, sort_order: 0, icon: null, color: null, description: "Work stuff", created_at: "", updated_at: "" },
    ];
    const sessionFolderMap: Record<string, string[]> = { a: ["f1"] };
    const result = resolveListContext(
      { type: "folder", folderId: "f1" },
      { ...emptyDeps, sessions, folders, sessionFolderMap },
    );
    expect(result.contextKey).toBe("folder:f1");
    expect(result.sources[0].label).toBe("1 Sessions");
    expect(result.placeholder).toBe("Ask about 1 session...");
  });

  it('"dictation" returns dictation contextKey and source', () => {
    const dictationHistory = [makeDictation(), makeDictation({ id: "d2" })];
    const result = resolveListContext({ type: "dictation" }, { ...emptyDeps, dictationHistory });
    expect(result.contextKey).toBe("dictation");
    expect(result.sources).toHaveLength(1);
    expect(result.sources[0].id).toBe("dictation");
    expect(result.placeholder).toBe("Ask about 2 dictations...");
  });

  it("pluralizes correctly for 0, 1, and 2 sessions", () => {
    const r0 = resolveListContext({ type: "all" }, { ...emptyDeps, sessions: [] });
    expect(r0.placeholder).toBe("Ask about 0 sessions...");

    const r1 = resolveListContext({ type: "all" }, { ...emptyDeps, sessions: [makeSession()] });
    expect(r1.placeholder).toBe("Ask about 1 session...");

    const r2 = resolveListContext({ type: "all" }, { ...emptyDeps, sessions: [makeSession({ id: "a" }), makeSession({ id: "b" })] });
    expect(r2.placeholder).toBe("Ask about 2 sessions...");
  });
});
