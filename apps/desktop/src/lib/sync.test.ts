import { describe, it, expect } from "vitest";
import {
  shouldShowUpgrade,
  normalizeCode,
  groupBase32,
  formatRecoveryCode,
  formatFingerprint,
  isValidRecoveryCode,
  isValidServerUrl,
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
