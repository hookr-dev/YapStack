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
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Loader2, ShieldCheck, ShieldAlert } from "lucide-react";
import { toast } from "sonner";
import { useAppStore } from "@/stores/appStore";
import { syncCommands, formatFingerprint, type DeviceRosterEntry } from "@/lib/sync";

/**
 * Device-authorization ceremony (CRYPTO_SPEC §7.5). An existing (already
 * trusted) device approves a pending new device: it bumps the roster counter,
 * re-asserts the live vault_key_epoch, re-signs, and uploads — re-anchoring the
 * new device to the current epoch. The human MUST compare the new device's
 * fingerprint out-of-band before approving (that check closes the first-boot
 * TOFU rollback). The dialog surfaces the fingerprint prominently and requires
 * an explicit out-of-band-verified acknowledgement.
 */
export function DeviceApprovalDialog({
  device,
}: {
  device: DeviceRosterEntry;
}) {
  const setSyncStatus = useAppStore((s) => s.setSyncStatus);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [verified, setVerified] = useState(false);

  const handleApprove = async () => {
    setBusy(true);
    setError(null);
    try {
      const status = await syncCommands.approveDevice(device.fingerprint);
      setSyncStatus(status);
      toast.success("Device approved.");
      setOpen(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) {
          setVerified(false);
          setError(null);
        }
      }}
    >
      <DialogTrigger asChild>
        <Button size="sm" variant="outline" className="justify-start font-mono">
          {formatFingerprint(device.fingerprint)}
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <ShieldCheck className="h-4 w-4" />
            Approve new device
          </DialogTitle>
          <DialogDescription>
            Only approve if you are signing in on a new device right now. The
            fingerprint below must exactly match the one shown on that device.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3 py-1">
          <div className="rounded-lg border bg-muted/50 p-3 text-center">
            <p className="text-[11px] text-muted-foreground mb-1">
              New device fingerprint
            </p>
            <p className="font-mono text-base tracking-wide">
              {formatFingerprint(device.fingerprint)}
            </p>
          </div>
          <Alert>
            <ShieldAlert className="h-4 w-4" />
            <AlertTitle>Verify out-of-band</AlertTitle>
            <AlertDescription>
              Compare this fingerprint to the one shown on the new device. If
              they differ, do not approve — the relay may be serving a stale or
              hostile roster.
            </AlertDescription>
          </Alert>
          <label className="flex items-start gap-2 text-xs">
            <input
              type="checkbox"
              checked={verified}
              onChange={(e) => setVerified(e.target.checked)}
              className="mt-0.5"
            />
            <span>
              I compared the fingerprint on both devices and they match.
            </span>
          </label>
          {error && (
            <Alert variant="destructive">
              <ShieldAlert className="h-4 w-4" />
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          )}
        </div>
        <DialogFooter>
          <Button disabled={!verified || busy} onClick={handleApprove}>
            {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            Approve device
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
