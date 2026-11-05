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
vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn().mockResolvedValue("1.0.0-alpha.13"),
}));
const APP_VERSION = "1.0.0-alpha.13";

type Variant = "TdtV3Int8" | "TdtV3CoreMl";

/**
 * An Apple-silicon launch (the backend recommends CoreML) with `onDisk`
 * already downloaded. Downloads and deletes update what
 * `getParakeetModels` reports, like the real ModelManager.
 */
async function launchOnAppleSilicon(
  onDisk: Variant[],
  persisted: Record<string, unknown> = {},
) {
  localStorage.setItem(
    "yapstack-settings",
    JSON.stringify({
      state: {
        settings: {
          selectedEngine: "Parakeet",
          selectedParakeetVariant: onDisk[0] ?? "TdtV3Int8",
          ...persisted,
        },
      },
      version: 33,
    }),
  );
  const { commands } = await import("@/lib/tauri");
  const { toast } = await import("sonner");
  const mocked = commands as unknown as Record<string, ReturnType<typeof vi.fn>>;
  const disk = new Set<Variant>(onDisk);
  mocked.backendReady = vi.fn().mockResolvedValue(true);
  mocked.getRecommendedParakeetVariant = vi
    .fn()
    .mockResolvedValue({ status: "ok", data: "TdtV3CoreMl" });
  mocked.getParakeetModels = vi.fn().mockImplementation(async () => ({
    status: "ok",
    data: (["TdtV3Int8", "TdtV3CoreMl"] as Variant[]).map((variant) => ({
      variant,
      downloaded: disk.has(variant),
      display_name: variant,
      approximate_size_bytes: 1,
    })),
  }));
  mocked.downloadParakeetModel = vi.fn().mockImplementation(async (v: Variant) => {
    disk.add(v);
    return { status: "ok", data: "" };
  });
  mocked.deleteParakeetModel = vi.fn().mockImplementation(async (v: Variant) => {
    disk.delete(v);
    return { status: "ok", data: null };
  });
  const { useAppStore } = await import("./appStore");
  return {
    useAppStore,
    commands: mocked,
    warning: toast.warning as unknown as ReturnType<typeof vi.fn>,
    disk,
  };
}

