import type { TranscriptRow } from "@/components/transcript/rows";

/// A row's extent in list content coordinates (a virtualizer measurement).
export interface RowSpan {
  start: number;
  end: number;
}

/// A content position expressed relative to a row, so it survives rows above
/// it being remeasured or inserted.
export interface RowAnchor {
  key: string;
  offset: number;
}

export type RowVisibility = "above" | "visible" | "below";

/// Where a row sits relative to the viewport. Only a fully visible row is
/// "visible"; a row cut off at the top is "above".
export function classifyRowVisibility(
  row: RowSpan,
  scrollTop: number,
  viewportHeight: number,
): RowVisibility {
  if (row.start < scrollTop) return "above";
  if (row.end > scrollTop + viewportHeight) return "below";
  return "visible";
}

/// Index of the first span ending after `y`; `spans.length` when none does.
/// Spans are in index order, so their ends ascend.
function firstEndingAfter(spans: readonly RowSpan[], y: number): number {
  let lo = 0;
  let hi = spans.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (spans[mid].end > y) hi = mid;
    else lo = mid + 1;
  }
  return lo;
}

/// The row at (or, in a gap, just below) content position `y`, and `y`'s
/// offset from that row's start.
export function anchorAt(
  spans: readonly RowSpan[],
  rows: readonly TranscriptRow[],
  y: number,
): RowAnchor | null {
  if (spans.length === 0) return null;
  const i = Math.min(firstEndingAfter(spans, y), spans.length - 1);
  return { key: rows[i].key, offset: y - spans[i].start };
}

/// Segment ids of every segment row overlapping the vertical band
/// `[top, bottom]`, in display order. Horizontal position is ignored: a drag
/// selects whole rows between its top and bottom edges.
export function pickSegmentsInBand(
  spans: readonly RowSpan[],
  rows: readonly TranscriptRow[],
  top: number,
  bottom: number,
): string[] {
  const picked: string[] = [];
  for (let i = firstEndingAfter(spans, top); i < spans.length; i++) {
    if (spans[i].start >= bottom) break;
    const row = rows[i];
    if (row.kind === "segment") picked.push(row.segment.id);
  }
  return picked;
}
