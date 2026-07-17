import { useEffect, useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Progress } from "@/components/ui/progress";
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
} from "@/components/ui/card";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import {
  Cloud,
  CloudOff,
  Loader2,
  ShieldCheck,
  ShieldAlert,
  ExternalLink,
  AlertTriangle,
  ChevronDown,
  RefreshCw,
} from "lucide-react";
import { toast } from "sonner";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  syncCommands,
  shouldShowUpgrade,
  formatFingerprint,
  deriveAudioBackup,
} from "@/lib/sync";
import type { SyncStatus } from "@/lib/sync";
import { deriveSyncDisplay, type SyncDisplay } from "@/lib/syncDisplay";
import { RelayCard } from "./RelayCard";
import { SignupDialog } from "./SignupDialog";
import { LoginDialog } from "./LoginDialog";
import { DeviceApprovalDialog } from "./DeviceApprovalDialog";

const CARD = "py-4 gap-3";
const HEAD = "px-4";
const BODY = "px-4";

/**
 * Settings → Sync. A single adaptive page composed as four stacked cards (plan
 * §2): Relay server, Account, Devices (signed-in), Enable sync (first-run). The
 * connection health axis (probe → `relayConn`) is separate from the sync phase;
 * one `deriveSyncDisplay` derivation feeds the header badge + steady-state line
 * so surfaces can never disagree. Connection/auth errors surface verbatim, never
 * auto-routed (feedback_surface_ai_errors).
 */
