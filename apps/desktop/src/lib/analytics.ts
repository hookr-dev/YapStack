import { trackEvent } from "@aptabase/tauri";

type Props = Record<string, string | number>;

/**
 * The single emit gate. Defaults to ON so behavior is unchanged until something
 * says otherwise — it matches `defaultSettings.analyticsEnabled`, and the store
 * pushes the persisted value in during rehydration (before any event can fire).
 *
 * This is a mechanism, not a policy: this module never reads the settings store
 * (that would be an import cycle — `appStore` already imports from here). The
 * caller decides when to flip it. See feedback_naming_mechanism_vs_policy.
 */
let emitEnabled = true;

/** Open or close the emit gate. Called by `appStore` on rehydrate + on toggle. */
export function setAnalyticsEnabled(enabled: boolean): void {
  emitEnabled = enabled;
}

/** Current gate state — for tests and for UI that wants to reflect reality. */
export function isAnalyticsEnabled(): boolean {
  return emitEnabled;
}

/**
 * Every `track*` helper in this file funnels through here, and this file is the
 * ONLY importer of `@aptabase/tauri` in the app, so this single check is a
 * complete gate on outbound analytics. The Rust side generates no events of its
 * own: `tauri-plugin-aptabase` registers only the `track_event` command, and its
 * dispatcher's flush is a no-op while the queue is empty — so with the gate shut,
 * nothing is enqueued and nothing is sent.
 */
function track(name: string, props?: Props): void {
  if (!emitEnabled) return;
  trackEvent(name, props).catch(() => {});
}

// App lifecycle
export function trackAppLaunched(props: {
  capture_source: string;
  model_size: string;
  dictation_enabled: number;
  dictation_slot_count: number;
  theme: string;
  /** Number of AI Connections the user has configured (0 = no AI set up). */
  ai_connection_count: number;
}): void {
  track("app_launched", props);
}

// Session lifecycle
export function trackSessionCreated(props: {
  source: string;
  backfill_seconds: number;
  trigger: string;
}): void {
  track("session_created", props);
}

export function trackSessionStopped(props: {
  duration_seconds: number;
  segment_count: number;
}): void {
  track("session_stopped", props);
}

export function trackSessionDeleted(): void {
  track("session_deleted");
}

export function trackSessionsCleared(): void {
  track("sessions_cleared");
}

export function trackManualNoteCreated(): void {
  track("manual_note_created");
}

// Transcript copy / export
export function trackTranscriptExported(props: {
  /** "selection" (bulk bar) or "session" (header ⋮ menu). */
  scope: string;
  /** "clipboard" (copy) or "file" (save to disk). */
  mechanism: string;
  /** "full" | "mine" | "theirs". */
  kind: string;
  /** Source segment count in scope. */
  segments: number;
}): void {
  track("transcript_exported", props);
}

// Dictation
export function trackDictationStarted(props: {
  slot_id: string;
  slot_name: string;
  ai_enabled: number;
  has_prompt: number;
  output_action: string;
}): void {
  track("dictation_started", props);
}

export function trackDictationCompleted(props: {
  slot_id: string;
  duration_ms: number;
  transcription_length: number;
  ai_processed: number;
  output_action: string;
}): void {
  track("dictation_completed", props);
}

export function trackDictationFailed(props: {
  slot_id: string;
  error_reason: string;
}): void {
  track("dictation_failed", { ...props, error_reason: props.error_reason.slice(0, 100) });
}

export function trackDictationCancelled(props: {
  slot_id: string;
  phase: string;
  duration_ms: number;
}): void {
  track("dictation_cancelled", props);
}

export function trackDictationSlotCreated(): void {
  track("dictation_slot_created");
}

export function trackDictationSlotDeleted(): void {
  track("dictation_slot_deleted");
}

export function trackDictationSlotConfigured(props: {
  changed_fields: string;
}): void {
  track("dictation_slot_configured", props);
}

// AI Chat
export function trackChatMessageSent(props: {
  context: string;
  has_action: number;
  action_id: string;
}): void {
  track("chat_message_sent", props);
}

export function trackChatToolExecuted(props: { tool_name: string }): void {
  track("chat_tool_executed", props);
}

export function trackChatToolUndone(props: { tool_name: string }): void {
  track("chat_tool_undone", props);
}

export function trackChatCleared(): void {
  track("chat_cleared");
}

// Navigation & Discovery
export function trackSearchUsed(): void {
  track("search_used");
}

export function trackFolderCreated(): void {
  track("folder_created");
}

export function trackSessionPinned(): void {
  track("session_pinned");
}

export function trackSessionUnpinned(): void {
  track("session_unpinned");
}

// Keyboard shortcuts
export function trackShortcutUsed(props: { shortcut_id: string }): void {
  track("shortcut_used", props);
}

// Model & Engine
export function trackModelDownloaded(props: { model_size: string }): void {
  track("model_downloaded", props);
}

export function trackModelDeleted(props: { model_size: string }): void {
  track("model_deleted", props);
}

export function trackModelSwitched(props: {
  model_size: string;
  from_size: string;
}): void {
  track("model_switched", props);
}

export function trackEngineError(props: {
  error: string;
  phase: string;
}): void {
  // Coerce defensively: a non-string sneaking in here must not throw —
  // this runs inside engine-error handling, and an exception here masks
  // the real engine error before it reaches state or logs.
  track("engine_error", {
    ...props,
    error: String(props.error ?? "unknown").slice(0, 100),
  });
}

// Settings
export function trackSettingChanged(props: {
  setting_name: string;
  new_value: string;
}): void {
  track("setting_changed", props);
}

// Audio playback
export function trackAudioPlaybackStarted(props: {
  duration_seconds: number;
}): void {
  track("audio_playback_started", props);
}

// Segment interaction
export function trackSegmentEdited(): void {
  track("segment_edited");
}

export function trackSegmentHidden(): void {
  track("segment_hidden");
}

// Drag & Drop
export function trackSessionMovedToFolder(): void {
  track("session_moved_to_folder");
}

// Stream health
export function trackStreamHealthEvent(props: {
  source: string;
  status: string;
}): void {
  track("stream_health_event", props);
}

// Auto-update
export function trackUpdateAvailable(props: { version: string }): void {
  track("update_available", props);
}

export function trackUpdateInstallStarted(props: { version: string }): void {
  track("update_install_started", props);
}

export function trackUpdateInstallFailed(props: {
  version: string;
  error: string;
}): void {
  track("update_install_failed", { ...props, error: props.error.slice(0, 100) });
}

// Dictation history
export function trackDictationHistoryCleared(): void {
  track("dictation_history_cleared");
}

export function trackDictationHistoryEntryDeleted(): void {
  track("dictation_history_entry_deleted");
}

export function trackDictationMovedToNote(): void {
  track("dictation_moved_to_note");
}
