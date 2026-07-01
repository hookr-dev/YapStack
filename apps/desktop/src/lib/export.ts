import type { DbSegment } from "@/lib/db";

/** Mic = the user ("Me"), System = the other party ("Them"). Decided upstream. */
type SpeakerSource = DbSegment["source"];

/** A run of consecutive same-speaker segments, merged into one turn. */
interface Turn {
  source: SpeakerSource;
  /** Offset of the FIRST segment in the merged turn (first-offset-wins). */
  offsetSeconds: number;
  /** The turn's text: member segments joined by a single space. */
  text: string;
}

/**
 * Formats an audio offset as a clock timestamp. `mm:ss` zero-padded, promoting
 * to `h:mm:ss` once the offset reaches 60 minutes. Negative inputs clamp to 0.
 */
export function formatTimestamp(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const hrs = Math.floor(total / 3600);
  const mins = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  if (hrs > 0) return `${hrs}:${pad(mins)}:${pad(secs)}`;
  return `${pad(mins)}:${pad(secs)}`;
}

/**
 * Time-sorts segments, drops empties, then merges consecutive same-speaker
 * segments into turns. The merged turn keeps the FIRST segment's offset.
 */
function segmentsToTurns(segments: DbSegment[]): Turn[] {
  const ordered = [...segments]
    .sort((a, b) => a.audio_offset_seconds - b.audio_offset_seconds)
    .map((s) => ({ source: s.source, offsetSeconds: s.audio_offset_seconds, text: s.text.trim() }))
    .filter((s) => s.text.length > 0);

  const turns: Turn[] = [];
  for (const seg of ordered) {
    const last = turns[turns.length - 1];
    if (last && last.source === seg.source) {
      last.text = `${last.text} ${seg.text}`;
    } else {
      turns.push({ source: seg.source, offsetSeconds: seg.offsetSeconds, text: seg.text });
    }
  }
  return turns;
}

function speakerLabel(source: SpeakerSource): string {
  return source === "Mic" ? "Me" : "Them";
}

/**
 * Renders segments as attributed markdown: one turn per paragraph, each prefixed
 * with a bold `**Me:**` / `**Them:**` label, consecutive same-speaker segments
 * merged. With `includeTimestamps`, a `[mm:ss]` (or `[h:mm:ss]`) tag derived from
 * the merged turn's first offset follows the label. Turns are separated by a
 * blank line. Returns "" for empty input.
 */
export function segmentsToAttributedMarkdown(
  segments: DbSegment[],
  { includeTimestamps = false }: { includeTimestamps?: boolean } = {},
): string {
  return segmentsToTurns(segments)
    .map((turn) => {
      const label = `**${speakerLabel(turn.source)}:**`;
      const stamp = includeTimestamps ? ` [${formatTimestamp(turn.offsetSeconds)}]` : "";
      return `${label}${stamp} ${turn.text}`;
    })
    .join("\n\n");
}

/**
 * Returns only the given speaker's turns as plain text — no labels, no
 * timestamps — time-ordered with consecutive same-speaker segments merged and
 * turns separated by a blank line. Turn boundaries are computed on the full
 * timeline first, so a turn break in the original conversation (a turn by the
 * other speaker in between) still yields a paragraph break here. Returns "" when
 * that speaker has no text.
 */
export function segmentsForSpeaker(segments: DbSegment[], source: SpeakerSource): string {
  return segmentsToTurns(segments)
    .filter((turn) => turn.source === source)
    .map((turn) => turn.text)
    .join("\n\n");
}

/** Concatenates segments as plain text, one per line, in time order. */
export function segmentsToPlainText(segments: DbSegment[]): string {
  return [...segments]
    .sort((a, b) => a.audio_offset_seconds - b.audio_offset_seconds)
    .map((s) => s.text.trim())
    .filter((t) => t.length > 0)
    .join("\n");
}
