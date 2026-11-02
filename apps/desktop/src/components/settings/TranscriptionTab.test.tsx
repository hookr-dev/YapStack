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
import type { EngineDescriptorDto } from "@/lib/types";
import { TranscriptionTab } from "./TranscriptionTab";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@tauri-apps/plugin-sql", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

// Mirror of the Rust CATALOGUE: Parakeet is the only engine (ADR 0005).
const PARAKEET: EngineDescriptorDto = {
  kind: "Parakeet",
  display_name: "Parakeet",
  languages: ["en", "de", "fr"],
  supports_diarization: true,
};

function setup(
  overrides: Partial<ReturnType<typeof useAppStore.getState>> = {},
  language = "en",
) {
  useAppStore.setState({
    engineCatalogue: [PARAKEET],
    enginePhase: "ready",
    engineError: null,
    legacyModelFiles: [],
    refreshLegacyModelFiles: vi.fn().mockResolvedValue(undefined),
    settings: { ...useAppStore.getState().settings, language },
    updateSettings: vi.fn(),
    ...overrides,
  });
}

beforeEach(() => {
  vi.clearAllMocks();
});

afterEach(() => {
  cleanup();
});

describe("TranscriptionTab — Parakeet only", () => {
  it("shows Parakeet as a static engine line with no engine picker or Whisper sliders", () => {
    setup();
    render(<TranscriptionTab />);

    expect(screen.getByText("Parakeet TDT v3")).toBeInTheDocument();
    expect(screen.queryByText("Whisper")).not.toBeInTheDocument();
    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
    for (const gone of [
      "Advanced",
      "Max Chunk Duration",
      "Silence Pause",
      "Prompt Context",
      "Prompt Decay",
    ]) {
      expect(screen.queryByText(gone)).not.toBeInTheDocument();
    }
  });

  it("lists the auto-detected languages read-only", async () => {
    setup();
    render(<TranscriptionTab />);

    await userEvent.click(
      screen.getByText("Auto-detected: 3 European languages"),
    );
    expect(screen.getByText("English, French, German")).toBeInTheDocument();
  });

  it("shows a notice for a persisted language Parakeet doesn't support, without rewriting it", () => {
    const updateSettings = vi.fn();
    setup({ updateSettings }, "ja");
    render(<TranscriptionTab />);

    expect(screen.getByRole("status")).toHaveTextContent(
      "Japanese isn't supported yet.",
    );
    expect(updateSettings).not.toHaveBeenCalled();
  });

  it("shows no language notice for a supported language", () => {
    setup({}, "de");
    render(<TranscriptionTab />);
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });

  it("offers to remove old Whisper models with their total size and removes them on click", async () => {
    const deleteLegacyModelFiles = vi.fn().mockResolvedValue(undefined);
    setup({
      legacyModelFiles: [
        { name: "ggml-small.bin", bytes: 466_000_000 },
        { name: "ggml-silero-v6.2.0.bin", bytes: 885_000 },
      ],
      deleteLegacyModelFiles,
    });
    render(<TranscriptionTab />);

    expect(screen.getByText(/Remove old Whisper models \(/)).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole("button", { name: "Remove old Whisper models" }),
    );
    expect(deleteLegacyModelFiles).toHaveBeenCalledTimes(1);
  });

  it("hides the old-models row when nothing is left on disk", () => {
    setup();
    render(<TranscriptionTab />);
    expect(
      screen.queryByText(/Remove old Whisper models/),
    ).not.toBeInTheDocument();
  });

  it("shows the engine error with a Retry that re-runs setup", async () => {
    const autoSetup = vi.fn().mockResolvedValue(undefined);
    setup({
      enginePhase: "error",
      engineError: "network unreachable",
      autoSetup,
    });
    render(<TranscriptionTab />);

    expect(screen.getByText("network unreachable")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Retry/ }));
    expect(autoSetup).toHaveBeenCalledTimes(1);
  });
});
