import { useEffect, useState } from "react";
import { Cloud, CloudCheck, CloudAlert, CloudOff, RefreshCw } from "lucide-react";
import { useAppStore } from "@/stores/appStore";
import { syncCommands } from "@/lib/sync";
import {
  deriveSyncDisplay,
  type SyncDisplayIcon,
  type SyncDisplayTone,
} from "@/lib/syncDisplay";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

/**
 * App-wide ambient sync indicator that lives on the sidebar Settings row. It is
 * a *pure projection* of {@link deriveSyncDisplay} (plan §3: "recompute from
 * live status; never latch") — it derives NOTHING locally, so it can never
 * disagree with the Settings → Sync badge that renders from the same helper.
 *
 * Owns a light app-wide poll (§4): the T024 Sync-tab poll only runs while that
 * tab is open, but the glyph must stay live everywhere. Renders nothing until
 * the user has engaged sync, and self-disables on no-sync builds where the
 * `sync_status` command doesn't exist.
 */
export function SyncStatusGlyph() {
  const syncStatus = useAppStore((s) => s.syncStatus);
  const relayConn = useAppStore((s) => s.relayConn);
  const email = useAppStore((s) => s.syncConfig.email);
  const syncEnabled = useAppStore((s) => s.syncConfig.syncEnabled);
  const setSettingsRequest = useAppStore((s) => s.setSettingsRequest);
  const navigateTo = useAppStore((s) => s.navigateTo);

  const signedIn = email != null;
  // "Engaged" = the user has ever signed in OR turned on sync. A user who never
  // touched sync (and every no-sync build) never engages, so there is no dead
  // cloud icon and no poll for them.
  const engaged = signedIn || syncEnabled;

  const available = useAmbientSyncPoll(engaged);

  if (!engaged || !available) return null;

  const display = deriveSyncDisplay({
    conn: relayConn,
    status: syncStatus,
    signedIn,
    syncEnabled,
  });

  // "off" means signed-out / disabled — never engaged from the glyph's view, so
  // show nothing (belt-and-suspenders with the `engaged` gate above).
  if (display.state === "off") return null;

  const openSyncSettings = () => {
    setSettingsRequest("sync");
    navigateTo("settings");
  };

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          onClick={openSyncSettings}
          aria-label={display.tooltip}
          className="flex shrink-0 items-center rounded-md p-1.5 text-muted-foreground transition-all duration-150 hover:bg-sidebar-accent/50 hover:text-foreground"
        >
          <SyncGlyphIcon
            icon={display.icon}
            tone={display.tone}
            motion={display.motion}
          />
        </button>
      </TooltipTrigger>
      <TooltipContent>{display.tooltip}</TooltipContent>
    </Tooltip>
  );
}

/** Tone → token class, mirroring the Sync-tab status line (T027) exactly so the
 *  two surfaces read identically. */
const TONE_CLASS: Record<SyncDisplayTone, string> = {
  muted: "text-muted-foreground",
  active: "text-primary",
  amber: "text-amber-600 dark:text-amber-400",
  destructive: "text-destructive",
};

/**
 * The single agreed lucide icon map (plan §3). `CloudCheck` / `CloudAlert` both
 * exist in our lucide-react (0.563) so no composition is needed there; only
 * `cloud-dot` composes a base `Cloud` with a tiny tone-colored dot for the
 * idle-with-backlog state.
 */
function SyncGlyphIcon({
  icon,
  tone,
  motion,
}: {
  icon: SyncDisplayIcon;
  tone: SyncDisplayTone;
  motion: boolean;
}) {
  const toneClass = TONE_CLASS[tone];
  const size = "h-3.5 w-3.5 shrink-0";

  switch (icon) {
    case "cloud":
      return <Cloud className={cn(size, toneClass)} aria-hidden />;
    case "cloud-check":
      return <CloudCheck className={cn(size, toneClass)} aria-hidden />;
    case "cloud-alert":
      return <CloudAlert className={cn(size, toneClass)} aria-hidden />;
    case "cloud-off":
      return <CloudOff className={cn(size, toneClass)} aria-hidden />;
    case "refresh":
      // Slow spin (~3s) so drain-cycle bursts don't strobe the sidebar. Motion
      // is gated upstream by deriveSyncDisplay — we only honor its decision and
      // NEVER animate any non-syncing state.
      return (
        <RefreshCw
          className={cn(
            size,
            toneClass,
            motion && "animate-spin [animation-duration:3s]",
          )}
          aria-hidden
        />
      );
    case "cloud-dot":
      return (
        <span className={cn("relative inline-flex", toneClass)} aria-hidden>
          <Cloud className={size} />
          <span className="absolute -right-0.5 -top-0.5 h-1.5 w-1.5 rounded-full bg-current" />
        </span>
      );
  }
}

/**
 * Light app-wide poll for the ambient glyph. Returns whether sync is available
 * on this build; `false` permanently once we learn the `sync_status` command
 * isn't registered.
 *
 * No-sync-build guard (§4, critical): `refreshSyncStatus` swallows its own
 * errors into a verbatim `lastError`, so we cannot observe a missing-command
 * rejection through it. We therefore probe `sync_status` directly on the first
 * tick — in a plain `tauri dev` (no `-f sync`) build the command is unregistered
 * and Tauri v2 rejects with `"Command sync_status not found"`. We detect that
 * once and stop for good, so we never hot-loop or console-spam. Any *other*
 * rejection means the command exists but failed at runtime (network/auth); we
 * hand that back to the store's normal verbatim-error handler and keep polling.
 */
function useAmbientSyncPoll(engaged: boolean): boolean {
  const refreshSyncStatus = useAppStore((s) => s.refreshSyncStatus);
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);
  const [available, setAvailable] = useState(true);

  useEffect(() => {
    if (!engaged) return;

    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let capabilityChecked = false;

    const schedule = () => {
      const syncing = useAppStore.getState().syncStatus?.phase === "syncing";
      timer = setTimeout(tick, syncing ? 2000 : 5000);
    };

    const tick = async () => {
      if (cancelled) return;

      if (!capabilityChecked) {
        capabilityChecked = true;
        try {
          const status = await syncCommands.status();
          if (cancelled) return;
          setSyncStatus(status);
        } catch (e) {
          if (cancelled) return;
          if (isMissingCommand(e)) {
            setAvailable(false);
            return; // never reschedule — permanently off for this session
          }
          await refreshSyncStatus();
        }
      } else {
        await refreshSyncStatus();
      }

      if (!cancelled) schedule();
    };

    void tick();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [engaged, refreshSyncStatus, setSyncStatus]);

  return available;
}

/** Tauri v2 rejects an unregistered command with `"Command {name} not found"`
 *  (tauri/src/webview/mod.rs). A genuine runtime failure surfaces network/auth
 *  phrasing, not "not found", so this signature won't false-positive on a real
 *  build's transient error. */
function isMissingCommand(e: unknown): boolean {
  const msg =
    e instanceof Error ? e.message : typeof e === "string" ? e : String(e);
  return /not found/i.test(msg);
}
