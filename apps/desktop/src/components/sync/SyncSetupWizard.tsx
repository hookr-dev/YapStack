import { useEffect, useMemo, useRef, useState } from "react";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { Dialog, DialogOverlay, DialogPortal } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { ProgressDots } from "@/components/ProgressDots";
import {
  Check,
  ChevronLeft,
  ChevronRight,
  Copy,
  Hourglass,
  KeyRound,
  Loader2,
  Server,
  ShieldAlert,
  ShieldCheck,
  X,
} from "lucide-react";
import { toast } from "sonner";
import { useAppStore } from "@/stores/appStore";
import {
  formatFingerprint,
  formatRecoveryCode,
  formatSecurityCode,
  isSelfPending,
  isValidRecoveryCode,
  isValidServerUrl,
  normalizeCode,
  syncCommands,
  type SignupResult,
  type SyncStatus,
} from "@/lib/sync";
import { DevicesIllustration } from "./DevicesIllustration";

type Step = "intro" | "server" | "account" | "approval" | "turn-on" | "done";

/** How often the approval step asks whether the other device has approved us. */
const APPROVAL_POLL_MS = 3000;

/**
 * Where to start. `"auto"` picks up wherever setup was left (a signed-in device lands on
 * approval or turn-on; a saved server lands on the server step; otherwise the intro).
 * `"sign-in"` is the expired-session re-login: straight to the account step with the
 * email locked, and the wizard closes once signed in.
 */
export type WizardStart = "auto" | "sign-in";

/**
 * Settings → Sync setup wizard. Same shell and rhythm as first-run onboarding (dots,
 * slide transitions, one idea per step, a big icon, a full-width primary action):
 *
 *   intro → server → account → [approval] → turn on → done
 *
 * Every step calls the same `syncCommands` the settings page used to reach through
 * dialogs; the wizard is the ONLY place a server is entered or an account signed in.
 * The approval step polls status and advances by itself when the other device
 * approves. The recovery-code capture inside "create account" cannot be dismissed
 * (CRYPTO_SPEC §6.1: lost password + lost code = lost data).
 */
