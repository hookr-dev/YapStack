import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { convertFileSrc } from "@tauri-apps/api/core";
import { revealAudioFile } from "@/lib/reveal-file";
import { useAppStore } from "@/stores/appStore";
import { commands } from "@/lib/tauri";
import {
  createManualSession as dbCreateManualSession,
  saveNote,
  updateDictationHistorySessionId,
  type DbDictationHistory,
} from "@/lib/db";
import { markdownToBasicHtml } from "@/lib/ai";
import {
  deriveTrackFetch,
  syncCommands,
  AUTH_EXPIRED_FETCH_COPY,
  ON_DEVICE_REPROBE_MS,
  type TrackFetch,
} from "@/lib/sync";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

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
  // S3 fetch-on-demand state for dictation audio missing on this device (null = not fetching).
  const [fetchState, setFetchState] = useState<TrackFetch | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const fetchAbortRef = useRef(false);

  // Pause + release on unmount. Detach `onerror` BEFORE clearing `src` so
  // resetting the element to an empty source doesn't fire the missing-file
  // toast on teardown. Navigate-away parity with the session player: a
  // still-QUEUED auto-fetch is dropped (it never started); an in-flight
  // download is left to complete into the cache.
  useEffect(() => {
    const dictationId = entry.id;
    return () => {
      fetchAbortRef.current = true;
      void syncCommands.releaseAudioPart(dictationId).catch(() => {});
      const audio = audioRef.current;
      if (audio) {
        audio.onerror = null;
        audio.onended = null;
        audio.pause();
        audio.src = "";
        audioRef.current = null;
      }
    };
  }, [entry.id]);

  // Start playback of an already-resolved audio-stream:// src.
  const playSrc = (src: string) => {
    const audio = new Audio(src);
    audio.onended = () => {
      setPlaying(false);
      audioRef.current = null;
    };
    audio.onerror = () => {
      setPlaying(false);
      audioRef.current = null;
      toast.error("Couldn't play audio — file missing or unreadable");
    };
    audioRef.current = audio;
    setPlaying(true);
    audio.play().catch(() => {
      if (audioRef.current === audio) {
        audioRef.current = null;
        setPlaying(false);
      }
    });
  };

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(entry.output_text);
      toast.success("Copied to clipboard");
    } catch {
      toast.error("Failed to copy");
    }
  };

  /**
   * D2 step 3: the fetch-on-demand poll loop (dictation part_id == dictation_history.id).
   * `autoPlay` plays the cached file when it lands (the explicit play-click path);
   * `quiet` suppresses the terminal toasts (the mount-time auto-fetch — a silentLY
   * prefetching list row must not toast for every entry; the fetchState surface + the
   * explicit play path keep every terminal honest and visible).
   */
  const runFetchLoop = useCallback(
    async (opts: { autoPlay: boolean; quiet: boolean }) => {
      fetchAbortRef.current = false;
      setFetchState({
        kind: "fetching",
        percent: 0,
        received: 0,
        total: 0,
        currentPart: 1,
        totalParts: 1,
      });
      for (;;) {
        if (fetchAbortRef.current) {
          setFetchState(null);
          return;
        }
        let st;
        try {
          st = await syncCommands.prepareAudioPart(entry.id);
        } catch (e) {
          setFetchState({ kind: "error", message: String(e) });
          if (!opts.quiet) toast.error("Couldn't fetch audio from sync");
          return;
        }
        const track = deriveTrackFetch([st]);
        setFetchState(track);
        if (track.kind === "ready" && st.state === "ready") {
          setFetchState(null);
          if (opts.autoPlay) playSrc(convertFileSrc(st.path, "audio-stream"));
          return;
        }
        if (track.kind === "fetching" || track.kind === "queued") {
          await sleep(600);
          continue;
        }
        // Quiet-mode 30s re-probe (parity with the session view): while the entry stays
        // open and the server lacks the blob (or the bearer is expired — the drain
        // refreshes it on its own), keep re-probing on the slow cadence so the fetch
        // self-starts once the upload/refresh lands. Toasts stay suppressed; the wait is
        // stateless (fetchState null) and the abort flag at the loop top ends it on
        // unmount/cancel.
        if (
          opts.quiet &&
          (track.kind === "on_device" || track.kind === "auth_expired")
        ) {
          setFetchState(null);
          await sleep(ON_DEVICE_REPROBE_MS);
          continue;
        }
        // Terminal, non-ready: honest toast (explicit path), never silent — the quiet
        // auto-prefetch leaves the terminal on fetchState instead.
        if (!opts.quiet) {
          toast.error(
            track.kind === "on_device"
              ? "Audio isn't backed up to sync yet"
              : track.kind === "unreachable"
                ? "Can't reach sync server"
                : track.kind === "auth_expired"
                  ? AUTH_EXPIRED_FETCH_COPY
                  : track.kind === "verification_failed"
                    ? "Audio failed verification"
                    : track.kind === "no_space"
                      ? "Not enough disk space to fetch audio"
                      : "Couldn't fetch audio from sync",
          );
        }
        return;
      }
    },
    // playSrc is stable per render usage (defined in this hook); entry.id keys the loop.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [entry.id],
  );

  // S3.5 auto-fetch: start fetching the moment the entry mounts (not on first play click)
  // when the WAV is not on this device — same exclusions as the play path (local probe
  // short-circuits; sync-off/no-blob resolves to a quiet on_device). Cache-hit + the Rust
  // single-flight + global cap make a re-mount idempotent and a long list bounded.
  useEffect(() => {
    let disposed = false;
    const kick = async () => {
      if (!entry.wav_file_path) return;
      let localOk = true;
      try {
        const [exists] = await commands.audioFilesExist([entry.wav_file_path]);
        localOk = !!exists;
      } catch {
        // Probe unavailable (older backend) → assume local; nothing to prefetch.
        localOk = true;
      }
      if (disposed || localOk) return;
      void runFetchLoop({ autoPlay: false, quiet: true });
    };
    void kick();
    return () => {
      disposed = true;
    };
  }, [entry.id, entry.wav_file_path, runFetchLoop]);

  const handlePlayAudio = async () => {
    if (!entry.wav_file_path) return;
    if (playing && audioRef.current) {
      audioRef.current.pause();
      audioRef.current = null;
      setPlaying(false);
      return;
    }
    // D2 step 2 (same-device fast path): if the durable wav_file_path resolves on THIS device,
    // play it directly — synthesizing `{entry.id}.{ext}` would miss the part index / custom
    // audio_save_location.
    let localOk = false;
    try {
      const [exists] = await commands.audioFilesExist([entry.wav_file_path]);
      localOk = !!exists;
    } catch {
      // Probe unavailable (older backend) → assume local; the element's onerror still catches.
      localOk = true;
    }
    if (localOk) {
      playSrc(convertFileSrc(entry.wav_file_path, "audio-stream"));
      return;
    }

    // Not local → run the loop loudly and play on landing (joins the auto-fetch's
    // download if one is already in flight — single-flight in Rust).
    await runFetchLoop({ autoPlay: true, quiet: false });
  };

  const cancelFetch = () => {
    fetchAbortRef.current = true;
    void syncCommands.cancelAudioPart(entry.id);
    setFetchState(null);
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
    /** S3: non-null while a missing dictation's audio is being fetched from sync. */
    fetchState,
    cancelFetch,
    handleCopy,
    handlePlayAudio,
    handleMoveToNote,
    handleOpenNote,
    handleDelete,
    handleShowFile,
  };
}
