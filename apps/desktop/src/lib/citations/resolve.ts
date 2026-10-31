import { isVisibleSegment } from "@/lib/ai";
import type { DbSegment } from "@/lib/db";
import type { CitationToken, TranscriptRef } from "./types";

/** Shortest truncated ID that may be repaired by unique-prefix lookup. */
export const MIN_PREFIX_LENGTH = 6;

/**
 * The segments a set of citations may point at: normally the transcript the
 * model was shown for one request. Build it once per render or conversion and
 * pass it to `resolveCitation`.
 */
export interface SegmentIndex {
  /** Non-deleted segments keyed by lowercased ID. */
  readonly byId: ReadonlyMap<string, DbSegment>;
  /** Visible segments (the ones a model could have seen), in input order. */
  readonly visible: readonly DbSegment[];
  /** The one session every segment belongs to, or `null` when mixed or empty. */
  readonly sessionId: string | null;
}

export function createSegmentIndex(segments: readonly DbSegment[]): SegmentIndex {
  const byId = new Map<string, DbSegment>();
  const sessions = new Set<string>();
  for (const s of segments) {
    if (s.deleted_at) continue;
    byId.set(s.id.toLowerCase(), s);
    sessions.add(s.session_id);
  }
  return {
    byId,
    visible: segments.filter(isVisibleSegment),
    sessionId: sessions.size === 1 ? [...sessions][0] : null,
  };
}

/** The indexed segment with this ID, if any. */
export function findSegment(
  index: SegmentIndex,
  segmentId: string,
): DbSegment | undefined {
  return index.byId.get(segmentId.toLowerCase());
}

function toRef(s: DbSegment): TranscriptRef {
  return {
    sessionId: s.session_id,
    segmentId: s.id,
    offsetSeconds: s.audio_offset_seconds,
  };
}

function byUniquePrefix(index: SegmentIndex, id: string): DbSegment | null {
  if (id.length < MIN_PREFIX_LENGTH) return null;
  const prefix = id.toLowerCase();
  let found: DbSegment | null = null;
  for (const s of index.visible) {
    if (!s.id.toLowerCase().startsWith(prefix)) continue;
    if (found) return null;
    found = s;
  }
  return found;
}

// Timestamps are written floored to the second, so compare against the
// floored start. A zero-length segment still owns its own second.
function byTime(index: SegmentIndex, atSeconds: number): DbSegment | null {
  if (index.sessionId === null) return null;
  let best: DbSegment | null = null;
  for (const s of index.visible) {
    const start = Math.floor(s.audio_offset_seconds);
    const end = s.audio_offset_seconds + Math.max(s.chunk_duration_seconds, 1);
    if (atSeconds < start || atSeconds >= end) continue;
    if (!best || s.audio_offset_seconds > best.audio_offset_seconds) best = s;
  }
  return best;
}

/**
 * Resolve one token, or return `null` when no segment matches with certainty.
 * Order: exact ID (case-insensitive); a truncated ID that is a unique prefix
 * of `MIN_PREFIX_LENGTH`+ characters; then the timestamp, matched to the
 * segment that spans it, only when the index holds a single session.
 *
 * IDs are never matched by edit distance: a mis-copied UUID resolves to
 * nothing, because a link to the wrong moment is worse than no link.
 */
export function resolveCitation(
  token: CitationToken,
  index: SegmentIndex,
): TranscriptRef | null {
  if (token.kind === "segment") {
    const segment =
      findSegment(index, token.segmentId) ??
      byUniquePrefix(index, token.segmentId) ??
      (token.atSeconds !== undefined ? byTime(index, token.atSeconds) : null);
    return segment ? toRef(segment) : null;
  }
  const segment = byTime(index, token.atSeconds);
  return segment ? toRef(segment) : null;
}
