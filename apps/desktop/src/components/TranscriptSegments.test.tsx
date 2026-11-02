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
import { TranscriptSegments } from "./TranscriptSegments";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { DbSegment } from "@/lib/db";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());

function seg(overrides: Partial<DbSegment>): DbSegment {
  return {
    id: "seg",
    session_id: "s1",
    source: "Mic",
    text: "",
    audio_offset_seconds: 0,
    chunk_duration_seconds: 1,
    confidence: 1,
    created_at: "",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
    speaker_id: null,
    ...overrides,
  };
}

const segments = [
  seg({ id: "a", source: "Mic", speaker_id: 0, text: "Room speaker" }),
  seg({ id: "b", source: "System", speaker_id: 0, audio_offset_seconds: 5, text: "Call speaker" }),
];

function renderTranscript() {
  return render(
    <TooltipProvider>
      <TranscriptSegments
        sessionId="s1"
        segments={segments}
        isEditable={false}
        activeSegmentId={null}
      />
    </TooltipProvider>,
  );
}

beforeEach(() => {
  useAppStore.setState((s) => ({ settings: { ...s.settings, speakerNames: {} } }));
});

describe("TranscriptSegments speaker headers", () => {
  it("renaming mic Speaker 1 does not rename system Speaker 1", async () => {
    renderTranscript();
    const headers = screen.getAllByRole("button", { name: "Speaker 1" });
    expect(headers).toHaveLength(2);

    await userEvent.click(headers[0]);
    const input = screen.getByPlaceholderText("Speaker 1");
    await userEvent.type(input, "Dana{Enter}");

    expect(screen.getByRole("button", { name: "Dana" })).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "Speaker 1" })).toHaveLength(1);
    expect(useAppStore.getState().settings.speakerNames).toEqual({
      s1: { "mic:c0": "Dana" },
    });
  });

  it("splits a legacy bare-id name when one channel is renamed", async () => {
    useAppStore.setState((s) => ({
      settings: { ...s.settings, speakerNames: { s1: { "0": "Alice" } } },
    }));
    renderTranscript();
    const headers = screen.getAllByRole("button", { name: "Alice" });
    expect(headers).toHaveLength(2);

    await userEvent.click(headers[1]);
    const input = screen.getByPlaceholderText("Speaker 1");
    await userEvent.clear(input);
    await userEvent.type(input, "Bob{Enter}");

    expect(screen.getByRole("button", { name: "Alice" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Bob" })).toBeInTheDocument();
    expect(useAppStore.getState().settings.speakerNames).toEqual({
      s1: { "mic:c0": "Alice", "system:c0": "Bob" },
    });
  });
});
