import type { DbSegment } from "@/lib/db";

/// Last segment whose start offset is at or before `seconds`, or null when
/// `seconds` precedes every segment. `segments` must be sorted ascending by
/// `audio_offset_seconds`; on equal offsets the later one wins.
export function findLastSegmentStartingAtOrBefore(
  segments: readonly DbSegment[],
  seconds: number,
): DbSegment | null {
  let lo = 0;
  let hi = segments.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (segments[mid].audio_offset_seconds <= seconds) lo = mid + 1;
    else hi = mid;
  }
  return lo === 0 ? null : segments[lo - 1];
}
