import { useCallback, useEffect, useRef, useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Progress } from "@/components/ui/progress";
import { Sheet, SheetContent, SheetHeader, SheetTitle } from "@/components/ui/sheet";
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
  AlertTriangle,
  ExternalLink,
  Laptop,
  Loader2,
  Lock,
  Monitor,
  MonitorSmartphone,
  Plus,
  RefreshCw,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";
import { toast } from "sonner";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  syncCommands,
  isCommandNotFound,
  isSelfPending,
  pendingPeerDevices,
  shouldShowUpgrade,
  formatBytes,
  deriveAudioBackup,
} from "@/lib/sync";
import type { AudioCacheStats, DeviceRosterEntry, SyncStatus } from "@/lib/sync";
import { deriveSyncDisplay, type SyncDisplay } from "@/lib/syncDisplay";
import { DevicesIllustration } from "./DevicesIllustration";
import { SyncSetupWizard, type WizardStart } from "./SyncSetupWizard";
import { DeviceApprovalDialog } from "./DeviceApprovalDialog";

/** Idle refresh cadence for the page — the status poll and the downloaded-audio row
 *  share it so the whole page moves on one clock. */
const IDLE_POLL_MS = 5000;

/**
 * Settings → Sync. Two faces:
 *
 *  - NOT SET UP: one hero block (illustration, a sentence, "Set up sync") that opens
 *    the {@link SyncSetupWizard}. Nothing to configure on the page itself. A device
 *    that left setup half-done sees the same hero as "Continue setup".
 *  - ON: the status under the heading, then your devices as tiles (a requesting device
 *    is amber with Review; "Add a device" opens a short guide), then the audio rows,
 *    then Server and Account as quiet rows. Warnings sit directly under the heading.
 *
 * One `deriveSyncDisplay` derivation feeds the badge, the status line and the sidebar
 * glyph so surfaces can never disagree. Connection/auth errors surface verbatim,
 * never auto-routed (feedback_surface_ai_errors).
 */
