import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { Play, Pause, Copy, FileText, Trash2, Loader2 } from "lucide-react";
import { useDictationEntry } from "@/hooks/useDictationEntry";
import { deriveFetchGlyph } from "@/lib/sync";
import { formatRelativeTime } from "@/lib/utils";
import type { DbDictationHistory } from "@/lib/db";

export function DictationTrayItem({ entry }: { entry: DbDictationHistory }) {
  const {
    playing,
    fetchState,
    handleCopy,
    handlePlayAudio,
    handleMoveToNote,
    handleDelete,
  } = useDictationEntry(entry);
  // Same decision as the feed row: while a sync fetch is genuinely running the action is
  // busy, not broken (it used to read as an inert "Play Audio" that did nothing).
  const glyph = deriveFetchGlyph(fetchState);

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <button
          className="flex w-full items-center gap-2 rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-sidebar-accent/50 cursor-default"
          onClick={handleCopy}
          title={entry.output_text}
        >
          <span className="flex-1 truncate text-left">
            {entry.output_text}
          </span>
          <span className="shrink-0 text-[10px] tabular-nums opacity-60">
            {formatRelativeTime(entry.created_at)}
          </span>
        </button>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextMenuItem onClick={handleCopy}>
          <Copy className="h-3.5 w-3.5 mr-2" />
          Copy Text
        </ContextMenuItem>
        {entry.wav_file_path && (
          <ContextMenuItem
            onClick={handlePlayAudio}
            disabled={glyph.kind === "busy"}
          >
            {glyph.kind === "busy" ? (
              <Loader2 className="h-3.5 w-3.5 mr-2 animate-spin" />
            ) : playing ? (
              <Pause className="h-3.5 w-3.5 mr-2" />
            ) : (
              <Play className="h-3.5 w-3.5 mr-2" />
            )}
            {glyph.kind === "busy"
              ? glyph.label
              : playing
                ? "Pause Audio"
                : "Play Audio"}
          </ContextMenuItem>
        )}
        {!entry.session_id && (
          <ContextMenuItem onClick={handleMoveToNote}>
            <FileText className="h-3.5 w-3.5 mr-2" />
            Move to Note
          </ContextMenuItem>
        )}
        <ContextMenuItem onClick={handleDelete} className="text-destructive">
          <Trash2 className="h-3.5 w-3.5 mr-2" />
          Delete
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  );
}
