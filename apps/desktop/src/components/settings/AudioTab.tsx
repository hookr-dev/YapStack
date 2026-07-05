import { useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Slider } from "@/components/ui/slider";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Separator } from "@/components/ui/separator";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { ChevronsUpDown, Folder, RotateCcw } from "lucide-react";
import { open } from "@tauri-apps/plugin-dialog";
import { revealAudioFile } from "@/lib/reveal-file";
import { appDataDir } from "@tauri-apps/api/path";
import type { CaptureSourceDto } from "@/lib/tauri";

export function AudioTab() {
  const captureSource = useAppStore((s) => s.settings.captureSource);
  const selectedMicDeviceId = useAppStore((s) => s.settings.selectedMicDeviceId);
  const bufferMaxSeconds = useAppStore((s) => s.settings.bufferMaxSeconds);
  const mixConfig = useAppStore((s) => s.settings.mixConfig);
  const audioSaveLocation = useAppStore((s) => s.settings.audioSaveLocation);
  const audioExportFormat = useAppStore((s) => s.settings.audioExportFormat);
  const mp3Bitrate = useAppStore((s) => s.settings.mp3Bitrate);
  const devices = useAppStore((s) => s.devices);
  const updateSettings = useAppStore((s) => s.updateSettings);
  const [showAdvanced, setShowAdvanced] = useState(false);

  const inputDevices = devices.filter((d) => d.device_type === "Input");

  const handlePickFolder = async () => {
    const selected = await open({ directory: true, multiple: false });
    if (selected) {
      updateSettings({ audioSaveLocation: selected });
    }
  };

  const handleOpenFolder = async () => {
    const folder = audioSaveLocation ?? (await appDataDir()) + "/audio";
    revealAudioFile(folder);
  };

  return (
    <>
      {/* Capture Source + Input Device — source decides whether the mic
          device matters, so it reads left-to-right. */}
      <div className="flex gap-3">
        <div className="flex-1 min-w-0 space-y-2">
          <Label className="text-xs text-muted-foreground">Capture Source</Label>
          <Select
            value={captureSource}
            onValueChange={(v) =>
              updateSettings({ captureSource: v as CaptureSourceDto })
            }
          >
            <SelectTrigger className="h-8 w-full text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="MicOnly" className="text-xs">
                Microphone Only
              </SelectItem>
              <SelectItem value="SystemOnly" className="text-xs">
                System Only
              </SelectItem>
              <SelectItem value="Mixed" className="text-xs">
                Mixed
              </SelectItem>
            </SelectContent>
          </Select>
        </div>

        <div className="flex-1 min-w-0 space-y-2">
          <Label className="text-xs text-muted-foreground">Input Device</Label>
          <Select
            value={selectedMicDeviceId ?? "__default"}
            onValueChange={(v) =>
              updateSettings({ selectedMicDeviceId: v === "__default" ? null : v })
            }
          >
            <SelectTrigger className="h-8 w-full text-xs">
              <SelectValue placeholder="Default" />
            </SelectTrigger>
            <SelectContent>
              {/* Sentinel row (same idiom as onboarding): null means
                  follow-the-OS-default, which Radix can't express as an
                  empty item value. Without this row there's no way back
                  to Default after picking a concrete device. */}
              <SelectItem value="__default" className="text-xs">
                System Default
              </SelectItem>
              {inputDevices.map((device) => (
                <SelectItem
                  key={device.id ?? device.name}
                  value={device.id ?? device.name}
                  className="text-xs"
                >
                  {device.name}
                  {device.is_default ? " (default)" : ""}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>

      {/* Buffer Size */}
      <div className="space-y-2">
        <Label className="text-xs text-muted-foreground">Buffer Size</Label>
        <div className="flex gap-1.5">
          {[60, 120, 300, 600].map((d) => (
            <Button
              key={d}
              size="sm"
              variant={
                bufferMaxSeconds === d ? "default" : "outline"
              }
              className="flex-1 text-xs"
              onClick={() => updateSettings({ bufferMaxSeconds: d })}
            >
              {`${d / 60}m`}
            </Button>
          ))}
        </div>
        <p className="text-[10px] text-muted-foreground/60">
          Maximum audio history kept in memory for rewind
        </p>
      </div>

      <Separator />

      {/* Export Format */}
      <div className="space-y-2">
        <Label className="text-xs text-muted-foreground">Export Format</Label>
        <div className="flex gap-1.5">
          <Button
            size="sm"
            variant={audioExportFormat === "mp3" ? "default" : "outline"}
            className="flex-1 text-xs"
            onClick={() => updateSettings({ audioExportFormat: "mp3" })}
          >
            MP3
          </Button>
          <Button
            size="sm"
            variant={audioExportFormat === "wav" ? "default" : "outline"}
            className="flex-1 text-xs"
            onClick={() => updateSettings({ audioExportFormat: "wav" })}
          >
            WAV
          </Button>
        </div>
        <p className="text-[10px] text-muted-foreground/60">
          {audioExportFormat === "mp3"
            ? "Compressed — significantly smaller files"
            : "Uncompressed — larger files, lossless quality"}
        </p>
      </div>

      {/* MP3 Quality */}
      {audioExportFormat === "mp3" && (
        <div className="space-y-2">
          <Label className="text-xs text-muted-foreground">MP3 Quality</Label>
          <div className="flex gap-1.5">
            {([
              { kbps: 16, label: "16 kbps" },
              { kbps: 32, label: "32 kbps" },
              { kbps: 64, label: "64 kbps" },
              { kbps: 128, label: "128 kbps" },
              { kbps: 192, label: "192 kbps" },
            ] as const).map(({ kbps, label }) => (
              <Button
                key={kbps}
                size="sm"
                variant={mp3Bitrate === kbps ? "default" : "outline"}
                className="flex-1 text-xs"
                onClick={() => updateSettings({ mp3Bitrate: kbps })}
              >
                {label}
              </Button>
            ))}
          </div>
          <p className="text-[10px] text-muted-foreground/60">
            {mp3Bitrate <= 16
              ? "Very low — smallest files, noticeable artifacts"
              : mp3Bitrate <= 32
                ? "Minimum — small files, reduced clarity"
              : mp3Bitrate <= 64
                ? "Best for speech — small files"
                : mp3Bitrate <= 128
                  ? "Balanced quality and file size"
                  : "Higher quality — larger files"}
          </p>
        </div>
      )}

      <Separator />

      {/* Save Location */}
      <div className="space-y-2">
        <Label className="text-xs text-muted-foreground">
          Save Location
        </Label>
        <div className="flex items-center gap-2">
          <div className="flex-1 min-w-0 rounded-md border bg-muted/50 px-2.5 py-1.5">
            <p className="truncate text-xs text-muted-foreground">
              {audioSaveLocation ?? "Default (App Data)"}
            </p>
          </div>
          <Button size="sm" variant="ghost" className="shrink-0 px-2" onClick={handleOpenFolder} aria-label="Open folder">
            <Folder className="h-3.5 w-3.5" />
          </Button>
          <Button size="sm" variant="outline" className="text-xs shrink-0" onClick={handlePickFolder}>
            Change
          </Button>
        </div>
        {audioSaveLocation && (
          <div className="flex items-center gap-2">
            <Button
              variant="inline"
              size="inline"
              onClick={() => updateSettings({ audioSaveLocation: null })}
            >
              <RotateCcw className="h-3 w-3" />
              Reset to default
            </Button>
          </div>
        )}
      </div>

      <Separator />

      {/* Advanced */}
      <Collapsible open={showAdvanced} onOpenChange={setShowAdvanced}>
        <CollapsibleTrigger className="flex w-full items-center justify-between">
          <Label className="text-xs text-muted-foreground pointer-events-none">
            Advanced
          </Label>
          <ChevronsUpDown className="h-3.5 w-3.5 text-muted-foreground/60" />
        </CollapsibleTrigger>
        <CollapsibleContent className="space-y-4 pt-3">
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-[11px] text-muted-foreground">Mic Gain</span>
              <span className="font-mono text-xs text-muted-foreground">
                {mixConfig.mic_gain.toFixed(1)}
              </span>
            </div>
            <Slider
              value={[mixConfig.mic_gain]}
              min={0}
              max={2}
              step={0.1}
              onValueChange={([v]) =>
                updateSettings({
                  mixConfig: { ...mixConfig, mic_gain: v },
                })
              }
            />
          </div>
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-[11px] text-muted-foreground">System Gain</span>
              <span className="font-mono text-xs text-muted-foreground">
                {mixConfig.system_gain.toFixed(1)}
              </span>
            </div>
            <Slider
              value={[mixConfig.system_gain]}
              min={0}
              max={2}
              step={0.1}
              onValueChange={([v]) =>
                updateSettings({
                  mixConfig: { ...mixConfig, system_gain: v },
                })
              }
            />
          </div>
          <p className="text-[10px] text-muted-foreground/60">
            Gain applies to the next recording session
          </p>
        </CollapsibleContent>
      </Collapsible>
    </>
  );
}
