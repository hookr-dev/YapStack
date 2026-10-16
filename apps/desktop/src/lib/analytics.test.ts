import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

const mockTrackEvent = vi.fn().mockResolvedValue(undefined);
vi.mock("@aptabase/tauri", () => ({
  trackEvent: (...args: unknown[]) => mockTrackEvent(...args),
}));

import {
  setAnalyticsEnabled,
  isAnalyticsEnabled,
  trackAppLaunched,
  trackSessionCreated,
  trackSessionStopped,
  trackSessionDeleted,
  trackManualNoteCreated,
  trackDictationStarted,
  trackDictationCompleted,
  trackDictationFailed,
  trackDictationSlotCreated,
  trackDictationSlotConfigured,
  trackChatMessageSent,
  trackChatToolExecuted,
  trackChatToolUndone,
  trackChatCleared,
  trackSearchUsed,
  trackFolderCreated,
  trackSessionPinned,
  trackShortcutUsed,
  trackModelDownloaded,
  trackEngineError,
  trackSettingChanged,
  trackSegmentEdited,
  trackSessionMovedToFolder,
} from "./analytics";

beforeEach(() => {
  mockTrackEvent.mockClear();
});

describe("analytics", () => {
  it("trackAppLaunched sends correct event name and props", () => {
    trackAppLaunched({
      capture_source: "Mixed",
      model_size: "Small",
      dictation_enabled: 1,
      dictation_slot_count: 2,
      theme: "dark",
      ai_connection_count: 1,
    });
    expect(mockTrackEvent).toHaveBeenCalledWith("app_launched", {
      capture_source: "Mixed",
      model_size: "Small",
      dictation_enabled: 1,
      dictation_slot_count: 2,
      theme: "dark",
      ai_connection_count: 1,
    });
  });

  it("trackSessionCreated sends props", () => {
    trackSessionCreated({ source: "MicOnly", backfill_seconds: 30, trigger: "button" });
    expect(mockTrackEvent).toHaveBeenCalledWith("session_created", {
      source: "MicOnly",
      backfill_seconds: 30,
      trigger: "button",
    });
  });

  it("trackSessionStopped sends duration and segment count", () => {
    trackSessionStopped({ duration_seconds: 120, segment_count: 10 });
    expect(mockTrackEvent).toHaveBeenCalledWith("session_stopped", {
      duration_seconds: 120,
      segment_count: 10,
    });
  });

  it("trackSessionDeleted sends event with no props", () => {
    trackSessionDeleted();
    expect(mockTrackEvent).toHaveBeenCalledWith("session_deleted", undefined);
  });

  it("trackManualNoteCreated sends event", () => {
    trackManualNoteCreated();
    expect(mockTrackEvent).toHaveBeenCalledWith("manual_note_created", undefined);
  });

  it("trackDictationStarted sends slot props", () => {
    trackDictationStarted({
      slot_id: "slot-1",
      slot_name: "Slot 1",
      ai_enabled: 0,
      has_prompt: 0,
      output_action: "paste",
    });
    expect(mockTrackEvent).toHaveBeenCalledWith("dictation_started", expect.objectContaining({
      slot_id: "slot-1",
      output_action: "paste",
    }));
  });

  it("trackDictationCompleted sends duration and length", () => {
    trackDictationCompleted({
      slot_id: "slot-1",
      duration_ms: 5000,
      transcription_length: 100,
      ai_processed: 0,
      output_action: "clipboard",
    });
    expect(mockTrackEvent).toHaveBeenCalledWith("dictation_completed", expect.objectContaining({
      duration_ms: 5000,
    }));
  });

  it("trackDictationFailed truncates error to 100 chars", () => {
    const longError = "x".repeat(200);
    trackDictationFailed({ slot_id: "slot-1", error_reason: longError });
    const calledProps = mockTrackEvent.mock.calls[0][1] as { error_reason: string };
    expect(calledProps.error_reason.length).toBe(100);
  });

  it("trackDictationSlotCreated sends event", () => {
    trackDictationSlotCreated();
    expect(mockTrackEvent).toHaveBeenCalledWith("dictation_slot_created", undefined);
  });

  it("trackDictationSlotConfigured sends changed fields", () => {
    trackDictationSlotConfigured({ changed_fields: "name,prompt" });
    expect(mockTrackEvent).toHaveBeenCalledWith("dictation_slot_configured", {
      changed_fields: "name,prompt",
    });
  });

  it("trackChatMessageSent sends context and action", () => {
    trackChatMessageSent({ context: "session", has_action: 1, action_id: "summarize" });
    expect(mockTrackEvent).toHaveBeenCalledWith("chat_message_sent", {
      context: "session",
      has_action: 1,
      action_id: "summarize",
    });
  });

  it("trackChatToolExecuted sends tool name", () => {
    trackChatToolExecuted({ tool_name: "update_title" });
    expect(mockTrackEvent).toHaveBeenCalledWith("chat_tool_executed", { tool_name: "update_title" });
  });

  it("trackChatToolUndone sends tool name", () => {
    trackChatToolUndone({ tool_name: "pin_session" });
    expect(mockTrackEvent).toHaveBeenCalledWith("chat_tool_undone", { tool_name: "pin_session" });
  });

  it("trackChatCleared sends event", () => {
    trackChatCleared();
    expect(mockTrackEvent).toHaveBeenCalledWith("chat_cleared", undefined);
  });

  it("trackSearchUsed sends event", () => {
    trackSearchUsed();
    expect(mockTrackEvent).toHaveBeenCalledWith("search_used", undefined);
  });

  it("trackFolderCreated sends event", () => {
    trackFolderCreated();
    expect(mockTrackEvent).toHaveBeenCalledWith("folder_created", undefined);
  });

  it("trackSessionPinned sends event", () => {
    trackSessionPinned();
    expect(mockTrackEvent).toHaveBeenCalledWith("session_pinned", undefined);
  });

  it("trackShortcutUsed sends shortcut id", () => {
    trackShortcutUsed({ shortcut_id: "toggle-recording" });
    expect(mockTrackEvent).toHaveBeenCalledWith("shortcut_used", { shortcut_id: "toggle-recording" });
  });

  it("trackModelDownloaded sends model size", () => {
    trackModelDownloaded({ model_size: "Small" });
    expect(mockTrackEvent).toHaveBeenCalledWith("model_downloaded", { model_size: "Small" });
  });

  it("trackEngineError truncates error to 100 chars", () => {
    const longError = "e".repeat(200);
    trackEngineError({ error: longError, phase: "initializing" });
    const calledProps = mockTrackEvent.mock.calls[0][1] as { error: string };
    expect(calledProps.error.length).toBe(100);
  });

  it("trackSettingChanged sends setting name and value", () => {
    trackSettingChanged({ setting_name: "theme", new_value: "dark" });
    expect(mockTrackEvent).toHaveBeenCalledWith("setting_changed", {
      setting_name: "theme",
      new_value: "dark",
    });
  });

  it("trackSegmentEdited sends event", () => {
    trackSegmentEdited();
    expect(mockTrackEvent).toHaveBeenCalledWith("segment_edited", undefined);
  });

  it("trackSessionMovedToFolder sends event", () => {
    trackSessionMovedToFolder();
    expect(mockTrackEvent).toHaveBeenCalledWith("session_moved_to_folder", undefined);
  });

  it("silently catches errors from trackEvent", () => {
    mockTrackEvent.mockRejectedValueOnce(new Error("network error"));
    // Should not throw
    expect(() => trackSessionDeleted()).not.toThrow();
  });
});

