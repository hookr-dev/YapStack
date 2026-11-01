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
    const word = m[1] ?? m[3] ?? "seg";
    const body = m[2] ?? m[4] ?? m[5] ?? "";
    // "segment" is an ordinary word ("[segment: 3 of the talk]"); only treat
    // it as a marker when an ID or a timestamp follows.
    if (word.length > 3) {
      const value = body.replace(QUOTES, "").trim();
      if (!ID_LIKE.test(value) && parseTimestamp(value) === null) continue;
    }

    const start = m.index;
    const end = start + m[0].length;
    const tokens = body.split(ITEM_SPLIT).map(parseItem);
    const marker = { start, end, tokens };
    const prev = runs[runs.length - 1];
    if (prev && RUN_GAP.test(text.slice(prev.end, start))) {
      prev.end = end;
      prev.tokens.push(...tokens);
      prev.markers.push(marker);
    } else {
      runs.push({ start, end, tokens: [...tokens], markers: [marker] });
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

/** A piece of text and whether it must be shown as-is, never parsed. */
export interface TextSpan {
  text: string;
  literal: boolean;
}

const OPENS = /^[[(]/;
const CLOSES = /[\])]$/;
const FULL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * Whether marker text found at the end of a line may have been cut off: a
 * bracketed marker missing its closer, or a bare ID shorter than a full
 * UUID (`seg:89ea3e1d` could be the start of one that continues in the next
 * block). A bare full UUID is complete.
 */
function isUnterminated(marker: string): boolean {
  if (CLOSES.test(marker)) return false;
  if (OPENS.test(marker)) return true;
  return !FULL_UUID.test(marker.replace(/^seg[:_]/i, ""));
}

/**
 * Split each of `pieces`, consecutive text in one line of inline content
 * (text nodes across formatting, code and links; a line ends at a block edge
 * or hard break), into spans, marking as literal any marker that can't be
 * trusted from its piece alone:
 *  - a marker that spans a boundary between pieces (`[[seg:` then
 *    `</strong>ID]]`): parsed from one piece, half of it would be dropped or
 *    misread, so the whole of it stays literal;
 *  - a marker the piece reads differently from the line as a whole, such as
 *    an unclosed marker the next piece continues;
 *  - a marker left unterminated at the line end (`[[seg:89ea3e1d`, or a
 *    bare `seg:89ea3e1d`), which may be cut off and must never be resolved
 *    by its prefix. A bare full UUID at the line end is complete.
 * Markers both readings agree on are left to the caller. Linear in the
 * length of the line.
 */
export function splitAtCrossingMarkers(pieces: readonly string[]): TextSpan[][] {
  const starts: number[] = [];
  let pos = 0;
  for (const piece of pieces) {
    starts.push(pos);
    pos += piece.length;
  }
  const lineEnd = pos;
  const joined = pieces.join("");
  // Literal ranges per piece, in the piece's own offsets.
  const literal: Array<Array<[number, number]>> = pieces.map(() => []);
  const agreed = new Set<string>();

  // Markers come back in order, so one pointer walks the piece boundaries.
  let p = 0;
  for (const run of parseCitations(joined)) {
    for (const { start, end } of run.markers) {
      while (p < pieces.length - 1 && starts[p + 1] <= start) p++;
      const crosses = p < pieces.length - 1 && starts[p + 1] < end;
      const unterminated = end === lineEnd && isUnterminated(joined.slice(start, end));
      if (!crosses && !unterminated) {
        agreed.add(`${start}:${end}`);
        continue;
      }
      for (let q = p; q < pieces.length && starts[q] < end; q++) {
        const from = Math.max(start, starts[q]) - starts[q];
        const to = Math.min(end, starts[q] + pieces[q].length) - starts[q];
        if (to > from) literal[q].push([from, to]);
      }
    }
  }

  return pieces.map((piece, i) => {
    const ranges = literal[i];
    for (const run of parseCitations(piece)) {
      for (const { start, end } of run.markers) {
        if (!agreed.has(`${starts[i] + start}:${starts[i] + end}`)) {
          ranges.push([start, end]);
        }
      }
    }
    ranges.sort((a, b) => a[0] - b[0]);
    const spans: TextSpan[] = [];
    const push = (from: number, to: number, isLiteral: boolean) => {
      if (to <= from) return;
      const text = piece.slice(from, to);
      const last = spans[spans.length - 1];
      if (last?.literal === isLiteral) last.text += text;
      else spans.push({ text, literal: isLiteral });
    };
    let cursor = 0;
    for (const [from, to] of ranges) {
      if (to <= cursor) continue;
      push(cursor, from, false);
      push(Math.max(cursor, from), to, true);
      cursor = to;
    }
    push(cursor, piece.length, false);
    return spans;
  });
}
