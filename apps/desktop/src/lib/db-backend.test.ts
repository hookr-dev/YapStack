import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// Mock the Tauri core `invoke` so we can drive the DB_SWAP_IN_PROGRESS retry (F6).
// `vi.hoisted` makes the spy available inside the hoisted `vi.mock` factory.
const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

import dbBackend from "./db-backend";

describe("db-backend DB_SWAP_IN_PROGRESS retry (F6)", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("retries while the swap sentinel is returned, then resolves", async () => {
    invokeMock
      .mockRejectedValueOnce("DB_SWAP_IN_PROGRESS: briefly unavailable; retry shortly")
      .mockRejectedValueOnce("DB_SWAP_IN_PROGRESS: briefly unavailable; retry shortly")
      .mockResolvedValueOnce([{ id: "s1" }]);

    const conn = await dbBackend.load("sqlite:yapstack.db");
    const pending = conn.select<Array<{ id: string }>>("SELECT id FROM sessions");

    // Two backoff windows elapse before the third attempt succeeds.
    await vi.advanceTimersByTimeAsync(250);
    await vi.advanceTimersByTimeAsync(250);

    await expect(pending).resolves.toEqual([{ id: "s1" }]);
    expect(invokeMock).toHaveBeenCalledTimes(3);
  });

  it("rejects a non-swap error immediately without retrying", async () => {
    invokeMock.mockRejectedValueOnce("UNIQUE constraint failed");

    const conn = await dbBackend.load("sqlite:yapstack.db");
    await expect(conn.execute("INSERT INTO sessions (id) VALUES ($1)", ["x"])).rejects.toBe(
      "UNIQUE constraint failed",
    );
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("gives up after the retry cap and surfaces the swap error", async () => {
    invokeMock.mockRejectedValue("DB_SWAP_IN_PROGRESS: still swapping");

    const conn = await dbBackend.load("sqlite:yapstack.db");
    const pending = conn.select("SELECT 1");
    const assertion = expect(pending).rejects.toContain("DB_SWAP_IN_PROGRESS");

    // Initial attempt + 10 retries, each separated by a 250ms backoff.
    await vi.advanceTimersByTimeAsync(250 * 11);
    await assertion;
    expect(invokeMock).toHaveBeenCalledTimes(11);
  });
});
