import { useCallback, useEffect, useRef, useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Progress } from "@/components/ui/progress";
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
  Loader2,
  ShieldCheck,
  ShieldAlert,
  ExternalLink,
  AlertTriangle,
  ChevronsUpDown,
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
  formatSecurityCode,
  formatBytes,
  deriveAudioBackup,
} from "@/lib/sync";
import type { AudioCacheStats, SyncStatus } from "@/lib/sync";
import { deriveSyncDisplay, type SyncDisplay } from "@/lib/syncDisplay";
import { ServerSection } from "./ServerSection";
import { SignupDialog } from "./SignupDialog";
import { LoginDialog } from "./LoginDialog";
import { DeviceApprovalDialog } from "./DeviceApprovalDialog";

/** Idle refresh cadence for the panel's derived rows — matches the status poll below so
 *  the whole page moves on one clock. */
const IDLE_POLL_MS = 5000;
const FETCHED_AUDIO_POLL_MS = IDLE_POLL_MS;

/**
 * Settings → Sync. Built from the same parts as the other settings tabs — labelled
 * sections split by separators, controls right-aligned, lists as bordered rows — so
 * it reads as one more settings page rather than a console.
 *
 * While a device is being set up the page is a guided flow: a setup rail under the
 * heading names the steps (Server → Sign in → [Approval] → Turn on) and a callout for
 * the CURRENT step sits above the Server/Account sections. Once sync is on it settles
 * into the steady state: one status line, then the audio lanes and any warnings.
 *
 * Connection health (probe → `relayConn`) is a separate axis from the sync phase; one
 * `deriveSyncDisplay` derivation feeds the badge, the status line and the sidebar
 * glyph so surfaces can never disagree. Connection/auth errors surface verbatim,
 * never auto-routed (feedback_surface_ai_errors).
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
  // transition INTO syncing, and — for a device waiting on approval — is how the
  // page notices the approval landed. Only runs while this tab is mounted.
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
    // Escape hatch: persist the URL without a successful probe. Signed-out only — a
    // signed-in device's URL is locked behind the change-server flow.
    setSyncConfig({ serverUrl: serverUrl.trim() });
    toast.success("Server saved.");
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
  // upgrade row and its click handler share one decision instead of re-guarding.
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
          <code className="rounded bg-muted px-1 py-0.5 font-mono">
            --features sync
          </code>{" "}
          and point it at your own server: see{" "}
          <span className="font-medium text-foreground">docs/self-hosting.md</span>.
        </p>
      </div>
    );
  }

  const settingUp = !signedIn || !syncEnabled;

  return (
    <>
      {/* Heading row: what this is + the one status badge. */}
      <div className="space-y-2">
        <div className="flex items-start justify-between gap-3">
          <div className="space-y-0.5">
            <Label className="text-xs">Sync</Label>
            <p className="text-[10px] text-muted-foreground">
              Keep your notes and recordings the same on every device. Everything
              is encrypted before it leaves this device.
            </p>
          </div>
          {display.state !== "off" && <StatusBadge display={display} />}
        </div>
        {settingUp && (
          <SetupRail
            relayReady={relayReady}
            signedIn={signedIn}
            selfPending={selfPending}
            syncEnabled={syncEnabled}
          />
        )}
        {signedIn && syncEnabled && syncStatus && (
          <SyncStatusLine status={syncStatus} display={display} />
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

      {/* Item 1 — persistent crypto-quarantine warning. Peer changes this device pulled but
          could not decrypt; NON-dismissable (a potential tamper signal), with a Retry that
          re-attempts once the key epoch is reconciled. Hidden while the count is 0. */}
      {signedIn && syncEnabled && syncStatus && (
        <UnreadableChangesWarning
          status={syncStatus}
          onRetried={() => void refreshSyncStatus()}
        />
      )}

      {/* Current step while setting up: waiting on an approval, or ready to turn on. */}
      {signedIn && syncStatus && !syncEnabled &&
        (selfPending ? (
          <WaitingApprovalCallout status={syncStatus} />
        ) : (
          <TurnOnCallout status={syncStatus} enabling={enabling} onEnable={handleEnable} />
        ))}

      <Separator />

      <ServerSection
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

      <Separator />

      <AccountSection
        status={syncStatus}
        signedIn={signedIn}
        serverUrl={serverUrl}
        savedServerUrl={syncConfig.serverUrl}
      />

      {signedIn && syncStatus && (
        <>
          <Separator />
          <DevicesSection status={syncStatus} pendingDevices={pendingDevices} />
        </>
      )}

      {/* Audio backup lane — DISTINCT from changeset sync (S2). Only once enabled and
          when there is something to report. */}
      {signedIn && syncEnabled && syncStatus && (
        <AudioBackupSection
          status={syncStatus}
          onRetried={() => void refreshSyncStatus()}
        />
      )}

      {/* Audio pulled from other devices is keep-until-clear; this row is the clear.
          Hidden while the cache is empty. */}
      {signedIn && syncEnabled && <DownloadedAudioSection />}

      {/* Upgrade — only when a billing_url is advertised. */}
      {billingUrl && (
        <>
          <Separator />
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
        </>
      )}

      {/* Advanced — the same collapsible the Audio tab uses; not a red danger zone. */}
      {signedIn && (
        <>
          <Separator />
          <Collapsible>
            <CollapsibleTrigger className="flex w-full items-center justify-between">
              <Label className="text-xs text-muted-foreground pointer-events-none">
                Advanced
              </Label>
              <ChevronsUpDown className="h-3.5 w-3.5 text-muted-foreground/60" />
            </CollapsibleTrigger>
            <CollapsibleContent className="pt-3">
              <div className="flex items-center justify-between gap-3">
                <div className="space-y-0.5">
                  <Label className="text-xs">Sign out</Label>
                  <p className="text-[10px] text-muted-foreground">
                    Stops syncing on this device. Your notes stay here.
                  </p>
                </div>
                <AlertDialog>
                  <AlertDialogTrigger asChild>
                    <Button size="sm" variant="outline" className="text-xs">
                      Sign out
                    </Button>
                  </AlertDialogTrigger>
                  <AlertDialogContent>
                    <AlertDialogHeader>
                      <AlertDialogTitle>Sign out of sync on this device?</AlertDialogTitle>
                      <AlertDialogDescription>
                        Your notes on this device are untouched, and signing back
                        in later won't need a new approval.
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
              </div>
            </CollapsibleContent>
          </Collapsible>
        </>
      )}
    </>
  );
}

