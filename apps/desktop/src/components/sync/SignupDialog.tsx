import { useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Loader2, Copy, KeyRound, ShieldAlert, Check } from "lucide-react";
import { toast } from "sonner";
import { useAppStore } from "@/stores/appStore";
import {
  syncCommands,
  formatRecoveryCode,
  normalizeCode,
  type SignupResult,
} from "@/lib/sync";

type Step = "credentials" | "recovery" | "confirm";

/**
 * Create-account flow. After the account is created the user is FORCED to
 * record the CSPRNG recovery code before they can finish: they must copy it,
 * check the acknowledgement, and re-enter it to confirm. There is no
 * server-side reset in true E2E — lost password + lost recovery code =
 * unrecoverable data (CRYPTO_SPEC §6.1, arch §11.1). This forced capture is the
 * #1 data-loss mitigation, so the dialog cannot be dismissed by the backdrop
 * once a code exists.
 */
export function SignupDialog({ serverUrl }: { serverUrl: string }) {
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);
  const refreshSyncStatus = useAppStore((s) => s.refreshSyncStatus);

  const [open, setOpen] = useState(false);
  const [step, setStep] = useState<Step>("credentials");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [confirmPw, setConfirmPw] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [result, setResult] = useState<SignupResult | null>(null);
  const [ack, setAck] = useState(false);
  const [copied, setCopied] = useState(false);
  const [confirmEntry, setConfirmEntry] = useState("");

  const reset = () => {
    setStep("credentials");
    setEmail("");
    setPassword("");
    setConfirmPw("");
    setBusy(false);
    setError(null);
    setResult(null);
    setAck(false);
    setCopied(false);
    setConfirmEntry("");
  };

  const handleOpenChange = (next: boolean) => {
    // Once a recovery code exists, block backdrop/Esc dismissal so the user
    // can't skip recording it. They must finish via the confirm step.
    if (!next && result) {
      toast.error("Save your recovery code before closing.");
      return;
    }
    setOpen(next);
    if (!next) reset();
  };

  const credentialsValid =
    /.+@.+\..+/.test(email) && password.length >= 8 && password === confirmPw;

  const handleCreate = async () => {
    setBusy(true);
    setError(null);
    try {
      const res = await syncCommands.signup({ serverUrl, email, password });
      setResult(res);
      setStep("recovery");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleCopy = async () => {
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
    !!result &&
    normalizeCode(confirmEntry) === normalizeCode(result.recoveryCode);

  const handleFinish = async () => {
    // Adopt the freshly-created session, then close.
    await refreshSyncStatus();
    setSyncStatus(useAppStore.getState().syncStatus);
    toast.success("Account created. You're signed in.");
    setOpen(false);
    reset();
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button size="sm">Create account</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        {step === "credentials" && (
          <>
            <DialogHeader>
              <DialogTitle>Create a sync account</DialogTitle>
              <DialogDescription>
                Your password derives the encryption key for your data. Choose a
                strong one — it is never sent to the server in a recoverable
                form.
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3 py-1">
              <div className="space-y-1.5">
                <Label htmlFor="signup-email" className="text-xs">
                  Email
                </Label>
                <Input
                  id="signup-email"
                  type="email"
                  autoComplete="username"
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="signup-pw" className="text-xs">
                  Password
                </Label>
                <Input
                  id="signup-pw"
                  type="password"
                  autoComplete="new-password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="signup-pw2" className="text-xs">
                  Confirm password
                </Label>
                <Input
                  id="signup-pw2"
                  type="password"
                  autoComplete="new-password"
                  value={confirmPw}
                  onChange={(e) => setConfirmPw(e.target.value)}
                />
              </div>
              {error && (
                <Alert variant="destructive">
                  <ShieldAlert className="h-4 w-4" />
                  <AlertDescription>{error}</AlertDescription>
                </Alert>
              )}
            </div>
            <DialogFooter>
              <Button
                onClick={handleCreate}
                disabled={!credentialsValid || busy}
              >
                {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                Create account
              </Button>
            </DialogFooter>
          </>
        )}

        {step === "recovery" && result && (
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2">
                <KeyRound className="h-4 w-4" />
                Save your recovery code
              </DialogTitle>
              <DialogDescription>
                This is the ONLY way to recover your data if you forget your
                password. We cannot reset it for you.
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3 py-1">
              <div className="rounded-lg border bg-muted/50 p-3 text-center font-mono text-sm tracking-wide select-all">
                {formatRecoveryCode(result.recoveryCode)}
              </div>
              <Button
                variant="outline"
                size="sm"
                className="w-full"
                onClick={handleCopy}
              >
                {copied ? (
                  <Check className="h-3.5 w-3.5" />
                ) : (
                  <Copy className="h-3.5 w-3.5" />
                )}
                {copied ? "Copied" : "Copy recovery code"}
              </Button>
              <Alert variant="destructive">
                <ShieldAlert className="h-4 w-4" />
                <AlertTitle>Lost password + lost code = lost data</AlertTitle>
                <AlertDescription>
                  Store this somewhere safe and offline. There is no server-side
                  reset.
                </AlertDescription>
              </Alert>
              <label className="flex items-start gap-2 text-xs">
                <input
                  type="checkbox"
                  checked={ack}
                  onChange={(e) => setAck(e.target.checked)}
                  className="mt-0.5"
                />
                <span>
                  I have saved my recovery code somewhere safe. I understand it
                  cannot be recovered later.
                </span>
              </label>
            </div>
            <DialogFooter>
              <Button disabled={!ack} onClick={() => setStep("confirm")}>
                Continue
              </Button>
            </DialogFooter>
          </>
        )}

        {step === "confirm" && result && (
          <>
            <DialogHeader>
              <DialogTitle>Confirm your recovery code</DialogTitle>
              <DialogDescription>
                Re-enter the recovery code to confirm you saved it correctly.
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3 py-1">
              <Input
                autoFocus
                placeholder="AAAA-BBBB-CCCC-DDDD-EEEE-FFFF-GGGG-HHHH"
                value={confirmEntry}
                onChange={(e) => setConfirmEntry(e.target.value)}
                className="font-mono text-sm"
              />
              {confirmEntry && !confirmMatches && (
                <p className="text-[11px] text-destructive">
                  That does not match. Check the code and try again.
                </p>
              )}
            </div>
            <DialogFooter className="flex-col-reverse gap-2 sm:flex-row">
              <Button
                variant="ghost"
                onClick={() => setStep("recovery")}
              >
                Back
              </Button>
              <Button disabled={!confirmMatches} onClick={handleFinish}>
                Finish
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
