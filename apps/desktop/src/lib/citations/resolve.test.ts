import { describe, it, expect } from "vitest";
import { createSegmentIndex, resolveCitation } from "./resolve";
import {
  rewriteCitations,
  splitCitations,
  toPendingPillHtml,
  toPillHtml,
  toPlainLabel,
} from "./render";
import { convertCitationsInHtml, hasUnconvertedCitations } from "./html";
import type { DbSegment } from "@/lib/db";

function seg(
  id: string,
  audio_offset_seconds: number,
  overrides?: Partial<DbSegment>,
): DbSegment {
  return {
    id,
    session_id: "s1",
    source: "Mic",
    text: "t",
    audio_offset_seconds,
    chunk_duration_seconds: 5,
    confidence: 0.9,
    created_at: "",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
    ...overrides,
  };
}

const A = seg("89ea3e1d-6adb-45f5-b0e5-7ca605644c60", 127.4);
const B = seg("48e2014f-ac1c-4c13-bb83-f4c2799df9dc", 1025);
const index = createSegmentIndex([A, B]);
const refA = { sessionId: "s1", segmentId: A.id, offsetSeconds: 127.4 };

describe("resolveCitation", () => {
  it("resolves an exact id, ignoring case", () => {
    expect(
      resolveCitation({ kind: "segment", segmentId: A.id.toUpperCase() }, index),
    ).toEqual(refA);
  });

  it("repairs a unique prefix of 6+ characters", () => {
    expect(
      resolveCitation({ kind: "segment", segmentId: "89ea3e" }, index),
    ).toEqual(refA);
  });

  it("rejects a prefix shorter than 6 characters", () => {
    expect(
      resolveCitation({ kind: "segment", segmentId: "89ea3" }, index),
    ).toBeNull();
  });

  it("rejects an ambiguous prefix", () => {
    const twins = createSegmentIndex([
      seg("abcdef01-0000", 1),
      seg("abcdef02-0000", 2),
    ]);
    expect(
      resolveCitation({ kind: "segment", segmentId: "abcdef0" }, twins),
    ).toBeNull();
  });

  it("never matches a mis-copied full id by similarity", () => {
    const miscopied = A.id.slice(0, -1) + "f";
    expect(
      resolveCitation({ kind: "segment", segmentId: miscopied }, index),
    ).toBeNull();
  });

  it("does not repair a prefix to a hidden segment the model never saw", () => {
    const hidden = createSegmentIndex([seg("abcdef01", 1, { hidden: 1 })]);
    expect(
      resolveCitation({ kind: "segment", segmentId: "abcdef" }, hidden),
    ).toBeNull();
  });

  it("ignores deleted segments entirely", () => {
    const deleted = createSegmentIndex([seg("abc", 1, { deleted_at: "x" })]);
    expect(
      resolveCitation({ kind: "segment", segmentId: "abc" }, deleted),
    ).toBeNull();
  });

  it("resolves a timestamp to the segment that spans it", () => {
    expect(resolveCitation({ kind: "time", atSeconds: 127 }, index)).toEqual(
      refA,
    );
    expect(resolveCitation({ kind: "time", atSeconds: 131 }, index)).toEqual(
      refA,
    );
    expect(resolveCitation({ kind: "time", atSeconds: 133 }, index)).toBeNull();
  });

  it("never swaps an unresolvable id for the segment at its timestamp", () => {
    expect(
      resolveCitation(
        { kind: "segment", segmentId: "nope", atSeconds: 1026 },
        index,
      ),
    ).toBeNull();
  });

  it("repairs prefixes only within a single-session scope", () => {
    const mixed = createSegmentIndex([A, seg("other", 5, { session_id: "s2" })]);
    expect(
      resolveCitation({ kind: "segment", segmentId: "89ea3e" }, mixed),
    ).toBeNull();
    expect(
      resolveCitation({ kind: "segment", segmentId: A.id }, mixed)?.segmentId,
    ).toBe(A.id);
  });

  it("refuses time-only lookup across sessions", () => {
    const mixed = createSegmentIndex([A, seg("other", 127, { session_id: "s2" })]);
    expect(resolveCitation({ kind: "time", atSeconds: 127 }, mixed)).toBeNull();
  });
});

describe("render", () => {
  it("labels a ref with its start time", () => {
    expect(toPlainLabel({ ...refA, offsetSeconds: 4512 })).toBe("1:15:12");
  });

  it("builds the span the segmentReference node parses", () => {
    expect(toPillHtml(refA)).toBe(
      `<span data-segment-ref="" data-segment-id="${A.id}" data-timestamp="2:07" data-offset="127.4">2:07</span>`,
    );
  });

  it("rewrites resolved markers and drops the rest with their space", () => {
    const result = rewriteCitations(
      "One [[seg:89ea3e1d]]. Two [[seg:ffffff00]]. Three [[seg:17:05]].",
      index,
      toPlainLabel,
    );
    expect(result).toEqual({
      text: "One 2:07. Two. Three 17:05.",
      invalidCount: 1,
    });
  });

  it("strips the following space for a dropped marker at line start", () => {
    expect(
      rewriteCitations("[[seg:ffffff00]] Leading\n[[seg:ffffff00]] Next", index, toPlainLabel).text,
    ).toBe("Leading\nNext");
  });

  it("collapses duplicate refs within a run", () => {
    expect(
      rewriteCitations(
        `x [[seg:${A.id}], [seg:89ea3e1d]]`,
        index,
        toPlainLabel,
      ).text,
    ).toBe("x 2:07");
  });

  it("removes every marker when no index is in scope", () => {
    expect(
      rewriteCitations(`Fact [[seg:${A.id}]].`, null, toPlainLabel),
    ).toEqual({ text: "Fact.", invalidCount: 1 });
  });
});

