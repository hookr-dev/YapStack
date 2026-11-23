import { describe, it, expect } from "vitest";
import type { DbSegment } from "@/lib/db";
import type { TranscriptRow } from "./rows";
import {
  anchorAt,
  classifyRowVisibility,
  pickSegmentsInBand,
} from "./rowGeometry";

const segRow = (id: string): TranscriptRow => ({
  kind: "segment",
  key: id,
  segment: { id } as DbSegment,
});

// Rows a..e, 40px tall with 10px gaps: a 0–40, hdr 50–90, b 100–140, ...
const rows: TranscriptRow[] = [
  segRow("a"),
  { kind: "speaker", key: "hdr:b", segment: { id: "b" } as DbSegment, speakerId: 0 },
  segRow("b"),
  { kind: "divider", key: "divider:backfill" },
  segRow("c"),
];
const spans = rows.map((_, i) => ({ start: i * 50, end: i * 50 + 40 }));

describe("classifyRowVisibility", () => {
  it("is visible only when the whole row is inside the viewport", () => {
    expect(classifyRowVisibility({ start: 100, end: 140 }, 100, 40)).toBe("visible");
    expect(classifyRowVisibility({ start: 100, end: 140 }, 101, 100)).toBe("above");
    expect(classifyRowVisibility({ start: 100, end: 140 }, 0, 139)).toBe("below");
  });

  it("calls a row taller than the viewport above once its top has scrolled off", () => {
    expect(classifyRowVisibility({ start: 0, end: 900 }, 10, 600)).toBe("above");
  });
});

describe("anchorAt", () => {
  it("anchors to the row under the position", () => {
    expect(anchorAt(spans, rows, 115)).toEqual({ key: "b", offset: 15 });
  });

  it("anchors a gap position to the row below it", () => {
    expect(anchorAt(spans, rows, 45)).toEqual({ key: "hdr:b", offset: -5 });
  });

  it("anchors past the end to the last row", () => {
    expect(anchorAt(spans, rows, 1000)).toEqual({ key: "c", offset: 800 });
  });

  it("returns null for an empty list", () => {
    expect(anchorAt([], [], 10)).toBeNull();
  });
});

describe("pickSegmentsInBand", () => {
  it("picks segment rows overlapping the band, in order, skipping headers and dividers", () => {
    expect(pickSegmentsInBand(spans, rows, 30, 210)).toEqual(["a", "b", "c"]);
  });

  it("excludes rows the band only touches at an edge or a gap", () => {
    expect(pickSegmentsInBand(spans, rows, 140, 200)).toEqual([]);
    expect(pickSegmentsInBand(spans, rows, 139, 201)).toEqual(["b", "c"]);
  });
});
