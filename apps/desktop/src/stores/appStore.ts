import { create } from "zustand";
import { persist } from "zustand/middleware";
import { listen } from "@tauri-apps/api/event";
import { commands } from "@/lib/tauri";
import { commandErrorMessage } from "@/lib/command-error";

/** Backend event fired by the sync drain after it merges peer changes (R6 item 2). */
const SYNC_APPLIED_EVENT = "sync://applied";
/** Collapse a burst of merge events into one coarse refresh (1.5s). */
const SYNC_APPLIED_DEBOUNCE_MS = 1500;

/**
 * Poll the builder-managed `backend_ready` flag until the Rust setup hook
 * has managed every command dependency. The webviews load concurrently with
 * setup, so invoking a setup-managed command too early fails with Tauri's
 * "state not managed" error. Resolves (with a logged warning) after the
 * timeout rather than blocking the app forever.
 */
async function waitForBackendReady(timeoutMs = 15000): Promise<void> {
  const start = Date.now();
  for (;;) {
    try {
      if (await commands.backendReady()) return;
    } catch {
      // invoke layer not up yet — keep polling
    }
    if (Date.now() - start > timeoutMs) {
      console.error(
        "backend_ready timed out; proceeding with engine setup anyway",
      );
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}
import type {
  CaptureStatusDto,
  BufferStatusDto,
  AudioDeviceInfoDto,
  ModelInfoDto,
  ModelSizeDto,
  CaptureSourceDto,
  MixConfigDto,
  LiveTranscriptionConfig,
  LiveTranscriptionPhase,
  LiveSegmentEvent,
  EngineKindDto,
  ParakeetVariantDto,
  EngineDescriptorDto,
  ParakeetModelInfoDto,
  SortformerModelInfoDto,
} from "@/lib/tauri";
import {
  createSession as dbCreateSession,
  updateSessionTitle,
  completeSession,
  markSessionCompleted as dbMarkSessionCompleted,
  markSessionRecording as dbMarkSessionRecording,
  deleteSession as dbDeleteSession,
  listSessions,
  getSession,
  getSessionSegments,
  insertSegment,
  deleteAllSessions,
  togglePin as dbTogglePin,
  createFolder as dbCreateFolder,
  updateFolder as dbUpdateFolder,
  updateSegmentText as dbUpdateSegmentText,
  softDeleteSegment as dbSoftDeleteSegment,
  softDeleteSegments as dbSoftDeleteSegments,
  toggleSegmentHidden as dbToggleSegmentHidden,
  setSegmentsHidden as dbSetSegmentsHidden,
  createManualSession as dbCreateManualSession,
  deleteFolder as dbDeleteFolder,
  updateFolderParent as dbUpdateFolderParent,
  listFolders,
  listSessionAudioParts,
  listAllAudioPartPaths,
  addSessionToFolder as dbAddSessionToFolder,
  removeSessionFromFolder as dbRemoveSessionFromFolder,
  removeSessionFromAllFolders as dbRemoveSessionFromAllFolders,
  listAllSessionFolders,
  reorderFolders as dbReorderFolders,
  listDictationHistory,
  deleteDictationHistoryEntry as dbDeleteDictationHistoryEntry,
  getDictationHistoryEntry,
  clearDictationHistory as dbClearDictationHistory,
  clearDictationSessionLink,
  listTags,
  listAllSessionTags,
  createTag as dbCreateTag,
  deleteTag as dbDeleteTag,
  addSessionTag as dbAddSessionTag,
  removeSessionTag as dbRemoveSessionTag,
  getTagByName,
  isRecordingStale,
} from "@/lib/db";
import type { DbDictationHistory, DbTag } from "@/lib/db";
import { findBranchConflicts, buildFolderTree, buildChildMap, type FolderTreeNode } from "@/lib/folder-tree";
import type { DbSession, DbSegment, DbFolder, DbAudioPart } from "@/lib/db";
import type { SessionPartReadyEvent } from "@/lib/events";
import { toast } from "sonner";
import { DEFAULT_AI_CONFIG } from "@/lib/ai";
import {
  DEFAULT_INSIGHTS_SETTINGS,
  type InsightsSettings,
  type LiveInsightResult,
  type LiveInsightStatus,
} from "@/lib/insights";
import { buildVocabularyHints } from "@/lib/transcription";
import type { AIConfig } from "@/lib/ai";
import {
  DEFAULT_SYNC_SERVER_URL,
  enqueueAudioForSession,
  syncCommands,
  type RelayConnState,
  type RelayProbeError,
  type SyncStatus,
} from "@/lib/sync";
import {
  migrateLegacyAISettings,
  type LegacyAISettings,
  type LegacyDictationSlot,
} from "@/lib/ai-config";
import {
  trackSessionCreated,
  trackSessionStopped,
  trackSessionDeleted,
  trackSessionsCleared,
  trackManualNoteCreated,
  trackFolderCreated,
  trackSessionPinned,
  trackSessionUnpinned,
  trackModelDownloaded,
  trackModelDeleted,
  trackModelSwitched,
  trackEngineError,
  trackSettingChanged,
  trackSessionMovedToFolder,
} from "@/lib/analytics";

export type ThemeMode = "light" | "dark" | "system";

export type DictationOutputAction = "paste" | "clipboard" | "new-note";

export interface DictationSlot {
  id: string;
  name: string;
  enabled: boolean;
  /** Profile to use for AI cleanup. null = no cleanup. */
  profileId: string | null;
  prompt: string;
  outputAction: DictationOutputAction;
  defaultBinding?: string;
}

export type DictationActivationMode = "hold" | "toggle";

export interface DictationSettings {
  enabled: boolean;
  activationMode: DictationActivationMode;
  slots: DictationSlot[];
}

const CLEAN_FOCUS_PROMPT = `You are a post-processor for dictated speech that will be sent to an LLM as a prompt. Your job is to turn raw transcription into a clean, precise version of what the speaker actually meant to say.

Rules:

1. Resolve self-corrections. When the speaker revises mid-thought ("make it red, actually no, blue" / "use Postgres, wait, use SQLite instead"), keep ONLY the final intent. Discard the earlier version entirely — do not hedge, do not mention the revision.

2. Remove disfluencies: filler words (um, uh, like, you know, I mean, sort of), false starts, abandoned fragments, and verbal throat-clearing ("so basically", "okay so", "right so").

3. Consolidate repeated or circling ideas. Speakers often restate the same point as they refine it — merge these into the single clearest statement.

4. Fix grammar, tense, and sentence boundaries, but do not upgrade the register. Keep the speaker's natural voice; do not make casual speech sound formal.

5. Preserve technical content exactly. Code, file paths, library names, CLI commands, acronyms, product names, numbers, and identifiers must survive intact. Correct obvious Whisper misrenderings of technical terms from context (e.g., "react jay s" → "React.js", "get hub" → "GitHub", "pie torch" → "PyTorch", "cursor I D" → "Cursor IDE"). When unsure, keep the speaker's word.

6. Organize for an LLM reader. Lead with the core ask or topic. Group related points. Use short paragraphs or a short list if the dictation naturally contains enumerated items — otherwise prose. Do not add headings, preambles, or meta-commentary.

7. Do not invent. Never add requirements, context, constraints, or assumptions the speaker did not state. If something is genuinely ambiguous, leave it ambiguous rather than guess.

8. Do not answer, interpret, or act on the content — only clean it. The downstream LLM will handle the actual task.

Output only the cleaned text. No preface, no explanation, no quotation marks.`;

const ENGINEER_PROMPT = `You are a post-processor for dictated speech that will be sent to an LLM as a prompt.

Your job is to turn raw transcription into a clean, precise version of what the speaker actually meant to say.

The speaker is a lead engineer dictating prompts, notes, implementation requests, architectural thoughts, or feedback for an LLM, junior engineer, or coding agent. They may understand the intended system behavior or technical direction but may not know the exact vocabulary, architectural term, design pattern, library name, or engineering concept.

Your goal is to preserve the speaker's actual intent while improving clarity, technical precision, and usefulness. Improve the wording inline where the meaning is clear, but do not add new requests, assumptions, requirements, or closing instructions.

Rules:

1. Resolve self-corrections. When the speaker revises mid-thought, keep only the final intent. Discard the earlier version entirely. Do not hedge or mention the revision.

2. Remove disfluencies: filler words, false starts, abandoned fragments, repeated phrasing, and verbal throat-clearing.

3. Consolidate repeated or circling ideas. Speakers often restate the same point while refining it. Merge these into the single clearest statement.

4. Fix grammar, tense, punctuation, and sentence boundaries, but keep the speaker's natural voice. Do not make casual speech sound overly formal.

5. Preserve technical content exactly when it is clear. Code, file paths, library names, CLI commands, acronyms, product names, numbers, identifiers, database fields, routes, and API names must survive intact.

6. Correct obvious transcription errors for technical terms from context. Examples:
   - "react jay s" → "React.js"
   - "get hub" → "GitHub"
   - "pie torch" → "PyTorch"
   - "cursor I D" → "Cursor IDE"
   - "post grass" → "Postgres"
   - "prisma O R M" → "Prisma ORM"
   - "type script" → "TypeScript"
   - "node jay s" → "Node.js"
   - "next jay s" → "Next.js"

7. When the speaker appears to be reaching for a technical concept, infer the most likely term or keyword from context and use it naturally in the cleaned output, as long as the meaning is strongly implied. Examples:
   - "the thing that keeps jobs from running twice" → "idempotency"
   - "making sure messages happen in order" → "ordered message processing"
   - "splitting the big file into smaller parts" → "modularizing the controller"
   - "the shape of the data between services" → "schema contract"
   - "how we recover when it fails halfway through" → "retry and failure recovery strategy"
   - "making sure two systems don't disagree" → "consistency guarantees"
   - "the place where all writes go first" → "source of truth"

8. Do not invent. Never add requirements, technologies, context, constraints, examples, explanations, or follow-up asks the speaker did not state or strongly imply.

9. Do not append generic helper instructions. Do not add closing sentences like:
   - "If useful, also help identify the right architectural terms…"
   - "Please explain this clearly to junior engineers…"
   - "Make recommendations…"
   - "Let me know if you need more context…"

   Only include requests like this if the speaker actually dictated them.

10. Terminology improvement should happen inline. Replace vague wording with clearer technical wording when the intended concept is clear. Do not add a separate vocabulary-improvement request unless the speaker explicitly asked for one.

11. If the concept is ambiguous, preserve the ambiguity rather than guessing. You may make the uncertainty clearer using the speaker's perspective, such as:
   - "I'm trying to describe the right abstraction here."
   - "I may be thinking of something like idempotency, but I'm not sure."
   - "Help identify the right term for this pattern."

12. Organize for an LLM reader. Lead with the core ask or topic. Group related points. Use short paragraphs or a short list if the dictation naturally contains enumerated items. Otherwise, use clean prose.

13. Keep the output prompt-ready. The cleaned text should be something the speaker could paste directly into another LLM, send to a junior engineer, or use as an implementation prompt.

14. Do not answer, interpret, or act on the underlying request. Do not solve the problem. Only clean and clarify the dictated input.

15. Output only the cleaned text. No preface, no explanation, no quotation marks, and no meta-commentary.`;

export const DEFAULT_DICTATION_SLOTS: DictationSlot[] = [
  {
    id: "1",
    name: "Raw Dictation",
    enabled: true,
    profileId: null,
    prompt: "",
    outputAction: "paste",
    defaultBinding: "Control+Shift+D",
  },
  {
    id: "2",
    name: "Clean & Focus",
    enabled: true,
    profileId: null,
    prompt: CLEAN_FOCUS_PROMPT,
    outputAction: "paste",
    defaultBinding: "Control+Shift+C",
  },
  {
    id: "3",
    name: "Engineer",
    enabled: true,
    profileId: null,
    prompt: ENGINEER_PROMPT,
    outputAction: "paste",
    defaultBinding: "Control+Shift+X",
  },
];

export const DEFAULT_DICTATION_SETTINGS: DictationSettings = {
  enabled: true,
  activationMode: "hold",
  slots: DEFAULT_DICTATION_SLOTS,
};

export interface OnboardingState {
  completedFlows: Record<string, string>; // flowId → ISO timestamp
}

/**
 * Persisted sync slice (YAPSTACK_SYNC §14). Deliberately holds NO secrets: the
 * vault key and auth tokens live in the OS keychain (Rust side), never here.
 * Only the non-sensitive handles the UI needs to render connection state are
 * persisted — the server URL, whether sync is enabled, the last-seen changeset
 * cursor, and this device's public fingerprint. Everything else is fetched live
 * via `syncCommands.status()`.
 */
export interface SyncConfig {
  /** Relay base URL — default hosted, or a pasted self-host URL. */
  serverUrl: string;
  /** True once crr_migrate has run and the user opted into sync. */
  syncEnabled: boolean;
  /** Signed-in account email (non-secret), null when signed out. */
  email: string | null;
  /** Last-seen server changeset_seq watermark (dense cursor, arch §7). */
  lastChangesetSeq: number;
  /** This device's public fingerprint (base32 4×4), null before enrollment. */
  deviceFingerprint: string | null;
}

export interface Settings {
  captureSource: CaptureSourceDto;
  selectedMicDeviceId: string | null;
  selectedEngine: EngineKindDto;
  selectedModelSize: ModelSizeDto;
  selectedParakeetVariant: ParakeetVariantDto;
  diarizationEnabled: boolean;
  /// `{ [sessionId]: { [speakerId]: "Alice" } }`. Client-side persisted.
  /// Populated when Parakeet + Sortformer diarization tags segments; applies
  /// to the `SpeakerHeader` component. When diarization is off, segments
  /// fall back to source-based labels ("You" / "Other") via `SourceHeader`.
  speakerNames: Record<string, Record<number, string>>;
  language: string;
  mixConfig: MixConfigDto;
  bufferMaxSeconds: number;
  silenceDurationMs: number;
  maxChunkSeconds: number;
  promptContextChars: number;
  promptDecaySilenceSeconds: number;
  theme: ThemeMode;
  sidebarCollapsed: boolean;
  /** AI Connection + Profile config (replaces the pre-refactor single-provider shape). */
  aiConfig: AIConfig;
  shortcutBindings: Record<string, string>;
  audioSaveLocation: string | null;
  audioExportFormat: "wav" | "mp3";
  mp3Bitrate: number;
  dictation: DictationSettings;
  /** Live insights — overlay-driven LLM extractions on a cadence trigger. */
  insights: InsightsSettings;
  showRecordingIndicator: boolean;
  /// Lower the system output volume during the recording phase of a
  /// dictation, so the user can hear themselves over earphone playback.
  /// Restored as soon as recording ends. Only ever lowers — never raises.
  /// macOS + Windows (no-op on other platforms).
  dictationDuckEnabled: boolean;
  /// Fraction by which to reduce the current system volume while ducking,
  /// in [0, 1]. The ducked level is `current * (1 - dictationDuckAmount)`.
  /// 0 = no reduction, 1 = mute. Default 0.8.
  dictationDuckAmount: number;
  onboarding: OnboardingState;
}

export type EnginePhase =
  | "idle"
  | "downloading"
  | "initializing"
  | "ready"
  | "error";

export interface ListFilter {
  type: "all" | "pinned" | "folder" | "dictation";
  folderId?: string;
}

interface AppState {
  // Engine setup
  enginePhase: EnginePhase;
  engineError: string | null;
  modelDownloadProgress: number | null;

  // Capture (pushed from backend events)
  captureStatus: CaptureStatusDto | null;
  bufferInfo: BufferStatusDto | null;

  // Navigation
  currentView: "note-list" | "note-detail" | "settings";
  selectedSessionId: string | null;
  listFilter: ListFilter;

  // Session list (from SQLite)
  sessions: DbSession[];

  // Folders
  folders: DbFolder[];
  folderTree: FolderTreeNode[];
  folderByIdMap: Map<string, DbFolder>;
  folderChildMap: Map<string, string[]>;
  sessionFolderMap: Record<string, string[]>;

  // Active recording session
  activeSessionId: string | null;
  // Dictation uses `startLiveTranscription` with its own synthetic session id
  // that is *not* inserted into the `sessions` table. The same live-segment
  // events fire through `onLiveSegment`, which would fail the segments FK.
  // Track the dictation id so `onLiveSegment` can skip DB persistence for it.
  dictationSessionId: string | null;
  setDictationSessionId: (id: string | null) => void;
  activeSessionSegments: DbSegment[];
  activeSessionStartTime: number | null;
  /** Parts of the active session, hydrated on resume so the player and
   *  duration math have the prior parts in scope. Empty for fresh sessions. */
  activeSessionParts: DbAudioPart[];

  // Viewing a completed session
  viewSessionSegments: DbSegment[];
  viewSession: DbSession | null;
  /** Parts of the currently-viewed session. The AudioPlayer consumes this
   *  to play the full session timeline across parts. */
  viewSessionParts: DbAudioPart[];

  // Live transcription
  liveTranscriptionActive: boolean;
  livePhase: LiveTranscriptionPhase | null;
  backfillActive: boolean;
  /// Audio offset (seconds) where backfill ends and live audio begins, used to
  /// position the "Processing prior audio" indicator between old and new
  /// segments in the chat. Seeded from the requested backfill duration, then
  /// expanded as backfill chunks land so the boundary tracks effective
  /// (clamped) backfill rather than requested. Null when no backfill is
  /// active for this session.
  backfillBoundarySeconds: number | null;
  // True from when the user clicks Stop until the backend's Stopped/Error
  // event fires. Lets the UI show a pulsing "Finalizing…" state immediately
  // while the engine drains the current chunk + finalizes the WAV.
  sessionStopping: boolean;

  // Note refresh (for cross-component refresh signaling)
  noteRefreshCounter: number;

  /**
   * Store-visible edit-in-progress signal (LIVE_SESSION_STATE.md D4 normative).
   * The segment editor is uncontrolled (contentEditable holds in-flight text in the
   * DOM), so the sync-applied live refresh must consult a store signal to avoid
   * clobbering an open edit. Currently set for the duration of an in-flight
   * `editSegmentText` save; the D4 refresh skips reloading the open session's
   * segments while it is non-null. Slice 4 / a follow-up can additionally set it on
   * `EditableSegment` focus/blur to cover the whole open-edit window.
   */
  editingSegmentId: string | null;
  setEditingSegmentId: (id: string | null) => void;

  // Audio playback
  playbackTime: number;
  isPlaying: boolean;

  // Tags
  tags: DbTag[];
  sessionTagMap: Record<string, string[]>;

  // Dictation history (runtime, loaded from SQLite)
  dictationHistory: DbDictationHistory[];

  // Live insights (runtime, not persisted — cleared on session end and on
  // active-insight change). See lib/insights.ts and the Insight engine hook.
  liveInsightResult: LiveInsightResult | null;
  liveInsightStatus: LiveInsightStatus;
  liveInsightError: string | null;
  setLiveInsightResult: (result: LiveInsightResult | null) => void;
  setLiveInsightStatus: (status: LiveInsightStatus) => void;
  setLiveInsightError: (error: string | null) => void;
  /** The Current Insight: the slot whose output is rendering in the overlay
   *  right now. Distinct from `settings.insights.defaultInsightId` — the
   *  Default is persisted and drives session-start; Current is ephemeral
   *  and mutated only by overlay interactions (header dropdown picks,
   *  × button). Initialized from the Default at session start; cleared on
   *  session end. Setting to `null` mid-session hides the overlay for that
   *  session only — Default is untouched. */
  currentInsightId: string | null;
  setCurrentInsightId: (id: string | null) => void;

  // Update availability (runtime, not persisted)
  updateAvailable: { version: string; body: string | undefined } | null;
  updateDismissedVersion: string | null;

  // Settings (persisted)
  settings: Settings;

  // Sync (config persisted; live status + connection health runtime-only).
  // See lib/sync.ts.
  syncConfig: SyncConfig;
  syncStatus: SyncStatus | null;
  /** Probe-derived relay connection health — a SEPARATE axis from sync phase
   *  (§1b). Runtime-only and never latched/persisted: recomputed from live
   *  probes so a stale "unreachable" can't linger (plan §3). */
  relayConn: RelayConnState;
  setSyncConfig: (partial: Partial<SyncConfig>) => void;
  setSyncStatus: (status: SyncStatus | null) => void;
  /** Fetch live sync/auth status from the Rust runtime. Surfaces connection
   *  errors verbatim into `syncStatus.lastError` — never auto-routes or falls
   *  back (repo posture: the user fixes a broken connection). */
  refreshSyncStatus: () => Promise<void>;
  /** Probe `url` (the Test-connection button and the one-shot blur/paste in
   *  T027 both call this). Single-flight via a monotonic token: a stale
   *  response from a superseded probe is discarded. On success auto-persists
   *  `syncConfig.serverUrl = normalizedUrl` — but ONLY when signed out (a
   *  signed-in URL is locked; changing it is an explicit sign-out flow). */
  probeRelay: (url: string) => Promise<void>;
  /** Return connection health to `idle` and bump the probe token so any
   *  in-flight probe's response is discarded (cancel-on-edit). Called when the
   *  user edits the URL field. Re-probing the same URL twice is cheap + safe. */
  resetProbe: () => void;

  // Devices (loaded once)
  devices: AudioDeviceInfoDto[];
  models: ModelInfoDto[];
  // Engine catalogue + Parakeet/Sortformer model state (loaded on autoSetup).
  // engineCatalogue is the static capability descriptor from the Rust side.
  // parakeetModels/sortformerStatus are the per-variant download status.
  engineCatalogue: EngineDescriptorDto[];
  parakeetModels: ParakeetModelInfoDto[];
  sortformerStatus: SortformerModelInfoDto | null;
  /// What the sidecar reported about the live model right after the
  /// transcription client initialized — resolved acceleration provider
  /// and the variant directory it loaded from. Populated by the
  /// `transcription-engine-loaded` event listener; cleared when the
  /// engine is shut down. Lets `StatusPopover` show "Parakeet · WebGPU"
  /// (or "· CPU" after fallback) keyed to ground truth.
  loadedEngineInfo: {
    engine: EngineKindDto;
    accel: string | null;
    modelDir: string | null;
  } | null;

  // Setters (called by event listeners)
  setCaptureStatus: (status: CaptureStatusDto) => void;
  setBufferInfo: (info: BufferStatusDto) => void;
  setModelDownloadProgress: (p: number | null) => void;

  // Live transcription setters
  setLivePhase: (phase: LiveTranscriptionPhase) => void;
  onLiveSegment: (event: LiveSegmentEvent) => void;
  onBackfillComplete: () => void;
  onSessionPartReady: (event: SessionPartReadyEvent) => void;
  recoverActiveSession: (sessionId: string, effectiveStartEpochMs?: number) => Promise<void>;

  // Actions
  autoSetup: () => Promise<void>;
  loadSessions: () => Promise<void>;
  loadFolders: () => Promise<void>;
  /**
   * Subscribe to the backend `sync://applied` event (R6 item 2): the sync drain merged
   * peer changes into the DB but nothing told the UI, so pulled sessions/notes/folders
   * only appeared after an app restart. On each event we debounce, then refresh the coarse
   * views (sessions / folders / tags and their maps). Returns an unlisten function.
   */
  startSyncAppliedRefresh: () => Promise<() => void>;
  createAndStartSession: (backfillSeconds?: number, trigger?: string) => Promise<void>;
  resumeSession: (sessionId: string) => Promise<void>;
  stopActiveSession: () => Promise<void>;
  openSession: (id: string) => Promise<void>;
  deleteSession: (id: string) => Promise<void>;
  markSessionCompleted: (id: string) => Promise<void>;
  navigateTo: (
    view: "note-list" | "note-detail" | "settings",
    sessionId?: string,
  ) => void;
  /**
   * One-shot signal from anywhere in the app to land the user on a specific
   * Settings location. Consumed by SettingsPanel + AITab + ConnectionsSection
   * on mount, then cleared. `"sync"` (from the ambient sidebar glyph) opens the
   * Sync tab and is cleared by SettingsPanel itself. Single-value rather than a
   * queue — only the most recent intent matters. Cleared after consumption.
   */
  settingsRequest: "ai-add-connection" | "sync" | null;
  setSettingsRequest: (request: "ai-add-connection" | "sync" | null) => void;
  setListFilter: (filter: ListFilter) => void;
  refreshDevices: () => Promise<void>;
  /**
   * Apply a freshly enumerated device list to the store. Reconciles
   * `settings.selectedMicDeviceId`: if the persisted device is no longer
   * present, resets to `null` (= follow default) and toasts the user
   * with the new effective default name. Called by the device broker's
   * `devices-changed` subscription and by `refreshDevices` after a
   * direct IPC fetch.
   */
  applyDeviceList: (devices: AudioDeviceInfoDto[]) => void;
  refreshModels: () => Promise<void>;
  updateSettings: (partial: Partial<Settings>) => void;
  clearAllSessions: () => Promise<void>;
  downloadModel: (size: ModelSizeDto) => Promise<void>;
  deleteModel: (size: ModelSizeDto) => Promise<void>;
  switchModel: (size: ModelSizeDto) => Promise<void>;

  // Engine + Parakeet/Sortformer actions
  loadEngineCatalogue: () => Promise<void>;
  refreshParakeetModels: () => Promise<void>;
  refreshSortformerStatus: () => Promise<void>;
  /// Switch the active transcription engine. Shuts down the current sidecar
  /// client and re-inits with the new engine. The frontend persists the
  /// selection via `updateSettings`; autoSetup picks it up on next start.
  switchEngine: (engine: EngineKindDto) => Promise<void>;
  downloadParakeetModel: (variant: ParakeetVariantDto) => Promise<void>;
  deleteParakeetModel: (variant: ParakeetVariantDto) => Promise<void>;
  /// Toggle speaker diarization. When flipped on for the first time and
  /// Sortformer is missing, downloads it. Re-inits the client so the new
  /// flag takes effect on the next transcribe call.
  setDiarizationEnabled: (enabled: boolean) => Promise<void>;
  /// Rename a speaker for a single session. Pass an empty/whitespace `name`
  /// to clear the override and fall back to the default `Speaker N` label.
  setSpeakerName: (sessionId: string, speakerId: number, name: string) => void;

  // Folder actions
  createFolder: (name: string, parentId?: string | null, icon?: string | null, color?: string | null, description?: string | null) => Promise<void>;
  updateFolder: (id: string, updates: { name?: string; icon?: string | null; color?: string | null; description?: string | null }) => Promise<void>;
  deleteFolder: (id: string) => Promise<void>;
  moveFolder: (folderId: string, newParentId: string | null) => Promise<void>;
  reorderFolders: (folderId: string, overFolderId: string) => Promise<void>;
  loadSessionFolders: () => Promise<void>;

  // Pin/folder session actions
  togglePin: (sessionId: string) => Promise<void>;
  toggleSessionFolder: (sessionId: string, folderId: string) => Promise<void>;
  addSessionToFolder: (sessionId: string, folderId: string) => Promise<void>;
  removeSessionFromAllFolders: (sessionId: string) => Promise<void>;

  // Tag actions
  loadTags: () => Promise<void>;
  loadSessionTags: () => Promise<void>;
  createTag: (name: string, color?: string | null) => Promise<DbTag>;
  deleteTagAction: (id: string) => Promise<void>;
  addTagToSession: (sessionId: string, tagName: string, source?: "manual" | "auto" | "ai") => Promise<void>;
  removeTagFromSession: (sessionId: string, tagId: string) => Promise<void>;

  // Segment editing actions
  editSegmentText: (segmentId: string, newText: string) => Promise<void>;
  deleteSegment: (segmentId: string) => Promise<void>;
  toggleSegmentHidden: (segmentId: string) => Promise<void>;
  showHiddenSegments: boolean;
  setShowHiddenSegments: (show: boolean) => void;
  refreshViewSessionSegments: () => Promise<void>;
  /** D4 live refresh: reload the open non-active session's row + segments on a
   *  sync-applied batch, skipping while an edit is in progress and never bumping
   *  noteRefreshCounter (LIVE_SESSION_STATE.md Gap 2). */
  refreshOpenViewSession: () => Promise<void>;

  // Segment multi-selection (ephemeral; not persisted). Reset on session change.
  selectedSegmentIds: Set<string>;
  lastSelectedSegmentId: string | null;
  toggleSegmentSelected: (
    segmentId: string,
    mode: "toggle" | "range" | "replace",
    orderedIds: string[],
  ) => void;
  setSegmentSelection: (ids: Set<string>) => void;
  clearSegmentSelection: () => void;
  setSegmentAnchor: (id: string | null) => void;
  deleteSegments: (ids: string[]) => Promise<void>;
  setSegmentsHidden: (ids: string[], hidden: boolean) => Promise<void>;

  // Sidebar
  toggleSidebar: () => void;

  // Manual notes
  createManualNote: (title?: string) => Promise<void>;

  // Note refresh
  incrementNoteRefresh: () => void;

  // Audio playback
  setPlaybackTime: (time: number) => void;
  setIsPlaying: (playing: boolean) => void;

  // Dictation history
  loadDictationHistory: () => Promise<void>;
  deleteDictationHistoryEntry: (id: string) => Promise<void>;
  clearDictationHistory: () => Promise<void>;

  // Onboarding
  completeFlow: (flowId: string) => void;

  // Update
  setUpdateAvailable: (update: { version: string; body: string | undefined } | null) => void;
  dismissUpdate: () => void;
}

/**
 * Serialization queue for onLiveSegment.
 * Concurrent backfill + live events can interleave async reads/writes to
 * activeSessionSegments. This queue ensures only one handler runs at a time.
 */
let segmentQueueTail: Promise<void> = Promise.resolve();
let lastSessionsRefreshTime = 0;

/**
 * Monotonic single-flight token for relay probes. Each `probeRelay` bumps it;
 * a resolving/rejecting probe whose token no longer matches the latest is stale
 * (the user re-fired or edited the field) and its result is discarded.
 * `resetProbe` bumps it too, cancelling any in-flight probe.
 */
let relayProbeToken = 0;
function enqueueSegmentWork(fn: () => Promise<void>): void {
  segmentQueueTail = segmentQueueTail.then(fn, fn);
}

/**
 * Cleans up audio files given a pre-fetched list of part paths. Caller
 * must read the paths *before* dropping the session row, since the
 * cascade on `sessions` delete also nukes `session_audio_parts`. Falls
 * back to the legacy session-glob deletion for sessions with no parts
 * rows (older imports, manual notes). Awaited so a failure (player still
 * has the file open, permissions issue, app exit) is logged, surfaced as
 * a toast, and not silently dropped — the part paths are gone from the
 * DB by this point and only the closure variable still holds them.
 */
async function deleteSessionAudio(
  sessionId: string,
  paths: string[],
  audioSaveLocation: string | null,
): Promise<void> {
  try {
    if (paths.length > 0) {
      await commands.deleteAudioFiles(paths);
    } else {
      await commands.deleteSessionWav(sessionId, audioSaveLocation);
    }
  } catch (e) {
    console.error(`Failed to delete audio for session ${sessionId}:`, e, paths);
    toast.error("Some audio files were not deleted", {
      id: `audio-cleanup-${sessionId}`,
    });
  }
}

const defaultSettings: Settings = {
  captureSource: "Mixed",
  selectedMicDeviceId: null,
  // Parakeet is the recommended default for fresh installs. Upgrade users
  // keep whatever engine they had — the v22 migration fallback below still
  // initializes pre-v22 users to "Whisper" so we don't silently swap
  // engines under anyone who chose Whisper explicitly.
  selectedEngine: "Parakeet",
  selectedModelSize: "Small",
  selectedParakeetVariant: "TdtV3",
  diarizationEnabled: false,
  speakerNames: {},
  language: "en",
  mixConfig: { mic_gain: 1.0, system_gain: 1.0, normalize: false },
  bufferMaxSeconds: 300,
  silenceDurationMs: 800,
  maxChunkSeconds: 30,
  promptContextChars: 350,
  promptDecaySilenceSeconds: 5,
  theme: "system",
  sidebarCollapsed: false,
  aiConfig: DEFAULT_AI_CONFIG,
  shortcutBindings: {},
  audioSaveLocation: null,
  audioExportFormat: "mp3",
  mp3Bitrate: 64,
  dictation: DEFAULT_DICTATION_SETTINGS,
  insights: DEFAULT_INSIGHTS_SETTINGS,
  showRecordingIndicator: true,
  dictationDuckEnabled: false,
  dictationDuckAmount: 0.8,
  onboarding: { completedFlows: {} },
};

const defaultSyncConfig: SyncConfig = {
  serverUrl: DEFAULT_SYNC_SERVER_URL,
  syncEnabled: false,
  email: null,
  lastChangesetSeq: 0,
  deviceFingerprint: null,
};

function updateSessionFolderMap(
  current: Record<string, string[]>,
  sessionId: string,
  add: string[],
  remove: string[],
): Record<string, string[]> {
  const ids = (current[sessionId] ?? []).filter((id) => !remove.includes(id));
  for (const id of add) {
    if (!ids.includes(id)) ids.push(id);
  }
  if (ids.length === 0) {
    const { [sessionId]: _, ...rest } = current;
    return rest;
  }
  return { ...current, [sessionId]: ids };
}

async function addSessionToFolderWithConflicts(
  sessionId: string,
  folderId: string,
  currentFolderIds: string[],
  folders: DbFolder[],
): Promise<string[]> {
  const conflicts = findBranchConflicts(folders, currentFolderIds, folderId);
  for (const cId of conflicts) {
    await dbRemoveSessionFromFolder(sessionId, cId);
  }
  await dbAddSessionToFolder(sessionId, folderId);
  return conflicts;
}

function deriveFolderState(folders: DbFolder[]) {
  return {
    folderTree: buildFolderTree(folders),
    folderByIdMap: new Map(folders.map((f) => [f.id, f])),
    folderChildMap: buildChildMap(folders),
  };
}

function createAppStore() {
  return create<AppState>()(
    persist(
      (set, get) => ({
      // Initial state
      enginePhase: "idle",
      engineError: null,
      modelDownloadProgress: null,
      captureStatus: null,
      bufferInfo: null,
      currentView: "note-list",
      selectedSessionId: null,
      listFilter: { type: "all" },
      sessions: [],
      folders: [],
      folderTree: [],
      folderByIdMap: new Map(),
      folderChildMap: new Map(),
      sessionFolderMap: {},
      activeSessionId: null,
      dictationSessionId: null,
      activeSessionSegments: [],
      activeSessionStartTime: null,
      activeSessionParts: [],
      viewSessionSegments: [],
      viewSession: null,
      viewSessionParts: [],
      liveTranscriptionActive: false,
      livePhase: null,
      backfillActive: false,
      backfillBoundarySeconds: null,
      sessionStopping: false,
      noteRefreshCounter: 0,
      editingSegmentId: null,
      setEditingSegmentId: (id: string | null) => set({ editingSegmentId: id }),
      playbackTime: 0,
      isPlaying: false,
      tags: [],
      sessionTagMap: {},
      dictationHistory: [],
      liveInsightResult: null,
      liveInsightStatus: "idle",
      liveInsightError: null,
      currentInsightId: null,
      setLiveInsightResult: (result) => set({ liveInsightResult: result }),
      setLiveInsightStatus: (status) => set({ liveInsightStatus: status }),
      setLiveInsightError: (error) => set({ liveInsightError: error }),
      setCurrentInsightId: (id) => set({ currentInsightId: id }),
      updateAvailable: null,
      updateDismissedVersion: null,
      settings: defaultSettings,
      syncConfig: defaultSyncConfig,
      syncStatus: null,
      relayConn: { kind: "idle" },
      devices: [],
      models: [],
      engineCatalogue: [],
      parakeetModels: [],
      sortformerStatus: null,
      loadedEngineInfo: null,
      showHiddenSegments: false,
      selectedSegmentIds: new Set<string>(),
      lastSelectedSegmentId: null,

      // Setters
      setCaptureStatus: (status) => set({ captureStatus: status }),
      setBufferInfo: (info) => set({ bufferInfo: info }),
      setModelDownloadProgress: (p) => set({ modelDownloadProgress: p }),
      setDictationSessionId: (id) => set({ dictationSessionId: id }),
      setLivePhase: (phase) => {
        const active = phase === "Running";
        set({ livePhase: phase, liveTranscriptionActive: active });

        // When backend reports Stopped or Error, finalize the session.
        // Enqueue finalization on segmentQueueTail so pending segment writes
        // complete before activeSessionId is cleared (prevents race condition).
        if (phase === "Stopped" || phase === "Error") {
          const { activeSessionId, activeSessionStartTime, activeSessionSegments } = get();
          if (activeSessionId) {
            const capturedSessionId = activeSessionId;
            const durationSeconds = activeSessionStartTime
              ? (Date.now() - activeSessionStartTime) / 1000
              : 0;

            enqueueSegmentWork(async () => {
              trackSessionStopped({
                duration_seconds: Math.round(durationSeconds),
                segment_count: activeSessionSegments.length,
              });
              // `completeSession` derives `duration_seconds` from
              // `session_audio_parts` (SUM). Both the part insert (queued by
              // `onSessionPartReady`) and this completion run on
              // `segmentQueueTail`, so the part row is in the DB before we
              // SUM. The wall-clock value is a fallback for the rare case
              // where WAV finalize failed and no part landed.
              await completeSession(capturedSessionId, durationSeconds).catch((e) => {
                console.error("Failed to complete session:", e);
              });

              // S2 audio round-trip: enqueue this session's finalized part(s) for
              // background upload to the relay (server-completeness invariant). The
              // part rows landed above (insert_audio_part_row / onSessionPartReady, same
              // queue), so they are visible now. Fire-and-forget — recording is never
              // blocked, the durable queue survives restart, and on a no-sync build the
              // command is simply absent and the call is swallowed.
              enqueueAudioForSession(capturedSessionId);

              set({
                activeSessionId: null,
                activeSessionStartTime: null,
                liveTranscriptionActive: false,
                livePhase: null,
                // Live-insights runtime is session-scoped; clear when the
                // session ends so a future session doesn't render stale
                // output. `currentInsightId` is re-initialized from
                // `settings.insights.defaultInsightId` at next session start.
                liveInsightResult: null,
                liveInsightStatus: "idle",
                liveInsightError: null,
                currentInsightId: null,
                backfillActive: false,
                backfillBoundarySeconds: null,
                sessionStopping: false,
              });

              // Reload session data — re-read selectedSessionId inside .then()
              // to guard against the user navigating away before resolution.
              try {
                const sessions = await listSessions();
                const { selectedSessionId } = get();
                if (selectedSessionId === capturedSessionId) {
                  const [session, segments] = await Promise.all([
                    getSession(capturedSessionId),
                    getSessionSegments(capturedSessionId),
                  ]);
                  // Re-check: user may have navigated during the inner await
                  if (get().selectedSessionId === capturedSessionId) {
                    set({
                      sessions,
                      viewSession: session,
                      viewSessionSegments: segments,
                    });
                  } else {
                    set({ sessions });
                  }
                } else {
                  set({ sessions });
                }
              } catch (e) {
                console.error("Failed to reload sessions:", e);
              }
            });
          }
        }
      },

      onLiveSegment: (event: LiveSegmentEvent) => {
        enqueueSegmentWork(async () => {
          console.debug(
            `[segment] source=${event.source} chunk=${event.chunk_index} ` +
              `offset=${event.audio_offset_seconds.toFixed(2)}s ` +
              `duration=${event.chunk_duration_seconds.toFixed(2)}s ` +
              `segments=${event.segments.length} ` +
              `origin=${event.origin}`,
          );

          // Primary route: source_kind. Dictation segments are owned by
          // useDictation.ts; they must never reach the session segments
          // table. Belt-and-suspenders: the legacy synthetic-id match below
          // catches any stale event lacking source_kind during a transitional
          // build (default = "session" on the Rust side, but a failed
          // binding regen would surface it here as undefined).
          if (event.source_kind === "dictation") return;
          // Prefer the session_id carried on the event so late-arriving
          // segments (after `setLivePhase("Stopped")` has cleared
          // activeSessionId) still persist to the right session in the DB.
          // Fall back to ambient activeSessionId for backward compat with any
          // older event payload that didn't carry session_id.
          const { activeSessionId, dictationSessionId } = get();
          const targetSessionId = event.session_id ?? activeSessionId;
          if (!targetSessionId) return;
          if (targetSessionId === dictationSessionId) return;

          // Create one segment per Whisper/Parakeet segment to preserve timestamps
          const newSegments: DbSegment[] = [];
          for (const seg of event.segments) {
            const text = seg.text.trim();
            if (!text) continue;
            newSegments.push({
              id: crypto.randomUUID(),
              session_id: targetSessionId,
              source: event.source,
              text,
              audio_offset_seconds:
                event.audio_offset_seconds + seg.start_ms / 1000,
              chunk_duration_seconds: (seg.end_ms - seg.start_ms) / 1000,
              confidence: seg.confidence,
              created_at: new Date().toISOString(),
              chunk_index: event.chunk_index,
              original_text: null,
              edited_at: null,
              deleted_at: null,
              hidden: 0,
              speaker_id: seg.speaker_id ?? null,
            });
          }

          // Track the highest end-offset seen from a backfill chunk so the
          // chat indicator can sit at the actual backfill→live boundary.
          // Stays null until the first backfill chunk lands — using the
          // requested duration as a placeholder would over-shoot the
          // effective (clamped) boundary and mis-classify early live
          // segments as backfill. Updated even for empty chunks (silence
          // still advances the rewind cursor).
          if (event.origin === "backfill" && targetSessionId === get().activeSessionId) {
            const chunkEnd = event.audio_offset_seconds + event.chunk_duration_seconds;
            const current = get().backfillBoundarySeconds;
            if (current == null || chunkEnd > current) {
              set({ backfillBoundarySeconds: chunkEnd });
            }
          }

          if (newSegments.length === 0) return;

          try {
            for (const segment of newSegments) {
              await insertSegment(segment);
            }

            // DB persistence is done. The remaining work updates the in-memory
            // active-session view; only do it if the target session is still
            // the active one. Late arrivals after stop persist to DB but don't
            // disturb whatever the user navigated to.
            const currentActiveId = get().activeSessionId;
            if (!currentActiveId || currentActiveId !== targetSessionId) return;

            // Auto-title from first segment — re-read state to avoid race
            const currentSegments = get().activeSessionSegments;
            if (currentSegments.length === 0) {
              const title = newSegments[0].text.slice(0, 60);
              await updateSessionTitle(targetSessionId, title);
            }

            // Re-check after the title update await
            if (get().activeSessionId !== targetSessionId) return;

            // Re-read activeSessionSegments after awaits to avoid overwriting
            // segments inserted by a concurrent onLiveSegment call
            const latestSegments = get().activeSessionSegments;
            const updated = [...latestSegments, ...newSegments].sort(
              (a, b) => a.audio_offset_seconds - b.audio_offset_seconds,
            );
            set({ activeSessionSegments: updated });

            // Refresh only the active session in the sidebar (avoids full SELECT *)
            // Throttled to max 1x/sec — sidebar lag is imperceptible,
            // and setLivePhase("Stopped") does a full loadSessions() for final state.
            const now = Date.now();
            if (now - lastSessionsRefreshTime >= 1000) {
              lastSessionsRefreshTime = now;
              const freshSession = await getSession(targetSessionId);
              if (freshSession) {
                set({
                  sessions: get().sessions.map((s) =>
                    s.id === targetSessionId ? freshSession : s,
                  ),
                });
              }
            }
          } catch (e) {
            // If the session row is gone (dictation, deleted session, or a
            // very-late segment after the session was removed), the segments
            // FK fails — log and drop, don't alarm the user.
            const msg = e instanceof Error ? e.message : String(e);
            if (/FOREIGN KEY|no such|segments_session/i.test(msg)) {
              console.debug(
                `[segment] dropped for missing session ${targetSessionId}:`,
                msg,
              );
              return;
            }
            console.error("Failed to persist live segment:", e);
            toast.error("Failed to save transcript segment", { id: "segment-write-error" });
          }
        });
      },

      onBackfillComplete: () => {
        set({ backfillActive: false });
      },

      onSessionPartReady: (event) => {
        // The Rust finalize path inserts the parts row before emitting, so
        // by the time we get here the DB is already authoritative — this
        // listener just refreshes the in-store views. Enqueue on
        // segmentQueueTail so the refresh runs strictly before
        // `setLivePhase("Stopped")`'s completion handler reads SUM(parts).
        enqueueSegmentWork(async () => {
          try {
            const refreshed = await listSessionAudioParts(event.session_id);
            const { activeSessionId, selectedSessionId } = get();
            const patch: {
              activeSessionParts?: DbAudioPart[];
              viewSessionParts?: DbAudioPart[];
            } = {};
            if (activeSessionId === event.session_id) {
              patch.activeSessionParts = refreshed;
            }
            if (selectedSessionId === event.session_id) {
              patch.viewSessionParts = refreshed;
            }
            if (Object.keys(patch).length > 0) {
              set(patch);
            }
          } catch (e) {
            console.error("Failed to refresh audio parts:", e);
          }
        });
      },

      recoverActiveSession: async (sessionId: string, effectiveStartEpochMs?: number) => {
        // If we already have this session active, skip
        if (get().activeSessionId === sessionId) return;

        try {
          const [segments, session, parts] = await Promise.all([
            getSessionSegments(sessionId),
            getSession(sessionId),
            listSessionAudioParts(sessionId),
          ]);

          set({
            activeSessionId: sessionId,
            activeSessionSegments: segments,
            activeSessionParts: parts,
            liveTranscriptionActive: true,
            activeSessionStartTime: effectiveStartEpochMs
              ?? (session ? new Date(session.created_at + "Z").getTime() : Date.now()),
            // Initialize the Current Insight from the persisted Default at
            // session start. Mid-session changes to the Default in Settings
            // do not retroactively update this — Settings affects future
            // sessions only.
            currentInsightId: get().settings.insights.defaultInsightId,
          });

          if (session) {
            set({
              selectedSessionId: sessionId,
              viewSession: session,
              viewSessionSegments: segments,
              viewSessionParts: parts,
              currentView: "note-detail",
            });
          }
        } catch (e) {
          console.error("Failed to recover active session:", e);
        }
      },

      // Actions
      loadSessions: async () => {
        try {
          const sessions = await listSessions();
          set({ sessions });
        } catch (e) {
          console.error("Failed to load sessions:", e);
        }
      },

      startSyncAppliedRefresh: async () => {
        // Debounce so a long pulled backlog (many rapid `sync://applied` events) triggers a
        // SINGLE coarse refresh instead of thrashing the UI. Coarse-only by design (v1): no
        // fine-grained per-row invalidation — reload the whole session/folder/tag views.
        let timer: ReturnType<typeof setTimeout> | undefined;
        const coarseRefresh = () => {
          if (timer) clearTimeout(timer);
          timer = setTimeout(() => {
            timer = undefined;
            const s = get();
            void s.loadSessions();
            void s.loadFolders();
            void s.loadSessionFolders();
            void s.loadTags();
            void s.loadSessionTags();
            // D4 (Gap 2): live-refresh the OPEN non-active session's row + segments so a
            // remote-live follow-along fills as segments merge (no manual reopen). The
            // local active session reads from activeSessionSegments and is skipped.
            void s.refreshOpenViewSession();
          }, SYNC_APPLIED_DEBOUNCE_MS);
        };
        const unlisten = await listen(SYNC_APPLIED_EVENT, coarseRefresh);
        return () => {
          if (timer) clearTimeout(timer);
          unlisten();
        };
      },

      loadFolders: async () => {
        try {
          const folders = await listFolders();
          set({ folders, ...deriveFolderState(folders) });
        } catch (e) {
          console.error("Failed to load folders:", e);
        }
      },

      loadSessionFolders: async () => {
        try {
          const rows = await listAllSessionFolders();
          const map: Record<string, string[]> = {};
          for (const row of rows) {
            if (!map[row.session_id]) map[row.session_id] = [];
            map[row.session_id].push(row.folder_id);
          }
          set({ sessionFolderMap: map });
        } catch (e) {
          console.error("Failed to load session folders:", e);
        }
      },

      loadTags: async () => {
        try {
          const tags = await listTags();
          set({ tags });
        } catch (e) {
          console.error("Failed to load tags:", e);
        }
      },

      loadSessionTags: async () => {
        try {
          const rows = await listAllSessionTags();
          const map: Record<string, string[]> = {};
          for (const row of rows) {
            if (!map[row.session_id]) map[row.session_id] = [];
            map[row.session_id].push(row.tag_id);
          }
          set({ sessionTagMap: map });
        } catch (e) {
          console.error("Failed to load session tags:", e);
        }
      },

      createTag: async (name: string, color: string | null = null) => {
        const existing = await getTagByName(name);
        if (existing) return existing;
        const id = crypto.randomUUID();
        await dbCreateTag(id, name, color);
        const tag: DbTag = { id, name, color, created_at: new Date().toISOString() };
        set({ tags: [...get().tags, tag] });
        return tag;
      },

      deleteTagAction: async (id: string) => {
        await dbDeleteTag(id);
        set({ tags: get().tags.filter((t) => t.id !== id) });
        const sessionTagMap = { ...get().sessionTagMap };
        for (const sessionId of Object.keys(sessionTagMap)) {
          sessionTagMap[sessionId] = sessionTagMap[sessionId].filter((tid) => tid !== id);
          if (sessionTagMap[sessionId].length === 0) delete sessionTagMap[sessionId];
        }
        set({ sessionTagMap });
      },

      addTagToSession: async (sessionId: string, tagName: string, source: "manual" | "auto" | "ai" = "manual") => {
        let tag = await getTagByName(tagName);
        if (!tag) {
          tag = await get().createTag(tagName);
        }
        await dbAddSessionTag(sessionId, tag.id, source);
        const map = { ...get().sessionTagMap };
        const current = map[sessionId] ?? [];
        if (!current.includes(tag.id)) {
          map[sessionId] = [...current, tag.id];
          set({ sessionTagMap: map });
        }
      },

      removeTagFromSession: async (sessionId: string, tagId: string) => {
        await dbRemoveSessionTag(sessionId, tagId);
        const map = { ...get().sessionTagMap };
        const current = map[sessionId] ?? [];
        map[sessionId] = current.filter((id) => id !== tagId);
        if (map[sessionId].length === 0) delete map[sessionId];
        set({ sessionTagMap: map });
      },

      createAndStartSession: async (backfillSeconds?: number, trigger?: string) => {
        const { settings, enginePhase, captureStatus } = get();

        if (enginePhase !== "ready") {
          throw new Error("Engine is not ready");
        }
        if (captureStatus?.state !== "Capturing") {
          throw new Error("Audio capture is not active");
        }

        const sessionId = crypto.randomUUID();

        // Attribute the recording to THIS device (LIVE_SESSION_STATE D2); null when
        // sync is not configured → single-device semantics unchanged.
        await dbCreateSession(
          sessionId,
          settings.captureSource,
          get().syncStatus?.deviceFingerprint ?? null,
        );

        const [freshFolders, freshTags] = await Promise.all([listFolders(), listTags()]);
        const vocabHints = buildVocabularyHints(freshFolders, freshTags);

        const config: LiveTranscriptionConfig = {
          silence_duration_ms: settings.silenceDurationMs,
          max_chunk_seconds: settings.maxChunkSeconds,
          backfill_seconds: backfillSeconds ?? 0,
          source: settings.captureSource,
          mix_config:
            settings.captureSource === "Mixed" ? settings.mixConfig : null,
          language: settings.language,
          prompt_context_chars: settings.promptContextChars,
          prompt_decay_silence_seconds:
            settings.promptDecaySilenceSeconds > 0
              ? settings.promptDecaySilenceSeconds
              : null,
          session_id: sessionId,
          audio_save_location: settings.audioSaveLocation,
          audio_export_format: settings.audioExportFormat,
          mp3_bitrate: settings.audioExportFormat === "mp3" ? settings.mp3Bitrate : null,
          diarization:
            settings.selectedEngine === "Parakeet" && settings.diarizationEnabled,
          vocabulary_hints: vocabHints,
          source_kind: "session",
        };

        // Pre-set `backfillActive` before the await so any `backfill-complete`
        // event emitted by the spawned live task during the await transitions
        // it cleanly to false. If we set it post-await instead, a fast or
        // empty-branch backfill-complete could fire before our set, the
        // listener would clear it to false, and our subsequent set would
        // overwrite it back to true — leaving the UI affordance stuck on.
        const requestedBackfill = (backfillSeconds ?? 0) > 0;
        if (requestedBackfill) {
          set({ backfillActive: true });
        }

        const result = await commands.startLiveTranscription(config);
        if (result.status === "error") {
          // Clean up the DB row we just created
          await dbDeleteSession(sessionId).catch(() => {});
          if (requestedBackfill) set({ backfillActive: false });
          throw new Error(commandErrorMessage(result.error));
        }

        trackSessionCreated({
          source: settings.captureSource,
          backfill_seconds: backfillSeconds ?? 0,
          trigger: trigger ?? "unknown",
        });

        // Note: `backfillActive` deliberately omitted — the pre-await set
        // above plus the `backfill-complete` listener are the source of
        // truth. Re-asserting it here would re-introduce the race.
        set({
          activeSessionId: sessionId,
          activeSessionSegments: [],
          activeSessionParts: [],
          activeSessionStartTime: result.data.effective_start_epoch_ms,
          liveTranscriptionActive: true,
          livePhase: "Running",
          currentView: "note-detail",
          selectedSessionId: sessionId,
          // Initialize Current Insight from the persisted Default at session
          // start. See `recoverActiveSession` for the same pattern.
          currentInsightId: get().settings.insights.defaultInsightId,
          // backfillActive intentionally omitted — pre-set before the await
          // (and listener-driven thereafter) so a fast `backfill-complete`
          // event arriving during the await isn't overwritten back to true.
          backfillBoundarySeconds: null,
        });

        // Reload sidebar
        const sessions = await listSessions();
        set({ sessions });
      },

      resumeSession: async (sessionId: string) => {
        const {
          settings,
          enginePhase,
          captureStatus,
          activeSessionId,
          liveTranscriptionActive,
          sessionStopping,
        } = get();

        if (liveTranscriptionActive || sessionStopping || activeSessionId) {
          toast.error("Another session or dictation is already running.");
          return;
        }
        if (enginePhase !== "ready") {
          toast.error("Transcription engine is not ready.");
          return;
        }
        if (captureStatus?.state !== "Capturing") {
          toast.error("Audio capture is not active.");
          return;
        }

        const session = await getSession(sessionId).catch(() => null);
        if (!session) {
          toast.error("Could not load session.");
          return;
        }
        // Defense-in-depth against the resume race (LIVE_SESSION_STATE resume-race
        // transition). The `status !== 'completed'` refusal below is the operative
        // guard and is strictly stronger — once a foreign device's `recording` write
        // has synced, that check already refuses. This belt-and-suspenders adds
        // nothing to the race window but yields a TRUTHFUL "live on <label>" message
        // instead of the generic refusal when a fresh foreign owner holds the session.
        const myFingerprint = get().syncStatus?.deviceFingerprint ?? null;
        const owner = session.recording_device_id;
        if (session.status === "recording" && owner && owner !== myFingerprint) {
          const freshCheckSegments = await getSessionSegments(sessionId).catch(
            () => [],
          );
          if (!isRecordingStale(session, freshCheckSegments)) {
            const label =
              get().syncStatus?.roster.find((r) => r.fingerprint === owner)
                ?.label ?? "another device";
            toast.error(`This session is live on ${label}.`);
            return;
          }
          // A stale foreign owner falls through to the generic refusal below (v1 does
          // not take over a session recorded elsewhere — see Non-goals).
        }
        if (session.status !== "completed") {
          toast.error("This session is not in a state that can be resumed.");
          return;
        }
        if (session.session_type === "manual") {
          toast.error("Manual notes can't be resumed as recordings.");
          return;
        }

        const [freshFolders, freshTags, existingSegments, existingParts] = await Promise.all([
          listFolders(),
          listTags(),
          getSessionSegments(sessionId),
          listSessionAudioParts(sessionId),
        ]);

        if (existingParts.length === 0) {
          toast.error("This session has no audio to resume from.");
          return;
        }

        const offsetBaseSeconds = existingParts.reduce(
          (acc, p) => acc + p.duration_seconds,
          0,
        );
        const nextPartIndex = existingParts.length;
        const vocabHints = buildVocabularyHints(freshFolders, freshTags);

        const config: LiveTranscriptionConfig = {
          silence_duration_ms: settings.silenceDurationMs,
          max_chunk_seconds: settings.maxChunkSeconds,
          backfill_seconds: 0,
          source: settings.captureSource,
          mix_config: settings.captureSource === "Mixed" ? settings.mixConfig : null,
          language: settings.language,
          prompt_context_chars: settings.promptContextChars,
          prompt_decay_silence_seconds:
            settings.promptDecaySilenceSeconds > 0
              ? settings.promptDecaySilenceSeconds
              : null,
          session_id: sessionId,
          audio_save_location: settings.audioSaveLocation,
          audio_export_format: settings.audioExportFormat,
          mp3_bitrate:
            settings.audioExportFormat === "mp3" ? settings.mp3Bitrate : null,
          diarization:
            settings.selectedEngine === "Parakeet" && settings.diarizationEnabled,
          vocabulary_hints: vocabHints,
          resume: {
            part_index: nextPartIndex,
            offset_base_seconds: offsetBaseSeconds,
          },
          source_kind: "session",
        };

        const result = await commands.startLiveTranscription(config);
        if (result.status === "error") {
          // No DB rollback needed — we deferred the status flip until after
          // backend success.
          toast.error(`Could not resume session: ${commandErrorMessage(result.error)}`);
          return;
        }

        // Status flip happens *after* the backend has accepted the start, so a
        // rejected resume leaves the session as `completed`.
        // Re-attribute to THIS device — a resumed session re-owns to whoever resumed
        // it (LIVE_SESSION_STATE D2). Null when sync is not configured.
        await dbMarkSessionRecording(
          sessionId,
          get().syncStatus?.deviceFingerprint ?? null,
        ).catch((e) => console.error("Failed to flip session to recording:", e));

        // Sidebar's `isRecording` keys off `activeSessionId`, so we don't
        // need to reload the sessions list — the badge updates from
        // activeSessionId alone.
        set({
          activeSessionId: sessionId,
          activeSessionSegments: existingSegments,
          activeSessionParts: existingParts,
          activeSessionStartTime: result.data.effective_start_epoch_ms,
          liveTranscriptionActive: true,
          livePhase: "Running",
          currentView: "note-detail",
          selectedSessionId: sessionId,
          sessionStopping: false,
          // Initialize Current Insight from the persisted Default at session
          // resume. See `recoverActiveSession` for the same pattern.
          currentInsightId: get().settings.insights.defaultInsightId,
          backfillActive: false,
          backfillBoundarySeconds: null,
        });
      },

      stopActiveSession: async () => {
        const { activeSessionId, sessionStopping } = get();
        if (!activeSessionId || sessionStopping) return;

        // Flip the UI immediately — the Rust command returns right away but
        // the loop still needs to drain the current chunk and finalize the
        // WAV before emitting Stopped. Users shouldn't see the stop button
        // appear to hang during that window.
        set({ sessionStopping: true });

        try {
          await commands.stopLiveTranscription("session");
        } catch (e) {
          console.error("Failed to stop session:", e);
          set({ sessionStopping: false });
        }
      },

      openSession: async (id: string) => {
        const { activeSessionId } = get();

        if (id === activeSessionId) {
          set({
            currentView: "note-detail",
            selectedSessionId: id,
          });
          return;
        }

        try {
          const [session, segments, parts] = await Promise.all([
            getSession(id),
            getSessionSegments(id),
            listSessionAudioParts(id),
          ]);
          set({
            currentView: "note-detail",
            selectedSessionId: id,
            viewSession: session,
            viewSessionSegments: segments,
            viewSessionParts: parts,
          });
        } catch (e) {
          console.error("Failed to open session:", e);
          toast.error("Failed to open session");
        }
      },

      deleteSession: async (id: string) => {
        const { activeSessionId, selectedSessionId } = get();

        // Can't delete active recording session
        if (id === activeSessionId) return;

        try {
          // Read part paths *before* the cascade deletes them. Without this
          // ordering, the file cleanup would race the row delete and the
          // helper would fall back to the legacy current-dir glob — orphaning
          // any parts saved to a different `audioSaveLocation`.
          let partPaths: string[] = [];
          try {
            partPaths = (await listSessionAudioParts(id)).map((p) => p.file_path);
          } catch {
            partPaths = [];
          }
          await dbDeleteSession(id);
          await deleteSessionAudio(id, partPaths, get().settings.audioSaveLocation);
          await clearDictationSessionLink(id);
          trackSessionDeleted();
          const sessions = await listSessions();
          const { [id]: _, ...restMap } = get().sessionFolderMap;
          const { [id]: _t, ...restTagMap } = get().sessionTagMap;

          // Refresh dictation history if viewing dictation list
          if (get().listFilter.type === "dictation") {
            get().loadDictationHistory();
          }

          if (selectedSessionId === id) {
            set({
              sessions,
              sessionFolderMap: restMap,
              sessionTagMap: restTagMap,
              currentView: "note-list",
              selectedSessionId: null,
              viewSession: null,
              viewSessionSegments: [],
            });
          } else {
            set({ sessions, sessionFolderMap: restMap, sessionTagMap: restTagMap });
          }
        } catch (e) {
          console.error("Failed to delete session:", e);
          toast.error("Failed to delete session");
        }
      },

      // Escape hatch (LIVE_SESSION_STATE.md Q1): finalize a foreign-and-stale
      // 'recording' row the owner-only boot sweep will never touch (dead device or
      // same-hardware re-pair orphan). The DB write is a plain LWW status flip
      // (no destructive recompute — see db.markSessionCompleted). Reload the list +
      // the open view row so the flip renders immediately.
      markSessionCompleted: async (id: string) => {
        try {
          await dbMarkSessionCompleted(id);
          const sessions = await listSessions();
          set({ sessions });
          if (get().selectedSessionId === id) {
            const row = await getSession(id);
            if (row) set({ viewSession: row });
          }
        } catch (e) {
          console.error("Failed to mark session completed:", e);
          toast.error("Failed to mark session completed");
        }
      },

      navigateTo: (view, sessionId) => {
        set({
          currentView: view,
          selectedSessionId: sessionId ?? null,
        });
      },

      settingsRequest: null,
      setSettingsRequest: (request) => {
        set({ settingsRequest: request });
      },

      setSyncConfig: (partial) => {
        set({ syncConfig: { ...get().syncConfig, ...partial } });
      },
      setSyncStatus: (status) => {
        set({ syncStatus: status });
        // Mirror the non-secret handles the UI persists. Never store tokens or
        // the vault key here — those stay in the OS keychain (Rust side).
        //
        // Non-destructive mirror: a signed-out / disconnected status DTO returns
        // serverUrl:"" and email:null (Rust `sync_status`). Blindly mirroring those into
        // the PERSISTED syncConfig meant one signed-out poll wiped the relay URL the user
        // configured (and their saved email) — so a later boot lost the relay entirely.
        // Only mirror MEANINGFUL values: never clobber a non-empty saved serverUrl with an
        // empty one, and never null a persisted email from a signed-out DTO. Sign-out
        // clears the email deliberately through its OWN path (handleSignOut →
        // setSyncConfig({ email: null })), not through a status poll.
        if (status) {
          const prev = get().syncConfig;
          set({
            syncConfig: {
              ...prev,
              serverUrl: status.serverUrl || prev.serverUrl,
              email: status.email ?? prev.email,
              syncEnabled: status.syncEnabled,
              deviceFingerprint: status.deviceFingerprint,
            },
          });
        }
      },
      refreshSyncStatus: async () => {
        try {
          const status = await syncCommands.status();
          get().setSyncStatus(status);
        } catch (e) {
          // Surface the failure verbatim; do NOT auto-route or fall back. A
          // broken connection is the user's to fix (feedback_surface_ai_errors).
          const msg = e instanceof Error ? e.message : String(e);
          console.error("Failed to fetch sync status:", msg);
          // R3: drain-level connectivity is now surfaced by the backend as a
          // typed phase=="unreachable" on a successful status fetch (the drain
          // classifies connect/timeout at the transport). This catch is only for
          // a failed status COMMAND itself, so we still surface it verbatim as an
          // error and never fabricate `relayConn = unreachable` by string-parsing.
          const prev = get().syncStatus;
          set({
            syncStatus: {
              phase: "error",
              serverUrl: get().syncConfig.serverUrl,
              email: get().syncConfig.email,
              deviceFingerprint: get().syncConfig.deviceFingerprint,
              roster: prev?.roster ?? [],
              vaultKeyEpoch: prev?.vaultKeyEpoch ?? null,
              rosterFingerprint: prev?.rosterFingerprint ?? null,
              syncEnabled: get().syncConfig.syncEnabled,
              lastError: msg,
              billingUrl: prev?.billingUrl ?? null,
              // Preserve the last-known progress snapshot; a status-fetch failure
              // is a connection problem, not evidence the backlog changed (T024).
              pendingEntries: prev?.pendingEntries ?? 0,
              pendingBytes: prev?.pendingBytes ?? 0,
              ackedThisSession: prev?.ackedThisSession ?? 0,
              lastSuccess: prev?.lastSuccess ?? null,
              pullBehind: prev?.pullBehind ?? 0,
              // Audio lane: preserve the last-known counts across a status-fetch blip.
              audioUploadOutstanding: prev?.audioUploadOutstanding ?? 0,
              audioBackfillOutstanding: prev?.audioBackfillOutstanding ?? 0,
              audioUploadFailed: prev?.audioUploadFailed ?? 0,
              audioUploadedTotal: prev?.audioUploadedTotal ?? 0,
              audioBackfillComplete: prev?.audioBackfillComplete ?? false,
            },
          });
        }
      },

      probeRelay: async (url: string) => {
        const token = ++relayProbeToken;
        set({ relayConn: { kind: "testing" } });
        try {
          const ok = await syncCommands.probe(url);
          // Single-flight: a newer probe (or a resetProbe) superseded us while
          // the request was in flight — discard this now-stale result.
          if (token !== relayProbeToken) return;
          set({
            relayConn: {
              kind: "ok",
              engineVersion: ok.engineVersion,
              protocolVersion: ok.protocolVersion,
              latencyMs: ok.latencyMs,
              normalizedUrl: ok.normalizedUrl,
              versionAdvisory: ok.versionAdvisory,
            },
          });
          // Test-and-save (plan §0.1): persist the server's normalized URL on a
          // successful probe — but ONLY when signed out. A signed-in device's
          // URL is locked (changing it signs the device out; that's an explicit
          // AlertDialog flow in T027), so never mutate it from an ambient probe.
          const signedIn = get().syncConfig.email != null;
          if (!signedIn) {
            get().setSyncConfig({ serverUrl: ok.normalizedUrl });
          }
        } catch (e) {
          if (token !== relayProbeToken) return;
          // The T025 contract rejects with a tagged RelayProbeError; map it
          // straight through (verbatim `raw` preserved). Any non-tagged throw
          // (e.g. the sync feature not built) is treated as a connection-level
          // failure with its message surfaced verbatim — never swallowed.
          const err = e as Partial<RelayProbeError> | undefined;
          if (
            err &&
            typeof err === "object" &&
            (err.kind === "unreachable" ||
              err.kind === "tls-error" ||
              err.kind === "not-a-relay") &&
            typeof err.raw === "string"
          ) {
            set({ relayConn: { kind: err.kind, raw: err.raw } });
          } else {
            const raw = e instanceof Error ? e.message : String(e);
            set({ relayConn: { kind: "unreachable", raw } });
          }
        }
      },

      resetProbe: () => {
        // Bump the token so any in-flight probe's response is discarded
        // (cancel-on-edit), then return health to idle.
        relayProbeToken++;
        set({ relayConn: { kind: "idle" } });
      },

      setListFilter: (filter) => {
        set({ listFilter: filter });
      },

      refreshDevices: async () => {
        try {
          const result = await commands.listAudioDevices();
          if (result.status === "ok") {
            get().applyDeviceList(result.data);
          }
        } catch (e) {
          console.error("Failed to refresh devices:", e);
        }
      },

      applyDeviceList: (devices) => {
        const prevSelectedId = get().settings.selectedMicDeviceId;
        set({ devices });

        // Reconcile selection. Only inputs are eligible for the mic.
        if (prevSelectedId == null) return;
        const stillPresent = devices.some(
          (d) => d.id === prevSelectedId && d.device_type === "Input",
        );
        if (stillPresent) return;

        // The persisted mic is gone. Drop to "follow default" and tell
        // the user which device they're now on. The toast id dedupes if
        // the broker fires several `devices-changed` in quick succession
        // (e.g. during a bluetooth disconnect storm).
        const newDefault = devices.find(
          (d) => d.is_default && d.device_type === "Input",
        );
        set((state) => ({
          settings: { ...state.settings, selectedMicDeviceId: null },
        }));
        toast.info(
          newDefault
            ? `Selected microphone disappeared — using ${newDefault.name}`
            : "Selected microphone disappeared — using system default",
          { id: "mic-disappeared" },
        );
      },

      refreshModels: async () => {
        try {
          const result = await commands.getAvailableModels();
          if (result.status === "ok") {
            set({ models: result.data });
          }
        } catch (e) {
          console.error("Failed to refresh models:", e);
        }
      },

      autoSetup: async () => {
        // HMR-preserved Zustand store already holds "ready" when the backend
        // is warm — skip the whole setup path. On fresh app start enginePhase
        // is "idle", so this falls through to full init. Backend commands are
        // idempotent (startCapture treats AlreadyRunning as Ok,
        // init_transcription_client short-circuits on is_some()), so any
        // duplicate calls are harmless.
        if (get().enginePhase === "ready") return;

        // The webview boots concurrently with the Rust setup hook, so a fast
        // frontend can invoke commands whose managed state doesn't exist yet
        // ("state not managed" — observed on the Windows canary). Wait for
        // the backend's ready flag before the first engine command; the flag
        // itself is builder-managed, so this poll can never race.
        await waitForBackendReady();

        const { settings } = get();

        // Request Screen Recording TCC up front — on macOS 14.2+ this gates
        // the Core Audio tap cpal uses for system audio loopback. Fire the
        // prompt before startCapture so users see it during onboarding instead
        // of later, silently failing.
        commands.requestScreenCapturePermission().catch(() => {});

        // Track 1 — Start capture (fire and forget, errors surface via capture-status events)
        commands
          .startCapture(
            settings.selectedMicDeviceId,
            settings.captureSource,
            settings.bufferMaxSeconds,
          )
          .then((result) => {
            if (result.status === "error") {
              commands.getCaptureStatus().then((r) => {
                if (r.status === "ok") get().setCaptureStatus(r.data);
              });
            }
          })
          .catch(() => {});

        // Track 2 — Engine setup
        try {
          // Load devices + catalogue in parallel (non-blocking)
          get()
            .refreshDevices()
            .catch((e) => console.error("Failed to refresh devices:", e));
          get()
            .loadEngineCatalogue()
            .catch((e) => console.error("Failed to load catalogue:", e));

          if (settings.selectedEngine === "Parakeet") {
            // Parakeet path
            //
            // Variant resolution is driven by the backend's
            // `recommended_for_host()` (cfg-driven, deterministic per
            // machine). For the post-update upgrade case — Apple Silicon
            // users sitting on `TdtV3` because that was the only variant
            // pre-int8 — we want to download `TdtV3Int8` AND keep the old
            // bundle as a safety net so an offline launch / dropped
            // download doesn't strand the user with a working model on
            // disk that we ignored.
            //
            // Order: refresh state first, then decide which variant to
            // load this launch.
            await get().refreshParakeetModels();
            await get().refreshSortformerStatus();

            const recommendedRes =
              await commands.getRecommendedParakeetVariant();
            if (recommendedRes.status === "error") {
              console.error(
                "getRecommendedParakeetVariant failed; falling back to selected variant:",
                commandErrorMessage(recommendedRes.error),
              );
            }
            const recommended =
              recommendedRes.status === "ok" ? recommendedRes.data : null;

            const downloadedVariants = get().parakeetModels.filter(
              (m) => m.downloaded,
            );
            const recommendedDownloaded =
              recommended &&
              downloadedVariants.some((m) => m.variant === recommended);
            // Any non-recommended variant that's already on disk — the
            // user's "previous bundle" if we're mid-upgrade. Used as the
            // fallback target when a recommended-variant download fails.
            const legacyFallback =
              downloadedVariants.find((m) => m.variant !== recommended)
                ?.variant ?? null;

            let variant = settings.selectedParakeetVariant;

            if (recommendedDownloaded) {
              // Recommended is already on disk. Use it directly.
              variant = recommended!;
            } else if (recommended) {
              // Need to download the recommended variant. If the user
              // already has another variant downloaded (the upgrade
              // case), surface a one-time toast so they know why a
              // fresh download is happening right after the app update.
              if (legacyFallback) {
                toast.info(
                  "Upgrading Parakeet to the optimized model for your Mac. Downloading…",
                  {
                    id: "parakeet-variant-upgrade",
                    duration: 10000,
                  },
                );
              }
              set({ enginePhase: "downloading", modelDownloadProgress: 0 });
              const dl = await commands.downloadParakeetModel(recommended);
              set({ modelDownloadProgress: null });

              if (dl.status === "error") {
                if (legacyFallback) {
                  // Keep the user working on the bundle they already
                  // have. Surface a warning so they know the upgrade
                  // didn't take and can retry next launch.
                  toast.warning(
                    "Couldn't download the optimized Parakeet model. Continuing with the existing bundle.",
                    {
                      id: "parakeet-variant-upgrade",
                      duration: 8000,
                    },
                  );
                  variant = legacyFallback;
                } else {
                  // No fallback on disk → first-install error path.
                  trackEngineError({
                    error: commandErrorMessage(dl.error),
                    phase: "downloading",
                  });
                  set({
                    enginePhase: "error",
                    engineError: commandErrorMessage(dl.error),
                  });
                  return;
                }
              } else {
                await get().refreshParakeetModels();
                variant = recommended;
              }
            }

            // Persist whichever variant we ended up using so StatusPopover,
            // pressure-event tagging, and the next autoSetup pass all see
            // a coherent view.
            if (variant !== settings.selectedParakeetVariant) {
              get().updateSettings({ selectedParakeetVariant: variant });
            }

            // Sortformer download is lazy — only when diarization is on.
            if (
              settings.diarizationEnabled &&
              !get().sortformerStatus?.downloaded
            ) {
              set({ modelDownloadProgress: 0 });
              const dl = await commands.downloadSortformerModel("V2_1");
              if (dl.status === "error") {
                console.error(
                  "sortformer download failed; continuing without diarization:",
                  commandErrorMessage(dl.error),
                );
              }
              set({ modelDownloadProgress: null });
              await get().refreshSortformerStatus();
            }

            set({ enginePhase: "initializing" });
            const initResult = await commands.initTranscriptionClient(
              "Parakeet",
              null,
              variant,
              settings.diarizationEnabled,
            );
            if (initResult.status === "error") {
              trackEngineError({ error: commandErrorMessage(initResult.error), phase: "initializing" });
              set({ enginePhase: "error", engineError: commandErrorMessage(initResult.error) });
              return;
            }
            set({ enginePhase: "ready", engineError: null });
            return;
          }

          // Whisper path (default).
          const modelsResult = await commands.getAvailableModels();
          if (modelsResult.status === "ok") {
            set({ models: modelsResult.data });
          }

          const models = get().models;
          const selectedModel = models.find(
            (m) => m.size === settings.selectedModelSize,
          );

          if (!selectedModel?.downloaded) {
            set({ enginePhase: "downloading", modelDownloadProgress: 0 });
            const downloadResult = await commands.downloadModel(
              settings.selectedModelSize,
            );
            if (downloadResult.status === "error") {
              trackEngineError({ error: commandErrorMessage(downloadResult.error), phase: "downloading" });
              set({
                enginePhase: "error",
                engineError: commandErrorMessage(downloadResult.error),
                modelDownloadProgress: null,
              });
              return;
            }
            set({ modelDownloadProgress: null });
            await get().refreshModels();
          }

          set({ enginePhase: "initializing" });
          const initResult = await commands.initTranscriptionClient(
            "Whisper",
            settings.selectedModelSize,
            null,
            false,
          );
          if (initResult.status === "error") {
            trackEngineError({ error: commandErrorMessage(initResult.error), phase: "initializing" });
            set({ enginePhase: "error", engineError: commandErrorMessage(initResult.error) });
            return;
          }

          set({ enginePhase: "ready", engineError: null });
        } catch (e) {
          // console.error routes through the frontend log bridge, so the
          // failure lands in the unified log file — engine setup errors on
          // a user's machine must be diagnosable from logs alone.
          console.error("engine autoSetup failed:", e);
          set({ enginePhase: "error", engineError: String(e) });
        }
      },

      downloadModel: async (size: ModelSizeDto) => {
        set({ modelDownloadProgress: 0 });
        try {
          const result = await commands.downloadModel(size);
          if (result.status === "error") {
            throw new Error(commandErrorMessage(result.error));
          }
          trackModelDownloaded({ model_size: size });
          await get().refreshModels();
        } finally {
          set({ modelDownloadProgress: null });
        }
      },

      deleteModel: async (size: ModelSizeDto) => {
        const result = await commands.deleteModel(size);
        if (result.status === "error") {
          throw new Error(commandErrorMessage(result.error));
        }
        trackModelDeleted({ model_size: size });
        await get().refreshModels();
      },

      switchModel: async (size: ModelSizeDto) => {
        const { models, settings: { selectedModelSize: fromSize } } = get();
        const model = models.find((m) => m.size === size);

        try {
          // Download if needed
          if (!model?.downloaded) {
            set({ enginePhase: "downloading", modelDownloadProgress: 0 });
            const downloadResult = await commands.downloadModel(size);
            if (downloadResult.status === "error") {
              throw new Error(commandErrorMessage(downloadResult.error));
            }
            set({ modelDownloadProgress: null });
            await get().refreshModels();
          }

          // Shutdown current engine
          set({ enginePhase: "initializing" });
          set({ loadedEngineInfo: null });
          await commands.shutdownTranscriptionClient();

          // Init with new model
          const initResult = await commands.initTranscriptionClient(
            "Whisper",
            size,
            null,
            false,
          );
          if (initResult.status === "error") {
            throw new Error(commandErrorMessage(initResult.error));
          }

          trackModelSwitched({ model_size: size, from_size: fromSize });
          set({
            enginePhase: "ready",
            engineError: null,
          });
          get().updateSettings({ selectedModelSize: size });
        } catch (e) {
          set({ enginePhase: "error", engineError: String(e) });
        }
      },

      // ---------- Engine + Parakeet/Sortformer actions ----------

      loadEngineCatalogue: async () => {
        try {
          const r = await commands.getEngineCatalogue();
          if (r.status === "ok") set({ engineCatalogue: r.data });
        } catch (e) {
          console.error("Failed to load engine catalogue:", e);
        }
      },

      refreshParakeetModels: async () => {
        try {
          const r = await commands.getParakeetModels();
          if (r.status === "ok") set({ parakeetModels: r.data });
        } catch (e) {
          console.error("Failed to refresh parakeet models:", e);
        }
      },

      refreshSortformerStatus: async () => {
        try {
          const r = await commands.getSortformerStatus();
          if (r.status === "ok") set({ sortformerStatus: r.data });
        } catch (e) {
          console.error("Failed to refresh sortformer status:", e);
        }
      },

      switchEngine: async (engine: EngineKindDto) => {
        const settings = get().settings;
        if (settings.selectedEngine === engine) return;
        const previousEngine = settings.selectedEngine;

        // Flip the UI to the target engine up front so any download shows
        // progress in the same model row the user would see after the switch
        // completes — no top-of-screen banner, no cross-engine flicker.
        get().updateSettings({ selectedEngine: engine });

        try {
          if (engine === "Parakeet") {
            await get().refreshParakeetModels();
            // Prefer a variant already on disk (host-recommended first),
            // else download the host-recommended one. Using the persisted
            // selection alone downloaded fp32 TdtV3 (2.5 GB, unusably slow
            // on CPU) on hosts whose recommendation is int8.
            const recommendedRes =
              await commands.getRecommendedParakeetVariant();
            const recommended =
              recommendedRes.status === "ok" ? recommendedRes.data : null;
            const downloadedVariants = get().parakeetModels.filter(
              (m) => m.downloaded,
            );
            const variant =
              downloadedVariants.find((m) => m.variant === recommended)
                ?.variant ??
              downloadedVariants[0]?.variant ??
              recommended ??
              settings.selectedParakeetVariant;
            if (variant !== settings.selectedParakeetVariant) {
              get().updateSettings({ selectedParakeetVariant: variant });
            }
            const ready = get().parakeetModels.find(
              (m) => m.variant === variant && m.downloaded,
            );
            if (!ready) {
              set({ modelDownloadProgress: 0 });
              const dl = await commands.downloadParakeetModel(variant);
              if (dl.status === "error") {
                set({ modelDownloadProgress: null });
                throw new Error(commandErrorMessage(dl.error));
              }
              set({ modelDownloadProgress: null });
              await get().refreshParakeetModels();
            }
            if (settings.diarizationEnabled) {
              await get().refreshSortformerStatus();
              if (!get().sortformerStatus?.downloaded) {
                set({ modelDownloadProgress: 0 });
                const dl = await commands.downloadSortformerModel("V2_1");
                if (dl.status === "error") {
                  console.error(
                    "sortformer download failed; continuing without diarization:",
                    commandErrorMessage(dl.error),
                  );
                }
                set({ modelDownloadProgress: null });
                await get().refreshSortformerStatus();
              }
            }
          } else if (engine === "Whisper") {
            await get().refreshModels();
            const ready = get().models.find(
              (m) => m.size === settings.selectedModelSize && m.downloaded,
            );
            if (!ready) {
              set({ modelDownloadProgress: 0 });
              const dl = await commands.downloadModel(
                settings.selectedModelSize,
              );
              if (dl.status === "error") {
                set({ modelDownloadProgress: null });
                throw new Error(commandErrorMessage(dl.error));
              }
              set({ modelDownloadProgress: null });
              await get().refreshModels();
            }
          }

          set({ enginePhase: "initializing" });
          set({ loadedEngineInfo: null });
          await commands.shutdownTranscriptionClient();

          const initResult = await commands.initTranscriptionClient(
            engine,
            engine === "Whisper" ? settings.selectedModelSize : null,
            // Re-read: the Parakeet branch above may have re-resolved the
            // variant to the host recommendation and persisted it.
            engine === "Parakeet" ? get().settings.selectedParakeetVariant : null,
            engine === "Parakeet" ? settings.diarizationEnabled : false,
          );
          if (initResult.status === "error") {
            throw new Error(commandErrorMessage(initResult.error));
          }

          set({ enginePhase: "ready", engineError: null });
        } catch (e) {
          get().updateSettings({ selectedEngine: previousEngine });
          set({ enginePhase: "error", engineError: String(e) });
          throw e;
        }
      },

      downloadParakeetModel: async (variant: ParakeetVariantDto) => {
        set({ modelDownloadProgress: 0 });
        try {
          const result = await commands.downloadParakeetModel(variant);
          if (result.status === "error") {
            throw new Error(commandErrorMessage(result.error));
          }
          await get().refreshParakeetModels();
        } finally {
          set({ modelDownloadProgress: null });
        }
      },

      setSpeakerName: (sessionId: string, speakerId: number, name: string) => {
        const trimmed = name.trim();
        const current = get().settings.speakerNames[sessionId]?.[speakerId];
        if ((current ?? "") === trimmed) return;
        set((state) => {
          const next = { ...state.settings.speakerNames };
          const sessionMap = { ...(next[sessionId] ?? {}) };
          if (trimmed === "") {
            delete sessionMap[speakerId];
          } else {
            sessionMap[speakerId] = trimmed;
          }
          if (Object.keys(sessionMap).length === 0) {
            delete next[sessionId];
          } else {
            next[sessionId] = sessionMap;
          }
          return {
            settings: { ...state.settings, speakerNames: next },
          };
        });
      },


      deleteParakeetModel: async (variant: ParakeetVariantDto) => {
        const result = await commands.deleteParakeetModel(variant);
        if (result.status === "error") {
          throw new Error(commandErrorMessage(result.error));
        }
        await get().refreshParakeetModels();
      },

      setDiarizationEnabled: async (enabled: boolean) => {
        // Diarization is intentionally locked off pending session-stable
        // speaker IDs. Sortformer::diarize() resets state per call, so the
        // speaker_id values it returns are chunk-local — the same person
        // can flip between Speaker 0 / Speaker 1 across chunk boundaries,
        // and the transcript UI groups + renames by that numeric id. The
        // entire IPC + sidecar + DB plumbing stays in place so re-enabling
        // is a one-line change once the chunk-local issue is resolved
        // (streaming Sortformer state, post-session pass on the full WAV,
        // or an embedding-based session registry — see the doc comment on
        // ParakeetBackend::run_diarization).
        //
        // Force enabled = false. We never persist `true` from this path.
        // Any caller that explicitly tries to enable gets a clear error.
        if (enabled) {
          throw new Error(
            "Speaker diarization is currently disabled — chunk-local speaker " +
              "IDs cause unstable labels across the session. Re-enable will " +
              "land once session-stable IDs are wired up.",
          );
        }
        const settings = get().settings;
        if (!settings.diarizationEnabled) return;

        get().updateSettings({ diarizationEnabled: false });

        if (settings.selectedEngine === "Parakeet") {
          set({ enginePhase: "initializing" });
          set({ loadedEngineInfo: null });
          await commands.shutdownTranscriptionClient();
          const r = await commands.initTranscriptionClient(
            "Parakeet",
            null,
            settings.selectedParakeetVariant,
            false,
          );
          if (r.status === "error") {
            set({ enginePhase: "error", engineError: commandErrorMessage(r.error) });
            throw new Error(commandErrorMessage(r.error));
          }
          set({ enginePhase: "ready", engineError: null });
        }
      },

      updateSettings: (partial) => {
        set((state) => ({
          settings: { ...state.settings, ...partial },
        }));

        const trackedKeys = [
          "captureSource", "theme", "language", "silenceDurationMs",
          "maxChunkSeconds", "promptContextChars",
          "promptDecaySilenceSeconds", "bufferMaxSeconds", "showRecordingIndicator",
        ] as const;
        for (const key of trackedKeys) {
          if (key in partial) {
            trackSettingChanged({ setting_name: key, new_value: String(partial[key]) });
          }
        }

        const needsRestart =
          partial.captureSource !== undefined ||
          partial.selectedMicDeviceId !== undefined ||
          partial.bufferMaxSeconds !== undefined;

        // Don't restart capture during an active live transcription session —
        // it would corrupt the in-progress recording.
        if (
          needsRestart &&
          get().captureStatus?.state === "Capturing" &&
          get().activeSessionId === null
        ) {
          const next = get().settings;
          commands
            .stopCapture()
            .then(() =>
              commands.startCapture(
                next.selectedMicDeviceId,
                next.captureSource,
                next.bufferMaxSeconds,
              ),
            )
            .catch((e) => {
              console.error("Failed to restart capture after settings change:", e);
              toast.error("Failed to restart capture");
            });
        }
      },

      clearAllSessions: async () => {
        if (get().activeSessionId) {
          toast.error("Cannot clear sessions while recording is active");
          return;
        }
        try {
          // One DB read of all parts paths beats N round-trips through
          // `listSessionAudioParts(session.id)`. Hand the full list to the
          // backend in a single IPC; it validates per-path and skips
          // unknown locations. Sessions with zero parts (manual notes,
          // legacy single-file dictations) still need the glob cleanup.
          const audioSaveLocation = get().settings.audioSaveLocation;
          let allPaths: string[] = [];
          try {
            allPaths = await listAllAudioPartPaths();
          } catch (e) {
            console.error("Failed to list audio parts before clear:", e);
          }
          // Capture session ids while the table still has them.
          const sessionIds = get().sessions.map((s) => s.id);
          await deleteAllSessions();
          if (allPaths.length > 0) {
            try {
              await commands.deleteAudioFiles(allPaths);
            } catch (e) {
              console.error("Failed to delete bulk audio files:", e);
              toast.error("Some audio files were not deleted");
            }
          }
          // Legacy single-file dictations: best-effort, awaited so a
          // failure surfaces. Run sequentially to keep error reporting
          // sane — clear is not latency-critical.
          for (const id of sessionIds) {
            try {
              await commands.deleteSessionWav(id, audioSaveLocation);
            } catch (e) {
              console.error(`Failed to delete legacy WAV for ${id}:`, e);
            }
          }
          trackSessionsCleared();
          set({
            sessions: [],
            sessionFolderMap: {},
            sessionTagMap: {},
            currentView: "note-list",
            selectedSessionId: null,
            viewSession: null,
            viewSessionSegments: [],
          });
        } catch (e) {
          console.error("Failed to clear all sessions:", e);
          toast.error("Failed to clear sessions");
        }
      },

      // Folder actions
      createFolder: async (name: string, parentId?: string | null, icon?: string | null, color?: string | null, description?: string | null) => {
        try {
          const id = crypto.randomUUID();
          await dbCreateFolder(id, name, parentId ?? null, icon ?? null, color ?? null, description ?? null);
          trackFolderCreated();
          const folders = await listFolders();
          set({ folders, ...deriveFolderState(folders) });
        } catch (e) {
          console.error("Failed to create folder:", e);
          toast.error("Failed to create folder");
        }
      },

      updateFolder: async (id: string, updates: { name?: string; icon?: string | null; color?: string | null; description?: string | null }) => {
        try {
          await dbUpdateFolder(id, updates);
          const folders = await listFolders();
          set({ folders, ...deriveFolderState(folders) });
        } catch (e) {
          console.error("Failed to update folder:", e);
          toast.error("Failed to update folder");
        }
      },

      deleteFolder: async (id: string) => {
        try {
          await dbDeleteFolder(id);
          const folders = await listFolders();
          const { listFilter } = get();
          const newFilter =
            listFilter.type === "folder" && listFilter.folderId === id
              ? { type: "all" as const }
              : listFilter;
          set({ folders, listFilter: newFilter, ...deriveFolderState(folders) });
          await get().loadSessionFolders();
        } catch (e) {
          console.error("Failed to delete folder:", e);
          toast.error("Failed to delete folder");
        }
      },

      moveFolder: async (folderId: string, newParentId: string | null) => {
        try {
          await dbUpdateFolderParent(folderId, newParentId);
          const folders = await listFolders();
          set({ folders, ...deriveFolderState(folders) });
        } catch (e) {
          console.error("Failed to move folder:", e);
          toast.error("Failed to move folder");
        }
      },

      reorderFolders: async (folderId: string, overFolderId: string) => {
        try {
          const { folders } = get();
          const dragged = folders.find((f) => f.id === folderId);
          const over = folders.find((f) => f.id === overFolderId);
          if (!dragged || !over || dragged.parent_id !== over.parent_id) return;

          // Get siblings sorted by current sort_order then name
          const siblings = folders
            .filter((f) => f.parent_id === dragged.parent_id)
            .sort((a, b) => a.sort_order - b.sort_order || a.name.localeCompare(b.name));

          const oldIndex = siblings.findIndex((f) => f.id === folderId);
          const newIndex = siblings.findIndex((f) => f.id === overFolderId);
          if (oldIndex === -1 || newIndex === -1 || oldIndex === newIndex) return;

          // Reorder: remove from old position and insert at new
          const reordered = [...siblings];
          const [moved] = reordered.splice(oldIndex, 1);
          reordered.splice(newIndex, 0, moved);

          const updates = reordered.map((f, i) => ({ id: f.id, sort_order: i }));
          await dbReorderFolders(updates);
          const freshFolders = await listFolders();
          set({ folders: freshFolders, ...deriveFolderState(freshFolders) });
        } catch (e) {
          console.error("Failed to reorder folders:", e);
          toast.error("Failed to reorder folders");
        }
      },

      togglePin: async (sessionId: string) => {
        try {
          const wasPinned = get().sessions.find((s) => s.id === sessionId)?.is_pinned;
          await dbTogglePin(sessionId);
          if (wasPinned) trackSessionUnpinned();
          else trackSessionPinned();
          const sessions = await listSessions();
          set({ sessions });
        } catch (e) {
          console.error("Failed to toggle pin:", e);
          toast.error("Failed to toggle pin");
        }
      },

      toggleSessionFolder: async (sessionId: string, folderId: string) => {
        try {
          const { sessionFolderMap, folders } = get();
          const current = sessionFolderMap[sessionId] ?? [];
          const isRemoving = current.includes(folderId);
          if (isRemoving) {
            await dbRemoveSessionFromFolder(sessionId, folderId);
            set({ sessionFolderMap: updateSessionFolderMap(get().sessionFolderMap, sessionId, [], [folderId]) });
          } else {
            const conflicts = await addSessionToFolderWithConflicts(sessionId, folderId, current, folders);
            trackSessionMovedToFolder();
            set({ sessionFolderMap: updateSessionFolderMap(get().sessionFolderMap, sessionId, [folderId], conflicts) });
          }
          const name = get().folders.find(f => f.id === folderId)?.name ?? "folder";
          toast.success(isRemoving ? `Removed from ${name}` : `Added to ${name}`);
        } catch (e) {
          console.error("Failed to toggle session folder:", e);
          toast.error("Failed to update folder");
        }
      },

      addSessionToFolder: async (sessionId: string, folderId: string) => {
        try {
          const { sessionFolderMap, folders } = get();
          const current = sessionFolderMap[sessionId] ?? [];
          const conflicts = await addSessionToFolderWithConflicts(sessionId, folderId, current, folders);
          trackSessionMovedToFolder();
          set({ sessionFolderMap: updateSessionFolderMap(get().sessionFolderMap, sessionId, [folderId], conflicts) });
          const name = get().folders.find(f => f.id === folderId)?.name ?? "folder";
          toast.success(`Added to ${name}`);
        } catch (e) {
          console.error("Failed to add session to folder:", e);
          toast.error("Failed to add to folder");
        }
      },

      removeSessionFromAllFolders: async (sessionId: string) => {
        try {
          await dbRemoveSessionFromAllFolders(sessionId);
          const { [sessionId]: _, ...restMap } = get().sessionFolderMap;
          set({ sessionFolderMap: restMap });
          toast.success("Removed from all folders");
        } catch (e) {
          console.error("Failed to remove from folders:", e);
          toast.error("Failed to remove from folders");
        }
      },

      // Segment editing
      editSegmentText: async (segmentId: string, newText: string) => {
        // Mark the edit in progress so a concurrent D4 sync-applied refresh does not
        // clobber this in-flight write (LIVE_SESSION_STATE.md D4 normative).
        set({ editingSegmentId: segmentId });
        try {
          await dbUpdateSegmentText(segmentId, newText);
          await get().refreshViewSessionSegments();
        } catch (e) {
          console.error("Failed to edit segment:", e);
          toast.error("Failed to edit segment");
        } finally {
          if (get().editingSegmentId === segmentId) {
            set({ editingSegmentId: null });
          }
        }
      },

      deleteSegment: async (segmentId: string) => {
        try {
          await dbSoftDeleteSegment(segmentId);
          await get().refreshViewSessionSegments();
        } catch (e) {
          console.error("Failed to delete segment:", e);
          toast.error("Failed to delete segment");
        }
      },

      toggleSegmentHidden: async (segmentId: string) => {
        try {
          await dbToggleSegmentHidden(segmentId);
          await get().refreshViewSessionSegments();
        } catch (e) {
          console.error("Failed to toggle segment visibility:", e);
          toast.error("Failed to toggle segment visibility");
        }
      },

      toggleSegmentSelected: (segmentId, mode, orderedIds) => {
        const state = get();
        const existing = state.selectedSegmentIds;
        if (mode === "replace") {
          set({
            selectedSegmentIds: new Set([segmentId]),
            lastSelectedSegmentId: segmentId,
          });
          return;
        }
        if (mode === "toggle") {
          const next = new Set(existing);
          if (next.has(segmentId)) next.delete(segmentId);
          else next.add(segmentId);
          set({
            selectedSegmentIds: next,
            lastSelectedSegmentId: segmentId,
          });
          return;
        }
        // range: replace the selection with the inclusive range from the
        // anchor (the last bare/cmd-click pivot) to the clicked segment.
        // The anchor stays fixed across consecutive shift-clicks so the
        // user can grow OR shrink the range — Finder / Gmail / Linear
        // behavior. With no anchor yet (first shift-click after a clear),
        // fall through to a single-item selection and seed the anchor
        // there.
        const anchor = state.lastSelectedSegmentId;
        if (anchor === null) {
          set({
            selectedSegmentIds: new Set([segmentId]),
            lastSelectedSegmentId: segmentId,
          });
          return;
        }
        const a = orderedIds.indexOf(anchor);
        const b = orderedIds.indexOf(segmentId);
        if (a === -1 || b === -1) {
          // Stale anchor (e.g., the anchor segment was deleted) — restart.
          set({
            selectedSegmentIds: new Set([segmentId]),
            lastSelectedSegmentId: segmentId,
          });
          return;
        }
        const [lo, hi] = a < b ? [a, b] : [b, a];
        const next = new Set<string>();
        for (let i = lo; i <= hi; i++) next.add(orderedIds[i]);
        set({ selectedSegmentIds: next });
      },

      setSegmentSelection: (ids) => {
        set({
          selectedSegmentIds: ids,
          lastSelectedSegmentId: ids.size > 0 ? Array.from(ids).pop()! : null,
        });
      },

      clearSegmentSelection: () => {
        if (get().selectedSegmentIds.size === 0) return;
        set({
          selectedSegmentIds: new Set(),
          lastSelectedSegmentId: null,
        });
      },

      setSegmentAnchor: (id) => {
        if (get().lastSelectedSegmentId === id) return;
        set({ lastSelectedSegmentId: id });
      },

      deleteSegments: async (ids) => {
        if (ids.length === 0) return;
        try {
          await dbSoftDeleteSegments(ids);
          set({
            selectedSegmentIds: new Set(),
            lastSelectedSegmentId: null,
          });
          await get().refreshViewSessionSegments();
        } catch (e) {
          console.error("Failed to delete segments:", e);
          toast.error("Failed to delete segments");
        }
      },

      setSegmentsHidden: async (ids, hidden) => {
        if (ids.length === 0) return;
        try {
          await dbSetSegmentsHidden(ids, hidden);
          await get().refreshViewSessionSegments();
        } catch (e) {
          console.error("Failed to update segment visibility:", e);
          toast.error("Failed to update segment visibility");
        }
      },

      setShowHiddenSegments: (show: boolean) => {
        set({ showHiddenSegments: show });
      },

      refreshViewSessionSegments: async () => {
        const { selectedSessionId, activeSessionId } = get();
        if (!selectedSessionId) return;
        try {
          const segments = await getSessionSegments(selectedSessionId);
          // When the selected session is the live one, NoteDetailView reads
          // from activeSessionSegments — refresh that array so context-menu
          // edits/deletes/hides take effect immediately during recording.
          if (selectedSessionId === activeSessionId) {
            set({ activeSessionSegments: segments });
          } else {
            set({ viewSessionSegments: segments });
          }
        } catch (e) {
          console.error("Failed to refresh segments:", e);
        }
      },

      // D4 live refresh (LIVE_SESSION_STATE.md Gap 2). Reload the OPEN session's row +
      // segments when it is not the local active one, so a remote-live follow-along
      // fills as segments merge. Normative constraints:
      //   - Skip while an edit is in progress (editingSegmentId) so the reload cannot
      //     clobber an open/in-flight edit or drop an editSegmentText write.
      //   - Never bump noteRefreshCounter (that would re-run NoteEditor's content-reload
      //     effect and could discard an open note edit); this reloads transcript state
      //     only, leaving the note editor untouched.
      refreshOpenViewSession: async () => {
        const { selectedSessionId, activeSessionId, editingSegmentId } = get();
        if (!selectedSessionId || selectedSessionId === activeSessionId) return;
        if (editingSegmentId) return;
        try {
          const [row, segs] = await Promise.all([
            getSession(selectedSessionId),
            getSessionSegments(selectedSessionId),
          ]);
          // Re-check nothing changed during the await (session switched, or an edit
          // opened) before committing the reload.
          const now = get();
          if (
            now.selectedSessionId !== selectedSessionId ||
            now.selectedSessionId === now.activeSessionId ||
            now.editingSegmentId
          ) {
            return;
          }
          if (!row) return;
          set({ viewSession: row, viewSessionSegments: segs });
        } catch (e) {
          console.error("Failed to live-refresh open session:", e);
        }
      },

      // Note refresh
      incrementNoteRefresh: () =>
        set((state) => ({ noteRefreshCounter: state.noteRefreshCounter + 1 })),

      // Audio playback
      setPlaybackTime: (time: number) => {
        if (get().playbackTime === time) return;
        set({ playbackTime: time });
      },
      setIsPlaying: (playing: boolean) => {
        if (get().isPlaying === playing) return;
        set({ isPlaying: playing });
      },

      // Dictation history
      loadDictationHistory: async () => {
        try {
          const history = await listDictationHistory();
          set({ dictationHistory: history });
        } catch (e) {
          console.error("Failed to load dictation history:", e);
        }
      },

      deleteDictationHistoryEntry: async (id: string) => {
        try {
          const entry = await getDictationHistoryEntry(id);
          if (entry?.wav_file_path) {
            // Delete by exact path so this matches the per-part trusted-dirs
            // model (handles WAV or MP3, treats NotFound as no-op). The
            // legacy session-glob deleter scanned the audio dir and could
            // miss parts that lived under a custom audio_save_location.
            // Awaited so a failure (file held by player, perms) is surfaced
            // before the DB row goes away — otherwise the path is gone and
            // we have no recovery handle.
            try {
              await commands.deleteAudioFiles([entry.wav_file_path]);
            } catch (cleanupErr) {
              console.error(
                `Failed to delete dictation audio for ${id}:`,
                cleanupErr,
                entry.wav_file_path,
              );
              toast.error("Audio file was not deleted", {
                id: `dictation-audio-cleanup-${id}`,
              });
            }
          }
          await dbDeleteDictationHistoryEntry(id);
          set({ dictationHistory: get().dictationHistory.filter((h) => h.id !== id) });
        } catch (e) {
          console.error("Failed to delete dictation history entry:", e);
          toast.error("Failed to delete entry");
        }
      },

      clearDictationHistory: async () => {
        try {
          // Batch every entry's stored part path into one deleteAudioFiles
          // call — same per-part trusted-dirs path as the single-entry
          // delete above; one Tauri call for the whole sweep. Awaited so a
          // failure surfaces before the DB rows are dropped (otherwise the
          // paths are gone and any orphans are silent).
          const paths = get()
            .dictationHistory.map((e) => e.wav_file_path)
            .filter((p): p is string => !!p);
          if (paths.length > 0) {
            try {
              await commands.deleteAudioFiles(paths);
            } catch (cleanupErr) {
              console.error("Failed to delete dictation audio batch:", cleanupErr, paths);
              toast.error("Some dictation audio files were not deleted", {
                id: "dictation-audio-cleanup-batch",
              });
            }
          }
          await dbClearDictationHistory();
          set({ dictationHistory: [] });
        } catch (e) {
          console.error("Failed to clear dictation history:", e);
          toast.error("Failed to clear history");
        }
      },

      toggleSidebar: () => {
        set((state) => ({
          settings: {
            ...state.settings,
            sidebarCollapsed: !state.settings.sidebarCollapsed,
          },
        }));
      },

      completeFlow: (flowId: string) => {
        set((state) => ({
          settings: {
            ...state.settings,
            onboarding: {
              ...state.settings.onboarding,
              completedFlows: {
                ...state.settings.onboarding?.completedFlows,
                [flowId]: new Date().toISOString(),
              },
            },
          },
        }));
      },

      setUpdateAvailable: (update) => {
        const { updateDismissedVersion } = get();
        const dismissed =
          update && updateDismissedVersion === update.version
            ? updateDismissedVersion
            : null;
        set({ updateAvailable: update, updateDismissedVersion: dismissed });
      },
      dismissUpdate: () => {
        const { updateAvailable } = get();
        if (updateAvailable) {
          set({ updateDismissedVersion: updateAvailable.version });
        }
      },

      createManualNote: async (title?: string) => {
        try {
          const sessionId = crypto.randomUUID();
          await dbCreateManualSession(sessionId, title || "Untitled Note");
          trackManualNoteCreated();
          const sessions = await listSessions();
          const session = await getSession(sessionId);
          set({
            sessions,
            currentView: "note-detail",
            selectedSessionId: sessionId,
            viewSession: session,
            viewSessionSegments: [],
          });
        } catch (e) {
          console.error("Failed to create manual note:", e);
          toast.error("Failed to create note");
        }
      },
    }),
    {
      name: "yapstack-settings",
      version: 31,
      partialize: (state) => ({
        settings: state.settings,
        // Non-secret sync handles only (server URL, enabled flag, cursor,
        // device fingerprint). Tokens + vault key never persist here.
        syncConfig: state.syncConfig,
      }),
      // Custom merge does two non-obvious things:
      // 1. Deep-merges `settings` so new fields in DEFAULT_SETTINGS backfill
      //    for users whose persisted state predates them — Zustand's default
      //    shallow merge would replace `settings` wholesale.
      // 2. Hosts legacy field renames so they run unconditionally on every
      //    rehydrate, robust to any prior persist-version arithmetic skew.
      merge: (persisted, current) => {
        const p =
          (persisted as {
            settings?: Record<string, unknown>;
            syncConfig?: Partial<SyncConfig>;
          }) ?? {};
        const persistedSettings = { ...(p.settings ?? {}) };

        // dictationDuckTarget (reduce-TO) → dictationDuckAmount (reduce-BY).
        // `new = 1 - old` preserves effective duck strength. Guard prevents
        // clobbering a value the user already edited under the new key.
        if (
          typeof persistedSettings.dictationDuckTarget === "number" &&
          persistedSettings.dictationDuckAmount === undefined
        ) {
          persistedSettings.dictationDuckAmount =
            1 - persistedSettings.dictationDuckTarget;
        }
        delete persistedSettings.dictationDuckTarget;

        return {
          ...current,
          settings: {
            ...current.settings,
            ...(persistedSettings as Partial<Settings>),
          },
          // Backfill new SyncConfig fields for pre-sync persisted state.
          syncConfig: {
            ...current.syncConfig,
            ...(p.syncConfig ?? {}),
          },
        };
      },
      migrate: (persisted: unknown, version: number) => {
        const state = persisted as { settings?: Record<string, unknown> };
        if (version < 1 && state.settings) {
          // Merge graceSeconds → backfillSeconds, drop captureHistorySeconds
          const old = state.settings as Record<string, unknown>;
          if (old.backfillSeconds === undefined) {
            old.backfillSeconds =
              (old.graceSeconds as number | undefined) ?? 30;
          }
          delete old.graceSeconds;
          delete old.captureHistorySeconds;
        }
        if (version < 2 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.silenceDurationMs === undefined) {
            old.silenceDurationMs = defaultSettings.silenceDurationMs;
          }
          if (old.maxChunkSeconds === undefined) {
            old.maxChunkSeconds = defaultSettings.maxChunkSeconds;
          }
        }
        if (version < 3 && state.settings) {
          // v2 shipped with aggressive defaults that hurt quality — reset to proven values
          const old = state.settings as Record<string, unknown>;
          if (old.silenceDurationMs === 500) {
            old.silenceDurationMs = defaultSettings.silenceDurationMs;
          }
          if (old.maxChunkSeconds === 15) {
            old.maxChunkSeconds = defaultSettings.maxChunkSeconds;
          }
        }
        if (version < 4 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.promptContextChars === undefined) {
            old.promptContextChars = defaultSettings.promptContextChars;
          }
        }
        if (version < 5 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.theme === undefined) {
            old.theme = defaultSettings.theme;
          }
        }
        if (version < 6 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.sidebarCollapsed === undefined) {
            old.sidebarCollapsed = defaultSettings.sidebarCollapsed;
          }
        }
        if (version < 7 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.bufferMaxSeconds === undefined) {
            old.bufferMaxSeconds = 300;
          }
          delete old.backfillSeconds;
        }
        // (v8 originally backfilled state.settings.ai with DEFAULT_AI_SETTINGS.
        // After the Connection/Profile refactor the legacy `ai` field is no
        // longer read; v25 below populates `aiConfig` from whatever `ai`
        // value exists, falling back to DEFAULT_AI_CONFIG when absent.
        // Pre-v8 users therefore start the new AI flow empty, which is the
        // greenfield default — they were on a years-old persisted state.)
        if (version < 9 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.shortcutBindings === undefined) {
            old.shortcutBindings = {};
          }
        }
        if (version < 10 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.audioSaveLocation === undefined) {
            old.audioSaveLocation = null;
          }
        }
        if (version < 11 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.dictation === undefined) {
            old.dictation = DEFAULT_DICTATION_SETTINGS;
          }
        }
        if (version < 12 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          const dict = old.dictation as { slots?: Array<Record<string, unknown>> } | undefined;
          if (dict?.slots) {
            for (const slot of dict.slots) {
              if (slot.outputAction === undefined) {
                slot.outputAction = "paste";
              }
            }
          }
        }
        if (version < 13 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.showRecordingIndicator === undefined) {
            old.showRecordingIndicator = true;
          }
        }
        if (version < 14 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.selectedModelSize === "Base") {
            old.selectedModelSize = "Small";
          }
          if (old.captureSource === "MicOnly") {
            old.captureSource = "Mixed";
          }
        }
        if (version < 15 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.promptDecaySilenceSeconds === undefined) {
            old.promptDecaySilenceSeconds = defaultSettings.promptDecaySilenceSeconds;
          }
        }
        if (version < 16 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          const dict = old.dictation as { activationMode?: string } | undefined;
          if (dict && dict.activationMode === undefined) {
            dict.activationMode = "hold";
          }
        }
        if (version < 17 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          // Existing users have already configured the app — skip onboarding
          old.onboardingCompleted = true;
        }
        if (version < 18 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          old.onboarding = {
            completedFlows: old.onboardingCompleted
              ? { initial: new Date().toISOString() }
              : {},
          };
          delete old.onboardingCompleted;
        }
        if (version < 19 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          const dict = old.dictation as { slots?: Array<Record<string, unknown>> } | undefined;
          if (dict?.slots) {
            const slot1 = dict.slots.find((s) => s.id === "1");
            if (slot1 && slot1.defaultBinding === undefined) {
              slot1.defaultBinding = "Control+Shift+Space";
            }
          }
        }
        if (version < 20 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          // Replace name-based selectedMicDevice with ID-based selectedMicDeviceId.
          // Reset to null (system default) — user re-picks once on upgrade.
          delete old.selectedMicDevice;
          old.selectedMicDeviceId = null;
        }
        if (version < 21 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.audioExportFormat === undefined) old.audioExportFormat = "mp3";
          if (old.mp3Bitrate === undefined) old.mp3Bitrate = 64;
        }
        if (version < 22 && state.settings) {
          // Engines become first-class peers. Default existing users to
          // Whisper so behavior is unchanged on upgrade; the cascading
          // settings UI lets them switch.
          const old = state.settings as Record<string, unknown>;
          if (old.selectedEngine === undefined) old.selectedEngine = "Whisper";
          if (old.selectedParakeetVariant === undefined)
            old.selectedParakeetVariant = "TdtV3";
          if (old.diarizationEnabled === undefined)
            old.diarizationEnabled = false;
          if (old.speakerNames === undefined) old.speakerNames = {};
        }
        if (version < 23 && state.settings) {
          // Force-disable diarization on upgrade. Sortformer's chunk-local
          // speaker IDs cause the same person to flip across speaker
          // numbers across chunk boundaries. We're keeping the IPC + DB +
          // sidecar plumbing intact so re-enable is one line away once
          // session-stable IDs land, but no user should be running with
          // the broken behavior in the meantime.
          const old = state.settings as Record<string, unknown>;
          old.diarizationEnabled = false;
        }
        if (version < 24 && state.settings) {
          const old = state.settings as Record<string, unknown>;
          if (old.dictationDuckEnabled === undefined)
            old.dictationDuckEnabled = false;
          if (old.dictationDuckTarget === undefined)
            old.dictationDuckTarget = 0.2;
        }
        if (version < 25 && state.settings) {
          // AI Connection/Profile migration. Transforms the legacy
          // single-active-provider AISettings into the new multi-Connection
          // AIConfig + per-feature Assignments shape. Per locked design
          // decision #6, only the active provider becomes a Profile; other
          // configured providers migrate as Connections so their keys aren't
          // lost. Per decision #12, dictation slots with aiEnabled=true point
          // at the active Profile; aiEnabled=false slots get profileId=null.
          //
          // Additive only — legacy `ai` and `aiEnabled` fields stay in place
          // until Commit 11 finishes the consumer migration.
          //
          // The original legacy `ai` object is left in place untouched (it is
          // the safety net for any future re-migration); we deliberately do
          // NOT write a second `_legacyAi` copy — that only duplicated the
          // plaintext API keys into an extra persisted blob nothing reads.
          const old = state.settings as Record<string, unknown>;
          const legacy = old.ai as LegacyAISettings | undefined;
          const dict = old.dictation as
            | { slots?: LegacyDictationSlot[] }
            | undefined;
          if (legacy && dict?.slots) {
            const { config, updatedSlots } = migrateLegacyAISettings(
              legacy,
              dict.slots,
            );
            old.aiConfig = config;
            // Mutate each slot in place to add `profileId` while leaving the
            // legacy `aiEnabled` field intact (consumers swap in Commit 6).
            dict.slots = dict.slots.map((s, i) => ({
              ...s,
              profileId: updatedSlots[i]?.profileId ?? null,
            }));
          } else if (!old.aiConfig) {
            old.aiConfig = DEFAULT_AI_CONFIG;
          }
        }
        if (version < 26 && state.settings) {
          // Live insights are a brand-new feature — seed with the empty default
          // for any existing user. Additive, no field renames.
          const old = state.settings as Record<string, unknown>;
          if (old.insights === undefined) {
            // Clone — assigning the module-level constant by reference would
            // share its `slots: []` array with a live persisted-state object,
            // so any future in-place mutation would corrupt the default.
            old.insights = structuredClone(DEFAULT_INSIGHTS_SETTINGS);
          }
        }
        if (version < 27 && state.settings) {
          // Insights rename: `activeInsightId` (persisted, mid-session edits
          // leaked into defaults) → `defaultInsightId` (persisted, drives
          // session-start only). The mid-session "what's running right now"
          // role is now a runtime-only field (`currentInsightId`) initialized
          // from `defaultInsightId` at session start.
          const insights = (state.settings as Record<string, unknown>)
            .insights as Record<string, unknown> | undefined;
          if (insights && "activeInsightId" in insights) {
            insights.defaultInsightId = insights.activeInsightId;
            delete insights.activeInsightId;
          }
        }
        if (version < 28 && state.settings) {
          // Insight cadence: per-slot `heartbeatSeconds` (fixed interval) →
          // `trigger` (preset + W/P/F/M). Map the old interval to a behavior-
          // preserving Custom tuple: threshold/settle off, floor == ceiling ==
          // the old heartbeat, i.e. "fire every N seconds if new content".
          const insights = (state.settings as Record<string, unknown>)
            .insights as { slots?: Array<Record<string, unknown>> } | undefined;
          if (insights?.slots) {
            for (const slot of insights.slots) {
              if (slot.trigger === undefined) {
                const hb =
                  typeof slot.heartbeatSeconds === "number"
                    ? slot.heartbeatSeconds
                    : 30;
                slot.trigger = {
                  preset: "custom",
                  thresholdWords: 0,
                  settleSeconds: 0,
                  minIntervalSeconds: hb,
                  maxWaitSeconds: hb,
                };
                delete slot.heartbeatSeconds;
              }
            }
          }
        }
        if (version < 29 && state.settings) {
          // Cadence presets renamed from content archetypes to tempo tiers
          // (summary/jargon/topic → relaxed/responsive/balanced), and a `type`
          // field was added (what the Insight extracts). Remap old preset names
          // and default existing freeform Insights to type "custom".
          const insights = (state.settings as Record<string, unknown>)
            .insights as { slots?: Array<Record<string, unknown>> } | undefined;
          const presetRemap: Record<string, string> = {
            summary: "relaxed",
            jargon: "responsive",
            topic: "balanced",
          };
          if (insights?.slots) {
            for (const slot of insights.slots) {
              const trigger = slot.trigger as
                | Record<string, unknown>
                | undefined;
              if (trigger && typeof trigger.preset === "string") {
                const mapped = presetRemap[trigger.preset];
                if (mapped) trigger.preset = mapped;
              }
              if (slot.type === undefined) slot.type = "custom";
            }
          }
        }
        if (version < 30 && state.settings) {
          // Dropped the per-Insight `enabled` flag — an Insight only runs when
          // it's the Current Insight, so the boolean was redundant. Remove the
          // vestigial key from persisted slots.
          const insights = (state.settings as Record<string, unknown>)
            .insights as { slots?: Array<Record<string, unknown>> } | undefined;
          if (insights?.slots) {
            for (const slot of insights.slots) {
              delete slot.enabled;
            }
          }
        }
        if (version < 31 && state.settings) {
          // Remove the redundant `_legacyAi` snapshot an earlier build of the
          // AI refactor wrote at v25 (a duplicate of the legacy `ai` object,
          // including plaintext keys, that nothing ever read). The original
          // legacy `ai` field stays as the re-migration safety net. Without
          // this, DBs that ran that build keep re-persisting the duplicate.
          delete (state.settings as Record<string, unknown>)._legacyAi;
        }
        return state as unknown as { settings: Settings };
      },
    },
  ),
);
}