describe("pending pills", () => {
  it("re-resolves a pending pill and leaves labelled pills untouched", () => {
    const pending = toPendingPillHtml({ kind: "segment", segmentId: A.id });
    const dead = `<span data-segment-ref="" data-segment-id="deadbeef" data-timestamp="deadbeef" data-offset="0">deadbeef</span>`;
    const html = `<p>${pending} ${dead}</p>`;
    expect(hasUnconvertedCitations(html)).toBe(true);
    expect(convertCitationsInHtml(html, index)).toBe(
      `<p>${toPillHtml(refA)} ${dead}</p>`,
    );
  });

  it("leaves a pending pill as-is while its segment is still missing", () => {
    const pending = toPendingPillHtml({ kind: "segment", segmentId: "late" });
    expect(convertCitationsInHtml(pending, index)).toBe(pending);
  });

  it("re-resolves a pending time-only pill", () => {
    const pending = toPendingPillHtml({ kind: "time", atSeconds: 1026 });
    expect(convertCitationsInHtml(pending, index)).toContain(
      `data-segment-id="${B.id}"`,
    );
  });

  it("never keeps a token with an empty id", () => {
    expect(
      splitCitations("x [[seg:]] y", index, { keepUnresolved: true }).parts,
    ).toEqual([{ kind: "text", text: "x y" }]);
  });

  it.each([
    ["no id-shaped value", "See [seg: intro] here", "See [seg: intro] here"],
    ["a word id", "See [[seg:overview]] here", "See [[seg:overview]] here"],
  ])("keeps %s as literal text", (_label, html, expected) => {
    expect(convertCitationsInHtml(`<p>${html}</p>`, index)).toBe(`<p>${expected}</p>`);
    expect(rewriteCitations(html, index, toPlainLabel).text).toBe(expected);
  });

  it("keeps a literal token and the prose around it in a mixed run", () => {
    const html = `<p>See [seg: intro], [[seg:${A.id}]].</p>`;
    expect(convertCitationsInHtml(html, index)).toBe(
      `<p>See [seg: intro], ${toPillHtml(refA)}.</p>`,
    );
    expect(
      rewriteCitations(`[[seg:${A.id}]], [seg: intro] and more`, index, toPlainLabel).text,
    ).toBe("2:07, [seg: intro] and more");
  });

  it.each(["abcd", "abcde", "2cb"])(
    "drops the short hex fragment %s",
    (frag) => {
      expect(convertCitationsInHtml(`<p>x [[seg:${frag}]].</p>`, index)).toBe(
        "<p>x.</p>",
      );
    },
  );

  it("leaves a marker split by formatting literal, never half-dropped", () => {
    const html = `<p>x [[seg:<code>${A.id}</code>]] y</p>`;
    expect(convertCitationsInHtml(html, index)).toBe(html);
  });

  // An id whose prefix uniquely matches B, but which is not B.
  const OTHER = "48e2014f-0000-4000-8000-000000000000";

  it.each([
    ["a closing </strong>", `x <strong>[[seg:</strong>${A.id}]] y`],
    ["a closing </em>", `x <em>[[seg:</em>${A.id}]] y`],
    ["nested formatting", `x <strong><em>[[seg:89ea3e</em></strong>1d-6adb-45f5-b0e5-7ca605644c60]] y`],
    ["a prefix that matches another segment", `x <strong>[[seg:48e2014f</strong>${OTHER.slice(8)}]] y`],
    ["an opening bracket", `x <strong>[[</strong>seg:${A.id}]] y`],
    ["a link", `x <a href="https://x.test">[[seg:</a>${A.id}]] y`],
    ["a link holding the id", `x [[seg:<a href="https://x.test">${A.id}</a>]] y`],
  ])("keeps a marker split across %s whole and literal", (_label, body) => {
    const html = `<p>${body}</p>`;
    expect(convertCitationsInHtml(html, index)).toBe(html);
  });

  it("still converts whole markers on either side of formatting", () => {
    const html = `<p><strong>[[seg:${A.id}]]</strong>, [[seg:${B.id}]]</p>`;
    expect(convertCitationsInHtml(html, index)).toBe(
      `<p><strong>${toPillHtml(refA)}</strong>, ${toPillHtml({
        sessionId: "s1",
        segmentId: B.id,
        offsetSeconds: 1025,
      })}</p>`,
    );
  });

  it("drops a short hex fragment instead of keeping a pending pill", () => {
    expect(convertCitationsInHtml("<p>x [[seg:48e2...]].</p>", index)).toBe(
      "<p>x.</p>",
    );
  });

  it("leaves citations inside link text literal", () => {
    const html = `<p><a href="https://x.test">see [[seg:${A.id}]]</a></p>`;
    expect(convertCitationsInHtml(html, index)).toBe(html);
  });

  it("finds nothing to convert in plain notes", () => {
    expect(hasUnconvertedCitations("<p>segment 3 of the talk</p>")).toBe(false);
    expect(hasUnconvertedCitations(`<p>${toPillHtml(refA)}</p>`)).toBe(false);
  });
});
