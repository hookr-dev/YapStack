import { describe, it, expect } from "vitest";
import { computeAnchorAdjustment } from "./useScrollAnchor";

describe("computeAnchorAdjustment", () => {
  it("prepend while NOT pinned keeps visible content fixed (scrollTop += height delta)", () => {
    // Older backfill segments land at the top: scrollHeight grows by 300, the
    // user is reading 500px down. To keep the same rows under the viewport we
    // must push scrollTop down by the same 300.
    const result = computeAnchorAdjustment({
      prevScrollHeight: 1000,
      prevScrollTop: 500,
      nextScrollHeight: 1300,
      clientHeight: 400,
      pinnedToBottom: false,
      grewAtTop: true,
    });
    expect(result.scrollTop).toBe(800);
    expect(result.changed).toBe(true);
  });

  it("append while pinned rides the bottom (live tail)", () => {
    const result = computeAnchorAdjustment({
      prevScrollHeight: 1000,
      prevScrollTop: 600, // 1000 - 400 = bottom
      nextScrollHeight: 1200,
      clientHeight: 400,
      pinnedToBottom: true,
      grewAtTop: false,
    });
    // New bottom is 1200 - 400 = 800.
    expect(result.scrollTop).toBe(800);
    expect(result.changed).toBe(true);
  });

  it("append (growth below the fold) while NOT pinned does not move a scrolled-up reader", () => {
    // Regression guard: a live segment appends at the BOTTOM, so scrollHeight
    // grows, but the rows the user is reading (above) did not move. grewAtTop is
    // false, so the viewport must stay exactly put — not get pushed down by the
    // appended row's height. (This is the bug the prepend/append split fixed.)
    const result = computeAnchorAdjustment({
      prevScrollHeight: 1000,
      prevScrollTop: 200,
      nextScrollHeight: 1200, // grew by 200, but at the bottom
      clientHeight: 400,
      pinnedToBottom: false,
      grewAtTop: false,
    });
    expect(result.scrollTop).toBe(200);
    expect(result.changed).toBe(false);
  });

  it("no-op when height is unchanged", () => {
    const result = computeAnchorAdjustment({
      prevScrollHeight: 1000,
      prevScrollTop: 350,
      nextScrollHeight: 1000,
      clientHeight: 400,
      pinnedToBottom: false,
      grewAtTop: false,
    });
    expect(result.scrollTop).toBe(350);
    expect(result.changed).toBe(false);
  });

  it("no-op when height shrinks (deletion) while not pinned", () => {
    const result = computeAnchorAdjustment({
      prevScrollHeight: 1000,
      prevScrollTop: 350,
      nextScrollHeight: 800,
      clientHeight: 400,
      pinnedToBottom: false,
      grewAtTop: false,
    });
    // Clamp keeps it within the (now smaller) range; no upward yank.
    expect(result.scrollTop).toBe(350);
    expect(result.changed).toBe(false);
  });

  it("clamps the compensated scrollTop to the bottom of the new range", () => {
    // Prepend delta would push scrollTop past the new max; clamp to the bottom.
    const result = computeAnchorAdjustment({
      prevScrollHeight: 1000,
      prevScrollTop: 580,
      nextScrollHeight: 1100,
      clientHeight: 400,
      pinnedToBottom: false,
      grewAtTop: true,
    });
    // compensated = 580 + 100 = 680, but max = 1100 - 400 = 700, so 680 is fine.
    expect(result.scrollTop).toBe(680);

    // Now push past the ceiling.
    const clamped = computeAnchorAdjustment({
      prevScrollHeight: 1000,
      prevScrollTop: 690,
      nextScrollHeight: 1100,
      clientHeight: 400,
      pinnedToBottom: false,
      grewAtTop: true,
    });
    // compensated = 790, max = 700 -> clamped to 700.
    expect(clamped.scrollTop).toBe(700);
  });

  it("never returns a negative scrollTop", () => {
    const result = computeAnchorAdjustment({
      prevScrollHeight: 100,
      prevScrollTop: -50, // degenerate input
      nextScrollHeight: 100,
      clientHeight: 400, // content shorter than viewport -> max 0
      pinnedToBottom: false,
      grewAtTop: false,
    });
    expect(result.scrollTop).toBe(0);
  });

  it("pinned-to-bottom with no growth is a no-op (already at bottom)", () => {
    const result = computeAnchorAdjustment({
      prevScrollHeight: 1000,
      prevScrollTop: 600, // already at bottom (1000 - 400)
      nextScrollHeight: 1000,
      clientHeight: 400,
      pinnedToBottom: true,
      grewAtTop: false,
    });
    expect(result.scrollTop).toBe(600);
    expect(result.changed).toBe(false);
  });

  it("pinned bottom is clamped to 0 when content is shorter than the viewport", () => {
    const result = computeAnchorAdjustment({
      prevScrollHeight: 200,
      prevScrollTop: 0,
      nextScrollHeight: 300,
      clientHeight: 400, // taller than content
      pinnedToBottom: true,
      grewAtTop: true,
    });
    expect(result.scrollTop).toBe(0);
    expect(result.changed).toBe(false);
  });
});
