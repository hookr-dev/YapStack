import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { ChevronUp, Camera, Copy, FolderOpen, Trash2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  clearLogs,
  formatLogEntries,
  getRecentLogs,
  INITIAL_LOG_FETCH,
  LOG_BUFFER_CAPACITY,
  LOG_FETCH_STEP,
  revealLogDir,
  subscribeLogs,
  type LogEntry,
} from "@/lib/logs";
import { captureDiagnostics } from "@/lib/logger";
import { useAppStore } from "@/stores/appStore";

const AUTO_SCROLL_THRESHOLD_PX = 40;

type LevelStyle = {
  /** Row background (errors/warnings tint; others stay transparent). */
  row: string;
  /** Color of the `[LEVEL]` bracket text. */
  level: string;
  /** Color of the message body (mutes DEBUG/TRACE). */
  message: string;
};

const LEVEL_STYLES: Record<LogEntry["level"], LevelStyle> = {
  ERROR: {
    row: "bg-red-500/10",
    level: "text-red-500",
    message: "text-red-500/90",
  },
  WARN: {
    row: "bg-yellow-500/[0.07]",
    level: "text-yellow-500",
    message: "text-yellow-200",
  },
  INFO: {
    row: "",
    level: "text-sky-400",
    message: "text-foreground",
  },
  DEBUG: {
    row: "",
    level: "text-muted-foreground",
    message: "text-muted-foreground",
  },
  TRACE: {
    row: "",
    level: "text-muted-foreground/60",
    message: "text-muted-foreground/60",
  },
};

function formatTs(ts: number): string {
  const d = new Date(ts);
  const h = d.getHours().toString().padStart(2, "0");
  const m = d.getMinutes().toString().padStart(2, "0");
  const s = d.getSeconds().toString().padStart(2, "0");
  return `${h}:${m}:${s}`;
}

/** Drop the `yapstack_` prefix (most targets share it) to save horizontal space. */
function shortTarget(target: string): string {
  return target.startsWith("yapstack_") ? target.slice("yapstack_".length) : target;
}

/**
 * One log row. Prefix fields are inline so wrapped lines of a long message
 * flow under the timestamp instead of being indented by fixed-width columns.
 * Click / Enter / Space toggles expand.
 */
function LogRow({ entry }: { entry: LogEntry }) {
  const [expanded, setExpanded] = useState(false);
  const style = LEVEL_STYLES[entry.level];
  return (
    <div
      role="button"
      tabIndex={0}
      onClick={() => setExpanded((v) => !v)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          setExpanded((v) => !v);
        }
      }}
      className={`group cursor-pointer rounded px-1 py-0.5 hover:bg-muted/40 ${style.row} ${
        expanded ? "whitespace-pre-wrap break-all" : "line-clamp-3 break-words"
      }`}
      title={`${entry.target} · ${entry.level} · click to ${expanded ? "collapse" : "expand"}`}
    >
      <span className="text-muted-foreground/60 tabular-nums">
        [{formatTs(entry.ts_ms)}]
      </span>
      <span className={`font-medium ${style.level}`}>[{entry.level}]</span>
      <span className="text-muted-foreground/70">
        [{shortTarget(entry.target)}]
      </span>{" "}
      <span className={style.message}>{entry.message}</span>
    </div>
  );
}