export function SyncTab() {
  const syncConfig = useAppStore((s) => s.syncConfig);
  const syncStatus = useAppStore((s) => s.syncStatus);
  const relayConn = useAppStore((s) => s.relayConn);
  const setSyncConfig = useAppStore((s) => s.setSyncConfig);
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);
  const refreshSyncStatus = useAppStore((s) => s.refreshSyncStatus);
  const probeRelay = useAppStore((s) => s.probeRelay);
  const resetProbe = useAppStore((s) => s.resetProbe);

  const [serverUrl, setServerUrl] = useState(syncConfig.serverUrl);
  const [enabling, setEnabling] = useState(false);

  useEffect(() => {
    void refreshSyncStatus();
  }, [refreshSyncStatus]);

  // Keep the edit buffer in step when a successful signed-out probe persists the
  // normalized URL into syncConfig (store auto-persist), or on sign-out reset.
  useEffect(() => {
    setServerUrl(syncConfig.serverUrl);
  }, [syncConfig.serverUrl]);

  const signedIn = !!syncStatus?.email;
  const syncEnabled = !!syncStatus?.syncEnabled;
  const phase = syncStatus?.phase ?? "disconnected";

  // Poll while the panel is open so progress feels live. Faster during an active
  // push; a gentle idle cadence keeps the "synced Nm ago" line fresh and catches
  // the transition INTO syncing (T024). Only runs while this tab is mounted.
  useEffect(() => {
    if (!signedIn) return;
    const active = phase === "syncing" || phase === "catching_up";
    const intervalMs = active ? 1500 : 5000;
    const id = window.setInterval(() => {
      void refreshSyncStatus();
    }, intervalMs);
    return () => window.clearInterval(id);
  }, [signedIn, phase, refreshSyncStatus]);

  const display = deriveSyncDisplay({
    conn: relayConn,
    status: syncStatus,
    signedIn,
    syncEnabled,
  });

  const handleSaveAnyway = () => {
    // Escape hatch (§0.5): persist the URL without a successful probe. Signed-out
    // only — a signed-in device's URL is locked behind the change-server flow.
    setSyncConfig({ serverUrl: serverUrl.trim() });
    toast.success("Server URL saved.");
  };

  const handleEnable = async () => {
    setEnabling(true);
    try {
      const status = await syncCommands.enable();
      setSyncStatus(status);
      toast.success("Sync enabled.");
    } catch (e) {
      // Surface verbatim — do not fall back.
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Could not enable sync: ${msg}`);
    } finally {
      setEnabling(false);
    }
  };

  const handleSignOut = async () => {
    try {
      await syncCommands.signOut();
      setSyncStatus(null);
      setSyncConfig({ email: null, syncEnabled: false });
      resetProbe();
      toast.success("Signed out of sync.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  const pendingDevices = (syncStatus?.roster ?? []).filter(
    (d) => d.pending && !d.isSelf,
  );

  return (
    <div className="space-y-4">
      {/* Header + single-source-of-truth status badge. */}
      <div className="flex items-start justify-between gap-3">
        <div className="space-y-1">
          <div className="flex items-center gap-2">
            {signedIn ? (
              <Cloud className="h-4 w-4 text-primary" />
            ) : (
              <CloudOff className="h-4 w-4 text-muted-foreground" />
            )}
            <h3 className="text-sm font-semibold">YapStack Sync</h3>
            <StatusBadge display={display} />
          </div>
          <p className="text-xs text-muted-foreground max-w-md">
            End-to-end encrypted sync across your devices. Your notes and audio
            are encrypted on this device before they ever reach the relay — the
            server never sees plaintext.
          </p>
        </div>
      </div>

      {/* Pinned verbatim connection error (never auto-routed, must-preserve). */}
      {syncStatus?.lastError && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertTitle>Connection problem</AlertTitle>
          <AlertDescription>{syncStatus.lastError}</AlertDescription>
        </Alert>
      )}

      {/* Card 1 — Relay server (always shown). */}
      <RelayCard
        serverUrl={serverUrl}
        setServerUrl={setServerUrl}
        savedUrl={syncConfig.serverUrl}
        signedIn={signedIn}
        relayConn={relayConn}
        probeRelay={probeRelay}
        resetProbe={resetProbe}
        onSaveAnyway={handleSaveAnyway}
        onChangeServer={handleSignOut}
      />

      {/* Card 2 — Account (always shown). */}
      <AccountCard
        status={syncStatus}
        signedIn={signedIn}
        serverUrl={serverUrl}
        savedServerUrl={syncConfig.serverUrl}
      />

      {/* Card 3 — Devices (signed-in only). */}
      {signedIn && syncStatus && (
        <DevicesCard status={syncStatus} pendingDevices={pendingDevices} />
      )}

      {/* Card 4 — Enable sync (signed-in && !enabled, first-run only). */}
      {signedIn && syncStatus && !syncEnabled && (
        <Card className={CARD}>
          <CardHeader className={HEAD}>
            <CardTitle className="text-sm">Enable sync</CardTitle>
            <CardDescription className="text-xs">
              Upgrades this library in place for encrypted sync and keeps a full
              backup alongside it (yapstack.db.pre-sync-backup) as a safety net.
            </CardDescription>
          </CardHeader>
          <CardContent className={BODY}>
            <Button size="sm" onClick={handleEnable} disabled={enabling}>
              {enabling ? (
                <>
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  Upgrading your library for sync…
                </>
              ) : (
                "Enable sync on this device"
              )}
            </Button>
          </CardContent>
        </Card>
      )}

      {/* Steady-state status line (replaces the Enable card once enabled). */}
      {signedIn && syncEnabled && syncStatus && (
        <SyncStatusLine status={syncStatus} display={display} />
      )}

      {/* Audio backup lane — DISTINCT from changeset sync (S2). Only shown once
          enabled and when there is something to report. */}
      {signedIn && syncEnabled && syncStatus && (
        <AudioBackupCard
          status={syncStatus}
          onRetried={() => void refreshSyncStatus()}
        />
      )}

      {/* Upgrade — only when a billing_url is advertised (unchanged). */}
      {syncStatus && shouldShowUpgrade(syncStatus) && syncStatus.billingUrl && (
        <Card className={CARD}>
          <CardContent
            className={`${BODY} flex items-center justify-between gap-3`}
          >
            <div className="space-y-0.5">
              <p className="text-xs font-medium">Manage your plan</p>
              <p className="text-[11px] text-muted-foreground">
                View usage and upgrade for more storage.
              </p>
            </div>
            <Button
              size="sm"
              variant="outline"
              onClick={() =>
                syncStatus.billingUrl &&
                openUrl(syncStatus.billingUrl).catch(() => {})
              }
            >
              Upgrade
              <ExternalLink className="h-3 w-3" />
            </Button>
          </CardContent>
        </Card>
      )}

      {/* Advanced — subdued Collapsible, not a red danger zone (plan §5). */}
      {signedIn && (
        <Collapsible>
          <CollapsibleTrigger className="group flex items-center gap-1 text-[11px] text-muted-foreground hover:text-foreground">
            <ChevronDown className="h-3 w-3 transition-transform group-data-[state=open]:rotate-180" />
            Advanced
          </CollapsibleTrigger>
          <CollapsibleContent className="pt-2">
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button size="sm" variant="ghost">
                  Sign out
                </Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Sign out of sync on this device?</AlertDialogTitle>
                  <AlertDialogDescription>
                    Your local data is untouched and your device identity is
                    preserved.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={handleSignOut}>
                    Sign out
                  </AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          </CollapsibleContent>
        </Collapsible>
      )}
    </div>
  );
}

/**
 * Account card: signed-out offers create/sign-in (dialogs unchanged); signed-in
 * shows the account email + this-device fingerprint. An expired session gets a
 * warm amber "sign in again" CTA — never destructive, never conflated with a
 * wrong server (plan §2).
 */
function AccountCard({
  status,
  signedIn,
  serverUrl,
  savedServerUrl,
}: {
  status: SyncStatus | null;
  signedIn: boolean;
  serverUrl: string;
  savedServerUrl: string;
}) {
  const authExpired = status?.phase === "auth_expired";
  // Signed-out dialogs authenticate against the URL the user is about to save;
  // signed-in re-login uses the locked, persisted server.
  const dialogUrl = signedIn ? savedServerUrl : serverUrl.trim() || savedServerUrl;

  return (
    <Card className={CARD}>
      <CardHeader className={HEAD}>
        <CardTitle className="text-sm">Account</CardTitle>
        <CardDescription className="text-xs">
          {signedIn
            ? "Your YapStack Sync account on this device."
            : "Sign in on this device, or create an account to start syncing."}
        </CardDescription>
      </CardHeader>
      <CardContent className={`${BODY} space-y-2`}>
        {!signedIn ? (
          <div className="flex items-center gap-2">
            <SignupDialog serverUrl={dialogUrl} />
            <LoginDialog serverUrl={dialogUrl} />
          </div>
        ) : (
          <>
            <div className="space-y-0.5">
              <div className="flex items-center gap-1.5 text-xs font-medium">
                <ShieldCheck className="h-3.5 w-3.5 text-primary" />
                {status?.email}
              </div>
              {status?.deviceFingerprint && (
                <p className="text-[11px] text-muted-foreground font-mono">
                  This device: {formatFingerprint(status.deviceFingerprint)}
                </p>
              )}
            </div>
            {authExpired && (
              <div className="flex items-center justify-between gap-2 rounded-md border border-amber-500/30 bg-amber-500/5 p-2.5">
                <div className="flex items-center gap-1.5 text-[11px] text-amber-700 dark:text-amber-300">
                  <ShieldAlert className="h-3.5 w-3.5" />
                  Your sign-in expired — sign in again to resume syncing.
                </div>
                <LoginDialog serverUrl={dialogUrl} />
              </div>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Devices card (signed-in): the pending-approval gate (ceremony copy UNCHANGED)
 * plus the roster rows — mono fingerprints with this-device / pending badges.
 */
function DevicesCard({
  status,
  pendingDevices,
}: {
  status: SyncStatus;
  pendingDevices: SyncStatus["roster"];
}) {
  return (
    <Card className={CARD}>
      <CardHeader className={HEAD}>
        <CardTitle className="text-sm">Devices</CardTitle>
        <CardDescription className="text-xs">
          Approve each new device before it can sync.
        </CardDescription>
      </CardHeader>
      <CardContent className={`${BODY} space-y-3`}>
        {/* Pending device approvals — the roster gate (§7.5, copy unchanged). */}
        {pendingDevices.length > 0 && (
          <Alert>
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>
              {pendingDevices.length === 1
                ? "A new device is requesting access"
                : `${pendingDevices.length} devices are requesting access`}
            </AlertTitle>
            <AlertDescription className="space-y-2">
              <span>
                Approve it only if you started signing in on that device. Check
                the fingerprint matches out-of-band.
              </span>
              <div className="flex flex-col gap-1.5 pt-1">
                {pendingDevices.map((d) => (
                  <DeviceApprovalDialog key={d.fingerprint} device={d} />
                ))}
              </div>
            </AlertDescription>
          </Alert>
        )}

        {status.roster.length > 0 && (
          <div className="space-y-1.5">
            {status.rosterFingerprint && (
              <div className="flex items-center justify-end">
                <span className="text-[11px] text-muted-foreground font-mono">
                  roster {formatFingerprint(status.rosterFingerprint)}
                  {status.vaultKeyEpoch != null &&
                    ` · epoch ${status.vaultKeyEpoch}`}
                </span>
              </div>
            )}
            <div className="rounded-lg border divide-y">
              {status.roster.map((d) => (
                <div
                  key={d.fingerprint}
                  className="flex items-center justify-between px-3 py-2"
                >
                  <span className="text-xs font-mono">
                    {formatFingerprint(d.fingerprint)}
                  </span>
                  <div className="flex items-center gap-1.5">
                    {d.label && (
                      <span className="text-[11px] text-muted-foreground">
                        {d.label}
                      </span>
                    )}
                    {d.isSelf && (
                      <Badge variant="secondary" className="text-[10px]">
                        this device
                      </Badge>
                    )}
                    {d.pending && (
                      <Badge variant="outline" className="text-[10px]">
                        pending
                      </Badge>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * The enabled-device steady-state line, driven by `deriveSyncDisplay` (the shared
 * derivation, plan §1b). During a big initial sync a determinate <Progress> rides
 * above it, capped at 99% while bytes remain (Syncthing "never 100% until done").
 * Total = entries acked this session + entries still pending; degenerate totals
 * fall back to the plain text line.
 */
function SyncStatusLine({
  status,
  display,
}: {
  status: SyncStatus;
  display: SyncDisplay;
}) {
  const total = status.ackedThisSession + status.pendingEntries;
  const showBar =
    status.phase === "syncing" && status.pendingBytes > 0 && total > 0;
  const pct = showBar
    ? Math.min(99, Math.round((status.ackedThisSession / total) * 100))
    : 0;

  const icon =
    display.state === "syncing" || display.state === "catching-up" ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin text-primary" />
    ) : display.state === "error" ? (
      <AlertTriangle className="h-3.5 w-3.5 text-destructive" />
    ) : display.state === "auth-expired" || display.state === "unreachable" ? (
      <ShieldAlert className="h-3.5 w-3.5 text-amber-600 dark:text-amber-400" />
    ) : (
      <ShieldCheck className="h-3.5 w-3.5 text-primary" />
    );

  return (
    <div className="space-y-2">
      {showBar && <Progress value={pct} />}
      <div
        className={`flex items-center gap-1.5 text-xs ${
          display.tone === "destructive"
            ? "text-destructive"
            : display.tone === "amber"
              ? "text-amber-600 dark:text-amber-400"
              : "text-muted-foreground"
        }`}
      >
        {icon}
        {display.label}
      </div>
    </div>
  );
}

/**
 * Audio backup card (S2): the audio-upload lane surfaced DISTINCTLY from changeset
 * sync (never merged, per repo posture). Shows an in-flight line (with the
 * existing-library backfill nuance), an "all backed up" resting line, or a failed
 * state with a manual Retry — hidden entirely when there is nothing to report.
 */
function AudioBackupCard({
  status,
  onRetried,
}: {
  status: SyncStatus;
  onRetried: () => void;
}) {
  const [retrying, setRetrying] = useState(false);
  const audio = deriveAudioBackup(status);
  if (audio.state === "hidden") return null;

  const handleRetry = async () => {
    setRetrying(true);
    try {
      const n = await syncCommands.retryFailedAudioUploads();
      toast.success(
        n > 0 ? `Retrying ${n} upload${n === 1 ? "" : "s"}…` : "Nothing to retry.",
      );
      onRetried();
    } catch (e) {
      // Surface verbatim — never auto-route.
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setRetrying(false);
    }
  };

  const icon =
    audio.state === "uploading" ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin text-primary" />
    ) : audio.state === "failed" ? (
      <AlertTriangle className="h-3.5 w-3.5 text-amber-600 dark:text-amber-400" />
    ) : (
      <ShieldCheck className="h-3.5 w-3.5 text-primary" />
    );

  return (
    <Card className={CARD}>
      <CardHeader className={HEAD}>
        <CardTitle className="text-sm">Audio backup</CardTitle>
        <CardDescription className="text-xs">
          Your recordings are encrypted on this device, then uploaded so any of
          your devices can play them.
        </CardDescription>
      </CardHeader>
      <CardContent
        className={`${BODY} flex items-center justify-between gap-3`}
      >
        <div
          className={`flex items-center gap-1.5 text-xs ${
            audio.state === "failed"
              ? "text-amber-600 dark:text-amber-400"
              : "text-muted-foreground"
          }`}
        >
          {icon}
          {audio.label}
        </div>
        {audio.state === "failed" && (
          <Button
            size="sm"
            variant="outline"
            onClick={handleRetry}
            disabled={retrying}
          >
            {retrying ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <RefreshCw className="h-3.5 w-3.5" />
            )}
            Retry
          </Button>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Header status chip — renders the shared derivation's label with a tone-mapped
 * Badge variant (plan §2/§8). Tone is the single differentiator; the icon/motion
 * map belongs to the T028 sidebar glyph, not here.
 */
function StatusBadge({ display }: { display: SyncDisplay }) {
  if (display.tone === "destructive") {
    return (
      <Badge variant="destructive" className="text-[10px]">
        {display.label}
      </Badge>
    );
  }
  if (display.tone === "amber") {
    return (
      <Badge
        variant="outline"
        className="border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400 text-[10px]"
      >
        {display.label}
      </Badge>
    );
  }
  if (display.tone === "active") {
    return (
      <Badge variant="secondary" className="text-[10px]">
        {display.label}
      </Badge>
    );
  }
  return (
    <Badge variant="outline" className="text-[10px]">
      {display.label}
    </Badge>
  );
}
