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
 * later load once the segment exists (see `lib/citations/html.ts`). The
 * editor renders it as an unavailable source.
 */
export function toPendingPillHtml(token: CitationToken): string {
  const segmentId = token.kind === "segment" ? token.segmentId : "";
  return `<span data-segment-ref="" data-segment-id="${escapeAttr(segmentId)}" data-timestamp="" data-offset="${token.atSeconds ?? 0}"></span>`;
}

/** The token a pending pill (`toPendingPillHtml`) was written from. */
export function pendingPillToken(
  segmentId: string,
  offsetSeconds: number,
): CitationToken {
  return segmentId
    ? { kind: "segment", segmentId }
    : { kind: "time", atSeconds: offsetSeconds };
}

/** A piece of text after citation resolution. */
export type CitationPart =
  | { kind: "text"; text: string }
  | { kind: "ref"; ref: TranscriptRef }
  | { kind: "unresolved"; token: CitationToken };

export interface SplitResult {
  parts: CitationPart[];
  /** Citations that didn't resolve (removed, or kept as `unresolved`). */
  invalidCount: number;
}

// A token that still carries what the model wrote: an ID, or a timestamp.
function isKeepable(token: CitationToken): boolean {
  return token.kind === "time" || token.segmentId !== "";
}

/**
 * Split `text` around its citation markers, resolving each against `index`
 * (nothing resolves when `index` is `null`). Refs from one run are separated
 * by a single-space text part. Unresolved tokens become `unresolved` parts
 * when `keepUnresolved` is set (paths that persist the result must keep
 * them, so a citation whose segment hasn't synced is never destroyed);
 * otherwise they are removed, along with the space before them, so the
 * sentence still reads cleanly.
 */
export function splitCitations(
  text: string,
  index: SegmentIndex | null,
  { keepUnresolved }: { keepUnresolved: boolean },
): SplitResult {
  const runs = parseCitations(text);
  if (runs.length === 0) return { parts: [{ kind: "text", text }], invalidCount: 0 };

  const parts: CitationPart[] = [];
  let emitted = "";
  let cursor = 0;
  let invalidCount = 0;
  const pushText = (t: string) => {
    if (!t) return;
    const last = parts[parts.length - 1];
    if (last?.kind === "text") last.text += t;
    else parts.push({ kind: "text", text: t });
    emitted += t;
  };

  for (const run of runs) {
    const seen = new Set<string>();
    const rendered: CitationPart[] = [];
    for (const token of run.tokens) {
      const ref = index ? resolveCitation(token, index) : null;
      if (!ref) {
        invalidCount++;
        if (keepUnresolved && isKeepable(token)) {
          rendered.push({ kind: "unresolved", token });
        }
        continue;
      }
      if (seen.has(ref.segmentId)) continue;
      seen.add(ref.segmentId);
      rendered.push({ kind: "ref", ref });
    }

    let before = text.slice(cursor, run.start);
    let end = run.end;
    if (rendered.length === 0) {
      const trimmed = before.replace(/[ \t]+$/, "");
      const lineStart = /(^|\n)$/.test(emitted + before);
      if (trimmed.length < before.length) {
        before = trimmed;
      } else if (lineStart) {
        const after = /^[ \t]+/.exec(text.slice(end));
        if (after) end += after[0].length;
      }
    }
    pushText(before);
    rendered.forEach((part, i) => {
      if (i > 0) pushText(" ");
      parts.push(part);
      emitted += "\u0000";
    });
    cursor = end;
  }
  pushText(text.slice(cursor));
  return { parts, invalidCount };
}

export interface RewriteResult {
  text: string;
  /** Citations that didn't resolve (removed, or kept via `renderUnresolved`). */
  invalidCount: number;
}

/**
 * String form of `splitCitations`: each resolved ref becomes `render(ref)`,
 * each unresolved token `renderUnresolved(token)` when given (else removed).
 * Only for plain text; note HTML goes through `lib/citations/html.ts` and
 * chat markdown through its AST, so markers in code or attributes are never
 * rewritten.
 */
export function rewriteCitations(
  text: string,
  index: SegmentIndex | null,
  render: (ref: TranscriptRef) => string,
  renderUnresolved?: (token: CitationToken) => string,
): RewriteResult {
  const { parts, invalidCount } = splitCitations(text, index, {
    keepUnresolved: renderUnresolved !== undefined,
  });
  const out = parts
    .map((p) =>
      p.kind === "text"
        ? p.text
        : p.kind === "ref"
          ? render(p.ref)
          : renderUnresolved!(p.token),
    )
    .join("");
  return { text: out, invalidCount };
}
