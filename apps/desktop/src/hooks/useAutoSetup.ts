import { useEffect, useRef } from "react";
import { useAppStore } from "@/stores/appStore";
import { trackAppLaunched } from "@/lib/analytics";

/** Runs one-time engine initialization (capture start, model load, transcription init). Mounted in AppLayout. */
export function useAutoSetup() {
  const autoSetup = useAppStore((s) => s.autoSetup);
  const ran = useRef(false);

  useEffect(() => {
    if (ran.current) return;
    ran.current = true;
    autoSetup().then(() => {
      const { settings } = useAppStore.getState();
      trackAppLaunched({
        capture_source: settings.captureSource,
        dictation_enabled: settings.dictation.enabled ? 1 : 0,
        dictation_slot_count: settings.dictation.slots.length,
        theme: settings.theme,
        ai_connection_count: settings.aiConfig.connections.length,
      });
    });
  }, [autoSetup]);
}
