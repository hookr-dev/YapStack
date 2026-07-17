import { describe, it, expect } from "vitest";
import {
  shouldShowUpgrade,
  normalizeCode,
  groupBase32,
  formatRecoveryCode,
  formatFingerprint,
  isValidRecoveryCode,
  isValidServerUrl,
  formatSyncProgress,
  formatCatchingUp,
  formatBytes,
  formatLastSynced,
} from "./sync";

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
