import { useCallback, useEffect, useRef, useState } from "react";
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
  Hourglass,
  Check,
} from "lucide-react";
import { toast } from "sonner";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  syncCommands,
  isCommandNotFound,
  isSelfPending,
  isValidServerUrl,
  pendingPeerDevices,
  shouldShowUpgrade,
  formatFingerprint,
  formatBytes,
  deriveAudioBackup,
} from "@/lib/sync";
import type { AudioCacheStats, SyncStatus } from "@/lib/sync";
import { deriveSyncDisplay, type SyncDisplay } from "@/lib/syncDisplay";
import { RelayCard } from "./RelayCard";
import { SignupDialog } from "./SignupDialog";
import { LoginDialog } from "./LoginDialog";
import { DeviceApprovalDialog } from "./DeviceApprovalDialog";

const CARD = "py-4 gap-3";
const HEAD = "px-4";
const BODY = "px-4";

/** Idle refresh cadence for the panel's derived rows — matches the status poll below so
 *  the whole page moves on one clock. */
const IDLE_POLL_MS = 5000;
const FETCHED_AUDIO_POLL_MS = IDLE_POLL_MS;

/**
 * Settings → Sync. One adaptive page. While a device is being set up it reads as a
 * guided flow: a setup rail under the header names the steps (Server → Sign in →
 * [Approval] → Turn on) and the card for the CURRENT step floats to the top, with the
 * Relay/Account identity cards beneath it as completed context. Once sync is on, the
 * page settles into the steady-state stack (status line, audio lanes, warnings).
 *
 * The connection health axis (probe → `relayConn`) is separate from the sync phase;
 * one `deriveSyncDisplay` derivation feeds the header badge + steady-state line so
 * surfaces can never disagree. Connection/auth errors surface verbatim, never
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
  const selfPending = isSelfPending(syncStatus);
  const pendingDevices = pendingPeerDevices(syncStatus);
  const relayReady = isValidServerUrl(syncConfig.serverUrl);

  // Poll while the panel is open so progress feels live. Faster during an active
  // push; a gentle idle cadence keeps the "synced Nm ago" line fresh, catches the
  // transition INTO syncing (T024), and — for a device waiting on approval — is how
  // the page notices the approval landed. Only runs while this tab is mounted.
  useEffect(() => {
    if (!signedIn) return;
    const active = phase === "syncing" || phase === "catching_up";
    const intervalMs = active ? 1500 : IDLE_POLL_MS;
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
      toast.success("Sync is on.");
    } catch (e) {
      // Surface verbatim — do not fall back.
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Could not turn on sync: ${msg}`);
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

  // The single named self-host gate (ENTITLEMENTS_SEAM) — decided once here so the
  // upgrade card and its click handler share one decision instead of re-guarding.
  const billingUrl =
    syncStatus && shouldShowUpgrade(syncStatus) ? syncStatus.billingUrl : null;

  // A build compiled WITHOUT the `sync` cargo feature registers none of the
  // `sync_*` commands, so the mount status call above rejects with Tauri's exact
  // "Command sync_status not found" (which `refreshSyncStatus` parks in
  // `lastError`). Every control below would be a dead end, so say so plainly
  // instead. Official releases DO ship sync; this is the custom-build case.
  const syncFeatureMissing =
    syncStatus?.phase === "error" &&
    isCommandNotFound(syncStatus.lastError, "sync_status");

  if (syncFeatureMissing) {
    return (
      <div className="space-y-4">
        <div className="flex items-center gap-2">
          <CloudOff className="h-4 w-4 text-muted-foreground" />
          <h3 className="text-sm font-semibold">YapStack Sync</h3>
          <Badge variant="outline" className="text-[10px]">
            Not in this build
          </Badge>
        </div>
        <Card className={CARD}>
          <CardHeader className={HEAD}>
            <CardTitle className="text-sm">
              Sync isn’t included in this build
            </CardTitle>
            <CardDescription className="text-xs">
              This copy of YapStack was compiled without the optional sync
              feature, so there is nothing to set up here — everything you
              record stays on this device. Official releases include sync. To
              build it yourself, compile with{" "}
              <code className="rounded bg-muted px-1 py-0.5 font-mono text-[10px]">
                --features sync
              </code>{" "}
              and point it at your own relay: see{" "}
              <span className="font-medium text-foreground">
                docs/self-hosting.md
              </span>
              .
            </CardDescription>
          </CardHeader>
        </Card>
      </div>
    );
  }

  const settingUp = !signedIn || !syncEnabled;

  return (
    <div className="space-y-4">
      {/* Header + single-source-of-truth status badge. */}
      <div className="space-y-2">
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
        {settingUp && (
          <SetupRail
            relayReady={relayReady}
            signedIn={signedIn}
            selfPending={selfPending}
            syncEnabled={syncEnabled}
          />
        )}
      </div>

      {/* Pinned verbatim connection error (never auto-routed, must-preserve). */}
      {syncStatus?.lastError && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertTitle>Connection problem</AlertTitle>
          <AlertDescription>{syncStatus.lastError}</AlertDescription>
        </Alert>
      )}

      {/* Current step while setting up, floated above the identity cards: either
          this device is waiting on an approval, or it is approved and ready to turn on. */}
      {signedIn && syncStatus && !syncEnabled && (
        selfPending ? (
          <WaitingApprovalCard status={syncStatus} />
        ) : (
          <TurnOnCard status={syncStatus} enabling={enabling} onEnable={handleEnable} />
        )
      )}

      {/* Relay server (always shown). */}
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

      {/* Account (always shown). */}
      <AccountCard
        status={syncStatus}
        signedIn={signedIn}
        serverUrl={serverUrl}
        savedServerUrl={syncConfig.serverUrl}
      />

      {/* Devices (signed-in only). */}
      {signedIn && syncStatus && (
        <DevicesCard status={syncStatus} pendingDevices={pendingDevices} />
      )}

      {/* Steady-state status line once sync is on. */}
      {signedIn && syncEnabled && syncStatus && (
        <SyncStatusLine status={syncStatus} display={display} />
      )}

      {/* Item 1 — persistent crypto-quarantine warning. Peer writes this device pulled but
          could not decrypt; NON-dismissable (a potential tamper signal), with a Retry that
          re-attempts once the key epoch is reconciled. Hidden while the count is 0. */}
      {signedIn && syncEnabled && syncStatus && (
        <CryptoQuarantineWarning
          status={syncStatus}
          onRetried={() => void refreshSyncStatus()}
        />
      )}

      {/* Audio backup lane — DISTINCT from changeset sync (S2). Only shown once
          enabled and when there is something to report. */}
      {signedIn && syncEnabled && syncStatus && (
        <AudioBackupCard
          status={syncStatus}
          onRetried={() => void refreshSyncStatus()}
        />
      )}

      {/* Fetched-audio cache (S3.5 auto-fetch): audio pulled from other devices is
          keep-until-clear; this row is the clear. Hidden while the cache is empty. */}
      {signedIn && syncEnabled && <FetchedAudioCard />}

      {/* Upgrade — only when a billing_url is advertised (unchanged). */}
      {billingUrl && (
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
              onClick={() => openUrl(billingUrl).catch(() => {})}
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

type StepState = "done" | "current" | "todo";

/**
 * The setup rail: a compact "you are here" for the device-setup journey, shown only
 * until sync is on. Server → Sign in → [Approval, only for a device that joined an
 * existing account and is waiting] → Turn on. Done steps carry a check, the current
 * step is emphasized, the rest are muted — the page's cards do the explaining.
 */
function SetupRail({
  relayReady,
  signedIn,
  selfPending,
  syncEnabled,
}: {
  relayReady: boolean;
  signedIn: boolean;
  selfPending: boolean;
  syncEnabled: boolean;
}) {
  const steps: { label: string; state: StepState }[] = [
    { label: "Server", state: relayReady ? "done" : "current" },
    {
      label: "Sign in",
      state: signedIn ? "done" : relayReady ? "current" : "todo",
    },
  ];
  if (selfPending) steps.push({ label: "Approval", state: "current" });
  steps.push({
    label: "Turn on",
    state: syncEnabled
      ? "done"
      : signedIn && !selfPending
        ? "current"
        : "todo",
  });

  return (
    <ol aria-label="Sync setup steps" className="flex items-center gap-1.5 pt-1">
      {steps.map((step, i) => (
        <li key={step.label} className="flex items-center gap-1.5">
          {i > 0 && <span aria-hidden className="h-px w-4 bg-border" />}
          <span
            aria-current={step.state === "current" ? "step" : undefined}
            className={`flex items-center gap-1.5 text-[11px] ${
              step.state === "current"
                ? "font-medium text-foreground"
                : "text-muted-foreground"
            }`}
          >
            <span
              aria-hidden
              className={`flex h-4 w-4 items-center justify-center rounded-full text-[9px] font-semibold tabular-nums ${
                step.state === "done"
                  ? "bg-primary/10 text-primary"
                  : step.state === "current"
                    ? "bg-foreground text-background"
                    : "border border-border text-muted-foreground"
              }`}
            >
              {step.state === "done" ? <Check className="h-2.5 w-2.5" /> : i + 1}
            </span>
            {step.label}
          </span>
        </li>
      ))}
    </ol>
  );
}

/**
 * The joining device's side of the approval ceremony (§7.5). It cannot turn on sync
 * until an existing device approves it, so this card IS the page for now: what to do
 * on the other device, this device's fingerprint large enough to read across a desk,
 * and the roster code the approver's dialog shows for the out-of-band check (§7.5.4).
 * The panel's status poll picks the approval up by itself.
 */
function WaitingApprovalCard({ status }: { status: SyncStatus }) {
  const approvers = status.roster.filter((d) => !d.isSelf && !d.pending);
  const named = approvers.map((d) => d.label).filter((l): l is string => !!l);

  return (
    <Card className={`${CARD} border-amber-500/30 view-enter`}>
      <CardHeader className={HEAD}>
        <CardTitle className="flex items-center gap-1.5 text-sm">
          <Hourglass className="h-3.5 w-3.5 text-amber-600 dark:text-amber-400" />
          Waiting for approval
        </CardTitle>
        <CardDescription className="text-xs">
          {approvers.length === 0
            ? "No other approved device is on this account yet, so nothing can approve this one. Open YapStack on a device you have used with this account before."
            : `Sync starts once one of your other devices approves this one. On ${
                named.length > 0 ? named.join(" or ") : "a device that is already signed in"
              }, open Settings → Sync → Devices and review the request.`}
        </CardDescription>
      </CardHeader>
      <CardContent className={`${BODY} space-y-3`}>
        <div className="rounded-lg border bg-muted/50 p-3 text-center">
          <p className="mb-1 text-[11px] text-muted-foreground">This device</p>
          <p className="font-mono text-base tracking-wide">
            {status.deviceFingerprint
              ? formatFingerprint(status.deviceFingerprint)
              : "—"}
          </p>
          {status.rosterFingerprint && (
            <p className="mt-2 text-[11px] text-muted-foreground">
              Roster code{" "}
              <span className="font-mono text-foreground">
                {formatFingerprint(status.rosterFingerprint)}
              </span>
              {status.vaultKeyEpoch != null && ` · epoch ${status.vaultKeyEpoch}`}
            </p>
          )}
        </div>
        <p className="text-[11px] text-muted-foreground">
          The approving device shows the same fingerprint and roster code. If either
          differs, don’t approve.
        </p>
        <p className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <Loader2 className="h-3 w-3 animate-spin" />
          Checking for approval — this page updates by itself.
        </p>
      </CardContent>
    </Card>
  );
}

/**
 * Turn on sync (signed-in, approved, not yet enabled). The copy says what will
 * actually happen to THIS library: a first device uploads it; a device joining an
 * account that already has approved peers downloads theirs and merges. Both keep a
 * full pre-sync backup first.
 */
function TurnOnCard({
  status,
  enabling,
  onEnable,
}: {
  status: SyncStatus;
  enabling: boolean;
  onEnable: () => void;
}) {
  const hasPeers = status.roster.some((d) => !d.isSelf && !d.pending);
  return (
    <Card className={`${CARD} view-enter`}>
      <CardHeader className={HEAD}>
        <CardTitle className="text-sm">Turn on sync</CardTitle>
        <CardDescription className="text-xs">
          {hasPeers
            ? "Notes and recordings from your other devices download here, and anything already on this device is merged in. A full backup of this library is kept first."
            : "This library is prepared for sync and uploaded to your relay, encrypted on this device. A full backup copy is kept first."}
        </CardDescription>
      </CardHeader>
      <CardContent className={BODY}>
        <Button size="sm" onClick={onEnable} disabled={enabling}>
          {enabling ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              Preparing your library…
            </>
          ) : (
            "Turn on sync"
          )}
        </Button>
      </CardContent>
    </Card>
  );
}

/**
 * Account card: signed-out offers create/sign-in, gated on a usable relay URL (an
 * account lives on a relay, so there is nothing to sign in to before one is set);
 * signed-in shows the account email + this-device fingerprint. An expired session
 * gets a warm amber "sign in again" CTA — never destructive, never conflated with a
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
  const relayReady = isValidServerUrl(dialogUrl);

  return (
    <Card className={CARD}>
      <CardHeader className={HEAD}>
        <CardTitle className="text-sm">Account</CardTitle>
        <CardDescription className="text-xs">
          {signedIn
            ? "Your YapStack Sync account on this device."
            : "Sign in to your account, or create one on this relay."}
        </CardDescription>
      </CardHeader>
      <CardContent className={`${BODY} space-y-2`}>
        {!signedIn ? (
          relayReady ? (
            <div className="flex items-center gap-2">
              <SignupDialog serverUrl={dialogUrl} />
              <LoginDialog serverUrl={dialogUrl} />
            </div>
          ) : (
            <p className="text-xs text-muted-foreground">
              Enter your relay server above first — your account lives on the
              relay you choose.
            </p>
          )
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
 * Devices card (signed-in): the pending-approval gate — each requesting device as a
 * named row with a Review button that opens the ceremony dialog — plus the roster of
 * approved devices (name first, fingerprint second). Pending peers appear only in the
 * request list, never twice.
 */
function DevicesCard({
  status,
  pendingDevices,
}: {
  status: SyncStatus;
  pendingDevices: SyncStatus["roster"];
}) {
  const roster = status.roster.filter((d) => !(d.pending && !d.isSelf));
  return (
    <Card className={CARD}>
      <CardHeader className={HEAD}>
        <CardTitle className="text-sm">Devices</CardTitle>
        <CardDescription className="text-xs">
          Every device signed in to this account. A new device syncs only after
          one you already trust approves it.
        </CardDescription>
      </CardHeader>
      <CardContent className={`${BODY} space-y-3`}>
        {pendingDevices.length > 0 && (
          <Alert className="border-amber-500/30 bg-amber-500/5">
            <AlertTriangle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
            <AlertTitle>
              {pendingDevices.length === 1
                ? "A new device is requesting access"
                : `${pendingDevices.length} devices are requesting access`}
            </AlertTitle>
            <AlertDescription className="space-y-2">
              <span>
                Approve only if you just signed in on that device yourself, and
                its fingerprint matches what that device shows.
              </span>
              <div className="flex flex-col gap-1.5 pt-1">
                {pendingDevices.map((d) => (
                  <div
                    key={d.fingerprint}
                    className="flex items-center justify-between gap-3"
                  >
                    <div className="min-w-0">
                      <p className="truncate text-xs font-medium text-foreground">
                        {d.label ?? "New device"}
                      </p>
                      <p className="font-mono text-[11px] text-muted-foreground">
                        {formatFingerprint(d.fingerprint)}
                      </p>
                    </div>
                    <DeviceApprovalDialog
                      device={d}
                      rosterFingerprint={status.rosterFingerprint}
                      vaultKeyEpoch={status.vaultKeyEpoch}
                    />
                  </div>
                ))}
              </div>
            </AlertDescription>
          </Alert>
        )}

        {roster.length > 0 && (
          <div className="rounded-lg border divide-y">
            {roster.map((d) => (
              <div
                key={d.fingerprint}
                className="flex items-center justify-between gap-3 px-3 py-2"
              >
                <div className="min-w-0">
                  <p className="truncate text-xs font-medium">
                    {d.label ?? "Unnamed device"}
                  </p>
                  <p className="font-mono text-[11px] text-muted-foreground">
                    {formatFingerprint(d.fingerprint)}
                  </p>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                  {d.isSelf && (
                    <Badge variant="secondary" className="text-[10px]">
                      this device
                    </Badge>
                  )}
                  {d.pending && (
                    <Badge
                      variant="outline"
                      className="border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400 text-[10px]"
                    >
                      awaiting approval
                    </Badge>
                  )}
                </div>
              </div>
            ))}
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
    ) : display.state === "auth-expired" ||
      display.state === "unreachable" ||
      display.state === "quarantined" ? (
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
 * One-shot async button action: owns the busy flag and the SINGLE verbatim failure
 * surfacing for this panel. `toast.error` gets the error's own text — never reworded,
 * never auto-routed (feedback_surface_ai_errors); centralizing it here is what keeps
 * that invariant from drifting across the panel's actions. `fn` performs its own side
 * effects and returns the success copy, so each action owns its wording.
 */
function useAsyncAction(fn: () => Promise<string>): {
  run: () => Promise<void>;
  busy: boolean;
} {
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

/** The panel's retry control: a spinner while busy, the refresh glyph otherwise. */
function BusyButton({
  busy,
  label,
  onClick,
}: {
  busy: boolean;
  label: string;
  onClick: () => void;
}) {
  return (
    <Button size="sm" variant="outline" onClick={onClick} disabled={busy}>
      {busy ? (
        <Loader2 className="h-3.5 w-3.5 animate-spin" />
      ) : (
        <RefreshCw className="h-3.5 w-3.5" />
      )}
      {label}
    </Button>
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
  const { run: handleRetry, busy: retrying } = useAsyncAction(async () => {
    const n = await syncCommands.retryFailedAudioUploads();
    onRetried();
    return n > 0
      ? `Retrying ${n} upload${n === 1 ? "" : "s"}…`
      : "Nothing to retry.";
  });
  const audio = deriveAudioBackup(status);
  // Unmounting this card whenever the counts fall back to zero yanked a whole card out of
  // the middle of the stack and jumped every card below it — while the user was looking
  // at them. Once the lane has reported anything in this panel session it keeps a
  // one-line resting presence instead of disappearing.
  const everReported = useRef(false);
  if (audio.state !== "hidden") everReported.current = true;
  const resting = audio.state === "hidden";
  if (resting && !everReported.current) return null;

  if (resting) {
    return (
      <Card className={`${CARD} view-enter`}>
        <CardContent
          className={`${BODY} flex items-center gap-1.5 text-xs text-muted-foreground`}
        >
          <ShieldCheck className="h-3.5 w-3.5 text-primary" />
          Audio backup — nothing waiting
        </CardContent>
      </Card>
    );
  }

  const icon =
    audio.state === "uploading" ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin text-primary" />
    ) : audio.state === "failed" ? (
      <AlertTriangle className="h-3.5 w-3.5 text-amber-600 dark:text-amber-400" />
    ) : audio.state === "incomplete" ? (
      // Lighter than `failed`: nothing has gone wrong that the user must act on — the
      // walk simply lost a race with the boot write-lock and retries by itself.
      <ShieldAlert className="h-3.5 w-3.5 text-amber-600/80 dark:text-amber-400/80" />
    ) : (
      <ShieldCheck className="h-3.5 w-3.5 text-primary" />
    );

  return (
    <Card className={`${CARD} view-enter`}>
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
          <BusyButton
            busy={retrying}
            label="Retry"
            onClick={() => void handleRetry()}
          />
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Item 1 — persistent crypto-quarantine warning row. Surfaces `cryptoQuarantined` (peer
 * changesets this device pulled but could not decrypt/decode). NON-dismissable: it stays until
 * every quarantined changeset is recovered, because a non-zero count is a potential
 * tamper/corruption signal that must never be silently hidden or auto-dismissed. Retry
 * re-attempts decryption under the CURRENT key epoch (the key-epoch recovery seam). Shown
 * independently of "up to date" — a caught-up device can still carry unreadable changesets.
 * Hidden only when the count is 0.
 */
function CryptoQuarantineWarning({
  status,
  onRetried,
}: {
  status: SyncStatus;
  onRetried: () => void;
}) {
  const { run: handleRetry, busy: retrying } = useAsyncAction(async () => {
    const recovered = await syncCommands.retryCryptoQuarantine();
    onRetried();
    return recovered > 0
      ? `Recovered ${recovered} changeset${recovered === 1 ? "" : "s"}.`
      : "Still unreadable — the key for these changes has not arrived yet.";
  });
  const n = Math.max(0, status.cryptoQuarantined ?? 0);
  if (n === 0) return null;

  const noun = n === 1 ? "changeset" : "changesets";
  return (
    <Alert variant="destructive">
      <AlertTriangle className="h-4 w-4" />
      <AlertTitle>
        {n} unreadable {noun}
      </AlertTitle>
      <AlertDescription className="space-y-2">
        <p>
          {n === 1 ? "A change" : `${n} changes`} synced from your other devices
          could not be decrypted on this device. This can happen when a device's
          key has not been reconciled yet — or, rarely, if data was tampered with
          in transit. It is kept safely and never dropped. Retry once every device
          is approved.
        </p>
        <BusyButton
          busy={retrying}
          label="Retry"
          onClick={() => void handleRetry()}
        />
      </AlertDescription>
    </Alert>
  );
}

/**
 * Fetched-audio cache row (S3.5 auto-fetch): "Fetched audio: N MB — Clear" with a confirm
 * dialog, in the stacked-card idiom. Hidden while the cache is empty (nothing to report).
 * Clearing is safe against in-flight fetches by construction (the Rust command skips live
 * fetch slots and download temps), so the row needs no in-flight special casing — a clear
 * during a fetch simply leaves that part's bytes behind, reflected in the refreshed count.
 */
function FetchedAudioCard() {
  const [stats, setStats] = useState<AudioCacheStats | null>(null);
  const { run: handleClear, busy: clearing } = useAsyncAction(async () => {
    const remaining = await syncCommands.audioCacheClear();
    setStats(remaining);
    return remaining.bytes > 0
      ? `Cleared — ${formatBytes(remaining.bytes)} still in use by an active fetch`
      : "Fetched audio cleared";
  });

  const refresh = useCallback(async () => {
    try {
      const s = await syncCommands.audioCacheStats();
      setStats(s && typeof s.bytes === "number" ? s : null);
    } catch {
      // Command unavailable (older backend / feature off) → hide the row.
      setStats(null);
    }
  }, []);
  useEffect(() => {
    void refresh();
    // The cache grows while the panel is open (auto-fetch runs in the background from any
    // view). Refreshing only on mount meant the row appeared out of nowhere on the NEXT
    // visit to Settings, shoving the cards under it. Same idle cadence the tab itself
    // uses for status.
    const id = window.setInterval(() => void refresh(), FETCHED_AUDIO_POLL_MS);
    return () => window.clearInterval(id);
  }, [refresh]);

  if (!stats || stats.bytes <= 0) return null;

  return (
    // Fades/slides in rather than popping into the middle of the stack the moment the
    // poll first sees bytes.
    <Card className={`${CARD} view-enter`}>
      <CardContent className={`${BODY} flex items-center justify-between gap-3`}>
        <div className="space-y-0.5">
          <p className="text-xs font-medium">
            Fetched audio: {formatBytes(stats.bytes)}
          </p>
          <p className="text-[11px] text-muted-foreground">
            Audio downloaded from your other devices, kept for playback until
            you clear it.
          </p>
        </div>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button size="sm" variant="outline" disabled={clearing}>
              {clearing ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : "Clear"}
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Clear fetched audio?</AlertDialogTitle>
              <AlertDialogDescription>
                Frees {formatBytes(stats.bytes)} on this device. Recordings stay
                safe on the devices that made them and on your sync server — you
                can fetch them again anytime.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction onClick={() => void handleClear()}>
                Clear
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
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
