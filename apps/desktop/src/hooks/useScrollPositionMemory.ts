import { useLayoutEffect, useRef, type RefObject } from "react";

/**
 * Module-level store so positions survive component remounts (e.g. when a view
 * is swapped out and back in via Zustand `currentView`, which fully unmounts the
 * subtree). Scroll position is ephemeral UI state — it lives in memory only and
 * is never persisted (see docs/PRINCIPLES.md "Don't persist speculative state").
 */
const positions = new Map<string, number>();

/** Pure position store, exported for unit testing without DOM geometry. */
export const scrollPositionStore = {
  /** Clamp to a non-negative integer so a stale/oversized value can't survive. */
  set(key: string, top: number): void {
    if (!Number.isFinite(top) || top <= 0) {
      positions.delete(key);
      return;
    }
    positions.set(key, Math.floor(top));
  },
  /** Returns the remembered position for `key`, or 0 if none. */
  get(key: string): number {
    return positions.get(key) ?? 0;
  },
  /**
   * Largest value safe to scroll to right now: never exceeds the current
   * scrollable range, so a remembered position from a taller list can't leave
   * the viewport stuck below its content.
   */
  resolve(key: string, maxScrollTop: number): number {
    const remembered = positions.get(key) ?? 0;
    const ceiling = Number.isFinite(maxScrollTop) ? Math.max(0, maxScrollTop) : 0;
    return Math.min(remembered, ceiling);
  },
  clear(key: string): void {
    positions.delete(key);
  },
  reset(): void {
    positions.clear();
  },
};

// The scrollable element is the project's `ScrollArea` viewport (components/ui/
// scroll-area.tsx), which tags it `data-slot="scroll-area-viewport"`. Matching
// Radix's internal `data-radix-scroll-area-viewport` would silently find
// nothing and make this hook inert.
const VIEWPORT_SELECTOR = '[data-slot="scroll-area-viewport"]';

/**
 * Remembers and restores the scroll position of a `ScrollArea` across remounts,
 * keyed by `key` (callers pass the active list filter so each filter keeps an
 * independent position).
 *
 * Returns a ref to attach to the ScrollArea root. The scrollable element is the
 * `[data-slot="scroll-area-viewport"]` node, found via `querySelector`.
 */
export function useScrollPositionMemory<T extends HTMLElement>(
  key: string,
): RefObject<T | null> {
  const rootRef = useRef<T | null>(null);

  useLayoutEffect(() => {
    // Snapshot the viewport for the lifetime of this effect: the root node is
    // stable while `key` is unchanged, so the cleanup writes back to the same
    // element it restored from (avoids reading a possibly-detached ref later).
    const viewport = rootRef.current?.querySelector<HTMLElement>(VIEWPORT_SELECTOR);
    if (!viewport) return;

    // Restore after layout so content exists; clamp to the current scroll range
    // so a remembered position from a taller list can't strand the viewport.
    const maxScrollTop = viewport.scrollHeight - viewport.clientHeight;
    viewport.scrollTop = scrollPositionStore.resolve(key, maxScrollTop);

    // Capture on scroll so the stored position stays current.
    const onScroll = () => scrollPositionStore.set(key, viewport.scrollTop);
    viewport.addEventListener("scroll", onScroll, { passive: true });

    return () => {
      viewport.removeEventListener("scroll", onScroll);
      // Final capture in case the listener missed the last move before unmount.
      scrollPositionStore.set(key, viewport.scrollTop);
    };
  }, [key]);

  return rootRef;
}
