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

const toastInfo = vi.fn();
vi.mock("sonner", () => ({
  toast: {
    info: (msg: string, opts?: unknown) => toastInfo(msg, opts),
    error: vi.fn(),
    warning: vi.fn(),
    success: vi.fn(),
  },
}));

import { useAppStore } from "./appStore";
import type { AudioDeviceInfoDto } from "@/lib/tauri";

const MIC_BUILTIN: AudioDeviceInfoDto = {
  id: "CoreAudio:BuiltInMic",
  name: "MacBook Pro Microphone",
  device_type: "Input",
  is_default: true,
};

const MIC_USB: AudioDeviceInfoDto = {
  id: "CoreAudio:USB-Mic-X",
  name: "USB Mic X",
  device_type: "Input",
  is_default: false,
};

const SPEAKER: AudioDeviceInfoDto = {
  id: "CoreAudio:BuiltInSpeaker",
  name: "MacBook Pro Speakers",
  device_type: "Output",
  is_default: true,
};

beforeEach(() => {
  toastInfo.mockClear();
});

describe("applyDeviceList — selectedMicDeviceId reconciliation", () => {
  it("preserves the persisted selection when it is still present", () => {
    useAppStore.setState((s) => ({
      devices: [MIC_BUILTIN, MIC_USB],
      settings: { ...s.settings, selectedMicDeviceId: "CoreAudio:USB-Mic-X" },
    }));

    useAppStore.getState().applyDeviceList([MIC_BUILTIN, MIC_USB, SPEAKER]);

    expect(useAppStore.getState().settings.selectedMicDeviceId).toBe(
      "CoreAudio:USB-Mic-X",
    );
    expect(toastInfo).not.toHaveBeenCalled();
  });

  it("resets the persisted selection to null when its device disappears", () => {
    useAppStore.setState((s) => ({
      devices: [MIC_BUILTIN, MIC_USB],
      settings: { ...s.settings, selectedMicDeviceId: "CoreAudio:USB-Mic-X" },
    }));

    // Only the builtin remains — USB mic was unplugged.
    useAppStore.getState().applyDeviceList([MIC_BUILTIN, SPEAKER]);

    expect(useAppStore.getState().settings.selectedMicDeviceId).toBeNull();
  });

  it("toasts with the new effective default name on reconciliation", () => {
    useAppStore.setState((s) => ({
      devices: [MIC_BUILTIN, MIC_USB],
      settings: { ...s.settings, selectedMicDeviceId: "CoreAudio:USB-Mic-X" },
    }));

    useAppStore.getState().applyDeviceList([MIC_BUILTIN, SPEAKER]);

    expect(toastInfo).toHaveBeenCalledTimes(1);
    const [msg, opts] = toastInfo.mock.calls[0];
    expect(msg).toContain("disappeared");
    expect(msg).toContain("MacBook Pro Microphone");
    expect((opts as { id?: string }).id).toBe("mic-disappeared");
  });

  it("does nothing when the user is already on follow-default (null selection)", () => {
    useAppStore.setState((s) => ({
      devices: [MIC_BUILTIN],
      settings: { ...s.settings, selectedMicDeviceId: null },
    }));

    useAppStore.getState().applyDeviceList([MIC_BUILTIN, MIC_USB, SPEAKER]);

    expect(useAppStore.getState().settings.selectedMicDeviceId).toBeNull();
    expect(toastInfo).not.toHaveBeenCalled();
  });

  it("does not match an output device against a mic selection by id", () => {
    // Defends against an output device with the same id as a missing
    // mic id from being treated as 'still present'.
    useAppStore.setState((s) => ({
      devices: [MIC_USB],
      settings: { ...s.settings, selectedMicDeviceId: "CoreAudio:USB-Mic-X" },
    }));

    const ghostOutput: AudioDeviceInfoDto = {
      ...MIC_USB,
      device_type: "Output",
    };
    useAppStore.getState().applyDeviceList([MIC_BUILTIN, ghostOutput]);

    expect(useAppStore.getState().settings.selectedMicDeviceId).toBeNull();
  });

  it("falls back to a generic message when no input device is marked default", () => {
    useAppStore.setState((s) => ({
      devices: [MIC_USB],
      settings: { ...s.settings, selectedMicDeviceId: "CoreAudio:USB-Mic-X" },
    }));

    // Empty list (catastrophic — all devices gone).
    useAppStore.getState().applyDeviceList([]);

    expect(useAppStore.getState().settings.selectedMicDeviceId).toBeNull();
    expect(toastInfo).toHaveBeenCalledTimes(1);
    const [msg] = toastInfo.mock.calls[0];
    expect(msg).toContain("system default");
  });
});
