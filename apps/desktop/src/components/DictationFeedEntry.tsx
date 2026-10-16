import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import {
  Bot,
  Play,
  Pause,
  Trash2,
  Copy,
  FileText,
  FolderSearch,
  Loader2,
} from "lucide-react";
import { useDictationEntry } from "@/hooks/useDictationEntry";
import { deriveFetchGlyph } from "@/lib/sync";
import type { DbDictationHistory } from "@/lib/db";

function formatTime12h(dateStr: string): string {
  const date = new Date(dateStr + "Z");
  return date.toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
    hour12: true,
  });
}

function formatDurationCompact(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const mins = Math.floor(seconds / 60);
  const secs = Math.round(seconds % 60);
  return `${mins}:${secs.toString().padStart(2, "0")}`;
}

export function DictationFeedEntry({
  entry,
  highlighted = false,
}: {
  entry: DbDictationHistory;
  highlighted?: boolean;
}) {
  const {
    playing,
    fetchState,
    handleCopy,
    handlePlayAudio,
    handleMoveToNote,
    handleOpenNote,
    handleDelete,
    handleShowFile,
  } = useDictationEntry(entry);
  // The hook has always tracked this entry's sync-fetch state; nothing rendered it, so a
  // row whose audio lives on another device showed a plain Play button that did nothing
  // visible for as long as the download took.
  const glyph = deriveFetchGlyph(fetchState);

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <div
          data-dictation-id={entry.id}
          className={`px-4 py-3 border-b border-border/30 hover:bg-accent/30 transition-colors cursor-default ${
            highlighted ? "bg-primary/10 ring-1 ring-primary/40" : ""
          }`}
        >
          {/* Metadata line */}
          <div className="flex items-center gap-1.5">
            <span className="text-[11px] text-muted-foreground tabular-nums">
              {formatTime12h(entry.created_at)}
            </span>
            <span className="text-muted-foreground/40">·</span>
            <span className="rounded-full bg-muted px-1.5 py-0.5 text-[10px] font-medium truncate max-w-24">
              {entry.slot_name}
            </span>
            {entry.ai_enabled === 1 && (
              <>
                <span className="text-muted-foreground/40">·</span>
                <span className="inline-flex items-center gap-0.5 rounded-full bg-amber-500/10 px-1.5 py-0.5 text-[10px] font-medium text-amber-600 dark:text-amber-400">
                  <Bot className="h-2.5 w-2.5" />
                  AI
                </span>
              </>
            )}
            {entry.session_id && (
              <>
                <span className="text-muted-foreground/40">·</span>
                <button
                  onClick={handleOpenNote}
                  className="inline-flex items-center gap-0.5 rounded-full bg-blue-500/10 px-1.5 py-0.5 text-[10px] font-medium text-blue-500 hover:bg-blue-500/20 transition-colors"
                >
                  <FileText className="h-2.5 w-2.5" />
                  Note
                </button>
              </>
            )}
            <div className="flex-1" />
            {entry.wav_duration_seconds != null && (
              <span className="text-[11px] text-muted-foreground tabular-nums mr-0.5">
                {formatDurationCompact(entry.wav_duration_seconds)}
              </span>
            )}
            {entry.wav_file_path && (
              <>
                <button
                  onClick={handlePlayAudio}
                  disabled={glyph.kind === "busy"}
                  title={glyph.kind === "idle" ? undefined : glyph.label}
                  className={`rounded p-1 transition-colors ${
                    glyph.kind === "busy"
                      ? "text-muted-foreground"
                      : glyph.kind === "blocked"
                        ? "text-amber-600 hover:bg-accent dark:text-amber-400"
                        : playing
                          ? "text-primary hover:bg-accent hover:text-primary/80"
                          : "text-muted-foreground hover:bg-accent hover:text-foreground"
                  }`}
                  aria-label={
                    glyph.kind === "busy"
                      ? glyph.label
                      : playing
                        ? "Pause audio"
                        : "Play audio"
                  }
                >
                  {/* Same 12px box in every state, so the row never re-flows. */}
                  {glyph.kind === "busy" ? (
                    <Loader2 className="h-3 w-3 animate-spin" />
                  ) : playing ? (
                    <Pause className="h-3 w-3" />
                  ) : (
                    <Play className="h-3 w-3" />
                  )}
                </button>
                <button
                  onClick={handleShowFile}
                  className="rounded p-1 text-muted-foreground hover:text-foreground hover:bg-accent transition-colors"
                  aria-label="Show file in Finder"
                >
                  <FolderSearch className="h-3 w-3" />
                </button>
              </>
            )}
            {entry.session_id ? (
              <button
                onClick={handleOpenNote}
                className="rounded p-1 text-blue-500 hover:text-blue-400 hover:bg-accent transition-colors"
                aria-label="Open note"
              >
                <FileText className="h-3 w-3" />
              </button>
            ) : (
              <button
                onClick={handleMoveToNote}
                className="rounded p-1 text-muted-foreground hover:text-foreground hover:bg-accent transition-colors"
                aria-label="Move to note"
              >
                <FileText className="h-3 w-3" />
              </button>
            )}
            <button
              onClick={handleCopy}
              className="rounded p-1 text-muted-foreground hover:text-foreground hover:bg-accent transition-colors"
              aria-label="Copy"
            >
              <Copy className="h-3 w-3" />
            </button>
            <button
              onClick={handleDelete}
              className="rounded p-1 text-muted-foreground hover:text-destructive hover:bg-accent transition-colors"
              aria-label="Delete"
            >
              <Trash2 className="h-3 w-3" />
            </button>
          </div>

          {/* Text body — full text, no truncation */}
          <p className="mt-1.5 text-xs leading-relaxed text-foreground">
            {entry.output_text}
          </p>
        </div>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextMenuItem onClick={handleCopy}>
          <Copy className="h-3.5 w-3.5 mr-2" />
          Copy Text
        </ContextMenuItem>
        {entry.wav_file_path && (
          <>
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
            <ContextMenuItem onClick={handleShowFile}>
              <FolderSearch className="h-3.5 w-3.5 mr-2" />
              Show audio file
            </ContextMenuItem>
          </>
        )}
        {entry.session_id ? (
          <ContextMenuItem onClick={handleOpenNote}>
            <FileText className="h-3.5 w-3.5 mr-2" />
            Open Note
          </ContextMenuItem>
        ) : (
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
