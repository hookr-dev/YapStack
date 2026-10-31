import { describe, it, expect } from "vitest";
import { createSegmentIndex, resolveCitation } from "./resolve";
import {
  hasPendingPills,
  resolvePendingPills,
  rewriteCitations,
  toPendingPillHtml,
  toPillHtml,
  toPlainLabel,
} from "./render";
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

  it("falls back to the timestamp beside an unresolvable id", () => {
    expect(
      resolveCitation(
        { kind: "segment", segmentId: "nope", atSeconds: 1026 },
        index,
      )?.segmentId,
    ).toBe(B.id);
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
      "One [[seg:89ea3e1d]]. Two [[seg:nope]]. Three [[seg:17:05]].",
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
      rewriteCitations("<p>[[seg:nope]] Leading</p>", index, toPlainLabel).text,
    ).toBe("<p>Leading</p>");
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
    const pending = toPendingPillHtml({ kind: "segment", segmentId: A.id })!;
    const dead = `<span data-segment-ref="" data-segment-id="deadbeef" data-timestamp="deadbeef" data-offset="0">deadbeef</span>`;
    const html = `<p>${pending} ${dead}</p>`;
    expect(hasPendingPills(html)).toBe(true);
    expect(resolvePendingPills(html, index)).toBe(
      `<p>${toPillHtml(refA)} ${dead}</p>`,
    );
  });

  it("leaves a pending pill as-is while its segment is still missing", () => {
    const pending = toPendingPillHtml({ kind: "segment", segmentId: "late" })!;
    expect(resolvePendingPills(pending, index)).toBe(pending);
  });

  it("keeps nothing for an empty id", () => {
    expect(toPendingPillHtml({ kind: "segment", segmentId: "" })).toBeNull();
  });
});