const failure = (message: string) => ({
  status: "error",
  error: { type: "Transcription", message },
});

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("switching an int8 Mac to the CoreML model (ADR 0008)", () => {
  it("deletes int8 only after the CoreML model has loaded", async () => {
    const { useAppStore, commands, warning, disk } = await launchOnAppleSilicon([
      "TdtV3Int8",
    ]);

    await useAppStore.getState().autoSetup();

    expect(commands.downloadParakeetModel).toHaveBeenCalledWith("TdtV3CoreMl");
    expect(commands.initTranscriptionClient).toHaveBeenCalledTimes(1);
    expect(commands.initTranscriptionClient).toHaveBeenCalledWith(
      "Parakeet",
      "TdtV3CoreMl",
      false,
    );
    expect(commands.deleteParakeetModel).toHaveBeenCalledExactlyOnceWith("TdtV3Int8");
    expect(
      commands.deleteParakeetModel.mock.invocationCallOrder[0],
    ).toBeGreaterThan(commands.initTranscriptionClient.mock.invocationCallOrder[0]);
    expect([...disk]).toEqual(["TdtV3CoreMl"]);
    const state = useAppStore.getState();
    expect(state.settings.selectedParakeetVariant).toBe("TdtV3CoreMl");
    expect(state.enginePhase).toBe("ready");
    expect(warning).not.toHaveBeenCalled();
  });

  it("a failed CoreML download stays on int8 and warns", async () => {
    const { useAppStore, commands, warning, disk } = await launchOnAppleSilicon([
      "TdtV3Int8",
    ]);
    commands.downloadParakeetModel.mockResolvedValueOnce(
      failure("network unreachable"),
    );

    await useAppStore.getState().autoSetup();

    expect(commands.initTranscriptionClient).toHaveBeenCalledExactlyOnceWith(
      "Parakeet",
      "TdtV3Int8",
      false,
    );
    expect(commands.deleteParakeetModel).not.toHaveBeenCalled();
    expect(disk.has("TdtV3Int8")).toBe(true);
    expect(warning).toHaveBeenCalledTimes(1);
    expect(JSON.stringify(warning.mock.calls[0])).toContain("network unreachable");
    const state = useAppStore.getState();
    expect(state.settings.selectedParakeetVariant).toBe("TdtV3Int8");
    expect(state.enginePhase).toBe("ready");
  });

  it("a CoreML file that fails its checksum stays on int8 and warns", async () => {
    const { useAppStore, commands, warning, disk } = await launchOnAppleSilicon([
      "TdtV3Int8",
    ]);
    commands.downloadParakeetModel.mockResolvedValueOnce(
      failure("download failed: SHA-256 mismatch for https://huggingface.co/…"),
    );

    await useAppStore.getState().autoSetup();

    expect(commands.initTranscriptionClient).toHaveBeenCalledExactlyOnceWith(
      "Parakeet",
      "TdtV3Int8",
      false,
    );
    expect(commands.deleteParakeetModel).not.toHaveBeenCalled();
    expect(disk.has("TdtV3Int8")).toBe(true);
    expect(JSON.stringify(warning.mock.calls[0])).toContain("SHA-256 mismatch");
    expect(useAppStore.getState().enginePhase).toBe("ready");
  });

  it("a CoreML load or warm-up failure stays on int8 and warns", async () => {
    const { useAppStore, commands, warning, disk } = await launchOnAppleSilicon([
      "TdtV3Int8",
    ]);
    commands.initTranscriptionClient.mockResolvedValueOnce(
      failure("sidecar exited during the model load"),
    );

    await useAppStore.getState().autoSetup();

    expect(commands.initTranscriptionClient.mock.calls).toEqual([
      ["Parakeet", "TdtV3CoreMl", false],
      ["Parakeet", "TdtV3Int8", false],
    ]);
    expect(commands.deleteParakeetModel).not.toHaveBeenCalled();
    expect(disk.has("TdtV3Int8")).toBe(true);
    expect(JSON.stringify(warning.mock.calls[0])).toContain(
      "sidecar exited during the model load",
    );
    const state = useAppStore.getState();
    expect(state.settings.selectedParakeetVariant).toBe("TdtV3Int8");
    expect(state.enginePhase).toBe("ready");
  });

  it("a fresh install with no int8 to keep surfaces a failed download as an error", async () => {
    const { useAppStore, commands } = await launchOnAppleSilicon([]);
    commands.downloadParakeetModel.mockResolvedValueOnce(
      failure("network unreachable"),
    );

    await useAppStore.getState().autoSetup();

    expect(commands.initTranscriptionClient).not.toHaveBeenCalled();
    expect(useAppStore.getState().enginePhase).toBe("error");
  });

  it("a launch with only the CoreML model loads it and deletes nothing", async () => {
    const { useAppStore, commands, warning } = await launchOnAppleSilicon([
      "TdtV3CoreMl",
    ]);

    await useAppStore.getState().autoSetup();

    expect(commands.downloadParakeetModel).not.toHaveBeenCalled();
    expect(commands.initTranscriptionClient).toHaveBeenCalledExactlyOnceWith(
      "Parakeet",
      "TdtV3CoreMl",
      false,
    );
    expect(commands.deleteParakeetModel).not.toHaveBeenCalled();
    expect(warning).not.toHaveBeenCalled();
  });
});

