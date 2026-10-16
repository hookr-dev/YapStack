import { render, screen, waitFor, cleanup } from "@testing-library/react";
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";
import type { DbDictationHistory } from "@/lib/db";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => ({
  toast: { info: vi.fn(), error: vi.fn(), warning: vi.fn(), success: vi.fn() },
}));

import { commands } from "@/lib/tauri";
import { invoke } from "@tauri-apps/api/core";
import { DictationFeedEntry } from "./DictationFeedEntry";

function makeEntry(over: Partial<DbDictationHistory> = {}): DbDictationHistory {
  return {
    id: "dict-1",
    slot_id: "slot-1",
    slot_name: "Notes",
    input_text: "raw",
    output_text: "polished",
    ai_enabled: 0,
    ai_prompt: null,
    output_action: "clipboard",
    wav_file_path: "/peer/dictation/dict-1.wav",
    wav_duration_seconds: 3,
    session_id: null,
    created_at: "2026-07-15 12:00:00",
    ...over,
  };
}

beforeEach(() => vi.clearAllMocks());
afterEach(() => cleanup());

/**
 * The hook has exported `fetchState` since S3; until now no surface rendered it, so a
 * dictation row whose audio lives on another device offered an ordinary Play button that
 * did nothing observable for the length of the download.
 */
describe("DictationFeedEntry — sync-fetch state on the play control", () => {
  it("shows a normal, enabled Play control when the audio is on this device", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([true]);
    render(<DictationFeedEntry entry={makeEntry()} />);
    await waitFor(() => expect(commands.audioFilesExist).toHaveBeenCalled());
    const play = screen.getByRole("button", { name: "Play audio" });
    expect(play).toBeEnabled();
  });

  it("replaces the glyph with a disabled spinner while a fetch is running", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_prepare_part") {
        return { state: "fetching", received: 42, total: 100 };
      }
      return null;
    });
    const { container } = render(<DictationFeedEntry entry={makeEntry()} />);

    // The honest detail the glyph cannot show lives on the accessible name.
    const busy = await screen.findByRole("button", { name: "Fetching… 42%" });
    expect(busy).toBeDisabled();
    expect(container.querySelector(".animate-spin")).not.toBeNull();
    expect(screen.queryByRole("button", { name: "Play audio" })).toBeNull();
  });

  it("shows a waiting spinner while the fetch queues behind the global cap", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_prepare_part") return { state: "queued" };
      return null;
    });
    render(<DictationFeedEntry entry={makeEntry()} />);
    expect(
      await screen.findByRole("button", { name: "Waiting to fetch…" }),
    ).toBeDisabled();
  });

  it("keeps a terminal's control LIVE, carrying the reason, never silently dead", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_prepare_part") {
        return { state: "error", message: "relay refused the blob" };
      }
      return null;
    });
    render(<DictationFeedEntry entry={makeEntry()} />);
    // Still "Play audio" (clicking re-runs the fetch loudly and toasts the reason),
    // but the reason is on the control and the tone is a warning.
    const play = await screen.findByTitle("relay refused the blob");
    expect(play).toBeEnabled();
    expect(play.className).toContain("amber");
  });

  it("does not spin for a quiet not-on-server re-probe (it is a wait, not a fetch)", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_prepare_part") return { state: "not_on_server" };
      return null;
    });
    const { container } = render(<DictationFeedEntry entry={makeEntry()} />);
    await waitFor(() =>
      expect(
        vi.mocked(invoke).mock.calls.some(([c]) => c === "audio_prepare_part"),
      ).toBe(true),
    );
    expect(screen.getByRole("button", { name: "Play audio" })).toBeEnabled();
    expect(container.querySelector(".animate-spin")).toBeNull();
  });
});
