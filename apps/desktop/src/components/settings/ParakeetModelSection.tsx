import { useEffect, useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Progress } from "@/components/ui/progress";
import { Download, Trash2 } from "lucide-react";
import { toast } from "sonner";
import type { ParakeetVariantDto } from "@/lib/tauri";
import { formatBytes } from "@/lib/utils";

/// Where each variant runs, told apart when more than one row shows.
const RUNS_ON: Record<ParakeetVariantDto, string> = {
  TdtV3Int8: "Runs on the CPU or GPU",
  TdtV3CoreMl: "Runs on the Neural Engine",
};

/// Parakeet model rows: the model this host should run, plus any other
/// model on disk. The backend lists the CoreML model only where it's the
/// recommended one (Apple silicon), so elsewhere this is the int8 row alone.
export function ParakeetModelSection() {
  const selectedVariant = useAppStore(
    (s) => s.settings.selectedParakeetVariant,
  );
  const parakeetModels = useAppStore((s) => s.parakeetModels);
  const modelDownloadProgress = useAppStore((s) => s.modelDownloadProgress);
  const enginePhase = useAppStore((s) => s.enginePhase);
  const downloadParakeet = useAppStore((s) => s.downloadParakeetModel);
  const deleteParakeet = useAppStore((s) => s.deleteParakeetModel);

  const isDownloading = modelDownloadProgress !== null;
  const [downloadingVariant, setDownloadingVariant] =
    useState<ParakeetVariantDto | null>(null);

  useEffect(() => {
    if (!isDownloading && downloadingVariant !== null)
      setDownloadingVariant(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isDownloading]);

  const handleDownload = async (variant: ParakeetVariantDto) => {
    setDownloadingVariant(variant);
    try {
      await downloadParakeet(variant);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  const handleDelete = async (variant: ParakeetVariantDto) => {
    try {
      await deleteParakeet(variant);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  const recommended: ParakeetVariantDto = parakeetModels.some(
    (m) => m.variant === "TdtV3CoreMl",
  )
    ? "TdtV3CoreMl"
    : selectedVariant;
  const models = parakeetModels.filter(
    (m) => m.downloaded || m.variant === recommended,
  );
  // autoSetup only ever downloads the recommended model.
  const progressVariant = downloadingVariant ?? recommended;

  return (
    <div className="space-y-1">
      {models.map((model) => {
        const isActive =
          model.variant === selectedVariant &&
          model.downloaded &&
          enginePhase === "ready";
        const showProgress =
          isDownloading && model.variant === progressVariant;
        // The selected model is in use or being set up, unless setup failed.
        const canDelete =
          model.variant !== selectedVariant || enginePhase === "error";
        return (
          <div
            key={model.variant}
            className="flex items-center justify-between rounded-md px-2 py-1.5 hover:bg-muted/50"
          >
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <span
                  className={
                    isActive
                      ? "text-sm font-medium text-foreground"
                      : "text-sm text-muted-foreground"
                  }
                >
                  {model.display_name}
                </span>
                <Badge variant="secondary" className="text-xs">
                  {formatBytes(model.approximate_size_bytes)}
                </Badge>
              </div>
              {models.length > 1 && (
                <p className="text-[10px] text-muted-foreground/60">
                  {RUNS_ON[model.variant]}
                </p>
              )}
            </div>

            <div className="flex items-center gap-1">
              {showProgress ? (
                <div className="w-24">
                  <Progress
                    value={modelDownloadProgress ?? 0}
                    className="h-2"
                  />
                </div>
              ) : isActive ? (
                <Badge
                  variant="outline"
                  className="border-green-600 text-xs text-green-600"
                >
                  Active
                </Badge>
              ) : model.downloaded ? (
                canDelete && (
                  <Button
                    variant="ghost"
                    size="icon-xs"
                    aria-label="Delete model"
                    disabled={isDownloading}
                    onClick={() => handleDelete(model.variant)}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </Button>
                )
              ) : (
                <Button
                  variant="ghost"
                  size="icon-xs"
                  aria-label="Download model"
                  disabled={isDownloading}
                  onClick={() => handleDownload(model.variant)}
                >
                  <Download className="h-3.5 w-3.5" />
                </Button>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}