describe("a failed switch is remembered for this app version", () => {
  const recorded = (appVersion: string) => ({
    coreMlSwitchFailure: { variant: "TdtV3CoreMl", appVersion },
  });

  it("records the failure, then the re-run after an engine drop stays on int8 quietly", async () => {
    const { useAppStore, commands, warning } = await launchOnAppleSilicon([
      "TdtV3Int8",
    ]);
    commands.downloadParakeetModel.mockResolvedValueOnce(
      failure("network unreachable"),
    );
    await useAppStore.getState().autoSetup();
    expect(useAppStore.getState().settings.coreMlSwitchFailure).toEqual({
      variant: "TdtV3CoreMl",
      appVersion: APP_VERSION,
    });
    expect(warning).toHaveBeenCalledTimes(1);

    // What the TRANSCRIPTION_ENGINE_DROPPED listener does.
    vi.clearAllMocks();
    useAppStore.setState({ enginePhase: "idle", engineError: null });
    await useAppStore.getState().autoSetup();

    expect(commands.downloadParakeetModel).not.toHaveBeenCalled();
    expect(commands.initTranscriptionClient).toHaveBeenCalledExactlyOnceWith(
      "Parakeet",
      "TdtV3Int8",
      false,
    );
    expect(commands.deleteParakeetModel).not.toHaveBeenCalled();
    expect(warning).not.toHaveBeenCalled();
    expect(useAppStore.getState().enginePhase).toBe("ready");
  });

  it("a launch on the same app version skips the switch quietly", async () => {
    const { useAppStore, commands, warning, disk } = await launchOnAppleSilicon(
      ["TdtV3Int8"],
      recorded(APP_VERSION),
    );

    await useAppStore.getState().autoSetup();

    expect(commands.downloadParakeetModel).not.toHaveBeenCalled();
    expect(commands.initTranscriptionClient).toHaveBeenCalledExactlyOnceWith(
      "Parakeet",
      "TdtV3Int8",
      false,
    );
    expect(disk.has("TdtV3Int8")).toBe(true);
    expect(warning).not.toHaveBeenCalled();
    expect(useAppStore.getState().settings.selectedParakeetVariant).toBe("TdtV3Int8");
  });

  it("a new app version tries the switch again, and success clears the record", async () => {
    const { useAppStore, commands, disk } = await launchOnAppleSilicon(
      ["TdtV3Int8"],
      recorded("1.0.0-alpha.12"),
    );

    await useAppStore.getState().autoSetup();

    expect(commands.downloadParakeetModel).toHaveBeenCalledWith("TdtV3CoreMl");
    expect(commands.initTranscriptionClient).toHaveBeenCalledExactlyOnceWith(
      "Parakeet",
      "TdtV3CoreMl",
      false,
    );
    expect([...disk]).toEqual(["TdtV3CoreMl"]);
    expect(useAppStore.getState().settings.coreMlSwitchFailure).toBeNull();
  });

  it("Retry ignores the record", async () => {
    const { useAppStore, commands } = await launchOnAppleSilicon(
      ["TdtV3Int8"],
      recorded(APP_VERSION),
    );

    await useAppStore.getState().autoSetup({ retry: true });

    expect(commands.downloadParakeetModel).toHaveBeenCalledWith("TdtV3CoreMl");
    expect(commands.initTranscriptionClient).toHaveBeenCalledWith(
      "Parakeet",
      "TdtV3CoreMl",
      false,
    );
  });

  it("a fresh install with a record still tries CoreML and errors on failure", async () => {
    const { useAppStore, commands } = await launchOnAppleSilicon(
      [],
      recorded(APP_VERSION),
    );
    commands.downloadParakeetModel.mockResolvedValueOnce(
      failure("network unreachable"),
    );

    await useAppStore.getState().autoSetup();

    expect(commands.downloadParakeetModel).toHaveBeenCalledWith("TdtV3CoreMl");
    expect(commands.initTranscriptionClient).not.toHaveBeenCalled();
    expect(useAppStore.getState().enginePhase).toBe("error");
  });
});

describe("Try the Neural Engine model again", () => {
  const recorded = {
    coreMlSwitchFailure: { variant: "TdtV3CoreMl", appVersion: APP_VERSION },
  };

  it("restarts the engine on the CoreML model and clears the record", async () => {
    const { useAppStore, commands, disk } = await launchOnAppleSilicon(
      ["TdtV3Int8", "TdtV3CoreMl"],
      recorded,
    );
    await useAppStore.getState().autoSetup();
    expect(commands.initTranscriptionClient).toHaveBeenLastCalledWith(
      "Parakeet",
      "TdtV3Int8",
      false,
    );
    vi.clearAllMocks();

    await useAppStore.getState().retryCoreMlSwitch();

    expect(commands.shutdownTranscriptionClient).toHaveBeenCalledTimes(1);
    expect(
      commands.shutdownTranscriptionClient.mock.invocationCallOrder[0],
    ).toBeLessThan(commands.initTranscriptionClient.mock.invocationCallOrder[0]);
    expect(commands.initTranscriptionClient).toHaveBeenCalledExactlyOnceWith(
      "Parakeet",
      "TdtV3CoreMl",
      false,
    );
    expect([...disk]).toEqual(["TdtV3CoreMl"]);
    const state = useAppStore.getState();
    expect(state.settings.coreMlSwitchFailure).toBeNull();
    expect(state.settings.selectedParakeetVariant).toBe("TdtV3CoreMl");
    expect(state.enginePhase).toBe("ready");
  });

  it("records the failure again when the CoreML model still won't load", async () => {
    const { useAppStore, commands, disk } = await launchOnAppleSilicon(
      ["TdtV3Int8", "TdtV3CoreMl"],
      recorded,
    );
    await useAppStore.getState().autoSetup();
    commands.initTranscriptionClient.mockResolvedValueOnce(
      failure("sidecar exited during the model load"),
    );

    await useAppStore.getState().retryCoreMlSwitch();

    expect(disk.has("TdtV3Int8")).toBe(true);
    const state = useAppStore.getState();
    expect(state.settings.selectedParakeetVariant).toBe("TdtV3Int8");
    expect(state.settings.coreMlSwitchFailure).toEqual(recorded.coreMlSwitchFailure);
    expect(state.enginePhase).toBe("ready");
  });

  it("keeps the running engine and the record when shutdown is refused", async () => {
    const { useAppStore, commands } = await launchOnAppleSilicon(
      ["TdtV3Int8", "TdtV3CoreMl"],
      recorded,
    );
    await useAppStore.getState().autoSetup();
    vi.clearAllMocks();
    commands.shutdownTranscriptionClient.mockResolvedValueOnce({
      status: "error",
      error: { type: "InvalidInput", message: "live transcription is running" },
    });

    await expect(useAppStore.getState().retryCoreMlSwitch()).rejects.toThrow(
      "live transcription is running",
    );
    expect(commands.initTranscriptionClient).not.toHaveBeenCalled();
    const state = useAppStore.getState();
    expect(state.settings.coreMlSwitchFailure).toEqual(recorded.coreMlSwitchFailure);
    expect(state.enginePhase).toBe("ready");
  });
});

