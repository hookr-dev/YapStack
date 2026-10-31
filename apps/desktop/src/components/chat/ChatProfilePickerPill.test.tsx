import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
import type { AIConfig } from "@/lib/ai";

const {
  getChatContextProfileIdMock,
  setChatContextProfileIdMock,
  clearChatContextProfileMock,
} = vi.hoisted(() => ({
  getChatContextProfileIdMock: vi.fn(),
  setChatContextProfileIdMock: vi.fn(),
  clearChatContextProfileMock: vi.fn(),
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
    getChatContextProfileId: getChatContextProfileIdMock,
    setChatContextProfileId: setChatContextProfileIdMock,
    clearChatContextProfile: clearChatContextProfileMock,
  };
});

import { toast } from "sonner";
import { useAppStore } from "@/stores/appStore";
import { ChatProfilePickerPill } from "./ChatProfilePickerPill";

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

beforeEach(() => {
  vi.clearAllMocks();
  getChatContextProfileIdMock.mockResolvedValue(null);
  setChatContextProfileIdMock.mockResolvedValue(undefined);
  clearChatContextProfileMock.mockResolvedValue(undefined);
  useAppStore.setState((s) => ({
    settings: { ...s.settings, aiConfig: CONFIG },
  }));
});

function trigger() {
  return screen.getAllByRole("button")[0];
}

describe("ChatProfilePickerPill", () => {
  it("shows Default, checked, when no override row exists", async () => {
    render(<ChatProfilePickerPill contextKey="s1" />);
    await waitFor(() => expect(getChatContextProfileIdMock).toHaveBeenCalledWith("s1"));
    expect(trigger()).toHaveTextContent("Default");
    expect(trigger()).not.toHaveTextContent("Chat default");

    await userEvent.click(trigger());
    const defaultItem = await screen.findByRole("button", { name: /Chat: Chat default · Actions: Actions default/ });
    // The checked item carries the Check icon (an svg) next to its label.
    expect(defaultItem.querySelector("svg")).not.toBeNull();
    const pickedItem = screen.getByRole("button", { name: /Local llama/ });
    expect(pickedItem.querySelector("svg")).toBeNull();
  });

  it("shows the picked Profile once the write succeeds", async () => {
    render(<ChatProfilePickerPill contextKey="s1" />);
    await waitFor(() => expect(getChatContextProfileIdMock).toHaveBeenCalled());
    await userEvent.click(trigger());
    await userEvent.click(await screen.findByRole("button", { name: /Local llama/ }));
    expect(setChatContextProfileIdMock).toHaveBeenCalledWith("s1", "p-picked");
    await waitFor(() => expect(trigger()).toHaveTextContent("Local llama"));
  });

  it("toasts on a failed write and leaves the label unchanged", async () => {
    setChatContextProfileIdMock.mockRejectedValue(new Error("database is locked"));
    render(<ChatProfilePickerPill contextKey="s1" />);
    await waitFor(() => expect(getChatContextProfileIdMock).toHaveBeenCalled());
    await userEvent.click(trigger());
    await userEvent.click(await screen.findByRole("button", { name: /Local llama/ }));
    await waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith(
        "Couldn't save the model choice: database is locked",
      ),
    );
    expect(trigger()).toHaveTextContent("Default");
    expect(trigger()).not.toHaveTextContent("Local llama");
  });

  it("shows a dangling override (deleted Profile) as Default", async () => {
    getChatContextProfileIdMock.mockResolvedValue("p-deleted");
    render(<ChatProfilePickerPill contextKey="s1" />);
    await waitFor(() => expect(getChatContextProfileIdMock).toHaveBeenCalled());
    expect(trigger()).toHaveTextContent("Default");
    expect(trigger()).not.toHaveTextContent("Profile missing");
  });
});
