import { renderHook, waitFor, act } from "@testing-library/react";
import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import type { AIContextValue } from "@/lib/ai-context";
import type { DbChatMessage } from "@/lib/db";

const {
  getChatMessagesMock,
  insertChatMessageMock,
  getChatContextProfileIdMock,
  resolveAndCreateClientMock,
  streamChatWithToolsMock,
} = vi.hoisted(() => ({
  getChatMessagesMock: vi.fn(),
  insertChatMessageMock: vi.fn(),
  getChatContextProfileIdMock: vi.fn(),
  resolveAndCreateClientMock: vi.fn(),
  streamChatWithToolsMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("@aptabase/tauri", () => ({
  trackEvent: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));
vi.mock("@/lib/db", async () => {
  const actual = await vi.importActual<typeof import("@/lib/db")>("@/lib/db");
  return {
    ...actual,
    getChatMessages: getChatMessagesMock,
    insertChatMessage: insertChatMessageMock,
    getChatContextProfileId: getChatContextProfileIdMock,
  };
});
vi.mock("@/lib/ai", async () => {
  const actual = await vi.importActual<typeof import("@/lib/ai")>("@/lib/ai");
  return {
    ...actual,
    resolveAndCreateClient: resolveAndCreateClientMock,
    streamChatWithTools: streamChatWithToolsMock,
  };
});

import { useAppStore } from "@/stores/appStore";
import { useChatMessages } from "./useChatMessages";

function row(id: string, content: string, role: "user" | "assistant"): DbChatMessage {
  return {
    id,
    context_key: "session:s1",
    session_id: "s1",
    role,
    content,
    action: null,
    created_at: "2026-01-01T00:00:00Z",
    tool_calls: null,
    send_id: id,
    sequence: 0,
    tool_call_id: null,
    observation: null,
    status: null,
  } as DbChatMessage;
}

function makeCtx(): AIContextValue {
  return {
    contextKey: "session:s1",
    sessionId: "s1",
    isSessionContext: true,
    sources: [],
    tools: { availableToolIds: [] },
    buildSystemPrompt: vi.fn().mockResolvedValue("system"),
  } as unknown as AIContextValue;
}

function renderChat() {
  return renderHook(() =>
    useChatMessages(makeCtx(), "hello", vi.fn(), [], vi.fn()),
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  getChatMessagesMock.mockResolvedValue([]);
  insertChatMessageMock.mockResolvedValue(undefined);
  getChatContextProfileIdMock.mockResolvedValue(null);
  resolveAndCreateClientMock.mockReturnValue({ client: {}, model: "m" });
  useAppStore.setState({ syncAppliedSeq: 0 });
});

// B2: chat_messages is synced, but the hook only read it on context change —
// a conversation continued on another device never appeared.
describe("useChatMessages — refresh on applied sync batches", () => {
  it("re-reads the conversation when the store commits a sync batch", async () => {
    const { result } = renderChat();
    await waitFor(() => expect(getChatMessagesMock).toHaveBeenCalledTimes(1));

    getChatMessagesMock.mockResolvedValue([
      row("u1", "from my phone", "user"),
      row("a1", "answered there", "assistant"),
    ]);
    act(() => {
      useAppStore.setState({ syncAppliedSeq: 1 });
    });

    await waitFor(() => expect(result.current.messages).toHaveLength(2));
    expect(result.current.messages[0].content).toBe("from my phone");
  });

  it("does not reload while a send is in flight, and drains once it settles", async () => {
    let releaseStream: (() => void) | null = null;
    const streamGate = new Promise<void>((resolve) => {
      releaseStream = resolve;
    });
    streamChatWithToolsMock.mockImplementation(async function* () {
      await streamGate;
      yield { type: "token", content: "hi" };
    });

    const { result } = renderChat();
    await waitFor(() => expect(getChatMessagesMock).toHaveBeenCalledTimes(1));

    // Start a send; it parks inside the stream.
    let sendDone: Promise<void>;
    act(() => {
      sendDone = result.current.handleSend();
    });
    await waitFor(() => expect(streamChatWithToolsMock).toHaveBeenCalled());
    const callsBefore = getChatMessagesMock.mock.calls.length;

    // A batch lands mid-send: it must NOT overwrite the streaming bubble.
    act(() => {
      useAppStore.setState({ syncAppliedSeq: 1 });
    });
    await Promise.resolve();
    expect(getChatMessagesMock.mock.calls.length).toBe(callsBefore);

    // When the send finishes, the suppressed reload runs.
    await act(async () => {
      releaseStream!();
      await sendDone!;
    });
    await waitFor(() =>
      expect(getChatMessagesMock.mock.calls.length).toBeGreaterThan(
        callsBefore,
      ),
    );
  });
});
