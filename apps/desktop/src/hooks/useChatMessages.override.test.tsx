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
import type { AIConfig } from "@/lib/ai";

const {
  getChatMessagesMock,
  insertChatMessageMock,
  getChatContextProfileIdMock,
  clearChatContextProfileMock,
  resolveAndCreateClientMock,
  streamChatWithToolsMock,
  executeToolMock,
} = vi.hoisted(() => ({
  getChatMessagesMock: vi.fn(),
  insertChatMessageMock: vi.fn(),
  getChatContextProfileIdMock: vi.fn(),
  clearChatContextProfileMock: vi.fn(),
  resolveAndCreateClientMock: vi.fn(),
  streamChatWithToolsMock: vi.fn(),
  executeToolMock: vi.fn(),
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
    clearChatContextProfile: clearChatContextProfileMock,
    // Folder tree is assembled for session-context actions; keep it out of
    // the DB mock surface.
  };
});
vi.mock("@/lib/ai-context", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/ai-context")>("@/lib/ai-context");
  return { ...actual, assembleFolderTreeForActions: vi.fn().mockResolvedValue("") };
});
vi.mock("@/lib/ai", async () => {
  const actual = await vi.importActual<typeof import("@/lib/ai")>("@/lib/ai");
  return {
    ...actual,
    resolveAndCreateClient: resolveAndCreateClientMock,
    streamChatWithTools: streamChatWithToolsMock,
  };
});

vi.mock("@/lib/ai-tools", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/ai-tools")>("@/lib/ai-tools");
  return { ...actual, executeTool: executeToolMock };
});

import { useAppStore } from "@/stores/appStore";
import { getAction } from "@/lib/ai-actions";
import { useChatMessages } from "./useChatMessages";

const CONFIG: AIConfig = {
  connections: [
    { id: "c-cloud", name: "OpenAI", kind: "openai", baseUrl: "https://api.openai.com/v1", apiKey: "sk-x" },
    { id: "c-local", name: "Ollama", kind: "custom", baseUrl: "http://127.0.0.1:11434/v1", apiKey: "" },
  ],
  profiles: [
    { id: "p-chat-default", name: "Chat default", connectionId: "c-cloud", model: "gpt-5-mini" },
    { id: "p-actions-default", name: "Actions default", connectionId: "c-cloud", model: "gpt-5" },
    { id: "p-picked", name: "Local llama", connectionId: "c-local", model: "llama3.1:8b" },
  ],
  assignments: { chatProfileId: "p-chat-default", aiActionsProfileId: "p-actions-default" },
};

function makeCtx(): AIContextValue {
  return {
    contextKey: "s1",
    sessionId: "s1",
    isSessionContext: true,
    sources: [],
    tools: { availableToolIds: [] },
    buildSystemPrompt: vi.fn().mockResolvedValue("system"),
  } as unknown as AIContextValue;
}

beforeEach(() => {
  vi.clearAllMocks();
  getChatMessagesMock.mockResolvedValue([]);
  insertChatMessageMock.mockResolvedValue(undefined);
  // The user picked "Local llama" in the chat-bar pill for this conversation.
  getChatContextProfileIdMock.mockResolvedValue("p-picked");
  clearChatContextProfileMock.mockResolvedValue(undefined);
  resolveAndCreateClientMock.mockReturnValue({ client: {}, model: "m" });
  streamChatWithToolsMock.mockImplementation(async function* () {
    yield { type: "token", content: "ok" };
    yield { type: "done" };
  });
  useAppStore.setState((s) => ({
    syncAppliedSeq: 0,
    settings: { ...s.settings, aiConfig: CONFIG },
  }));
});

async function send(actionId?: string) {
  const { result } = renderHook(() =>
    useChatMessages(makeCtx(), actionId ? "" : "hello", vi.fn(), [], vi.fn()),
  );
  await waitFor(() => expect(getChatMessagesMock).toHaveBeenCalled());
  await act(async () => {
    await result.current.handleSend(actionId ? getAction(actionId) : undefined);
  });
  expect(resolveAndCreateClientMock).toHaveBeenCalledTimes(1);
  return resolveAndCreateClientMock.mock.calls[0][1] as string | null;
}

