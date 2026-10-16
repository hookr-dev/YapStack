import { useState, useEffect, useMemo } from "react";
import { useAppStore } from "@/stores/appStore";
import { SortableContext, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import {
  Plus,
  Settings,
  Pin,
  FolderPlus,
  Inbox,
  PenLine,
  ChevronDown,
  ChevronRight,
  Loader2,
} from "lucide-react";
import { useCreateSession } from "@/hooks/useCreateSession";
import { RecordingBeacon } from "@/components/RecordingBeacon";
import { FolderItem } from "@/components/FolderItem";
import { FolderDialog } from "@/components/FolderDialog";
import type { FolderDialogData } from "@/components/FolderDialog";
import { DictationTrayItem } from "@/components/DictationTrayItem";
import { cn, formatShortcutDisplay } from "@/lib/utils";
import { getBinding } from "@/lib/shortcuts";
import { YapStackIcon } from "@/components/YapStackIcon";
import { BackfillDropdown } from "@/components/BackfillDropdown";
import { UpdateBanner } from "@/components/UpdateBanner";
import { SyncStatusGlyph } from "@/components/SyncStatusGlyph";

export function AppSidebar() {
  const navigateTo = useAppStore((s) => s.navigateTo);
  const folderTree = useAppStore((s) => s.folderTree);
  const listFilter = useAppStore((s) => s.listFilter);
  const setListFilter = useAppStore((s) => s.setListFilter);
  const createFolder = useAppStore((s) => s.createFolder);
  const createManualNote = useAppStore((s) => s.createManualNote);
  const availableSeconds = useAppStore((s) =>
    Math.max(
      s.bufferInfo?.mic?.available_seconds ?? 0,
      s.bufferInfo?.system?.available_seconds ?? 0,
    ),
  );
  const shortcutBindings = useAppStore((s) => s.settings.shortcutBindings);
  const dictationEnabled = useAppStore((s) => s.settings.dictation.enabled);
  const dictationHistory = useAppStore((s) => s.dictationHistory);
  const { canCreate, creatingSession, handleNew } = useCreateSession();

  const [dictationOpen, setDictationOpen] = useState(true);
  const recentDictations = useMemo(() => dictationHistory.slice(0, 5), [dictationHistory]);

  const [newFolderOpen, setNewFolderOpen] = useState(false);

  useEffect(() => {
    const handler = () => setNewFolderOpen(true);
    window.addEventListener("yapstack:open-new-folder-dialog", handler);
    return () => window.removeEventListener("yapstack:open-new-folder-dialog", handler);
  }, []);

  const handleCreateFolder = (data: FolderDialogData) => {
    createFolder(data.name, null, data.icon, data.color, data.description);
  };

  return (
    <div className="flex h-full flex-col bg-sidebar text-sidebar-foreground">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-sidebar-border pl-4 pr-2 py-2">
        <button
          className="cursor-pointer opacity-80 hover:opacity-100 transition-opacity"
          onClick={() => {
            setListFilter({ type: "all" });
            navigateTo("note-list");
          }}
          aria-label="All Sessions"
        >
          <YapStackIcon className="h-6 w-6 text-sidebar-foreground" />
        </button>
        <div className="flex items-center gap-0.5">
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon-xs"
                disabled={!canCreate}
                onClick={() => handleNew()}
              >
                {creatingSession ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                ) : (
                  <Plus className="h-3.5 w-3.5" />
                )}
              </Button>
            </TooltipTrigger>
            <TooltipContent>
              {creatingSession ? "Starting session…" : "New session"}
            </TooltipContent>
          </Tooltip>
          <BackfillDropdown
            availableSeconds={availableSeconds}
            canCreate={canCreate}
            onBackfill={handleNew}
          />
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon-xs"
                onClick={() => createManualNote()}
              >
                <PenLine className="h-3.5 w-3.5" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>New note ({formatShortcutDisplay(getBinding("new-note", shortcutBindings))})</TooltipContent>
          </Tooltip>
        </div>
      </div>

      {/* Navigation */}
      <div className="px-2 pt-2 space-y-1.5">
        <RecordingBeacon />
        <div className="space-y-0.5">
        <button
          className={cn(
            "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-xs transition-all duration-150",
            listFilter.type === "all"
              ? "bg-sidebar-accent text-sidebar-accent-foreground"
              : "text-muted-foreground hover:bg-sidebar-accent/50 hover:text-foreground",
          )}
          onClick={() => {
            setListFilter({ type: "all" });
            navigateTo("note-list");
          }}
        >
          <Inbox className="h-3.5 w-3.5 shrink-0" />
          All Sessions
        </button>
        <button
          className={cn(
            "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-xs transition-all duration-150",
            listFilter.type === "pinned"
              ? "bg-sidebar-accent text-sidebar-accent-foreground"
              : "text-muted-foreground hover:bg-sidebar-accent/50 hover:text-foreground",
          )}
          onClick={() => {
            setListFilter({ type: "pinned" });
            navigateTo("note-list");
          }}
        >
          <Pin className="h-3.5 w-3.5 shrink-0" />
          Pinned
        </button>
        </div>
      </div>

      <Separator className="my-1.5" />

      {/* Folders */}
      <div className="flex items-center justify-between pl-4 pr-2 py-1">
        <span className="text-[11px] font-medium uppercase text-muted-foreground">
          Folders
        </span>
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              variant="ghost"
              size="icon-xs"
              onClick={() => setNewFolderOpen(true)}
            >
              <FolderPlus className="h-3.5 w-3.5" />
            </Button>
          </TooltipTrigger>
          <TooltipContent>New folder</TooltipContent>
        </Tooltip>
      </div>

      <ScrollArea className="min-h-0 flex-1 px-2">
        <div className="space-y-0.5">
          <SortableContext items={folderTree.map(n => `folder-${n.folder.id}`)} strategy={verticalListSortingStrategy}>
            {folderTree.map((node) => (
              <FolderItem key={node.folder.id} folder={node.folder} childNodes={node.children} depth={0} />
            ))}
          </SortableContext>
          {folderTree.length === 0 && (
            <p className="px-2 py-2 text-center text-[11px] text-muted-foreground">
              No folders
            </p>
          )}
        </div>
      </ScrollArea>

      {/* Dictation tray */}
      {dictationEnabled && recentDictations.length > 0 && (
        <>
          <Separator className="my-1.5" />
          <Collapsible open={dictationOpen} onOpenChange={setDictationOpen}>
            <div className="flex items-center justify-between pl-4 pr-2 pb-1">
              <CollapsibleTrigger className="-ml-0.5 flex items-center gap-0.5 text-[11px] font-medium uppercase text-muted-foreground hover:text-foreground transition-colors">
                {dictationOpen ? (
                  <ChevronDown className="h-2.5 w-2.5" />
                ) : (
                  <ChevronRight className="h-2.5 w-2.5" />
                )}
                Dictation
              </CollapsibleTrigger>
              <Button
                variant="inline"
                size="inline"
                onClick={() => {
                  setListFilter({ type: "dictation" });
                  navigateTo("note-list");
                }}
              >
                View all &rarr;
              </Button>
            </div>
            <CollapsibleContent>
              <div className="px-2 pb-1 space-y-0.5">
                {recentDictations.map((entry) => (
                  <DictationTrayItem key={entry.id} entry={entry} />
                ))}
              </div>
            </CollapsibleContent>
          </Collapsible>
        </>
      )}

      {/* Footer */}
      <div className="border-t border-sidebar-border px-2 py-2">
        <UpdateBanner />
        <div className="flex items-center gap-1">
          <button
            className="flex flex-1 min-w-0 items-center gap-2 rounded-md px-2 py-1.5 text-xs text-muted-foreground hover:bg-sidebar-accent/50 hover:text-foreground transition-all duration-150"
            onClick={() => navigateTo("settings")}
            aria-label="Settings"
          >
            <Settings className="h-3.5 w-3.5 shrink-0" />
            Settings
          </button>
          <SyncStatusGlyph />
        </div>
      </div>

      {/* New folder dialog */}
      <FolderDialog
        open={newFolderOpen}
        onOpenChange={setNewFolderOpen}
        mode="create"
        onSubmit={handleCreateFolder}
      />
    </div>
  );
}
