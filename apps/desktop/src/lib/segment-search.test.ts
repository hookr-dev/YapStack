import { describe, it, expect } from "vitest";
import type { DbSegment } from "@/lib/db";
import { findLastSegmentStartingAtOrBefore } from "./segment-search";

const at = (id: string, audio_offset_seconds: number) =>
  ({ id, audio_offset_seconds }) as DbSegment;

const segments = [at("a", 0), at("b", 5), at("c", 5), at("d", 12.5)];

function linearScan(list: DbSegment[], t: number): string | null {
  for (let i = list.length - 1; i >= 0; i--) {
    if (list[i].audio_offset_seconds <= t) return list[i].id;
  }
  return null;
}

describe("findLastSegmentStartingAtOrBefore", () => {
  it("returns null for an empty list or a time before the first segment", () => {
    expect(findLastSegmentStartingAtOrBefore([], 3)).toBeNull();
    expect(findLastSegmentStartingAtOrBefore([at("a", 2)], 1.9)).toBeNull();
  });

  it("matches a segment that starts exactly at the time", () => {
    expect(findLastSegmentStartingAtOrBefore(segments, 0)?.id).toBe("a");
    expect(findLastSegmentStartingAtOrBefore(segments, 12.5)?.id).toBe("d");
  });

  it("picks the later of segments sharing an offset", () => {
    expect(findLastSegmentStartingAtOrBefore(segments, 5)?.id).toBe("c");
    expect(findLastSegmentStartingAtOrBefore(segments, 9)?.id).toBe("c");
  });

  it("returns the last segment past the end", () => {
    expect(findLastSegmentStartingAtOrBefore(segments, 1e6)?.id).toBe("d");
  });

  it("agrees with a backward linear scan", () => {
    for (let t = -1; t <= 14; t += 0.25) {
      expect(findLastSegmentStartingAtOrBefore(segments, t)?.id ?? null).toBe(
        linearScan(segments, t),
      );
    }
  });
});