type StepState = "done" | "current" | "todo";

/**
 * The setup rail: a compact "you are here" for the device-setup journey, shown only
 * until sync is on. Server → Sign in → [Approval, only for a device that joined an
 * existing account and is waiting] → Turn on.
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

/** The one place a code is shown for a person to compare: large, spaced, centered. */
function CodeBlock({
  label,
  code,
  footnote,
}: {
  label: string;
  code: string;
  footnote?: string;
}) {
  return (
    <div className="rounded-md bg-muted/50 p-3 text-center">
      <p className="mb-1 text-[10px] text-muted-foreground">{label}</p>
      <p className="font-mono text-base tracking-wide">{code}</p>
      {footnote && <p className="mt-1.5 text-[10px] text-muted-foreground">{footnote}</p>}
    </div>
  );
}

/**
 * The joining device's side of the approval ceremony (§7.5). It cannot turn on sync
 * until an existing device approves it, so this callout IS the page for now: what to
 * do on the other device, this device's code large enough to read across a desk, and
 * the security code the approver's dialog shows for the out-of-band check (§7.5.4).
 * The panel's status poll picks the approval up by itself.
 */
function WaitingApprovalCallout({ status }: { status: SyncStatus }) {
  const approvers = status.roster.filter((d) => !d.isSelf && !d.pending);
  const named = approvers.map((d) => d.label).filter((l): l is string => !!l);
  const security = formatSecurityCode(status.rosterFingerprint, status.vaultKeyEpoch);

  return (
    <div className="view-enter space-y-3 rounded-lg border border-amber-500/30 p-3">
      <div className="space-y-0.5">
        <p className="flex items-center gap-1.5 text-xs font-medium">
          <Hourglass className="h-3.5 w-3.5 text-amber-600 dark:text-amber-400" />
          Waiting for approval
        </p>
        <p className="text-[11px] text-muted-foreground">
          {approvers.length === 0
            ? "No other approved device is on this account yet, so nothing can approve this one. Open YapStack on a device you have used with this account before."
            : `Sync starts once one of your other devices approves this one. On ${
                named.length > 0 ? named.join(" or ") : "a device that is already signed in"
              }, open Settings → Sync → Devices and review the request.`}
        </p>
      </div>
      <CodeBlock
        label="This device"
        code={status.deviceFingerprint ? formatFingerprint(status.deviceFingerprint) : "—"}
        footnote={security ?? undefined}
      />
      <p className="text-[11px] text-muted-foreground">
        The approving device shows the same codes. If either differs, don't approve.
      </p>
      <p className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" />
        Checking for approval — this page updates by itself.
      </p>
    </div>
  );
}

