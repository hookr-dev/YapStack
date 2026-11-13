import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  defaultRangeExtractor,
  useVirtualizer,
  type Range,
} from "@tanstack/react-virtual";
import { ArrowDown, ArrowUp } from "lucide-react";
import { ScrollArea } from "@/components/ui/scroll-area";
import { TranscriptRowView } from "@/components/transcript/TranscriptRowView";
import {
  buildTranscriptRows,
  estimateRowHeight,
} from "@/components/transcript/rows";
import {
  anchorAt,
  classifyRowVisibility,
  pickSegmentsInBand,
  type RowAnchor,
} from "@/components/transcript/rowGeometry";
import {
  MarqueeOverlay,
  type MarqueeOverlayHandle,
  type MarqueeRect,
} from "@/components/MarqueeOverlay";
import { BulkActionsBar } from "@/components/transcript/BulkActionsBar";
import { SegmentContextMenu } from "@/components/transcript/SegmentContextMenu";
import {
  SegmentListContext,
  useSegmentListHandle,
} from "@/components/transcript/segmentList";
import { findLastSegmentStartingAtOrBefore } from "@/lib/segment-search";
import { useAppStore } from "@/stores/appStore";
import type { DbSegment } from "@/lib/db";

// Click-vs-drag threshold for marquee promotion. Matches the macOS Finder
// convention and gives trackpad jitter room before a tap turns into a
// drag.
const DRAG_THRESHOLD_PX = 6;
// Edge auto-scroll: linear velocity ramp from 0 at SCROLL_HOT_ZONE px
// from the viewport edge to SCROLL_MAX_VELOCITY px/frame at the edge.
const SCROLL_HOT_ZONE = 30;
const SCROLL_MAX_VELOCITY = 12;
// The list's former `space-y-2 py-2`, now owned by the virtualizer.
const ROW_GAP_PX = 8;
const LIST_PADDING_Y_PX = 8;
const OVERSCAN_ROWS = 8;
// Distance from the bottom under which the viewport counts as pinned.
const PINNED_THRESHOLD_PX = 4;
// Width assumed for size estimates until the viewport has been measured.
const DEFAULT_LIST_WIDTH_PX = 400;