// Pin the store into import.meta.hot.data so Vite HMR preserves runtime state
// (enginePhase, captureStatus, active session, etc.) across module reloads.
// Without this, every HMR tick resets enginePhase to "idle" until autoSetup's
// backend probe completes, causing dictation and other engine-gated features
// to flicker. See the HMR rehydration research notes for context.
const HMR_STORE_KEY = "__yapstack_app_store__";
type HmrStore = ReturnType<typeof createAppStore>;

function getOrCreateAppStore(): HmrStore {
  // In vitest, import.meta.hot exists but .data may be undefined. Fall back
  // to a fresh store in that case — HMR-persistence is a dev-server concern
  // that doesn't apply to test isolation.
  const hot = import.meta.hot?.data as
    | Record<string, HmrStore>
    | undefined;
  if (!hot) return createAppStore();
  if (!hot[HMR_STORE_KEY]) {
    hot[HMR_STORE_KEY] = createAppStore();
  }
  return hot[HMR_STORE_KEY];
}

export const useAppStore = getOrCreateAppStore();

// Auto-subscribe to backend `sync://applied` events at module load so pulled peer changes
// refresh the UI without an app restart (R6 item 2). Guarded to a real Tauri window so unit
// tests (jsdom, no `__TAURI_INTERNALS__`) don't register a live listener; those exercise
// `startSyncAppliedRefresh` directly with a mocked event API. Best-effort: a failure here
// must never break app boot.
if (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window) {
  void useAppStore.getState().startSyncAppliedRefresh();
}
