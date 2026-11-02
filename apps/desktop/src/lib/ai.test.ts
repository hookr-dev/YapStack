import { describe, it, expect, vi, afterEach } from "vitest";

// Mock db.ts to prevent Tauri import chain
vi.mock("@/lib/db", () => ({}));
// Mock openai to prevent network imports
vi.mock("openai", () => ({
  default: vi.fn(),
}));
// Mock the Tauri HTTP plugin — AI requests go through it to bypass CORS.
// Individual tests override this mock's behavior via `vi.mocked(tauriFetch)`.
vi.mock("@tauri-apps/plugin-http", () => ({
  fetch: vi.fn(),
}));

import { fetch as tauriFetch } from "@tauri-apps/plugin-http";

import {
  chatContextKey,
  chatContextTypeFromKey,
  assembleTranscriptContext,
  assembleNoteContext,
  formatSegmentSpeaker,
  markdownToBasicHtml,
  fetchCustomModels,
  shouldAutoRefreshModels,
  isVisibleSegment,
  type ChatContext,
} from "./ai";
import type { DbSegment } from "./db";

function makeSegment(overrides: Partial<DbSegment> = {}): DbSegment {
  return {
    id: "s1",
    session_id: "sess",
    source: "Mic",
    text: "hello",
    audio_offset_seconds: 0,
    chunk_duration_seconds: 1,
    confidence: 0.9,
    created_at: "",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
    speaker_id: null,
    ...overrides,
  };
}

describe("isVisibleSegment (the AI visibility gate)", () => {
  it("is true for a normal segment", () => {
    expect(isVisibleSegment(makeSegment())).toBe(true);
  });

  it("is false for a hidden segment", () => {
    expect(isVisibleSegment(makeSegment({ hidden: 1 }))).toBe(false);
  });

  it("is false for a soft-deleted segment", () => {
    expect(
      isVisibleSegment(makeSegment({ deleted_at: "2026-01-01T00:00:00Z" })),
    ).toBe(false);
  });

  it("excludes hidden/deleted segments from assembled transcript context", () => {
    const out = assembleTranscriptContext([
      makeSegment({ id: "a", text: "visible one" }),
      makeSegment({ id: "b", text: "hidden one", hidden: 1 }),
      makeSegment({ id: "c", text: "deleted one", deleted_at: "2026-01-01" }),
    ]);
    expect(out).toContain("visible one");
    expect(out).not.toContain("hidden one");
    expect(out).not.toContain("deleted one");
  });
});

describe("chatContextKey", () => {
  it("returns sessionId for session context", () => {
    const ctx: ChatContext = { type: "session", sessionId: "abc-123" };
    expect(chatContextKey(ctx)).toBe("abc-123");
  });

  it("returns folder: prefix for folder context", () => {
    const ctx: ChatContext = { type: "folder", folderId: "folder-1" };
    expect(chatContextKey(ctx)).toBe("folder:folder-1");
  });

  it('returns "global" for all context', () => {
    const ctx: ChatContext = { type: "all" };
    expect(chatContextKey(ctx)).toBe("global");
  });

  it('returns "pinned" for pinned context', () => {
    const ctx: ChatContext = { type: "pinned" };
    expect(chatContextKey(ctx)).toBe("pinned");
  });
});

describe("chatContextTypeFromKey", () => {
  it("round-trips every context type without the id", () => {
    const contexts: ChatContext[] = [
      { type: "session", sessionId: "3f2b8c1e-9d4a-4e7b-8a6f-1c2d3e4f5a6b" },
      { type: "folder", folderId: "folder-1" },
      { type: "all" },
      { type: "pinned" },
      { type: "dictation" },
    ];
    for (const ctx of contexts) {
      expect(chatContextTypeFromKey(chatContextKey(ctx))).toBe(ctx.type);
    }
  });
});

describe("formatSegmentSpeaker", () => {
  function seg(overrides: Partial<DbSegment>): DbSegment {
    return {
      id: "s1",
      session_id: "sess",
      source: "Mic",
      text: "",
      audio_offset_seconds: 0,
      chunk_duration_seconds: 1,
      confidence: 0.9,
      created_at: "",
      chunk_index: 0,
      original_text: null,
      edited_at: null,
      deleted_at: null,
      hidden: 0,
      speaker_id: null,
      ...overrides,
    };
  }

  it("returns 'You' for an undiarized Mic segment", () => {
    expect(formatSegmentSpeaker(seg({ source: "Mic" }))).toBe("You");
  });

  // A diarized mic means several people in the room share the user's
  // microphone, so a mic cluster is not assumed to be the user.
  it("returns 'Speaker N' for a diarized Mic segment, not 'You'", () => {
    expect(formatSegmentSpeaker(seg({ source: "Mic", speaker_id: 0 }))).toBe(
      "Speaker 1",
    );
    expect(formatSegmentSpeaker(seg({ source: "Mic", speaker_id: 1 }))).toBe(
      "Speaker 2",
    );
  });

  it("returns 'You' for a Mic cluster marked as the user", () => {
    expect(
      formatSegmentSpeaker(seg({ source: "Mic", speaker_id: 1 }), {
        "mic:c1": { is_self_override: true },
      }),
    ).toBe("You");
  });

  it("returns 'Other' for System source without speaker_id", () => {
    expect(formatSegmentSpeaker(seg({ source: "System" }))).toBe("Other");
  });

  it("returns 'Speaker N' (1-indexed) for diarised System segments", () => {
    expect(
      formatSegmentSpeaker(seg({ source: "System", speaker_id: 0 })),
    ).toBe("Speaker 1");
    expect(
      formatSegmentSpeaker(seg({ source: "System", speaker_id: 2 })),
    ).toBe("Speaker 3");
  });

  it("uses speaker display names when provided", () => {
    expect(
      formatSegmentSpeaker(seg({ source: "System", speaker_id: 0 }), {
        "system:c0": { display_name: "Alice" },
      }),
    ).toBe("Alice");
  });
});