describe("chat-bar Profile override wins over every per-usage default", () => {
  it("plain chat send uses the picked Profile", async () => {
    expect(await send()).toBe("p-picked");
  });

  it.each(["summarize", "key-points", "action-items", "meeting-minutes"])(
    "action '%s' sent from the chat bar uses the picked Profile, not the AI-actions default",
    async (actionId) => {
      expect(getAction(actionId)).toBeDefined();
      expect(await send(actionId)).toBe("p-picked");
    },
  );
});

describe("chat-bar sends without a usable override fall back per usage", () => {
  it("no override: chat uses the Chat default", async () => {
    getChatContextProfileIdMock.mockResolvedValue(null);
    expect(await send()).toBe("p-chat-default");
    expect(clearChatContextProfileMock).not.toHaveBeenCalled();
  });

  it("no override: an action uses the AI-actions default", async () => {
    getChatContextProfileIdMock.mockResolvedValue(null);
    expect(await send("summarize")).toBe("p-actions-default");
    expect(clearChatContextProfileMock).not.toHaveBeenCalled();
  });

  it("dangling override: chat falls back to the Chat default and clears the row", async () => {
    getChatContextProfileIdMock.mockResolvedValue("p-deleted");
    expect(await send()).toBe("p-chat-default");
    expect(clearChatContextProfileMock).toHaveBeenCalledWith("s1");
  });

  it("dangling override: an action falls back to the AI-actions default and clears the row", async () => {
    getChatContextProfileIdMock.mockResolvedValue("p-deleted");
    expect(await send("summarize")).toBe("p-actions-default");
    expect(clearChatContextProfileMock).toHaveBeenCalledWith("s1");
  });
});

describe("Summarize's title, tag and folder tool rounds", () => {
  it("every round runs on the picked Profile's client and model", async () => {
    const pickedClient = { tag: "client-for-p-picked" };
    resolveAndCreateClientMock.mockImplementation(
      (cfg: AIConfig, id: string | null) => ({
        client: id === "p-picked" ? pickedClient : { tag: `client-for-${id}` },
        model: cfg.profiles.find((p) => p.id === id)?.model ?? "?",
      }),
    );
    executeToolMock.mockResolvedValue({ name: "x", detail: "done" });
    const rounds = [
      [
        { id: "t1", name: "update_title", arguments: { title: "Standup" } },
        { id: "t2", name: "tag_session", arguments: { tags: ["team"] } },
      ],
      [{ id: "t3", name: "add_session_to_folder", arguments: { folder_id: "f1" } }],
    ];
    let call = 0;
    streamChatWithToolsMock.mockImplementation(async function* () {
      const calls = rounds[call++];
      if (calls) {
        yield { type: "tool_calls", calls };
      } else {
        yield { type: "token", content: "summary" };
      }
      yield { type: "done" };
    });

    const ctx = makeCtx();
    (ctx as unknown as { tools: unknown }).tools = {
      availableToolIds: ["update_title", "tag_session", "add_session_to_folder"],
      getToolContext: vi.fn().mockResolvedValue({}),
    };
    const { result } = renderHook(() =>
      useChatMessages(ctx, "", vi.fn(), [], vi.fn()),
    );
    await waitFor(() => expect(getChatMessagesMock).toHaveBeenCalled());
    await act(async () => {
      await result.current.handleSend(getAction("summarize"));
    });

    expect(resolveAndCreateClientMock).toHaveBeenCalledTimes(1);
    expect(resolveAndCreateClientMock.mock.calls[0][1]).toBe("p-picked");
    expect(executeToolMock.mock.calls.map((c) => c[0])).toEqual([
      "update_title",
      "tag_session",
      "add_session_to_folder",
    ]);
    // Two tool rounds plus the final synthesis round.
    expect(streamChatWithToolsMock).toHaveBeenCalledTimes(3);
    for (const [client, model] of streamChatWithToolsMock.mock.calls) {
      expect(client).toBe(pickedClient);
      expect(model).toBe("llama3.1:8b");
    }
  });
});
