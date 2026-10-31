import { describe, it, expect } from "vitest";
import { parseCitations, parseTimestamp, trimPartialCitation } from "./parse";
import type { CitationToken } from "./types";

const A = "89ea3e1d-6adb-45f5-b0e5-7ca605644c60";
const B = "48e2014f-ac1c-4c13-bb83-f4c2799df9dc";

function tokensOf(text: string): CitationToken[] {
  return parseCitations(text).flatMap((r) => r.tokens);
}

describe("parseCitations", () => {
  it.each<[string, string, CitationToken[]]>([
    ["canonical", `x [[seg:${A}]]`, [{ kind: "segment", segmentId: A }]],
    ["F1 truncated id", "x [[seg:89ea3e1d]]", [{ kind: "segment", segmentId: "89ea3e1d" }]],
    ["F4 hedged id", "x [[seg:89ea3e?]]", [{ kind: "segment", segmentId: "89ea3e" }]],
    ["F4 hedge with junk", "x [[seg:2cb? nope]]", [{ kind: "segment", segmentId: "2cb" }]],
    ["F4 ellipsis", "x [[seg:9a16...]]", [{ kind: "segment", segmentId: "9a16" }]],
    ["F5 timestamp as id", "x [[seg:17:05]]", [{ kind: "time", atSeconds: 1025 }]],
    ["F5 hour timestamp", "x [[seg:1:02:03]]", [{ kind: "time", atSeconds: 3723 }]],
    [
      "F6 two ids, two brackets",
      `x [[seg:${A}], [seg:${B}]]`,
      [
        { kind: "segment", segmentId: A },
        { kind: "segment", segmentId: B },
      ],
    ],
    [
      "F6 comma list in one bracket",
      `x [[seg:${A}, seg:${B}]]`,
      [
        { kind: "segment", segmentId: A },
        { kind: "segment", segmentId: B },
      ],
    ],
    ["F7 missing close", "x [[seg:53...] y", [{ kind: "segment", segmentId: "53" }]],
    ["F7 single open", `x [seg:${A}]] y`, [{ kind: "segment", segmentId: A }]],
    ["F7 unterminated", `x [[seg:${A} y`, [{ kind: "segment", segmentId: A }]],
    [
      "F8 input line shape",
      `x [seg:${A} 2:07]`,
      [{ kind: "segment", segmentId: A, atSeconds: 127 }],
    ],
    ["F9 upper case", `x [[SEG:${A}]]`, [{ kind: "segment", segmentId: A }]],
    ["F9 spaces", `x [[ seg : ${A} ]]`, [{ kind: "segment", segmentId: A }]],
    ["F9 underscore", `x seg_${A}`, [{ kind: "segment", segmentId: A }]],
    ["F9 segment: prefix", `x [segment:${A}]`, [{ kind: "segment", segmentId: A }]],
    ["F9 parentheses", `x (seg:${A})`, [{ kind: "segment", segmentId: A }]],
    ["quoted id", `x [[seg:"${A}"]]`, [{ kind: "segment", segmentId: A }]],
    ["single-quoted id", `x [[seg:'${A}']]`, [{ kind: "segment", segmentId: A }]],
    ["backticked id", "x [[seg:`" + A + "`]]", [{ kind: "segment", segmentId: A }]],
  ])("%s", (_label, text, expected) => {
    expect(tokensOf(text)).toEqual(expected);
  });

  it("merges adjacent markers into one run spanning both", () => {
    const text = `a [[seg:${A}]], [[seg:${B}]] b`;
    const runs = parseCitations(text);
    expect(runs).toHaveLength(1);
    expect(text.slice(runs[0].start, runs[0].end)).toBe(
      `[[seg:${A}]], [[seg:${B}]]`,
    );
  });

  it("keeps separate runs when prose sits between them", () => {
    expect(parseCitations(`[[seg:${A}]] and then [[seg:${B}]]`)).toHaveLength(2);
  });

  it.each([
    ["plain prose", "We talked about the segment: intro and outro."],
    ["seg: in prose", "That was a seg: fault in the plan."],
    ["bare timestamp", "At [2:07] we agreed."],
    ["word containing seg", "The myseg:12 value"],
    ["markdown link", "[docs](https://example.com)"],
    ["segment: with minutes", "The segment: 12 minutes long."],
    ["segment N of", "This was segment 3 of the talk."],
    ["bracketed segment prose", "See [segment: 3 of the talk]."],
    ["parenthesised segment", "(segment 2) covers pricing."],
    ["footnote number", "As noted [3] earlier."],
    ["clock time", "We meet at 12:30 pm."],
    ["bracketed clock time", "Agenda [12:30] lunch."],
    ["seg: with a small number", "Set seg: 12 in the config."],
    ["seg_ identifier", "Call seg_fault_handler() here."],
    ["seg_ with short number", "the seg_2 column"],
  ])("ignores %s", (_label, text) => {
    expect(parseCitations(text)).toEqual([]);
  });

  it("never spans HTML tags", () => {
    const runs = parseCitations("<p>a [[seg:abc123</p><p>next]] b</p>");
    expect(runs).toHaveLength(1);
    expect(runs[0].tokens).toEqual([{ kind: "segment", segmentId: "abc123" }]);
  });
});

describe("parseTimestamp", () => {
  it.each([
    ["0:07", 7],
    ["17:05", 1025],
    ["1:15:12", 4512],
    ["2:61", null],
    ["abc", null],
  ])("%s -> %s", (text, expected) => {
    expect(parseTimestamp(text)).toBe(expected);
  });
});

describe("trimPartialCitation", () => {
  it.each([
    ["Hiring is costly [", "Hiring is costly "],
    ["Hiring is costly [[se", "Hiring is costly "],
    ["Hiring is costly [[seg:89ea", "Hiring is costly "],
    ["Hiring is costly [seg:89ea3e1d 2:0", "Hiring is costly "],
    ["Hiring is costly seg_89ea", "Hiring is costly "],
  ])("holds back %j", (text, expected) => {
    expect(trimPartialCitation(text)).toBe(expected);
  });

  it.each([
    "Hiring is costly [[seg:89ea3e1d]].",
    "A list [of things] here",
    "See [docs](https://example.com)",
    "Plain text",
    "Starts a [note",
  ])("leaves %j alone", (text) => {
    expect(trimPartialCitation(text)).toBe(text);
  });
});
