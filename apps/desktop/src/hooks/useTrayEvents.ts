import { useEffect } from "react";
import { useAppStore } from "@/stores/appStore";
import { EVENTS, listenEvent } from "@/lib/events";

/** Listens for system tray menu events (new session, stop session). Mounted in MainApp. */
export function useTrayEvents() {
  useEffect(() => {
    const unlistenSession = listenEvent(EVENTS.TRAY_NEW_SESSION, (payload) => {
      const { enginePhase, captureStatus, activeSessionId, createAndStartSession } =
        useAppStore.getState();
      if (enginePhase !== "ready") return;
      if (captureStatus?.state !== "Capturing") return;
      if (activeSessionId) return;
      // Fire-and-forget: `createAndStartSession` never rejects and reports its own
      // failures with a toast (a console.error alone was invisible to the user).
      void createAndStartSession(payload || undefined, "tray");
    });

    const unlistenAll = listenEvent(EVENTS.TRAY_NEW_SESSION_ALL, () => {
      const {
        enginePhase,
        captureStatus,
        activeSessionId,
        bufferInfo,
        createAndStartSession,
      } = useAppStore.getState();
      if (enginePhase !== "ready") return;
      if (captureStatus?.state !== "Capturing") return;
      if (activeSessionId) return;
      const micAvail = bufferInfo?.mic?.available_seconds ?? 0;
      const sysAvail = bufferInfo?.system?.available_seconds ?? 0;
      const maxAvail = Math.max(micAvail, sysAvail);
      if (maxAvail <= 0) return;
      void createAndStartSession(Math.ceil(maxAvail), "tray");
    });

    const unlistenStop = listenEvent(EVENTS.TRAY_STOP_SESSION, () => {
      const { stopActiveSession } = useAppStore.getState();
      stopActiveSession();
    });

    return () => {
      unlistenSession.then((fn) => fn());
      unlistenAll.then((fn) => fn());
      unlistenStop.then((fn) => fn());
    };
  }, []);
}