export function ChatView({
  sessionId,
  segments,
  backfillActive,
  backfillBoundarySeconds,
  isEditable,
  currentPlaybackTime,
  onTimestampClick,
}: {
  /// Required for speaker-name rename persistence; falls back to "" when
  /// rendering ad-hoc segment lists outside a session.
  sessionId?: string;
  segments: DbSegment[];
  backfillActive?: boolean;
  /// Audio offset (seconds) where backfill ends and live audio begins. Used
  /// to position the "Processing prior audio" indicator between old and new
  /// segments. Null until the first backfill chunk lands.
  backfillBoundarySeconds?: number | null;
  isEditable?: boolean;
  currentPlaybackTime?: number;
  onTimestampClick?: (time: number) => void;
}) {
  const scrollAreaRef = useRef<HTMLDivElement>(null);
  const autoScrollingRef = useRef(false);
  const lastScrollTopRef = useRef(0);
  // Whether the viewport was at the bottom at its last scroll event. A new
  // session (and the first render) counts as at the bottom: transcripts open
  // at their latest line.
  const atBottomRef = useRef(true);
  const [userScrolled, setUserScrolled] = useState(false);
  const [scrollDirection, setScrollDirection] = useState<"up" | "down">("down");

  const selectedSegmentIds = useAppStore((s) => s.selectedSegmentIds);
  const setSegmentSelection = useAppStore((s) => s.setSegmentSelection);
  const clearSegmentSelection = useAppStore((s) => s.clearSegmentSelection);
  const deleteSegments = useAppStore((s) => s.deleteSegments);
  const editingSegmentId = useAppStore((s) => s.editingSegmentId);
  // `currentPlaybackTime` persists across sessions and is `0` even
  // when no audio has played, so it isn't a reliable "audio active"
  // signal — gate playback UI on `isPlaying` instead.
  const isPlaying = useAppStore((s) => s.isPlaying);
  const setPlaybackTime = useAppStore((s) => s.setPlaybackTime);
  const setIsPlaying = useAppStore((s) => s.setIsPlaying);

  // Playback state is global; reset on session switch so it doesn't
  // bleed across sessions.
  useEffect(() => {
    clearSegmentSelection();
    setPlaybackTime(0);
    setIsPlaying(false);
  }, [sessionId, clearSegmentSelection, setPlaybackTime, setIsPlaying]);

  // Selection-scoped keyboard shortcuts: Escape clears, Backspace/Delete
  // bulk-deletes. Only fires when the user isn't typing in an input and
  // there's an active selection.
  useEffect(() => {
    if (selectedSegmentIds.size === 0) return;
    const onKey = (e: KeyboardEvent) => {
      const el = document.activeElement as HTMLElement | null;
      if (
        el &&
        (el.tagName === "INPUT" ||
          el.tagName === "TEXTAREA" ||
          el.isContentEditable)
      ) {
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        clearSegmentSelection();
      } else if ((e.key === "Backspace" || e.key === "Delete") && isEditable) {
        e.preventDefault();
        deleteSegments(Array.from(selectedSegmentIds));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [selectedSegmentIds, clearSegmentSelection, deleteSegments, isEditable]);

  const segmentList = useSegmentListHandle(segments);

  const rows = useMemo(
    () => buildTranscriptRows(segments, !!backfillActive, backfillBoundarySeconds),
    [segments, backfillActive, backfillBoundarySeconds],
  );
  const indexByKey = useMemo(
    () => new Map(rows.map((row, i) => [row.key, i])),
    [rows],
  );
  const selectionActive = selectedSegmentIds.size > 0;

  // Rows that must stay mounted wherever the viewport is: the segment being
  // edited (its contentEditable and the D4 edit guard live in the row, and
  // the guard outlives the blur while the save is in flight) and the row
  // holding focus (e.g. a speaker rename input).
  const [focusedRowKey, setFocusedRowKey] = useState<string | null>(null);
  const pinnedIndexes = useMemo(() => {
    const out: number[] = [];
    for (const key of [editingSegmentId, focusedRowKey]) {
      const i = key == null ? undefined : indexByKey.get(key);
      if (i !== undefined && !out.includes(i)) out.push(i);
    }
    return out;
  }, [editingSegmentId, focusedRowKey, indexByKey]);
  const rangeExtractor = useCallback(
    (range: Range) => {
      const indexes = defaultRangeExtractor(range);
      const extra = pinnedIndexes.filter((i) => !indexes.includes(i));
      return extra.length === 0
        ? indexes
        : [...indexes, ...extra].sort((a, b) => a - b);
    },
    [pinnedIndexes],
  );

  const getViewport = useCallback(
    () =>
      (scrollAreaRef.current?.querySelector(
        "[data-slot='scroll-area-viewport']",
      ) as HTMLElement | null) ?? null,
    [],
  );
  const listWidthRef = useRef(DEFAULT_LIST_WIDTH_PX);
  const getItemKey = useCallback((i: number) => rows[i].key, [rows]);
  const estimateSize = useCallback(
    (i: number) => estimateRowHeight(rows[i], listWidthRef.current),
    [rows],
  );
  // `anchorTo: "end"` makes the virtualizer anchor by key: when rows are
  // inserted or removed, the row at the top of the viewport keeps its screen
  // position, and a row resizing while the viewport is at the bottom keeps
  // it there.
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: getViewport,
    estimateSize,
    getItemKey,
    rangeExtractor,
    overscan: OVERSCAN_ROWS,
    gap: ROW_GAP_PX,
    paddingStart: LIST_PADDING_Y_PX,
    paddingEnd: LIST_PADDING_Y_PX,
    anchorTo: "end",
    scrollEndThreshold: PINNED_THRESHOLD_PX,
  });
  const measuredWidth = virtualizer.scrollRect?.width;
  useLayoutEffect(() => {
    if (measuredWidth) listWidthRef.current = measuredWidth;
  }, [measuredWidth]);

  useLayoutEffect(() => {
    atBottomRef.current = true;
  }, [sessionId]);

  // Follow the bottom when the viewport was there before the rows changed:
  // opening a transcript, a live append, and backfill landing above the
  // divider (which the virtualizer reads as a mid-list insert, not an
  // append). Instant, so a long transcript doesn't animate on open.
  useLayoutEffect(() => {
    if (atBottomRef.current && rows.length > 0) virtualizer.scrollToEnd();
  }, [rows, virtualizer]);

  // When playback stops, resume following so the next session sticks again.
  useEffect(() => {
    if (!isPlaying) {
      setUserScrolled(false);
    }
  }, [isPlaying]);

  // Find the active segment based on playback time. Null when not
  // playing — `currentPlaybackTime` alone is unreliable (see above).
  const activeSegmentId = useMemo(() => {
    if (!isPlaying || currentPlaybackTime == null) return null;
    return (
      findLastSegmentStartingAtOrBefore(segments, currentPlaybackTime)?.id ??
      null
    );
  }, [segments, currentPlaybackTime, isPlaying]);
  const activeIndex =
    activeSegmentId == null ? null : (indexByKey.get(activeSegmentId) ?? null);
  const activeIndexRef = useRef(activeIndex);
  useLayoutEffect(() => {
    activeIndexRef.current = activeIndex;
  }, [activeIndex]);

  // A mounted row scrolls smoothly; an unmounted one is far away, so jump
  // straight to it rather than animate through rows that aren't measured.
  const revealRow = useCallback(
    (index: number, block: "nearest" | "center") => {
      const el = virtualizer.elementsCache.get(rows[index].key) as
        | HTMLElement
        | undefined;
      if (el?.isConnected) {
        el.scrollIntoView({ behavior: "smooth", block });
      } else {
        virtualizer.scrollToIndex(index, { align: "center" });
      }
    },
    [virtualizer, rows],
  );

  // Auto-scroll to the active segment during playback (unless the user is freed).
  useEffect(() => {
    if (!isPlaying || activeIndex == null || userScrolled) return;
    autoScrollingRef.current = true;
    revealRow(activeIndex, "nearest");
    const t = setTimeout(() => {
      autoScrollingRef.current = false;
    }, 150);
    return () => {
      clearTimeout(t);
      autoScrollingRef.current = false;
    };
    // `revealRow` changes with every rows change; following is keyed on
    // the active row, not on the list.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeIndex, userScrolled, isPlaying]);

  // Disengage stick-to-bottom on user scroll. Programmatic moves either go
  // down (follow, prepend anchoring) or stay at the bottom (a row above
  // shrinking while pinned), so moving up away from the bottom is the user.
  const hasRows = rows.length > 0;
  useEffect(() => {
    const viewport = getViewport();
    if (!viewport) return;

    lastScrollTopRef.current = viewport.scrollTop;

    const handleScroll = () => {
      const newScrollTop = viewport.scrollTop;
      const wentUp = newScrollTop < lastScrollTopRef.current - 1;
      lastScrollTopRef.current = newScrollTop;
      const distFromBottom =
        viewport.scrollHeight - newScrollTop - viewport.clientHeight;
      atBottomRef.current = distFromBottom < PINNED_THRESHOLD_PX;

      const active = activeIndexRef.current;
      if (isPlaying && active != null) {
        if (autoScrollingRef.current) return;
        const item = virtualizer.measurementsCache[active];
        if (!item) return;
        const where = classifyRowVisibility(
          item,
          newScrollTop,
          viewport.clientHeight,
        );
        setUserScrolled(where !== "visible");
        if (where !== "visible") {
          setScrollDirection(where === "above" ? "up" : "down");
        }
        return;
      }

      if (wentUp && distFromBottom >= PINNED_THRESHOLD_PX) {
        setUserScrolled(true);
        setScrollDirection("down");
      } else if (distFromBottom < PINNED_THRESHOLD_PX) {
        setUserScrolled(false);
      }
    };

    viewport.addEventListener("scroll", handleScroll, { passive: true });
    return () => viewport.removeEventListener("scroll", handleScroll);
    // `hasRows`: the viewport mounts with the first row.
  }, [isPlaying, getViewport, virtualizer, hasRows]);

  const handleJumpToCurrent = useCallback(() => {
    setUserScrolled(false);
    requestAnimationFrame(() => {
      const index = activeIndexRef.current;
      if (index != null) revealRow(index, "center");
    });
  }, [revealRow]);

  const handleTimestampClick = useCallback(
    (time: number) => {
      setUserScrolled(false);
      onTimestampClick?.(time);
    },
    [onTimestampClick]
  );

  const rowKeyOf = (target: EventTarget | null): string | null => {
    const el = (target as Element | null)?.closest?.("[data-index]");
    if (!el) return null;
    return rows[Number(el.getAttribute("data-index"))]?.key ?? null;
  };

  const handleFocusCapture = (e: React.FocusEvent) => {
    setFocusedRowKey(rowKeyOf(e.target));
  };

  const handleBlurCapture = (e: React.FocusEvent) => {
    if (rowKeyOf(e.relatedTarget) === null) setFocusedRowKey(null);
  };

  // Marquee selection state — drag-select across the transcript. Any
  // pointerdown in the container records a pending start; we commit to
  // marquee mode once the pointer moves past DRAG_THRESHOLD_PX. Below
  // threshold the pointerup falls through to the bubble's onClick
  // (toggle / shift-range / cmd-toggle / enter-edit). Active editing
  // controls (contenteditable, input, textarea) are exempt from the
  // pending-pointer path so in-edit text selection still works.
  //
  // The drag's start is held relative to a row, not as a content offset:
  // rows above it are measured (and resized) as the drag auto-scrolls, which
  // moves every row below them.
  const marqueeStartRef = useRef<{ x: number; anchor: RowAnchor } | null>(null);
  const pendingDownRef = useRef<{
    clientX: number;
    clientY: number;
    onBubble: boolean;
  } | null>(null);
  // Set when marquee promotion commits; the synthetic click that would
  // otherwise fire on the originating bubble is intercepted in an
  // onClickCapture handler and discarded. More deterministic than
  // leaning on setPointerCapture to suppress the click.
  const suppressNextClickRef = useRef(false);
  // The marquee rect is intentionally NOT React state: at ~120 Hz a
  // setState per pointermove forces ChatView and its subtree to reconcile
  // and the drag visibly stutters. Instead we keep the rect in a ref and
  // mutate the overlay div's style imperatively via marqueeOverlayRef.
  // The ref is read on pointerup to pick the rows in its band.
  const marqueeRectRef = useRef<MarqueeRect | null>(null);
  const marqueeOverlayRef = useRef<MarqueeOverlayHandle | null>(null);
  // Edge auto-scroll: 30 px hot zones at top + bottom, linear velocity
  // ramp capped at 12 px/frame, RAF loop that recomputes the marquee
  // end-point from the cached pointer each tick so the rect grows as
  // the viewport scrolls under a stationary cursor.
  const viewportRef = useRef<HTMLElement | null>(null);
  const lastPointerRef = useRef<{ x: number; y: number } | null>(null);
  const scrollRafRef = useRef<number | null>(null);
  const containerRef = useRef<HTMLElement | null>(null);

  // Content-space y of a row anchor, from the current measurements.
  const resolveAnchor = (anchor: RowAnchor): number | null => {
    const i = indexByKey.get(anchor.key);
    const item = i === undefined ? undefined : virtualizer.measurementsCache[i];
    return item ? item.start + anchor.offset : null;
  };

  // Compute auto-scroll velocity from a pointer's clientY relative to
  // the viewport. Linear ramp inside the hot zones, zero otherwise.
  // Positive = scroll down, negative = scroll up.
  const computeScrollVelocity = (clientY: number, viewport: HTMLElement) => {
    const r = viewport.getBoundingClientRect();
    const distFromTop = clientY - r.top;
    const distFromBottom = r.bottom - clientY;
    if (distFromTop < SCROLL_HOT_ZONE && distFromTop >= 0) {
      const ramp = (SCROLL_HOT_ZONE - distFromTop) / SCROLL_HOT_ZONE;
      return -Math.min(SCROLL_MAX_VELOCITY, SCROLL_MAX_VELOCITY * ramp);
    }
    if (distFromBottom < SCROLL_HOT_ZONE && distFromBottom >= 0) {
      const ramp = (SCROLL_HOT_ZONE - distFromBottom) / SCROLL_HOT_ZONE;
      return Math.min(SCROLL_MAX_VELOCITY, SCROLL_MAX_VELOCITY * ramp);
    }
    // Outside the viewport entirely (pointer captured past the edge):
    // keep scrolling at max velocity in the appropriate direction.
    if (distFromTop < 0) return -SCROLL_MAX_VELOCITY;
    if (distFromBottom < 0) return SCROLL_MAX_VELOCITY;
    return 0;
  };

  // Recompute the marquee rect from the cached pointer position. Called
  // both on pointermove (cursor moved) and during the auto-scroll RAF
  // tick (viewport scrolled but cursor stationary).
  const updateMarqueeFromPointer = () => {
    const start = marqueeStartRef.current;
    const ptr = lastPointerRef.current;
    const container = containerRef.current;
    if (!start || !ptr || !container) return;
    const startY = resolveAnchor(start.anchor);
    if (startY === null) return;
    const rect = container.getBoundingClientRect();
    const x = ptr.x - rect.left;
    const y = ptr.y - rect.top;
    const left = Math.min(start.x, x);
    const top = Math.min(startY, y);
    const width = Math.abs(x - start.x);
    const height = Math.abs(y - startY);
    if (width < DRAG_THRESHOLD_PX && height < DRAG_THRESHOLD_PX) return;
    const next = { left, top, width, height };
    marqueeRectRef.current = next;
    marqueeOverlayRef.current?.setRect(next);
  };

  // Wipe all drag-tracking refs and hide the overlay. Shared by the
  // pointerup commit path and the pointercancel path. Does NOT touch
  // suppressNextClickRef — its disarm policy differs by branch (deferred
  // on commit, immediate on cancel).
  const resetMarqueeState = () => {
    marqueeStartRef.current = null;
    pendingDownRef.current = null;
    lastPointerRef.current = null;
    viewportRef.current = null;
    containerRef.current = null;
    marqueeRectRef.current = null;
    marqueeOverlayRef.current?.hide();
  };

  const stopAutoScroll = () => {
    if (scrollRafRef.current != null) {
      cancelAnimationFrame(scrollRafRef.current);
      scrollRafRef.current = null;
    }
  };

  const tickAutoScroll = () => {
    const viewport = viewportRef.current;
    const ptr = lastPointerRef.current;
    if (!viewport || !ptr || !marqueeStartRef.current) {
      scrollRafRef.current = null;
      return;
    }
    const velocity = computeScrollVelocity(ptr.y, viewport);
    if (velocity === 0) {
      scrollRafRef.current = null;
      return;
    }
    const before = viewport.scrollTop;
    viewport.scrollTop = before + velocity;
    if (viewport.scrollTop !== before) updateMarqueeFromPointer();
    scrollRafRef.current = requestAnimationFrame(tickAutoScroll);
  };

  const ensureAutoScrollRunning = () => {
    if (scrollRafRef.current != null) return;
    if (!viewportRef.current || !lastPointerRef.current) return;
    if (computeScrollVelocity(lastPointerRef.current.y, viewportRef.current) === 0) return;
    scrollRafRef.current = requestAnimationFrame(tickAutoScroll);
  };

  // Cancel any in-flight RAF when the component unmounts mid-drag.
  useEffect(() => {
    return () => stopAutoScroll();
  }, []);

  const handleContainerPointerDown = (e: React.PointerEvent) => {
    if (e.button !== 0) return;
    // React routes synthetic events through the component tree, so a click
    // on a Radix portal child (e.g. an open ContextMenu item) bubbles here
    // even though its DOM lives in document.body. Without this bail we'd
    // record a pendingDownRef for an event that isn't ours.
    if (!e.currentTarget.contains(e.target as Node)) return;
    const target = e.target as HTMLElement;
    // Active editing controls own their own pointer behavior. Skipping
    // pendingDownRef here keeps drag-to-select-text inside an editing
    // bubble (or future <input>/<textarea>) from being clobbered by the
    // marquee promotion path.
    if (target.closest('[contenteditable="true"], input, textarea')) return;
    pendingDownRef.current = {
      clientX: e.clientX,
      clientY: e.clientY,
      onBubble: !!target.closest("[data-segment-id]"),
    };
  };

  const handleContainerPointerMove = (e: React.PointerEvent) => {
    if (!marqueeStartRef.current) {
      const pending = pendingDownRef.current;
      if (!pending) return;
      const dx = Math.abs(e.clientX - pending.clientX);
      const dy = Math.abs(e.clientY - pending.clientY);
      if (Math.max(dx, dy) < DRAG_THRESHOLD_PX) return;
      const container = e.currentTarget as HTMLElement;
      const rect = container.getBoundingClientRect();
      // Anchor the marquee at the *pending* start, not the current
      // pointer — otherwise the first 6 px of movement wouldn't be
      // covered by the rect.
      const anchor = anchorAt(
        virtualizer.measurementsCache,
        rows,
        pending.clientY - rect.top,
      );
      if (!anchor) return;
      // Promote to marquee mode. Pointer capture so pointermove keeps
      // firing past the viewport edge for auto-scroll.
      e.currentTarget.setPointerCapture(e.pointerId);
      e.preventDefault();
      // Arm the one-shot click interceptor so the bubble's onClick
      // (toggle / enter-edit) cannot fire after the drag releases.
      suppressNextClickRef.current = true;
      // Blur any editing bubble so its onBlur (save) fires before the
      // selection changes underneath it.
      const active = document.activeElement as HTMLElement | null;
      if (active && active !== document.body) active.blur();
      containerRef.current = container;
      viewportRef.current = getViewport();
      marqueeStartRef.current = { x: pending.clientX - rect.left, anchor };
      pendingDownRef.current = null;
    }
    // Clear any selection the browser may have started before we
    // promoted (or any caret left by a click that re-promoted later).
    window.getSelection()?.removeAllRanges();
    lastPointerRef.current = { x: e.clientX, y: e.clientY };
    updateMarqueeFromPointer();
    ensureAutoScrollRunning();
  };

  const handleContainerPointerUp = (e: React.PointerEvent) => {
    // Pre-threshold: pointerdown was tracked but no marquee committed.
    // Hand off to the bubble's click (or clear selection on whitespace).
    if (!marqueeStartRef.current) {
      const pending = pendingDownRef.current;
      pendingDownRef.current = null;
      if (!pending) return;
      if (!pending.onBubble) {
        // Bare whitespace click — preserve the existing
        // "click-empty-space clears multi-selection" behavior.
        clearSegmentSelection();
      }
      return;
    }
    stopAutoScroll();
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    const rect = marqueeRectRef.current;
    resetMarqueeState();
    if (!rect) {
      // Threshold was crossed (start ref was set) but the marquee never
      // rendered (movement stayed under the per-axis render threshold).
      // The originating bubble (if any) will not receive a click because
      // suppressNextClickRef was armed at promotion time.
      return;
    }
    // Picked from measurements, not the DOM: rows the drag scrolled past
    // have unmounted.
    const picked = pickSegmentsInBand(
      virtualizer.measurementsCache,
      rows,
      rect.top,
      rect.top + rect.height,
    );
    const additive = e.shiftKey || e.metaKey || e.ctrlKey;
    if (additive) {
      const merged = new Set(selectedSegmentIds);
      picked.forEach((id) => merged.add(id));
      setSegmentSelection(merged);
    } else {
      setSegmentSelection(new Set(picked));
    }
    // Belt-and-suspenders: if Chromium suppressed the synthetic click
    // after the drag, the onClickCapture interceptor won't fire and the
    // ref would leak, swallowing an unrelated future click. Disarm on
    // the next tick — any synthesized click will already have fired.
    window.setTimeout(() => {
      suppressNextClickRef.current = false;
    }, 0);
  };

  const handleContainerPointerCancel = (e: React.PointerEvent) => {
    stopAutoScroll();
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    // A canceled drag never produces a click, so disarm immediately so
    // a later normal click on a bubble is not swallowed.
    suppressNextClickRef.current = false;
    resetMarqueeState();
  };

  if (segments.length === 0 && !backfillActive) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <p className="text-sm text-muted-foreground">
          Start speaking to begin transcription
        </p>
      </div>
    );
  }

  return (
    <div className="relative min-h-0 flex-1 select-text">
      <ScrollArea
        ref={scrollAreaRef}
        className="h-full"
        // The virtualizer anchors scroll itself; browser anchoring (WebView2)
        // would correct the same shift a second time.
        viewportClassName="[overflow-anchor:none]"
      >
        <SegmentListContext.Provider value={segmentList}>
          <SegmentContextMenu segments={segments} readOnly={!isEditable}>
            <div
              className="relative min-h-full select-none"
              style={{ height: virtualizer.getTotalSize() }}
              onPointerDown={handleContainerPointerDown}
              onPointerMove={handleContainerPointerMove}
              onPointerUp={handleContainerPointerUp}
              onPointerCancel={handleContainerPointerCancel}
              onFocusCapture={handleFocusCapture}
              onBlurCapture={handleBlurCapture}
              onClickCapture={(e) => {
                if (suppressNextClickRef.current) {
                  suppressNextClickRef.current = false;
                  e.preventDefault();
                  e.stopPropagation();
                }
              }}
            >
              <div role="list" aria-label="Transcript">
                {virtualizer.getVirtualItems().map((item) => {
                  const row = rows[item.index];
                  const segmentId =
                    row.kind === "segment" ? row.segment.id : null;
                  return (
                    <div
                      key={item.key}
                      data-index={item.index}
                      ref={virtualizer.measureElement}
                      role="listitem"
                      aria-setsize={rows.length}
                      aria-posinset={item.index + 1}
                      className="absolute top-0 left-0 w-full px-3"
                      style={{ transform: `translateY(${item.start}px)` }}
                    >
                      <TranscriptRowView
                        row={row}
                        sessionId={sessionId ?? ""}
                        isEditable={!!isEditable}
                        isActive={
                          segmentId !== null && segmentId === activeSegmentId
                        }
                        isSelected={
                          segmentId !== null && selectedSegmentIds.has(segmentId)
                        }
                        selectionActive={selectionActive}
                        onTimestampClick={handleTimestampClick}
                      />
                    </div>
                  );
                })}
              </div>
              <MarqueeOverlay ref={marqueeOverlayRef} />
            </div>
          </SegmentContextMenu>
        </SegmentListContext.Provider>
      </ScrollArea>
      <BulkActionsBar segments={segments} readOnly={!isEditable} />
      {userScrolled && isPlaying && (
        <button
          onClick={handleJumpToCurrent}
          className="absolute bottom-3 left-1/2 -translate-x-1/2 z-10 flex items-center gap-1.5 px-3 py-1.5 text-xs font-medium bg-primary text-primary-foreground rounded-full shadow-md hover:bg-primary/90 transition-colors"
        >
          {scrollDirection === "up" ? (
            <ArrowUp className="h-3 w-3" />
          ) : (
            <ArrowDown className="h-3 w-3" />
          )}
          Jump to current
        </button>
      )}
    </div>
  );
}
