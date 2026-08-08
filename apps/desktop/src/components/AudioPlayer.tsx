import { useState, useRef, useEffect, useCallback, useMemo } from "react";
import { toast } from "sonner";
import { Slider } from "@/components/ui/slider";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { Play, Pause, Circle } from "lucide-react";
import { cn, formatTime } from "@/lib/utils";
import { trackAudioPlaybackStarted } from "@/lib/analytics";

const PLAYBACK_SPEEDS = [0.5, 0.75, 1, 1.25, 1.5, 2] as const;

/**
 * The shared shell for the single row that sits under a session header: the player
 * itself, the fetch/unavailable bar, and the probing placeholder all render at exactly
 * this height. Every one of those states can replace any other while the view is open
 * (probe resolves, fetch completes, bearer expires), so they must be the same height or
 * the whole transcript below jumps. `min-h-10` = 24px (`icon-xs` = `size-6`) of control
 * plus the 8px `py-2` on each side — the tallest variant — so the text-only variants
 * (16px of line box) stop collapsing to 32px.
 */
export const AUDIO_ROW_CLASS = "flex min-h-10 items-center gap-3 border-b px-4 py-2";

export interface AudioPart {
  src: string;
  duration: number;
}

/**
 * Plays an ordered sequence of audio parts as one continuous timeline. Each
 * part is its own file (WAV or MP3); the player swaps the underlying
 * `<audio>` element's src when a part ends or when a seek crosses a boundary.
 *
 * Timeline math:
 *   - global time = SUM(parts[0..partIndex-1].duration) + audio.currentTime
 *   - global duration = SUM(parts.duration)
 *
 * `onTimeUpdate` and `onDurationResolved` operate in global time so callers
 * (transcript timestamp clicks, etc.) don't need to know about parts.
 */