export function SyncSetupWizard({
  open,
  onOpenChange,
  start = "auto",
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  start?: WizardStart;
}) {
  const syncConfig = useAppStore((s) => s.syncConfig);
  const syncStatus = useAppStore((s) => s.syncStatus);

  const [step, setStep] = useState<Step>("intro");
  const [direction, setDirection] = useState<"forward" | "backward">("forward");
  // The recovery-code capture owns dismissal: while a fresh code is on screen the
  // wizard cannot be closed.
  const [locked, setLocked] = useState(false);

  // Resume where setup was left, each time the wizard opens.
  useEffect(() => {
    if (!open) return;
    setDirection("forward");
    setLocked(false);
    if (start === "sign-in") {
      setStep("account");
      return;
    }
    const signedIn = !!syncStatus?.email;
    if (signedIn && isSelfPending(syncStatus)) setStep("approval");
    else if (signedIn) setStep("turn-on");
    else if (isValidServerUrl(syncConfig.serverUrl)) setStep("server");
    else setStep("intro");
    // Only on open: later status changes are handled by the steps themselves.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const steps = useMemo<Step[]>(() => {
    if (start === "sign-in") return ["account", "done"];
    const base: Step[] = ["intro", "server", "account"];
    if (step === "approval" || isSelfPending(syncStatus)) base.push("approval");
    base.push("turn-on", "done");
    return base;
  }, [start, step, syncStatus]);

  const go = (next: Step) => {
    setDirection(steps.indexOf(next) < steps.indexOf(step) ? "backward" : "forward");
    setStep(next);
  };

  const close = () => {
    if (locked) {
      toast.error("Save your recovery code before closing.");
      return;
    }
    onOpenChange(false);
  };

  const afterSignIn = (status: SyncStatus) => {
    if (start === "sign-in") {
      go("done");
      return;
    }
    go(isSelfPending(status) ? "approval" : "turn-on");
  };

  let content;
  switch (step) {
    case "intro":
      content = <IntroStep onNext={() => go("server")} />;
      break;
    case "server":
      content = (
        <ServerStep onBack={() => go("intro")} onNext={() => go("account")} />
      );
      break;
    case "account":
      content = (
        <AccountStep
          reLogin={start === "sign-in"}
          onBack={start === "sign-in" ? undefined : () => go("server")}
          onSignedIn={afterSignIn}
          onLockChange={setLocked}
        />
      );
      break;
    case "approval":
      content = (
        <ApprovalStep
          onApproved={() => go("turn-on")}
          onCancel={close}
        />
      );
      break;
    case "turn-on":
      content = <TurnOnStep onDone={() => go("done")} />;
      break;
    case "done":
      content = <DoneStep reLogin={start === "sign-in"} onDone={close} />;
      break;
  }

  return (
    <Dialog open={open} onOpenChange={(next) => (next ? onOpenChange(true) : close())}>
      <DialogPortal>
        <DialogOverlay className="bg-black/60" />
        <DialogPrimitive.Content
          className="fixed left-[50%] top-[50%] z-50 w-full max-w-md translate-x-[-50%] translate-y-[-50%] border bg-background p-6 shadow-lg sm:rounded-lg duration-200 data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=open]:zoom-in-95"
          onPointerDownOutside={(e) => {
            if (locked) e.preventDefault();
          }}
          onEscapeKeyDown={(e) => {
            if (locked) e.preventDefault();
          }}
        >
          <DialogPrimitive.Title className="sr-only">Set up sync</DialogPrimitive.Title>
          <DialogPrimitive.Description className="sr-only">
            Connect this device to your sync server and account
          </DialogPrimitive.Description>
          {!locked && (
            <button
              type="button"
              onClick={close}
              aria-label="Close"
              className="absolute right-3 top-3 rounded-sm opacity-70 transition-opacity hover:opacity-100"
            >
              <X className="h-4 w-4" />
            </button>
          )}
          <ProgressDots current={Math.max(0, steps.indexOf(step))} total={steps.length} />
          <div
            key={step}
            className={
              direction === "forward"
                ? "onboarding-slide-forward"
                : "onboarding-slide-backward"
            }
          >
            {content}
          </div>
        </DialogPrimitive.Content>
      </DialogPortal>
    </Dialog>
  );
}

// --- Shared bits -----------------------------------------------------------------

function StepIcon({ children }: { children: React.ReactNode }) {
  return (
    <div className="mb-4 flex h-12 w-12 items-center justify-center rounded-full bg-primary/10 text-primary [&>svg]:h-6 [&>svg]:w-6">
      {children}
    </div>
  );
}

function StepHeading({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <>
      <h2 className="mb-0.5 text-lg font-semibold">{title}</h2>
      <p className="mb-6 text-sm text-muted-foreground">{children}</p>
    </>
  );
}

function BackNext({
  onBack,
  onNext,
  nextLabel = "Continue",
  nextDisabled,
  busy,
}: {
  onBack?: () => void;
  onNext: () => void;
  nextLabel?: string;
  nextDisabled?: boolean;
  busy?: boolean;
}) {
  return (
    <div className="mt-auto flex gap-2 pt-4">
      {onBack && (
        <Button variant="outline" className="flex-1" onClick={onBack} disabled={busy}>
          <ChevronLeft className="mr-1.5 h-4 w-4" />
          Back
        </Button>
      )}
      <Button className="flex-1" onClick={onNext} disabled={nextDisabled || busy}>
        {busy ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : null}
        {nextLabel}
        {!busy && <ChevronRight className="ml-1.5 h-4 w-4" />}
      </Button>
    </div>
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
  footnote?: string | null;
}) {
  return (
    <div className="rounded-lg border bg-muted/40 p-3 text-center">
      <p className="mb-1 text-[11px] text-muted-foreground">{label}</p>
      <p className="font-mono text-base tracking-wide">{code}</p>
      {footnote && <p className="mt-1.5 text-[11px] text-muted-foreground">{footnote}</p>}
    </div>
  );
}

function ErrorLine({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <Alert variant="destructive">
      <ShieldAlert className="h-4 w-4" />
      <AlertDescription>{error}</AlertDescription>
    </Alert>
  );
}

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

