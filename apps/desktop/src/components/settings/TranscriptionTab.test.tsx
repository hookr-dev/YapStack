import { render, screen, cleanup } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import { useAppStore } from "@/stores/appStore";
import type { EngineDescriptorDto, EngineKindDto } from "@/lib/types";
import { TranscriptionTab } from "./TranscriptionTab";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@tauri-apps/plugin-sql", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

// Mirror of the Rust CATALOGUE capability flags that matter here: Whisper
// seeds an initial prompt, Parakeet does not.
const WHISPER: EngineDescriptorDto = {
  kind: "Whisper",
  display_name: "Whisper",
  languages: ["en"],
  supports_diarization: false,
  supports_initial_prompt: true,
};

const PARAKEET: EngineDescriptorDto = {
  kind: "Parakeet",
  display_name: "Parakeet",
  languages: ["en"],
  supports_diarization: true,
  supports_initial_prompt: false,
};

function setupEngine(selectedEngine: EngineKindDto) {
  useAppStore.setState({
    engineCatalogue: [WHISPER, PARAKEET],
    settings: {
      ...useAppStore.getState().settings,
      selectedEngine,
    },
    updateSettings: vi.fn(),
  });
}

// Advanced settings live inside a Collapsible that is collapsed by default,
// so its content is not mounted until the trigger is clicked.
async function openAdvanced() {
  await userEvent.click(screen.getByText("Advanced"));
}

beforeEach(() => {
  vi.clearAllMocks();
});

afterEach(() => {
  cleanup();
});

describe("TranscriptionTab — Advanced capability gating", () => {
  it("shows Prompt Context and Prompt Decay when the engine supports an initial prompt (Whisper)", async () => {
    setupEngine("Whisper");
    render(<TranscriptionTab />);
    await openAdvanced();

    expect(screen.getByText("Prompt Context")).toBeInTheDocument();
    expect(screen.getByText("Prompt Decay")).toBeInTheDocument();
  });

  it("hides Prompt Context and Prompt Decay when the engine has no initial prompt (Parakeet)", async () => {
    setupEngine("Parakeet");
    render(<TranscriptionTab />);
    await openAdvanced();

    expect(screen.queryByText("Prompt Context")).not.toBeInTheDocument();
    expect(screen.queryByText("Prompt Decay")).not.toBeInTheDocument();
  });

  it("keeps the engine-agnostic Max Chunk and Silence Pause rows for both engines", async () => {
    setupEngine("Whisper");
    render(<TranscriptionTab />);
    await openAdvanced();
    expect(screen.getByText("Max Chunk Duration")).toBeInTheDocument();
    expect(screen.getByText("Silence Pause")).toBeInTheDocument();
    cleanup();

    setupEngine("Parakeet");
    render(<TranscriptionTab />);
    await openAdvanced();
    expect(screen.getByText("Max Chunk Duration")).toBeInTheDocument();
    expect(screen.getByText("Silence Pause")).toBeInTheDocument();
  });

  it("does not clobber persisted prompt settings when the active engine hides them", async () => {
    setupEngine("Parakeet");
    const updateSettings = vi.fn();
    useAppStore.setState({
      settings: {
        ...useAppStore.getState().settings,
        promptContextChars: 350,
        promptDecaySilenceSeconds: 5,
      },
      updateSettings,
    });

    render(<TranscriptionTab />);
    await openAdvanced();

    // Hiding is UI-only: no write to the prompt settings on render.
    const touchedPrompt = updateSettings.mock.calls.some(([patch]) => {
      const p = patch as Record<string, unknown>;
      return (
        "promptContextChars" in p || "promptDecaySilenceSeconds" in p
      );
    });
    expect(touchedPrompt).toBe(false);
    expect(useAppStore.getState().settings.promptContextChars).toBe(350);
    expect(useAppStore.getState().settings.promptDecaySilenceSeconds).toBe(5);
  });
});
