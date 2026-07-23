import { useLayoutEffect, useRef, type RefObject } from "react";

// Module-level so positions survive component remounts. In-memory only —
// never persisted.
const positions = new Map<string, number>();

/** Exported for unit testing without DOM geometry. */
export const scrollPositionStore = {
  set(key: string, top: number): void {
    if (!Number.isFinite(top) || top <= 0) {
      positions.delete(key);
      return;
    }
    positions.set(key, Math.floor(top));
  },
  get(key: string): number {
    return positions.get(key) ?? 0;
  },
  /** Remembered position clamped to the current scrollable range. */
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

// The project's `ScrollArea` tags its viewport `data-slot`; Radix's internal
// `data-radix-scroll-area-viewport` is not present here.
const VIEWPORT_SELECTOR = '[data-slot="scroll-area-viewport"]';

/**
 * Remembers and restores a `ScrollArea`'s scroll position across remounts,
 * keyed by `key`. Returns a ref to attach to the ScrollArea root.
 */
export function useScrollPositionMemory<T extends HTMLElement>(
  key: string,
): RefObject<T | null> {
  const rootRef = useRef<T | null>(null);

  useLayoutEffect(() => {
    const viewport = rootRef.current?.querySelector<HTMLElement>(VIEWPORT_SELECTOR);
    if (!viewport) return;

    const maxScrollTop = viewport.scrollHeight - viewport.clientHeight;
    viewport.scrollTop = scrollPositionStore.resolve(key, maxScrollTop);

    const onScroll = () => scrollPositionStore.set(key, viewport.scrollTop);
    viewport.addEventListener("scroll", onScroll, { passive: true });

    return () => {
      viewport.removeEventListener("scroll", onScroll);
      // Capture the final position in case the listener missed the last move.
      scrollPositionStore.set(key, viewport.scrollTop);
    };
  }, [key]);

  return rootRef;
}
