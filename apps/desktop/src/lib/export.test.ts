import { describe, it, expect } from "vitest";
import type { DbSegment } from "@/lib/db";
import {
  formatTimestamp,
  segmentsForSpeaker,
  segmentsToAttributedMarkdown,
  segmentsToPlainText,
} from "./export";

/** Minimal DbSegment factory — only the fields the formatters read matter. */
function seg(
  source: DbSegment["source"],
  text: string,
  audio_offset_seconds: number,
): DbSegment {
  return {
    id: `${source}-${audio_offset_seconds}`,
    session_id: "s1",
    source,
    text,
    audio_offset_seconds,
    chunk_duration_seconds: 0,
    confidence: 1,
    created_at: "2025-06-15T00:00:00Z",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
  };
}

describe("formatTimestamp", () => {
  it("formats sub-minute offsets as mm:ss zero-padded", () => {
    expect(formatTimestamp(0)).toBe("00:00");
    expect(formatTimestamp(7)).toBe("00:07");
    expect(formatTimestamp(65)).toBe("01:05");
  });

  it("promotes to h:mm:ss at and past 60 minutes", () => {
    expect(formatTimestamp(3600)).toBe("1:00:00");
    expect(formatTimestamp(3725)).toBe("1:02:05");
  });

  it("floors fractional seconds and clamps negatives to zero", () => {
    expect(formatTimestamp(8.9)).toBe("00:08");
    expect(formatTimestamp(-5)).toBe("00:00");
  });
});

describe("segmentsToAttributedMarkdown", () => {
  it("labels Mic as **Me:** and System as **Them:**", () => {
    const md = segmentsToAttributedMarkdown([
      seg("System", "Thanks for hopping on.", 0),
      seg("Mic", "Happy to be here.", 5),
    ]);
    expect(md).toBe("**Them:** Thanks for hopping on.\n\n**Me:** Happy to be here.");
  });

  it("time-sorts before rendering regardless of input order", () => {
    const md = segmentsToAttributedMarkdown([
      seg("Mic", "second", 10),
      seg("System", "first", 0),
    ]);
    expect(md).toBe("**Them:** first\n\n**Me:** second");
  });

  it("merges consecutive same-speaker segments into one turn, joined by a space", () => {
    const md = segmentsToAttributedMarkdown([
      seg("Mic", "I did.", 0),
      seg("Mic", "The scope looks right.", 3),
      seg("System", "Great.", 6),
    ]);
    expect(md).toBe("**Me:** I did. The scope looks right.\n\n**Them:** Great.");
  });

  it("separates turns with a single blank line", () => {
    const md = segmentsToAttributedMarkdown([
      seg("Mic", "a", 0),
      seg("System", "b", 1),
    ]);
    expect(md.split("\n\n")).toHaveLength(2);
  });

  it("omits timestamps by default", () => {
    const md = segmentsToAttributedMarkdown([seg("Mic", "hi", 65)]);
    expect(md).toBe("**Me:** hi");
  });

  it("inserts [mm:ss] after the label when includeTimestamps is on", () => {
    const md = segmentsToAttributedMarkdown([seg("Mic", "hi", 187)], {
      includeTimestamps: true,
    });
    expect(md).toBe("**Me:** [03:07] hi");
  });

  it("promotes the timestamp to h:mm:ss past 60 minutes", () => {
    const md = segmentsToAttributedMarkdown([seg("System", "later", 3725)], {
      includeTimestamps: true,
    });
    expect(md).toBe("**Them:** [1:02:05] later");
  });

  it("uses the merged turn's FIRST offset for the timestamp", () => {
    const md = segmentsToAttributedMarkdown(
      [seg("Mic", "one", 8), seg("Mic", "two", 42)],
      { includeTimestamps: true },
    );
    expect(md).toBe("**Me:** [00:08] one two");
  });

  it("returns an empty string for empty input", () => {
    expect(segmentsToAttributedMarkdown([])).toBe("");
  });

  it("drops empty / whitespace-only segments", () => {
    const md = segmentsToAttributedMarkdown([
      seg("Mic", "   ", 0),
      seg("Mic", "real", 1),
    ]);
    expect(md).toBe("**Me:** real");
  });
});

describe("segmentsForSpeaker", () => {
  const convo = [
    seg("System", "Thanks for hopping on.", 0),
    seg("Mic", "I did.", 8),
    seg("Mic", "Push the timeline a week.", 12),
    seg("System", "That works.", 15),
  ];

  it("returns only that speaker's plain text with no labels or timestamps", () => {
    expect(segmentsForSpeaker(convo, "Mic")).toBe(
      "I did. Push the timeline a week.",
    );
    expect(segmentsForSpeaker(convo, "System")).toBe(
      "Thanks for hopping on.\n\nThat works.",
    );
  });

  it("merges consecutive same-speaker turns but separates non-adjacent ones with a blank line", () => {
    const out = segmentsForSpeaker(
      [
        seg("System", "alpha", 0),
        seg("System", "beta", 2),
        seg("Mic", "gap", 4),
        seg("System", "gamma", 6),
      ],
      "System",
    );
    expect(out).toBe("alpha beta\n\ngamma");
  });

  it("contains no markdown labels or timestamp brackets", () => {
    const out = segmentsForSpeaker(convo, "Mic");
    expect(out).not.toMatch(/\*\*/);
    expect(out).not.toMatch(/\[\d/);
  });

  it("returns an empty string when the speaker has no segments", () => {
    expect(segmentsForSpeaker([seg("Mic", "solo", 0)], "System")).toBe("");
    expect(segmentsForSpeaker([], "Mic")).toBe("");
  });
});

describe("segmentsToPlainText (unchanged behavior)", () => {
  it("joins time-sorted segment text one per line with no labels", () => {
    expect(
      segmentsToPlainText([seg("Mic", "second", 5), seg("System", "first", 0)]),
    ).toBe("first\nsecond");
  });

  it("returns an empty string for empty input", () => {
    expect(segmentsToPlainText([])).toBe("");
  });
});
