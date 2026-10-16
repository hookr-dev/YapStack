import { describe, it, expect, vi } from "vitest";
import { tauriSqlMock } from "@/test/tauri-mocks";

vi.mock("@/lib/db-backend", () => tauriSqlMock());

import {
  sessionHeartbeatMs,
  isRecordingStale,
  RECORDING_STALE_THRESHOLD_MINUTES,
  type DbSession,
  type DbSegment,
} from "@/lib/db";

// A SQLite `datetime('now')`-shaped UTC timestamp (no zone suffix) `deltaSec`
// seconds from `nowMs`.
function dbTs(nowMs: number, deltaSec: number): string {
  return new Date(nowMs + deltaSec * 1000)
    .toISOString()
    .replace("T", " ")
    .replace(/\.\d+Z$/, "");
}

function session(created_at: string): Pick<DbSession, "created_at"> {
  return { created_at };
}

function seg(created_at: string): Pick<DbSegment, "created_at"> {
  return { created_at };
}

const NOW = Date.UTC(2026, 6, 15, 12, 0, 0);

describe("sessionHeartbeatMs (LIVE_SESSION_STATE D5)", () => {
  it("uses the max segment created_at when segments exist", () => {
    const segs = [seg(dbTs(NOW, -120)), seg(dbTs(NOW, -30)), seg(dbTs(NOW, -90))];
    // Heartbeat is the newest segment (-30s), NOT the session start.
    const hb = sessionHeartbeatMs(session(dbTs(NOW, -600)), segs);
    expect(Math.abs(hb - (NOW - 30_000))).toBeLessThan(1000);
  });

  it("falls back to session.created_at at zero segments", () => {
    const hb = sessionHeartbeatMs(session(dbTs(NOW, -45)), []);
    expect(Math.abs(hb - (NOW - 45_000))).toBeLessThan(1000);
  });
});

describe("isRecordingStale (LIVE_SESSION_STATE D5, 3-min threshold)", () => {
  it("is fresh when the newest segment is under the threshold", () => {
    const segs = [seg(dbTs(NOW, -60))];
    expect(isRecordingStale(session(dbTs(NOW, -600)), segs, NOW)).toBe(false);
  });

  it("is stale when the heartbeat is older than the threshold", () => {
    const past = -(RECORDING_STALE_THRESHOLD_MINUTES * 60 + 30);
    const segs = [seg(dbTs(NOW, past))];
    expect(isRecordingStale(session(dbTs(NOW, -600)), segs, NOW)).toBe(true);
  });

  it("uses session.created_at at zero segments (empty fresh vs stale)", () => {
    expect(isRecordingStale(session(dbTs(NOW, -30)), [], NOW)).toBe(false);
    expect(
      isRecordingStale(
        session(dbTs(NOW, -(RECORDING_STALE_THRESHOLD_MINUTES * 60 + 5))),
        [],
        NOW,
      ),
    ).toBe(true);
  });

  it("threshold constant mirrors the Rust boot-sweep value", () => {
    expect(RECORDING_STALE_THRESHOLD_MINUTES).toBe(3);
  });
});
