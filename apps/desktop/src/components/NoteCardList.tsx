import { useMemo } from "react";
import { useAppStore } from "@/stores/appStore";
import { useScrollPositionMemory } from "@/hooks/useScrollPositionMemory";
import { ScrollArea } from "@/components/ui/scroll-area";
import { NoteCard } from "@/components/NoteCard";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu";
import { Plus, Rewind, ChevronRight, Inbox, Folder, Pin } from "lucide-react";
import { useCreateSession } from "@/hooks/useCreateSession";
import { groupSessionsByDay } from "@/lib/utils";
import { getFolderPath, getDescendantIds } from "@/lib/folder-tree";
import { ICON_MAP } from "@/lib/folder-constants";
import { DictationHistoryList } from "@/components/DictationHistoryList";
import { BACKFILL_STEPS_SECONDS, formatBackfillSeconds } from "@/lib/backfill";

export function NoteCardList() {
  const sessions = useAppStore((s) => s.sessions);
  const listFilter = useAppStore((s) => s.listFilter);
  const setListFilter = useAppStore((s) => s.setListFilter);
  const sessionFolderMap = useAppStore((s) => s.sessionFolderMap);
  const folders = useAppStore((s) => s.folders);
  const folderChildMap = useAppStore((s) => s.folderChildMap);
  const availableSeconds = useAppStore((s) =>
    Math.max(
      s.bufferInfo?.mic?.available_seconds ?? 0,
      s.bufferInfo?.system?.available_seconds ?? 0,
    ),
  );
  const { canCreate, handleNew } = useCreateSession();

  const breadcrumbs = useMemo(() => {
    if (listFilter.type !== "folder" || !listFilter.folderId) return null;
    return getFolderPath(folders, listFilter.folderId);
  }, [listFilter, folders]);

  const visibleFolders = useMemo(() => {
    if (listFilter.type === "folder" && listFilter.folderId) {
      return folders.filter((f) => f.parent_id === listFilter.folderId);
    }
    if (listFilter.type === "all") {
      return folders.filter((f) => !f.parent_id);
    }
    return [];
  }, [listFilter, folders]);

  const pinnedCount = useMemo(
    () => sessions.filter((s) => s.is_pinned === 1).length,
    [sessions],
  );

  const showPinnedCard = listFilter.type === "all" && pinnedCount > 0;

  const sessionFolderCounts = useMemo(() => {
    if (visibleFolders.length === 0) return {};
    const counts: Record<string, number> = {};
    for (const f of visibleFolders) {
      const targetIds = new Set([f.id, ...getDescendantIds(folders, f.id, folderChildMap)]);
      counts[f.id] = Object.values(sessionFolderMap).filter((ids) =>
        ids.some((fId) => targetIds.has(fId)),
      ).length;
    }
    return counts;
  }, [visibleFolders, sessionFolderMap, folders, folderChildMap]);

  const filteredSessions = useMemo(() => {
    switch (listFilter.type) {
      case "pinned":
        return sessions.filter((s) => s.is_pinned === 1);
      case "folder": {
        const targetIds = new Set([
          listFilter.folderId!,
          ...getDescendantIds(folders, listFilter.folderId!, folderChildMap),
        ]);
        return sessions.filter(
          (s) =>
            (sessionFolderMap[s.id] ?? []).some((fId) => targetIds.has(fId)),
        );
      }
      default:
        return sessions;
    }
  }, [sessions, listFilter, sessionFolderMap, folders, folderChildMap]);

  const grouped = useMemo(
    () => groupSessionsByDay(filteredSessions),
    [filteredSessions],
  );

  // Each filter keeps its own scroll position across remounts.
  const scrollKey =
    listFilter.type === "folder"
      ? `folder:${listFilter.folderId ?? ""}`
      : listFilter.type;
  const scrollRef = useScrollPositionMemory<HTMLDivElement>(scrollKey);

  if (listFilter.type === "dictation") {
    return <DictationHistoryList />;
  }

  const breadcrumbBar = (
    <nav className="flex items-center border-b px-4 py-2 text-xs text-muted-foreground shrink-0">
      {/* Inner h-6 row mirrors the sidebar header's 24px content so the
          border-b on each side aligns to the same pixel. */}
      <div className="flex items-center gap-1 h-6">
        <button
          className={
            listFilter.type === "all"
              ? "font-medium text-foreground flex items-center gap-1"
              : "hover:text-foreground transition-colors flex items-center gap-1"
          }
          onClick={() => setListFilter({ type: "all" })}
        >
          <Inbox className="h-3 w-3" />
          All Sessions
        </button>
        {listFilter.type === "pinned" && (
          <>
            <ChevronRight className="h-3 w-3 shrink-0" />
            <span className="font-medium text-foreground flex items-center gap-1">
              <Pin className="h-3 w-3" />
              Pinned
            </span>
          </>
        )}
        {breadcrumbs?.map((crumb, i) => {
          const isLast = i === breadcrumbs.length - 1;
          const CrumbIcon = crumb.icon ? ICON_MAP[crumb.icon] : null;
          return (
            <span key={crumb.id} className="flex items-center gap-1">
              <ChevronRight className="h-3 w-3 shrink-0" />
              <button
                className={
                  isLast
                    ? "font-medium text-foreground flex items-center gap-1"
                    : "hover:text-foreground transition-colors flex items-center gap-1"
                }
                onClick={() =>
                  setListFilter({ type: "folder", folderId: crumb.id })
                }
              >
                {CrumbIcon && (
                  <CrumbIcon
                    className="h-3 w-3 shrink-0"
                    style={crumb.color ? { color: crumb.color } : undefined}
                  />
                )}
                {crumb.name}
              </button>
            </span>
          );
        })}
      </div>
    </nav>
  );

  const hasCards = visibleFolders.length > 0 || showPinnedCard;

  if (filteredSessions.length === 0 && !hasCards) {
    return (
      <div className="flex flex-1 flex-col min-h-0">
        {breadcrumbBar}
        <div className="flex flex-1 flex-col items-center justify-center gap-4 p-8 pb-20">
          <p className="text-center text-sm text-muted-foreground">
            {listFilter.type === "pinned"
              ? "No pinned notes"
              : listFilter.type === "folder"
                ? "No notes in this folder"
                : canCreate
                  ? "Start a new session to begin transcribing"
                  : "Waiting for engine and audio capture to be ready..."}
          </p>
          {canCreate && listFilter.type === "all" && (
            <div className="flex items-center gap-2">
              <Button onClick={() => handleNew()} size="sm">
                <Plus className="mr-2 h-4 w-4" />
                New Session
              </Button>
              {availableSeconds > 0 && (
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button size="sm" variant="outline">
                      <Rewind className="mr-2 h-4 w-4" />
                      Rewind ({formatBackfillSeconds(availableSeconds)})
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent>
                    <DropdownMenuItem onClick={() => handleNew(availableSeconds)}>
                      Full buffer ({formatBackfillSeconds(availableSeconds)})
                    </DropdownMenuItem>
                    {BACKFILL_STEPS_SECONDS.some((d) => d < availableSeconds) && (
                      <DropdownMenuSeparator />
                    )}
                    {BACKFILL_STEPS_SECONDS.filter((d) => d < availableSeconds).map((d) => (
                      <DropdownMenuItem key={d} onClick={() => handleNew(d)}>
                        Last {d}s
                      </DropdownMenuItem>
                    ))}
                  </DropdownMenuContent>
                </DropdownMenu>
              )}
            </div>
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 flex-col min-h-0">
      {breadcrumbBar}
      <ScrollArea ref={scrollRef} className="min-h-0 flex-1">
        <div className="space-y-1 px-3 pt-1 pb-28">
          {hasCards && (
            <div className="grid grid-cols-2 gap-1.5 mt-2">
              {showPinnedCard && (
                <button
                  className="flex items-center gap-2 rounded-md border bg-card/50 px-2.5 py-1.5 text-left transition-colors hover:bg-accent"
                  onClick={() => setListFilter({ type: "pinned" })}
                >
                  <Pin className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                  <span className="truncate text-xs font-medium flex-1">
                    Pinned
                  </span>
                  <span className="text-[10px] text-muted-foreground shrink-0">
                    {pinnedCount}
                  </span>
                </button>
              )}
              {visibleFolders.map((f) => {
                const FolderIcon = f.icon ? (ICON_MAP[f.icon] ?? Folder) : Folder;
                const count = sessionFolderCounts[f.id] ?? 0;
                return (
                  <button
                    key={f.id}
                    className="flex items-center gap-2 rounded-md border bg-card/50 px-2.5 py-1.5 text-left transition-colors hover:bg-accent"
                    onClick={() =>
                      setListFilter({ type: "folder", folderId: f.id })
                    }
                  >
                    <FolderIcon
                      className="h-3.5 w-3.5 shrink-0"
                      style={f.color ? { color: f.color } : undefined}
                    />
                    <span className="truncate text-xs font-medium flex-1">
                      {f.name}
                    </span>
                    {count > 0 && (
                      <span className="text-[10px] text-muted-foreground shrink-0">
                        {count}
                      </span>
                    )}
                  </button>
                );
              })}
            </div>
          )}
          {grouped.map((group) => (
            <div key={group.label}>
              <div className="sticky top-0 z-10 bg-background/80 backdrop-blur-sm px-1 py-1.5">
                <span className="text-[11px] font-medium text-muted-foreground">
                  {group.label}
                </span>
              </div>
              {group.sessions.map((session) => (
                <NoteCard key={session.id} session={session} />
              ))}
            </div>
          ))}
        </div>
      </ScrollArea>
    </div>
  );
}
