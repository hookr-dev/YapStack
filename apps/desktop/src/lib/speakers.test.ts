import { describe, it, expect } from "vitest";
import {
  normalizeSpeakerNames,
  resolveSpeakerLabel,
  speakerKeyOf,
  speakersFromNames,
} from "./speakers";

const mic = (speaker_id: number | null = null) => ({ source: "Mic" as const, speaker_id });
const sys = (speaker_id: number | null = null) => ({ source: "System" as const, speaker_id });

describe("speakerKeyOf", () => {
  it("keys undiarized channels by channel and clusters by channel + id", () => {
    expect(speakerKeyOf(mic())).toBe("mic");
    expect(speakerKeyOf(sys())).toBe("system");
    expect(speakerKeyOf(mic(0))).toBe("mic:c0");
    expect(speakerKeyOf(sys(0))).toBe("system:c0");
  });

  it("prefers an explicit speaker_key_override", () => {
    expect(speakerKeyOf({ ...mic(0), speaker_key_override: "c3" })).toBe("c3");
  });
});

describe("resolveSpeakerLabel", () => {
  it("labels the undiarized channels by audience", () => {
    expect(resolveSpeakerLabel(mic(), undefined, "model")).toBe("You");
    expect(resolveSpeakerLabel(mic(), undefined, "human")).toBe("Me");
    expect(resolveSpeakerLabel(sys(), undefined, "model")).toBe("Other");
    expect(resolveSpeakerLabel(sys(), undefined, "human")).toBe("Them");
  });

  it("labels clusters 'Speaker N' on either channel", () => {
    expect(resolveSpeakerLabel(mic(0), undefined, "model")).toBe("Speaker 1");
    expect(resolveSpeakerLabel(sys(2), undefined, "human")).toBe("Speaker 3");
  });

  it("uses a display name, falling back on a blank one", () => {
    const speakers = { "mic:c1": { display_name: "Dana" }, "system:c0": { display_name: "  " } };
    expect(resolveSpeakerLabel(mic(1), speakers, "model")).toBe("Dana");
    expect(resolveSpeakerLabel(sys(0), speakers, "model")).toBe("Speaker 1");
  });

  it("lets is_self_override win over the automatic rule both ways", () => {
    const speakers = {
      "mic:c1": { display_name: "Clay", is_self_override: true },
      mic: { is_self_override: false },
    };
    expect(resolveSpeakerLabel(mic(1), speakers, "model")).toBe("You");
    expect(resolveSpeakerLabel(mic(1), speakers, "human")).toBe("Me");
    expect(resolveSpeakerLabel(mic(), speakers, "model")).toBe("Other");
  });

  it("resolves an overridden key against that key's speaker", () => {
    const speakers = { "system:c4": { display_name: "Alice" } };
    expect(
      resolveSpeakerLabel({ ...mic(0), speaker_key_override: "system:c4" }, speakers, "human"),
    ).toBe("Alice");
  });
});

describe("normalizeSpeakerNames", () => {
  it("copies a legacy bare-id name to both channels unless a keyed name exists", () => {
    expect(normalizeSpeakerNames({ "0": "Alice", "mic:c0": "Bob" })).toEqual({
      "mic:c0": "Bob",
      "system:c0": "Alice",
    });
    expect(normalizeSpeakerNames(undefined)).toEqual({});
  });

  it("feeds speakersFromNames", () => {
    expect(speakersFromNames({ "system:c1": "Eve" })).toEqual({
      "system:c1": { display_name: "Eve" },
    });
  });
});
