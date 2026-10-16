import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
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

// The ONLY thing stubbed on the analytics path is the Aptabase SDK itself, so this
// exercises the real gate through the real store action — the wiring, not a double.
const { trackEventMock } = vi.hoisted(() => ({
  trackEventMock: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@aptabase/tauri", () => ({
  trackEvent: (...args: unknown[]) => trackEventMock(...args),
}));

import { useAppStore } from "./appStore";
import { isAnalyticsEnabled, trackSessionDeleted } from "@/lib/analytics";

/** Snapshot the shipped default BEFORE any test mutates it. jsdom starts with empty
 *  localStorage, so this is the rehydrated-from-nothing value. */
const shippedDefault = useAppStore.getState().settings.analyticsEnabled;

beforeEach(() => {
  trackEventMock.mockClear();
});

afterEach(() => {
  // Leave the store + module gate in the default state for other suites.
  useAppStore.getState().updateSettings({ analyticsEnabled: true });
});

describe("analytics opt-out — store wiring", () => {
  it("ships ON, matching the pre-toggle behavior, with the gate open", () => {
    expect(shippedDefault).toBe(true);
    expect(isAnalyticsEnabled()).toBe(true);
  });

  it("updateSettings({ analyticsEnabled: false }) closes the emit gate", () => {
    useAppStore.getState().updateSettings({ analyticsEnabled: false });

    expect(useAppStore.getState().settings.analyticsEnabled).toBe(false);
    expect(isAnalyticsEnabled()).toBe(false);

    trackSessionDeleted();
    expect(trackEventMock).not.toHaveBeenCalled();
  });

  it("opting out never emits telemetry about the opt-out itself", () => {
    // The gate is applied before updateSettings' tracked-key loop, and
    // analyticsEnabled is deliberately not a tracked key — so the flip is silent
    // in BOTH directions.
    useAppStore.getState().updateSettings({ analyticsEnabled: false });
    expect(trackEventMock).not.toHaveBeenCalled();

    trackEventMock.mockClear();
    useAppStore.getState().updateSettings({ analyticsEnabled: true });
    expect(trackEventMock).not.toHaveBeenCalled();
  });

  it("re-enabling reopens the gate", () => {
    useAppStore.getState().updateSettings({ analyticsEnabled: false });
    trackSessionDeleted();
    expect(trackEventMock).not.toHaveBeenCalled();

    useAppStore.getState().updateSettings({ analyticsEnabled: true });
    expect(isAnalyticsEnabled()).toBe(true);
    trackSessionDeleted();
    expect(trackEventMock).toHaveBeenCalledWith("session_deleted", undefined);
  });

  it("leaves an unrelated setting change tracked as before", () => {
    useAppStore.getState().updateSettings({ theme: "dark" });
    expect(trackEventMock).toHaveBeenCalledWith("setting_changed", {
      setting_name: "theme",
      new_value: "dark",
    });
  });
});
