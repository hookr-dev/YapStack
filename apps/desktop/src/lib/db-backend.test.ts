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

describe('db-backend "database is locked" retry (R6 item 1b)', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("retries transient write-lock contention with the drain, then resolves", async () => {
    invokeMock
      .mockRejectedValueOnce("database is locked")
      .mockRejectedValueOnce(new Error("database is locked"))
      .mockResolvedValueOnce([{ id: "s1" }]);

    const conn = await dbBackend.load("sqlite:yapstack.db");
    const pending = conn.select<Array<{ id: string }>>("SELECT id FROM sessions");

    // Capped backoff windows: 60ms then 120ms before the third attempt lands.
    await vi.advanceTimersByTimeAsync(60);
    await vi.advanceTimersByTimeAsync(120);

    await expect(pending).resolves.toEqual([{ id: "s1" }]);
    expect(invokeMock).toHaveBeenCalledTimes(3);
  });

  it("does NOT retry a genuine SQL error that merely mentions a table", async () => {
    invokeMock.mockRejectedValueOnce("no such table: sessions");

    const conn = await dbBackend.load("sqlite:yapstack.db");
    await expect(conn.select("SELECT 1 FROM sessions")).rejects.toBe(
      "no such table: sessions",
    );
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("gives up after the lock retry cap and surfaces the lock error", async () => {
    invokeMock.mockRejectedValue("database is locked");

    const conn = await dbBackend.load("sqlite:yapstack.db");
    const pending = conn.execute("UPDATE sessions SET title='x'");
    const assertion = expect(pending).rejects.toContain("database is locked");

    // Initial attempt + 8 retries; capped backoff tops out at 500ms, so drain the max.
    await vi.advanceTimersByTimeAsync(500 * 9);
    await assertion;
    expect(invokeMock).toHaveBeenCalledTimes(9);
  });
});
