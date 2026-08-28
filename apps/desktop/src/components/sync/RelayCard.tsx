import { useEffect, useRef } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardAction,
  CardContent,
} from "@/components/ui/card";
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
import { Loader2, Server, Check, Lock, Pencil } from "lucide-react";
import { isValidServerUrl, type RelayConnState } from "@/lib/sync";

const CARD = "py-4 gap-3";
const HEAD = "px-4";
const BODY = "px-4";

/** Distinct heading per typed probe failure (plan §2). The verbatim `raw`
 *  string is rendered untouched beneath it — never reworded. */
const FAILURE_HEADING: Record<
  "unreachable" | "tls-error" | "not-a-relay",
  string
> = {
  unreachable: "Can't reach server",
  "tls-error": "Certificate problem",
  "not-a-relay": "Not a YapStack relay",
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

interface RelayCardProps {
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
  /** Persist the current URL without a successful probe (escape hatch, §0.5). */
  onSaveAnyway: () => void;
  /** Confirmed "change server" while signed in → sign out to unlock the field. */
  onChangeServer: () => void;
}

/**
 * Relay-server card: the URL control stays first-class (plan §0.4) while only the
 * probe RESULT collapses on success (LiveSync pattern). Signed-out users edit +
 * test the URL; signed-in users see a locked host identity line whose "Change
 * server" affordance signs the device out before unlocking (plan §2). Every probe
 * failure surfaces its verbatim `raw` string with a Save-anyway escape hatch.
 */
export function RelayCard({
  serverUrl,
  setServerUrl,
  savedUrl,
  signedIn,
  relayConn,
  probeRelay,
  resetProbe,
  onSaveAnyway,
  onChangeServer,
}: RelayCardProps) {
  const trimmed = serverUrl.trim();
  const dirty = trimmed !== savedUrl;
  const valid = isValidServerUrl(serverUrl);
  const testing = relayConn.kind === "testing";

  // One-shot per edit: a single auto-probe (blur/paste) fires at most once until
  // the field changes again. Never per-keystroke (plan §5). `resetProbe()` on
  // every change cancels any in-flight probe (cancel-on-edit).
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

  return (
    <Card className={CARD}>
      <CardHeader className={HEAD}>
        <CardTitle className="flex items-center gap-1.5 text-sm">
          <Server className="h-3.5 w-3.5 text-muted-foreground" />
          Relay server
        </CardTitle>
        <CardDescription className="text-xs">
          Point YapStack at a blind relay. The server never sees your plaintext.
        </CardDescription>
        <CardAction>
          <ConnectionBadge conn={relayConn} />
        </CardAction>
      </CardHeader>

      <CardContent className={`${BODY} space-y-2`}>
        {signedIn ? (
          // Locked identity line — the confirmed relay host, edit gated behind
          // an explicit sign-out (plan §2: never conflate expired session with
          // wrong server).
          <div className="flex items-center justify-between gap-2">
            <div className="flex items-center gap-1.5 text-xs font-medium">
              <Lock className="h-3.5 w-3.5 text-muted-foreground" />
              <span className="font-mono">{hostOnly(savedUrl)}</span>
            </div>
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button size="sm" variant="ghost">
                  <Pencil className="h-3 w-3" />
                  Change server
                </Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Change relay server?</AlertDialogTitle>
                  <AlertDialogDescription>
                    Changing your relay server signs out this device and stops
                    syncing. Your local data is untouched.
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
        ) : (
          <>
            <div className="flex items-center gap-2">
              <div className="relative flex-1">
                <Server className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                <Input
                  id="sync-server-url"
                  aria-label="Relay server URL"
                  value={serverUrl}
                  onChange={(e) => handleChange(e.target.value)}
                  onBlur={maybeAutoProbe}
                  onPaste={() => {
                    pendingPasteProbe.current = true;
                  }}
                  placeholder="https://sync.yapstack.app"
                  className="pl-7 text-xs"
                />
              </div>
              <Button
                size="sm"
                variant="outline"
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

            <ProbeResult
              conn={relayConn}
              canSave={!signedIn}
              savedUrl={savedUrl}
              onSaveAnyway={onSaveAnyway}
            />
          </>
        )}
      </CardContent>
    </Card>
  );
}

/** CardAction health chip — never green (plan §0.7: the green ✓ lives only in the
 *  transient result line). Amber for the three typed failures, muted otherwise. */
function ConnectionBadge({ conn }: { conn: RelayConnState }) {
  if (conn.kind === "testing") {
    return (
      <Badge variant="outline" className="text-[10px]">
        Testing…
      </Badge>
    );
  }
  if (conn.kind === "ok") {
    return (
      <Badge variant="secondary" className="text-[10px]">
        Connected
      </Badge>
    );
  }
  if (
    conn.kind === "unreachable" ||
    conn.kind === "tls-error" ||
    conn.kind === "not-a-relay"
  ) {
    return (
      <Badge
        variant="outline"
        className="border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400 text-[10px]"
      >
        Not connected
      </Badge>
    );
  }
  return null;
}

/**
 * Collapse-on-success probe result. `ok` → a single muted confirmation line
 * (plus an amber advisory when the relay outranks this app). Any failure →
 * auto-expanded block: a distinct heading + the VERBATIM `raw` string
 * (`role="alert"`, must-preserve) + a Save-anyway escape hatch.
 */
function ProbeResult({
  conn,
  canSave,
  savedUrl,
  onSaveAnyway,
}: {
  conn: RelayConnState;
  canSave: boolean;
  /** Persisted `syncConfig.serverUrl`; used to confirm the probed URL was saved. */
  savedUrl: string;
  onSaveAnyway: () => void;
}) {
  if (conn.kind === "ok") {
    // A signed-out successful probe auto-persists the normalized URL into syncConfig
    // (store test-and-save, §0.1). Surface that explicitly — the owner reported seeing
    // no "save" for the relay connection — by appending "· saved" once the persisted
    // value matches the probed URL.
    const persisted = conn.normalizedUrl === savedUrl;
    return (
      <div className="space-y-1">
        <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Check className="h-3.5 w-3.5 text-emerald-600 dark:text-emerald-400" />
          Connected — engine v{conn.engineVersion} · protocol v
          {conn.protocolVersion} · {conn.latencyMs} ms
          {persisted && " · saved"}
        </p>
        {conn.versionAdvisory && (
          <p className="text-[11px] text-amber-600 dark:text-amber-400">
            Server requires app version ≥ {conn.versionAdvisory.minClientVersion}{" "}
            — update YapStack
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
        <p
          role="alert"
          className="text-[11px] text-muted-foreground font-mono break-words"
        >
          {conn.raw}
        </p>
        {canSave && (
          <Button size="sm" variant="ghost" onClick={onSaveAnyway}>
            Save anyway
          </Button>
        )}
      </div>
    );
  }

  return null;
}
