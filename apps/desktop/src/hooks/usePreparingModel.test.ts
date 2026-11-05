import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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
import { usePreparingModel } from "./usePreparingModel";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

function startLoad(variant: "TdtV3CoreMl" | "TdtV3Int8") {
  useAppStore.setState({
    enginePhase: "initializing",
    initializingParakeet: { variant, startedAt: Date.now() },
  });
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  useAppStore.setState({ enginePhase: "idle", initializingParakeet: null });
});

describe("usePreparingModel", () => {
  it("turns on once a CoreML load has run for 3 s", () => {
    startLoad("TdtV3CoreMl");
    const { result } = renderHook(() => usePreparingModel());

    act(() => vi.advanceTimersByTime(2_999));
    expect(result.current).toBe(false);
    act(() => vi.advanceTimersByTime(1));
    expect(result.current).toBe(true);
  });

  it("is on straight away in a view opened after the load passed 3 s", () => {
    startLoad("TdtV3CoreMl");
    act(() => vi.advanceTimersByTime(5_000));

    const { result } = renderHook(() => usePreparingModel());
    expect(result.current).toBe(true);
  });

  it("turns off when the load finishes, and restarts the wait on the next load", () => {
    startLoad("TdtV3CoreMl");
    const { result } = renderHook(() => usePreparingModel());
    act(() => vi.advanceTimersByTime(3_000));
    expect(result.current).toBe(true);

    act(() => useAppStore.setState({ enginePhase: "error" }));
    expect(result.current).toBe(false);

    act(() => startLoad("TdtV3CoreMl"));
    expect(result.current).toBe(false);
    act(() => vi.advanceTimersByTime(3_000));
    expect(result.current).toBe(true);
  });

  it("stays off for the int8 model, whose load has nothing to compile", () => {
    startLoad("TdtV3Int8");
    const { result } = renderHook(() => usePreparingModel());

    act(() => vi.advanceTimersByTime(60_000));
    expect(result.current).toBe(false);
  });
});
