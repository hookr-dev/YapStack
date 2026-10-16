import { Laptop, Monitor, Smartphone, Lock } from "lucide-react";
import { cn } from "@/lib/utils";

/**
 * The sync feature's picture: three devices in tinted circles, overlapping, with a small
 * lock — "every device, encrypted". Shared by the not-set-up hero on Settings → Sync,
 * the wizard's intro step, and the first-run onboarding step, so the feature looks the
 * same everywhere it is introduced.
 */
export function DevicesIllustration({ className }: { className?: string }) {
  const circle = "h-12 w-12 [&>svg]:h-5 [&>svg]:w-5";
  const overlap = "-ml-3";
  return (
    <div
      aria-hidden
      className={cn("relative inline-flex items-center", className)}
    >
      <span
        className={cn(
          "flex items-center justify-center rounded-full bg-primary/10 text-primary ring-2 ring-background",
          circle,
        )}
      >
        <Laptop />
      </span>
      <span
        className={cn(
          "flex items-center justify-center rounded-full bg-primary/15 text-primary ring-2 ring-background",
          circle,
          overlap,
        )}
      >
        <Monitor />
      </span>
      <span
        className={cn(
          "flex items-center justify-center rounded-full bg-primary/10 text-primary ring-2 ring-background",
          circle,
          overlap,
        )}
      >
        <Smartphone />
      </span>
      <span
        className={cn(
          "absolute -bottom-1 -right-1 flex h-5 w-5 items-center justify-center rounded-full bg-foreground text-background ring-2 ring-background [&>svg]:h-3 [&>svg]:w-3",
        )}
      >
        <Lock />
      </span>
    </div>
  );
}
