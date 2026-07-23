import { useMemo } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { Copy, Eye, EyeOff, Trash2, X } from "lucide-react";
import { toast } from "sonner";
import type { DbSegment } from "@/lib/db";
import { segmentsToAttributedMarkdown } from "@/lib/export";
import { trackTranscriptExported } from "@/lib/analytics";

export function BulkActionsBar({
  segments,
  readOnly,
}: {
  segments: DbSegment[];
  readOnly: boolean;
}) {
  const selectedIds = useAppStore((s) => s.selectedSegmentIds);
  const clearSegmentSelection = useAppStore((s) => s.clearSegmentSelection);
  const deleteSegments = useAppStore((s) => s.deleteSegments);
  const setSegmentsHidden = useAppStore((s) => s.setSegmentsHidden);

  const selected = useMemo(
    () => segments.filter((s) => selectedIds.has(s.id)),
    [segments, selectedIds],
  );

  if (selected.length === 0) return null;

  const anyVisible = selected.some((s) => s.hidden === 0);
  const ids = selected.map((s) => s.id);

  const handleCopy = async () => {
    const text = segmentsToAttributedMarkdown(selected);
    if (text.length === 0) {
      toast.info("Nothing to copy");
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
      toast.success(`Copied ${selected.length} segments`);
      trackTranscriptExported({
        scope: "selection",
        mechanism: "clipboard",
        kind: "full",
        segments: selected.length,
      });
    } catch {
      toast.error("Clipboard copy failed");
    }
  };

  const handleDelete = async () => {
    await deleteSegments(ids);
    toast.success(`Deleted ${ids.length} segments`);
  };

  const handleHideToggle = async () => {
    await setSegmentsHidden(ids, anyVisible);
  };

  return (
    <div className="pointer-events-none absolute inset-x-0 bottom-3 z-20 flex justify-center">
      <div
        className="pointer-events-auto flex items-center gap-1 rounded-full border bg-background/95 px-2 py-1 shadow-lg backdrop-blur"
        title="AI chat is scoped to the selected segments"
      >
        <Button
          size="sm"
          variant="ghost"
          className="h-7 gap-1.5 text-xs"
          onClick={handleCopy}
        >
          <Copy className="h-3.5 w-3.5" />
          Copy
        </Button>
        {!readOnly && (
          <Button
            size="sm"
            variant="ghost"
            className="h-7 gap-1.5 text-xs"
            onClick={handleHideToggle}
          >
            {anyVisible ? (
              <EyeOff className="h-3.5 w-3.5" />
            ) : (
              <Eye className="h-3.5 w-3.5" />
            )}
            {anyVisible ? "Hide" : "Unhide"}
          </Button>
        )}
        {!readOnly && (
          <Button
            size="sm"
            variant="ghost"
            className="h-7 gap-1.5 text-xs text-destructive hover:text-destructive"
            onClick={handleDelete}
          >
            <Trash2 className="h-3.5 w-3.5" />
            Delete
          </Button>
        )}
        <div className="mx-1 h-4 w-px bg-border" />
        <Button
          size="sm"
          variant="ghost"
          className="h-7 w-7 p-0"
          onClick={clearSegmentSelection}
          aria-label="Clear selection"
        >
          <X className="h-3.5 w-3.5" />
        </Button>
      </div>
    </div>
  );
}
