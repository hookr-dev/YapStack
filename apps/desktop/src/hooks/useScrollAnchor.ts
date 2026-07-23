import { useLayoutEffect, useRef } from "react";

/**
 * Scroll-anchoring for a list whose items can arrive at either end. Backfill
 * segments sort to the top and prepend mid-stream; the hook keeps visible
 * content fixed on prepend and follows the bottom only when the user is
 * pinned there and content was appended.
 */

export interface AnchorSnapshot {
  /** `scrollHeight` of the viewport before the DOM grew. */
  prevScrollHeight: number;
  /** `scrollTop` of the viewport before the DOM grew. */
  prevScrollTop: number;
  /** Whether the user was pinned to the bottom before the DOM grew. */
  pinnedToBottom: boolean;
}

export interface AnchorInputs extends AnchorSnapshot {
  /** `scrollHeight` of the viewport after the DOM grew. */
  nextScrollHeight: number;
  /** Viewport height — unchanged by content, used to compute the bottom. */
  clientHeight: number;
  /**
   * Whether the growth happened at the top (a prepend). Only a prepend needs
   * compensation; a bottom-append must not move the viewport.
   */
  grewAtTop: boolean;
}

export interface AnchorAdjustment {
  /** The `scrollTop` the viewport should hold after the update. */
  scrollTop: number;
  /** `false` means leave `scrollTop` alone. */
  changed: boolean;
}

/**
 * Pure decision/delta math for scroll anchoring (no DOM access).
 *
 * - Pinned to bottom (live tail) -> ride the bottom.
 * - Prepend while reading above the fold -> shift `scrollTop` by the height
 *   delta so the visible rows do not move.
 * - Otherwise -> leave `scrollTop` alone.
 *
 * Results are clamped to `[0, maxScrollTop]`.
 */
export function computeAnchorAdjustment({
  prevScrollHeight,
  prevScrollTop,
  nextScrollHeight,
  clientHeight,
  pinnedToBottom,
  grewAtTop,
}: AnchorInputs): AnchorAdjustment {
  const maxScrollTop = Math.max(0, nextScrollHeight - clientHeight);
  const clamp = (top: number) =>
    Math.min(Math.max(0, top), maxScrollTop);

  const heightDelta = nextScrollHeight - prevScrollHeight;

  if (pinnedToBottom) {
    return { scrollTop: maxScrollTop, changed: maxScrollTop !== prevScrollTop };
  }

  if (heightDelta <= 0 || !grewAtTop) {
    return { scrollTop: clamp(prevScrollTop), changed: false };
  }

  const compensated = prevScrollTop + heightDelta;
  const next = clamp(compensated);
  return { scrollTop: next, changed: next !== prevScrollTop };
}

/** Distance (px) from the bottom under which the viewport counts as "pinned". */
const PINNED_THRESHOLD_PX = 4;

function isPinnedToBottom(viewport: HTMLElement): boolean {
  const distFromBottom =
    viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight;
  return distFromBottom < PINNED_THRESHOLD_PX;
}

export interface UseScrollAnchorArgs {
  /** Locates the scrollable viewport; may return null. */
  getViewport: () => HTMLElement | null;
  /** Changes whenever the list content changes (e.g. `segments.length`). */
  dep: number;
  /**
   * Stable identity of the top (earliest) row. A change while the list grows
   * marks a prepend; growth with an unchanged top is a bottom-append.
   */
  topKey: string | number | null;
  /** True when the user has scrolled away and must not be auto-followed. */
  userScrolled: boolean;
  /** Scrolls to the bottom, for the live-tail (pinned + append) case. */
  scrollToBottom: () => void;
}

/**
 * Wires {@link computeAnchorAdjustment} to a real viewport. Snapshots geometry
 * during render (before the new rows paint) and reconciles in a layout effect.
 *
 * `isAdjustingRef` is set while `scrollTop` is moved programmatically; the
 * caller's scroll handler must ignore scroll events while it is true.
 */
export function useScrollAnchor({
  getViewport,
  dep,
  topKey,
  userScrolled,
  scrollToBottom,
}: UseScrollAnchorArgs): { isAdjustingRef: React.RefObject<boolean> } {
  const snapshotRef = useRef<(AnchorSnapshot & { grewAtTop: boolean }) | null>(
    null,
  );
  const isAdjustingRef = useRef(false);
  // "Grew AND top changed" = prepend; "grew, top unchanged" = bottom-append.
  const prevDepRef = useRef(dep);
  const prevTopKeyRef = useRef(topKey);

  const grewAtTop = dep > prevDepRef.current && topKey !== prevTopKeyRef.current;

  // Snapshot during render, before React commits the new rows to the DOM.
  const viewportForSnapshot = getViewport();
  if (viewportForSnapshot) {
    snapshotRef.current = {
      prevScrollHeight: viewportForSnapshot.scrollHeight,
      prevScrollTop: viewportForSnapshot.scrollTop,
      pinnedToBottom: !userScrolled && isPinnedToBottom(viewportForSnapshot),
      grewAtTop,
    };
  }

  useLayoutEffect(() => {
    const viewport = getViewport();
    const snapshot = snapshotRef.current;
    prevDepRef.current = dep;
    prevTopKeyRef.current = topKey;
    if (!viewport || !snapshot) return;

    if (snapshot.pinnedToBottom) {
      scrollToBottom();
      return;
    }

    const { scrollTop, changed } = computeAnchorAdjustment({
      ...snapshot,
      nextScrollHeight: viewport.scrollHeight,
      clientHeight: viewport.clientHeight,
    });

    if (!changed) return;

    // Cleared on the next frame, after the scroll event has fired.
    isAdjustingRef.current = true;
    viewport.scrollTop = scrollTop;
    requestAnimationFrame(() => {
      isAdjustingRef.current = false;
    });
    // dep drives reconciliation; getViewport/scrollToBottom are stable refs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dep]);

  return { isAdjustingRef };
}
