import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  shouldShowUpgrade,
  normalizeCode,
  groupBase32,
  formatRecoveryCode,
  formatFingerprint,
  isValidRecoveryCode,
  isValidServerUrl,
  isCommandNotFound,
  formatSyncProgress,
  formatCatchingUp,
  formatBytes,
  formatLastSynced,
  deriveAudioBackup,
  enqueueAudioForSession,
  enqueueAudioForDictation,
  deriveTrackFetch,
  formatFetchProgress,
  formatTrackFetchLabel,
  formatNoSpace,
  nextPollDelayMs,
  ON_DEVICE_REPROBE_MS,
  type AudioPartPrepare,
} from "./sync";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

describe("shouldShowUpgrade", () => {
  it("shows upgrade only when a billing_url is advertised", () => {
    expect(shouldShowUpgrade({ billingUrl: "https://pay.example" })).toBe(true);
  });
  it("hides upgrade for self-host (no billing_url)", () => {
    expect(shouldShowUpgrade({ billingUrl: null })).toBe(false);
    expect(shouldShowUpgrade({ billingUrl: "" })).toBe(false);
  });
});

describe("normalizeCode", () => {
  it("strips hyphens/whitespace and uppercases", () => {
    expect(normalizeCode(" aaaa-bbbb cccc ")).toBe("AAAABBBBCCCC");
  });
});

describe("groupBase32", () => {
  it("groups into N blocks of 4", () => {
    expect(groupBase32("AAAABBBBCCCCDDDD", 4)).toBe("AAAA-BBBB-CCCC-DDDD");
  });
  it("only takes the first N*4 chars for the primary groups", () => {
    // 8 groups from a 32-char code
    const code = "A".repeat(32);
    expect(formatRecoveryCode(code)).toBe(
      "AAAA-AAAA-AAAA-AAAA-AAAA-AAAA-AAAA-AAAA",
    );
  });
});

describe("formatFingerprint", () => {
  it("renders 4 groups of 4 for a 16-char fingerprint", () => {
    expect(formatFingerprint("ABCD2345EFGH6789")).toBe("ABCD-2345-EFGH-6789");
  });
});

describe("isValidRecoveryCode", () => {
  it("accepts a 32-char base32 code (case/hyphen-insensitive)", () => {
    expect(isValidRecoveryCode("aaaa-bbbb-cccc-dddd-eeee-ffff-gggg-2345")).toBe(
      true,
    );
  });
  it("rejects wrong length or non-base32 chars", () => {
    expect(isValidRecoveryCode("AAAA")).toBe(false);
    // 0, 1, 8, 9 are not in the RFC 4648 base32 alphabet
    expect(isValidRecoveryCode("0".repeat(32))).toBe(false);
  });
  it("R3/R1: gates out a long non-base32 string the old length-only check accepted", () => {
    // The LoginDialog recover button previously gated only on `trim().length < 32`, so a 40-
    // char string of invalid alphabet ("8" ∉ base32) would ENABLE recover and fail server-
    // side. isValidRecoveryCode rejects it (right length AND alphabet), so the button stays
    // disabled — the fix wired into LoginDialog.
    expect("8".repeat(40).length >= 32).toBe(true); // the old gate would have passed this
    expect(isValidRecoveryCode("8".repeat(40))).toBe(false);
    // And a well-formed hyphen/lowercase code (normalized to 32 base32 chars) is accepted.
    expect(isValidRecoveryCode("aaaa-bbbb-cccc-dddd-eeee-ffff-gggg-2345")).toBe(
      true,
    );
  });
});

describe("isValidServerUrl", () => {
  it("accepts http/https origins", () => {
    expect(isValidServerUrl("https://sync.yapstack.app")).toBe(true);
    expect(isValidServerUrl("http://localhost:8080")).toBe(true);
  });
  it("rejects junk and non-http schemes", () => {
    expect(isValidServerUrl("not a url")).toBe(false);
    expect(isValidServerUrl("ftp://x")).toBe(false);
  });
});

describe("formatSyncProgress", () => {
  it("pluralizes and omits size below ~1 MiB", () => {
    expect(formatSyncProgress(1, 500)).toBe("1 item remaining");
    expect(formatSyncProgress(3, 1024)).toBe("3 items remaining");
  });
  it("appends the byte size once it is meaningfully large", () => {
    // 68 MiB across a big initial sync.
    expect(formatSyncProgress(137, 68 * 1024 * 1024)).toBe(
      "137 items remaining · 68.0 MB",
    );
  });
});

