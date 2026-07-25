import { renderHook, waitFor } from "@testing-library/react";
import { describe, it, expect, afterEach, beforeEach, vi } from "vitest";
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
import { toast } from "sonner";
import { useDictationEntry } from "./useDictationEntry";

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

const prepareCalls = () =>
  vi
    .mocked(invoke)
    .mock.calls.filter(([cmd]) => cmd === "audio_prepare_part")
    .map(([, args]) => (args as Record<string, unknown>)?.partId);

beforeEach(() => vi.clearAllMocks());
afterEach(() => vi.useRealTimers());

describe("useDictationEntry auto-fetch (S3.5 mount trigger)", () => {
  it("starts fetching on MOUNT when the WAV is not on this device (no play click)", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_prepare_part") {
        return { state: "ready", path: "/cache/dict-1.wav" };
      }
      return null;
    });

    const { result } = renderHook(() => useDictationEntry(makeEntry()));
    await waitFor(() => expect(prepareCalls()).toContain("dict-1"));
    // Prefetch only: the cached file must NOT auto-play.
    expect(result.current.playing).toBe(false);
    // And the quiet path never toasts.
    expect(vi.mocked(toast.error)).not.toHaveBeenCalled();
  });

  it("does NOT fetch on mount when the WAV resolves locally", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([true]);
    renderHook(() => useDictationEntry(makeEntry()));
    await waitFor(() =>
      expect(commands.audioFilesExist).toHaveBeenCalledWith([
        "/peer/dictation/dict-1.wav",
      ]),
    );
    expect(prepareCalls()).toHaveLength(0);
  });

  it("does NOT fetch on mount for an entry with no recorded WAV", async () => {
    renderHook(() => useDictationEntry(makeEntry({ wav_file_path: null })));
    // Nothing to probe, nothing to fetch.
    await new Promise((r) => setTimeout(r, 0));
    expect(commands.audioFilesExist).not.toHaveBeenCalled();
    expect(prepareCalls()).toHaveLength(0);
  });

  it("quiet auto-fetch surfaces a fetch error on state, without a toast", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_prepare_part") {
        return { state: "error", message: "boom" };
      }
      return null;
    });
    const { result } = renderHook(() => useDictationEntry(makeEntry()));
    await waitFor(() =>
      expect(result.current.fetchState).toEqual({ kind: "error", message: "boom" }),
    );
    expect(vi.mocked(toast.error)).not.toHaveBeenCalled();
  });

  it("quiet not-on-server re-probes on the 30s cadence (parity with sessions)", async () => {
    vi.useFakeTimers();
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_prepare_part") return { state: "not_on_server" };
      return null;
    });
    const { unmount } = renderHook(() => useDictationEntry(makeEntry()));
    // Flush the mount probe + first prepare (microtasks under fake timers).
    await vi.advanceTimersByTimeAsync(0);
    expect(prepareCalls()).toHaveLength(1);
    // Dictation prefetch stays in the NORMAL class (sessions outrank it).
    expect(vi.mocked(invoke)).toHaveBeenCalledWith("audio_prepare_part", {
      partId: "dict-1",
      highPriority: false,
    });
    // Still waiting just before the cadence…
    await vi.advanceTimersByTimeAsync(29_000);
    expect(prepareCalls()).toHaveLength(1);
    // …and a real re-attempt lands at 30s (quiet: no toast either way).
    await vi.advanceTimersByTimeAsync(1_500);
    expect(prepareCalls()).toHaveLength(2);
    expect(vi.mocked(toast.error)).not.toHaveBeenCalled();
    unmount();
  });

  it("unmount releases a still-queued fetch (cancel-if-queued, leave-in-flight)", async () => {
    vi.mocked(commands.audioFilesExist).mockResolvedValue([false]);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "audio_prepare_part") return { state: "queued" };
      return false;
    });
    const { unmount } = renderHook(() => useDictationEntry(makeEntry()));
    await waitFor(() => expect(prepareCalls()).toContain("dict-1"));
    unmount();
    await waitFor(() =>
      expect(vi.mocked(invoke)).toHaveBeenCalledWith("audio_release_part", {
        partId: "dict-1",
      }),
    );
  });
});
