import { describe, it, expect, vi, beforeEach } from "vitest";

// Capturing db-backend mock: record every execute(query, values) so we can assert
// the recording_device_id attribution reaches the SQL (LIVE_SESSION_STATE D2).
const { executeMock } = vi.hoisted(() => ({
  executeMock: vi.fn().mockResolvedValue({ rowsAffected: 1, lastInsertId: 0 }),
}));

vi.mock("@/lib/db-backend", () => ({
  default: {
    load: vi.fn().mockResolvedValue({
      execute: executeMock,
      select: vi.fn().mockResolvedValue([]),
    }),
  },
}));

import { createSession, markSessionRecording } from "@/lib/db";

function lastCall() {
  return executeMock.mock.calls[executeMock.mock.calls.length - 1];
}

describe("createSession attribution (LIVE_SESSION_STATE D2)", () => {
  beforeEach(() => executeMock.mockClear());

  it("stamps the device fingerprint into recording_device_id", async () => {
    await createSession("s1", "MicOnly", "AAAABBBBCCCCDDDD");
    const [query, values] = lastCall();
    expect(query).toContain("recording_device_id");
    expect(values).toEqual(["s1", "MicOnly", "AAAABBBBCCCCDDDD"]);
  });

  it("writes NULL when sync is not configured (single-device semantics)", async () => {
    await createSession("s2", "MicOnly");
    const [, values] = lastCall();
    expect(values).toEqual(["s2", "MicOnly", null]);
  });
});

describe("markSessionRecording re-attribution (LIVE_SESSION_STATE D2)", () => {
  beforeEach(() => executeMock.mockClear());

  it("re-stamps the resuming device's fingerprint", async () => {
    await markSessionRecording("s3", "EEEEFFFFGGGGHHHH");
    const [query, values] = lastCall();
    expect(query).toContain("recording_device_id = $2");
    expect(values).toEqual(["s3", "EEEEFFFFGGGGHHHH"]);
  });

  it("writes NULL when sync is not configured", async () => {
    await markSessionRecording("s4");
    const [, values] = lastCall();
    expect(values).toEqual(["s4", null]);
  });
});
