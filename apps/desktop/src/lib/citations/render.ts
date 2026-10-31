import { formatTime } from "@/lib/utils";
import { parseCitations } from "./parse";
import { resolveCitation, type SegmentIndex } from "./resolve";
import type { TranscriptRef } from "./types";

/** The visible label for a reference: its start time as `m:ss` / `h:mm:ss`. */
export function toPlainLabel(ref: TranscriptRef): string {
  return formatTime(ref.offsetSeconds);
}

function escapeAttr(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

/**
 * The stored note form: the `span[data-segment-ref]` that the Tiptap
 * `segmentReference` node (`lib/tiptap-segment-ref.ts`) parses into a pill.
 */
export function toPillHtml(ref: TranscriptRef): string {
  const label = toPlainLabel(ref);
  return `<span data-segment-ref="" data-segment-id="${escapeAttr(ref.segmentId)}" data-timestamp="${label}" data-offset="${ref.offsetSeconds}">${label}</span>`;
}

export interface RewriteResult {
  text: string;
  /** Citations that resolved to nothing and were removed. */
  invalidCount: number;
}

/**
 * Replace every citation marker in `text` with `render(ref)` for each ref it
 * resolves to, joined by a space. Markers that resolve to nothing are
 * removed, along with the space before them, so the sentence still reads
 * cleanly. With no `index` (no transcript in scope) every marker is removed.
 */
export function rewriteCitations(
  text: string,
  index: SegmentIndex | null,
  render: (ref: TranscriptRef) => string,
): RewriteResult {
  const runs = parseCitations(text);
  if (runs.length === 0) return { text, invalidCount: 0 };

  let out = "";
  let cursor = 0;
  let invalidCount = 0;
  for (const run of runs) {
    const seen = new Set<string>();
    const rendered: string[] = [];
    for (const token of run.tokens) {
      const ref = index ? resolveCitation(token, index) : null;
      if (!ref) {
        invalidCount++;
        continue;
      }
      if (seen.has(ref.segmentId)) continue;
      seen.add(ref.segmentId);
      rendered.push(render(ref));
    }

    let before = text.slice(cursor, run.start);
    let end = run.end;
    if (rendered.length === 0) {
      const trimmed = before.replace(/[ \t]+$/, "");
      const lineStart = /(^|\n|>)$/.test(out + before);
      if (trimmed.length < before.length) {
        before = trimmed;
      } else if (lineStart) {
        const after = /^[ \t]+/.exec(text.slice(end));
        if (after) end += after[0].length;
      }
    }
    out += before + rendered.join(" ");
    cursor = end;
  }
  return { text: out + text.slice(cursor), invalidCount };
}
