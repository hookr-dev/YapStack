import type { CitationToken, ParsedCitation } from "./types";

// Grammar for one marker, case-insensitive:
//  - bracketed and closed: `[[seg:ID]]`, `[seg:ID m:ss]`, `(seg:ID)`,
//    `[[seg:ID?]]`, `[[seg:A, seg:B]]`, mismatched `[[seg:ID]`. The body runs
//    to the first closing bracket and may hold hedges or junk (`2cb? nope`).
//  - unclosed or unbracketed: `[[seg:ID` / `seg_ID]]`. The body is limited to
//    ID-ish characters so an unterminated marker can't swallow a sentence.
// `<`/`>` are excluded so a marker never spans HTML tags.
const MARKER = String.raw`\b(seg(?:ment)?)\s*[:_]`;
const OPEN = String.raw`(?:\[\[|\[|\()`;
const CLOSED = String.raw`${OPEN}\s*${MARKER}([^\[\]()<>\n]{0,200}?)\s*(?:\]\]|\]|\))`;
const UNCLOSED = String.raw`(?:${OPEN}\s*)?${MARKER}\s*([A-Za-z0-9_\-.…?:]*(?:\s+\d{1,2}(?::\d{2}){1,2}\b)?)(?:\s*\]{1,2})?`;
const MARKER_REGEX = new RegExp(`${CLOSED}|${UNCLOSED}`, "gi");

const TIME = /^(?:(\d{1,2}):)?(\d{1,2}):(\d{2})$/;
const TIME_IN_TEXT = /(?:^|\s)((?:\d{1,2}:)?\d{1,2}:\d{2})(?=\s|$)/;
const ITEM_SPLIT = /[,;]|\s+and\s+/i;
const ITEM_MARKER = /^\s*seg(?:ment)?\s*[:_]\s*/i;
const ID_HEAD = /^[A-Za-z0-9_-]+/;
const RUN_GAP = /^[\s,;]*$/;

/** Parse `m:ss` or `h:mm:ss` into seconds, or `null`. */
export function parseTimestamp(text: string): number | null {
  const m = TIME.exec(text.trim());
  if (!m) return null;
  const [, h, mm, ss] = m;
  if (Number(ss) > 59) return null;
  return Number(h ?? 0) * 3600 + Number(mm) * 60 + Number(ss);
}

function parseItem(raw: string): CitationToken {
  const item = raw.replace(ITEM_MARKER, "").trim();
  const asTime = parseTimestamp(item);
  if (asTime !== null) return { kind: "time", atSeconds: asTime };
  const segmentId = ID_HEAD.exec(item)?.[0] ?? "";
  const rest = item.slice(segmentId.length);
  const timeText = TIME_IN_TEXT.exec(rest)?.[1];
  const atSeconds = timeText ? parseTimestamp(timeText) : null;
  return atSeconds === null
    ? { kind: "segment", segmentId }
    : { kind: "segment", segmentId, atSeconds };
}

/**
 * Find every citation marker in `text`, in order. Tolerates the shapes seen
 * in real model output: truncated or hedged IDs, a timestamp in place of the
 * ID, several IDs in one bracket, the input line shape `[seg:ID m:ss]`,
 * missing or extra brackets, and case or spacing variants. Bare `[m:ss]` is
 * not treated as a citation.
 */
export function parseCitations(text: string): ParsedCitation[] {
  const runs: ParsedCitation[] = [];
  for (const m of text.matchAll(MARKER_REGEX)) {
    const marker = m[1] ?? m[3];
    const body = m[2] ?? m[4] ?? "";
    // "segment", and "seg:" outside brackets, can be ordinary prose; only
    // treat them as markers when an ID or timestamp follows.
    const bracketed = m[0][0] === "[" || m[0][0] === "(";
    if ((marker.length > 3 || !bracketed) && !/\d/.test(body)) continue;

    const start = m.index;
    const end = start + m[0].length;
    const tokens = body.split(ITEM_SPLIT).map(parseItem);
    const prev = runs[runs.length - 1];
    if (prev && RUN_GAP.test(text.slice(prev.end, start))) {
      prev.end = end;
      prev.tokens.push(...tokens);
    } else {
      runs.push({ start, end, tokens });
    }
  }
  return runs;
}

// A trailing marker still being typed: `[`, `[[se`, `(seg:89ea`, `seg_4`.
const PARTIAL_TAIL = new RegExp(
  String.raw`(?:${OPEN}\s*(?:s(?:e(?:g(?:m(?:e(?:n(?:t)?)?)?)?)?)?)?(?:\s*[:_][^\[\]()<>\n]{0,200})?|\bseg(?:ment)?\s*[:_][A-Za-z0-9_\-.…?:]{0,60})$`,
  "i",
);

/**
 * For text that is still streaming: cut a trailing marker that hasn't closed
 * yet, so `[[seg:89ea` never flashes on screen before it resolves. Returns
 * `text` unchanged when it doesn't end mid-marker.
 */
export function trimPartialCitation(text: string): string {
  const m = PARTIAL_TAIL.exec(text);
  return m ? text.slice(0, m.index) : text;
}
