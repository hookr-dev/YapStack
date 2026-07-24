import { vi } from "vitest";

/**
 * Shared mock factory functions for Tauri APIs.
 *
 * IMPORTANT: vi.mock() calls must be in the test file itself for hoisting to work.
 * Import these factories and use them in vi.mock() calls:
 *
 *   import { tauriCoreMock, tauriEventMock, ... } from "@/test/tauri-mocks";
 *   vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
 */

export const tauriCoreMock = () => ({
  invoke: vi.fn().mockResolvedValue(null),
  // Identity passthrough — tests only need a deterministic string, not a real
  // asset URL, and components (e.g. NoteDetailView) call this to build
  // `audio-stream://` sources.
  convertFileSrc: vi.fn((path: string) => path),
});

export const tauriEventMock = () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
  emit: vi.fn(),
});

export const tauriWindowMock = () => ({
  getCurrentWindow: vi.fn().mockReturnValue({
    setPosition: vi.fn().mockResolvedValue(undefined),
    setSize: vi.fn().mockResolvedValue(undefined),
    outerPosition: vi.fn().mockResolvedValue({ x: 0, y: 0 }),
    innerSize: vi.fn().mockResolvedValue({ width: 1100, height: 750 }),
    onCloseRequested: vi.fn().mockResolvedValue(() => {}),
    onMoved: vi.fn().mockResolvedValue(() => {}),
    onResized: vi.fn().mockResolvedValue(() => {}),
    hide: vi.fn().mockResolvedValue(undefined),
    show: vi.fn().mockResolvedValue(undefined),
    setFocus: vi.fn().mockResolvedValue(undefined),
  }),
});

export const tauriDpiMock = () => ({
  PhysicalPosition: vi.fn(),
  PhysicalSize: vi.fn(),
});

export const tauriWebviewWindowMock = () => ({
  WebviewWindow: {
    getByLabel: vi.fn().mockResolvedValue(null),
  },
});

export const tauriSqlMock = () => ({
  default: {
    load: vi.fn().mockResolvedValue({
      select: vi.fn().mockResolvedValue([]),
      execute: vi.fn().mockResolvedValue(null),
    }),
  },
});

export const tauriDialogMock = () => ({
  open: vi.fn().mockResolvedValue(null),
  save: vi.fn().mockResolvedValue(null),
  message: vi.fn().mockResolvedValue(undefined),
  ask: vi.fn().mockResolvedValue(false),
  confirm: vi.fn().mockResolvedValue(false),
});

export const tauriFsMock = () => ({
  readTextFile: vi.fn().mockResolvedValue(""),
  writeTextFile: vi.fn().mockResolvedValue(undefined),
  exists: vi.fn().mockResolvedValue(false),
  mkdir: vi.fn().mockResolvedValue(undefined),
  remove: vi.fn().mockResolvedValue(undefined),
});

export const tauriOpenerMock = () => ({
  revealItemInDir: vi.fn().mockResolvedValue(undefined),
  openUrl: vi.fn().mockResolvedValue(undefined),
});

export const tauriGlobalShortcutMock = () => ({
  register: vi.fn().mockResolvedValue(undefined),
  unregister: vi.fn().mockResolvedValue(undefined),
  unregisterAll: vi.fn().mockResolvedValue(undefined),
  isRegistered: vi.fn().mockResolvedValue(false),
});

export const tauriPathMock = () => ({
  appDataDir: vi.fn().mockResolvedValue("/mock/app-data"),
  appConfigDir: vi.fn().mockResolvedValue("/mock/app-config"),
});

export const tauriCommandsMock = () => ({
  commands: {
    healthCheck: vi.fn().mockResolvedValue({ status: "ok", version: "0.1.0" }),
    logFrontend: vi.fn().mockResolvedValue(undefined),
    writeTextFile: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    listAudioDevices: vi.fn().mockResolvedValue({ status: "ok", data: [] }),
    getDefaultInputDevice: vi
      .fn()
      .mockResolvedValue({ status: "error", error: "no device" }),
    startCapture: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    stopCapture: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    getCaptureStatus: vi.fn().mockResolvedValue({
      status: "ok",
      data: {
        state: "Idle",
        mic_active: false,
        system_audio_active: false,
        error_message: null,
      },
    }),
    checkSystemAudioPermission: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: "Granted" }),
    getBufferInfo: vi.fn().mockResolvedValue({
      status: "ok",
      data: { mic: null, system: null },
    }),
    getAvailableModels: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: [] }),
    downloadModel: vi.fn().mockResolvedValue({ status: "ok", data: "" }),
    deleteModel: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    initTranscriptionClient: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    shutdownTranscriptionClient: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    getTranscriptionStatus: vi.fn().mockResolvedValue({
      status: "ok",
      data: { initialized: false },
    }),
    getEngineCatalogue: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: [] }),
    getParakeetModels: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: [] }),
    // Test environment defaults to the fp32 variant — matches non-Apple-Silicon
    // CI runners. Tests that exercise the int8 path can override this mock.
    getRecommendedParakeetVariant: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: "TdtV3" }),
    downloadParakeetModel: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: "" }),
    deleteParakeetModel: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    getSortformerStatus: vi.fn().mockResolvedValue({
      status: "ok",
      data: { downloaded: false, sample_rate: 16000 },
    }),
    downloadSortformerModel: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: "" }),
    deleteSortformerModel: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    startLiveTranscription: vi.fn().mockResolvedValue({
      status: "ok",
      data: { effective_start_epoch_ms: 0 },
    }),
    stopLiveTranscription: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    getLiveTranscriptionStatus: vi.fn().mockResolvedValue({
      status: "ok",
      data: {
        phase: "Stopped",
        chunks_processed: 0,
        total_audio_seconds: 0,
        error_message: null,
        session_id: null,
        effective_start_epoch_ms: null,
        lag_seconds: null,
        live_drain_backlog_chunks: 0,
        live_drain_backlog_seconds: 0,
      },
    }),
    clipboardPaste: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    applyVolumeDuck: vi.fn().mockResolvedValue(undefined),
    restoreVolume: vi.fn().mockResolvedValue(undefined),
    checkScreenCapturePermission: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: "Granted" }),
    requestScreenCapturePermission: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: "Granted" }),
    updateVocabularyHints: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    deleteAudioFiles: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    deleteSessionWav: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    // Infallible command: resolves a plain boolean[] parallel to the input.
    // Default to "all present" so unrelated tests keep the normal player.
    audioFilesExist: vi.fn().mockResolvedValue([]),
    peekCaptureEnergy: vi.fn().mockResolvedValue({
      status: "ok",
      data: { mic_rms: 0, system_rms: 0 },
    }),
    getAutostartEnabled: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: false }),
    setAutostartEnabled: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    showOverlayPanel: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    hideOverlayPanel: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: null }),
    getRecentLogs: vi.fn().mockResolvedValue({ status: "ok", data: [] }),
    clearLogs: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    getLogDir: vi.fn().mockResolvedValue({ status: "ok", data: "/mock/logs" }),
    revealLogDir: vi.fn().mockResolvedValue({ status: "ok", data: null }),
  },
});
