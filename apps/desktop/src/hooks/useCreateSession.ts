import { useAppStore } from "@/stores/appStore";

/** Provides `canCreate` / `creatingSession` flags and `handleNew` for starting recording sessions. */
export function useCreateSession() {
  const enginePhase = useAppStore((s) => s.enginePhase);
  const captureStatus = useAppStore((s) => s.captureStatus);
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const creatingSession = useAppStore((s) => s.creatingSession);
  const createAndStartSession = useAppStore((s) => s.createAndStartSession);

  const isReady =
    enginePhase === "ready" && captureStatus?.state === "Capturing";
  // `creatingSession` folds into `canCreate` so every affordance already wired to
  // it (the Plus buttons, BackfillDropdown) refuses a double-fire for free; the
  // store single-flights too, so this is the visual half of one guard.
  const canCreate = isReady && !activeSessionId && !creatingSession;

  // No try/catch: `createAndStartSession` reports its own failures, so the sidebar,
  // the global shortcut and the tray all show the same toast.
  const handleNew = (backfillSeconds?: number) => {
    void createAndStartSession(backfillSeconds, "sidebar");
  };

  return { canCreate, creatingSession, handleNew };
}
