import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { convertFileSrc } from "@tauri-apps/api/core";
import { revealAudioFile } from "@/lib/reveal-file";
import { useAppStore } from "@/stores/appStore";
import {
  createManualSession as dbCreateManualSession,
  saveNote,
  updateDictationHistorySessionId,
  type DbDictationHistory,
} from "@/lib/db";
import { markdownToBasicHtml } from "@/lib/ai";

/// Shared state and handlers for any UI surface that renders a single
/// `DictationHistory` entry. Owns the play/pause audio toggle, clipboard
/// copy, "move to note" (creates a manual session bound to the entry), and
/// delete. The audio element is paused and released on unmount so clicking
/// play and then navigating away doesn't leak playback.
export function useDictationEntry(entry: DbDictationHistory) {
  const deleteEntry = useAppStore((s) => s.deleteDictationHistoryEntry);
  const loadSessions = useAppStore((s) => s.loadSessions);
  const openSession = useAppStore((s) => s.openSession);
  const loadDictationHistory = useAppStore((s) => s.loadDictationHistory);

  const [playing, setPlaying] = useState(false);
  const audioRef = useRef<HTMLAudioElement | null>(null);

  // Pause + release on unmount. Detach `onerror` BEFORE clearing `src` so
  // resetting the element to an empty source doesn't fire the missing-file
  // toast on teardown.
  useEffect(() => {
    return () => {
      const audio = audioRef.current;
      if (audio) {
        audio.onerror = null;
        audio.onended = null;
        audio.pause();
        audio.src = "";
        audioRef.current = null;
      }
    };
  }, []);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(entry.output_text);
      toast.success("Copied to clipboard");
    } catch {
      toast.error("Failed to copy");
    }
  };

  const handlePlayAudio = () => {
    if (!entry.wav_file_path) return;
    if (playing && audioRef.current) {
      audioRef.current.pause();
      audioRef.current = null;
      setPlaying(false);
      return;
    }
    // Use the durable wav_file_path on the dictation_history row so we hit
    // the actual {dictation_id}.{part_index}.{wav|mp3} file under the
    // session's tracked audio dir — synthesizing `{entry.id}.{ext}` would
    // miss the part index and any custom audio_save_location.
    const audio = new Audio(convertFileSrc(entry.wav_file_path, "audio-stream"));
    audio.onended = () => {
      setPlaying(false);
      audioRef.current = null;
    };
    audio.onerror = () => {
      // Fires when the media element can't load/decode the source — most often
      // a missing or unreadable file (e.g. dictation audio that never reached
      // this device). Surface it instead of resetting state silently.
      setPlaying(false);
      audioRef.current = null;
      toast.error("Couldn't play audio — file missing or unreadable");
    };
    audioRef.current = audio;
    setPlaying(true);
    // `audio.play()` returns a Promise that rejects if playback was
    // interrupted (e.g. user clicked stop before it started). Swallow it so
    // the rejection doesn't surface as an unhandled-promise warning, and
    // unwind the playing flag if the start itself failed.
    audio.play().catch(() => {
      if (audioRef.current === audio) {
        audioRef.current = null;
        setPlaying(false);
      }
    });
  };

  const handleMoveToNote = async () => {
    try {
      const sessionId = crypto.randomUUID();
      const title = entry.output_text.slice(0, 60);
      await dbCreateManualSession(sessionId, title);
      await saveNote(sessionId, markdownToBasicHtml(entry.output_text));
      await updateDictationHistorySessionId(entry.id, sessionId);
      await loadSessions();
      await loadDictationHistory();
      await openSession(sessionId);
      toast.success("Moved to note");
    } catch (e) {
      console.error("Failed to move to note:", e);
      toast.error("Failed to create note");
    }
  };

  const handleOpenNote = () => {
    if (entry.session_id) {
      openSession(entry.session_id);
    }
  };

  const handleDelete = () => {
    deleteEntry(entry.id);
  };

  const handleShowFile = () => {
    if (!entry.wav_file_path) return;
    void revealAudioFile(entry.wav_file_path);
  };

  return {
    playing,
    handleCopy,
    handlePlayAudio,
    handleMoveToNote,
    handleOpenNote,
    handleDelete,
    handleShowFile,
  };
}
