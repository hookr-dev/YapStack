import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowLeft, Play, X, RefreshCw, AlertTriangle } from "lucide-react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { commands } from "@/lib/tauri";
import {
  deriveTrackFetch,
  formatTrackFetchLabel,
  formatNoSpace,
  nextPollDelayMs,
  syncCommands,
  type AudioPartPrepare,
  type TrackFetch,
} from "@/lib/sync";
import { SessionHeader } from "@/components/SessionHeader";
import { ChatView } from "@/components/ChatView";
import { NoteEditor } from "@/components/NoteEditor";
import { FloatingChatBar } from "@/components/FloatingChatBar";
import { AIContextProvider } from "@/components/AIContextProvider";
import { AudioPlayer } from "@/components/AudioPlayer";
import {
  ResizablePanelGroup,
  ResizablePanel,
  ResizableHandle,
} from "@/components/ui/resizable";
import { canResumeSession, getSession, type DbSession, type DbAudioPart } from "@/lib/db";
import type { AudioPart } from "@/components/AudioPlayer";
import {
  createSessionSources,
  createSessionTools,
  createSessionSystemPromptBuilder,
} from "@/lib/ai-context";
import { getActionsForSession } from "@/lib/ai-actions";
import { getToolEffects } from "@/lib/ai-tools";
import { convertFileSrc } from "@tauri-apps/api/core";
import { useAutoTag } from "@/hooks/useAutoTag";
import { AutoTagSuggestions } from "@/components/AutoTagSuggestions";

/** Build a URL for the custom audio-stream:// protocol registered in Rust. */
function audioStreamUrl(filePath: string): string {
  return convertFileSrc(filePath, "audio-stream");
}

/**
 * Remote-live selection predicate (LIVE_SESSION_STATE.md D3): a `recording` session
 * owned by ANOTHER device — read-only follow-along. Pure so the branch selection is
 * unit-testable without mounting the view. NULL `myFingerprint` (sync off / pre-
 * enrollment) makes this always false, preserving single-device behavior. For this
 * slice any non-mine truthy-owner `recording` row qualifies; slice 4 splits fresh
 * ("Live") vs stale ("Interrupted") via `isRecordingStale`.
 */
export function isRemoteLiveSession(
  session: Pick<DbSession, "status" | "recording_device_id">,
  isActiveSession: boolean,
  myFingerprint: string | null,
): boolean {
  const owner = session.recording_device_id;
  return (
    !isActiveSession &&
    session.status === "recording" &&
    !!owner &&
    // D7: with no fingerprint (sync off / pre-enrollment) every D3 comparison is
    // false → single-device behavior, never a remote-live branch.
    !!myFingerprint &&
    owner !== myFingerprint
  );
}

export interface AudioAvailability {
  /** Parts to hand to `<AudioPlayer>`, in original order. Empty when the track
   *  is unavailable and the honest "audio is on <device>" state renders. */
  playableParts: AudioPart[];
  /** True when at least one expected part's file is absent on this device —
   *  the sync case where the metadata arrived but the audio bytes did not. */
  unavailable: boolean;
}

/**
 * All-or-nothing audio availability selection (v1). `exists[i]` is the on-disk
 * presence of `parts[i]` as reported by the `audio_files_exist` command; `null`
 * (or a length mismatch) means "not checked yet" — we stay optimistic so a
 * local session's player renders immediately on mount rather than flashing an
 * unavailable state.
 *
 * When every part is present the full ordered list plays. When ANY part is
 * missing we withhold the player entirely instead of compressing the timeline:
 * transcript timestamp clicks seek by GLOBAL time (a cumulative offset across
 * the full part list), so playing a subset would mis-seek every click. In the
 * sync scenario a session's parts are either all-local (recorded here) or
 * all-foreign (synced from a peer, no bytes), so all-or-nothing is the honest
 * common case; a rare genuinely-mixed session degrades to the unavailable
 * state rather than to silent mis-seeking. Pure so the branch is unit-testable.
 */