export function SyncTab() {
  const syncStatus = useAppStore((s) => s.syncStatus);
  const relayConn = useAppStore((s) => s.relayConn);
  const setSyncConfig = useAppStore((s) => s.setSyncConfig);
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);
  const refreshSyncStatus = useAppStore((s) => s.refreshSyncStatus);
  const resetProbe = useAppStore((s) => s.resetProbe);
  const settingsRequest = useAppStore((s) => s.settingsRequest);
  const setSettingsRequest = useAppStore((s) => s.setSettingsRequest);

  const [wizard, setWizard] = useState<{ open: boolean; start: WizardStart }>({
    open: false,
    start: "auto",
  });
  const openWizard = (start: WizardStart = "auto") => setWizard({ open: true, start });

  useEffect(() => {
    void refreshSyncStatus();
  }, [refreshSyncStatus]);

  // A `"sync-setup"` deep link (first-run onboarding, the sidebar) opens the wizard once
  // this tab is mounted, then clears itself.
  useEffect(() => {
    if (settingsRequest === "sync-setup") {
      setSettingsRequest(null);
      openWizard("auto");
    }
  }, [settingsRequest, setSettingsRequest]);

  const signedIn = !!syncStatus?.email;
  const syncEnabled = !!syncStatus?.syncEnabled;
  const phase = syncStatus?.phase ?? "disconnected";

  // Poll while the page is open so progress feels live: faster during an active
  // push; a gentle idle cadence keeps "synced Nm ago" fresh and catches the
  // transition INTO syncing. Only runs while this tab is mounted.
  useEffect(() => {
    if (!signedIn) return;
    const active = phase === "syncing" || phase === "catching_up";
    const id = window.setInterval(
      () => void refreshSyncStatus(),
      active ? 1500 : IDLE_POLL_MS,
    );
    return () => window.clearInterval(id);
  }, [signedIn, phase, refreshSyncStatus]);

  const display = deriveSyncDisplay({
    conn: relayConn,
    status: syncStatus,
    signedIn,
    syncEnabled,
  });

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

  // The single named self-host gate (ENTITLEMENTS_SEAM).
  const billingUrl =
    syncStatus && shouldShowUpgrade(syncStatus) ? syncStatus.billingUrl : null;

  // A build compiled WITHOUT the `sync` cargo feature registers none of the `sync_*`
  // commands, so the mount status call rejects with Tauri's exact "Command sync_status
  // not found" (parked in `lastError`). Say so instead of rendering dead controls.
  if (
    syncStatus?.phase === "error" &&
    isCommandNotFound(syncStatus.lastError, "sync_status")
  ) {
    return (
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <Label className="text-xs">Sync</Label>
          <Badge variant="outline" className="text-[10px]">
            Not in this build
          </Badge>
        </div>
        <p className="text-[10px] text-muted-foreground">
          This copy of YapStack was built without the optional sync feature, so
          there is nothing to set up here — everything you record stays on this
          device. Official releases include sync. To build it yourself, compile
          with{" "}
          <code className="rounded bg-muted px-1 py-0.5 font-mono">--features sync</code>{" "}
          and point it at your own server: see{" "}
          <span className="font-medium text-foreground">docs/self-hosting.md</span>.
        </p>
      </div>
    );
  }

  const wizardEl = (
    <SyncSetupWizard
      open={wizard.open}
      start={wizard.start}
      onOpenChange={(open) => setWizard((w) => ({ ...w, open }))}
    />
  );

  // ---- Not set up (or half set up): the hero. ----
  if (!syncEnabled) {
    const midway = signedIn;
    const waiting = midway && isSelfPending(syncStatus);
    return (
      <>
        <div className="flex flex-col items-center rounded-xl border bg-gradient-to-b from-primary/[0.06] to-transparent px-6 py-8 text-center">
          <DevicesIllustration className="mb-5" />
          <h3 className="mb-1 text-base font-semibold">Your notes on every device</h3>
          <p className="mb-5 max-w-sm text-xs text-muted-foreground">
            {waiting
              ? "This device is signed in and waiting for one of your other devices to approve it."
              : midway
                ? `Signed in as ${syncStatus?.email}. One more step and this device starts syncing.`
                : "Record on one device, read and edit on another. Everything is encrypted before it leaves this device — the server you run only ever holds scrambled data."}
          </p>
          <Button className="w-48" onClick={() => openWizard("auto")}>
            {midway ? "Continue setup" : "Set up sync"}
          </Button>
          <p className="mt-4 flex items-center gap-2 text-[11px] text-muted-foreground">
            <ShieldCheck className="h-3 w-3 text-primary" />
            End-to-end encrypted · Self-hosted
          </p>
          {midway && (
            <button
              type="button"
              className="mt-3 text-[11px] text-muted-foreground underline-offset-2 hover:underline"
              onClick={() => void handleSignOut()}
            >
              Use a different account
            </button>
          )}
        </div>
        {wizardEl}
      </>
    );
  }

  // ---- On. ----
  const status = syncStatus!;
  const pendingDevices = pendingPeerDevices(status);
  const roster = status.roster.filter((d) => !(d.pending && !d.isSelf));
  const authExpired = status.phase === "auth_expired";

  return (
    <>
      <div className="space-y-2">
        <div className="flex items-start justify-between gap-3">
          <div className="space-y-0.5">
            <Label className="text-xs">Sync</Label>
            <p className="text-[10px] text-muted-foreground">
              Your notes and recordings, the same on every device.
            </p>
          </div>
          <StatusBadge display={display} />
        </div>
        <SyncStatusLine status={status} display={display} />
      </div>

      {/* Pinned verbatim connection error (never auto-routed, must-preserve). */}
      {status.lastError && !authExpired && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertTitle>Connection problem</AlertTitle>
          <AlertDescription>{status.lastError}</AlertDescription>
        </Alert>
      )}

      {authExpired && (
        <div className="flex items-center justify-between gap-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-3">
          <div className="flex items-center gap-2 text-xs text-amber-700 dark:text-amber-300">
            <ShieldAlert className="h-4 w-4 shrink-0" />
            Your sign-in expired. Sign in again to keep syncing.
          </div>
          <Button size="sm" className="shrink-0 text-xs" onClick={() => openWizard("sign-in")}>
            Sign in
          </Button>
        </div>
      )}

      <UnreadableChangesWarning status={status} onRetried={() => void refreshSyncStatus()} />

      {/* Devices */}
      <div className="space-y-2">
        <Label className="text-xs">Devices</Label>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
          {pendingDevices.map((d) => (
            <PendingDeviceTile key={d.fingerprint} device={d} status={status} />
          ))}
          {roster.map((d) => (
            <DeviceTile key={d.fingerprint} device={d} />
          ))}
          <AddDeviceTile email={status.email ?? ""} serverUrl={status.serverUrl} />
        </div>
      </div>

      <AudioBackupRow status={status} onRetried={() => void refreshSyncStatus()} />
      <DownloadedAudioRow />

      <Separator />

      {/* Server + account: quiet identity rows. */}
      <div className="flex items-center justify-between gap-3">
        <div className="space-y-0.5">
          <Label className="text-xs">Server</Label>
          <p className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
            <Lock className="h-3 w-3" />
            {hostOnly(status.serverUrl)}
          </p>
        </div>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button size="sm" variant="ghost" className="text-xs">
              Change
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Change sync server?</AlertDialogTitle>
              <AlertDialogDescription>
                Changing servers signs this device out and stops syncing. Your
                notes on this device are untouched. You can set up sync again
                with the new server afterwards.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction onClick={() => void handleSignOut()}>
                Sign out & change
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </div>

      <div className="flex items-center justify-between gap-3">
        <div className="space-y-0.5">
          <Label className="text-xs">Account</Label>
          <p className="text-[10px] text-muted-foreground">{status.email}</p>
        </div>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button size="sm" variant="ghost" className="text-xs">
              Sign out
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Sign out of sync on this device?</AlertDialogTitle>
              <AlertDialogDescription>
                Syncing stops here until you sign in again. Your notes on this
                device are untouched, and signing back in won't need a new
                approval.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction onClick={() => void handleSignOut()}>
                Sign out
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </div>

      {billingUrl && (
        <div className="flex items-center justify-between gap-3">
          <div className="space-y-0.5">
            <Label className="text-xs">Plan</Label>
            <p className="text-[10px] text-muted-foreground">
              View usage and upgrade for more storage.
            </p>
          </div>
          <Button
            size="sm"
            variant="outline"
            className="text-xs"
            onClick={() => openUrl(billingUrl).catch(() => {})}
          >
            Upgrade
            <ExternalLink className="h-3 w-3" />
          </Button>
        </div>
      )}

      {wizardEl}
    </>
  );
}

