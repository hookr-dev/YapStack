import { describe, it, expect, vi } from "vitest";

// Same Tauri-import-chain mocks as ai-tools.test.ts.
vi.mock("@/lib/db", () => ({ listFolders: vi.fn().mockResolvedValue([]) }));
vi.mock("@/lib/folder-tree", () => ({
  findBranchConflicts: vi.fn().mockReturnValue([]),
  getFolderPath: vi.fn().mockReturnValue([]),
  buildFolderTree: vi.fn().mockReturnValue([]),
}));

import { convertCitationsToSegmentRefs } from "../ai-tools";
import type { DbSegment } from "../db";

function seg(id: string, audio_offset_seconds: number): DbSegment {
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
  };
}

// Real segment IDs are 36-char UUIDs. These are shaped exactly like the
// malformed citations found in the owner's local notes/chat_messages tables.
const A = seg("89ea3e1d-6adb-45f5-b0e5-7ca605644c60", 127);
const B = seg("48e2014f-ac1c-4c13-bb83-f4c2799df9dc", 1025);
const SEGMENTS = [A, B];

/** Text the user actually sees: tags stripped, pill labels kept. */
function visibleText(html: string): string {
  return html.replace(/<[^>]+>/g, "");
}

function pills(html: string) {
  return [
    ...html.matchAll(
      /data-segment-id="([^"]*)" data-timestamp="([^"]*)" data-offset="([^"]*)"/g,
    ),
  ].map(([, id, ts, off]) => ({ id, ts, off }));
}

/** Contract: a citation becomes a working pill (known segment id, m:ss
 *  label) or an unlabelled pending pill (shown as an unavailable source, kept
 *  so it can resolve later). It never leaves a raw `seg:` code on screen and
 *  never produces a pill labelled with a hex fragment that seeks to 0. */
function expectNoDeadOrRawCitations(html: string) {
  expect(visibleText(html)).not.toMatch(/seg[:_]/i);
  for (const p of pills(html)) {
    if (p.ts === "") continue;
    expect(SEGMENTS.map((s) => s.id)).toContain(p.id);
    expect(p.ts).toMatch(/^\d+:\d{2}(:\d{2})?$/);
  }
}

describe("citation post-processing on real model output", () => {
  it.each([
    // 88 of 148 dead pills in the local DB: model shortened the UUID to its
    // first 8 chars. Unique prefix in-session, but rendered as label
    // "89ea3e1d" with data-offset 0.
    ["8-char prefix", "Hiring is costly [[seg:89ea3e1d]]."],
    // 29 of 40 full-length misses are within Hamming distance 4 of a real id:
    // the model mis-copied hex characters.
    ["mis-copied full UUID", "Hiring is costly [[seg:89ea3e1d-6adb-45f5-b0e5-7ca605644c6f]]."],
    // Two ids in one double-bracket group.
    ["two ids in one bracket", `Tooling [[seg:${A.id}], [seg:${B.id}]].`],
    // Model mirrors the transcript's input format `[seg:ID m:ss]`.
    ["mirrors input format", `Tooling [seg:${A.id} 2:07].`],
    // Model emits the timestamp where the id belongs.
    ["timestamp instead of id", "A nudge, not an alert [[seg:17:05]]."],
    // Model gives up mid-id.
    ["hedged partial id", "Follow-up during outreach [[seg:89ea3e?]]."],
    ["ellipsis partial id", "Status values [[seg:48e2...]]."],
    ["missing closing bracket", "Issues this week [[seg:48e2...]."],
  ])("%s", (_label, modelOutput) => {
    const html = convertCitationsToSegmentRefs(`<p>${modelOutput}</p>`, SEGMENTS);
    expectNoDeadOrRawCitations(html);
  });
});

describe("unresolved citations in notes are kept, not destroyed", () => {
  it("keeps the full id of a mis-copied UUID on a pending pill", () => {
    const miscopied = "89ea3e1d-6adb-45f5-b0e5-7ca605644c6f";
    const html = convertCitationsToSegmentRefs(
      `<p>x [[seg:${miscopied}]].</p>`,
      SEGMENTS,
    );
    expect(pills(html)).toEqual([{ id: miscopied, ts: "", off: "0" }]);
    expect(visibleText(html)).toBe("x .");
  });

  it("keeps a timestamp-only citation so it can resolve later", () => {
    const html = convertCitationsToSegmentRefs("<p>x [[seg:9:59]].</p>", SEGMENTS);
    expect(pills(html)).toEqual([{ id: "", ts: "", off: "599" }]);
  });
});
