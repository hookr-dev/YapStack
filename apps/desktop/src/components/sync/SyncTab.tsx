import { useEffect, useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import {
  CloudOff,
  Cloud,
  Loader2,
  ShieldCheck,
  ExternalLink,
  AlertTriangle,
  Server,
} from "lucide-react";
import { toast } from "sonner";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  syncCommands,
  shouldShowUpgrade,
  isValidServerUrl,
  formatFingerprint,
  DEFAULT_SYNC_SERVER_URL,
} from "@/lib/sync";
import { SignupDialog } from "./SignupDialog";
import { LoginDialog } from "./LoginDialog";
import { DeviceApprovalDialog } from "./DeviceApprovalDialog";

/**
 * "Connect to YapStack Sync" — the settings surface for E2E sync. Signed-out
 * users see the server field + create/sign-in; signed-in users see connection
 * status, the device roster, an enable-sync action (which runs crr_migrate on a
 * DB copy), and — only when the relay advertises a billing_url — an upgrade
 * link. Connection/auth errors are surfaced verbatim, never auto-routed.
 */
export function SyncTab() {
  const syncConfig = useAppStore((s) => s.syncConfig);
  const syncStatus = useAppStore((s) => s.syncStatus);
  const setSyncConfig = useAppStore((s) => s.setSyncConfig);
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);
  const refreshSyncStatus = useAppStore((s) => s.refreshSyncStatus);

  const [serverUrl, setServerUrl] = useState(syncConfig.serverUrl);
  const [enabling, setEnabling] = useState(false);

  useEffect(() => {
    void refreshSyncStatus();
  }, [refreshSyncStatus]);

  const signedIn = !!syncStatus?.email;
  const phase = syncStatus?.phase ?? "disconnected";
  const serverDirty = serverUrl.trim() !== syncConfig.serverUrl;
  const serverValid = isValidServerUrl(serverUrl);

  const handleSaveServer = () => {
    if (!serverValid) {
      toast.error("Enter a valid http(s) server URL.");
      return;
    }
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
      toast.success("Signed out of sync.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  const pendingDevices = (syncStatus?.roster ?? []).filter(
    (d) => d.pending && !d.isSelf,
  );

  return (
    <div className="space-y-5">
      {/* Header + status */}
      <div className="flex items-start justify-between gap-3">
        <div className="space-y-1">
          <div className="flex items-center gap-2">
            {signedIn ? (
              <Cloud className="h-4 w-4 text-primary" />
            ) : (
              <CloudOff className="h-4 w-4 text-muted-foreground" />
            )}
            <h3 className="text-sm font-semibold">YapStack Sync</h3>
            <StatusBadge phase={phase} signedIn={signedIn} />
          </div>
          <p className="text-xs text-muted-foreground max-w-md">
            End-to-end encrypted sync across your devices. Your notes and audio
            are encrypted on this device before they ever reach the relay — the
            server never sees plaintext.
          </p>
        </div>
      </div>

      {/* Verbatim connection error (never auto-routed). */}
      {syncStatus?.lastError && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertTitle>Connection problem</AlertTitle>
          <AlertDescription>{syncStatus.lastError}</AlertDescription>
        </Alert>
      )}

      <Separator />

      {/* Server URL */}
      <div className="space-y-2">
        <Label htmlFor="sync-server-url" className="text-xs">
          Relay server
        </Label>
        <div className="flex items-center gap-2">
          <div className="relative flex-1">
            <Server className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              id="sync-server-url"
              value={serverUrl}
              onChange={(e) => setServerUrl(e.target.value)}
              placeholder={DEFAULT_SYNC_SERVER_URL}
              className="pl-7 text-xs"
              disabled={signedIn}
            />
          </div>
          {serverDirty && !signedIn && (
            <Button size="sm" variant="outline" onClick={handleSaveServer}>
              Save
            </Button>
          )}
        </div>
        <p className="text-[11px] text-muted-foreground">
          Default is the hosted relay. Paste your own URL to use a self-hosted
          server. Sign out to change this.
        </p>
      </div>

      <Separator />

      {/* Signed-out: create / sign in */}
      {!signedIn && (
        <div className="space-y-3">
          <p className="text-xs text-muted-foreground">
            Sign in on this device, or create an account to start syncing.
          </p>
          <div className="flex items-center gap-2">
            <SignupDialog
              serverUrl={serverValid ? serverUrl.trim() : syncConfig.serverUrl}
            />
            <LoginDialog
              serverUrl={serverValid ? serverUrl.trim() : syncConfig.serverUrl}
            />
          </div>
        </div>
      )}

      {/* Signed-in: account + roster + enable */}
      {signedIn && syncStatus && (
        <div className="space-y-4">
          <div className="flex items-center justify-between">
            <div className="space-y-0.5">
              <div className="flex items-center gap-1.5 text-xs font-medium">
                <ShieldCheck className="h-3.5 w-3.5 text-primary" />
                {syncStatus.email}
              </div>
              {syncStatus.deviceFingerprint && (
                <p className="text-[11px] text-muted-foreground font-mono">
                  This device: {formatFingerprint(syncStatus.deviceFingerprint)}
                </p>
              )}
            </div>
            <Button size="sm" variant="ghost" onClick={handleSignOut}>
              Sign out
            </Button>
          </div>

          {/* Pending device approvals — the roster gate (§7.5). */}
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

          {/* Device roster */}
          {syncStatus.roster.length > 0 && (
            <div className="space-y-1.5">
              <div className="flex items-center justify-between">
                <Label className="text-xs">Devices</Label>
                {syncStatus.rosterFingerprint && (
                  <span className="text-[11px] text-muted-foreground font-mono">
                    roster {formatFingerprint(syncStatus.rosterFingerprint)}
                    {syncStatus.vaultKeyEpoch != null &&
                      ` · epoch ${syncStatus.vaultKeyEpoch}`}
                  </span>
                )}
              </div>
              <div className="rounded-lg border divide-y">
                {syncStatus.roster.map((d) => (
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

          <Separator />

          {/* Enable sync — runs crr_migrate on a DB copy. */}
          {!syncStatus.syncEnabled ? (
            <div className="space-y-2">
              <p className="text-xs text-muted-foreground">
                Enabling sync prepares a copy of your library for encrypted
                sync. Your original data is untouched during preparation.
              </p>
              <Button size="sm" onClick={handleEnable} disabled={enabling}>
                {enabling ? (
                  <>
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    Preparing your library for sync…
                  </>
                ) : (
                  "Enable sync on this device"
                )}
              </Button>
            </div>
          ) : (
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <ShieldCheck className="h-3.5 w-3.5 text-primary" />
              Sync is enabled on this device.
            </div>
          )}

          {/* Upgrade — only when a billing_url is advertised. */}
          {shouldShowUpgrade(syncStatus) && syncStatus.billingUrl && (
            <>
              <Separator />
              <div className="flex items-center justify-between rounded-lg border bg-card p-3">
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
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}

function StatusBadge({
  phase,
  signedIn,
}: {
  phase: string;
  signedIn: boolean;
}) {
  if (phase === "error") {
    return (
      <Badge variant="destructive" className="text-[10px]">
        error
      </Badge>
    );
  }
  if (phase === "preparing") {
    return (
      <Badge variant="secondary" className="text-[10px]">
        preparing
      </Badge>
    );
  }
  if (signedIn && phase === "connected") {
    return (
      <Badge variant="secondary" className="text-[10px]">
        connected
      </Badge>
    );
  }
  return (
    <Badge variant="outline" className="text-[10px]">
      not connected
    </Badge>
  );
}
