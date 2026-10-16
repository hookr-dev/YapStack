import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
import { useAppStore } from "@/stores/appStore";
import { RecordingBeacon } from "./RecordingBeacon";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

beforeEach(() => {
  vi.clearAllMocks();
});

describe("RecordingBeacon", () => {
  it("renders nothing when no active session", () => {
    useAppStore.setState({ activeSessionId: null });
    const { container } = render(<RecordingBeacon />);
    expect(container.firstChild).toBeNull();
  });

  it("renders Recording text when session is active", () => {
    useAppStore.setState({
      activeSessionId: "session-1",
      activeSessionStartTime: Date.now(),
    });
    render(<RecordingBeacon />);
    expect(screen.getByText("Recording")).toBeInTheDocument();
  });

  it("calls openSession on click", async () => {
    const openSession = vi.fn();
    useAppStore.setState({
      activeSessionId: "session-1",
      activeSessionStartTime: Date.now(),
      openSession,
    });
    render(<RecordingBeacon />);
    await userEvent.click(screen.getByText("Recording"));
    expect(openSession).toHaveBeenCalledWith("session-1");
  });

  it("displays elapsed time", () => {
    useAppStore.setState({
      activeSessionId: "session-1",
      activeSessionStartTime: Date.now() - 150000, // 2.5 minutes ago
    });
    render(<RecordingBeacon />);
    expect(screen.getByText("02:30")).toBeInTheDocument();
  });
});
