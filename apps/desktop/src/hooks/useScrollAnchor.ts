import { useLayoutEffect, useRef } from "react";

/**
 * Standard scroll-anchoring for a list whose items can arrive at EITHER end.
 *
 * The transcript is time-ordered (see appStore `onLiveSegment`), so a backfill
 * segment with an earlier `audio_offset_seconds` sorts to the TOP and prepends
 * mid-stream. A naive "scroll to bottom on length change" treats that prepend
 * like a live-tail append and yanks the viewport, producing the "bounce". This
 * hook keeps the visible content fixed on prepend and only follows the bottom
 * when the user is genuinely pinned there and content was appended.
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
   * Whether the growth happened at the TOP (a prepend). Only a prepend shifts
   * the rows a scrolled-up reader is looking at, so only a prepend needs
   * compensation. A bottom-append grows `scrollHeight` too but lands below the
   * fold and must NOT move the viewport.
   */
  grewAtTop: boolean;
}

export interface AnchorAdjustment {
  /** The `scrollTop` the viewport should hold after the update. */
  scrollTop: number;
  /**
   * Whether the viewport actually needs to move. `false` means leave
   * `scrollTop` alone (e.g. content appended while the user is reading above
   * the fold, or no height change at all).
   */
  changed: boolean;
}

/**
 * Pure decision/delta math for scroll anchoring. No DOM access so it is unit
 * testable without jsdom geometry.
 *
 * - Pinned to bottom + content appended (live tail) -> ride the bottom.
 * - Content prepended (height grew at the TOP, user reading above the fold) ->
 *   shift `scrollTop` by the height delta so the visible rows do not move.
 * - Append below the fold, or no height change -> no-op (leave the user put).
 *
 * The result is always clamped to `[0, maxScrollTop]` so it can never strand
 * the viewport past its range or go negative.
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

  // Live tail: the user is glued to the bottom, so keep them there regardless
  // of which end grew. The caller animates this case; we only report the target.
  if (pinnedToBottom) {
    return { scrollTop: maxScrollTop, changed: maxScrollTop !== prevScrollTop };
  }

  // Not pinned: the viewport only needs to move if rows were inserted ABOVE the
  // fold (a prepend). A bottom-append grows scrollHeight but lands below what
  // the user is reading, so it must leave scrollTop alone — moving it here was
  // the bug that pushed a scrolled-up reader down on every live segment.
  if (heightDelta <= 0 || !grewAtTop) {
    return { scrollTop: clamp(prevScrollTop), changed: false };
  }

  // Prepend while reading above the bottom: compensate by the full delta so the
  // rows under the cursor stay put.
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
  /** Locates the scrollable viewport. Re-read each commit; may return null. */
  getViewport: () => HTMLElement | null;
  /**
   * Changes whenever the list content changes (e.g. `segments.length`). Drives
   * the post-update layout effect.
   */
  dep: number;
  /**
   * Stable identity of the TOP (earliest) row. When the list grows AND this
   * changes, a row was inserted at the top (a prepend) and the viewport is
   * compensated; when it grows but this is unchanged, the growth was a
   * bottom-append and the viewport is left alone.
   */
  topKey: string | number | null;
  /**
   * True when the user has scrolled away and should NOT be auto-followed. When
   * set, even a pinned-bottom snapshot is treated as not-pinned so live tail
   * does not fight the user.
   */
  userScrolled: boolean;
  /**
   * Scrolls to the bottom (live-tail). Called only when the user is pinned and
   * content was appended, so the existing smooth animation is preserved.
   */
  scrollToBottom: () => void;
}

/**
 * Wires {@link computeAnchorAdjustment} to a real viewport.
 *
 * Snapshots `scrollHeight`/`scrollTop`/pinned-state on render (before the
 * browser paints the new rows) and reconciles in a `useLayoutEffect` after the
 * commit, so the user never sees an intermediate jumped frame.
 *
 * Returns `isAdjustingRef`: the scroll handler must check it and ignore scroll
 * events while true, because the prepend compensation moves `scrollTop`
 * programmatically and must not be mistaken for a user scroll (which would flip
 * `userScrolled` and break live tail).
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
  // Last committed content shape, used to classify the next growth. The top row
  // changes only on a structural change at the front (segment ids/offsets are
  // stable), so "grew AND top changed" is a prepend; "grew, top unchanged" is a
  // bottom-append.
  const prevDepRef = useRef(dep);
  const prevTopKeyRef = useRef(topKey);

  const grewAtTop = dep > prevDepRef.current && topKey !== prevTopKeyRef.current;

  // Snapshot during render, BEFORE React commits the new rows to the DOM. The
  // body runs while the viewport still shows the pre-update geometry.
  const viewportForSnapshot = getViewport();
  if (viewportForSnapshot) {
    snapshotRef.current = {
      prevScrollHeight: viewportForSnapshot.scrollHeight,
      prevScrollTop: viewportForSnapshot.scrollTop,
      // A user who scrolled away is never treated as pinned, so live tail
      // can't drag them back down.
      pinnedToBottom: !userScrolled && isPinnedToBottom(viewportForSnapshot),
      grewAtTop,
    };
  }

  useLayoutEffect(() => {
    const viewport = getViewport();
    const snapshot = snapshotRef.current;
    // Advance the committed-shape trackers for the next render's classification.
    prevDepRef.current = dep;
    prevTopKeyRef.current = topKey;
    if (!viewport || !snapshot) return;

    if (snapshot.pinnedToBottom) {
      // Live tail: defer to the caller's (smooth) bottom scroll.
      scrollToBottom();
      return;
    }

    const { scrollTop, changed } = computeAnchorAdjustment({
      ...snapshot,
      nextScrollHeight: viewport.scrollHeight,
      clientHeight: viewport.clientHeight,
    });

    if (!changed) return;

    // Programmatic move — guard the scroll handler so it doesn't read this as a
    // user scroll. Cleared on the next frame, after the scroll event has fired.
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
