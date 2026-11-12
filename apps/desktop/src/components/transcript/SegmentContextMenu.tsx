import { useRef, useState, type ReactElement } from "react";
import { BookmarkPlus, Copy, Eye, EyeOff, Pencil, Trash2 } from "lucide-react";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { useAppStore } from "@/stores/appStore";
import { useSegmentList } from "@/components/transcript/segmentList";
import { trackSegmentHidden } from "@/lib/analytics";
import { formatTime } from "@/lib/utils";
import type { DbSegment } from "@/lib/db";

/// Attribute a transcript row carries so one list-level context menu can
/// resolve which segment was right-clicked.
const SEGMENT_ROW_ATTR = "data-segment-row-id";

/// One context menu for a whole transcript list. `children` is the list
/// container; a right-click inside a `[data-segment-row-id]` row opens the
/// menu for that segment, anywhere else does nothing.
export function SegmentContextMenu({
  segments,
  readOnly,
  children,
}: {
  segments: DbSegment[];
  readOnly: boolean;
  children: ReactElement;
}) {
  const segmentList = useSegmentList();
  const deleteSegment = useAppStore((s) => s.deleteSegment);
  const toggleSegmentHidden = useAppStore((s) => s.toggleSegmentHidden);
  const [targetId, setTargetId] = useState<string | null>(null);
  // "Edit" waits until the menu has closed: while it is open its focus trap
  // pulls focus back from the bubble, and the blur saves and exits the edit.
  const editOnCloseRef = useRef<string | null>(null);

  const target = targetId
    ? segments.find((s) => s.id === targetId)
    : undefined;

  // Resolves and records the row under the event; null off a row, so a menu
  // opened there renders nothing rather than the previous target's items.
  const resolveTarget = (e: React.SyntheticEvent): string | null => {
    const row = (e.target as Element).closest(`[${SEGMENT_ROW_ATTR}]`);
    const id =
      row && e.currentTarget.contains(row)
        ? row.getAttribute(SEGMENT_ROW_ATTR)
        : null;
    setTargetId(id);
    return id;
  };

  const handleContextMenu = (e: React.MouseEvent) => {
    if (resolveTarget(e) === null) {
      // Not on a row: preventDefault is how Radix's trigger is told not to
      // open. The window-level handler in main.tsx suppresses the native
      // menu here anyway.
      e.preventDefault();
    }
  };

  // Radix also opens on a touch/pen long-press timed from pointerdown, which
  // never passes through handleContextMenu.
  const handlePointerDown = (e: React.PointerEvent) => {
    if (e.pointerType === "touch" || e.pointerType === "pen") resolveTarget(e);
  };

  const isHidden = target?.hidden === 1;

  return (
    <ContextMenu>
      <ContextMenuTrigger
        asChild
        onContextMenu={handleContextMenu}
        onPointerDown={handlePointerDown}
      >
        {children}
      </ContextMenuTrigger>
      {target && (
        <ContextMenuContent
          onCloseAutoFocus={(e) => {
            const editId = editOnCloseRef.current;
            if (editId === null) return;
            editOnCloseRef.current = null;
            e.preventDefault();
            segmentList?.startEdit(editId);
          }}
        >
          {!readOnly && (
            <ContextMenuItem
              onClick={() => {
                editOnCloseRef.current = target.id;
              }}
            >
              <Pencil />
              Edit
            </ContextMenuItem>
          )}
          <ContextMenuItem
            onClick={() => navigator.clipboard.writeText(target.text)}
          >
            <Copy />
            Copy
          </ContextMenuItem>
          <ContextMenuItem
            onClick={() =>
              window.dispatchEvent(
                new CustomEvent("yapstack:insert-segment-ref", {
                  detail: {
                    segmentId: target.id,
                    timestamp: formatTime(
                      Math.max(0, target.audio_offset_seconds),
                    ),
                    offsetSeconds: target.audio_offset_seconds,
                  },
                }),
              )
            }
          >
            <BookmarkPlus />
            Insert into Notes
          </ContextMenuItem>
          {!readOnly && (
            <>
              <ContextMenuItem
                onClick={() => {
                  toggleSegmentHidden(target.id);
                  trackSegmentHidden();
                }}
              >
                {isHidden ? <Eye /> : <EyeOff />}
                {isHidden ? "Unhide" : "Hide"}
              </ContextMenuItem>
              <ContextMenuSeparator />
              <ContextMenuItem
                className="text-destructive"
                onClick={() => deleteSegment(target.id)}
              >
                <Trash2 />
                Delete
              </ContextMenuItem>
            </>
          )}
        </ContextMenuContent>
      )}
    </ContextMenu>
  );
}
