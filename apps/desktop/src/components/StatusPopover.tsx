import { useEffect, useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Progress } from "@/components/ui/progress";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { Button } from "@/components/ui/button";
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from "@/components/ui/tabs";
import { LogsPanel } from "@/components/LogsPanel";
import { formatBytes, formatElapsed, SOURCE_LABELS_FULL } from "@/lib/utils";
import { accelDisplayLabel } from "@/lib/accel";
import { commands } from "@/lib/tauri";
import type {
  LiveTranscriptionStatus,
  RingBufferInfoDto,
} from "@/lib/tauri";
import {
  Mic,
  Speaker,
  Activity,
  HardDrive,
  Cpu,
  AlertCircle,
  Radio,
  Copy,
  Hash,
} from "lucide-react";
import { toast } from "sonner";

type Tone = "green" | "amber" | "red" | "gray";

const DOT_BG: Record<Tone, string> = {
  green: "bg-green-500",
  amber: "bg-yellow-500",
  red: "bg-red-500",
  gray: "bg-muted-foreground/40",
};

const TINT_BG: Record<Tone, string> = {
  green: "bg-green-500/5 border-green-500/20",
  amber: "bg-yellow-500/5 border-yellow-500/20",
  red: "bg-red-500/5 border-red-500/20",
  gray: "bg-muted/30 border-border",
};

function LevelMeter({
  label,
  rms,
  Icon,
  active,
}: {
  label: string;
  rms: number | null;
  Icon: React.ElementType;
  active: boolean;
}) {
  // RMS for speech sits around 0.01–0.5. Square-root curve compresses the
  // dynamic range so the meter responds vividly to typical voice levels.
  const pct =
    !active || rms == null ? 0 : Math.min(100, Math.sqrt(rms) * 140);
  return (
    <div className="flex items-center gap-2">
      <Icon
        className={`h-3 w-3 shrink-0 ${active ? "text-foreground" : "text-muted-foreground/40"}`}
      />
      <span
        className={`w-12 shrink-0 text-[10px] ${active ? "text-muted-foreground" : "text-muted-foreground/40"}`}
      >
        {label}
      </span>
      <div className="relative h-1.5 flex-1 overflow-hidden rounded-full bg-muted">
        <div
          className={`h-full transition-[width] duration-100 ${active ? "bg-green-500/70" : "bg-muted-foreground/20"}`}
          style={{ width: `${pct}%` }}
        />
      </div>
      <span className="w-9 shrink-0 text-right font-mono text-[10px] tabular-nums text-muted-foreground/60">
        {active && rms != null ? (rms * 100).toFixed(1) : "—"}
      </span>
    </div>
  );
}

function BufferRow({
  label,
  info,
}: {
  label: string;
  info: RingBufferInfoDto | null;
}) {
  if (!info) {
    return (
      <div className="flex items-center justify-between">
        <span className="text-[11px] font-medium text-muted-foreground/70">
          {label}
        </span>
        <span className="text-[10px] text-muted-foreground/40">inactive</span>
      </div>
    );
  }
  const pct =
    info.capacity_seconds > 0
      ? (info.available_seconds / info.capacity_seconds) * 100
      : 0;
  // Ring buffer stores f32 samples — 4 bytes per sample.
  const memBytes = info.capacity_samples * 4;
  return (
    <div className="space-y-1">
      <div className="flex items-center justify-between">
        <span className="text-[11px] font-medium">{label}</span>
        <span className="font-mono text-[10px] tabular-nums">
          {info.available_seconds.toFixed(1)}s
          <span className="text-muted-foreground/50">
            {" "}
            / {Math.floor(info.capacity_seconds)}s
          </span>
        </span>
      </div>
      <Progress value={pct} className="h-1" />
      <div className="flex items-center gap-1.5 font-mono text-[10px] tabular-nums text-muted-foreground/60">
        <span>{(info.sample_rate / 1000).toFixed(1)}kHz</span>
        <span className="text-muted-foreground/30">·</span>
        <span>{info.channels}ch</span>
        <span className="text-muted-foreground/30">·</span>
        <span>{formatBytes(memBytes)}</span>
      </div>
    </div>
  );
}