/**
 * Turn on sync (signed-in, approved, not yet enabled). The copy says what will
 * actually happen to THIS library: a first device uploads it; a device joining an
 * account that already has approved peers downloads theirs and merges. Both keep a
 * full pre-sync backup first.
 */
function TurnOnCallout({
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
    <div className="view-enter flex items-center justify-between gap-3 rounded-lg border p-3">
      <div className="space-y-0.5">
        <p className="text-xs font-medium">Turn on sync</p>
        <p className="text-[11px] text-muted-foreground">
          {hasPeers
            ? "Notes and recordings from your other devices download here, and anything already on this device is merged in. A full backup of this library is kept first."
            : "This library is prepared for sync and uploaded to your server, encrypted on this device. A full backup copy is kept first."}
        </p>
      </div>
      <Button size="sm" className="shrink-0 text-xs" onClick={onEnable} disabled={enabling}>
        {enabling ? (
          <>
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            Preparing…
          </>
        ) : (
          "Turn on sync"
        )}
      </Button>
    </div>
  );
}

/**
 * Account: signed-out offers create/sign-in, gated on a usable server URL (an
 * account lives on a server, so there is nothing to sign in to before one is set);
 * signed-in shows the account email. An expired session gets a warm amber "sign in
 * again" CTA — never destructive, never conflated with a wrong server.
 */
function AccountSection({
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

  if (!signedIn) {
    return (
      <div className="flex items-center justify-between gap-3">
        <div className="space-y-0.5">
          <Label className="text-xs">Account</Label>
          <p className="text-[10px] text-muted-foreground">
            {relayReady
              ? "Sign in to your account, or create one on this server."
              : "Enter your server above first — your account lives on the server you choose."}
          </p>
        </div>
        {relayReady && (
          <div className="flex shrink-0 items-center gap-2">
            <SignupDialog serverUrl={dialogUrl} />
            <LoginDialog serverUrl={dialogUrl} />
          </div>
        )}
      </div>
    );
  }

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-3">
        <div className="space-y-0.5">
          <Label className="text-xs">Account</Label>
          <p className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
            <ShieldCheck className="h-3 w-3 text-primary" />
            {status?.email}
          </p>
        </div>
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
    </div>
  );
}

/**
 * Devices (signed-in): requests first — each requesting device as a named row with a
 * Review button that opens the ceremony dialog — then the approved devices by name.
 * A pending peer appears only in the request list, never twice.
 */
