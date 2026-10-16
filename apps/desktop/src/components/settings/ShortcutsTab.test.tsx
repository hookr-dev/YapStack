import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
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
import { shortcutCaptureActive } from "@/lib/shortcuts";
import { ShortcutsTab } from "./ShortcutsTab";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

// In jsdom, navigator.userAgent doesn't contain "Mac", so:
// - In-app bindings display as "Ctrl+K" (not "⌘K")
// - Global bindings display as "Ctrl+Shift+N" (not "⌘⇧N")
// - eventToBinding uses e.ctrlKey as "mod" (not e.metaKey)

/** Click the binding button for the "Search" shortcut (default: mod+k → "Ctrl+K"). */
async function enterRecordingForSearch() {
  const btn = screen.getByRole("button", { name: "Ctrl+K" });
  await userEvent.click(btn);
}

beforeEach(() => {
  vi.clearAllMocks();
  shortcutCaptureActive.current = false;
  useAppStore.setState({
    settings: {
      ...useAppStore.getState().settings,
      shortcutBindings: {},
      dictation: { enabled: false, activationMode: "hold", slots: [] },
    },
    updateSettings: vi.fn(),
  });
});

afterEach(() => {
  cleanup();
  shortcutCaptureActive.current = false;
});

describe("ShortcutsTab", () => {
  it("renders shortcut rows with binding buttons", () => {
    render(<ShortcutsTab />);
    expect(screen.getByText("Search")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Ctrl+K" })).toBeInTheDocument();
  });

  it("enters recording mode when binding button is clicked", async () => {
    render(<ShortcutsTab />);
    await enterRecordingForSearch();

    expect(screen.getByText("Press keys...")).toBeInTheDocument();
    expect(shortcutCaptureActive.current).toBe(true);
  });

  it("captures a key combo and saves the binding on key release", async () => {
    const updateSettings = vi.fn();
    useAppStore.setState({ updateSettings });

    render(<ShortcutsTab />);
    await enterRecordingForSearch();
    expect(screen.getByText("Press keys...")).toBeInTheDocument();

    // In jsdom (non-Mac), eventToBinding uses ctrlKey as "mod"
    fireEvent.keyDown(window, {
      key: "l",
      code: "KeyL",
      ctrlKey: true,
      bubbles: true,
    });

    // Should show preview but NOT save yet (keys still held)
    expect(updateSettings).not.toHaveBeenCalled();
    expect(screen.getByText("Ctrl+L")).toBeInTheDocument();

    // Release all keys → auto-save
    fireEvent.keyUp(window, {
      key: "Control",
      code: "ControlLeft",
      ctrlKey: false,
      bubbles: true,
    });

    expect(updateSettings).toHaveBeenCalled();
    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
  });

  it("cancels on Escape", async () => {
    render(<ShortcutsTab />);
    await enterRecordingForSearch();
    expect(screen.getByText("Press keys...")).toBeInTheDocument();

    fireEvent.keyDown(window, { key: "Escape", code: "Escape", bubbles: true });

    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
    expect(shortcutCaptureActive.current).toBe(false);
  });

  it("cancels on click outside", async () => {
    render(<ShortcutsTab />);
    await enterRecordingForSearch();
    expect(screen.getByText("Press keys...")).toBeInTheDocument();

    fireEvent.mouseDown(document.body);

    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
    expect(shortcutCaptureActive.current).toBe(false);
  });

  it("cancels on window blur when no combo pressed", async () => {
    render(<ShortcutsTab />);
    await enterRecordingForSearch();
    expect(screen.getByText("Press keys...")).toBeInTheDocument();

    fireEvent.blur(window);

    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
    expect(shortcutCaptureActive.current).toBe(false);
  });

  it("saves pending combo on window blur", async () => {
    const updateSettings = vi.fn();
    useAppStore.setState({ updateSettings });

    render(<ShortcutsTab />);
    await enterRecordingForSearch();

    // Press a combo (but don't release)
    fireEvent.keyDown(window, {
      key: "l",
      code: "KeyL",
      ctrlKey: true,
      bubbles: true,
    });
    expect(updateSettings).not.toHaveBeenCalled();

    // Window blurs before keys are released
    fireEvent.blur(window);

    expect(updateSettings).toHaveBeenCalled();
    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
  });

  it("resets shortcutCaptureActive on unmount", async () => {
    const { unmount } = render(<ShortcutsTab />);
    await enterRecordingForSearch();
    expect(shortcutCaptureActive.current).toBe(true);

    unmount();

    expect(shortcutCaptureActive.current).toBe(false);
  });

  it("ignores repeated keydown events", async () => {
    const updateSettings = vi.fn();
    useAppStore.setState({ updateSettings });

    render(<ShortcutsTab />);
    await enterRecordingForSearch();

    fireEvent.keyDown(window, {
      key: "k",
      code: "KeyK",
      ctrlKey: true,
      repeat: true,
      bubbles: true,
    });

    // Should still be in recording mode
    expect(screen.getByText("Press keys...")).toBeInTheDocument();
    expect(updateSettings).not.toHaveBeenCalled();
  });
});