/**
 * Section wrapper. Header is `icon + UPPERCASE LABEL` on the left with an
 * optional right-side slot (e.g. a source chip). Section content flows
 * flush-left below — no indent, no visual offset. The icon acts as a
 * section marker above the column; content lives on the same axis as
 * everything else in the popover.
 */
function Section({
  icon: Icon,
  label,
  right,
  children,
  className,
}: {
  icon: React.ElementType;
  label: React.ReactNode;
  right?: React.ReactNode;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <section className={`space-y-1.5 ${className ?? ""}`}>
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-1.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
          <Icon className="h-2.5 w-2.5" />
          {label}
        </div>
        {right}
      </div>
      {children}
    </section>
  );
}

export function StatusPopover() {
  const enginePhase = useAppStore((s) => s.enginePhase);
  const engineError = useAppStore((s) => s.engineError);
  const modelDownloadProgress = useAppStore((s) => s.modelDownloadProgress);
  const bufferInfo = useAppStore((s) => s.bufferInfo);
  const captureStatus = useAppStore((s) => s.captureStatus);
  const captureSource = useAppStore((s) => s.settings.captureSource);
  const selectedMicDeviceId = useAppStore(
    (s) => s.settings.selectedMicDeviceId,
  );
  const devices = useAppStore((s) => s.devices);
  const selectedEngine = useAppStore((s) => s.settings.selectedEngine);
  const selectedModelSize = useAppStore((s) => s.settings.selectedModelSize);
  const selectedParakeetVariant = useAppStore(
    (s) => s.settings.selectedParakeetVariant,
  );
  const diarizationEnabled = useAppStore(
    (s) => s.settings.diarizationEnabled,
  );
  const loadedEngineInfo = useAppStore((s) => s.loadedEngineInfo);
  const liveActive = useAppStore((s) => s.liveTranscriptionActive);
  const livePhase = useAppStore((s) => s.livePhase);
  const backfillActive = useAppStore((s) => s.backfillActive);
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const activeSessionStartTime = useAppStore((s) => s.activeSessionStartTime);
  const activeSessionSegments = useAppStore((s) => s.activeSessionSegments);

  const [rms, setRms] = useState<{
    mic: number | null;
    system: number | null;
  }>({ mic: null, system: null });
  const [liveStatus, setLiveStatus] =
    useState<LiveTranscriptionStatus | null>(null);
  const [now, setNow] = useState(Date.now());

  useEffect(() => {
    let cancelled = false;
    const rmsTimer = window.setInterval(async () => {
      try {
        const r = await commands.peekCaptureEnergy(0.2);
        if (!cancelled && r.status === "ok") {
          setRms({ mic: r.data.mic_rms, system: r.data.system_rms });
        }
      } catch {
        /* probe fails while engine is setting up; ignore */
      }
    }, 100);
    const statusTimer = window.setInterval(async () => {
      try {
        const r = await commands.getLiveTranscriptionStatus("session");
        if (!cancelled && r.status === "ok") setLiveStatus(r.data);
      } catch {
        /* ignore */
      }
    }, 1000);
    const clockTimer = window.setInterval(
      () => setNow(Date.now()),
      1000,
    );
    return () => {
      cancelled = true;
      window.clearInterval(rmsTimer);
      window.clearInterval(statusTimer);
      window.clearInterval(clockTimer);
    };
  }, []);

  const deviceName = selectedMicDeviceId
    ? (devices.find((d) => d.id === selectedMicDeviceId) ??
        devices.find((d) => d.name === selectedMicDeviceId))?.name
    : null;
  const sourceLabel = SOURCE_LABELS_FULL[captureSource];

  // Parakeet variants intentionally collapse to one user-facing label —
  // int8 vs fp32 is an implementation detail driven by host capability,
  // not a user-visible choice.
  const modelLabel =
    selectedEngine === "Whisper"
      ? selectedModelSize
      : selectedParakeetVariant === "TdtV3" ||
          selectedParakeetVariant === "TdtV3Int8"
        ? "TDT v3"
        : selectedParakeetVariant;

  const micActive = captureStatus?.mic_active ?? false;
  const systemActive = captureStatus?.system_audio_active ?? false;

  let headerTitle = "Idle";
  let headerSub = "Not capturing";
  let tone: Tone = "gray";
  if (enginePhase === "downloading") {
    headerTitle = "Downloading model";
    headerSub = `${Math.round(modelDownloadProgress ?? 0)}%`;
    tone = "amber";
  } else if (enginePhase === "initializing") {
    headerTitle = "Loading engine";
    headerSub = "Preparing transcription runtime";
    tone = "amber";
  } else if (enginePhase === "error") {
    headerTitle = "Engine error";
    headerSub = engineError ?? "Unknown";
    tone = "red";
  } else if (enginePhase === "ready") {
    if (captureStatus?.state === "Capturing") {
      headerTitle = "Listening";
      headerSub = liveActive
        ? "Live transcription active"
        : "Capturing audio";
      tone = "green";
    } else if (captureStatus?.state === "Error") {
      headerTitle = "Capture error";
      headerSub = captureStatus.error_message ?? "Unknown capture fault";
      tone = "red";
    } else {
      headerTitle = "Ready";
      headerSub = "Engine loaded, not capturing";
      tone = "amber";
    }
  } else {
    headerTitle = "Setting up";
    headerSub = "Initializing…";
    tone = "gray";
  }

  const displayError =
    captureStatus?.state === "Error" ? null : engineError ?? null;

  const elapsedMs = activeSessionStartTime
    ? Math.max(0, now - activeSessionStartTime)
    : 0;
  const sessionIdShort = activeSessionId?.slice(0, 8) ?? null;

  const copyDebug = async () => {
    const lines = [
      `Phase: ${headerTitle} — ${headerSub}`,
      `Engine: ${selectedEngine} / ${modelLabel}${
        selectedEngine === "Parakeet" && diarizationEnabled
          ? " + Diarization"
          : ""
      }${
        loadedEngineInfo?.accel
          ? `   Accel: ${loadedEngineInfo.accel}${
              loadedEngineInfo.accelFallback
                ? ` [fallback: ${loadedEngineInfo.accelFallback}]`
                : ""
            }${loadedEngineInfo.accelPartial ? " [partial]" : ""}${
              loadedEngineInfo.modelDir
                ? ` (${loadedEngineInfo.modelDir
                    .split(/[/\\]/)
                    .filter(Boolean)
                    .pop()})`
                : ""
            }`
          : ""
      }`,
      `Capture source: ${sourceLabel}`,
      `Mic device: ${deviceName ?? "(default)"}`,
      `Mic active: ${micActive}   System active: ${systemActive}`,
      ``,
      `Mic buffer: ${
        bufferInfo?.mic
          ? `${bufferInfo.mic.available_seconds.toFixed(1)}s / ${Math.floor(
              bufferInfo.mic.capacity_seconds,
            )}s @ ${bufferInfo.mic.sample_rate}Hz x${bufferInfo.mic.channels}`
          : "n/a"
      }`,
      `Sys buffer: ${
        bufferInfo?.system
          ? `${bufferInfo.system.available_seconds.toFixed(
              1,
            )}s / ${Math.floor(bufferInfo.system.capacity_seconds)}s @ ${bufferInfo.system.sample_rate}Hz x${bufferInfo.system.channels}`
          : "n/a"
      }`,
      ``,
      `Live: active=${liveActive} phase=${livePhase ?? "-"} backfill=${backfillActive}`,
      activeSessionId
        ? `Session: ${activeSessionId} (${formatElapsed(elapsedMs)}, ${activeSessionSegments.length} segments)`
        : `Session: (none)`,
      liveStatus
        ? `Chunks: ${liveStatus.chunks_processed}   Audio: ${liveStatus.total_audio_seconds.toFixed(1)}s   Lag: ${
            liveStatus.lag_seconds == null
              ? "—"
              : `${liveStatus.lag_seconds.toFixed(1)}s`
          }   Drain backlog: ${liveStatus.live_drain_backlog_chunks}× / ${liveStatus.live_drain_backlog_seconds.toFixed(1)}s`
        : `Chunks: n/a`,
      engineError ? `Engine error: ${engineError}` : "",
      captureStatus?.error_message
        ? `Capture error: ${captureStatus.error_message}`
        : "",
    ]
      .filter(Boolean)
      .join("\n");
    try {
      await navigator.clipboard.writeText(lines);
      toast.success("Debug info copied");
    } catch {
      toast.error("Failed to copy");
    }
  };

  return (
    <div className="w-[420px]">
      {/* Header */}
      <div
        className={`flex items-center gap-2 border-b px-3 py-2.5 ${TINT_BG[tone]}`}
      >
        <span
          className={`inline-block h-2.5 w-2.5 rounded-full ${DOT_BG[tone]} ${
            tone === "green" ? "animate-pulse" : ""
          }`}
          aria-hidden
        />
        <div className="min-w-0 flex-1">
          <p className="truncate text-xs font-semibold">{headerTitle}</p>
          <p className="truncate text-[10px] text-muted-foreground">
            {headerSub}
          </p>
        </div>
        {backfillActive && (
          <Badge className="border-blue-500/30 bg-blue-500/15 px-1.5 py-0 text-[9px] text-blue-500">
            Backfilling
          </Badge>
        )}
      </div>

      {displayError && (
        <div className="mx-3 mt-2.5 flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/10 px-2 py-1.5">
          <AlertCircle className="mt-0.5 h-3 w-3 shrink-0 text-destructive" />
          <p className="text-[11px] leading-relaxed text-destructive">
            {displayError}
          </p>
        </div>
      )}

      <Tabs defaultValue="metrics" className="gap-0">
        <TabsList className="mx-3 mb-0 mt-2 grid h-8 grid-cols-2">
          <TabsTrigger value="metrics" className="h-6 text-[11px]">
            Metrics
          </TabsTrigger>
          <TabsTrigger value="logs" className="h-6 text-[11px]">
            Logs
          </TabsTrigger>
        </TabsList>

        <TabsContent value="metrics" className="mt-0">
          <div className="space-y-3 px-3 py-2.5">
            {/* Engine */}
            <Section icon={Cpu} label="Engine">
              <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-[11px]">
                <span className="font-medium">{selectedEngine}</span>
                <span className="text-muted-foreground">{modelLabel}</span>
                {selectedEngine === "Parakeet" && diarizationEnabled && (
                  <span className="text-amber-600 dark:text-amber-400">+ Diarization</span>
                )}
              </div>
              {loadedEngineInfo?.accel && (
                <p className="mt-0.5 text-[10px] text-muted-foreground/80">
                  <span className="text-muted-foreground/50">Accelerator:</span>{" "}
                  <span
                    className="font-mono"
                    title={loadedEngineInfo.accelFallback ?? undefined}
                  >
                    {accelDisplayLabel(
                      loadedEngineInfo.accel,
                      loadedEngineInfo.accelFallback,
                      loadedEngineInfo.accelPartial,
                    )}
                  </span>
                  {loadedEngineInfo.modelDir && (
                    <span className="ml-2 text-muted-foreground/50">
                      ({/* show only the trailing dir name (e.g. parakeet-tdt-v3-int8) */}
                      {loadedEngineInfo.modelDir
                        .split(/[/\\]/)
                        .filter(Boolean)
                        .pop()}
                      )
                    </span>
                  )}
                </p>
              )}
            </Section>

            {/* Capture + levels */}
            <Section
              icon={Radio}
              label="Capture"
              right={
                <span className="text-[10px] text-muted-foreground">
                  {sourceLabel}
                </span>
              }
            >
              {micActive && (
                <p className="truncate text-[10px] text-muted-foreground/80">
                  <span className="text-muted-foreground/50">Device:</span>{" "}
                  {deviceName ?? "Default"}
                </p>
              )}
              <div className="mt-1.5 space-y-1">
                <LevelMeter
                  label="Mic"
                  rms={rms.mic}
                  Icon={Mic}
                  active={micActive}
                />
                <LevelMeter
                  label="System"
                  rms={rms.system}
                  Icon={Speaker}
                  active={systemActive}
                />
              </div>
            </Section>

            <Separator />

            {/* Ring buffers */}
            <Section icon={HardDrive} label="Ring Buffers" className="space-y-2">
              <div className="space-y-2">
                <BufferRow label="Microphone" info={bufferInfo?.mic ?? null} />
                <BufferRow
                  label="System Audio"
                  info={bufferInfo?.system ?? null}
                />
              </div>
            </Section>

            {/* Live session (only when active) */}
            {liveActive && (
              <>
                <Separator />
                <Section icon={Activity} label="Live Session">
                  <div className="grid grid-cols-2 gap-x-3 gap-y-1 text-[11px]">
                    <div className="flex items-center justify-between">
                      <span className="text-muted-foreground/70">Phase</span>
                      <span className="font-mono tabular-nums">
                        {livePhase ?? "—"}
                      </span>
                    </div>
                    <div className="flex items-center justify-between">
                      <span className="text-muted-foreground/70">Elapsed</span>
                      <span className="font-mono tabular-nums">
                        {activeSessionStartTime
                          ? formatElapsed(elapsedMs)
                          : "—"}
                      </span>
                    </div>
                    <div className="flex items-center justify-between">
                      <span className="text-muted-foreground/70">
                        Segments
                      </span>
                      <span className="font-mono tabular-nums">
                        {activeSessionSegments.length}
                      </span>
                    </div>
                    <div className="flex items-center justify-between">
                      <span className="text-muted-foreground/70">Chunks</span>
                      <span className="font-mono tabular-nums">
                        {liveStatus?.chunks_processed ?? 0}
                      </span>
                    </div>
                    <div className="col-span-2 flex items-center justify-between">
                      <span className="text-muted-foreground/70">
                        Audio processed
                      </span>
                      <span className="font-mono tabular-nums">
                        {(liveStatus?.total_audio_seconds ?? 0).toFixed(1)}s
                      </span>
                    </div>
                    <div className="col-span-2 flex items-center justify-between">
                      <span className="text-muted-foreground/70">
                        Lag (live → real time)
                      </span>
                      <span
                        className={`font-mono tabular-nums ${
                          (liveStatus?.lag_seconds ?? 0) > 5
                            ? "text-amber-600 dark:text-amber-400"
                            : ""
                        }`}
                      >
                        {liveStatus?.lag_seconds == null
                          ? "—"
                          : `${liveStatus.lag_seconds.toFixed(1)}s`}
                      </span>
                    </div>
                    {(liveStatus?.live_drain_backlog_chunks ?? 0) > 0 && (
                      <div className="col-span-2 flex items-center justify-between">
                        <span className="text-amber-700 dark:text-amber-400">
                          Live drain backlog
                        </span>
                        <span className="font-mono tabular-nums text-amber-700 dark:text-amber-400">
                          {liveStatus?.live_drain_backlog_chunks ?? 0}× ·{" "}
                          {(
                            liveStatus?.live_drain_backlog_seconds ?? 0
                          ).toFixed(1)}
                          s
                        </span>
                      </div>
                    )}
                    {sessionIdShort && (
                      <div className="col-span-2 flex items-center justify-between">
                        <span className="flex items-center gap-1 text-muted-foreground/70">
                          <Hash className="h-2.5 w-2.5" />
                          Session
                        </span>
                        <span className="font-mono text-[10px] text-muted-foreground">
                          {sessionIdShort}
                        </span>
                      </div>
                    )}
                  </div>
                </Section>
              </>
            )}

            <Separator />

            <Button
              variant="ghost"
              size="sm"
              className="h-6 w-full text-[10px]"
              onClick={copyDebug}
            >
              <Copy className="mr-1 h-3 w-3" />
              Copy debug info
            </Button>
          </div>
        </TabsContent>

        <TabsContent value="logs" className="mt-2">
          <LogsPanel />
        </TabsContent>
      </Tabs>
    </div>
  );
}