describe("formatCatchingUp", () => {
  it("phrases the pull backlog as changesets to go (plural)", () => {
    expect(formatCatchingUp(1650)).toBe("Syncing — catching up (1650 changes to go)");
  });
  it("uses the singular noun for one changeset", () => {
    expect(formatCatchingUp(1)).toBe("Syncing — catching up (1 change to go)");
  });
  it("falls back to plain phrasing for a non-positive count", () => {
    expect(formatCatchingUp(0)).toBe("Syncing — catching up");
    expect(formatCatchingUp(-5)).toBe("Syncing — catching up");
  });
});

describe("isCommandNotFound", () => {
  it("matches Tauri's exact unregistered-command rejection for that command", () => {
    // The literal shape tauri 2.11 rejects with (src/webview/mod.rs) when no
    // invoke handler claims the message: a plain string, not an Error.
    expect(isCommandNotFound("Command sync_status not found", "sync_status")).toBe(true);
    // Tolerates an Error wrapper without loosening the message test.
    expect(
      isCommandNotFound(new Error("Command sync_status not found"), "sync_status"),
    ).toBe(true);
  });

  it("does not match a different command's absence", () => {
    expect(isCommandNotFound("Command sync_probe not found", "sync_status")).toBe(false);
  });

  it("does not match real errors raised by a registered command", () => {
    for (const real of [
      "relay unreachable: connection refused",
      "sync_status failed: keychain entry not found",
      "Command failed: not found",
      "not found",
      "Command sync_status not found (device offline)",
      "",
    ]) {
      expect(isCommandNotFound(real, "sync_status")).toBe(false);
    }
  });

  it("returns false for non-string, non-Error values", () => {
    expect(isCommandNotFound(null, "sync_status")).toBe(false);
    expect(isCommandNotFound(undefined, "sync_status")).toBe(false);
    expect(isCommandNotFound({ message: "Command sync_status not found" }, "sync_status")).toBe(
      false,
    );
  });
});

describe("formatBytes", () => {
  it("renders MB below a GB and GB above", () => {
    expect(formatBytes(0)).toBe("0 MB");
    expect(formatBytes(5 * 1024 * 1024)).toBe("5.0 MB");
    expect(formatBytes(2 * 1024 * 1024 * 1024)).toBe("2.0 GB");
  });
});

describe("formatLastSynced", () => {
  const now = Date.parse("2026-07-07T12:00:00Z");
  it("returns empty string when never synced", () => {
    expect(formatLastSynced(null, now)).toBe("");
    expect(formatLastSynced("not-a-date", now)).toBe("");
  });
  it("phrases sub-minute as just now, then m/h/d", () => {
    expect(formatLastSynced("2026-07-07T11:59:30Z", now)).toBe("just now");
    expect(formatLastSynced("2026-07-07T11:58:00Z", now)).toBe("2m ago");
    expect(formatLastSynced("2026-07-07T09:00:00Z", now)).toBe("3h ago");
    expect(formatLastSynced("2026-07-05T12:00:00Z", now)).toBe("2d ago");
  });
});

