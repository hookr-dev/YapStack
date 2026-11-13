import { memo, useMemo, useState } from "react";
import { EditableSegment } from "@/components/EditableSegment";
import { useAppStore } from "@/stores/appStore";
import type { DbSegment } from "@/lib/db";
import { Pencil } from "lucide-react";
import { Input } from "@/components/ui/input";
import {
  resolveSpeakerLabel,
  speakerKeyOf,
  speakersFromNames,
} from "@/lib/speakers";
import type { TranscriptRow } from "@/components/transcript/rows";

const SPEAKER_COLORS = [
  "bg-teal-500/10 text-teal-700 dark:text-teal-300 border-teal-500/30",
  "bg-rose-500/10 text-rose-700 dark:text-rose-300 border-rose-500/30",
  "bg-amber-500/10 text-amber-700 dark:text-amber-300 border-amber-500/30",
  "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300 border-emerald-500/30",
] as const;

function speakerColor(speakerId: number): string {
  return SPEAKER_COLORS[speakerId % SPEAKER_COLORS.length];
}

interface SpeakerHeaderProps {
  sessionId: string;
  segment: DbSegment;
  speakerId: number;
}

export function SpeakerHeader({ sessionId, segment, speakerId }: SpeakerHeaderProps) {
  const setSpeakerName = useAppStore((s) => s.setSpeakerName);
  const speakerNames = useAppStore((s) => s.settings.speakerNames[sessionId]);
  const speakers = useMemo(() => speakersFromNames(speakerNames), [speakerNames]);
  const speakerKey = speakerKeyOf(segment);
  const customName = speakers[speakerKey]?.display_name ?? undefined;
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(customName ?? "");

  const display = resolveSpeakerLabel(segment, speakers, "human");

  const commit = () => {
    setSpeakerName(sessionId, speakerKey, draft);
    setEditing(false);
  };

  if (editing) {
    return (
      <div className="flex items-center gap-1.5 pt-2">
        <Input
          autoFocus
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          // Keep the native text menu: stop the event before the list's
          // shared segment context menu (and main.tsx's window-level
          // suppressor) sees it.
          onContextMenu={(e) => e.stopPropagation()}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
            if (e.key === "Escape") {
              setDraft(customName ?? "");
              setEditing(false);
            }
          }}
          placeholder={resolveSpeakerLabel(segment, undefined, "human")}
          className="h-6 max-w-[140px] px-2 text-[11px]"
        />
      </div>
    );
  }

  return (
    <button
      type="button"
      onClick={() => {
        setDraft(customName ?? "");
        setEditing(true);
      }}
      className={`group flex items-center gap-1 rounded-md border px-2 py-0.5 text-[10px] font-medium ${speakerColor(
        speakerId,
      )} hover:opacity-90`}
    >
      <span>{display}</span>
      <Pencil className="h-2.5 w-2.5 opacity-0 group-hover:opacity-70" />
    </button>
  );
}

export function BackfillDivider() {
  return (
    <div className="flex items-center gap-4 py-2 text-muted-foreground">
      <div className="h-px flex-1 border-t border-dashed" />
      <span className="relative flex h-2 w-2">
        <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary opacity-75" />
        <span className="relative inline-flex h-2 w-2 rounded-full bg-primary" />
      </span>
      <span className="text-[11px] font-medium">Processing prior audio</span>
      <div className="h-px flex-1 border-t border-dashed" />
    </div>
  );
}

interface TranscriptRowViewProps {
  row: TranscriptRow;
  sessionId: string;
  isEditable: boolean;
  isActive: boolean;
  isSelected: boolean;
  selectionActive: boolean;
  onTimestampClick?: (time: number) => void;
}

// Row objects are rebuilt whenever the segment list changes, so rows compare
// by content: same key and kind over the same segment object.
function sameRow(a: TranscriptRow, b: TranscriptRow): boolean {
  if (a === b) return true;
  if (a.key !== b.key || a.kind !== b.kind) return false;
  if (a.kind === "divider" || b.kind === "divider") return true;
  if (a.segment !== b.segment) return false;
  return a.kind !== "speaker" || b.kind !== "speaker" || a.speakerId === b.speakerId;
}

function samePropsAsBefore(
  prev: TranscriptRowViewProps,
  next: TranscriptRowViewProps,
): boolean {
  return (
    sameRow(prev.row, next.row) &&
    prev.sessionId === next.sessionId &&
    prev.isEditable === next.isEditable &&
    prev.isActive === next.isActive &&
    prev.isSelected === next.isSelected &&
    prev.selectionActive === next.selectionActive &&
    prev.onTimestampClick === next.onTimestampClick
  );
}

// Memoized so a parent re-render (an append, a playback tick, marquee
// bookkeeping) only reaches rows whose own props changed.
export const TranscriptRowView = memo(function TranscriptRowView({
  row,
  sessionId,
  isEditable,
  isActive,
  isSelected,
  selectionActive,
  onTimestampClick,
}: TranscriptRowViewProps) {
  switch (row.kind) {
    case "divider":
      return <BackfillDivider />;
    case "speaker":
      return (
        <SpeakerHeader
          sessionId={sessionId}
          segment={row.segment}
          speakerId={row.speakerId}
        />
      );
    case "segment":
      return (
        <EditableSegment
          segment={row.segment}
          isActive={isActive}
          isSelected={isSelected}
          selectionActive={selectionActive}
          readOnly={!isEditable}
          onTimestampClick={onTimestampClick}
        />
      );
  }
}, samePropsAsBefore);
