import { render, screen, cleanup, waitFor, act } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

import type { LogEntry } from "@/lib/logs";

// Mock the logs lib so we control what the backend "returns" and can assert
// the exact limits requested. The numeric constants stay real (re-exported
// from the actual module) so the test asserts against the same source of
// truth the component uses.
const { getRecentLogs, subscribeLogs, clearLogs } = vi.hoisted(() => ({
  getRecentLogs: vi.fn(),
  subscribeLogs: vi.fn(),
  clearLogs: vi.fn(),
}));

vi.mock("@/lib/logs", async () => {
  const actual = await vi.importActual<typeof import("@/lib/logs")>("@/lib/logs");
  return {
    ...actual,
    getRecentLogs,
    subscribeLogs,
    clearLogs,
  };
});

// captureDiagnostics / logger pull in tauri internals we don't exercise here.
vi.mock("@/lib/logger", () => ({ captureDiagnostics: vi.fn() }));

import {
  INITIAL_LOG_FETCH,
  LOG_BUFFER_CAPACITY,
  LOG_FETCH_STEP,
} from "@/lib/logs";
import { LogsPanel } from "./LogsPanel";

function makeEntries(n: number): LogEntry[] {
  return Array.from({ length: n }, (_, i) => ({
    ts_ms: 1_700_000_000_000 + i,
    level: "INFO" as const,
    target: "yapstack_test",
    message: `entry ${i}`,
  }));
}

beforeEach(() => {
  vi.clearAllMocks();
  // Default: subscription is a no-op unsubscribe.
  subscribeLogs.mockResolvedValue(() => {});
  clearLogs.mockResolvedValue(undefined);
});

afterEach(() => {
  cleanup();
});

describe("LogsPanel — history loading", () => {
  it("requests the raised initial cap on mount (not the old 200)", async () => {
    getRecentLogs.mockResolvedValue(makeEntries(INITIAL_LOG_FETCH));

    render(<LogsPanel />);

    await waitFor(() => {
      expect(getRecentLogs).toHaveBeenCalledWith(INITIAL_LOG_FETCH);
    });
    // Sanity: we actually raised it past the previous hard-coded 200.
    expect(INITIAL_LOG_FETCH).toBeGreaterThan(200);
    expect(getRecentLogs).not.toHaveBeenCalledWith(200);
  });

  it("shows 'Load older entries' when the initial snapshot is full", async () => {
    getRecentLogs.mockResolvedValue(makeEntries(INITIAL_LOG_FETCH));

    render(<LogsPanel />);

    expect(
      await screen.findByRole("button", { name: /load older entries/i }),
    ).toBeInTheDocument();
  });

  it("increases the requested limit and re-fetches when 'Load older' is clicked", async () => {
    // First call (mount) returns a full window; second call (load older)
    // returns the larger window.
    getRecentLogs
      .mockResolvedValueOnce(makeEntries(INITIAL_LOG_FETCH))
      .mockResolvedValueOnce(makeEntries(INITIAL_LOG_FETCH + LOG_FETCH_STEP));

    render(<LogsPanel />);

    const button = await screen.findByRole("button", {
      name: /load older entries/i,
    });
    await userEvent.click(button);

    await waitFor(() => {
      expect(getRecentLogs).toHaveBeenCalledWith(
        INITIAL_LOG_FETCH + LOG_FETCH_STEP,
      );
    });
  });

  it("hides 'Load older' once a re-fetch returns fewer entries than requested", async () => {
    getRecentLogs
      .mockResolvedValueOnce(makeEntries(INITIAL_LOG_FETCH))
      // End of history: backend has fewer entries than the new limit.
      .mockResolvedValueOnce(makeEntries(INITIAL_LOG_FETCH + 10));

    render(<LogsPanel />);

    const button = await screen.findByRole("button", {
      name: /load older entries/i,
    });
    await userEvent.click(button);

    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: /load older entries/i }),
      ).not.toBeInTheDocument();
    });
  });

  it("hides 'Load older' immediately when the initial snapshot is short (whole buffer fits)", async () => {
    getRecentLogs.mockResolvedValue(makeEntries(INITIAL_LOG_FETCH - 50));

    render(<LogsPanel />);

    // Wait for the snapshot to render, then assert no affordance.
    await screen.findByText(/entries/);
    expect(
      screen.queryByRole("button", { name: /load older entries/i }),
    ).not.toBeInTheDocument();
  });

  it("caps the requested limit at the buffer ceiling and then hides 'Load older'", async () => {
    // The backend always has at least `LOG_BUFFER_CAPACITY` entries, so every
    // step returns a full window — the only thing that stops paging is the
    // ceiling, never a short snapshot.
    getRecentLogs.mockImplementation(async (limit: number) =>
      makeEntries(Math.min(limit, LOG_BUFFER_CAPACITY)),
    );

    render(<LogsPanel />);

    // Wait for the initial snapshot to resolve and reveal the affordance.
    await screen.findByRole("button", { name: /load older entries/i });

    // Page until the affordance disappears. Each click is followed by a
    // `waitFor` so the async state update flushes before the next query.
    for (let i = 0; i < 10; i++) {
      const button = screen.queryByRole("button", {
        name: /load older entries/i,
      });
      if (!button) break;
      const before = getRecentLogs.mock.calls.length;
      await userEvent.click(button);
      await waitFor(() => {
        expect(getRecentLogs.mock.calls.length).toBeGreaterThan(before);
      });
    }

    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: /load older entries/i }),
      ).not.toBeInTheDocument();
    });
    // The largest limit we ever asked for is the buffer ceiling, never above.
    const maxRequested = Math.max(
      ...getRecentLogs.mock.calls.map((c) => c[0] as number),
    );
    expect(maxRequested).toBe(LOG_BUFFER_CAPACITY);
  });

  it("keeps tail behavior: a live entry after load-older still appends", async () => {
    let emit: ((e: LogEntry) => void) | undefined;
    subscribeLogs.mockImplementation(async (cb: (e: LogEntry) => void) => {
      emit = cb;
      return () => {};
    });
    getRecentLogs
      .mockResolvedValueOnce(makeEntries(INITIAL_LOG_FETCH))
      .mockResolvedValueOnce(makeEntries(INITIAL_LOG_FETCH + LOG_FETCH_STEP));

    render(<LogsPanel />);

    const button = await screen.findByRole("button", {
      name: /load older entries/i,
    });
    await userEvent.click(button);
    await waitFor(() =>
      expect(getRecentLogs).toHaveBeenCalledWith(
        INITIAL_LOG_FETCH + LOG_FETCH_STEP,
      ),
    );

    // A new live entry arrives after loading older history; it must not be
    // dropped (the live cap grew with the loaded window) and the panel keeps
    // tailing it onto the end.
    expect(emit).toBeDefined();
    act(() => {
      emit?.({
        ts_ms: 1_800_000_000_000,
        level: "WARN",
        target: "yapstack_live",
        message: "a fresh live line",
      });
    });

    expect(await screen.findByText("a fresh live line")).toBeInTheDocument();
  });
});
