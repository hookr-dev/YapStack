import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { createElement } from "react";
import {
  scrollPositionStore,
  useScrollPositionMemory,
} from "./useScrollPositionMemory";

describe("scrollPositionStore", () => {
  beforeEach(() => {
    scrollPositionStore.reset();
  });

  it("stores and reads a position per key", () => {
    scrollPositionStore.set("all", 240);
    expect(scrollPositionStore.get("all")).toBe(240);
  });

  it("returns 0 for an unknown key", () => {
    expect(scrollPositionStore.get("never-set")).toBe(0);
  });

  it("keeps positions for different keys independent", () => {
    scrollPositionStore.set("all", 100);
    scrollPositionStore.set("pinned", 500);
    scrollPositionStore.set("folder:abc", 50);

    expect(scrollPositionStore.get("all")).toBe(100);
    expect(scrollPositionStore.get("pinned")).toBe(500);
    expect(scrollPositionStore.get("folder:abc")).toBe(50);
  });

  it("floors fractional scrollTop values", () => {
    scrollPositionStore.set("all", 123.9);
    expect(scrollPositionStore.get("all")).toBe(123);
  });

  it("treats non-positive or non-finite values as no position", () => {
    scrollPositionStore.set("all", 300);
    scrollPositionStore.set("all", 0);
    expect(scrollPositionStore.get("all")).toBe(0);

    scrollPositionStore.set("all", 300);
    scrollPositionStore.set("all", -5);
    expect(scrollPositionStore.get("all")).toBe(0);

    scrollPositionStore.set("all", 300);
    scrollPositionStore.set("all", Number.NaN);
    expect(scrollPositionStore.get("all")).toBe(0);
  });

  it("clamps resolve() to the current scrollable range", () => {
    scrollPositionStore.set("all", 1000);
    // List is now shorter than when the position was captured.
    expect(scrollPositionStore.resolve("all", 400)).toBe(400);
    // List is tall enough to honor the full position.
    expect(scrollPositionStore.resolve("all", 5000)).toBe(1000);
  });

  it("resolve() never returns a negative value", () => {
    scrollPositionStore.set("all", 200);
    expect(scrollPositionStore.resolve("all", -100)).toBe(0);
    expect(scrollPositionStore.resolve("all", Number.NaN)).toBe(0);
  });

  it("resolve() yields 0 for a key with no remembered position", () => {
    expect(scrollPositionStore.resolve("fresh", 1000)).toBe(0);
  });

  it("clear() removes a single key without touching others", () => {
    scrollPositionStore.set("all", 100);
    scrollPositionStore.set("pinned", 200);
    scrollPositionStore.clear("all");

    expect(scrollPositionStore.get("all")).toBe(0);
    expect(scrollPositionStore.get("pinned")).toBe(200);
  });

  it("reset() empties every remembered position", () => {
    scrollPositionStore.set("all", 100);
    scrollPositionStore.set("pinned", 200);
    scrollPositionStore.reset();

    expect(scrollPositionStore.get("all")).toBe(0);
    expect(scrollPositionStore.get("pinned")).toBe(0);
  });
});

// Renders the hook over a viewport tagged like the project's ScrollArea
// (`data-slot="scroll-area-viewport"`). These fail if VIEWPORT_SELECTOR doesn't
// match the real viewport — i.e. they guard against the hook being inert.
function Harness({ keyName }: { keyName: string }) {
  const ref = useScrollPositionMemory<HTMLDivElement>(keyName);
  return createElement(
    "div",
    { ref },
    createElement(
      "div",
      { "data-slot": "scroll-area-viewport", "data-testid": "viewport" },
      "content",
    ),
  );
}

describe("useScrollPositionMemory (DOM wiring)", () => {
  beforeEach(() => scrollPositionStore.reset());
  afterEach(() => cleanup());

  it("finds the ScrollArea viewport and captures scroll position", () => {
    const { getByTestId } = render(createElement(Harness, { keyName: "all" }));
    const viewport = getByTestId("viewport");
    // jsdom has no layout; pin a scrollTop the scroll handler can read back.
    Object.defineProperty(viewport, "scrollTop", {
      configurable: true,
      value: 137,
    });
    viewport.dispatchEvent(new Event("scroll"));
    // If the selector didn't match the viewport, no listener attaches and this
    // stays 0 — exactly the inert-hook regression.
    expect(scrollPositionStore.get("all")).toBe(137);
  });

  it("captures the final position on unmount", () => {
    const { getByTestId, unmount } = render(
      createElement(Harness, { keyName: "pinned" }),
    );
    const viewport = getByTestId("viewport");
    Object.defineProperty(viewport, "scrollTop", {
      configurable: true,
      value: 64,
    });
    unmount();
    expect(scrollPositionStore.get("pinned")).toBe(64);
  });
});
