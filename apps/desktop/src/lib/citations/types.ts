/**
 * Shared types for transcript citations: a reference from generated text (a
 * summary, a chat reply, a note) to one moment in a session's transcript.
 *
 * The pipeline is parse → resolve → render:
 *  - `parse.ts` finds citation markers in model output and yields unresolved
 *    `CitationToken`s, tolerating the shapes models actually produce.
 *  - `resolve.ts` turns a token into a `TranscriptRef` against a
 *    `SegmentIndex`, or `null` when nothing matches with certainty.
 *  - `render.ts` writes resolved refs back out in a caller-chosen form and
 *    drops unresolved markers, so a citation is either a working reference or
 *    absent — never a raw code or a dead link.
 */

/** A resolved citation: a segment that exists, and where it starts. */
export interface TranscriptRef {
  sessionId: string;
  segmentId: string;
  /** Segment start in seconds from the beginning of the session audio. */
  offsetSeconds: number;
}

/**
 * One citation as written by a model, before resolution.
 *  - `segment`: an ID, possibly truncated or mis-copied, with the `m:ss`
 *    timestamp the model put beside it when present.
 *  - `time`: a timestamp written where the ID belongs (`[[seg:17:05]]`).
 */
export type CitationToken =
  | { kind: "segment"; segmentId: string; atSeconds?: number }
  | { kind: "time"; atSeconds: number };

/** One marker (`[[seg:A]]`, or `[[seg:A, seg:B]]`) and the tokens it holds. */
export interface CitationMarker {
  start: number;
  end: number;
  tokens: CitationToken[];
}

/**
 * A run of citation markers found in a string. Adjacent markers separated
 * only by whitespace or commas (`[[seg:A]], [[seg:B]]`) form one run, so the
 * whole run can be replaced or removed together.
 */
export interface ParsedCitation {
  /** Index of the first character of the run in the input. */
  start: number;
  /** Index one past the last character of the run. */
  end: number;
  /** Every token in the run, in order (the union of `markers[].tokens`). */
  tokens: CitationToken[];
  markers: CitationMarker[];
}
