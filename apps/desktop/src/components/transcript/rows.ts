import type { DbSegment } from "@/lib/db";
import { speakerKeyOf } from "@/lib/speakers";

/// One line of the rendered transcript. `key` is stable across appends,
/// prepends and re-renders, so a list can key rows (and remembered row
/// sizes) by it.
export type TranscriptRow =
  | { kind: "segment"; key: string; segment: DbSegment }
  | { kind: "speaker"; key: string; segment: DbSegment; speakerId: number }
  | { kind: "divider"; key: string };

export const BACKFILL_DIVIDER_KEY = "divider:backfill";

/// Flattens segments into display rows.
///
/// While backfill runs, segments split at `backfillBoundarySeconds` (backfill
/// audio covers `0..boundary`, live audio comes after) with the "Processing
/// prior audio" divider between the halves. Until the first backfill chunk
/// lands the boundary is null and every segment is live, below the divider.
///
/// Within each half, if any segment carries a diarization `speaker_id`, runs
/// of the same channel-qualified speaker get a header row for the run's
/// speaker. Segments with blank text render nothing, so they get no row.
export function buildTranscriptRows(
  segments: DbSegment[],
  backfillActive: boolean,
  backfillBoundarySeconds: number | null | undefined,
): TranscriptRow[] {
  const rows: TranscriptRow[] = [];
  if (!backfillActive) {
    pushHalf(rows, segments);
    return rows;
  }
  const boundary = backfillBoundarySeconds ?? 0;
  let split = segments.findIndex((s) => s.audio_offset_seconds >= boundary);
  if (split === -1) split = segments.length;
  pushHalf(rows, segments.slice(0, split));
  rows.push({ kind: "divider", key: BACKFILL_DIVIDER_KEY });
  pushHalf(rows, segments.slice(split));
  return rows;
}

function pushHalf(rows: TranscriptRow[], half: DbSegment[]) {
  // Blank segments go before grouping, so they can't split one speaker's
  // run into two runs with identical headers.
  const segments = half.filter((s) => s.text.trim() !== "");
  if (!segments.some((s) => s.speaker_id != null)) {
    for (const s of segments) rows.push({ kind: "segment", key: s.id, segment: s });
    return;
  }
  // Speaker keys are channel-qualified, so two adjacent same-id segments
  // from different sources don't merge under one header.
  let runKey: string | null = null;
  for (const s of segments) {
    const speakerKey = speakerKeyOf(s);
    if (speakerKey !== runKey) {
      runKey = speakerKey;
      if (s.speaker_id != null) {
        rows.push({ kind: "speaker", key: `hdr:${s.id}`, segment: s, speakerId: s.speaker_id });
      }
    }
    rows.push({ kind: "segment", key: s.id, segment: s });
  }
}

// Row heights from the transcript's CSS (EditableSegment, SpeakerHeader, the
// backfill divider). A bubble is `py-1.5` around `text-xs leading-relaxed`
// lines (19.5 px each), then a 2 px gap and a 13.5 px timestamp line.
const SEGMENT_CHROME_PX = 27.5;
const SEGMENT_LINE_PX = 19.5;
const SPEAKER_HEADER_PX = 21;
const DIVIDER_PX = 32.5;
// List `px-3`, bubble `max-w-[80%]` and `px-2.5`.
const LIST_PADDING_X_PX = 24;
const BUBBLE_MAX_FRACTION = 0.8;
const BUBBLE_PADDING_X_PX = 20;
// Average advance of a 12 px proportional UI font.
const AVG_CHAR_PX = 6.2;

/// Estimated rendered height of a row at a given list width, used before the
/// row has been measured. Text wraps per paragraph (`whitespace-pre-wrap`).
export function estimateRowHeight(row: TranscriptRow, listWidth: number): number {
  if (row.kind === "speaker") return SPEAKER_HEADER_PX;
  if (row.kind === "divider") return DIVIDER_PX;
  const textWidth =
    (listWidth - LIST_PADDING_X_PX) * BUBBLE_MAX_FRACTION - BUBBLE_PADDING_X_PX;
  const charsPerLine = Math.max(1, Math.floor(textWidth / AVG_CHAR_PX));
  let lines = 0;
  for (const paragraph of row.segment.text.trim().split("\n")) {
    lines += Math.max(1, Math.ceil(paragraph.length / charsPerLine));
  }
  return SEGMENT_CHROME_PX + SEGMENT_LINE_PX * lines;
}
