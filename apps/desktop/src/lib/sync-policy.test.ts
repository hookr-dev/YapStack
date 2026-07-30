import { describe, it, expect } from "vitest";
import {
  classifySettingField,
  projectSyncedSettings,
  isSyncableField,
} from "./sync-policy";
import { prepareAiConfigForSync, stripAiSecrets, type AIConfig } from "./ai";
import type { Settings } from "@/stores/appStore";

const sampleAiConfig: AIConfig = {
  connections: [
    {
      id: "c1",
      name: "OpenAI",
      kind: "openai",
      baseUrl: "https://api.openai.com/v1",
      apiKey: "sk-SECRET-should-never-sync",
    },
  ],
  profiles: [],
  assignments: { chatProfileId: null, aiActionsProfileId: null },
};

// Minimal Settings fixture — only the fields the policy asserts on need to be
// realistic; the classification is keyed by name, not value.
const sampleSettings = {
  selectedMicDeviceId: "device-42",
  speakerNames: { s1: { 0: "Alice" } },
  language: "en",
  aiConfig: sampleAiConfig,
  theme: "dark",
} as unknown as Settings;

describe("SETTINGS_SYNC_POLICY", () => {
  it("classifies hardware/chrome as device-local", () => {
    expect(classifySettingField("selectedMicDeviceId")).toBe("device-local");
    expect(classifySettingField("theme")).toBe("device-local");
    expect(classifySettingField("selectedEngine")).toBe("device-local");
  });
  it("classifies analytics consent as device-local — consent never travels", () => {
    // One device opting out must not opt the other one out (or back in) via sync.
    expect(classifySettingField("analyticsEnabled")).toBe("device-local");
    expect(isSyncableField("analyticsEnabled")).toBe(false);
  });

  it("classifies user prefs as synced", () => {
    expect(classifySettingField("speakerNames")).toBe("synced");
    expect(classifySettingField("language")).toBe("synced");
  });
  it("classifies aiConfig (holds credentials) as secret", () => {
    expect(classifySettingField("aiConfig")).toBe("secret");
  });
});

describe("projectSyncedSettings", () => {
  it("includes only synced fields", () => {
    const projected = projectSyncedSettings(sampleSettings);
    expect(projected.speakerNames).toBeDefined();
    expect(projected.language).toBe("en");
  });

  it("NEVER leaks the AI apiKey into the synced projection", () => {
    const projected = projectSyncedSettings(sampleSettings);
    expect("aiConfig" in projected).toBe(false);
    expect(JSON.stringify(projected)).not.toContain("sk-SECRET");
  });

  it("drops device-local fields", () => {
    const projected = projectSyncedSettings(sampleSettings);
    expect("selectedMicDeviceId" in projected).toBe(false);
    expect("theme" in projected).toBe(false);
  });
});

describe("isSyncableField", () => {
  it("is false for secret and device-local, true for synced", () => {
    expect(isSyncableField("aiConfig")).toBe(false);
    expect(isSyncableField("selectedMicDeviceId")).toBe(false);
    expect(isSyncableField("language")).toBe(true);
  });
});

describe("AI secret vault-wrapping (deliverable E)", () => {
  it("stripAiSecrets removes apiKey/baseUrl entirely", () => {
    const stripped = stripAiSecrets(sampleAiConfig);
    const s = JSON.stringify(stripped);
    expect(s).not.toContain("sk-SECRET");
    expect(s).not.toContain("api.openai.com");
    expect(stripped.connections[0].name).toBe("OpenAI");
  });

  it("prepareAiConfigForSync wraps the apiKey via the vault, never emits plaintext", async () => {
    const wrap = async (pt: string) => `wrapped(${pt.length})`;
    const wrapped = await prepareAiConfigForSync(sampleAiConfig, wrap);
    const s = JSON.stringify(wrapped);
    expect(s).not.toContain("sk-SECRET");
    expect(wrapped.connections[0].wrappedApiKey).toBe("wrapped(27)");
    expect(wrapped.connections[0].wrappedBaseUrl).toContain("wrapped(");
    // The plaintext keys must be absent from the wrapped shape.
    expect("apiKey" in wrapped.connections[0]).toBe(false);
    expect("baseUrl" in wrapped.connections[0]).toBe(false);
  });
});