// --- Intro -------------------------------------------------------------------------

function IntroStep({ onNext }: { onNext: () => void }) {
  return (
    <div className="flex flex-col items-center text-center">
      <DevicesIllustration className="mb-5" />
      <h2 className="mb-0.5 text-xl font-semibold">Your notes on every device</h2>
      <p className="mb-6 text-sm text-muted-foreground">
        Record on one device, read and edit on another. Everything is encrypted
        here before it leaves, so the server you run only ever holds scrambled
        data.
      </p>
      <div className="mb-8 flex items-center gap-3 text-[11px] text-muted-foreground">
        <span className="flex items-center gap-1">
          <ShieldCheck className="h-3 w-3 text-primary" />
          End-to-end encrypted
        </span>
        <span aria-hidden>·</span>
        <span>Self-hosted</span>
        <span aria-hidden>·</span>
        <span>Open source</span>
      </div>
      <Button className="w-full" onClick={onNext}>
        Get started
        <ChevronRight className="ml-1.5 h-4 w-4" />
      </Button>
    </div>
  );
}

// --- Server ------------------------------------------------------------------------

/** One field. A probe runs when the field settles (blur, paste, Enter) and when you
 *  press Continue; a successful probe saves the address. Failures show the server's
 *  verbatim reason with a "Use anyway" escape hatch. */