export function LogsPanel() {
  const [entries, setEntries] = useState<LogEntry[]>([]);
  // How many entries we last asked the backend for. Grows when the user loads
  // older history; the live cap tracks this so freshly-loaded older entries
  // aren't immediately trimmed by an incoming `log://entry` event.
  const [fetchLimit, setFetchLimit] = useState(INITIAL_LOG_FETCH);
  // True until a snapshot comes back with fewer entries than we asked for,
  // which means we've reached the oldest entry the ring buffer still holds.
  const [hasMoreHistory, setHasMoreHistory] = useState(true);
  const [loadingOlder, setLoadingOlder] = useState(false);

  const viewportRef = useRef<HTMLDivElement | null>(null);
  const pinnedToBottom = useRef(true);
  // Set when an entries change is driven by prepended history (load older)
  // rather than a new live tail entry. The tail effect reads this to keep the
  // viewport stable instead of yanking the user back to the bottom.
  const preserveScrollFromBottom = useRef<number | null>(null);
  // The live cap can't drop below what's currently loaded, or a single live
  // entry would silently discard older entries the user just fetched. Held in
  // a ref so the (mount-once) live subscription always trims to the current
  // window without needing to re-subscribe.
  const liveCapRef = useRef(INITIAL_LOG_FETCH);
  liveCapRef.current = Math.max(fetchLimit, INITIAL_LOG_FETCH);

  // Re-fetch a larger window of history. Re-applies on top of the live stream
  // by replacing `entries` with the bigger snapshot; the tail effect is told
  // (via `preserveScrollFromBottom`) to hold the viewport in place.
  const loadOlder = useCallback(async () => {
    const vp = viewportRef.current;
    if (vp) {
      // Distance from the bottom we want to keep constant across the re-render
      // so the rows the user is reading don't jump under them.
      preserveScrollFromBottom.current = vp.scrollHeight - vp.scrollTop;
    }
    const nextLimit = Math.min(fetchLimit + LOG_FETCH_STEP, LOG_BUFFER_CAPACITY);
    setLoadingOlder(true);
    try {
      const snapshot = await getRecentLogs(nextLimit);
      setFetchLimit(nextLimit);
      setEntries(snapshot);
      // Fewer than requested (or we've already reached the buffer ceiling) =>
      // there is no older history left to load.
      setHasMoreHistory(
        snapshot.length >= nextLimit && nextLimit < LOG_BUFFER_CAPACITY,
      );
    } catch (err) {
      console.error("failed to load older logs", err);
    } finally {
      setLoadingOlder(false);
    }
  }, [fetchLimit]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;

    (async () => {
      try {
        const snapshot = await getRecentLogs(INITIAL_LOG_FETCH);
        if (cancelled) return;
        setEntries(snapshot);
        // Older history exists only if the buffer handed back a *full* initial
        // window (a short snapshot means we already have everything buffered)
        // and the initial window is below the buffer ceiling (otherwise there's
        // nothing further to reach for).
        setHasMoreHistory(
          snapshot.length >= INITIAL_LOG_FETCH &&
            INITIAL_LOG_FETCH < LOG_BUFFER_CAPACITY,
        );
      } catch (err) {
        if (!cancelled) console.error("failed to load recent logs", err);
      }

      unlisten = await subscribeLogs((entry) => {
        const cap = liveCapRef.current;
        setEntries((prev) => {
          const next = prev.concat(entry);
          return next.length > cap ? next.slice(next.length - cap) : next;
        });
      });
    })();

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // Tail: when pinned to bottom, jump to the end on every entries change
  // (including the initial snapshot load). useLayoutEffect so the scroll
  // happens before paint — avoids a brief flicker at the top on first mount.
  // A load-older change instead restores the prior distance-from-bottom so the
  // viewport stays put over the rows the user is reading.
  useLayoutEffect(() => {
    const vp = viewportRef.current;
    if (!vp) return;
    if (preserveScrollFromBottom.current !== null) {
      vp.scrollTop = vp.scrollHeight - preserveScrollFromBottom.current;
      preserveScrollFromBottom.current = null;
      return;
    }
    if (!pinnedToBottom.current) return;
    vp.scrollTop = vp.scrollHeight;
  }, [entries]);

  const onScroll = (e: React.UIEvent<HTMLDivElement>) => {
    const t = e.currentTarget;
    // "At the bottom" = within AUTO_SCROLL_THRESHOLD_PX of the end. Below
    // that, we treat the user as reading scroll-back and stop tailing;
    // scroll back down and tailing resumes automatically.
    pinnedToBottom.current =
      t.scrollHeight - t.clientHeight - t.scrollTop < AUTO_SCROLL_THRESHOLD_PX;
  };

  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(formatLogEntries(entries));
      toast.success(`Copied ${entries.length} log ${entries.length === 1 ? "line" : "lines"}`);
    } catch (err) {
      toast.error("Failed to copy logs");
      console.error(err);
    }
  };

  const onReveal = async () => {
    try {
      await revealLogDir();
    } catch (err) {
      toast.error("Failed to reveal log folder");
      console.error(err);
    }
  };

  const onSnapshot = () => {
    // Use store.getState() (not hooks) so the snapshot reflects the values
    // at the moment the user clicked, even if a render is pending.
    const state = useAppStore.getState();
    captureDiagnostics({
      activeSession: state.activeSessionId ?? "none",
      activeSegments: state.activeSessionSegments.length,
      livePhase: state.livePhase ?? "Idle",
      backfillActive: state.backfillActive ? 1 : 0,
      sessions: state.sessions.length,
    });
    toast.success("Diagnostic snapshot logged");
  };

  const onClear = async () => {
    try {
      await clearLogs();
      setEntries([]);
      // The buffer is empty now, so reset the history window back to its
      // initial size and re-hide "load older" until enough entries accrue.
      setFetchLimit(INITIAL_LOG_FETCH);
      setHasMoreHistory(false);
    } catch (err) {
      toast.error("Failed to clear logs");
      console.error(err);
    }
  };

  return (
    <div className="flex flex-col gap-2">
      <div
        ref={viewportRef}
        onScroll={onScroll}
        className="h-[260px] divide-y divide-border/40 overflow-y-auto border-t border-border px-1 py-1 font-mono text-[10px] leading-4"
      >
        {entries.length === 0 ? (
          <div className="text-muted-foreground/60">No log entries yet.</div>
        ) : (
          <>
            {hasMoreHistory && (
              <div className="flex justify-center py-0.5">
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-6 gap-1 px-2 text-[10px] text-muted-foreground"
                  onClick={loadOlder}
                  disabled={loadingOlder}
                >
                  <ChevronUp className="h-3 w-3" />
                  {loadingOlder ? "Loading…" : "Load older entries"}
                </Button>
              </div>
            )}
            {entries.map((e, i) => (
              <LogRow key={`${e.ts_ms}-${i}`} entry={e} />
            ))}
          </>
        )}
      </div>
      <div className="flex items-center justify-between gap-2 border-t border-border px-3 py-2">
        <span className="text-[10px] text-muted-foreground">
          {entries.length} {entries.length === 1 ? "entry" : "entries"}
        </span>
        <div className="flex items-center gap-1">
          <Button
            size="sm"
            variant="ghost"
            className="h-7 gap-1.5 px-2 text-[11px]"
            onClick={onCopy}
            disabled={entries.length === 0}
          >
            <Copy className="h-3 w-3" />
            Copy
          </Button>
          <Button
            size="sm"
            variant="ghost"
            className="h-7 gap-1.5 px-2 text-[11px]"
            onClick={onSnapshot}
            title="Log a heap + session-state snapshot to the rolling log"
          >
            <Camera className="h-3 w-3" />
            Snapshot
          </Button>
          <Button
            size="sm"
            variant="ghost"
            className="h-7 gap-1.5 px-2 text-[11px]"
            onClick={onReveal}
          >
            <FolderOpen className="h-3 w-3" />
            Reveal
          </Button>
          <Button
            size="sm"
            variant="ghost"
            className="h-7 gap-1.5 px-2 text-[11px]"
            onClick={onClear}
            disabled={entries.length === 0}
          >
            <Trash2 className="h-3 w-3" />
            Clear
          </Button>
        </div>
      </div>
    </div>
  );
}
