import type { DbSession, DbSegment, DbFolder, DbChatMessage } from "@/lib/db";

function uid(): string {
  return crypto.randomUUID();
}

/** Create a mock DbSession with sensible defaults. Override any field via `overrides`. */
export function makeSession(overrides: Partial<DbSession> = {}): DbSession {
  const id = uid();
  return {
    id: `session-${id}`,
    title: "Test Session",
    created_at: "2024-06-15T10:00:00",
    updated_at: "2024-06-15T10:00:00",
    source: "MicOnly",
    status: "completed",
    duration_seconds: 120,
    total_segments: 3,
    folder_id: null,
    is_pinned: 0,
    pinned_at: null,
    session_type: "transcription",
    sort_order: 0,
    recording_device_id: null,
    ...overrides,
  };
}

/** Create a mock DbSegment with sensible defaults. */
export function makeSegment(overrides: Partial<DbSegment> = {}): DbSegment {
  const id = uid();
  return {
    id: `segment-${id}`,
    session_id: "session-1",
    source: "Mic",
    text: "Segment text",
    audio_offset_seconds: 0,
    chunk_duration_seconds: 5,
    confidence: 0.9,
    created_at: "2024-06-15T10:00:00",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
    speaker_id: null,
    ...overrides,
  };
}

/** Create a mock DbFolder with sensible defaults. */
export function makeFolder(overrides: Partial<DbFolder> = {}): DbFolder {
  const id = uid();
  return {
    id: `folder-${id}`,
    name: "Test Folder",
    parent_id: null,
    sort_order: 0,
    icon: null,
    color: null,
    description: null,
    created_at: "2024-06-15T10:00:00",
    updated_at: "2024-06-15T10:00:00",
    ...overrides,
  };
}

/** Create a mock DbChatMessage with sensible defaults. */
export function makeChatMessage(
  overrides: Partial<DbChatMessage> = {},
): DbChatMessage {
  const id = uid();
  return {
    id: `msg-${id}`,
    context_key: "session-1",
    session_id: "session-1",
    role: "user",
    content: "Test message",
    action: null,
    created_at: "2024-06-15T10:00:00",
    tool_calls: null,
    send_id: `send-${id}`,
    sequence: 0,
    tool_call_id: null,
    observation: null,
    status: null,
    ...overrides,
  };
}
