import { useEffect, useState } from "react";
import { useAppStore } from "@/stores/appStore";

/// How long a CoreML load runs before the UI says it's preparing the model.
/// A load from the OS compile cache takes well under a second.
export const PREPARING_MODEL_AFTER_MS = 3_000;

/// Shown while the model is being prepared, in the status popover and
/// Settings.
export const PREPARING_MODEL_DETAIL =
  "Setting up for the Neural Engine. This happens once.";

/// True once a CoreML model load has run past `PREPARING_MODEL_AFTER_MS`,
/// counted from when the load started, so a view opened mid-load agrees with
/// the others. Its first load compiles the model for the Neural Engine,
/// which takes tens of seconds once and then loads from the OS cache
/// (ADR 0008).
export function usePreparingModel(): boolean {
  const coreMlLoadStartedAt = useAppStore((s) =>
    s.enginePhase === "initializing" &&
    s.initializingParakeet?.variant === "TdtV3CoreMl"
      ? s.initializingParakeet.startedAt
      : null,
  );
  const [waitedOutLoad, setWaitedOutLoad] = useState<number | null>(null);

  useEffect(() => {
    if (coreMlLoadStartedAt === null) return;
    const remaining = coreMlLoadStartedAt + PREPARING_MODEL_AFTER_MS - Date.now();
    const timer = setTimeout(
      () => setWaitedOutLoad(coreMlLoadStartedAt),
      Math.max(0, remaining),
    );
    return () => clearTimeout(timer);
  }, [coreMlLoadStartedAt]);

  return (
    coreMlLoadStartedAt !== null &&
    (waitedOutLoad === coreMlLoadStartedAt ||
      Date.now() - coreMlLoadStartedAt >= PREPARING_MODEL_AFTER_MS)
  );
}