export function AudioPlayer({
  parts,
  onTimeUpdate,
  onPlayStateChange,
  onDurationResolved,
  onResume,
  className,
}: {
  parts: AudioPart[];
  onTimeUpdate?: (time: number) => void;
  onPlayStateChange?: (playing: boolean) => void;
  onDurationResolved?: (d: number) => void;
  /** When provided, renders a Resume button to the left of Play. */
  onResume?: () => void;
  /** Extra classes for the row shell. The caller decides whether this mount deserves a
   *  transition — e.g. the player that replaces a finished fetch bar passes
   *  `view-enter`, while a player that was there all along passes nothing. */
  className?: string;
}) {
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const [partIndex, setPartIndex] = useState(0);
  const [isPlaying, setIsPlaying] = useState(false);
  const [partTime, setPartTime] = useState(0);
  const [speed, setSpeed] = useState(1);
  const rafRef = useRef<number>(0);
  const lastEmitRef = useRef(0);
  const pendingSeekRef = useRef<number | null>(null);
  const wantPlayingAfterSwapRef = useRef(false);

  // Surface playback failures to the user instead of swallowing them to the
  // console. The common cause is a missing/unreadable file (e.g. audio that
  // never reached this device); keep the copy factual, no stack traces.
  const surfacePlaybackError = useCallback(() => {
    toast.error("Couldn't play audio — file missing or unreadable");
  }, []);

  const safePartIndex = Math.min(partIndex, Math.max(parts.length - 1, 0));
  const activePart = parts[safePartIndex];

  // When the parts identity changes (different session opened, parts
  // reloaded after delete) reset to part 0 / time 0. We use the first part's
  // src as the identity so appending a new part during a resume — same
  // session, same first part — does NOT reset playback position.
  const partsIdentityRef = useRef<string | null>(parts[0]?.src ?? null);
  useEffect(() => {
    const next = parts[0]?.src ?? null;
    if (next !== partsIdentityRef.current) {
      partsIdentityRef.current = next;
      setPartIndex(0);
      setPartTime(0);
      pendingSeekRef.current = null;
      wantPlayingAfterSwapRef.current = false;
    }
  }, [parts]);

  const cumulativeBefore = useMemo(() => {
    const out: number[] = [0];
    for (let i = 0; i < parts.length; i++) {
      out.push(out[i] + (parts[i]?.duration ?? 0));
    }
    return out;
  }, [parts]);

  const globalDuration = cumulativeBefore[parts.length] ?? 0;
  const globalCurrentTime = (cumulativeBefore[safePartIndex] ?? 0) + partTime;

  useEffect(() => {
    if (globalDuration > 0) {
      onDurationResolved?.(globalDuration);
    }
  }, [globalDuration, onDurationResolved]);

  const updateTime = useCallback(() => {
    const audio = audioRef.current;
    if (audio) {
      const t = audio.currentTime;
      setPartTime(t);
      const now = performance.now();
      if (now - lastEmitRef.current >= 250) {
        const base = cumulativeBefore[safePartIndex] ?? 0;
        onTimeUpdate?.(base + t);
        lastEmitRef.current = now;
      }
    }
    if (isPlaying) {
      rafRef.current = requestAnimationFrame(updateTime);
    }
  }, [isPlaying, onTimeUpdate, cumulativeBefore, safePartIndex]);

  useEffect(() => {
    if (isPlaying) {
      rafRef.current = requestAnimationFrame(updateTime);
    }
    return () => cancelAnimationFrame(rafRef.current);
  }, [isPlaying, updateTime]);

  // Audio element event wiring. When the active part ends, advance to the
  // next part and continue playback. Cross-part seeks land via
  // `pendingSeekRef`: we change src, then in `loadedmetadata` we apply the
  // pending seek and (if appropriate) resume playback.
  useEffect(() => {
    const audio = audioRef.current;
    if (!audio) return;

    const handleEnded = () => {
      const isLast = safePartIndex >= parts.length - 1;
      if (isLast) {
        setIsPlaying(false);
        onPlayStateChange?.(false);
        const base = cumulativeBefore[safePartIndex] ?? 0;
        onTimeUpdate?.(base + (parts[safePartIndex]?.duration ?? 0));
        return;
      }
      wantPlayingAfterSwapRef.current = true;
      pendingSeekRef.current = 0;
      setPartIndex(safePartIndex + 1);
    };

    const handleSeeked = () => {
      setPartTime(audio.currentTime);
      const base = cumulativeBefore[safePartIndex] ?? 0;
      onTimeUpdate?.(base + audio.currentTime);
    };

    const handlePlay = () => {
      setIsPlaying(true);
      onPlayStateChange?.(true);
    };

    const handleLoadedMetadata = () => {
      const target = pendingSeekRef.current;
      pendingSeekRef.current = null;
      if (target !== null && isFinite(target)) {
        audio.currentTime = Math.max(0, target);
      }
      if (wantPlayingAfterSwapRef.current) {
        wantPlayingAfterSwapRef.current = false;
        audio.play().catch(() => {
          setIsPlaying(false);
          onPlayStateChange?.(false);
        });
      }
    };

    audio.addEventListener("ended", handleEnded);
    audio.addEventListener("seeked", handleSeeked);
    audio.addEventListener("play", handlePlay);
    audio.addEventListener("loadedmetadata", handleLoadedMetadata);
    return () => {
      audio.removeEventListener("ended", handleEnded);
      audio.removeEventListener("seeked", handleSeeked);
      audio.removeEventListener("play", handlePlay);
      audio.removeEventListener("loadedmetadata", handleLoadedMetadata);
    };
  }, [
    safePartIndex,
    parts,
    cumulativeBefore,
    onPlayStateChange,
    onTimeUpdate,
  ]);

  // Apply playback rate when speed changes or src swaps in.
  useEffect(() => {
    const audio = audioRef.current;
    if (audio) audio.playbackRate = speed;
  }, [speed, safePartIndex]);

  const togglePlay = () => {
    const audio = audioRef.current;
    if (!audio || parts.length === 0) return;

    if (isPlaying) {
      audio.pause();
      setIsPlaying(false);
      onPlayStateChange?.(false);
    } else {
      audio.play().catch(() => {
        surfacePlaybackError();
        setIsPlaying(false);
        onPlayStateChange?.(false);
      });
      setIsPlaying(true);
      onPlayStateChange?.(true);
      trackAudioPlaybackStarted({ duration_seconds: Math.round(globalDuration) });
    }
  };

  const seekToGlobal = useCallback(
    (globalTime: number, options?: { autoPlay?: boolean }) => {
      if (parts.length === 0) return;
      const clamped = Math.max(0, Math.min(globalTime, globalDuration));
      let newIndex = parts.length - 1;
      for (let i = 0; i < parts.length; i++) {
        if (clamped < cumulativeBefore[i + 1]) {
          newIndex = i;
          break;
        }
      }
      const newPartTime = clamped - cumulativeBefore[newIndex];
      const wantPlay = options?.autoPlay ?? false;

      const audio = audioRef.current;
      if (newIndex !== safePartIndex) {
        // Cross-part seek: src swap is async. Stash the seek + play intent;
        // the loadedmetadata handler applies them once the new part loads.
        wantPlayingAfterSwapRef.current = wantPlay || isPlaying;
        pendingSeekRef.current = newPartTime;
        setPartIndex(newIndex);
      } else if (audio) {
        audio.currentTime = newPartTime;
        setPartTime(newPartTime);
        onTimeUpdate?.(clamped);
        if (wantPlay && audio.paused) {
          audio.play().catch(() => {
            surfacePlaybackError();
            setIsPlaying(false);
            onPlayStateChange?.(false);
          });
        }
      }
    },
    [
      parts.length,
      cumulativeBefore,
      globalDuration,
      safePartIndex,
      isPlaying,
      onTimeUpdate,
      onPlayStateChange,
      surfacePlaybackError,
    ],
  );

  const handleSeekSlider = useCallback(
    (value: number[]) => {
      // Slider drag preserves play state; never force-plays.
      seekToGlobal(value[0] ?? 0);
    },
    [seekToGlobal],
  );

  const cycleSpeed = () => {
    const idx = PLAYBACK_SPEEDS.indexOf(speed as (typeof PLAYBACK_SPEEDS)[number]);
    const nextIdx = (idx + 1) % PLAYBACK_SPEEDS.length;
    setSpeed(PLAYBACK_SPEEDS[nextIdx]);
  };

  // Expose `seekTo(globalTime, options?)` on the audio element so external
  // callers (transcript timestamp clicks) can drive playback without knowing
  // about the parts internals. `autoPlay` defaults to false to match slider
  // drag semantics; transcript clicks pass true.
  useEffect(() => {
    if (audioRef.current) {
      (
        audioRef.current as HTMLAudioElement & {
          seekTo?: (t: number, options?: { autoPlay?: boolean }) => void;
        }
      ).seekTo = seekToGlobal;
    }
  }, [seekToGlobal]);

  return (
    <div className={cn(AUDIO_ROW_CLASS, className)}>
      <audio
        ref={audioRef}
        src={activePart?.src ?? ""}
        preload="metadata"
        data-session-audio
      />

      {onResume && (
        <>
          <Button
            variant="ghost"
            size="icon-xs"
            onClick={() => {
              const audio = audioRef.current;
              if (audio && !audio.paused) {
                audio.pause();
                setIsPlaying(false);
                onPlayStateChange?.(false);
              }
              onResume();
            }}
            className="text-red-500 hover:text-red-500 hover:bg-red-500/10"
            title="Resume recording"
          >
            <Circle className="h-3 w-3 fill-current" />
          </Button>
          <Separator orientation="vertical" className="!h-5" />
        </>
      )}

      <Button variant="ghost" size="icon-xs" onClick={togglePlay}>
        {isPlaying ? (
          <Pause className="h-4 w-4" />
        ) : (
          <Play className="h-4 w-4" />
        )}
      </Button>

      <span className="text-xs tabular-nums text-muted-foreground w-10 text-right">
        {formatTime(globalCurrentTime)}
      </span>

      <Slider
        className="flex-1"
        min={0}
        max={globalDuration}
        step={0.1}
        value={[globalCurrentTime]}
        onValueChange={handleSeekSlider}
      />

      <span className="text-xs tabular-nums text-muted-foreground w-10">
        {formatTime(globalDuration)}
      </span>

      <button
        onClick={cycleSpeed}
        className="text-xs tabular-nums text-muted-foreground hover:text-foreground transition-colors min-w-[2.5rem] text-center"
      >
        {speed}x
      </button>
    </div>
  );
}