// --- Device tiles ------------------------------------------------------------------

function hostOnly(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/** OS as the device reported it → icon + the word people use for it. */
function platformLook(platform: string | null): { Icon: typeof Laptop; name: string | null } {
  switch (platform) {
    case "macos":
      return { Icon: Laptop, name: "macOS" };
    case "windows":
      return { Icon: Monitor, name: "Windows" };
    case "linux":
      return { Icon: Monitor, name: "Linux" };
    default:
      return { Icon: MonitorSmartphone, name: null };
  }
}

function TileShell({
  children,
  className = "",
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div
      className={`flex min-h-[7.5rem] flex-col items-center justify-center gap-2 rounded-lg border bg-card p-3 text-center ${className}`}
    >
      {children}
    </div>
  );
}

function DeviceTile({ device }: { device: DeviceRosterEntry }) {
  const { Icon, name } = platformLook(device.platform);
  return (
    <TileShell className={device.isSelf ? "border-primary/40" : ""}>
      <span className="flex h-9 w-9 items-center justify-center rounded-full bg-primary/10 text-primary">
        <Icon className="h-4 w-4" />
      </span>
      <div className="min-w-0 space-y-0.5">
        <p className="truncate text-xs font-medium">{device.label ?? "Unnamed device"}</p>
        <p className="text-[10px] text-muted-foreground">
          {device.isSelf ? "This device" : (name ?? "Approved")}
        </p>
      </div>
    </TileShell>
  );
}

/** A requesting device: amber, with the Review button that opens the ceremony. */
function PendingDeviceTile({
  device,
  status,
}: {
  device: DeviceRosterEntry;
  status: SyncStatus;
}) {
  const { Icon } = platformLook(device.platform);
  return (
    <TileShell className="border-amber-500/40 bg-amber-500/5">
      <span className="flex h-9 w-9 items-center justify-center rounded-full bg-amber-500/15 text-amber-600 dark:text-amber-400">
        <Icon className="h-4 w-4" />
      </span>
      <div className="min-w-0 space-y-0.5">
        <p className="truncate text-xs font-medium">{device.label ?? "New device"}</p>
        <p className="text-[10px] text-amber-700 dark:text-amber-300">Wants to join</p>
      </div>
      <DeviceApprovalDialog
        device={device}
        rosterFingerprint={status.rosterFingerprint}
        vaultKeyEpoch={status.vaultKeyEpoch}
      />
    </TileShell>
  );
}

/** Opens a short guide: nothing can be "added" from here — the other device signs in
 *  and this one approves it. */
function AddDeviceTile({ email, serverUrl }: { email: string; serverUrl: string }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="flex min-h-[7.5rem] flex-col items-center justify-center gap-2 rounded-lg border border-dashed p-3 text-center text-muted-foreground transition-colors hover:border-foreground/30 hover:text-foreground"
      >
        <span className="flex h-9 w-9 items-center justify-center rounded-full border border-dashed">
          <Plus className="h-4 w-4" />
        </span>
        <p className="text-xs font-medium">Add a device</p>
      </button>
      <Sheet open={open} onOpenChange={setOpen}>
        <SheetContent className="sm:max-w-md">
          <SheetHeader className="p-0">
            <SheetTitle>Add a device</SheetTitle>
            <p className="text-sm text-muted-foreground">
              Devices join by signing in with your account. Here's the whole thing:
            </p>
          </SheetHeader>
          <ol className="mt-2 space-y-4 text-sm">
            <GuideStep n={1} title="Install YapStack on the other device">
              Any Mac or Windows PC works.
            </GuideStep>
            <GuideStep n={2} title="Open Settings → Sync there and sign in">
              Use the same server and account as this device:
              <div className="mt-2 space-y-1 rounded-md bg-muted/50 p-2.5 font-mono text-xs">
                <p>{serverUrl}</p>
                <p>{email}</p>
              </div>
            </GuideStep>
            <GuideStep n={3} title="Approve it from here">
              It appears in your devices as a request. Check that the codes on
              both screens match, then approve. It starts syncing right away.
            </GuideStep>
          </ol>
        </SheetContent>
      </Sheet>
    </>
  );
}

