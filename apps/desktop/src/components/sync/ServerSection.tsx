import { useEffect, useRef } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
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
import { Loader2, Check, Lock, Pencil } from "lucide-react";
import { isValidServerUrl, type RelayConnState } from "@/lib/sync";

/** Distinct heading per typed probe failure. The verbatim `raw` string is rendered
 *  untouched beneath it — never reworded. */
const FAILURE_HEADING: Record<
  "unreachable" | "tls-error" | "not-a-relay",
  string
> = {
  unreachable: "Can't reach server",
  "tls-error": "Certificate problem",
  "not-a-relay": "Not a YapStack sync server",
};

/** Host-only display for the locked identity line — strip scheme + path so the
 *  signed-in user sees `relay.example.com`, not the full URL. */
function hostOnly(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

interface ServerSectionProps {
  /** Live edit buffer (component-local in the parent). */
  serverUrl: string;
  setServerUrl: (v: string) => void;
  /** Persisted `syncConfig.serverUrl` — the baseline dirty is measured against. */
  savedUrl: string;
  signedIn: boolean;
  relayConn: RelayConnState;
  /** Store single-flight probe (button + one-shot blur/paste). */
  probeRelay: (url: string) => Promise<void>;
  /** Cancel-on-edit: bumps the probe token + returns health to idle. */
  resetProbe: () => void;
  /** Persist the current URL without a successful probe (escape hatch). */
  onSaveAnyway: () => void;
  /** Confirmed "change server" while signed in → sign out to unlock the field. */
  onChangeServer: () => void;
}

/**
 * Settings → Sync → Server. Signed-out users enter + test the address; a successful
 * test saves it. Signed-in users see the confirmed host with a "Change server"
 * affordance that signs the device out before unlocking (an expired session is never
 * conflated with a wrong server). Every probe failure surfaces its verbatim `raw`
 * string with a Save-anyway escape hatch.
 */
export function ServerSection({
  serverUrl,
  setServerUrl,
  savedUrl,
  signedIn,
  relayConn,
  probeRelay,
  resetProbe,
  onSaveAnyway,
  onChangeServer,
}: ServerSectionProps) {
  const trimmed = serverUrl.trim();
  const dirty = trimmed !== savedUrl;
  const valid = isValidServerUrl(serverUrl);
  const testing = relayConn.kind === "testing";

  // One-shot per edit: a single auto-probe (blur/paste) fires at most once until
  // the field changes again. Never per-keystroke. `resetProbe()` on every change
  // cancels any in-flight probe (cancel-on-edit).
  const probedThisEdit = useRef(false);
  const pendingPasteProbe = useRef(false);

  const doProbe = () => {
    probedThisEdit.current = true;
    void probeRelay(trimmed);
  };

  const maybeAutoProbe = () => {
    if (!signedIn && dirty && valid && !probedThisEdit.current) doProbe();
  };

  // Paste lands the new value on the next render; probe once it has settled so we
  // read the pasted URL, not the pre-paste buffer.
  useEffect(() => {
    if (pendingPasteProbe.current) {
      pendingPasteProbe.current = false;
      maybeAutoProbe();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serverUrl]);

  const handleChange = (v: string) => {
    setServerUrl(v);
    probedThisEdit.current = false;
    resetProbe();
  };

  if (signedIn) {
    return (
      <div className="flex items-center justify-between gap-3">
        <div className="space-y-0.5">
          <Label className="text-xs">Server</Label>
          <p className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
            <Lock className="h-3 w-3" />
            {hostOnly(savedUrl)}
          </p>
        </div>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button size="sm" variant="ghost" className="text-xs">
              <Pencil className="h-3 w-3" />
              Change server
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Change sync server?</AlertDialogTitle>
              <AlertDialogDescription>
                Changing servers signs this device out and stops syncing. Your
                notes on this device are untouched.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction onClick={onChangeServer}>
                Sign out & change
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </div>
    );
  }

  return (
    <div className="space-y-2">
      <div className="space-y-0.5">
        <Label htmlFor="sync-server-url" className="text-xs">
          Server
        </Label>
        <p className="text-[10px] text-muted-foreground">
          The address of the sync server you host. It only ever stores encrypted
          data.
        </p>
      </div>
      <div className="flex items-center gap-2">
        <Input
          id="sync-server-url"
          aria-label="Sync server URL"
          value={serverUrl}
          onChange={(e) => handleChange(e.target.value)}
          onBlur={maybeAutoProbe}
          onPaste={() => {
            pendingPasteProbe.current = true;
          }}
          placeholder="https://sync.example.com"
          className="text-xs"
        />
        <Button
          size="sm"
          variant="outline"
          className="shrink-0 text-xs"
          onClick={doProbe}
          disabled={testing || !valid}
        >
          {testing ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              Testing…
            </>
          ) : (
            "Test connection"
          )}
        </Button>
      </div>
      <ProbeResult conn={relayConn} savedUrl={savedUrl} onSaveAnyway={onSaveAnyway} />
    </div>
  );
}

/**
 * Probe result under the field. `ok` → one quiet confirmation line (the engine and
 * protocol versions ride in its tooltip, not the copy) plus an advisory when the server
 * outranks this app. Any failure → a plain heading + the VERBATIM `raw` string
 * (`role="alert"`, must-preserve) + a Save-anyway escape hatch.
 */
function ProbeResult({
  conn,
  savedUrl,
  onSaveAnyway,
}: {
  conn: RelayConnState;
  /** Persisted `syncConfig.serverUrl`; used to confirm the probed URL was saved. */
  savedUrl: string;
  onSaveAnyway: () => void;
}) {
  if (conn.kind === "ok") {
    // A signed-out successful probe auto-persists the normalized URL, so say so — the
    // owner reported seeing no "save" for the connection.
    const persisted = conn.normalizedUrl === savedUrl;
    return (
      <div className="space-y-1">
        <p
          className="flex items-center gap-1.5 text-[11px] text-muted-foreground"
          title={`Engine v${conn.engineVersion} · protocol v${conn.protocolVersion}`}
        >
          <Check className="h-3.5 w-3.5 text-emerald-600 dark:text-emerald-400" />
          Connected · {conn.latencyMs} ms{persisted && " · saved"}
        </p>
        {conn.versionAdvisory && (
          <p className="text-[11px] text-amber-600 dark:text-amber-400">
            This server needs YapStack {conn.versionAdvisory.minClientVersion} or
            newer — update the app.
          </p>
        )}
      </div>
    );
  }

  if (
    conn.kind === "unreachable" ||
    conn.kind === "tls-error" ||
    conn.kind === "not-a-relay"
  ) {
    return (
      <div className="space-y-1.5 rounded-md border border-amber-500/30 bg-amber-500/5 p-2.5">
        <p className="text-xs font-medium text-amber-700 dark:text-amber-300">
          {FAILURE_HEADING[conn.kind]}
        </p>
        <p role="alert" className="break-words text-[11px] text-muted-foreground">
          {conn.raw}
        </p>
        <Button size="sm" variant="ghost" className="text-xs" onClick={onSaveAnyway}>
          Save anyway
        </Button>
      </div>
    );
  }

  return null;
}
