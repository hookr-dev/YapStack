import { useEffect, useMemo, useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { ChevronsUpDown, Info, RotateCw, Trash2 } from "lucide-react";
import { toast } from "sonner";
import { formatBytes } from "@/lib/utils";
import { ParakeetModelSection } from "./ParakeetModelSection";

/// Friendly display labels for the language codes the engine catalogue
/// returns. Codes not in this map fall back to displaying the raw code so
/// the dropdown stays usable as the catalogue grows.
const LANGUAGE_LABELS: Record<string, string> = {
  en: "English",
  es: "Spanish",
  fr: "French",
  de: "German",
  ja: "Japanese",
  zh: "Chinese",
  ko: "Korean",
  pt: "Portuguese",
  it: "Italian",
  ru: "Russian",
  nl: "Dutch",
  pl: "Polish",
  sv: "Swedish",
  no: "Norwegian",
  da: "Danish",
  fi: "Finnish",
  cs: "Czech",
  sk: "Slovak",
  hu: "Hungarian",
  ro: "Romanian",
  bg: "Bulgarian",
  hr: "Croatian",
  sl: "Slovenian",
  el: "Greek",
  uk: "Ukrainian",
  et: "Estonian",
  lv: "Latvian",
  lt: "Lithuanian",
  ga: "Irish",
  mt: "Maltese",
  ar: "Arabic",
  hi: "Hindi",
  vi: "Vietnamese",
  th: "Thai",
  he: "Hebrew",
  tr: "Turkish",
  id: "Indonesian",
  ca: "Catalan",
};

function languageLabel(code: string): string {
  return LANGUAGE_LABELS[code] ?? code;
}

export function TranscriptionTab() {
  const language = useAppStore((s) => s.settings.language);
  // diarizationEnabled is intentionally not read here — the Speaker
  // diarization toggle is hard-locked to off pending session-stable
  // speaker IDs (see setDiarizationEnabled). The persisted value is still
  // forced to false on upgrade via migration v23, and the store action
  // throws if anything tries to enable it.
  const engineCatalogue = useAppStore((s) => s.engineCatalogue);
  const enginePhase = useAppStore((s) => s.enginePhase);
  const engineError = useAppStore((s) => s.engineError);
  const autoSetup = useAppStore((s) => s.autoSetup);
  const legacyModelFiles = useAppStore((s) => s.legacyModelFiles);
  const refreshLegacyModelFiles = useAppStore(
    (s) => s.refreshLegacyModelFiles,
  );
  const deleteLegacyModelFiles = useAppStore((s) => s.deleteLegacyModelFiles);
  const setDiarizationEnabled = useAppStore((s) => s.setDiarizationEnabled);
  const [showLanguages, setShowLanguages] = useState(false);
  const [removingLegacy, setRemovingLegacy] = useState(false);
  const [retrying, setRetrying] = useState(false);

  useEffect(() => {
    refreshLegacyModelFiles().catch(() => {});
  }, [refreshLegacyModelFiles]);

  // Parakeet is the only engine (ADR 0005), so the catalogue has one entry.
  const descriptor = engineCatalogue[0];
  const languages = useMemo(
    () =>
      (descriptor?.languages ?? [])
        .map(languageLabel)
        .sort((a, b) => a.localeCompare(b)),
    [descriptor],
  );
  // The persisted language is kept (it becomes the primary-language input
  // for English-only model routing later); it only drives this notice today.
  const languageUnsupported =
    descriptor !== undefined &&
    !descriptor.languages.some(
      (code) => code.toLowerCase() === language.toLowerCase(),
    );

  const legacyBytes = legacyModelFiles.reduce((sum, f) => sum + f.bytes, 0);

  const handleRemoveLegacy = async () => {
    setRemovingLegacy(true);
    try {
      await deleteLegacyModelFiles();
      toast.success(`Removed old Whisper models (${formatBytes(legacyBytes)} freed)`);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setRemovingLegacy(false);
    }
  };

  return (
    <>
      {/* Engine */}
      <div className="space-y-2">
        <Label className="text-xs text-muted-foreground">
          Transcription engine
        </Label>
        <p className="text-sm font-medium text-foreground">Parakeet TDT v3</p>
        <p className="text-[10px] text-muted-foreground/60">
          NVIDIA Parakeet, running on this device.
        </p>
      </div>

      <Separator />

      {/* Languages — read-only. Parakeet detects the spoken language itself. */}
      <Collapsible open={showLanguages} onOpenChange={setShowLanguages}>
        <div className="space-y-2">
          <Label className="text-xs text-muted-foreground">Languages</Label>
          <CollapsibleTrigger className="flex w-full items-center justify-between rounded-md px-2 py-1.5 text-left hover:bg-muted/50">
            <span className="text-sm text-foreground">
              Auto-detected: {languages.length || 25} European languages
            </span>
            <ChevronsUpDown className="h-3.5 w-3.5 text-muted-foreground/60" />
          </CollapsibleTrigger>
          <CollapsibleContent>
            <p className="px-2 text-[11px] leading-relaxed text-muted-foreground">
              {languages.join(", ")}
            </p>
          </CollapsibleContent>
          {languageUnsupported && (
            <div
              role="status"
              className="flex gap-2 rounded-md border border-border bg-muted/40 px-2.5 py-2"
            >
              <Info className="mt-0.5 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
              <p className="text-[11px] text-muted-foreground">
                {languageLabel(language)} isn't supported yet. YapStack
                transcribes the languages listed above and detects which one
                is being spoken.
              </p>
            </div>
          )}
        </div>
      </Collapsible>

      <Separator />

      {/* Model */}
      <div className="space-y-2">
        <Label className="text-xs text-muted-foreground">Model</Label>
        <ParakeetModelSection />
        {enginePhase === "error" && (
          <div className="flex items-center justify-between gap-2 rounded-md border border-destructive/40 px-2.5 py-2">
            <p className="text-[11px] text-destructive">
              {engineError ?? "The transcription engine failed to start."}
            </p>
            <Button
              size="sm"
              variant="outline"
              className="h-7 shrink-0 text-xs"
              disabled={retrying}
              onClick={() => {
                setRetrying(true);
                autoSetup()
                  .catch(() => {})
                  .finally(() => setRetrying(false));
              }}
            >
              <RotateCw className="mr-1 h-3 w-3" />
              Retry
            </Button>
          </div>
        )}
        {legacyModelFiles.length > 0 && (
          <div className="flex items-center justify-between rounded-md px-2 py-1.5 hover:bg-muted/50">
            <div className="space-y-0.5">
              <p className="text-sm text-muted-foreground">
                Remove old Whisper models ({formatBytes(legacyBytes)})
              </p>
              <p className="text-[10px] text-muted-foreground/60">
                Whisper is no longer used. Removing its files frees disk space.
              </p>
            </div>
            <Button
              variant="ghost"
              size="icon-xs"
              aria-label="Remove old Whisper models"
              disabled={removingLegacy}
              onClick={handleRemoveLegacy}
            >
              <Trash2 className="h-3.5 w-3.5" />
            </Button>
          </div>
        )}
      </div>

      <Separator />

      {/* Diarization toggle — currently locked off because Sortformer's
          per-chunk reset produces unstable speaker IDs across the session.
          Plumbing stays so re-enable is a one-line change once session-
          stable IDs land. The toggle is shown (not removed) so users can
          see the feature exists and that it's coming, with a tooltip
          explaining why it's disabled. */}
      <div className="space-y-2">
        <div className="flex items-center justify-between">
          <Label className="text-xs text-muted-foreground">
            Speaker diarization
          </Label>
          <TooltipProvider delayDuration={300}>
            <Tooltip>
              <TooltipTrigger asChild>
                <span>
                  <Switch
                    checked={false}
                    disabled
                    onCheckedChange={async (checked) => {
                      try {
                        await setDiarizationEnabled(checked);
                      } catch (e) {
                        toast.error(
                          e instanceof Error ? e.message : String(e),
                        );
                      }
                    }}
                  />
                </span>
              </TooltipTrigger>
              <TooltipContent side="left">
                Coming soon — currently disabled while we ship session-stable
                speaker IDs
              </TooltipContent>
            </Tooltip>
          </TooltipProvider>
        </div>
        <p className="text-[10px] text-muted-foreground/60">
          Will identify distinct speakers in mixed-source recordings.
          Temporarily disabled — chunk-local speaker IDs cause unstable
          labels across long sessions.
        </p>
      </div>
    </>
  );
}