function GuideStep({
  n,
  title,
  children,
}: {
  n: number;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <li className="flex gap-3">
      <span className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-primary/10 text-[11px] font-semibold text-primary">
        {n}
      </span>
      <div className="min-w-0 space-y-0.5">
        <p className="text-xs font-medium">{title}</p>
        <div className="text-xs text-muted-foreground">{children}</div>
      </div>
    </li>
  );
}

// --- Status ------------------------------------------------------------------------

/**
 * The steady-state line under the heading, driven by `deriveSyncDisplay`. During a big
 * initial sync a determinate <Progress> rides above it, capped at 99% while bytes
 * remain (Syncthing "never 100% until done").
 */
function SyncStatusLine({ status, display }: { status: SyncStatus; display: SyncDisplay }) {
  const total = status.ackedThisSession + status.pendingEntries;
  const showBar = status.phase === "syncing" && status.pendingBytes > 0 && total > 0;
  const pct = showBar ? Math.min(99, Math.round((status.ackedThisSession / total) * 100)) : 0;

  const icon =
    display.state === "syncing" || display.state === "catching-up" ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin text-primary" />
    ) : display.state === "error" ? (
      <AlertTriangle className="h-3.5 w-3.5 text-destructive" />
    ) : display.tone === "amber" ? (
      <ShieldAlert className="h-3.5 w-3.5 text-amber-600 dark:text-amber-400" />
    ) : (
      <ShieldCheck className="h-3.5 w-3.5 text-primary" />
    );

  return (
    <div className="space-y-1.5 pt-1">
      {showBar && <Progress value={pct} className="h-1.5" />}
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

function StatusBadge({ display }: { display: SyncDisplay }) {
  const cls = "shrink-0 text-[10px]";
  if (display.tone === "destructive") {
    return <Badge variant="destructive" className={cls}>{display.label}</Badge>;
  }
  if (display.tone === "amber") {
    return (
      <Badge
        variant="outline"
        className={`${cls} border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400`}
      >
        {display.label}
      </Badge>
    );
  }
  if (display.tone === "active") {
    return <Badge variant="secondary" className={cls}>{display.label}</Badge>;
  }
  return <Badge variant="outline" className={cls}>{display.label}</Badge>;
}

// --- Rows --------------------------------------------------------------------------

/**
 * One-shot async button action: owns the busy flag and the SINGLE verbatim failure
 * surfacing for this page. `toast.error` gets the error's own text — never reworded,
 * never auto-routed (feedback_surface_ai_errors). `fn` returns the success copy.
 */
function useAsyncAction(fn: () => Promise<string>): { run: () => Promise<void>; busy: boolean } {
  const [busy, setBusy] = useState(false);
  const run = async () => {
    setBusy(true);
    try {
      toast.success(await fn());
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return { run, busy };
}

function RetryButton({ busy, onClick }: { busy: boolean; onClick: () => void }) {
  return (
    <Button size="sm" variant="outline" className="shrink-0 text-xs" onClick={onClick} disabled={busy}>
      {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="h-3.5 w-3.5" />}
      Retry
    </Button>
  );
}

/**
 * Audio backup (S2): the audio-upload lane, surfaced DISTINCTLY from changeset sync.
 * Omitted entirely while there is nothing to report; once it has reported anything in
 * this visit it keeps a resting line rather than unmounting from the middle of the page.
 */
function AudioBackupRow({ status, onRetried }: { status: SyncStatus; onRetried: () => void }) {
  const { run: retry, busy } = useAsyncAction(async () => {
    const n = await syncCommands.retryFailedAudioUploads();
    onRetried();
    return n > 0 ? `Retrying ${n} upload${n === 1 ? "" : "s"}…` : "Nothing to retry.";
  });
  const audio = deriveAudioBackup(status);
  const everReported = useRef(false);
  if (audio.state !== "hidden") everReported.current = true;
  const resting = audio.state === "hidden";
  if (resting && !everReported.current) return null;

  const icon =
    audio.state === "uploading" ? (
      <Loader2 className="h-3 w-3 animate-spin text-primary" />
    ) : audio.state === "failed" ? (
      <AlertTriangle className="h-3 w-3 text-amber-600 dark:text-amber-400" />
    ) : audio.state === "incomplete" ? (
      // Lighter than `failed`: nothing needs the user — the walk lost a race with the
      // boot write-lock and retries by itself next launch.
      <ShieldAlert className="h-3 w-3 text-amber-600/80 dark:text-amber-400/80" />
    ) : (
      <ShieldCheck className="h-3 w-3 text-primary" />
    );

  return (
    <div className="view-enter flex items-center justify-between gap-3">
      <div className="space-y-0.5">
        <Label className="text-xs">Audio backup</Label>
        <p
          className={`flex items-center gap-1.5 text-[10px] ${
            audio.state === "failed" ? "text-amber-600 dark:text-amber-400" : "text-muted-foreground"
          }`}
        >
          {icon}
          {resting ? "Nothing waiting" : audio.label}
        </p>
      </div>
      {audio.state === "failed" && <RetryButton busy={busy} onClick={() => void retry()} />}
    </div>
  );
}

/**
 * Peer changes this device pulled but could not decrypt/decode. NON-dismissable: a
 * non-zero count is a potential tamper/corruption signal that must never be silently
 * hidden. Retry re-attempts under the CURRENT key epoch. Hidden only when the count is 0.
 */
function UnreadableChangesWarning({ status, onRetried }: { status: SyncStatus; onRetried: () => void }) {
  const { run: retry, busy } = useAsyncAction(async () => {
    const recovered = await syncCommands.retryCryptoQuarantine();
    onRetried();
    return recovered > 0
      ? `Recovered ${recovered} change${recovered === 1 ? "" : "s"}.`
      : "Still unreadable — the key for these changes hasn't arrived yet.";
  });
  const n = Math.max(0, status.cryptoQuarantined ?? 0);
  if (n === 0) return null;

  return (
    <Alert variant="destructive">
      <AlertTriangle className="h-4 w-4" />
      <AlertTitle>{n === 1 ? "1 change couldn't be read" : `${n} changes couldn't be read`}</AlertTitle>
      <AlertDescription className="space-y-2">
        <p>
          {n === 1 ? "A change" : "Some changes"} from your other devices couldn't be
          decrypted here. That usually means a device's key hasn't caught up yet — or,
          rarely, that something was tampered with in transit. Nothing is dropped. Retry
          once every device is approved.
        </p>
        <RetryButton busy={busy} onClick={() => void retry()} />
      </AlertDescription>
    </Alert>
  );
}

/**
 * Audio pulled from other devices is keep-until-clear; this row is the clear. Hidden
 * while the cache is empty. Clearing is safe against in-flight downloads by construction
 * (the Rust command skips live fetch slots and temps).
 */
function DownloadedAudioRow() {
  const [stats, setStats] = useState<AudioCacheStats | null>(null);
  const { run: clear, busy } = useAsyncAction(async () => {
    const remaining = await syncCommands.audioCacheClear();
    setStats(remaining);
    return remaining.bytes > 0
      ? `Cleared — ${formatBytes(remaining.bytes)} still in use by an active download`
      : "Downloaded audio cleared";
  });

  const refresh = useCallback(async () => {
    try {
      const s = await syncCommands.audioCacheStats();
      setStats(s && typeof s.bytes === "number" ? s : null);
    } catch {
      setStats(null); // command unavailable → no row
    }
  }, []);
  useEffect(() => {
    void refresh();
    // The cache grows while the page is open (auto-fetch runs from any view).
    const id = window.setInterval(() => void refresh(), IDLE_POLL_MS);
    return () => window.clearInterval(id);
  }, [refresh]);

  if (!stats || stats.bytes <= 0) return null;

  return (
    <div className="view-enter flex items-center justify-between gap-3">
      <div className="space-y-0.5">
        <Label className="text-xs">Downloaded audio</Label>
        <p className="text-[10px] text-muted-foreground">
          {formatBytes(stats.bytes)} from your other devices, kept for playback until you
          clear it.
        </p>
      </div>
      <AlertDialog>
        <AlertDialogTrigger asChild>
          <Button size="sm" variant="outline" className="shrink-0 text-xs" disabled={busy}>
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : "Clear"}
          </Button>
        </AlertDialogTrigger>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Clear downloaded audio?</AlertDialogTitle>
            <AlertDialogDescription>
              Frees {formatBytes(stats.bytes)} on this device. Recordings stay safe on the
              devices that made them and on your sync server — you can download them
              again anytime.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction onClick={() => void clear()}>Clear</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
