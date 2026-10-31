import { formatTime } from "@/lib/utils";
import { parseCitations } from "./parse";
import { resolveCitation, type SegmentIndex } from "./resolve";
import type { CitationToken, TranscriptRef } from "./types";

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

/**
 * The stored note form of a citation that didn't resolve: a
 * `span[data-segment-ref]` with an empty label that keeps what the model
 * wrote (the ID, and any timestamp as `data-offset`), so it can resolve on a
 * later load once the segment exists (see `resolvePendingPills`). The editor
 * renders it as an unavailable source. Returns `null` when the token holds
 * nothing worth keeping.
 */
export function toPendingPillHtml(token: CitationToken): string | null {
  const segmentId = token.kind === "segment" ? token.segmentId : "";
  const offset = token.atSeconds ?? 0;
  if (!segmentId && token.kind === "segment") return null;
  return `<span data-segment-ref="" data-segment-id="${escapeAttr(segmentId)}" data-timestamp="" data-offset="${offset}"></span>`;
}

const PENDING_PILL = /<span\b[^>]*\bdata-segment-ref\b[^>]*>[^<]*<\/span>/g;

function attr(tag: string, name: string): string | null {
  const m = new RegExp(`\\b${name}="([^"]*)"`).exec(tag);
  return m ? m[1] : null;
}

/** Whether `html` holds a pill written by `toPendingPillHtml`. */
export function hasPendingPills(html: string): boolean {
  return [...html.matchAll(PENDING_PILL)].some((m) => !attr(m[0], "data-timestamp"));
}

/**
 * Re-resolve pills written by `toPendingPillHtml` against `index`, replacing
 * each one that now resolves with `toPillHtml`. Pills that still don't
 * resolve, and every labelled pill, are left exactly as they are.
 */
export function resolvePendingPills(html: string, index: SegmentIndex): string {
  return html.replace(PENDING_PILL, (tag) => {
    if (attr(tag, "data-timestamp")) return tag;
    const segmentId = attr(tag, "data-segment-id") ?? "";
    const offset = Number(attr(tag, "data-offset") ?? "0");
    const token: CitationToken = segmentId
      ? { kind: "segment", segmentId, ...(offset > 0 ? { atSeconds: offset } : {}) }
      : { kind: "time", atSeconds: offset };
    const ref = resolveCitation(token, index);
    return ref ? toPillHtml(ref) : tag;
  });
}

export interface RewriteResult {
  text: string;
  /** Citations that didn't resolve (removed, or kept via `renderUnresolved`). */
  invalidCount: number;
}

/**
 * Replace every citation marker in `text` with `render(ref)` for each ref it
 * resolves to, joined by a space. A marker that resolves to nothing becomes
 * `renderUnresolved(token)` when given; otherwise it is removed, along with
 * the space before it, so the sentence still reads cleanly. With no `index`
 * (no transcript in scope) nothing resolves.
 *
 * Display paths drop unresolved markers. Paths that persist the result keep
 * them via `renderUnresolved`, so a citation whose segment hasn't synced yet
 * is never destroyed.
 */
export function rewriteCitations(
  text: string,
  index: SegmentIndex | null,
  render: (ref: TranscriptRef) => string,
  renderUnresolved?: (token: CitationToken) => string | null,
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
        const kept = renderUnresolved?.(token);
        if (kept) rendered.push(kept);
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