export function selectAudioAvailability(
  parts: AudioPart[],
  exists: boolean[] | null,
): AudioAvailability {
  if (parts.length === 0) {
    return { playableParts: [], unavailable: false };
  }
  // Optimistic while the async check is unresolved or shape-mismatched.
  if (exists === null || exists.length !== parts.length) {
    return { playableParts: parts, unavailable: false };
  }
  const allPresent = exists.every(Boolean);
  return allPresent
    ? { playableParts: parts, unavailable: false }
    : { playableParts: [], unavailable: true };
}

/**
 * Assemble the ordered player parts from the raw part rows, this device's on-disk presence
 * (`presentFlags`, aligned to `rawParts`; `null` = optimistic/unchecked), and any fetched
 * cache paths (`cacheByPart`, part id → decrypted cache file). A part's src resolves in D2
 * order: locally present → `file_path` (same-device fast path); else fetched → the cache
 * path; else `""` (unresolved). `allPlayable` is the all-or-nothing gate — every part must
 * resolve before the timeline (which seeks by global time) is honest. Pure & unit-testable.
 */
export function assembleTrack(
  rawParts: DbAudioPart[],
  presentFlags: boolean[] | null,
  cacheByPart: Record<string, string>,
  toStreamUrl: (filePath: string) => string,
): { parts: AudioPart[]; allPlayable: boolean } {
  const aligned = presentFlags && presentFlags.length === rawParts.length ? presentFlags : null;
  const parts = rawParts.map((p, i) => {
    const present = aligned ? aligned[i] !== false : true;
    const cache = cacheByPart[p.id];
    const src = present
      ? toStreamUrl(p.file_path)
      : cache
        ? toStreamUrl(cache)
        : "";
    return { src, duration: p.duration_seconds };
  });
  const allPlayable = rawParts.length > 0 && parts.every((pp) => pp.src !== "");
  return { parts, allPlayable };
}

/**
 * The honest missing-audio bar for a synced session whose bytes are not on this device (S3).
 * With auto-fetch (S3.5) the happy path never needs a click: the bar shows queued/fetching
 * progress with a cancel X. Renders one of: the classic disabled "audio is on <device>"
 * (sync off — nothing to fetch); a re-start affordance (only reachable after the user
 * cancelled); "Waiting to fetch…" (admission-queued); "Fetching [part K of M —] N%" with
 * cancel; or a terminal error / on-device / no-space state with Retry where a retry can
 * help. Never silent. Pure presentational — all wiring is passed in.
 */
function AudioFetchBar({
  deviceLabel,
  syncEnabled,
  active,
  track,
  onFetch,
  onCancel,
  onRetry,
}: {
  deviceLabel: string;
  syncEnabled: boolean;
  /** Auto-fetch armed (or manually re-started) — false only after a user cancel. */
  active: boolean;
  track: TrackFetch | null;
  onFetch: () => void;
  onCancel: () => void;
  onRetry: () => void;
}) {
  const barCls =
    "flex items-center gap-3 border-b px-4 py-2 text-muted-foreground";

  // Sync off (or no relay): the audio can only be fetched via sync — stay honest & disabled.
  if (!syncEnabled) {
    return (
      <div className={barCls}>
        <Button variant="ghost" size="icon-xs" disabled aria-label="Audio unavailable" title="Audio is not on this device">
          <Play className="h-4 w-4" />
        </Button>
        <span className="text-xs">Audio is on {deviceLabel}</span>
      </div>
    );
  }

  // Only reachable after an explicit user cancel (auto-fetch is otherwise always armed):
  // offer to start it again.
  if (!active || !track) {
    return (
      <div className={barCls}>
        <Button variant="ghost" size="icon-xs" onClick={onFetch} aria-label="Fetch and play audio" title="Fetch audio from sync">
          <Play className="h-4 w-4" />
        </Button>
        <span className="text-xs">Audio is on {deviceLabel} — click to fetch</span>
      </div>
    );
  }

  if (track.kind === "queued") {
    return (
      <div className={barCls}>
        <span className="text-xs">Waiting to fetch…</span>
        <div className="h-1 flex-1 overflow-hidden rounded bg-muted" />
        <Button variant="ghost" size="icon-xs" onClick={onCancel} aria-label="Cancel fetch" title="Cancel">
          <X className="h-4 w-4" />
        </Button>
      </div>
    );
  }

  if (track.kind === "fetching") {
    return (
      <div className={barCls}>
        <span className="text-xs tabular-nums">{formatTrackFetchLabel(track)}</span>
        <div className="h-1 flex-1 overflow-hidden rounded bg-muted">
          <div className="h-full bg-primary transition-[width]" style={{ width: `${track.percent}%` }} />
        </div>
        <Button variant="ghost" size="icon-xs" onClick={onCancel} aria-label="Cancel fetch" title="Cancel">
          <X className="h-4 w-4" />
        </Button>
      </div>
    );
  }

  if (track.kind === "on_device") {
    // The server doesn't (fully) hold the track yet; the poll re-probes every 30s and the
    // fetch self-starts when the source device's upload lands.
    return (
      <div className={barCls}>
        <Button variant="ghost" size="icon-xs" disabled aria-label="Audio unavailable" title="Not yet backed up to sync — retries automatically">
          <Play className="h-4 w-4" />
        </Button>
        <span className="text-xs">Audio is on {deviceLabel}</span>
      </div>
    );
  }

  if (track.kind === "verification_failed") {
    return (
      <div className={barCls}>
        <AlertTriangle className="h-4 w-4 text-amber-500" />
        <span className="text-xs">Audio failed verification</span>
      </div>
    );
  }

  // ready shouldn't reach here (the player renders instead) — guard for exhaustiveness.
  if (track.kind === "ready") return null;

  // no_space / unreachable / error → surface verbatim + offer retry.
  const message =
    track.kind === "unreachable"
      ? "Can't reach sync server"
      : track.kind === "no_space"
        ? formatNoSpace(track.needed)
        : track.message;
  return (
    <div className={barCls}>
      <AlertTriangle className="h-4 w-4 text-amber-500" />
      <span className="min-w-0 flex-1 truncate text-xs" title={message}>{message}</span>
      <Button variant="ghost" size="icon-xs" onClick={onRetry} aria-label="Retry fetch" title="Retry">
        <RefreshCw className="h-4 w-4" />
      </Button>
    </div>
  );
}