describe("deriveAudioBackup", () => {
  const base = {
    audioUploadOutstanding: 0,
    audioBackfillOutstanding: 0,
    audioUploadFailed: 0,
    audioUploadedTotal: 0,
  };

  it("is hidden when nothing is outstanding, failed, or ever uploaded", () => {
    expect(deriveAudioBackup(base).state).toBe("hidden");
  });

  it("failures take precedence and pluralize", () => {
    expect(deriveAudioBackup({ ...base, audioUploadFailed: 1 })).toEqual({
      state: "failed",
      label: "1 recording failed to back up",
    });
    // Failed wins even while uploads are outstanding.
    const d = deriveAudioBackup({
      ...base,
      audioUploadFailed: 3,
      audioUploadOutstanding: 5,
    });
    expect(d.state).toBe("failed");
    expect(d.label).toBe("3 recordings failed to back up");
  });

  it("in-flight uploads show a count, singular/plural", () => {
    expect(deriveAudioBackup({ ...base, audioUploadOutstanding: 1 })).toEqual({
      state: "uploading",
      label: "Backing up 1 recording",
    });
    expect(deriveAudioBackup({ ...base, audioUploadOutstanding: 4 }).label).toBe(
      "Backing up 4 recordings",
    );
  });

  it("distinguishes existing-library backfill", () => {
    // Entirely backfill.
    expect(
      deriveAudioBackup({
        ...base,
        audioUploadOutstanding: 6,
        audioBackfillOutstanding: 6,
      }).label,
    ).toBe("Backing up 6 recordings from your existing library");
    // Mixed new + backfill.
    expect(
      deriveAudioBackup({
        ...base,
        audioUploadOutstanding: 10,
        audioBackfillOutstanding: 4,
      }).label,
    ).toBe("Backing up 10 recordings (4 from your existing library)");
  });

  it("rests on an all-backed-up line once idle", () => {
    expect(deriveAudioBackup({ ...base, audioUploadedTotal: 1 })).toEqual({
      state: "complete",
      label: "1 recording backed up",
    });
    expect(
      deriveAudioBackup({ ...base, audioUploadedTotal: 42 }).label,
    ).toBe("42 recordings backed up");
  });
});

