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
import { DictationTab } from "./DictationTab";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

const TEST_SLOT = {
  id: "test-slot-1",
  name: "Test Dictation",
  enabled: true,
  profileId: null,
  prompt: "",
  outputAction: "paste" as const,
};

function setupDictationEnabled() {
  useAppStore.setState({
    settings: {
      ...useAppStore.getState().settings,
      shortcutBindings: {},
      dictation: { enabled: true, activationMode: "hold", slots: [TEST_SLOT] },
    },
    updateSettings: vi.fn(),
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  shortcutCaptureActive.current = false;
});

afterEach(() => {
  cleanup();
  shortcutCaptureActive.current = false;
});

describe("DictationTab", () => {
  it("shows slot card when dictation is enabled", () => {
    setupDictationEnabled();
    render(<DictationTab />);
    expect(screen.getByDisplayValue("Test Dictation")).toBeInTheDocument();
  });

  it("enters recording mode when keybind button is clicked", async () => {
    setupDictationEnabled();
    render(<DictationTab />);

    // Find the keybind button — shows "Not set" since no binding
    const keybindBtn = screen.getByText("Not set");
    await userEvent.click(keybindBtn);

    expect(screen.getByText("Press keys...")).toBeInTheDocument();
    expect(shortcutCaptureActive.current).toBe(true);
  });

  it("captures a global key combo on key release", async () => {
    setupDictationEnabled();
    const updateSettings = vi.fn();
    useAppStore.setState({ updateSettings });

    render(<DictationTab />);

    const keybindBtn = screen.getByText("Not set");
    await userEvent.click(keybindBtn);

    // Fire a key combo
    fireEvent.keyDown(window, {
      key: "n",
      code: "KeyN",
      metaKey: true,
      bubbles: true,
    });

    // Should show preview but NOT save yet (keys still held)
    expect(updateSettings).not.toHaveBeenCalled();

    // Release all keys → auto-save
    fireEvent.keyUp(window, {
      key: "Meta",
      code: "MetaLeft",
      metaKey: false,
      bubbles: true,
    });

    // Should have saved the binding
    expect(updateSettings).toHaveBeenCalled();
    const call = updateSettings.mock.calls[0][0];
    expect(call.shortcutBindings[`global.dictation-${TEST_SLOT.id}`]).toBe(
      "CommandOrControl+N",
    );
    // Recording mode should end
    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
  });

  it("cancels on Escape", async () => {
    setupDictationEnabled();
    render(<DictationTab />);

    const keybindBtn = screen.getByText("Not set");
    await userEvent.click(keybindBtn);
    expect(screen.getByText("Press keys...")).toBeInTheDocument();

    fireEvent.keyDown(window, { key: "Escape", code: "Escape", bubbles: true });

    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
    expect(shortcutCaptureActive.current).toBe(false);
  });

  it("cancels on click outside", async () => {
    setupDictationEnabled();
    render(<DictationTab />);

    const keybindBtn = screen.getByText("Not set");
    await userEvent.click(keybindBtn);
    expect(screen.getByText("Press keys...")).toBeInTheDocument();

    fireEvent.mouseDown(document.body);

    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
    expect(shortcutCaptureActive.current).toBe(false);
  });

  it("cancels on window blur when no combo pressed", async () => {
    setupDictationEnabled();
    render(<DictationTab />);

    const keybindBtn = screen.getByText("Not set");
    await userEvent.click(keybindBtn);
    expect(screen.getByText("Press keys...")).toBeInTheDocument();

    fireEvent.blur(window);

    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
    expect(shortcutCaptureActive.current).toBe(false);
  });

  it("saves pending combo on window blur", async () => {
    setupDictationEnabled();
    const updateSettings = vi.fn();
    useAppStore.setState({ updateSettings });

    render(<DictationTab />);

    const keybindBtn = screen.getByText("Not set");
    await userEvent.click(keybindBtn);

    // Press a combo (but don't release)
    fireEvent.keyDown(window, {
      key: "n",
      code: "KeyN",
      metaKey: true,
      bubbles: true,
    });
    expect(updateSettings).not.toHaveBeenCalled();

    // Window blurs before keys are released
    fireEvent.blur(window);

    expect(updateSettings).toHaveBeenCalled();
    expect(screen.queryByText("Press keys...")).not.toBeInTheDocument();
  });

  it("cleans up listener on unmount (no leak)", async () => {
    setupDictationEnabled();
    const updateSettings = vi.fn();
    useAppStore.setState({ updateSettings });

    const { unmount } = render(<DictationTab />);

    const keybindBtn = screen.getByText("Not set");
    await userEvent.click(keybindBtn);
    expect(shortcutCaptureActive.current).toBe(true);

    // Unmount while recording
    unmount();

    expect(shortcutCaptureActive.current).toBe(false);

    // Fire a keydown — should not trigger any callback
    fireEvent.keyDown(window, {
      key: "n",
      code: "KeyN",
      metaKey: true,
      bubbles: true,
    });
    expect(updateSettings).not.toHaveBeenCalled();
  });

  it("ignores repeated keydown events", async () => {
    setupDictationEnabled();
    const updateSettings = vi.fn();
    useAppStore.setState({ updateSettings });

    render(<DictationTab />);

    const keybindBtn = screen.getByText("Not set");
    await userEvent.click(keybindBtn);

    fireEvent.keyDown(window, {
      key: "n",
      code: "KeyN",
      metaKey: true,
      repeat: true,
      bubbles: true,
    });

    // Should still be in recording mode
    expect(screen.getByText("Press keys...")).toBeInTheDocument();
    expect(updateSettings).not.toHaveBeenCalled();
  });
});