describe("analytics opt-out gate", () => {
  // Every test above ran with the gate untouched, so this also pins that the
  // module ships OPEN — the toggle changes nothing until a user flips it.
  it("is open by default so existing behavior is unchanged", () => {
    expect(isAnalyticsEnabled()).toBe(true);
  });

  // Restore the default for any test that runs after this block.
  afterEach(() => setAnalyticsEnabled(true));

  it("suppresses every event helper while closed — one gate, not per-callsite", () => {
    setAnalyticsEnabled(false);
    expect(isAnalyticsEnabled()).toBe(false);

    // A spread across the event families: lifecycle, sessions, dictation, chat,
    // navigation, engine errors, settings. All funnel through the same `track`.
    trackAppLaunched({
      capture_source: "Mixed",
      model_size: "Small",
      dictation_enabled: 1,
      dictation_slot_count: 2,
      theme: "dark",
      ai_connection_count: 1,
    });
    trackSessionCreated({ source: "MicOnly", backfill_seconds: 30, trigger: "button" });
    trackSessionDeleted();
    trackDictationStarted({
      slot_id: "slot-1",
      slot_name: "Slot 1",
      ai_enabled: 0,
      has_prompt: 0,
      output_action: "paste",
    });
    trackChatMessageSent({ context: "session", has_action: 0, action_id: "" });
    trackSearchUsed();
    trackEngineError({ error: "boom", phase: "initializing" });
    trackSettingChanged({ setting_name: "theme", new_value: "dark" });

    expect(mockTrackEvent).not.toHaveBeenCalled();
  });

  it("emits again once reopened", () => {
    setAnalyticsEnabled(false);
    trackSessionDeleted();
    expect(mockTrackEvent).not.toHaveBeenCalled();

    setAnalyticsEnabled(true);
    trackSessionDeleted();
    expect(mockTrackEvent).toHaveBeenCalledTimes(1);
    expect(mockTrackEvent).toHaveBeenCalledWith("session_deleted", undefined);
  });
});
