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

type Variant = "TdtV3Int8" | "TdtV3CoreMl";

/**
 * An Apple-silicon launch (the backend recommends CoreML) with `onDisk`
 * already downloaded. Downloads and deletes update what
 * `getParakeetModels` reports, like the real ModelManager.
 */
async function launchOnAppleSilicon(onDisk: Variant[]) {
  localStorage.setItem(
    "yapstack-settings",
    JSON.stringify({
      state: {
        settings: {
          selectedEngine: "Parakeet",
          selectedParakeetVariant: onDisk[0] ?? "TdtV3Int8",
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
