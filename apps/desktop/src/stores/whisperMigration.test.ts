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

/** A Whisper user's persisted settings as v31 wrote them (ADR 0005). */
function seedPersisted(version: number) {
  localStorage.setItem(
    "yapstack-settings",
    JSON.stringify({
      state: {
        settings: {
          selectedEngine: "Whisper",
          selectedModelSize: "Small",
          selectedParakeetVariant: "TdtV3",
          language: "ja",
          silenceDurationMs: 800,
          maxChunkSeconds: 30,
          promptContextChars: 350,
          promptDecaySilenceSeconds: 5,
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

describe("Whisper user upgrading to the Parakeet-only build", () => {
  it("rehydrates v31 Whisper settings as Parakeet and drops the Whisper-only fields", async () => {
    seedPersisted(31);
    const { useAppStore } = await launch();
    const settings = useAppStore.getState().settings as unknown as Record<
      string,
      unknown
    >;

    expect(settings.selectedEngine).toBe("Parakeet");
    expect(settings.whisperRemovedNotice).toBe(true);
    // The language is kept for later routing; only the notice uses it today.
    expect(settings.language).toBe("ja");
    for (const key of [
      "selectedModelSize",
      "silenceDurationMs",
      "maxChunkSeconds",
      "promptContextChars",
      "promptDecaySilenceSeconds",
    ]) {
      expect(settings).not.toHaveProperty(key);
    }
  });

  it("launch takes the Parakeet download path with progress, then initializes Parakeet", async () => {
    seedPersisted(31);
    const { useAppStore, commands, phases } = await launch();

    await useAppStore.getState().autoSetup();

    expect(commands.downloadParakeetModel).toHaveBeenCalledWith("TdtV3");
    expect(phases).toContain("downloading");
    expect(commands.initTranscriptionClient).toHaveBeenCalledWith(
      "Parakeet",
      "TdtV3",
      false,
    );
    expect(useAppStore.getState().enginePhase).toBe("ready");
    expect(useAppStore.getState().settings.whisperRemovedNotice).toBe(false);
  });

  it("a failed download (offline first launch) ends in enginePhase error with no fallback", async () => {
    seedPersisted(31);
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

  it("merge coerces a Whisper engine even when the persisted version is already current", async () => {
    // Persist-version skew: a v32 blob that still says Whisper must never
    // reach the backend, which no longer accepts that engine.
    seedPersisted(32);
    const { useAppStore } = await launch();

    expect(useAppStore.getState().settings.selectedEngine).toBe("Parakeet");
  });
});