export function NoteDetailView() {
  const selectedSessionId = useAppStore((s) => s.selectedSessionId);
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const activeSessionSegments = useAppStore((s) => s.activeSessionSegments);
  const viewSession = useAppStore((s) => s.viewSession);
  const viewSessionSegments = useAppStore((s) => s.viewSessionSegments);
  const activeSession = useAppStore(
    (s) => s.activeSessionId
      ? s.sessions.find((x) => x.id === s.activeSessionId) ?? null
      : null,
  );
  const backfillActive = useAppStore((s) => s.backfillActive);
  const backfillBoundarySeconds = useAppStore((s) => s.backfillBoundarySeconds);
  const playbackTime = useAppStore((s) => s.playbackTime);
  const setPlaybackTime = useAppStore((s) => s.setPlaybackTime);
  const setIsPlaying = useAppStore((s) => s.setIsPlaying);
  const noteRefreshCounter = useAppStore((s) => s.noteRefreshCounter);
  const loadSessions = useAppStore((s) => s.loadSessions);
  const incrementNoteRefresh = useAppStore((s) => s.incrementNoteRefresh);
  const activeSessionParts = useAppStore((s) => s.activeSessionParts);
  const viewSessionParts = useAppStore((s) => s.viewSessionParts);
  const resumeSession = useAppStore((s) => s.resumeSession);
  const liveTranscriptionActive = useAppStore((s) => s.liveTranscriptionActive);
  const sessionStopping = useAppStore((s) => s.sessionStopping);
  const syncStatus = useAppStore((s) => s.syncStatus);
  const navigateTo = useAppStore((s) => s.navigateTo);

  const isActiveSession = selectedSessionId === activeSessionId;
  const myFingerprint = syncStatus?.deviceFingerprint ?? null;

  const {
    suggestions,
    folders: autoTagFolders,
    folderTree: autoTagFolderTree,
    acceptSuggestion,
    applyOverride,
    dismissSuggestion,
  } = useAutoTag(selectedSessionId, isActiveSession);

  const session = isActiveSession ? activeSession : viewSession;

  const segments = isActiveSession ? activeSessionSegments : viewSessionSegments;

  // AI Context setup
  const sources = useMemo(
    () => selectedSessionId
      ? createSessionSources(selectedSessionId, segments.length, session?.session_type ?? "transcription")
      : [],
    [selectedSessionId, segments.length, session?.session_type],
  );
  const tools = useMemo(
    () => selectedSessionId ? createSessionTools(selectedSessionId) : { availableToolIds: [], getToolContext: null },
    [selectedSessionId],
  );
  const buildSystemPrompt = useMemo(
    () => selectedSessionId ? createSessionSystemPromptBuilder(selectedSessionId) : async () => "",
    [selectedSessionId],
  );
  const sessionActions = useMemo(
    () => getActionsForSession(session?.session_type ?? "transcription"),
    [session?.session_type],
  );
  const loadSessionFolders = useAppStore((s) => s.loadSessionFolders);
  const loadSessionTags = useAppStore((s) => s.loadSessionTags);
  const loadTags = useAppStore((s) => s.loadTags);
  const refreshViewSessionSegments = useAppStore(
    (s) => s.refreshViewSessionSegments,
  );
  const handleToolsExecuted = useCallback(
    async (names: string[]) => {
      if (!selectedSessionId) return;
      const effects = getToolEffects(names);
      if (effects.has("session-meta")) {
        await loadSessions();
        const refreshed = await getSession(selectedSessionId);
        if (refreshed) {
          useAppStore.setState({ viewSession: refreshed });
        }
      }
      if (effects.has("notes")) {
        incrementNoteRefresh();
      }
      if (effects.has("organization")) {
        await Promise.all([loadSessionFolders(), loadSessionTags(), loadTags()]);
      }
      if (effects.has("transcript")) {
        await refreshViewSessionSegments();
      }
    },
    [
      selectedSessionId,
      loadSessions,
      incrementNoteRefresh,
      loadSessionFolders,
      loadSessionTags,
      loadTags,
      refreshViewSessionSegments,
    ],
  );

  const handleSeek = useCallback(
    (time: number) => {
      setPlaybackTime(time);
      const audioEl = document.querySelector<
        HTMLAudioElement & {
          seekTo?: (t: number, options?: { autoPlay?: boolean }) => void;
        }
      >("audio[data-session-audio]");
      if (!audioEl) return;
      // `time` is global across parts; use the player's seekTo so a click
      // that targets a different part swaps src and applies the seek on
      // loadedmetadata. Setting `audioEl.currentTime` directly would clamp
      // to the active part's local duration.
      if (audioEl.seekTo) {
        audioEl.seekTo(time, { autoPlay: true });
      } else {
        audioEl.currentTime = time;
        if (audioEl.paused) audioEl.play().catch(() => {});
      }
    },
    [setPlaybackTime],
  );

  // Audio-file presence check (honest player). `session_audio_parts.file_path`
  // is the RECORDING device's absolute path, synced verbatim; on a peer device
  // that file is absent because audio bytes are NOT synced in this release —
  // only metadata. Probe existence off the render path (resolves post-mount)
  // and re-check when the parts change (session switch / D4 reload replaces the
  // parts array) or the window regains focus (the user may have copied the
  // file over). `partsExist` stays `null` (optimistic) until the probe lands.
  const partPaths = useMemo(
    () =>
      (isActiveSession ? activeSessionParts : viewSessionParts).map(
        (p) => p.file_path,
      ),
    [isActiveSession, activeSessionParts, viewSessionParts],
  );
  const [partsExist, setPartsExist] = useState<boolean[] | null>(null);
  useEffect(() => {
    if (partPaths.length === 0) {
      setPartsExist(null);
      return;
    }
    let cancelled = false;
    const check = () => {
      commands
        .audioFilesExist(partPaths)
        .then((res) => {
          if (!cancelled) setPartsExist(res);
        })
        .catch(() => {
          // Command unavailable (older backend) → stay optimistic; the
          // player's own play-error toast still catches a genuinely missing
          // file when the user hits play.
          if (!cancelled) setPartsExist(null);
        });
    };
    setPartsExist(null); // reset to optimistic while re-checking
    check();
    window.addEventListener("focus", check);
    return () => {
      cancelled = true;
      window.removeEventListener("focus", check);
    };
  }, [partPaths]);

  // ---- S3.5 auto-fetch playback ----
  // When a synced session's audio is missing locally, fetching starts BY ITSELF the moment
  // the session opens (owner directive — no click gate): GET → decrypt-to-cache → player,
  // with progress + cancel. Single-flight + the global cap (2) + FIFO admission live in
  // Rust; here we POLL `audio_prepare_part` per missing part IN part_index ORDER (the
  // submission order IS the queue order) and aggregate to one honest track state. A
  // remote-LIVE session (foreign `recording`) is excluded — its parts are still growing —
  // but the arm predicate re-evaluates when the D4 live-refresh flips it to `completed`
  // (session.status is part of `remoteLive` below), so the fetch then fires unprompted.
  const rawParts = useMemo(
    () => (isActiveSession ? activeSessionParts : viewSessionParts),
    [isActiveSession, activeSessionParts, viewSessionParts],
  );
  const syncEnabled = !!syncStatus?.syncEnabled;
  const remoteLive = session
    ? isRemoteLiveSession(session, isActiveSession, myFingerprint)
    : false;
  const missingPartIds = useMemo(() => {
    if (!partsExist || partsExist.length !== rawParts.length) return [];
    return rawParts.filter((_, i) => partsExist[i] === false).map((p) => p.id);
  }, [rawParts, partsExist]);
  const [prepareStates, setPrepareStates] = useState<Record<string, AudioPartPrepare>>({});
  // Set ONLY by the user's explicit cancel (X): auto-fetch must not re-arm right after a
  // cancel. Reset on track change and by the manual re-start affordance.
  const [fetchSuppressed, setFetchSuppressed] = useState(false);
  const [fetchNonce, setFetchNonce] = useState(0);
  // A stable key for the missing-set so effects/handlers don't churn on array identity.
  const missingKey = missingPartIds.join(",");
  const missingRef = useRef(missingPartIds);
  missingRef.current = missingPartIds;

  // Reset the fetch flow whenever the track changes (session switch / parts reload).
  useEffect(() => {
    setFetchSuppressed(false);
    setPrepareStates({});
  }, [missingKey]);

  // The auto-arm predicate (deliverable: session open + sync on + missing parts + not
  // remote-live, minus an explicit user cancel).
  const fetchArmed =
    syncEnabled && !remoteLive && missingPartIds.length > 0 && !fetchSuppressed;

  // Poll while armed: submit/poll every missing part in order, re-arm per the derived
  // track state (600ms while queued/fetching, 30s while the server lacks a blob — the
  // still-uploading re-probe; stop on ready/terminals). On unmount (or disarm) drop the
  // QUEUED parts only — an in-flight download completes into the cache, so reopening the
  // session re-submits idempotently (cache hits short-circuit in Rust).
  useEffect(() => {
    if (!fetchArmed) return;
    const ids = missingRef.current;
    if (ids.length === 0) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const tick = async () => {
      const next: Record<string, AudioPartPrepare> = {};
      // Sequential, ordered submission: part_index order = FIFO admission order in Rust.
      for (const id of ids) {
        try {
          const st = await syncCommands.prepareAudioPart(id);
          // IPC boundary: never trust the shape blindly (an older backend or a bad
          // serialization must degrade to an honest error, not a render crash).
          next[id] =
            st && typeof st === "object" && "state" in st
              ? st
              : { state: "error", message: "invalid prepare response" };
        } catch (e) {
          next[id] = { state: "error", message: String(e) };
        }
      }
      if (cancelled) return;
      setPrepareStates(next);
      const delay = nextPollDelayMs(deriveTrackFetch(ids.map((id) => next[id])));
      if (delay !== null) timer = setTimeout(() => void tick(), delay);
    };
    void tick();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      // Navigate-away: cancel queued (never-started) parts; leave in-flight ones to land.
      ids.forEach((id) => void syncCommands.releaseAudioPart(id).catch(() => {}));
    };
  }, [fetchArmed, missingKey, fetchNonce]);

  const startFetch = useCallback(() => {
    setPrepareStates({});
    setFetchSuppressed(false);
    setFetchNonce((n) => n + 1);
  }, []);
  const cancelFetch = useCallback(() => {
    missingRef.current.forEach((id) => void syncCommands.cancelAudioPart(id));
    setFetchSuppressed(true);
    setPrepareStates({});
  }, []);
  const retryFetch = useCallback(() => {
    missingRef.current.forEach((id) => void syncCommands.cancelAudioPart(id));
    setPrepareStates({});
    setFetchSuppressed(false);
    setFetchNonce((n) => n + 1);
  }, []);

  const cacheByPart = useMemo(() => {
    const out: Record<string, string> = {};
    for (const [id, st] of Object.entries(prepareStates)) {
      if (st.state === "ready") out[id] = st.path;
    }
    return out;
  }, [prepareStates]);

  if (!session) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <p className="text-sm text-muted-foreground">Session not found</p>
      </div>
    );
  }
  const isEditable = isActiveSession || session.status === "completed";
  const isManual = session.session_type === "manual";
  const isTranscription = !isManual;
  // D2-ordered assembly: local file_path for present parts, fetched cache path for the rest.
  const assembled = assembleTrack(rawParts, partsExist, cacheByPart, audioStreamUrl);
  const partsForPlayer: AudioPart[] = assembled.parts;
  const hasAudio = rawParts.length > 0;
  // Aggregate fetch state across the MISSING parts (all-or-nothing: the timeline seeks by
  // global time, so every part must resolve before we render the player).
  const trackFetch: TrackFetch | null =
    fetchArmed && missingPartIds.length > 0
      ? deriveTrackFetch(
          missingPartIds.map(
            (id) =>
              prepareStates[id] ?? { state: "fetching", received: 0, total: 0 },
          ),
        )
      : null;
  // `audioPlayable` = every part has a resolved src (local or freshly fetched). When parts
  // are missing and not (yet) fetched we render the honest fetch/unavailable bar and withhold
  // every playback affordance (transcript-timestamp seeking included).
  const audioPlayable = hasAudio && assembled.allPlayable;
  const audioMissing = hasAudio && missingPartIds.length > 0 && !assembled.allPlayable;
  const audioDeviceLabel =
    syncStatus?.roster.find(
      (r) => r.fingerprint === session.recording_device_id,
    )?.label ?? "another device";

  const chatBar = selectedSessionId ? (
    <AIContextProvider
      contextKey={selectedSessionId}
      sources={sources}
      tools={tools}
      actions={sessionActions}
      segments={segments}
      buildSystemPrompt={buildSystemPrompt}
      isSessionContext={true}
      sessionId={selectedSessionId}
      onToolsExecuted={handleToolsExecuted}
    >
      <FloatingChatBar />
    </AIContextProvider>
  ) : null;

  // Manual notes: full-width editor
  if (isManual) {
    return (
      <div className="relative flex flex-1 flex-col min-h-0 pb-16 view-enter">
        <SessionHeader session={session} segments={segments} />
        <NoteEditor sessionId={session.id} refreshKey={noteRefreshCounter} />
        {chatBar}
      </div>
    );
  }

  // Active recording: split pane with transcript (left) + notes (right)
  if (isActiveSession) {
    return (
      <div className="flex flex-1 flex-col min-h-0 view-enter">
        <SessionHeader session={session} segments={segments} />
        <AutoTagSuggestions
          suggestions={suggestions}
          folders={autoTagFolders}
          folderTree={autoTagFolderTree}
          onAccept={acceptSuggestion}
          onApplyOverride={applyOverride}
          onDismiss={dismissSuggestion}
        />
        <ResizablePanelGroup orientation="horizontal" className="flex-1">
          <ResizablePanel defaultSize="40%" minSize="20%">
            <div className="flex flex-col h-full min-h-0">
              <ChatView
                sessionId={selectedSessionId ?? undefined}
                segments={segments}
                backfillActive={backfillActive && isActiveSession}
                backfillBoundarySeconds={
                  isActiveSession ? backfillBoundarySeconds : null
                }
                isEditable={isEditable}
                initialScrollToBottom={isActiveSession}
              />
            </div>
          </ResizablePanel>
          <ResizableHandle />
          <ResizablePanel defaultSize="60%" minSize="25%">
            <div className="relative flex flex-col h-full min-h-0 pb-24">
              <NoteEditor sessionId={session.id} refreshKey={noteRefreshCounter} />
              {chatBar}
            </div>
          </ResizablePanel>
        </ResizablePanelGroup>
      </div>
    );
  }

  // Completed transcription: split pane with transcript (left) + notes (right)
  if (isTranscription && isEditable) {
    return (
      <div className="flex flex-1 flex-col min-h-0 view-enter">
        <SessionHeader session={session} segments={segments} />
        {audioMissing ? (
          // Metadata synced from another device but the audio bytes did not. S3: offer
          // fetch-on-demand (progress + cancel) instead of a dead disabled control.
          <AudioFetchBar
            deviceLabel={audioDeviceLabel}
            syncEnabled={syncEnabled}
            active={fetchArmed}
            track={trackFetch}
            onFetch={startFetch}
            onCancel={cancelFetch}
            onRetry={retryFetch}
          />
        ) : hasAudio ? (
          <AudioPlayer
            parts={partsForPlayer}
            onTimeUpdate={setPlaybackTime}
            onPlayStateChange={setIsPlaying}
            onResume={
              canResumeSession(
                session,
                viewSessionParts,
                liveTranscriptionActive,
                sessionStopping,
              )
                ? () => resumeSession(session.id)
                : undefined
            }
          />
        ) : null}
        <ResizablePanelGroup orientation="horizontal" className="flex-1">
          <ResizablePanel defaultSize="40%" minSize="20%">
            <div className="flex flex-col h-full min-h-0">
              <ChatView
                sessionId={selectedSessionId ?? undefined}
                segments={segments}
                isEditable={isEditable}
                currentPlaybackTime={audioPlayable ? playbackTime : undefined}
                onTimestampClick={audioPlayable ? handleSeek : undefined}
              />
            </div>
          </ResizablePanel>
          <ResizableHandle />
          <ResizablePanel defaultSize="60%" minSize="25%">
            <div className="relative flex flex-col h-full min-h-0 pb-24">
              <NoteEditor sessionId={session.id} refreshKey={noteRefreshCounter} onSeekTime={audioPlayable ? handleSeek : undefined} />
              {chatBar}
            </div>
          </ResizablePanel>
        </ResizablePanelGroup>
      </div>
    );
  }

  // Remote-live follow-along (D3): a `recording` session owned by ANOTHER device.
  // Read-only — every write affordance (record/resume/edit/delete, the notes editor,
  // the chat bar) is withheld; the transcript streams in as D4 refreshes fire. For
  // this slice any non-mine 'recording' row renders here (slice 4 will split fresh vs
  // stale into "Live"/"Interrupted"); the liveness helper (isRecordingStale) already
  // exists for slice 4 to consume.
  const owner = session.recording_device_id;

  if (remoteLive) {
    const label =
      syncStatus?.roster.find((r) => r.fingerprint === owner)?.label ??
      "another device";
    return (
      <div className="flex flex-1 flex-col min-h-0 pb-16 view-enter">
        <div className="flex items-center justify-between border-b px-4 py-2">
          <div className="flex min-w-0 flex-1 items-center gap-2">
            <button
              className="shrink-0 rounded p-1 text-muted-foreground hover:bg-muted"
              onClick={() => navigateTo("note-list")}
              aria-label="Back to notes"
            >
              <ArrowLeft className="h-4 w-4" />
            </button>
            <span className="truncate text-sm font-medium">
              {session.title || "Untitled"}
            </span>
          </div>
          <div
            className="flex shrink-0 items-center gap-2 rounded-full border border-emerald-500/20 bg-emerald-500/10 px-3 py-1 text-xs font-medium text-emerald-600 dark:text-emerald-400"
            aria-live="polite"
          >
            <span className="h-2 w-2 rounded-full bg-emerald-500 animate-pulse" />
            <span>Live on {label}</span>
          </div>
        </div>
        {segments.length > 0 ? (
          <ChatView
            sessionId={selectedSessionId ?? undefined}
            segments={segments}
            initialScrollToBottom
          />
        ) : (
          <div className="flex flex-1 items-center justify-center">
            <p className="text-sm text-muted-foreground">
              Waiting for {label} to start speaking…
            </p>
          </div>
        )}
      </div>
    );
  }

  // Fallback: transcript only
  return (
    <div className="flex flex-1 flex-col min-h-0 pb-16 view-enter">
      <SessionHeader session={session} segments={segments} />
      <ChatView sessionId={selectedSessionId ?? undefined} segments={segments} />
    </div>
  );
}
