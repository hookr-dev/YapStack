export { commands } from "./types";
export type * from "./types";

export type AudioSourceLabel = "Mic" | "System";

/**
 * Mirror of the Rust `TranscriptSegmentDto` (in
 * `commands/transcription.rs`). Specta only emits types that are reachable
 * from a registered command's args/return; since live segments arrive via
 * the `live-transcription-segment` event surface (not a command), we declare
 * the shape locally here to keep the FE event type self-contained.
 */
export type TranscriptSegmentDto = {
  start_ms: number;
  end_ms: number;
  text: string;
  confidence: number;
  /** Populated when diarization was requested for the originating
   *  transcribe call; otherwise `null`. */
  speaker_id: number | null;
};

/** Origin class of a live segment event. Mirrors the scheduler's priority
 *  tier (FinalFlush > Dictation > Live > Backfill). Strictly distinct from
 *  `source_kind` — `origin` is the priority class, `source_kind` is the
 *  routing identity. Frontend code must route by `source_kind`, never by
 *  `origin`. */
export type SegmentOrigin = "live" | "backfill" | "final_flush";

/** Routing identity of a live runtime — session vs dictation. The
 *  frontend filters every live-transcription event on this so a
 *  concurrently-running dictation never leaks segments / status into the
 *  session UI and vice-versa. Mirrors the Rust `LiveSourceKind` enum. */
export type LiveSourceKind = "session" | "dictation";

export type LiveSegmentEvent = {
  chunk_index: number;
  source: AudioSourceLabel;
  segments: TranscriptSegmentDto[];
  audio_offset_seconds: number;
  chunk_duration_seconds: number;
  accumulated_text: string;
  /** Origin class set by the scheduler at emit time. */
  origin: SegmentOrigin;
  /** Routing identity of the runtime that produced this segment. */
  source_kind: LiveSourceKind;
  /** Session this chunk belongs to. Late-arriving segments still persist to
   * this session even after the frontend has cleared activeSessionId. */
  session_id: string | null;
};
