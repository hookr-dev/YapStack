import { describe, it, expect } from "vitest";
import type { DbSegment } from "@/lib/db";
import {
  BACKFILL_DIVIDER_KEY,
  buildTranscriptRows,
  estimateRowHeight,
  type TranscriptRow,
} from "./rows";

function seg(id: string, overrides?: Partial<DbSegment>): DbSegment {
  return {
    id,
    session_id: "s1",
    source: "Mic",
    text: `text ${id}`,
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
    ...overrides,
  };
}

const keys = (rows: TranscriptRow[]) => rows.map((r) => r.key);

describe("buildTranscriptRows", () => {
  it("emits one row per segment, keyed by segment id, without diarization", () => {
    const rows = buildTranscriptRows([seg("a"), seg("b")], false, null);
    expect(keys(rows)).toEqual(["a", "b"]);
    expect(rows.every((r) => r.kind === "segment")).toBe(true);
  });

  it("drops segments whose text is blank", () => {
    const rows = buildTranscriptRows(
      [seg("a"), seg("b", { text: "   " }), seg("c")],
      false,
      null,
    );
    expect(keys(rows)).toEqual(["a", "c"]);
  });

  it("heads each run of one channel-qualified speaker", () => {
    const rows = buildTranscriptRows(
      [
        seg("a", { speaker_id: 0 }),
        seg("b", { speaker_id: 0 }),
        seg("c", { speaker_id: 0, source: "System" }),
        seg("d", { speaker_id: 1 }),
        seg("e"),
      ],
      false,
      null,
    );
    expect(keys(rows)).toEqual([
      "hdr:a", "a", "b",
      "hdr:c", "c",
      "hdr:d", "d",
      "e",
    ]);
  });

  it("keys a header by its run's first visible segment and skips all-blank runs", () => {
    const rows = buildTranscriptRows(
      [
        seg("a", { speaker_id: 0, text: "" }),
        seg("b", { speaker_id: 0 }),
        seg("c", { speaker_id: 1, text: " " }),
      ],
      false,
      null,
    );
    expect(keys(rows)).toEqual(["hdr:b", "b"]);
  });

  it("merges one speaker's runs around a blank-only run of another speaker", () => {
    const rows = buildTranscriptRows(
      [
        seg("a", { speaker_id: 0 }),
        seg("b", { speaker_id: 1, text: "" }),
        seg("c", { speaker_id: 0 }),
      ],
      false,
      null,
    );
    expect(keys(rows)).toEqual(["hdr:a", "a", "c"]);
  });

  it("splits at the backfill boundary with the divider between halves", () => {
    const rows = buildTranscriptRows(
      [
        seg("a", { audio_offset_seconds: 0 }),
        seg("b", { audio_offset_seconds: 5 }),
        seg("c", { audio_offset_seconds: 10 }),
      ],
      true,
      10,
    );
    expect(keys(rows)).toEqual(["a", "b", BACKFILL_DIVIDER_KEY, "c"]);
  });

  it("puts every segment below the divider until the boundary is known", () => {
    const rows = buildTranscriptRows([seg("a"), seg("b")], true, null);
    expect(keys(rows)).toEqual([BACKFILL_DIVIDER_KEY, "a", "b"]);
  });

  it("puts every segment above the divider when none reach the boundary", () => {
    const rows = buildTranscriptRows([seg("a")], true, 30);
    expect(keys(rows)).toEqual(["a", BACKFILL_DIVIDER_KEY]);
  });

  it("groups speakers within each half separately", () => {
    const rows = buildTranscriptRows(
      [
        seg("a", { speaker_id: 0, audio_offset_seconds: 0 }),
        seg("b", { speaker_id: 0, audio_offset_seconds: 10 }),
      ],
      true,
      10,
    );
    expect(keys(rows)).toEqual(["hdr:a", "a", BACKFILL_DIVIDER_KEY, "hdr:b", "b"]);
  });
});

describe("estimateRowHeight", () => {
  const row = (text: string): TranscriptRow => ({
    kind: "segment",
    key: "x",
    segment: seg("x", { text }),
  });

  it("estimates a one-line bubble", () => {
    expect(estimateRowHeight(row("hi"), 400)).toBe(47);
  });

  it("adds a line per wrap, and wraps sooner when narrower", () => {
    const long = "word ".repeat(60);
    const wide = estimateRowHeight(row(long), 800);
    const narrow = estimateRowHeight(row(long), 300);
    expect(wide).toBeGreaterThan(47);
    expect(narrow).toBeGreaterThan(wide);
    expect((narrow - 27.5) % 19.5).toBe(0);
  });

  it("counts explicit line breaks", () => {
    expect(estimateRowHeight(row("a\nb\nc"), 400)).toBe(27.5 + 19.5 * 3);
  });

  it("uses fixed heights for headers and the divider", () => {
    expect(estimateRowHeight({ kind: "divider", key: "d" }, 400)).toBeGreaterThan(0);
    expect(
      estimateRowHeight({ kind: "speaker", key: "h", segment: seg("x"), speakerId: 0 }, 400),
    ).toBeGreaterThan(0);
  });
});
