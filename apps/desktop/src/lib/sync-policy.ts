// Per-field settings sync policy (YAPSTACK_SYNC_ARCHITECTURE §4.6).
//
// Every persisted `Settings` field belongs to exactly one policy class. This is
// the single source of truth the sync layer consults before it ever projects
// local settings into the (potentially uploaded) synced store:
//
//   - "device-local": NEVER leaves the device. Audio input device, window/UI
//     chrome, engine/model selection, download-gated toggles — these describe
//     *this* machine and syncing them would fight the other device's hardware.
//   - "synced":       plaintext-safe user preferences that should converge
//     across devices (speaker names, folder/audio prefs, language, dictation
//     and insight configuration minus any embedded secret).
//   - "secret":       secrets that must be vault-WRAPPED (CRYPTO_SPEC §4) before
//     any persistence that could sync. Today this is the AI connection
//     credentials (apiKey / baseUrl live inside `aiConfig`). They must NEVER be
//     written to a syncable surface in plaintext.
//
// The mechanism is caller-agnostic: this module only classifies and projects.
// The sync runtime decides *when* to sync; `ai.ts`/the Rust vault command
// decide *how* to wrap. See feedback_naming_mechanism_vs_policy.

import type { Settings } from "@/stores/appStore";

export type FieldSyncClass = "device-local" | "synced" | "secret";

/**
 * Canonical classification for every top-level `Settings` key. Keeping this a
 * `Record<keyof Settings, …>` means adding a new setting is a compile error
 * until its sync class is declared — a field can never silently default into
 * the sync stream.
 */
export const SETTINGS_SYNC_POLICY: Record<keyof Settings, FieldSyncClass> = {
  // Device-local — hardware / this-machine chrome, never synced.
  selectedMicDeviceId: "device-local",
  captureSource: "device-local",
  selectedEngine: "device-local",
  selectedModelSize: "device-local",
  selectedParakeetVariant: "device-local",
  diarizationEnabled: "device-local",
  sidebarCollapsed: "device-local",
  theme: "device-local",
  showRecordingIndicator: "device-local",
  dictationDuckEnabled: "device-local",
  dictationDuckAmount: "device-local",
  audioSaveLocation: "device-local",
  onboarding: "device-local",

  // Synced — plaintext-safe cross-device preferences.
  speakerNames: "synced",
  language: "synced",
  mixConfig: "synced",
  bufferMaxSeconds: "synced",
  silenceDurationMs: "synced",
  maxChunkSeconds: "synced",
  promptContextChars: "synced",
  promptDecaySilenceSeconds: "synced",
  shortcutBindings: "synced",
  audioExportFormat: "synced",
  mp3Bitrate: "synced",
  dictation: "synced",
  insights: "synced",

  // Secret — contains AI connection credentials; must be vault-wrapped, never
  // synced raw. `aiConfig.connections[*].apiKey` / `baseUrl` are the sensitive
  // members (see `stripAiSecrets`).
  aiConfig: "secret",
};

export function classifySettingField(key: keyof Settings): FieldSyncClass {
  return SETTINGS_SYNC_POLICY[key];
}

/**
 * A projection of `Settings` safe to hand to the synced store: only "synced"
 * fields survive. Device-local fields are dropped (they belong to the other
 * machine's hardware) and secret fields are dropped here — they take the
 * separate vault-wrapped path (`prepareAiConfigForSync`), never the plaintext
 * projection. This is the guard that STOPS the AI apiKey from ever entering the
 * sync stream in plaintext.
 */
export function projectSyncedSettings(settings: Settings): Partial<Settings> {
  const out: Partial<Settings> = {};
  for (const key of Object.keys(settings) as (keyof Settings)[]) {
    if (SETTINGS_SYNC_POLICY[key] === "synced") {
      // Preserve value identity; the sync layer serializes downstream.
      (out as Record<string, unknown>)[key] = settings[key];
    }
  }
  return out;
}

/**
 * True if `key` is safe to include in a plaintext synced payload. Secrets and
 * device-local fields are not.
 */
export function isSyncableField(key: keyof Settings): boolean {
  return SETTINGS_SYNC_POLICY[key] === "synced";
}
