import { describe, it, expect, vi } from "vitest";
vi.mock("./db", () => ({}));
vi.mock("openai", () => ({ default: class {} }));
import { assembleTranscriptContext } from "./ai";
import type { DbSegment } from "./db";

function seg(o: Partial<DbSegment>): DbSegment {
  return {
    id: "s",
    session_id: "sess",
    source: "Mic",
    text: "",
    audio_offset_seconds: 0,
    chunk_duration_seconds: 1,
    confidence: 1,
    created_at: "",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
    speaker_id: null,
    ...o,
  };
}

describe("in-person speakers on the mic channel", () => {
  it("does not attribute a second in-room speaker's words to the user", () => {
    const ctx = assembleTranscriptContext([
      seg({ id: "a", source: "Mic", speaker_id: 0, text: "I'll own the budget." }),
      seg({ id: "b", source: "Mic", speaker_id: 1, audio_offset_seconds: 5, text: "No, I will." }),
    ]);
    const labels = [...ctx.matchAll(/\] \(([^)]+)\)/g)].map((m) => m[1]);
    expect(new Set(labels).size).toBe(2);
  });
});
