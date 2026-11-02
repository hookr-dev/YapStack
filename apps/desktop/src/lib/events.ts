import { listen, emit } from "@tauri-apps/api/event";
import type {
  AudioDeviceInfoDto,
  CaptureStatusDto,
  BufferStatusDto,
  EngineKindDto,
  LiveTranscriptionStatus,
} from "@/lib/tauri";
import type { LiveSegmentEvent } from "@/lib/tauri";

// --- Payload types for events not covered by specta-generated DTOs ---

export type ModelDownloadProgress = {
  percent: number;
  size: string;
};

export type SessionPartReadyEvent = {
  session_id: string;
  part_index: number;
  file_path: string;
  format: "wav" | "mp3";
  duration_seconds: number;
  sample_rate: number;
};

export type SessionWavErrorEvent = {
  session_id: string;
  message: string;
};

export type SessionWavWarningEvent = {
  session_id: string;
  message: string;
};

export type LiveTranscriptionWarningEvent = {
  message: string;
};

export type StreamHealthEvent = {
  source: "Mic" | "System";
  status: "restarted" | "restart_failed" | "restart_abandoned";
  message: string;
  /**
   * Human-readable name of the device the Stream is bound to after the
   * event. Set on successful auto-failover so the UI can render
   * "Switched to {name}" toasts. `null` for failures or when the underlying
   * capture didn't report a device name.
   */
  bound_device_name?: string | null;
};

/**
 * Emitted by the device broker whenever the device list changes or any
 * system default flips. Payload is the freshly enumerated device list
 * — input devices first, then output, with `is_default` flags
 * recomputed against the current OS state. Subscribers should replace
 * their cached device list and reconcile the user's persisted
 * selection if the chosen device is no longer present.
 */
export type DevicesChangedEvent = AudioDeviceInfoDto[];

/**
 * Emitted right after a transcription client successfully initializes,
 * carrying the resolved acceleration provider and model directory the
 * sidecar actually loaded. Lets the FE display "Parakeet · WebGPU"
 * keyed to ground truth instead of FE-side state. Defined manually
 * because specta only generates types reachable from a registered
 * command's args/return — event-only payloads have to be declared
 * here.
 */
export type TranscriptionEngineLoadedEvent = {
  engine: EngineKindDto;
  accel: string | null;
  model_dir: string | null;
  /** Why the attempted accelerator failed to load, when `accel` is a CPU
   *  fallback. Optional: older hosts don't send it. */
  accel_fallback?: string | null;
  /** `accel` is a GPU EP that leaves part of the model's heavy ops on the
   *  CPU. Optional: older hosts don't send it. */
  accel_partial?: boolean;
};

/**
 * Per-chunk transcription pressure telemetry. Fires after every transcribe
 * attempt (success or failure). RTFx < 1 means the pipeline is slower than
 * real time; sustained low RTFx + rising lag is the failure mode that Stage 1
 * exists to surface.
 */
export type LiveTranscriptionPressureEvent = {
  source: "Mic" | "System";
  chunk_index: number;
  chunk_audio_seconds: number;
  wall_ms: number;
  /** Null when the transcribe call failed or wall_ms was 0. */
  rtfx: number | null;
  engine: "Whisper" | "Parakeet";
  /** Priority tier the scheduler ran this chunk at. Distinguishes live
   *  throughput from backfill drain rate in pressure telemetry. */
  origin: "live" | "backfill" | "final_flush";
  /** Null when the chunk did not produce a successful transcription. */
  lag_seconds: number | null;
  /** Source-local preserved audio backlog after this live chunk dispatch. */
  drain_backlog_seconds: number;
  /** Resolved accelerator (e.g. "webgpu", "coreml", "cpu", "metal", "cuda")
   *  captured at session start. Null if the sidecar didn't report one. */
  accel: string | null;
  /** Variant directory name (e.g. "parakeet-tdt-v3-int8"). Null for Whisper
   *  or when the sidecar didn't report a model_dir. */
  variant: string | null;
};

export type BubbleState =
  | "recording" | "transcribing" | "processing"
  | "pasted" | "copied" | "note-created"
  | "no-speech" | "no-input" | "error" | "cancelled";

export type DictationStateEvent = {
  state: BubbleState;
  slotName?: string;
};

// --- Centralized window label constants ---

export const WINDOWS = {
  MAIN: "main",
  DICTATION: "dictation",
  RECORDING_INDICATOR: "recording-indicator",
  INSIGHT: "insight",
} as const;

// --- Centralized event name constants ---

