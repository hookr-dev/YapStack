import { useCallback, useMemo } from "react";
import { useAppStore } from "@/stores/appStore";
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
import { canResumeSession, getSession } from "@/lib/db";
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

  const isActiveSession = selectedSessionId === activeSessionId;

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
  const partsForPlayer: AudioPart[] = (
    isActiveSession ? activeSessionParts : viewSessionParts
  ).map((p) => ({
    src: audioStreamUrl(p.file_path),
    duration: p.duration_seconds,
  }));
  const hasAudio = partsForPlayer.length > 0;

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
        {hasAudio && (
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
        )}
        <ResizablePanelGroup orientation="horizontal" className="flex-1">
          <ResizablePanel defaultSize="40%" minSize="20%">
            <div className="flex flex-col h-full min-h-0">
              <ChatView
                sessionId={selectedSessionId ?? undefined}
                segments={segments}
                isEditable={isEditable}
                currentPlaybackTime={hasAudio ? playbackTime : undefined}
                onTimestampClick={hasAudio ? handleSeek : undefined}
              />
            </div>
          </ResizablePanel>
          <ResizableHandle />
          <ResizablePanel defaultSize="60%" minSize="25%">
            <div className="relative flex flex-col h-full min-h-0 pb-24">
              <NoteEditor sessionId={session.id} refreshKey={noteRefreshCounter} onSeekTime={hasAudio ? handleSeek : undefined} />
              {chatBar}
            </div>
          </ResizablePanel>
        </ResizablePanelGroup>
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
