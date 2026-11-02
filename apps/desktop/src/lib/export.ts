import type { DbSegment } from "@/lib/db";
import {
  resolveSpeakerLabel,
  speakerKeyOf,
  type SessionSpeakers,
  type SpeakerKey,
} from "@/lib/speakers";

type SpeakerSource = DbSegment["source"];

/** A run of consecutive same-speaker segments, merged into one turn. */
interface Turn {
  source: SpeakerSource;
  speakerKey: SpeakerKey;
  label: string;
  /** Offset of the first segment in the merged turn. */
  offsetSeconds: number;
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
function segmentsToTurns(segments: DbSegment[], speakers?: SessionSpeakers): Turn[] {
  const ordered = [...segments]
    .sort((a, b) => a.audio_offset_seconds - b.audio_offset_seconds)
    .map((s) => ({
      source: s.source,
      speakerKey: speakerKeyOf(s),
      label: resolveSpeakerLabel(s, speakers, "human"),
      offsetSeconds: s.audio_offset_seconds,
      text: s.text.trim(),
    }))
    .filter((s) => s.text.length > 0);

  const turns: Turn[] = [];
  for (const seg of ordered) {
    const last = turns[turns.length - 1];
    if (last && last.speakerKey === seg.speakerKey) {
      last.text = `${last.text} ${seg.text}`;
    } else {
      turns.push({ ...seg });
    }
  }
  return turns;
}

/**
 * Renders segments as attributed markdown: one merged turn per paragraph,
 * prefixed with the speaker's label (`**Me:**`, `**Them:**`, `**Speaker 2:**`,
 * a renamed speaker's name) and, with `includeTimestamps`, a `[mm:ss]` tag
 * from the turn's first offset.
 */
export function segmentsToAttributedMarkdown(
  segments: DbSegment[],
  {
    includeTimestamps = false,
    speakers,
  }: { includeTimestamps?: boolean; speakers?: SessionSpeakers } = {},
): string {
  return segmentsToTurns(segments, speakers)
    .map((turn) => {
      const label = `**${turn.label}:**`;
      const stamp = includeTimestamps ? ` [${formatTimestamp(turn.offsetSeconds)}]` : "";
      return `${label}${stamp} ${turn.text}`;
    })
    .join("\n\n");
}

/**
 * Returns only the given speaker's turns as plain text, one paragraph per
 * turn. Turn boundaries are computed on the full timeline first, so a turn by
 * the other speaker still yields a paragraph break here.
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