export const EVENTS = {
  // Capture
  CAPTURE_STATUS: "capture-status",
  BUFFER_INFO: "buffer-info",

  // Live transcription
  LIVE_TRANSCRIPTION_SEGMENT: "live-transcription-segment",
  LIVE_TRANSCRIPTION_STATUS: "live-transcription-status",
  LIVE_TRANSCRIPTION_WARNING: "live-transcription-warning",
  LIVE_TRANSCRIPTION_PRESSURE: "live-transcription-pressure",
  TRANSCRIPTION_ENGINE_LOADED: "transcription-engine-loaded",
  TRANSCRIPTION_ENGINE_DROPPED: "transcription-engine-dropped",
  BACKFILL_COMPLETE: "backfill-complete",
  SESSION_PART_READY: "session-part-ready",
  SESSION_WAV_ERROR: "session-wav-error",
  SESSION_WAV_WARNING: "session-wav-warning",
  STREAM_HEALTH: "stream-health",
  DEVICES_CHANGED: "devices-changed",

  // Model
  MODEL_DOWNLOAD_PROGRESS: "model-download-progress",

  // Tray
  TRAY_NEW_SESSION: "tray:new-session",
  TRAY_NEW_SESSION_ALL: "tray:new-session-all",
  TRAY_STOP_SESSION: "tray:stop-session",

  // Dictation
  DICTATION_STATE: "dictation:state",

  // Recording indicator
  RECORDING_INDICATOR_ACTIVE: "recording-indicator:active",
  RECORDING_INDICATOR_OPEN_MAIN: "recording-indicator:open-main",

  // Insight overlay
  INSIGHT_STATE: "insight:state",
  INSIGHT_HIDE_REQUEST: "insight:hide-request",
  INSIGHT_CHANGE_ACTIVE: "insight:change-active",
  /** Main → overlay: whether the overlay window is currently shown. The
   *  overlay gates its cursor-position poll on this so the hidden window
   *  doesn't run Tauri IPC ~17×/sec while insights are off or no session is
   *  live (its webview is created at startup and never unmounts). */
  INSIGHT_VISIBILITY: "insight:visibility",
} as const;

/** Payload for {@link EVENTS.INSIGHT_STATE}. Emitted by the main window's
 *  overlay controller whenever the Active Insight's runtime state changes.
 *  The overlay window listens and renders. */
export interface InsightStateEvent {
  insightName: string;
  status: "idle" | "running" | "error";
  content: string | null;
  generatedAt: string | null;
  error: string | null;
  /** Current Insight id (or `null` when no insight is running). Reflects
   *  the runtime `currentInsightId` from the main window's store; used by
   *  the overlay's in-header switcher to render the checked state. */
  currentInsightId: string | null;
  /** All currently-enabled Insight slots — populated so the overlay's
   *  in-header switcher has a list to render without needing direct
   *  Zustand access. */
  slots: { id: string; name: string }[];
}

/** Payload for {@link EVENTS.INSIGHT_CHANGE_ACTIVE}. Emitted by the overlay
 *  when the user picks a different Insight (or "None") from the in-header
 *  switcher; the main-window controller listens and writes the change to
 *  Zustand. `null` disables the active assignment entirely. */
export interface InsightChangeActiveEvent {
  insightId: string | null;
}

// --- Type-safe event payload map ---

type EventPayloadMap = {
  [EVENTS.CAPTURE_STATUS]: CaptureStatusDto;
  [EVENTS.BUFFER_INFO]: BufferStatusDto;
  [EVENTS.LIVE_TRANSCRIPTION_SEGMENT]: LiveSegmentEvent;
  [EVENTS.LIVE_TRANSCRIPTION_STATUS]: LiveTranscriptionStatus;
  [EVENTS.LIVE_TRANSCRIPTION_WARNING]: LiveTranscriptionWarningEvent;
  [EVENTS.LIVE_TRANSCRIPTION_PRESSURE]: LiveTranscriptionPressureEvent;
  [EVENTS.TRANSCRIPTION_ENGINE_LOADED]: TranscriptionEngineLoadedEvent;
  [EVENTS.TRANSCRIPTION_ENGINE_DROPPED]: void;
  [EVENTS.BACKFILL_COMPLETE]: void;
  [EVENTS.SESSION_PART_READY]: SessionPartReadyEvent;
  [EVENTS.SESSION_WAV_ERROR]: SessionWavErrorEvent;
  [EVENTS.SESSION_WAV_WARNING]: SessionWavWarningEvent;
  [EVENTS.STREAM_HEALTH]: StreamHealthEvent;
  [EVENTS.DEVICES_CHANGED]: DevicesChangedEvent;
  [EVENTS.MODEL_DOWNLOAD_PROGRESS]: ModelDownloadProgress;
  [EVENTS.TRAY_NEW_SESSION]: number;
  [EVENTS.TRAY_NEW_SESSION_ALL]: void;
  [EVENTS.TRAY_STOP_SESSION]: void;
  [EVENTS.DICTATION_STATE]: DictationStateEvent;
  [EVENTS.RECORDING_INDICATOR_ACTIVE]: boolean;
  [EVENTS.RECORDING_INDICATOR_OPEN_MAIN]: void;
  [EVENTS.INSIGHT_STATE]: InsightStateEvent;
  [EVENTS.INSIGHT_HIDE_REQUEST]: void;
  [EVENTS.INSIGHT_CHANGE_ACTIVE]: InsightChangeActiveEvent;
  [EVENTS.INSIGHT_VISIBILITY]: boolean;
};

type EventName = keyof EventPayloadMap;

/**
 * Type-safe wrapper around Tauri's `listen`. Enforces the correct payload type
 * for each event name.
 */
export function listenEvent<E extends EventName>(
  event: E,
  handler: (payload: EventPayloadMap[E]) => void,
) {
  return listen<EventPayloadMap[E]>(event, (e) => handler(e.payload));
}

/**
 * Type-safe wrapper around Tauri's `emit`. Enforces the correct payload type
 * for each event name.
 */
export function emitEvent<E extends EventName>(
  event: E,
  ...args: EventPayloadMap[E] extends void ? [] : [EventPayloadMap[E]]
) {
  return emit(event, args[0]);
}