describe("enqueueAudioForSession (fire-and-forget)", () => {
  beforeEach(() => invokeMock.mockReset());

  it("invokes the enqueue command with the session id", () => {
    invokeMock.mockResolvedValue(2);
    enqueueAudioForSession("sess-1");
    expect(invokeMock).toHaveBeenCalledWith("audio_enqueue_session", {
      sessionId: "sess-1",
    });
  });

  it("attaches a catch so a rejection is swallowed (fire-and-forget)", () => {
    // The swallow mechanism is `invoke(...).catch(...)`. Assert the `.catch` is attached
    // (so a no-sync build's "command not found" rejection never surfaces) via a thenable
    // spy — never create a real rejected promise, which would trip the unhandled-rejection
    // watcher regardless of the handler.
    const catchSpy = vi.fn().mockReturnValue(undefined);
    invokeMock.mockReturnValue({ catch: catchSpy });
    expect(() => enqueueAudioForSession("sess-2")).not.toThrow();
    expect(invokeMock).toHaveBeenCalledWith("audio_enqueue_session", {
      sessionId: "sess-2",
    });
    expect(catchSpy).toHaveBeenCalledTimes(1);
  });

  it("is a no-op for an empty session id", () => {
    enqueueAudioForSession("");
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe("deriveTrackFetch (S3 player state machine)", () => {
  const ready = (path = "/c/x.wav"): AudioPartPrepare => ({ state: "ready", path });
  const fetching = (received: number, total: number): AudioPartPrepare => ({
    state: "fetching",
    received,
    total,
  });

  it("is ready when nothing is missing (empty input)", () => {
    expect(deriveTrackFetch([])).toEqual({ kind: "ready" });
  });

  it("is ready when every missing part is now cached (fetch complete)", () => {
    expect(deriveTrackFetch([ready(), ready("/c/y.wav")])).toEqual({ kind: "ready" });
  });

  it("keeps fetching as the headline while another part has no server copy yet", () => {
    // Auto-fetch S3.5: a not-yet-uploaded part is re-probed on the slow cadence while the
    // rest keep downloading — mid-download progress stays the honest headline.
    expect(deriveTrackFetch([fetching(10, 100), { state: "not_on_server" }])).toEqual({
      kind: "fetching",
      percent: 10,
      received: 10,
      total: 100,
      currentPart: 1,
      totalParts: 2,
    });
  });

  it("is on_device when a part has no server copy and nothing is in flight", () => {
    expect(
      deriveTrackFetch([ready(), { state: "not_on_server" }]),
    ).toEqual({ kind: "on_device" });
  });

  it("aggregates progress across in-flight parts into a percent", () => {
    const t = deriveTrackFetch([fetching(50, 100), fetching(50, 300)]);
    expect(t).toEqual({
      kind: "fetching",
      percent: 25,
      received: 100,
      total: 400,
      currentPart: 1,
      totalParts: 2,
    });
  });

  it("reports the FIRST in-flight part's ordinal among the missing parts (the K)", () => {
    const t = deriveTrackFetch([ready(), fetching(5, 10), { state: "queued" }]);
    expect(t).toMatchObject({ kind: "fetching", currentPart: 2, totalParts: 3 });
  });

  it("reports fetching with 0% when the total is not yet known", () => {
    expect(deriveTrackFetch([fetching(0, 0)])).toEqual({
      kind: "fetching",
      percent: 0,
      received: 0,
      total: 0,
      currentPart: 1,
      totalParts: 1,
    });
  });

  it("is queued while parts wait for a download permit and none is in flight", () => {
    expect(
      deriveTrackFetch([{ state: "queued" }, { state: "queued" }]),
    ).toEqual({ kind: "queued" });
    // A queued part never outranks live progress.
    expect(
      deriveTrackFetch([{ state: "queued" }, fetching(1, 2)]),
    ).toMatchObject({ kind: "fetching", currentPart: 2, totalParts: 2 });
  });

  it("surfaces no_space (disk precheck) over fetching/queued with the needed budget", () => {
    expect(
      deriveTrackFetch([
        { state: "no_space", needed: 1_500_000_000 },
        fetching(1, 2),
        { state: "queued" },
      ]),
    ).toEqual({ kind: "no_space", needed: 1_500_000_000 });
  });

  it("surfaces unreachable (transient) over not_on_server", () => {
    expect(
      deriveTrackFetch([{ state: "unreachable" }, { state: "not_on_server" }]),
    ).toEqual({ kind: "unreachable" });
  });

  it("ranks auth_expired below unreachable and above no_space/error/fetching", () => {
    // Connectivity outranks a stale credential…
    expect(
      deriveTrackFetch([{ state: "auth_expired" }, { state: "unreachable" }]),
    ).toEqual({ kind: "unreachable" });
    // …but the expired bearer outranks everything that needs a user action or is in
    // flight (its self-healing copy is the honest headline).
    expect(
      deriveTrackFetch([
        { state: "auth_expired" },
        { state: "no_space", needed: 1 },
        { state: "error", message: "x" },
        fetching(1, 2),
        { state: "queued" },
        { state: "not_on_server" },
      ]),
    ).toEqual({ kind: "auth_expired" });
  });

  it("surfaces verification failure with highest precedence (tamper signal)", () => {
    expect(
      deriveTrackFetch([
        { state: "verification_failed" },
        { state: "unreachable" },
        fetching(1, 2),
      ]),
    ).toEqual({ kind: "verification_failed" });
  });

  it("surfaces a generic error message", () => {
    expect(
      deriveTrackFetch([{ state: "error", message: "disk full" }, fetching(1, 2)]),
    ).toEqual({ kind: "error", message: "disk full" });
  });
});

describe("formatFetchProgress", () => {
  it("shows a percent once progress is known", () => {
    expect(formatFetchProgress(42)).toBe("Fetching 42%");
  });
  it("falls back to an indeterminate label at 0 / unknown", () => {
    expect(formatFetchProgress(0)).toBe("Fetching…");
    expect(formatFetchProgress(NaN)).toBe("Fetching…");
  });
});

describe("formatTrackFetchLabel (aggregate copy)", () => {
  it("single-part → 'Fetching… N%'", () => {
    expect(
      formatTrackFetchLabel({ percent: 42, currentPart: 1, totalParts: 1 }),
    ).toBe("Fetching… 42%");
  });
  it("single-part with unknown percent → 'Fetching…'", () => {
    expect(
      formatTrackFetchLabel({ percent: 0, currentPart: 1, totalParts: 1 }),
    ).toBe("Fetching…");
  });
  it("multi-part → 'Fetching part K of M — N%'", () => {
    expect(
      formatTrackFetchLabel({ percent: 37, currentPart: 2, totalParts: 3 }),
    ).toBe("Fetching part 2 of 3 — 37%");
  });
  it("multi-part with unknown percent omits the suffix", () => {
    expect(
      formatTrackFetchLabel({ percent: 0, currentPart: 1, totalParts: 4 }),
    ).toBe("Fetching part 1 of 4…");
  });
});

describe("formatNoSpace", () => {
  it("renders the honest need-~X copy from the byte budget", () => {
    expect(formatNoSpace(2 * 1024 * 1024 * 1024)).toBe(
      "Not enough disk space (need ~2.0 GB)",
    );
  });
});

describe("nextPollDelayMs (auto-fetch cadence)", () => {
  const fetching = (): ReturnType<typeof deriveTrackFetch> =>
    deriveTrackFetch([{ state: "fetching", received: 1, total: 2 }]);
  it("polls fast while downloading or waiting for a permit", () => {
    expect(nextPollDelayMs(fetching())).toBe(600);
    expect(nextPollDelayMs({ kind: "queued" })).toBe(600);
  });
  it("re-probes on the slow 30s cadence while the server lacks a blob", () => {
    expect(nextPollDelayMs({ kind: "on_device" })).toBe(30_000);
    expect(ON_DEVICE_REPROBE_MS).toBe(30_000);
  });
  it("re-probes on the slow cadence while the bearer is expired (self-healing)", () => {
    expect(nextPollDelayMs({ kind: "auth_expired" })).toBe(30_000);
  });
  it("stops on ready and on the terminals that need a user action", () => {
    expect(nextPollDelayMs({ kind: "ready" })).toBeNull();
    expect(nextPollDelayMs({ kind: "unreachable" })).toBeNull();
    expect(nextPollDelayMs({ kind: "no_space", needed: 1 })).toBeNull();
    expect(nextPollDelayMs({ kind: "verification_failed" })).toBeNull();
    expect(nextPollDelayMs({ kind: "error", message: "x" })).toBeNull();
  });
});

describe("prepareAudioPart / cancelAudioPart / enqueueAudioForDictation IPC", () => {
  beforeEach(() => invokeMock.mockReset());

  it("prepareAudioPart invokes with the part id (normal class) and returns the DTO", async () => {
    const { syncCommands } = await import("./sync");
    invokeMock.mockResolvedValue({ state: "fetching", received: 1, total: 2 });
    const r = await syncCommands.prepareAudioPart("part-9");
    expect(invokeMock).toHaveBeenCalledWith("audio_prepare_part", {
      partId: "part-9",
      highPriority: false,
    });
    expect(r).toEqual({ state: "fetching", received: 1, total: 2 });
  });

  it("prepareAudioPart forwards the high-priority class (session-view promotion)", async () => {
    const { syncCommands } = await import("./sync");
    invokeMock.mockResolvedValue({ state: "queued" });
    await syncCommands.prepareAudioPart("part-9", { highPriority: true });
    expect(invokeMock).toHaveBeenCalledWith("audio_prepare_part", {
      partId: "part-9",
      highPriority: true,
    });
  });

  it("cancelAudioPart invokes with the part id", async () => {
    const { syncCommands } = await import("./sync");
    invokeMock.mockResolvedValue(undefined);
    await syncCommands.cancelAudioPart("part-9");
    expect(invokeMock).toHaveBeenCalledWith("audio_cancel_part", { partId: "part-9" });
  });

  it("releaseAudioPart invokes cancel-if-queued semantics with the part id", async () => {
    const { syncCommands } = await import("./sync");
    invokeMock.mockResolvedValue(true);
    await expect(syncCommands.releaseAudioPart("part-9")).resolves.toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("audio_release_part", { partId: "part-9" });
  });

  it("audioCacheStats / audioCacheClear round-trip the cache footprint", async () => {
    const { syncCommands } = await import("./sync");
    invokeMock.mockResolvedValue({ bytes: 42, files: 2 });
    await expect(syncCommands.audioCacheStats()).resolves.toEqual({
      bytes: 42,
      files: 2,
    });
    expect(invokeMock).toHaveBeenCalledWith("audio_cache_stats");
    invokeMock.mockResolvedValue({ bytes: 0, files: 0 });
    await expect(syncCommands.audioCacheClear()).resolves.toEqual({
      bytes: 0,
      files: 0,
    });
    expect(invokeMock).toHaveBeenCalledWith("audio_cache_clear");
  });

  it("enqueueAudioForDictation is fire-and-forget and no-ops on empty id", () => {
    const catchSpy = vi.fn();
    invokeMock.mockReturnValue({ catch: catchSpy });
    enqueueAudioForDictation("dict-1");
    expect(invokeMock).toHaveBeenCalledWith("audio_enqueue_dictation", {
      dictationId: "dict-1",
    });
    expect(catchSpy).toHaveBeenCalledTimes(1);
    invokeMock.mockReset();
    enqueueAudioForDictation("");
    expect(invokeMock).not.toHaveBeenCalled();
  });
});
