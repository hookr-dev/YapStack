import type { CitationToken, ParsedCitation } from "./types";

// Grammar for one marker, case-insensitive:
//  - bracketed and closed: `[[seg:ID]]`, `[seg:ID m:ss]`, `(seg:ID)`,
//    `[[seg:"ID"]]`, `[[seg:ID?]]`, `[[seg:A, seg:B]]`, mismatched
//    `[[seg:ID]`. The body runs to the first closing bracket and may hold
//    hedges or junk (`2cb? nope`).
//  - bracketed but unclosed: `[[seg:ID`. The body is limited to ID-ish
//    characters so an unterminated marker can't swallow a sentence.
//  - bare: `seg_ID` / `seg:ID]]`, only with no space after the marker and an
//    ID of 6+ hex characters, so prose like "seg: 12" never matches.
// `<`/`>` are excluded so a marker never spans HTML tags.
const MARKER = String.raw`\b(seg(?:ment)?)\s*[:_]`;
const OPEN = String.raw`(?:\[\[|\[|\()`;
const TIME_SRC = String.raw`\d{1,2}(?::\d{2}){1,2}\b`;
const CLOSED = String.raw`${OPEN}\s*${MARKER}([^\[\]()<>\n]{0,200}?)\s*(?:\]\]|\]|\))`;
const UNCLOSED = String.raw`${OPEN}\s*${MARKER}\s*([A-Za-z0-9_\-.…?:"'\x60]*(?:\s+${TIME_SRC})?)`;
const BARE = String.raw`\bseg[:_]([0-9a-f][0-9a-f-]{5,})(?![\w-])(?:\]{1,2})?`;
const MARKER_REGEX = new RegExp(`${CLOSED}|${UNCLOSED}|${BARE}`, "gi");

const TIME = /^(?:(\d{1,2}):)?(\d{1,2}):(\d{2})$/;
const TIME_IN_TEXT = /(?:^|\s)((?:\d{1,2}:)?\d{1,2}:\d{2})(?=\s|$)/;
const ITEM_SPLIT = /[,;]|\s+and\s+/i;
const ITEM_MARKER = /^\s*seg(?:ment)?\s*[:_]\s*/i;
const QUOTES = /["'\x60\u2018\u2019\u201C\u201D]/g;
const ID_HEAD = /^[A-Za-z0-9_-]+/;
const ID_LIKE = /^[0-9a-f][0-9a-f-]{5,}/i;
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
  const item = raw.replace(ITEM_MARKER, "").replace(QUOTES, "").trim();
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
    const marker = m[1] ?? m[3] ?? "seg";
    const body = m[2] ?? m[4] ?? m[5] ?? "";
    // "segment" is an ordinary word ("[segment: 3 of the talk]"); only treat
    // it as a marker when an ID or a timestamp follows.
    if (marker.length > 3) {
      const value = body.replace(QUOTES, "").trim();
      if (!ID_LIKE.test(value) && parseTimestamp(value) === null) continue;
    }

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
  String.raw`(?:${OPEN}\s*(?:s(?:e(?:g(?:m(?:e(?:n(?:t)?)?)?)?)?)?)?(?:\s*[:_][^\[\]()<>\n]{0,200})?|\bseg[:_][0-9a-f-]{0,60})$`,
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
