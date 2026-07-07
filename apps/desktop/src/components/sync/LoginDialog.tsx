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
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Loader2, ShieldAlert, KeyRound } from "lucide-react";
import { toast } from "sonner";
import { useAppStore } from "@/stores/appStore";
import { syncCommands } from "@/lib/sync";

type Step = "email" | "password" | "recovery";

/**
 * Two-round login (CRYPTO_SPEC §3.2): round 1 sends the email and fetches the
 * Argon2 salt; round 2 derives auth_key and authenticates. If round 1 reports a
 * salt mismatch for a known device (hostile-relay salt substitution, §3.2 C3)
 * the flow HARD-STOPS and refuses to proceed. A "lost password" affordance
 * pivots to recovery-code entry (§6).
 */
export function LoginDialog({ serverUrl }: { serverUrl: string }) {
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);

  const [open, setOpen] = useState(false);
  const [step, setStep] = useState<Step>("email");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [recoveryCode, setRecoveryCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saltMismatch, setSaltMismatch] = useState(false);

  const reset = () => {
    setStep("email");
    setEmail("");
    setPassword("");
    setRecoveryCode("");
    setBusy(false);
    setError(null);
    setSaltMismatch(false);
  };

  const handleBegin = async () => {
    setBusy(true);
    setError(null);
    try {
      const res = await syncCommands.loginBegin(serverUrl, email);
      if (res.saltMismatch) {
        // Do NOT proceed — this is a security signal, not a retryable error.
        setSaltMismatch(true);
        return;
      }
      setStep("password");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleFinish = async () => {
    setBusy(true);
    setError(null);
    try {
      const status = await syncCommands.loginFinish(password);
      setSyncStatus(status);
      toast.success("Signed in.");
      setOpen(false);
      reset();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleRecover = async () => {
    setBusy(true);
    setError(null);
    try {
      const status = await syncCommands.recover(serverUrl, email, recoveryCode);
      setSyncStatus(status);
      toast.success("Recovered. Set a new password in settings.");
      setOpen(false);
      reset();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleOpenChange = (next: boolean) => {
    setOpen(next);
    if (!next) reset();
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button size="sm" variant="outline">
          Sign in
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        {saltMismatch ? (
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2 text-destructive">
                <ShieldAlert className="h-4 w-4" />
                Security warning
              </DialogTitle>
              <DialogDescription>
                The server returned a different encryption salt than this device
                has seen before for this account. This can indicate a
                compromised or misconfigured relay. Sign-in has been stopped for
                your safety — do not enter your password. Verify your server URL
                and contact your relay operator.
              </DialogDescription>
            </DialogHeader>
            <DialogFooter>
              <Button variant="outline" onClick={() => handleOpenChange(false)}>
                Close
              </Button>
            </DialogFooter>
          </>
        ) : step === "email" ? (
          <>
            <DialogHeader>
              <DialogTitle>Sign in to sync</DialogTitle>
              <DialogDescription>
                Enter the email for your YapStack Sync account.
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3 py-1">
              <div className="space-y-1.5">
                <Label htmlFor="login-email" className="text-xs">
                  Email
                </Label>
                <Input
                  id="login-email"
                  type="email"
                  autoComplete="username"
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && email && handleBegin()}
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
                onClick={handleBegin}
                disabled={!/.+@.+\..+/.test(email) || busy}
              >
                {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                Continue
              </Button>
            </DialogFooter>
          </>
        ) : step === "password" ? (
          <>
            <DialogHeader>
              <DialogTitle>Enter your password</DialogTitle>
              <DialogDescription>{email}</DialogDescription>
            </DialogHeader>
            <div className="space-y-3 py-1">
              <Input
                autoFocus
                type="password"
                autoComplete="current-password"
                placeholder="Password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && password && handleFinish()}
              />
              {error && (
                <Alert variant="destructive">
                  <ShieldAlert className="h-4 w-4" />
                  <AlertDescription>{error}</AlertDescription>
                </Alert>
              )}
              <button
                type="button"
                className="text-[11px] text-muted-foreground underline-offset-2 hover:underline"
                onClick={() => {
                  setStep("recovery");
                  setError(null);
                }}
              >
                Forgot your password? Use your recovery code
              </button>
            </div>
            <DialogFooter className="flex-col-reverse gap-2 sm:flex-row">
              <Button variant="ghost" onClick={() => setStep("email")}>
                Back
              </Button>
              <Button onClick={handleFinish} disabled={!password || busy}>
                {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                Sign in
              </Button>
            </DialogFooter>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2">
                <KeyRound className="h-4 w-4" />
                Recover with your recovery code
              </DialogTitle>
              <DialogDescription>
                Enter the recovery code you saved when you created your account.
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3 py-1">
              <Input
                autoFocus
                placeholder="AAAA-BBBB-CCCC-DDDD-EEEE-FFFF-GGGG-HHHH"
                value={recoveryCode}
                onChange={(e) => setRecoveryCode(e.target.value)}
                className="font-mono text-sm"
              />
              {error && (
                <Alert variant="destructive">
                  <ShieldAlert className="h-4 w-4" />
                  <AlertDescription>{error}</AlertDescription>
                </Alert>
              )}
            </div>
            <DialogFooter className="flex-col-reverse gap-2 sm:flex-row">
              <Button variant="ghost" onClick={() => setStep("password")}>
                Back
              </Button>
              <Button
                onClick={handleRecover}
                disabled={recoveryCode.trim().length < 32 || busy}
              >
                {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                Recover
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
