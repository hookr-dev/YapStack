import { render, screen } from "@testing-library/react";
import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import { setupMatchMedia } from "@/test/match-media";
import { useAppStore } from "@/stores/appStore";
import type { OnboardingState } from "@/stores/appStore";
import { getActiveFlow } from "./onboarding-utils";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

beforeEach(() => {
  vi.clearAllMocks();
  setupMatchMedia();
});

// --- Flow registry ---

describe("getActiveFlow", () => {
  it("returns the initial flow when completedFlows is empty", () => {
    const state: OnboardingState = { completedFlows: {} };
    const flow = getActiveFlow(state);
    expect(flow).not.toBeNull();
    expect(flow!.id).toBe("initial");
  });

  it("returns null when initial flow is completed", () => {
    const state: OnboardingState = {
      completedFlows: { initial: "2025-01-01T00:00:00.000Z" },
    };
    expect(getActiveFlow(state)).toBeNull();
  });

  it("returns null when state is undefined (pre-hydration)", () => {
    expect(getActiveFlow(undefined)).toBeNull();
  });
});

// --- Replay behavior ---

describe("Replay", () => {
  it("deleting only the initial key re-enables the initial flow", () => {
    const state: OnboardingState = {
      completedFlows: {
        initial: "2025-01-01T00:00:00.000Z",
        feature_x: "2025-02-01T00:00:00.000Z",
      },
    };
    // Simulate replay: delete only the initial key
    const { initial: _, ...rest } = state.completedFlows;
    const newState: OnboardingState = { completedFlows: rest };

    const flow = getActiveFlow(newState);
    expect(flow).not.toBeNull();
    expect(flow!.id).toBe("initial");
  });

  it("preserves other flow keys after replay", () => {
    const state: OnboardingState = {
      completedFlows: {
        initial: "2025-01-01T00:00:00.000Z",
        feature_x: "2025-02-01T00:00:00.000Z",
      },
    };
    const { initial: _, ...rest } = state.completedFlows;
    expect(rest).toEqual({ feature_x: "2025-02-01T00:00:00.000Z" });
  });
});

// --- Migration v17→v18 ---

describe("Migration v17→v18", () => {
  function migrateV17toV18(old: Record<string, unknown>) {
    old.onboarding = {
      completedFlows: old.onboardingCompleted
        ? { initial: new Date().toISOString() }
        : {},
    };
    delete old.onboardingCompleted;
    return old;
  }

  it("converts onboardingCompleted: true to completedFlows with initial", () => {
    const old: Record<string, unknown> = { onboardingCompleted: true };
    const result = migrateV17toV18(old);
    const onboarding = result.onboarding as OnboardingState;
    expect(onboarding.completedFlows).toHaveProperty("initial");
    expect(typeof onboarding.completedFlows.initial).toBe("string");
  });

  it("converts onboardingCompleted: false to empty completedFlows", () => {
    const old: Record<string, unknown> = { onboardingCompleted: false };
    const result = migrateV17toV18(old);
    const onboarding = result.onboarding as OnboardingState;
    expect(onboarding.completedFlows).toEqual({});
  });

  it("deletes the onboardingCompleted key", () => {
    const old: Record<string, unknown> = { onboardingCompleted: true };
    const result = migrateV17toV18(old);
    expect(result).not.toHaveProperty("onboardingCompleted");
  });
});

// --- Accessibility ---

describe("OnboardingModal accessibility", () => {
  it("has an accessible name via sr-only title", async () => {
    // Render with incomplete onboarding so the modal appears
    useAppStore.setState({
      settings: {
        ...useAppStore.getState().settings,
        onboarding: { completedFlows: {} },
      },
    });
    // Dynamic import to avoid hoisting issues with the full component
    const { OnboardingModal } = await import("./OnboardingModal");
    render(<OnboardingModal />);
    expect(screen.getByText("Setup")).toHaveClass("sr-only");
  });
});
