import { useEffect, useRef } from "react";
import { toast } from "sonner";
import { useAppStore } from "@/stores/appStore";
import { commands } from "@/lib/tauri";
import { EVENTS, listenEvent } from "@/lib/events";
import { trackStreamHealthEvent } from "@/lib/analytics";

/** Listens for live transcription segment, backfill, and status events. Mounted in AppLayout. */

/// Restarts landing within this window render as one combined toast — an
/// OS default-device change commonly restarts mic and system audio
/// back-to-back.
const RESTART_COALESCE_MS = 1500;
export function useLiveTranscriptionEvents() {
  const setLivePhase = useAppStore((s) => s.setLivePhase);
  // source -> latest restart within the coalescing window (see
  // RESTART_COALESCE_MS). Ref, not state: toast rendering is imperative.
  const recentRestartsRef = useRef(new Map<string, { text: string; at: number }>());
  const onLiveSegment = useAppStore((s) => s.onLiveSegment);
  const onBackfillComplete = useAppStore((s) => s.onBackfillComplete);
  const onSessionPartReady = useAppStore((s) => s.onSessionPartReady);
  const recoverActiveSession = useAppStore((s) => s.recoverActiveSession);

  useEffect(() => {
    const unlisten = Promise.all([
      listenEvent(EVENTS.LIVE_TRANSCRIPTION_SEGMENT, (payload) => {
        // Filter on source kind — dictation segments are routed by
        // useDictation.ts, not the session pipeline.
        if (payload.source_kind === "dictation") return;
        onLiveSegment(payload);
      }),
      listenEvent(EVENTS.LIVE_TRANSCRIPTION_STATUS, (payload) => {
        // Two-prong filter: source_kind AND matching active session_id.
        // Without source_kind, a concurrent dictation's `Stopped` would
        // flip the session UI's phase machine and finalize the wrong
        // recording. Without session_id, a stale event from a previous
        // session (after recovery/restart) could clobber the freshly-
        // active session.
        if (payload.source_kind === "dictation") return;
        const activeId = useAppStore.getState().activeSessionId;
        if (
          payload.session_id != null &&
          activeId != null &&
          payload.session_id !== activeId
        ) {
          return;
        }
        setLivePhase(payload.phase);

        // Show toast for Error phase
        if (payload.phase === "Error" && payload.error_message) {
          toast.error(payload.error_message);
        }
      }),
      listenEvent(EVENTS.TRANSCRIPTION_ENGINE_LOADED, (payload) => {
        // Source of truth for "what's actually running" — emitted right
        // after the sidecar's first model_loaded probe and on every
        // subsequent engine/variant switch. StatusPopover renders this.
        useAppStore.setState({
          loadedEngineInfo: {
            engine: payload.engine,
            accel: payload.accel,
            modelDir: payload.model_dir,
          },
        });
      }),
      listenEvent(EVENTS.TRANSCRIPTION_ENGINE_DROPPED, () => {
        // The scheduler timed out on shutdown and dropped the transcription
        // client rather than handing it back to shared state (the aborted
        // worker may still hold an in-flight clone). Without this signal
        // `enginePhase` would stay `ready` while shared state is empty, and
        // the next session start would pass the FE readiness check only to
        // fail with `NotInitialized` in the backend. Reset and re-run
        // autoSetup so the next user action finds a healthy engine.
        useAppStore.setState({
          enginePhase: "idle",
          engineError: null,
          loadedEngineInfo: null,
        });
        toast.warning("Transcription engine restarting…", {
          id: "engine-restart",
        });
        useAppStore.getState().autoSetup();
      }),
      listenEvent(EVENTS.LIVE_TRANSCRIPTION_WARNING, (payload) => {
        toast.warning(payload.message, { id: "transcription-warning" });
      }),
      listenEvent(EVENTS.BACKFILL_COMPLETE, () => {
        onBackfillComplete();
      }),
      listenEvent(EVENTS.SESSION_PART_READY, (payload) => {
        onSessionPartReady(payload);
      }),
      listenEvent(EVENTS.SESSION_WAV_ERROR, (payload) => {
        toast.error(payload.message);
      }),
      listenEvent(EVENTS.SESSION_WAV_WARNING, (payload) => {
        toast.warning(payload.message, { id: `wav-warning-${payload.session_id}` });
      }),
      listenEvent(EVENTS.STREAM_HEALTH, (payload) => {
        trackStreamHealthEvent({
          source: payload.source,
          status: payload.status,
        });
        if (payload.status === "restarted") {
          // Auto-failover toast names the new device when known. A single
          // OS default-device change often restarts BOTH sources within a
          // beat; render every restart in the window through ONE updating
          // toast instead of stacking a mic toast on a system toast.
          const now = Date.now();
          const recent = recentRestartsRef.current;
          recent.set(payload.source, {
            text: payload.bound_device_name
              ? `${payload.source === "Mic" ? "mic" : "system audio"} → ${payload.bound_device_name}`
              : payload.message,
            at: now,
          });
          for (const [source, entry] of recent) {
            if (now - entry.at > RESTART_COALESCE_MS) recent.delete(source);
          }
          const parts = [...recent.values()].map((e) => e.text);
          const text =
            parts.length > 1 ? `Switched ${parts.join(" · ")}` : `Switched ${parts[0]}`;
          toast.success(text, { id: "stream-health-restarted", duration: 3000 });
        } else {
          toast.error(payload.message, {
            id: `stream-health-${payload.source}`,
            duration: payload.status === "restart_abandoned" ? Infinity : 5000,
          });
        }
      }),
    ]);

    // Eager fetch — recover state on mount/reload (session slot only;
    // dictation lifecycle is owned by useDictation).
    commands.getLiveTranscriptionStatus("session").then((r) => {
      if (r.status === "ok") {
        setLivePhase(r.data.phase);
        // If backend is still running with a session, recover frontend state
        if (r.data.phase === "Running" && r.data.session_id) {
          recoverActiveSession(r.data.session_id, r.data.effective_start_epoch_ms ?? undefined);
        }
      }
    });

    return () => {
      unlisten.then((fns) => fns.forEach((fn) => fn()));
    };
  }, [setLivePhase, onLiveSegment, onBackfillComplete, onSessionPartReady, recoverActiveSession]);
}
