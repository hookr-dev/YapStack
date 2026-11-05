import { act, render, screen, cleanup } from "@testing-library/react";
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

  it("disables Retry while setup is running, so a double click retries once", async () => {
    let finish: () => void = () => {};
    const autoSetup = vi.fn(
      () => new Promise<void>((resolve) => (finish = resolve)),
    );
    setup({ enginePhase: "error", engineError: "offline", autoSetup });
    render(<TranscriptionTab />);

    const retry = screen.getByRole("button", { name: /Retry/ });
    await userEvent.dblClick(retry);
    expect(autoSetup).toHaveBeenCalledTimes(1);
    expect(retry).toBeDisabled();
    finish();
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

  it("shows a hung load's init error with Retry, and no Active model", async () => {
    const autoSetup = vi.fn().mockResolvedValue(undefined);
    setup({
      enginePhase: "error",
      engineError: "request timed out after 190s",
      autoSetup,
      parakeetModels: [COREML],
      settings: settingsWith({ selectedParakeetVariant: "TdtV3CoreMl" }),
    });
    render(<TranscriptionTab />);

    expect(screen.getByText("request timed out after 190s")).toBeInTheDocument();
    expect(screen.queryByText("Active")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: /Retry/ }));
    expect(autoSetup).toHaveBeenCalledExactlyOnceWith({ retry: true });
  });
});

const INT8 = {
  variant: "TdtV3Int8" as const,
  downloaded: true,
  display_name: "Parakeet TDT v3 (~700 MB)",
  approximate_size_bytes: 700_000_000,
};
const COREML = {
  variant: "TdtV3CoreMl" as const,
  downloaded: true,
  display_name: "Parakeet TDT v3 (~630 MB)",
  approximate_size_bytes: 630_000_000,
};

function settingsWith(overrides: Record<string, unknown>) {
  return { ...useAppStore.getState().settings, language: "en", ...overrides };
}

describe("TranscriptionTab — Parakeet models", () => {
  it("shows one int8 row where the CoreML model isn't offered (Windows, Intel)", () => {
    setup({
      parakeetModels: [INT8],
      settings: settingsWith({ selectedParakeetVariant: "TdtV3Int8" }),
    });
    render(<TranscriptionTab />);

    expect(screen.getByText(INT8.display_name)).toBeInTheDocument();
    expect(screen.getByText("Active")).toBeInTheDocument();
    expect(screen.queryByText(/Runs on/)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Delete model" })).toBeNull();
  });

  it("shows the CoreML row next to the int8 model in use, and deletes it", async () => {
    const deleteParakeetModel = vi.fn().mockResolvedValue(undefined);
    setup({
      parakeetModels: [INT8, COREML],
      deleteParakeetModel,
      settings: settingsWith({ selectedParakeetVariant: "TdtV3Int8" }),
    });
    render(<TranscriptionTab />);

    expect(screen.getByText(COREML.display_name)).toBeInTheDocument();
    expect(screen.getByText("Runs on the Neural Engine")).toBeInTheDocument();
    expect(screen.getByText("Runs on the CPU or GPU")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Delete model" }));
    expect(deleteParakeetModel).toHaveBeenCalledExactlyOnceWith("TdtV3CoreMl");
  });

  it("offers no Delete for the model being set up, only once its setup failed", () => {
    setup({
      enginePhase: "initializing",
      parakeetModels: [COREML],
      settings: settingsWith({ selectedParakeetVariant: "TdtV3CoreMl" }),
    });
    const { unmount } = render(<TranscriptionTab />);
    expect(screen.queryByRole("button", { name: "Delete model" })).toBeNull();
    unmount();

    setup({
      enginePhase: "error",
      engineError: "request timed out after 190s",
      parakeetModels: [COREML],
      settings: settingsWith({ selectedParakeetVariant: "TdtV3CoreMl" }),
    });
    render(<TranscriptionTab />);
    expect(screen.getByRole("button", { name: "Delete model" })).toBeInTheDocument();
  });

  it("shows why a delete was refused", async () => {
    const { toast } = await import("sonner");
    const error = vi.spyOn(toast, "error");
    setup({
      parakeetModels: [INT8, COREML],
      deleteParakeetModel: vi
        .fn()
        .mockRejectedValue(new Error("Cannot delete the active model")),
      settings: settingsWith({ selectedParakeetVariant: "TdtV3Int8" }),
    });
    render(<TranscriptionTab />);

    await userEvent.click(screen.getByRole("button", { name: "Delete model" }));
    expect(error).toHaveBeenCalledWith("Cannot delete the active model");
  });

  it("offers to try the Neural Engine model again after a failed switch", async () => {
    const retryCoreMlSwitch = vi.fn().mockResolvedValue(undefined);
    setup({
      parakeetModels: [INT8, COREML],
      retryCoreMlSwitch,
      settings: settingsWith({
        selectedParakeetVariant: "TdtV3Int8",
        coreMlSwitchFailure: { variant: "TdtV3CoreMl", appVersion: "1.0.0" },
      }),
    });
    render(<TranscriptionTab />);

    await userEvent.click(
      screen.getByRole("button", { name: /Try the Neural Engine model again/ }),
    );
    expect(retryCoreMlSwitch).toHaveBeenCalledTimes(1);
  });

  it("doesn't offer it without a recorded failure", () => {
    setup({
      parakeetModels: [COREML],
      settings: settingsWith({
        selectedParakeetVariant: "TdtV3CoreMl",
        coreMlSwitchFailure: null,
      }),
    });
    render(<TranscriptionTab />);
    expect(
      screen.queryByRole("button", { name: /Try the Neural Engine model again/ }),
    ).toBeNull();
  });

  it("says the model is being prepared once a CoreML load runs past 3 s", () => {
    vi.useFakeTimers();
    try {
      setup({
        enginePhase: "initializing",
        initializingParakeet: { variant: "TdtV3CoreMl", startedAt: Date.now() },
        parakeetModels: [COREML],
      });
      render(<TranscriptionTab />);
      expect(screen.queryByRole("status")).toBeNull();
      act(() => vi.advanceTimersByTime(3_000));
      expect(screen.getByRole("status")).toHaveTextContent(
        "Setting up for the Neural Engine. This happens once.",
      );
    } finally {
      vi.useRealTimers();
    }
  });
});