describe("assembleTranscriptContext", () => {
  it("returns empty string for no segments", () => {
    expect(assembleTranscriptContext([])).toBe("");
  });

  it("formats a single segment", () => {
    const segments = [
      {
        id: "seg1",
        session_id: "s1",
        source: "Mic" as const,
        text: "Hello world",
        audio_offset_seconds: 65,
        chunk_duration_seconds: 5,
        confidence: 0.9,
        created_at: "",
        chunk_index: 0,
        original_text: null,
        edited_at: null,
        deleted_at: null,
        hidden: 0,
      },
    ];
    expect(assembleTranscriptContext(segments)).toBe(
      "[seg:seg1 1:05] (You) Hello world",
    );
  });

  it("filters hidden segments", () => {
    const segments = [
      {
        id: "seg1",
        session_id: "s1",
        source: "Mic" as const,
        text: "Visible",
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
      {
        id: "seg2",
        session_id: "s1",
        source: "Mic" as const,
        text: "Hidden",
        audio_offset_seconds: 5,
        chunk_duration_seconds: 5,
        confidence: 0.9,
        created_at: "",
        chunk_index: 1,
        original_text: null,
        edited_at: null,
        deleted_at: null,
        hidden: 1,
      },
    ];
    expect(assembleTranscriptContext(segments)).toBe(
      "[seg:seg1 0:00] (You) Visible",
    );
  });

  it("filters deleted segments", () => {
    const segments = [
      {
        id: "seg1",
        session_id: "s1",
        source: "Mic" as const,
        text: "Deleted",
        audio_offset_seconds: 0,
        chunk_duration_seconds: 5,
        confidence: 0.9,
        created_at: "",
        chunk_index: 0,
        original_text: null,
        edited_at: null,
        deleted_at: "2024-01-01",
        hidden: 0,
        speaker_id: null,
      },
    ];
    expect(assembleTranscriptContext(segments)).toBe("");
  });
});

describe("assembleNoteContext", () => {
  it("strips HTML tags", () => {
    expect(assembleNoteContext("<p>Hello <b>world</b></p>")).toBe("Hello world");
  });

  it("converts br to newline", () => {
    expect(assembleNoteContext("Line 1<br>Line 2")).toBe("Line 1\nLine 2");
  });

  it("decodes HTML entities", () => {
    expect(assembleNoteContext("A &amp; B &lt; C &gt; D")).toBe("A & B < C > D");
  });

  it("decodes quote entities", () => {
    expect(assembleNoteContext("&quot;hello&quot; &#39;world&#39;")).toBe(
      '"hello" \'world\'',
    );
  });

  it("collapses excessive newlines", () => {
    expect(assembleNoteContext("<p>A</p><p></p><p></p><p>B</p>")).toBe(
      "A\n\nB",
    );
  });
});

describe("markdownToBasicHtml", () => {
  it("converts bold text", () => {
    const html = markdownToBasicHtml("**bold**");
    expect(html).toContain("<strong>bold</strong>");
  });

  it("converts unordered list", () => {
    const html = markdownToBasicHtml("- item 1\n- item 2");
    expect(html).toContain("<li>item 1</li>");
    expect(html).toContain("<li>item 2</li>");
  });

  it("converts headings", () => {
    const html = markdownToBasicHtml("# Title");
    expect(html).toContain("<h1");
    expect(html).toContain("Title");
  });

  it("strips script tags (XSS prevention)", () => {
    const html = markdownToBasicHtml('<script>alert("xss")</script>');
    expect(html).not.toContain("<script");
    expect(html).not.toContain("alert");
  });

  it("strips event handler attributes", () => {
    const html = markdownToBasicHtml('<img src="x" onerror="alert(1)">');
    expect(html).not.toContain("onerror");
    expect(html).not.toContain("alert");
  });

  it("strips javascript: URLs", () => {
    const html = markdownToBasicHtml('[click](javascript:alert(1))');
    expect(html).not.toContain("javascript:");
  });

  it("preserves safe HTML from markdown", () => {
    const html = markdownToBasicHtml("**safe** and *italic*");
    expect(html).toContain("<strong>safe</strong>");
    expect(html).toContain("<em>italic</em>");
  });
});

describe("fetchCustomModels", () => {
  const mockFetch = vi.mocked(tauriFetch);

  afterEach(() => {
    mockFetch.mockReset();
  });

  it("appends /models to baseUrl and returns string ids", async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: async () => ({
        data: [{ id: "qwen2.5-7b" }, { id: "llama-3.1-8b" }],
      }),
    } as unknown as Response);

    const ids = await fetchCustomModels("http://127.0.0.1:8080/v1");
    expect(ids).toEqual(["qwen2.5-7b", "llama-3.1-8b"]);
    expect(mockFetch).toHaveBeenCalledWith("http://127.0.0.1:8080/v1/models");
  });

  it("strips trailing slash before appending /models", async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: async () => ({ data: [] }),
    } as unknown as Response);

    await fetchCustomModels("http://127.0.0.1:8080/v1/");
    expect(mockFetch).toHaveBeenCalledWith("http://127.0.0.1:8080/v1/models");
  });

  it("throws HTTP status on non-OK response", async () => {
    mockFetch.mockResolvedValue({
      ok: false,
      status: 404,
    } as unknown as Response);

    await expect(fetchCustomModels("http://x/v1")).rejects.toThrow("HTTP 404");
  });

  it("throws on unexpected response shape", async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: async () => ({ models: [] }),
    } as unknown as Response);

    await expect(fetchCustomModels("http://x/v1")).rejects.toThrow(
      "Unexpected response shape",
    );
  });

  it("filters out entries without a string id", async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: async () => ({
        data: [{ id: "a" }, { id: 42 }, { notId: "x" }, { id: "b" }],
      }),
    } as unknown as Response);

    const ids = await fetchCustomModels("http://x/v1");
    expect(ids).toEqual(["a", "b"]);
  });

  it("sends Bearer auth header when apiKey is provided (required by OpenAI)", async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: async () => ({ data: [{ id: "gpt-4o-mini" }] }),
    } as unknown as Response);

    await fetchCustomModels("https://api.openai.com/v1", "sk-test-key");
    expect(mockFetch).toHaveBeenCalledWith(
      "https://api.openai.com/v1/models",
      { headers: { Authorization: "Bearer sk-test-key" } },
    );
  });

  it("treats whitespace-only apiKey as no auth", async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: async () => ({ data: [] }),
    } as unknown as Response);

    await fetchCustomModels("http://x/v1", "   ");
    expect(mockFetch).toHaveBeenCalledWith("http://x/v1/models");
  });
});

