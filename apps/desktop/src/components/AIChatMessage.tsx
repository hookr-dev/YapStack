import ReactMarkdown, { type Components } from "react-markdown";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Copy, Check, BookmarkPlus, Quote } from "lucide-react";
import { memo, useMemo, useState } from "react";
import type { ChatMessage } from "@/lib/ai";
import type { DbSegment } from "@/lib/db";
import { getAction } from "@/lib/ai-actions";
import { ToolExecutionBlock } from "@/components/ToolExecutionBlock";
import { formatTime } from "@/lib/utils";
import { trimPartialCitation } from "@/lib/citations/parse";
import { createSegmentIndex, findSegment } from "@/lib/citations/resolve";
import { rewriteCitations, toPlainLabel } from "@/lib/citations/render";
import type { TranscriptRef } from "@/lib/citations/types";

interface ToolBadge {
  tool: string;
  detail: string;
}

interface AIChatMessageProps {
  message: ChatMessage;
  onSaveToNotes?: (content: string) => void;
  segments?: DbSegment[];
  onCitationClick?: (segmentId: string) => void;
}

const TOOL_LABELS: Record<string, string> = {
  update_title: "Title updated",
  save_to_notes: "Notes saved",
  pin_session: "Pin toggled",
  tag_session: "Tags updated",
  add_session_to_folder: "Classified",
  search_folders: "Folders searched",
  search_sessions: "Sessions searched",
  get_session_context: "Sessions expanded",
  search_dictations: "Dictations searched",
  replace_in_transcript: "Transcript edited",
};

function parseToolBadges(content: string): {
  badges: ToolBadge[];
  text: string;
} {
  const lines = content.split("\n");
  const badges: ToolBadge[] = [];
  let textStart = 0;

  for (const line of lines) {
    const match = line.match(/^\[tool:(\w+)\]\s*(.*)$/);
    if (match) {
      badges.push({ tool: match[1], detail: match[2] });
      textStart++;
    } else {
      break;
    }
  }

  return {
    badges,
    text: lines.slice(textStart).join("\n").trimStart(),
  };
}

// Resolved citations are rewritten into markdown links with this href so
// react-markdown places them in every inline context (bold, headings, table
// cells), and the `a` override below turns them into chips.
const CITATION_HREF_PREFIX = "#yapstack-citation:";

function toCitationLink(ref: TranscriptRef): string {
  return `[${toPlainLabel(ref)}](${CITATION_HREF_PREFIX}${ref.segmentId})`;
}

function CitationChip({
  segment,
  onCitationClick,
}: {
  segment: DbSegment;
  onCitationClick: ((segmentId: string) => void) | undefined;
}) {
  const ts = formatTime(segment.audio_offset_seconds);
  const preview =
    segment.text.length > 80 ? segment.text.slice(0, 80) + "..." : segment.text;
  const chipClass =
    "inline-flex items-center gap-0.5 rounded bg-accent px-1 py-0.5 text-[10px] font-medium text-accent-foreground align-baseline";

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        {onCitationClick ? (
          <button
            type="button"
            onClick={() => onCitationClick(segment.id)}
            className={`${chipClass} hover:bg-accent/80 cursor-pointer`}
          >
            <Quote className="h-2.5 w-2.5" />
            {ts}
          </button>
        ) : (
          <span className={chipClass}>
            <Quote className="h-2.5 w-2.5" />
            {ts}
          </span>
        )}
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-xs text-xs">
        {preview}
      </TooltipContent>
    </Tooltip>
  );
}

export const AIChatMessage = memo(function AIChatMessage({
  message,
  onSaveToNotes,
  segments,
  onCitationClick,
}: AIChatMessageProps) {
  const [copied, setCopied] = useState(false);
  const index = useMemo(
    () => (segments ? createSegmentIndex(segments) : null),
    [segments],
  );
  const markdownComponents = useMemo<Components>(
    () => ({
      a: ({ href, children, node: _node, ...props }) => {
        if (index && href?.startsWith(CITATION_HREF_PREFIX)) {
          const segment = findSegment(
            index,
            href.slice(CITATION_HREF_PREFIX.length),
          );
          if (segment) {
            return (
              <CitationChip
                segment={segment}
                onCitationClick={onCitationClick}
              />
            );
          }
        }
        return (
          <a href={href} {...props}>
            {children}
          </a>
        );
      },
    }),
    [index, onCitationClick],
  );

  const isUser = message.role === "user";

  function handleCopy() {
    navigator.clipboard.writeText(message.content);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }

  if (isUser) {
    const actionDef = message.action ? getAction(message.action) : undefined;

    if (actionDef) {
      const Icon = actionDef.icon;
      return (
        <div className="flex justify-end">
          <div className="inline-flex items-center gap-1.5 rounded-full bg-primary/10 border border-primary/20 px-3 py-1.5">
            <Icon className="h-3 w-3 text-primary" />
            <span className="text-xs font-medium text-primary">
              {actionDef.label}
            </span>
          </div>
        </div>
      );
    }

    return (
      <div className="flex justify-end">
        <div className="max-w-[85%] rounded-2xl rounded-br-md bg-primary text-primary-foreground px-3 py-2">
          <p className="text-xs whitespace-pre-wrap">{message.content}</p>
        </div>
      </div>
    );
  }

  const hasLiveExecs = message.toolExecutions && message.toolExecutions.length > 0;
  const { badges, text } = parseToolBadges(message.content);
  // Without a transcript in scope (folder or multi-session chat) markers can't
  // resolve, so they're stripped rather than printed raw. While streaming, a
  // marker that hasn't closed yet is held back until it can be resolved.
  const displayText = rewriteCitations(
    message.isStreaming ? trimPartialCitation(text) : text,
    index,
    toCitationLink,
  ).text;

  const toolExecsFromBadges = !hasLiveExecs && badges.length > 0
    ? badges.map((b) => ({
        name: b.tool,
        label: TOOL_LABELS[b.tool] ?? b.tool,
        detail: b.detail || undefined,
        status: "done" as const,
      }))
    : undefined;

  const toolExecs = message.toolExecutions ?? toolExecsFromBadges;

  return (
    <div className="flex flex-col gap-1">
      {toolExecs && toolExecs.length > 0 && (
        <div className="max-w-[95%] rounded-xl bg-muted/40 border border-border/30 px-2.5 py-1.5">
          <ToolExecutionBlock executions={toolExecs} />
        </div>
      )}
      {(text.trim() || message.isStreaming) && (
        <div className="max-w-[95%] rounded-2xl rounded-bl-md bg-muted/80 border border-border/50 px-3 py-2 text-xs ai-chat-markdown">
          <ReactMarkdown components={markdownComponents}>
            {displayText}
          </ReactMarkdown>
          {message.isStreaming && (
            <span className="inline-block w-1.5 h-4 bg-foreground/60 animate-pulse ml-0.5 align-text-bottom" />
          )}
        </div>
      )}
      {!message.isStreaming && text.trim() && (
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="icon-xs"
            onClick={handleCopy}
            className="h-6 w-6 text-muted-foreground"
          >
            {copied ? (
              <Check className="h-3 w-3" />
            ) : (
              <Copy className="h-3 w-3" />
            )}
          </Button>
          {onSaveToNotes && (
            <Button
              variant="ghost"
              size="icon-xs"
              onClick={() => onSaveToNotes(message.content)}
              className="h-6 w-6 text-muted-foreground"
            >
              <BookmarkPlus className="h-3 w-3" />
            </Button>
          )}
        </div>
      )}
    </div>
  );
});
