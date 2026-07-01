import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowDown, ArrowUp } from "lucide-react";
import { ScrollArea } from "@/components/ui/scroll-area";
import { useScrollAnchor } from "@/hooks/useScrollAnchor";
import { TranscriptSegments } from "@/components/TranscriptSegments";
import {
  MarqueeOverlay,
  type MarqueeOverlayHandle,
  type MarqueeRect,
} from "@/components/MarqueeOverlay";
import { BulkActionsBar } from "@/components/transcript/BulkActionsBar";
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

export function ChatView({
  sessionId,
  segments,
  backfillActive,
  backfillBoundarySeconds,
  isEditable,
  currentPlaybackTime,
  onTimestampClick,
  initialScrollToBottom,
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
  initialScrollToBottom?: boolean;
}) {
  const bottomRef = useRef<HTMLDivElement>(null);
  const activeRef = useRef<HTMLDivElement>(null);
  const scrollAreaRef = useRef<HTMLDivElement>(null);
  const autoScrollingRef = useRef(false);
  const lastScrollTopRef = useRef(0);
  const [userScrolled, setUserScrolled] = useState(false);
  const [scrollDirection, setScrollDirection] = useState<"up" | "down">("down");

  const selectedSegmentIds = useAppStore((s) => s.selectedSegmentIds);
  const setSegmentSelection = useAppStore((s) => s.setSegmentSelection);
  const clearSegmentSelection = useAppStore((s) => s.clearSegmentSelection);
  const deleteSegments = useAppStore((s) => s.deleteSegments);
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

  const orderedIds = useMemo(() => segments.map((s) => s.id), [segments]);

  // Split segments at the backfill→live boundary so the "Processing prior
  // audio" indicator sits between old and new audio. Backfill segments
  // occupy offsets `0..backfillBoundarySeconds`; live audio comes after.
  // Until the first backfill chunk lands, boundary is null and every
  // received segment is live (rendered below the indicator).
  const { backfillSegments, liveSegments } = useMemo(() => {
    if (!backfillActive) {
      return { backfillSegments: segments, liveSegments: [] as DbSegment[] };
    }
    const boundary = backfillBoundarySeconds ?? 0;
    const splitIndex = segments.findIndex(
      (s) => s.audio_offset_seconds >= boundary,
    );
    if (splitIndex === -1) {
      return { backfillSegments: segments, liveSegments: [] as DbSegment[] };
    }
    return {
      backfillSegments: segments.slice(0, splitIndex),
      liveSegments: segments.slice(splitIndex),
    };
  }, [segments, backfillActive, backfillBoundarySeconds]);

  // Marquee selection state — drag-select across the transcript. Any
  // pointerdown in the container records a pending start; we commit to
  // marquee mode once the pointer moves past DRAG_THRESHOLD_PX. Below
  // threshold the pointerup falls through to the bubble's onClick
  // (toggle / shift-range / cmd-toggle / enter-edit). Active editing
  // controls (contenteditable, input, textarea) are exempt from the
  // pending-pointer path so in-edit text selection still works.
  const marqueeStartRef = useRef<{ x: number; y: number } | null>(null);
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
  // The ref is read on pointerup to compute the intersection.
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

  // Scroll anchoring. The transcript is time-ordered, so backfill brings in
  // OLDER segments that sort to the TOP and prepend mid-stream. Anchoring keeps
  // the visible content fixed on prepend (no "bounce") and only follows the
  // bottom (live tail) when the user is genuinely pinned there and content was
  // appended. `isAdjustingRef` flags the programmatic compensation so the scroll
  // handler below ignores it instead of mistaking it for a user scroll.
  const getViewport = useCallback(
    () =>
      (scrollAreaRef.current?.querySelector(
        "[data-slot='scroll-area-viewport']",
      ) as HTMLElement | null) ?? null,
    [],
  );
  const { isAdjustingRef } = useScrollAnchor({
    getViewport,
    dep: segments.length,
    // Identity of the top (earliest) row. A backfill prepend changes it; a
    // live append at the bottom does not — that's how the hook tells a prepend
    // (compensate) from an append (leave a scrolled-up reader alone).
    topKey: segments[0]?.id ?? null,
    userScrolled,
    // Smooth bottom-follow for the live-tail (append-while-pinned) case only.
    scrollToBottom: () => {
      bottomRef.current?.scrollIntoView({ behavior: "smooth" });
    },
  });

  // Resume following on demand (e.g. playback stops -> userScrolled cleared)
  // even without a new segment, matching the prior stick-to-bottom behavior.
  useEffect(() => {
    if (!userScrolled) {
      bottomRef.current?.scrollIntoView({ behavior: "smooth" });
    }
  }, [userScrolled]);

  useEffect(() => {
    if (initialScrollToBottom) {
      requestAnimationFrame(() => {
        bottomRef.current?.scrollIntoView({ behavior: "instant" });
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // When playback stops, resume following so the next session sticks again.
  useEffect(() => {
    if (!isPlaying) {
      setUserScrolled(false);
    }
  }, [isPlaying]);

  // Auto-scroll to the active segment during playback (unless the user is freed).
  useEffect(() => {
    if (!isPlaying || !activeRef.current || userScrolled) return;
    autoScrollingRef.current = true;
    activeRef.current.scrollIntoView({ behavior: "smooth", block: "nearest" });
    // Cleanup matters here — the effect re-runs on every timeupdate
    // tick, so without it stale timers race with subsequent legit
    // scrolls and flip autoScrollingRef.current mid-gesture.
    const t = setTimeout(() => {
      autoScrollingRef.current = false;
    }, 150);
    return () => clearTimeout(t);
  }, [currentPlaybackTime, userScrolled, isPlaying]);

  // Disengage stick-to-bottom on user scroll. Programmatic autoscroll only
  // moves the viewport *down*, so any decrease in scrollTop is unambiguously
  // the user — robust against rapid segment arrival.
  useEffect(() => {
    const viewport = scrollAreaRef.current?.querySelector(
      "[data-slot='scroll-area-viewport']"
    ) as HTMLElement | null;
    if (!viewport) return;

    lastScrollTopRef.current = viewport.scrollTop;

    const handleScroll = () => {
      const newScrollTop = viewport.scrollTop;
      // Prepend compensation moves scrollTop programmatically. Track the new
      // position (so the next real scroll deltas off the right baseline) but
      // do not let it flip userScrolled or scrollDirection — it isn't a user
      // gesture.
      if (isAdjustingRef.current) {
        lastScrollTopRef.current = newScrollTop;
        return;
      }
      const wentUp = newScrollTop < lastScrollTopRef.current - 1;
      lastScrollTopRef.current = newScrollTop;

      if (isPlaying && activeRef.current) {
        if (autoScrollingRef.current) return;
        const rect = activeRef.current.getBoundingClientRect();
        const vpRect = viewport.getBoundingClientRect();
        const visible = rect.top >= vpRect.top && rect.bottom <= vpRect.bottom;
        setUserScrolled(!visible);
        if (!visible) {
          setScrollDirection(rect.top < vpRect.top ? "up" : "down");
        }
        return;
      }

      const distFromBottom =
        viewport.scrollHeight - newScrollTop - viewport.clientHeight;
      if (wentUp) {
        setUserScrolled(true);
        setScrollDirection("down");
      } else if (distFromBottom < 4) {
        setUserScrolled(false);
      }
    };

    viewport.addEventListener("scroll", handleScroll, { passive: true });
    return () => viewport.removeEventListener("scroll", handleScroll);
  }, [isPlaying, isAdjustingRef]);

  const handleJumpToCurrent = useCallback(() => {
    setUserScrolled(false);
    requestAnimationFrame(() => {
      activeRef.current?.scrollIntoView({
        behavior: "smooth",
        block: "center",
      });
    });
  }, []);

  const handleTimestampClick = useCallback(
    (time: number) => {
      setUserScrolled(false);
      onTimestampClick?.(time);
    },
    [onTimestampClick]
  );

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
    const rect = container.getBoundingClientRect();
    const x = ptr.x - rect.left + container.scrollLeft;
    const y = ptr.y - rect.top + container.scrollTop;
    const left = Math.min(start.x, x);
    const top = Math.min(start.y, y);
    const width = Math.abs(x - start.x);
    const height = Math.abs(y - start.y);
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
      const container = e.currentTarget as HTMLElement;
      containerRef.current = container;
      viewportRef.current =
        scrollAreaRef.current?.querySelector<HTMLElement>(
          '[data-slot="scroll-area-viewport"]',
        ) ?? null;
      const rect = container.getBoundingClientRect();
      // Anchor the marquee at the *pending* start, not the current
      // pointer — otherwise the first 6 px of movement wouldn't be
      // covered by the rect.
      marqueeStartRef.current = {
        x: pending.clientX - rect.left + container.scrollLeft,
        y: pending.clientY - rect.top + container.scrollTop,
      };
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
      // Treat as a whitespace clear if we started on whitespace.
      // The originating bubble (if any) will not receive a click because
      // suppressNextClickRef was armed at promotion time.
      return;
    }
    const container = e.currentTarget as HTMLElement;
    const containerRect = container.getBoundingClientRect();
    const marqueeRect = {
      left: rect.left - container.scrollLeft + containerRect.left,
      top: rect.top - container.scrollTop + containerRect.top,
      right:
        rect.left + rect.width - container.scrollLeft + containerRect.left,
      bottom:
        rect.top + rect.height - container.scrollTop + containerRect.top,
    };
    const nodes = container.querySelectorAll<HTMLElement>("[data-segment-id]");
    const picked = new Set<string>();
    nodes.forEach((node) => {
      const r = node.getBoundingClientRect();
      const intersects =
        r.left < marqueeRect.right &&
        r.right > marqueeRect.left &&
        r.top < marqueeRect.bottom &&
        r.bottom > marqueeRect.top;
      if (intersects) {
        const id = node.getAttribute("data-segment-id");
        if (id) picked.add(id);
      }
    });
    const additive = e.shiftKey || e.metaKey || e.ctrlKey;
    if (additive) {
      const merged = new Set(selectedSegmentIds);
      picked.forEach((id) => merged.add(id));
      setSegmentSelection(merged);
    } else {
      setSegmentSelection(picked);
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

  // Find the active segment based on playback time. Null when not
  // playing — `currentPlaybackTime` alone is unreliable (see above).
  const activeSegmentId = useMemo(() => {
    if (!isPlaying || currentPlaybackTime == null) return null;
    for (let i = segments.length - 1; i >= 0; i--) {
      if (segments[i].audio_offset_seconds <= currentPlaybackTime) {
        return segments[i].id;
      }
    }
    return null;
  }, [segments, currentPlaybackTime, isPlaying]);

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
      <ScrollArea ref={scrollAreaRef} className="h-full">
        <div
          className="relative min-h-full space-y-2 px-3 py-2 select-none"
          onPointerDown={handleContainerPointerDown}
          onPointerMove={handleContainerPointerMove}
          onPointerUp={handleContainerPointerUp}
          onPointerCancel={handleContainerPointerCancel}
          onClickCapture={(e) => {
            if (suppressNextClickRef.current) {
              suppressNextClickRef.current = false;
              e.preventDefault();
              e.stopPropagation();
            }
          }}
        >
          <TranscriptSegments
            sessionId={sessionId ?? ""}
            segments={backfillSegments}
            isEditable={!!isEditable}
            activeSegmentId={activeSegmentId}
            activeRef={activeRef}
            selectedSegmentIds={selectedSegmentIds}
            orderedIds={orderedIds}
            onTimestampClick={handleTimestampClick}
          />
          {backfillActive && (
            <div className="flex items-center gap-4 py-2 text-muted-foreground">
              <div className="h-px flex-1 border-t border-dashed" />
              <span className="relative flex h-2 w-2">
                <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary opacity-75" />
                <span className="relative inline-flex h-2 w-2 rounded-full bg-primary" />
              </span>
              <span className="text-[11px] font-medium">Processing prior audio</span>
              <div className="h-px flex-1 border-t border-dashed" />
            </div>
          )}
          {liveSegments.length > 0 && (
            <TranscriptSegments
              sessionId={sessionId ?? ""}
              segments={liveSegments}
              isEditable={!!isEditable}
              activeSegmentId={activeSegmentId}
              activeRef={activeRef}
              selectedSegmentIds={selectedSegmentIds}
              orderedIds={orderedIds}
              onTimestampClick={handleTimestampClick}
            />
          )}
          <MarqueeOverlay ref={marqueeOverlayRef} />
          <div ref={bottomRef} />
        </div>
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