describe("shouldAutoRefreshModels", () => {
  const base = {
    baseUrl: "https://api.openai.com/v1",
    baseUrlBaseline: "https://api.openai.com/v1",
    apiKey: "sk-key",
    apiKeyBaseline: "sk-key",
  };

  it("never refreshes while a fetch is already in flight", () => {
    expect(
      shouldAutoRefreshModels({
        ...base,
        mode: "create",
        hasLocalModels: false,
        fetching: true,
      }),
    ).toBe(false);
  });

  it("on create, refreshes only when no catalog was fetched in the dialog", () => {
    expect(
      shouldAutoRefreshModels({
        ...base,
        mode: "create",
        hasLocalModels: false,
        fetching: false,
      }),
    ).toBe(true);
    expect(
      shouldAutoRefreshModels({
        ...base,
        mode: "create",
        hasLocalModels: true,
        fetching: false,
      }),
    ).toBe(false);
  });

  it("on edit, refreshes when the base URL changed", () => {
    expect(
      shouldAutoRefreshModels({
        ...base,
        mode: "edit",
        hasLocalModels: true,
        fetching: false,
        baseUrl: "http://localhost:1234/v1",
      }),
    ).toBe(true);
  });

  // Regression: fixing only the API key (base URL unchanged) must still
  // re-validate, otherwise a stale fetchError / model list sticks around.
  it("on edit, refreshes when only the API key changed", () => {
    expect(
      shouldAutoRefreshModels({
        ...base,
        mode: "edit",
        hasLocalModels: true,
        fetching: false,
        apiKey: "sk-corrected-key",
      }),
    ).toBe(true);
  });

  // Regression: a manual Refresh in the dialog moves the baselines forward, so
  // saving must NOT fire a redundant second fetch (which on a flaky endpoint
  // could overwrite the just-fetched good catalog with an error).
  it("on edit, does not refresh when nothing changed since the baseline", () => {
    expect(
      shouldAutoRefreshModels({
        ...base,
        mode: "edit",
        hasLocalModels: true,
        fetching: false,
      }),
    ).toBe(false);
  });
});
