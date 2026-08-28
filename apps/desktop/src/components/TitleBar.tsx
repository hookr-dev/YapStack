import { useState, useEffect, useCallback } from "react";
import { useAppStore } from "@/stores/appStore";
import { useCreateSession } from "@/hooks/useCreateSession";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Button } from "@/components/ui/button";
import { formatShortcutDisplay, isMac } from "@/lib/utils";
import { getBinding } from "@/lib/shortcuts";
import { Search, PanelLeftClose, PanelLeftOpen, Plus, PenLine, Minus, Square, X, Loader2 } from "lucide-react";
import { SearchCommand } from "@/components/SearchCommand";
import { BackfillDropdown } from "@/components/BackfillDropdown";
import { StatusPopover } from "@/components/StatusPopover";
import { getCurrentWindow } from "@tauri-apps/api/window";

function StatusDot({ color, pulse }: { color: string; pulse?: boolean }) {
  return (
    <span
      className={`inline-block h-2 w-2 rounded-full ${color} ${pulse ? "animate-pulse" : ""}`}
      aria-hidden
    />
  );
}

export function TitleBar() {
  const enginePhase = useAppStore((s) => s.enginePhase);
  const engineError = useAppStore((s) => s.engineError);
  const captureStatus = useAppStore((s) => s.captureStatus);
  const modelDownloadProgress = useAppStore((s) => s.modelDownloadProgress);

  const sidebarCollapsed = useAppStore((s) => s.settings.sidebarCollapsed);
  const toggleSidebar = useAppStore((s) => s.toggleSidebar);

  const createManualNote = useAppStore((s) => s.createManualNote);

  const shortcutBindings = useAppStore((s) => s.settings.shortcutBindings);
  const availableSeconds = useAppStore((s) =>
    Math.max(
      s.bufferInfo?.mic?.available_seconds ?? 0,
      s.bufferInfo?.system?.available_seconds ?? 0,
    ),
  );
  const [searchOpen, setSearchOpen] = useState(false);
  const { canCreate, creatingSession, handleNew } = useCreateSession();

  const onCreateSession = useCallback(() => handleNew(), [handleNew]);

  useEffect(() => {
    window.addEventListener("yapstack:create-session", onCreateSession);
    return () => window.removeEventListener("yapstack:create-session", onCreateSession);
  }, [onCreateSession]);

  useEffect(() => {
    const handler = () => setSearchOpen(true);
    window.addEventListener("yapstack:toggle-search", handler);
    return () => window.removeEventListener("yapstack:toggle-search", handler);
  }, []);

  // Single source of truth for engine + capture status. The status dot in
  // this title bar is the only place these states should surface — no banners
  // in the main content area.
  let dotColor = "bg-muted-foreground";
  let statusText = "Idle";
  // Pulse during any transitional / not-yet-ready state so the dot doesn't
  // look idle while work is in progress. Steady when ready or errored.
  let dotPulse = false;

  if (enginePhase === "downloading") {
    dotColor = "bg-amber-500";
    const pct = modelDownloadProgress ?? 0;
    statusText = `Downloading model (${Math.round(pct)}%)`;
    dotPulse = true;
  } else if (enginePhase === "initializing") {
    dotColor = "bg-amber-500";
    statusText = "Loading engine...";
    dotPulse = true;
  } else if (enginePhase === "error") {
    dotColor = "bg-red-500";
    statusText = engineError ?? "Error";
  } else if (enginePhase === "ready") {
    if (captureStatus?.state === "Capturing") {
      dotColor = "bg-green-500";
      statusText = "Listening";
    } else if (captureStatus?.state === "Error") {
      dotColor = "bg-red-500";
      statusText = captureStatus.error_message ?? "Capture error";
    } else {
      dotColor = "bg-amber-500";
      statusText = "Engine ready";
    }
  } else {
    dotColor = "bg-muted-foreground";
    statusText = "Setting up...";
    dotPulse = true;
  }

  return (
    <>
      <div className="flex h-[38px] shrink-0 items-center border-b px-2" data-tauri-drag-region>
        {/* Traffic light padding (macOS only) */}
        {isMac && <div className="w-[78px] shrink-0" data-tauri-drag-region />}

        {/* Sidebar toggle */}
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              variant="ghost"
              size="icon-xs"
              onClick={toggleSidebar}
              className="shrink-0 text-muted-foreground"
            >
              {sidebarCollapsed ? (
                <PanelLeftOpen className="h-3.5 w-3.5" />
              ) : (
                <PanelLeftClose className="h-3.5 w-3.5" />
              )}
            </Button>
          </TooltipTrigger>
          <TooltipContent>{sidebarCollapsed ? "Show sidebar" : "Hide sidebar"} ({formatShortcutDisplay(getBinding("toggle-sidebar", shortcutBindings))})</TooltipContent>
        </Tooltip>

        {sidebarCollapsed && (
          <>
            <div className="mx-1.5 h-4 w-px bg-border" />
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon-xs"
                  disabled={!canCreate}
                  onClick={() => handleNew()}
                  className="text-muted-foreground"
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
                  className="text-muted-foreground"
                >
                  <PenLine className="h-3.5 w-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>New note ({formatShortcutDisplay(getBinding("new-note", shortcutBindings))})</TooltipContent>
            </Tooltip>
          </>
        )}

        {/* Spacer */}
        <div className="flex-1" data-tauri-drag-region />

        <div className="flex items-center gap-1">
            {/* Status dot + popover */}
            <Popover>
              <PopoverTrigger asChild>
                <button className="flex items-center gap-1.5 rounded px-2 py-0.5 text-xs text-muted-foreground hover:bg-muted">
                  <StatusDot color={dotColor} pulse={dotPulse} />
                  <span className="max-w-[180px] truncate">{statusText}</span>
                </button>
              </PopoverTrigger>
              <PopoverContent className="w-auto p-0" align="end">
                <StatusPopover />
              </PopoverContent>
            </Popover>

            {/* Search button */}
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon-xs"
                  onClick={() => setSearchOpen(true)}
                >
                  <Search className="h-3.5 w-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Search ({formatShortcutDisplay(getBinding("command-palette", shortcutBindings))})</TooltipContent>
            </Tooltip>
          </div>

          {/* Windows window controls (no native decorations) */}
          {!isMac && (
            <div className="ml-1 flex items-center">
              <button
                onClick={() => getCurrentWindow().minimize()}
                className="inline-flex h-[38px] w-[46px] items-center justify-center text-muted-foreground hover:bg-muted"
              >
                <Minus className="h-3.5 w-3.5" />
              </button>
              <button
                onClick={() => getCurrentWindow().toggleMaximize()}
                className="inline-flex h-[38px] w-[46px] items-center justify-center text-muted-foreground hover:bg-muted"
              >
                <Square className="h-3 w-3" />
              </button>
              <button
                onClick={() => getCurrentWindow().close()}
                className="inline-flex h-[38px] w-[46px] items-center justify-center text-muted-foreground hover:bg-red-500 hover:text-white"
              >
                <X className="h-3.5 w-3.5" />
              </button>
            </div>
          )}
      </div>

      <SearchCommand open={searchOpen} onOpenChange={setSearchOpen} />
    </>
  );
}
