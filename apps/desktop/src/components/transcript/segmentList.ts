import {
  createContext,
  useContext,
  useLayoutEffect,
  useMemo,
  useRef,
} from "react";
import type { DbSegment } from "@/lib/db";

/// List-level operations a transcript row reaches through context instead of
/// props. The handle is referentially stable for the list's lifetime, so rows
/// don't re-render when segments are appended.
export interface SegmentListHandle {
  /// Segment ids in display order, read at call time.
  getOrderedIds: () => string[];
  /// Rows register how to enter edit mode so list-level UI (the shared
  /// context menu) can start an edit. Returns the unregister function.
  registerStartEdit: (segmentId: string, startEdit: () => void) => () => void;
  startEdit: (segmentId: string) => void;
}

export const SegmentListContext = createContext<SegmentListHandle | null>(null);

export function useSegmentList(): SegmentListHandle | null {
  return useContext(SegmentListContext);
}

export function useSegmentListHandle(segments: DbSegment[]): SegmentListHandle {
  const segmentsRef = useRef(segments);
  useLayoutEffect(() => {
    segmentsRef.current = segments;
  }, [segments]);
  const startEditById = useRef(new Map<string, () => void>());

  return useMemo<SegmentListHandle>(
    () => ({
      getOrderedIds: () => segmentsRef.current.map((s) => s.id),
      registerStartEdit: (segmentId, startEdit) => {
        const handlers = startEditById.current;
        handlers.set(segmentId, startEdit);
        return () => {
          if (handlers.get(segmentId) === startEdit) handlers.delete(segmentId);
        };
      },
      startEdit: (segmentId) => startEditById.current.get(segmentId)?.(),
    }),
    [],
  );
}