function DevicesSection({
  status,
  pendingDevices,
}: {
  status: SyncStatus;
  pendingDevices: SyncStatus["roster"];
}) {
  const roster = status.roster.filter((d) => !(d.pending && !d.isSelf));
  return (
    <div className="space-y-2">
      <div className="space-y-0.5">
        <Label className="text-xs">Devices</Label>
        <p className="text-[10px] text-muted-foreground">
          Every device signed in to this account. A new device syncs only after
          one you already trust approves it.
        </p>
      </div>

      {pendingDevices.length > 0 && (
        <div className="space-y-2 rounded-lg border border-amber-500/30 bg-amber-500/5 p-3">
          <p className="flex items-center gap-1.5 text-xs font-medium">
            <AlertTriangle className="h-3.5 w-3.5 text-amber-600 dark:text-amber-400" />
            {pendingDevices.length === 1
              ? "A new device is requesting access"
              : `${pendingDevices.length} devices are requesting access`}
          </p>
          <p className="text-[11px] text-muted-foreground">
            Approve only if you just signed in on that device yourself, and its
            code matches what that device shows.
          </p>
          <div className="flex flex-col gap-1.5">
            {pendingDevices.map((d) => (
              <div
                key={d.fingerprint}
                className="flex items-center justify-between gap-3"
              >
                <div className="min-w-0">
                  <p className="truncate text-xs font-medium">
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
        </div>
      )}

      {roster.length > 0 && (
        <div className="divide-y divide-border rounded-md border border-border bg-card">
          {roster.map((d) => (
            <div
              key={d.fingerprint}
              className="flex items-center justify-between gap-3 px-3 py-2"
            >
              <p className="truncate text-xs">{d.label ?? "Unnamed device"}</p>
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
    </div>
  );
}

/**
 * The enabled-device steady-state line, driven by `deriveSyncDisplay`. During a big
 * initial sync a determinate <Progress> rides above it, capped at 99% while bytes
 * remain (Syncthing "never 100% until done"). Total = entries acked this session +
 * entries still pending; degenerate totals fall back to the plain text line.
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

/**
 * One-shot async button action: owns the busy flag and the SINGLE verbatim failure
 * surfacing for this panel. `toast.error` gets the error's own text — never reworded,
 * never auto-routed (feedback_surface_ai_errors). `fn` performs its own side effects
 * and returns the success copy, so each action owns its wording.
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
    <Button size="sm" variant="outline" className="shrink-0 text-xs" onClick={onClick} disabled={busy}>
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
 * Audio backup (S2): the audio-upload lane, surfaced DISTINCTLY from changeset sync
 * (never merged, per repo posture). An in-flight line (with the existing-library
 * backfill nuance), an "all backed up" resting line, or a failed state with a manual
 * Retry — omitted entirely when there is nothing to report.
 */
function AudioBackupSection({
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
  // Unmounting this section whenever the counts fall back to zero yanked it out of the
  // middle of the page and jumped everything below it — while the user was looking.
  // Once the lane has reported anything in this panel session it keeps a one-line
  // resting presence instead of disappearing.
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
      // Lighter than `failed`: nothing has gone wrong that the user must act on — the
      // walk simply lost a race with the boot write-lock and retries by itself.
      <ShieldAlert className="h-3 w-3 text-amber-600/80 dark:text-amber-400/80" />
    ) : (
      <ShieldCheck className="h-3 w-3 text-primary" />
    );

  return (
    <>
      <Separator />
      <div className="view-enter flex items-center justify-between gap-3">
        <div className="space-y-0.5">
          <Label className="text-xs">Audio backup</Label>
          <p
            className={`flex items-center gap-1.5 text-[10px] ${
              audio.state === "failed"
                ? "text-amber-600 dark:text-amber-400"
                : "text-muted-foreground"
            }`}
          >
            {icon}
            {resting ? "Audio backup — nothing waiting" : audio.label}
          </p>
        </div>
        {audio.state === "failed" && (
          <BusyButton busy={retrying} label="Retry" onClick={() => void handleRetry()} />
        )}
      </div>
    </>
  );
}

/**
 * Item 1 — persistent warning for peer changes this device pulled but could not
 * decrypt/decode. NON-dismissable: it stays until every one is recovered, because a
 * non-zero count is a potential tamper/corruption signal that must never be silently
 * hidden. Retry re-attempts decryption under the CURRENT key epoch (the key-epoch
 * recovery seam). Shown independently of "up to date". Hidden only when the count is 0.
 */
function UnreadableChangesWarning({
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
      ? `Recovered ${recovered} change${recovered === 1 ? "" : "s"}.`
      : "Still unreadable — the key for these changes hasn't arrived yet.";
  });
  const n = Math.max(0, status.cryptoQuarantined ?? 0);
  if (n === 0) return null;

  return (
    <Alert variant="destructive">
      <AlertTriangle className="h-4 w-4" />
      <AlertTitle>
        {n === 1 ? "1 change couldn't be read" : `${n} changes couldn't be read`}
      </AlertTitle>
      <AlertDescription className="space-y-2">
        <p>
          {n === 1 ? "A change" : "Some changes"} from your other devices couldn't
          be decrypted on this device. That usually means a device's key hasn't
          caught up yet — or, rarely, that something was tampered with in
          transit. Nothing is dropped. Retry once every device is approved.
        </p>
        <BusyButton busy={retrying} label="Retry" onClick={() => void handleRetry()} />
      </AlertDescription>
    </Alert>
  );
}

/**
 * Downloaded-audio row: "N MB from your other devices — Clear" with a confirm dialog.
 * Hidden while the cache is empty. Clearing is safe against in-flight fetches by
 * construction (the Rust command skips live fetch slots and download temps), so the
 * row needs no in-flight special casing — a clear during a fetch simply leaves that
 * part's bytes behind, reflected in the refreshed count.
 */
function DownloadedAudioSection() {
  const [stats, setStats] = useState<AudioCacheStats | null>(null);
  const { run: handleClear, busy: clearing } = useAsyncAction(async () => {
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
      // Command unavailable (older backend / feature off) → hide the row.
      setStats(null);
    }
  }, []);
  useEffect(() => {
    void refresh();
    // The cache grows while the panel is open (auto-fetch runs in the background from
    // any view). Refreshing only on mount meant the row appeared out of nowhere on the
    // NEXT visit to Settings. Same idle cadence the tab itself uses for status.
    const id = window.setInterval(() => void refresh(), FETCHED_AUDIO_POLL_MS);
    return () => window.clearInterval(id);
  }, [refresh]);

  if (!stats || stats.bytes <= 0) return null;

  return (
    <>
      <Separator />
      {/* Fades in rather than popping into the page the moment the poll first sees bytes. */}
      <div className="view-enter flex items-center justify-between gap-3">
        <div className="space-y-0.5">
          <Label className="text-xs">Downloaded audio</Label>
          <p className="text-[10px] text-muted-foreground">
            {formatBytes(stats.bytes)} from your other devices, kept for playback
            until you clear it.
          </p>
        </div>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button size="sm" variant="outline" className="shrink-0 text-xs" disabled={clearing}>
              {clearing ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : "Clear"}
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Clear downloaded audio?</AlertDialogTitle>
              <AlertDialogDescription>
                Frees {formatBytes(stats.bytes)} on this device. Recordings stay
                safe on the devices that made them and on your sync server — you
                can download them again anytime.
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
      </div>
    </>
  );
}

/** Heading status chip — the shared derivation's label with a tone-mapped Badge. */
function StatusBadge({ display }: { display: SyncDisplay }) {
  if (display.tone === "destructive") {
    return (
      <Badge variant="destructive" className="shrink-0 text-[10px]">
        {display.label}
      </Badge>
    );
  }
  if (display.tone === "amber") {
    return (
      <Badge
        variant="outline"
        className="shrink-0 border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400 text-[10px]"
      >
        {display.label}
      </Badge>
    );
  }
  if (display.tone === "active") {
    return (
      <Badge variant="secondary" className="shrink-0 text-[10px]">
        {display.label}
      </Badge>
    );
  }
  return (
    <Badge variant="outline" className="shrink-0 text-[10px]">
      {display.label}
    </Badge>
  );
}