function ServerStep({ onBack, onNext }: { onBack: () => void; onNext: () => void }) {
  const savedUrl = useAppStore((s) => s.syncConfig.serverUrl);
  const relayConn = useAppStore((s) => s.relayConn);
  const probeRelay = useAppStore((s) => s.probeRelay);
  const resetProbe = useAppStore((s) => s.resetProbe);
  const setSyncConfig = useAppStore((s) => s.setSyncConfig);

  const [url, setUrl] = useState(savedUrl);
  const [usedAnyway, setUsedAnyway] = useState(false);
  const trimmed = url.trim();
  const valid = isValidServerUrl(url);
  const dirty = trimmed !== savedUrl;
  const testing = relayConn.kind === "testing";
  const ok = relayConn.kind === "ok" && relayConn.normalizedUrl === savedUrl;
  const failed =
    relayConn.kind === "unreachable" ||
    relayConn.kind === "tls-error" ||
    relayConn.kind === "not-a-relay";
  // A saved, unedited address that has not been probed this visit is still good to go.
  const ready = valid && !testing && (ok || usedAnyway || (!dirty && !failed));

  const probedThisEdit = useRef(false);
  const pendingPaste = useRef(false);
  const probe = () => {
    if (!valid || testing) return;
    probedThisEdit.current = true;
    void probeRelay(trimmed);
  };
  const settle = () => {
    if (dirty && !probedThisEdit.current) probe();
  };
  useEffect(() => {
    if (pendingPaste.current) {
      pendingPaste.current = false;
      settle();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [url]);

  const handleChange = (v: string) => {
    setUrl(v);
    setUsedAnyway(false);
    probedThisEdit.current = false;
    resetProbe();
  };
  const useAnyway = () => {
    setSyncConfig({ serverUrl: trimmed });
    setUsedAnyway(true);
  };
  const next = () => {
    if (ready) onNext();
    else settle();
  };

  return (
    <div className="flex flex-col">
      <StepIcon>
        <Server />
      </StepIcon>
      <StepHeading title="Where should your devices meet?">
        Enter the address of the sync server you host. It stores only encrypted
        data and never sees your notes.
      </StepHeading>

      <div className="space-y-2">
        <Label htmlFor="wizard-server-url" className="text-xs text-muted-foreground">
          Server address
        </Label>
        <Input
          id="wizard-server-url"
          autoFocus
          value={url}
          onChange={(e) => handleChange(e.target.value)}
          onBlur={settle}
          onPaste={() => {
            pendingPaste.current = true;
          }}
          onKeyDown={(e) => e.key === "Enter" && next()}
          placeholder="https://sync.example.com"
          className="h-9"
        />
        <div className="min-h-5 text-[11px]">
          {testing && (
            <p className="flex items-center gap-1.5 text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              Checking…
            </p>
          )}
          {ok && (
            <p
              className="flex items-center gap-1.5 text-muted-foreground"
              title={`Engine v${relayConn.engineVersion} · protocol v${relayConn.protocolVersion}`}
            >
              <Check className="h-3.5 w-3.5 text-emerald-600 dark:text-emerald-400" />
              Connected · {relayConn.latencyMs} ms
              {relayConn.versionAdvisory &&
                ` · needs YapStack ${relayConn.versionAdvisory.minClientVersion} or newer`}
            </p>
          )}
          {failed && (
            <div className="space-y-1 rounded-md border border-amber-500/30 bg-amber-500/5 p-2.5">
              <p className="text-xs font-medium text-amber-700 dark:text-amber-300">
                {relayConn.kind === "tls-error"
                  ? "Certificate problem"
                  : relayConn.kind === "not-a-relay"
                    ? "That isn't a YapStack sync server"
                    : "Can't reach that server"}
              </p>
              <p role="alert" className="break-words text-muted-foreground">
                {relayConn.raw}
              </p>
              {!usedAnyway && (
                <button
                  type="button"
                  className="text-muted-foreground underline-offset-2 hover:underline"
                  onClick={useAnyway}
                >
                  Use this address anyway
                </button>
              )}
            </div>
          )}
        </div>
      </div>

      <p className="mt-2 text-[11px] text-muted-foreground">
        Don't have a server yet? Running one takes a few minutes — see{" "}
        <span className="font-medium text-foreground">docs/self-hosting.md</span>.
      </p>

      <BackNext onBack={onBack} onNext={next} nextDisabled={!valid} busy={testing} />
    </div>
  );
}

// --- Account -----------------------------------------------------------------------

type AccountMode = "sign-in" | "create";
type SignInPhase = "email" | "password" | "recovery" | "salt-mismatch" | "set-password";
type CreatePhase = "credentials" | "recovery" | "confirm";

/**
 * Sign in or create an account on the chosen server. Sign-in is the two-round login
 * (CRYPTO_SPEC §3.2): email → salt, then password; a salt mismatch for a known device
 * HARD-STOPS (§3.2 C3). "Forgot" pivots to recovery-code entry (§6). Create forces the
 * recovery code to be copied, acknowledged and re-typed before it can finish (§6.1).
 */
function AccountStep({
  reLogin,
  onBack,
  onSignedIn,
  onLockChange,
}: {
  reLogin: boolean;
  onBack?: () => void;
  onSignedIn: (status: SyncStatus) => void;
  onLockChange: (locked: boolean) => void;
}) {
  const serverUrl = useAppStore((s) => s.syncConfig.serverUrl);
  const savedEmail = useAppStore((s) => s.syncConfig.email);
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);
  const refreshSyncStatus = useAppStore((s) => s.refreshSyncStatus);

  const [mode, setMode] = useState<AccountMode>("sign-in");
  const [email, setEmail] = useState(reLogin ? (savedEmail ?? "") : "");
  const [password, setPassword] = useState("");
  const [confirmPw, setConfirmPw] = useState("");
  const [recoveryCode, setRecoveryCode] = useState("");
  // After a recovery-code sign-in: the status to hand on once the user has (or has
  // declined) setting a new password, and the new-password fields.
  const [recoveredStatus, setRecoveredStatus] = useState<SyncStatus | null>(null);
  const [newPw, setNewPw] = useState("");
  const [newPw2, setNewPw2] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [signInPhase, setSignInPhase] = useState<SignInPhase>("email");
  const [createPhase, setCreatePhase] = useState<CreatePhase>("credentials");
  const [result, setResult] = useState<SignupResult | null>(null);
  const [ack, setAck] = useState(false);
  const [copied, setCopied] = useState(false);
  const [confirmEntry, setConfirmEntry] = useState("");

  const emailValid = /.+@.+\..+/.test(email);

  const switchMode = (next: AccountMode) => {
    if (busy || result) return;
    setMode(next);
    setError(null);
    setPassword("");
    setConfirmPw("");
    setSignInPhase("email");
    setCreatePhase("credentials");
  };

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };

  // --- sign in ---
  const begin = () =>
    run(async () => {
      const res = await syncCommands.loginBegin(serverUrl, email);
      if (res.saltMismatch) {
        // A security signal, not a retryable error — do not proceed.
        setSignInPhase("salt-mismatch");
        return;
      }
      setSignInPhase("password");
    });
  const finish = () =>
    run(async () => {
      const status = await syncCommands.loginFinish(password);
      setSyncStatus(status);
      onSignedIn(status);
    });
  const recover = () =>
    run(async () => {
      const status = await syncCommands.recover(serverUrl, email, recoveryCode);
      setSyncStatus(status);
      toast.success("Recovered — you're signed in on this device.");
      // The forgotten password still works until it is changed; offer that now, while
      // the relay's 15-minute recovery window waives the current-password check.
      setRecoveredStatus(status);
      setSignInPhase("set-password");
    });
  const newPwValid = newPw.length >= 8 && newPw === newPw2;
  const setNewPassword = () =>
    run(async () => {
      await syncCommands.changePassword(null, newPw);
      toast.success("Password changed.");
      if (recoveredStatus) onSignedIn(recoveredStatus);
    });
  const keepOldPassword = () => {
    if (recoveredStatus) onSignedIn(recoveredStatus);
  };

  // --- create ---
  const passwordLongEnough = password.length >= 8;
  const passwordsMatch = password === confirmPw;
  const credentialsValid = emailValid && passwordLongEnough && passwordsMatch;
  const create = () =>
    run(async () => {
      const res = await syncCommands.signup({ serverUrl, email, password });
      setResult(res);
      setCreatePhase("recovery");
      onLockChange(true);
    });
  const copy = async () => {
    if (!result) return;
    try {
      await navigator.clipboard.writeText(result.recoveryCode);
      setCopied(true);
      toast.success("Recovery code copied.");
    } catch {
      toast.error("Could not copy — please write it down instead.");
    }
  };
  const confirmMatches =
    !!result && normalizeCode(confirmEntry) === normalizeCode(result.recoveryCode);
  const finishCreate = () =>
    run(async () => {
      await refreshSyncStatus();
      onLockChange(false);
      const status = useAppStore.getState().syncStatus;
      if (status) onSignedIn(status);
    });

  // --- render ---
  if (mode === "sign-in" && signInPhase === "set-password") {
    return (
      <div className="flex flex-col">
        <StepIcon>
          <KeyRound />
        </StepIcon>
        <StepHeading title="Set a new password">
          You're back in. The old password still works until you change it, so
          choose a new one now — or keep it if you remember it.
        </StepHeading>
        <div className="space-y-3">
          <div className="space-y-1.5">
            <Label htmlFor="wizard-set-password" className="text-xs text-muted-foreground">
              New password
            </Label>
            <Input
              id="wizard-set-password"
              autoFocus
              type="password"
              autoComplete="new-password"
              value={newPw}
              onChange={(e) => setNewPw(e.target.value)}
              className="h-9"
            />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="wizard-set-password-2" className="text-xs text-muted-foreground">
              Confirm new password
            </Label>
            <Input
              id="wizard-set-password-2"
              type="password"
              autoComplete="new-password"
              value={newPw2}
              onChange={(e) => setNewPw2(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && newPwValid && setNewPassword()}
              className="h-9"
            />
            {newPw && newPw.length < 8 && (
              <p className="text-[11px] text-muted-foreground">At least 8 characters.</p>
            )}
            {newPw2 && newPw !== newPw2 && (
              <p className="text-[11px] text-destructive">Passwords don't match.</p>
            )}
          </div>
          <ErrorLine error={error} />
        </div>
        <div className="mt-auto space-y-2 pt-4">
          <Button className="w-full" disabled={!newPwValid || busy} onClick={setNewPassword}>
            {busy && <Loader2 className="h-4 w-4 animate-spin" />}
            Change password
          </Button>
          <Button variant="ghost" className="w-full" disabled={busy} onClick={keepOldPassword}>
            Keep my current password
          </Button>
        </div>
      </div>
    );
  }

  if (mode === "sign-in" && signInPhase === "salt-mismatch") {
    return (
      <div className="flex flex-col">
        <StepIcon>
          <ShieldAlert />
        </StepIcon>
        <StepHeading title="Stopped for your safety">
          The server returned a different encryption salt than this device has
          seen before for this account. That can mean a compromised or
          misconfigured server. Don't enter your password — check the server
          address and contact whoever runs it.
        </StepHeading>
        <Button variant="outline" className="w-full" onClick={() => setSignInPhase("email")}>
          Back
        </Button>
      </div>
    );
  }

  if (mode === "create" && createPhase === "recovery" && result) {
    return (
      <div className="flex flex-col">
        <StepIcon>
          <KeyRound />
        </StepIcon>
        <StepHeading title="Save your recovery code">
          This is the only way back in if you forget your password. There is no
          reset — not even by the server.
        </StepHeading>
        <div className="space-y-3">
          <CodeBlock label="Recovery code" code={formatRecoveryCode(result.recoveryCode)} />
          <Button variant="outline" size="sm" className="w-full" onClick={copy}>
            {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
            {copied ? "Copied" : "Copy recovery code"}
          </Button>
          <label className="flex items-start gap-2 text-xs">
            <input
              type="checkbox"
              checked={ack}
              onChange={(e) => setAck(e.target.checked)}
              className="mt-0.5"
            />
            <span>
              I've saved this somewhere safe and offline. I understand it can't be
              shown again.
            </span>
          </label>
        </div>
        <div className="mt-auto pt-4">
          <Button className="w-full" disabled={!ack} onClick={() => setCreatePhase("confirm")}>
            Continue
            <ChevronRight className="ml-1.5 h-4 w-4" />
          </Button>
        </div>
      </div>
    );
  }

  if (mode === "create" && createPhase === "confirm" && result) {
    return (
      <div className="flex flex-col">
        <StepIcon>
          <KeyRound />
        </StepIcon>
        <StepHeading title="Type it back to confirm">
          Re-enter the recovery code so we know you saved it correctly.
        </StepHeading>
        <div className="space-y-2">
          <Input
            autoFocus
            placeholder="AAAA-BBBB-CCCC-DDDD-EEEE-FFFF-GGGG-HHHH"
            value={confirmEntry}
            onChange={(e) => setConfirmEntry(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && confirmMatches && finishCreate()}
            className="h-9 font-mono"
          />
          {confirmEntry && !confirmMatches && (
            <p className="text-[11px] text-destructive">
              That doesn't match. Check the code and try again.
            </p>
          )}
        </div>
        <BackNext
          onBack={() => setCreatePhase("recovery")}
          onNext={finishCreate}
          nextLabel="Finish"
          nextDisabled={!confirmMatches}
          busy={busy}
        />
      </div>
    );
  }

  return (
    <div className="flex flex-col">
      <StepIcon>
        <ShieldCheck />
      </StepIcon>
      <StepHeading title={reLogin ? "Sign in again" : "Your account"}>
        {reLogin
          ? "Your sign-in expired. Enter your password to keep syncing."
          : "Your password protects your encryption key, so it can't be reset — only recovered with a recovery code."}
      </StepHeading>

      {!reLogin && (
        <div role="tablist" aria-label="Account" className="mb-4 flex gap-1 rounded-md bg-muted p-0.5">
          {(["sign-in", "create"] as AccountMode[]).map((m) => (
            <button
              key={m}
              type="button"
              role="tab"
              aria-selected={mode === m}
              onClick={() => switchMode(m)}
              className={`flex-1 rounded px-2 py-1 text-xs transition-colors ${
                mode === m
                  ? "bg-background font-medium text-foreground shadow-sm"
                  : "text-muted-foreground hover:text-foreground"
              }`}
            >
              {m === "sign-in" ? "Sign in" : "Create account"}
            </button>
          ))}
        </div>
      )}

      {mode === "sign-in" ? (
        <div className="space-y-3">
          {signInPhase === "email" && (
            <div className="space-y-1.5">
              <Label htmlFor="wizard-email" className="text-xs text-muted-foreground">
                Email
              </Label>
              <Input
                id="wizard-email"
                autoFocus
                type="email"
                autoComplete="username"
                value={email}
                disabled={reLogin}
                onChange={(e) => setEmail(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && emailValid && begin()}
                className="h-9"
              />
            </div>
          )}
          {signInPhase === "password" && (
            <div className="space-y-1.5">
              <Label htmlFor="wizard-password" className="text-xs text-muted-foreground">
                Password for {email}
              </Label>
              <Input
                id="wizard-password"
                autoFocus
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && password && finish()}
                className="h-9"
              />
              <button
                type="button"
                className="text-[11px] text-muted-foreground underline-offset-2 hover:underline"
                onClick={() => {
                  setSignInPhase("recovery");
                  setError(null);
                }}
              >
                Forgot your password? Use your recovery code
              </button>
            </div>
          )}
          {signInPhase === "recovery" && (
            <div className="space-y-1.5">
              <Label htmlFor="wizard-recovery" className="text-xs text-muted-foreground">
                Recovery code
              </Label>
              <Input
                id="wizard-recovery"
                autoFocus
                placeholder="AAAA-BBBB-CCCC-DDDD-EEEE-FFFF-GGGG-HHHH"
                value={recoveryCode}
                onChange={(e) => setRecoveryCode(e.target.value)}
                onKeyDown={(e) =>
                  e.key === "Enter" && isValidRecoveryCode(recoveryCode) && recover()
                }
                className="h-9 font-mono"
              />
              <p className="text-[11px] text-muted-foreground">
                The code you saved when you created the account. Your password
                stays the same afterwards.
              </p>
            </div>
          )}
          <ErrorLine error={error} />
        </div>
      ) : (
        <div className="space-y-3">
          <div className="space-y-1.5">
            <Label htmlFor="wizard-new-email" className="text-xs text-muted-foreground">
              Email
            </Label>
            <Input
              id="wizard-new-email"
              autoFocus
              type="email"
              autoComplete="username"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              className="h-9"
            />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="wizard-new-password" className="text-xs text-muted-foreground">
              Password
            </Label>
            <Input
              id="wizard-new-password"
              type="password"
              autoComplete="new-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              className="h-9"
            />
            <p
              className={`text-[11px] ${
                password && !passwordLongEnough ? "text-destructive" : "text-muted-foreground"
              }`}
            >
              At least 8 characters.
            </p>
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="wizard-confirm-password" className="text-xs text-muted-foreground">
              Confirm password
            </Label>
            <Input
              id="wizard-confirm-password"
              type="password"
              autoComplete="new-password"
              value={confirmPw}
              onChange={(e) => setConfirmPw(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && credentialsValid && create()}
              className="h-9"
            />
            {confirmPw && !passwordsMatch && (
              <p className="text-[11px] text-destructive">Passwords don't match.</p>
            )}
          </div>
          <ErrorLine error={error} />
        </div>
      )}

      {mode === "sign-in" && signInPhase === "email" && (
        <BackNext onBack={onBack} onNext={begin} nextDisabled={!emailValid} busy={busy} />
      )}
      {mode === "sign-in" && signInPhase === "password" && (
        <BackNext
          onBack={() => setSignInPhase("email")}
          onNext={finish}
          nextLabel="Sign in"
          nextDisabled={!password}
          busy={busy}
        />
      )}
      {mode === "sign-in" && signInPhase === "recovery" && (
        <BackNext
          onBack={() => setSignInPhase("password")}
          onNext={recover}
          nextLabel="Recover"
          nextDisabled={!isValidRecoveryCode(recoveryCode)}
          busy={busy}
        />
      )}
      {mode === "create" && (
        <BackNext
          onBack={onBack}
          onNext={create}
          nextLabel="Create account"
          nextDisabled={!credentialsValid}
          busy={busy}
        />
      )}
    </div>
  );
}

// --- Approval ----------------------------------------------------------------------

/**
 * The joining device's side of the approval ceremony (§7.5). Shows this device's code
 * and the security code the approver's dialog shows (§7.5.4), names the device that can
 * approve, and polls until the approval lands.
 */
function ApprovalStep({
  onApproved,
  onCancel,
}: {
  onApproved: () => void;
  onCancel: () => void;
}) {
  const syncStatus = useAppStore((s) => s.syncStatus);
  const refreshSyncStatus = useAppStore((s) => s.refreshSyncStatus);

  const pending = isSelfPending(syncStatus);
  useEffect(() => {
    if (!pending) {
      onApproved();
      return;
    }
    const id = window.setInterval(() => void refreshSyncStatus(), APPROVAL_POLL_MS);
    return () => window.clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pending, refreshSyncStatus]);

  const approvers = (syncStatus?.roster ?? []).filter((d) => !d.isSelf && !d.pending);
  const named = approvers.map((d) => d.label).filter((l): l is string => !!l);

  return (
    <div className="flex flex-col">
      <StepIcon>
        <Hourglass />
      </StepIcon>
      <StepHeading title="Approve this device">
        {approvers.length === 0
          ? "No other approved device is on this account yet, so nothing can approve this one. Open YapStack on a device you've used with this account before."
          : `On ${
              named.length > 0 ? named.join(" or ") : "a device that's already signed in"
            }, open Settings → Sync and review the request. Sync starts here the moment it's approved.`}
      </StepHeading>
      <div className="space-y-3">
        <CodeBlock
          label="This device"
          code={
            syncStatus?.deviceFingerprint
              ? formatFingerprint(syncStatus.deviceFingerprint)
              : "—"
          }
          footnote={formatSecurityCode(
            syncStatus?.rosterFingerprint ?? null,
            syncStatus?.vaultKeyEpoch ?? null,
          )}
        />
        <p className="text-[11px] text-muted-foreground">
          The approving device shows the same codes. If either differs, don't approve.
        </p>
        <p className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <Loader2 className="h-3 w-3 animate-spin" />
          Waiting — this continues by itself once approved.
        </p>
      </div>
      <div className="mt-auto pt-4">
        <Button variant="outline" className="w-full" onClick={onCancel}>
          Finish later
        </Button>
      </div>
    </div>
  );
}

// --- Turn on -----------------------------------------------------------------------

function TurnOnStep({ onDone }: { onDone: () => void }) {
  const syncStatus = useAppStore((s) => s.syncStatus);
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const hasPeers = (syncStatus?.roster ?? []).some((d) => !d.isSelf && !d.pending);

  const enable = async () => {
    setBusy(true);
    setError(null);
    try {
      const status = await syncCommands.enable();
      setSyncStatus(status);
      onDone();
    } catch (e) {
      // Surface verbatim — do not fall back.
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col">
      <StepIcon>
        <ShieldCheck />
      </StepIcon>
      <StepHeading title="Turn on sync">
        {hasPeers
          ? "Notes and recordings from your other devices download here, and anything already on this device is merged in."
          : "This library is prepared for sync and uploaded to your server, encrypted on this device."}{" "}
        A full backup of your library is kept first.
      </StepHeading>
      <ErrorLine error={error} />
      <div className="mt-auto pt-4">
        <Button className="w-full" onClick={enable} disabled={busy}>
          {busy ? (
            <>
              <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
              Preparing your library…
            </>
          ) : (
            "Turn on sync"
          )}
        </Button>
      </div>
    </div>
  );
}

// --- Done --------------------------------------------------------------------------

function DoneStep({ reLogin, onDone }: { reLogin: boolean; onDone: () => void }) {
  const syncStatus = useAppStore((s) => s.syncStatus);
  const peers = (syncStatus?.roster ?? []).filter((d) => !d.isSelf && !d.pending).length;
  return (
    <div className="flex flex-col items-center text-center">
      <div className="mb-4 flex h-12 w-12 items-center justify-center rounded-full bg-primary/10">
        <Check className="h-6 w-6 text-primary" />
      </div>
      <h2 className="mb-0.5 text-lg font-semibold">
        {reLogin ? "You're signed in" : "Sync is on"}
      </h2>
      <p className="mb-6 text-sm text-muted-foreground">
        {reLogin
          ? "Syncing picks up where it left off."
          : peers > 0
            ? `This device now keeps in step with your ${peers === 1 ? "other device" : `${peers} other devices`}.`
            : "To add another device, sign in with the same account there — you'll approve it from here."}
      </p>
      <Button className="w-full" onClick={onDone}>
        Done
      </Button>
    </div>
  );
}
