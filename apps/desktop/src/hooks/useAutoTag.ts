import { useState, useEffect, useRef, useCallback } from "react";
import { useAppStore } from "@/stores/appStore";
import {
  FolderSuggestionTracker,
  buildFolderProfiles,
  type FolderSuggestion,
} from "@/lib/auto-tag";
import { isVisibleSegment } from "@/lib/ai";

export function useAutoTag(sessionId: string | null, isRecording: boolean) {
  const folders = useAppStore((s) => s.folders);
  const folderTree = useAppStore((s) => s.folderTree);
  const tags = useAppStore((s) => s.tags);
  const sessionFolderMap = useAppStore((s) => s.sessionFolderMap);
  const sessionTagMap = useAppStore((s) => s.sessionTagMap);
  const activeSessionSegments = useAppStore((s) => s.activeSessionSegments);
  const addSessionToFolder = useAppStore((s) => s.addSessionToFolder);

  const [suggestions, setSuggestions] = useState<FolderSuggestion[]>([]);
  const trackerRef = useRef<FolderSuggestionTracker | null>(null);
  const processedCountRef = useRef(0);
  const profilesRef = useRef(
    buildFolderProfiles(folders, tags, sessionFolderMap, sessionTagMap),
  );
  // Sessions for which the user has already filed a folder via this UI.
  // Lives outside the tracker so the completion state survives the
  // tracker rebuild that fires when sessionFolderMap mutates after accept
  // or override (otherwise the bar would flash back into view).
  const completedSessionsRef = useRef(new Set<string>());

  // Profiles depend on the global folder/tag membership graph; the runtime
  // tracker just consumes them. Rebuild when any of those inputs change so a
  // newly-created folder or a tag added mid-session is reflected.
  useEffect(() => {
    profilesRef.current = buildFolderProfiles(
      folders,
      tags,
      sessionFolderMap,
      sessionTagMap,
    );
  }, [folders, tags, sessionFolderMap, sessionTagMap]);

  useEffect(() => {
    if (!sessionId || !isRecording) {
      trackerRef.current = null;
      processedCountRef.current = 0;
      setSuggestions([]);
      return;
    }
    const existingFolders = sessionFolderMap[sessionId] ?? [];
    const tracker = new FolderSuggestionTracker(
      profilesRef.current,
      existingFolders,
    );
    if (completedSessionsRef.current.has(sessionId)) {
      tracker.complete();
    }
    trackerRef.current = tracker;
    processedCountRef.current = 0;
  }, [sessionId, isRecording, sessionFolderMap]);

  useEffect(() => {
    if (!trackerRef.current || !isRecording) return;

    const newSegments = activeSessionSegments.slice(processedCountRef.current);
    if (newSegments.length === 0) return;
    processedCountRef.current = activeSessionSegments.length;

    let latest: FolderSuggestion[] = [];
    for (const seg of newSegments) {
      // Hidden/deleted segments are excluded from all AI-derived processing,
      // including the folder-suggestion heuristic (same gate as LLM context).
      if (!isVisibleSegment(seg)) continue;
      latest = trackerRef.current.processSegment(seg.text);
    }
    setSuggestions((prev) => {
      if (latest.length === 0 && prev.length === 0) return prev;
      if (
        latest.length === prev.length &&
        latest.every((s, i) => s.id === prev[i].id && s.confidence === prev[i].confidence)
      ) {
        return prev;
      }
      return latest;
    });
  }, [activeSessionSegments, isRecording]);

  const acceptSuggestion = useCallback(
    async (suggestion: FolderSuggestion) => {
      if (!sessionId) return;
      completedSessionsRef.current.add(sessionId);
      trackerRef.current?.accept(suggestion.id);
      setSuggestions([]);
      await addSessionToFolder(sessionId, suggestion.id);
    },
    [sessionId, addSessionToFolder],
  );

  const applyOverride = useCallback(
    async (folderId: string) => {
      if (!sessionId) return;
      completedSessionsRef.current.add(sessionId);
      trackerRef.current?.complete();
      setSuggestions([]);
      await addSessionToFolder(sessionId, folderId);
    },
    [sessionId, addSessionToFolder],
  );

  const dismissSuggestion = useCallback((suggestion: FolderSuggestion) => {
    trackerRef.current?.dismiss(suggestion.id);
    setSuggestions((prev) => prev.filter((s) => s.id !== suggestion.id));
  }, []);

  return {
    suggestions,
    folders,
    folderTree,
    acceptSuggestion,
    applyOverride,
    dismissSuggestion,
  };
}