describe("a CoreML load that hangs", () => {
  // The backend gives the first load its budget, then fails init.
  const hung = () => failure("request timed out after 190s");

  it("surfaces the init error on a Mac with nothing to fall back to, and never reports ready", async () => {
    const { useAppStore, commands } = await launchOnAppleSilicon(["TdtV3CoreMl"]);
    commands.initTranscriptionClient.mockResolvedValueOnce(hung());
    const phases: string[] = [];
    const unsubscribe = useAppStore.subscribe((s) => phases.push(s.enginePhase));

    await useAppStore.getState().autoSetup();
    unsubscribe();

    expect(phases).toContain("initializing");
    expect(phases).not.toContain("ready");
    const state = useAppStore.getState();
    expect(state.enginePhase).toBe("error");
    expect(state.engineError).toBe("request timed out after 190s");
  });

  it("falls back to int8 when it's still on disk", async () => {
    const { useAppStore, commands, warning } = await launchOnAppleSilicon([
      "TdtV3Int8",
    ]);
    commands.initTranscriptionClient.mockResolvedValueOnce(hung());

    await useAppStore.getState().autoSetup();

    expect(commands.initTranscriptionClient).toHaveBeenLastCalledWith(
      "Parakeet",
      "TdtV3Int8",
      false,
    );
    expect(JSON.stringify(warning.mock.calls[0])).toContain("timed out");
    expect(useAppStore.getState().enginePhase).toBe("ready");
  });
});

describe("deleting a Parakeet model", () => {
  it.each(["downloading", "initializing"] as const)(
    "is refused while the engine is %s",
    async (phase) => {
      const { useAppStore, commands, disk } = await launchOnAppleSilicon([
        "TdtV3Int8",
      ]);
      useAppStore.setState({ enginePhase: phase });

      await expect(
        useAppStore.getState().deleteParakeetModel("TdtV3Int8"),
      ).rejects.toThrow("Cannot delete the active model");
      expect(commands.deleteParakeetModel).not.toHaveBeenCalled();
      expect(disk.has("TdtV3Int8")).toBe(true);
    },
  );

  it("is refused for the selected model while it is ready", async () => {
    const { useAppStore, commands } = await launchOnAppleSilicon(["TdtV3Int8"]);
    useAppStore.setState({ enginePhase: "ready" });

    await expect(
      useAppStore.getState().deleteParakeetModel("TdtV3Int8"),
    ).rejects.toThrow("Cannot delete the active model");
    expect(commands.deleteParakeetModel).not.toHaveBeenCalled();
  });

  it("deletes a model that isn't in use", async () => {
    const { useAppStore, commands } = await launchOnAppleSilicon(["TdtV3Int8"]);
    useAppStore.setState({ enginePhase: "error" });

    await useAppStore.getState().deleteParakeetModel("TdtV3Int8");
    expect(commands.deleteParakeetModel).toHaveBeenCalledWith("TdtV3Int8");
  });
});
