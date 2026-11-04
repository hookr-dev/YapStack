import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => ({
  toast: { info: vi.fn(), error: vi.fn(), warning: vi.fn(), success: vi.fn() },
}));

/** Persisted settings naming the removed fp32 Parakeet variant. */
function seedPersisted(version: number) {
  localStorage.setItem(
    "yapstack-settings",
    JSON.stringify({
      state: {
        settings: {
          selectedEngine: "Parakeet",
          selectedParakeetVariant: "TdtV3",
          language: "de",
        },
      },
      version,
    }),
  );
}

/** Fresh store + fresh command mocks, rehydrated from the seeded storage. */
async function launch() {
  const { commands } = await import("@/lib/tauri");
  const mocked = commands as unknown as Record<string, ReturnType<typeof vi.fn>>;
  mocked.backendReady = vi.fn().mockResolvedValue(true);
  const { useAppStore } = await import("./appStore");
  const phases: string[] = [];
  useAppStore.subscribe((s, prev) => {
    if (s.enginePhase !== prev.enginePhase) phases.push(s.enginePhase);
  });
  return { useAppStore, commands: mocked, phases };
}

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("upgrading from the fp32 Parakeet variant", () => {
  it("rehydrates a persisted TdtV3 as TdtV3Int8", async () => {
    seedPersisted(32);
    const { useAppStore } = await launch();
    const { settings } = useAppStore.getState();

    expect(settings.selectedParakeetVariant).toBe("TdtV3Int8");
    expect(settings.language).toBe("de");
  });

  it("migrates an alpha.12 (v31) Whisper user to Parakeet", async () => {
    localStorage.setItem(
      "yapstack-settings",
      JSON.stringify({
        state: {
          settings: {
            selectedEngine: "Whisper",
            selectedModelSize: "Small",
            selectedParakeetVariant: "TdtV3",
          },
        },
        version: 31,
      }),
    );
    const { useAppStore } = await launch();
    const { settings } = useAppStore.getState();

    expect(settings.selectedEngine).toBe("Parakeet");
    expect(settings.selectedParakeetVariant).toBe("TdtV3Int8");
  });

  it("launch downloads the int8 model with progress, then initializes it", async () => {
    seedPersisted(32);
    const { useAppStore, commands, phases } = await launch();

    await useAppStore.getState().autoSetup();

    expect(commands.downloadParakeetModel).toHaveBeenCalledWith("TdtV3Int8");
    expect(phases).toContain("downloading");
    expect(commands.initTranscriptionClient).toHaveBeenCalledWith(
      "Parakeet",
      "TdtV3Int8",
      false,
    );
    expect(useAppStore.getState().enginePhase).toBe("ready");
  });

  it("a failed download ends in enginePhase error with no fallback", async () => {
    seedPersisted(32);
    const { useAppStore, commands } = await launch();
    commands.downloadParakeetModel.mockResolvedValueOnce({
      status: "error",
      error: { type: "Transcription", message: "network unreachable" },
    });

    await useAppStore.getState().autoSetup();

    expect(useAppStore.getState().enginePhase).toBe("error");
    expect(useAppStore.getState().engineError).toBeTruthy();
    expect(commands.initTranscriptionClient).not.toHaveBeenCalled();
  });

  it("a second autoSetup while the first is still running starts no second download", async () => {
    seedPersisted(32);
    const { useAppStore, commands } = await launch();

    // Retry double-clicked from the error state: both calls start before
    // the first download resolves.
    await Promise.all([
      useAppStore.getState().autoSetup(),
      useAppStore.getState().autoSetup(),
    ]);

    expect(commands.downloadParakeetModel).toHaveBeenCalledTimes(1);
    expect(commands.initTranscriptionClient).toHaveBeenCalledTimes(1);
  });
});
