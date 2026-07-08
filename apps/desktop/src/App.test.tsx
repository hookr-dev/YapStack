import { act, render, screen } from "@testing-library/react";
import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
  tauriGlobalShortcutMock,
  tauriPathMock,
  tauriOpenerMock,
  tauriDialogMock,
} from "@/test/tauri-mocks";
import { setupMatchMedia } from "@/test/match-media";
import App from "./App";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@tauri-apps/api/path", () => tauriPathMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@tauri-apps/plugin-global-shortcut", () => tauriGlobalShortcutMock());
vi.mock("@tauri-apps/plugin-opener", () => tauriOpenerMock());
vi.mock("@tauri-apps/plugin-dialog", () => tauriDialogMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

beforeEach(() => {
  vi.clearAllMocks();
  setupMatchMedia();
});

/**
 * `<App />` mounts a tree of effects (autoSetup, hydrate from localStorage,
 * Radix tooltip/scroll registrations, ...) that schedule state updates after
 * the synchronous render returns. Each test renders inside `act` and awaits a
 * microtask flush so those updates land before the assertion runs — without
 * this the render counts as "outside act" and vitest spams the suite output
 * with React `act(...)` warnings.
 */
async function renderApp() {
  await act(async () => {
    render(<App />);
  });
}

describe("App", () => {
  it("renders the sidebar navigation", async () => {
    await renderApp();
    expect(screen.getAllByText("All Sessions").length).toBeGreaterThan(0);
  });

  it("renders the settings button", async () => {
    await renderApp();
    expect(screen.getByLabelText("Settings")).toBeInTheDocument();
  });

  it("shows waiting message when engine is not ready", async () => {
    await renderApp();
    expect(
      screen.getByText(
        "Waiting for engine and audio capture to be ready...",
      ),
    ).toBeInTheDocument();
  });
});
