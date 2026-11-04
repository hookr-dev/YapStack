import { useEffect, useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Progress } from "@/components/ui/progress";
import { Download, Trash2 } from "lucide-react";
import { toast } from "sonner";
import type { ParakeetVariantDto } from "@/lib/tauri";
import { formatBytes } from "@/lib/utils";

/// Parakeet model row. The variant is resolved by the backend
/// (`get_recommended_parakeet_variant`) and coerced into the store at
/// `autoSetup` time, so this UI only ever shows one row.
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
    if (
      variant === selectedVariant &&
      enginePhase === "ready"
    ) {
      toast.error("Cannot delete the active model");
      return;
    }
    try {
      await deleteParakeet(variant);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  // Render only the host-recommended variant. autoSetup coerces
  // `selectedParakeetVariant` to whatever `get_recommended_parakeet_variant`
  // returns at launch, so this filter is effectively "the row the user
  // can actually use." If `parakeetModels` hasn't loaded yet (rare race
  // during cold start) we show nothing — the surrounding panel handles
  // the loading state.
  const model = parakeetModels.find((m) => m.variant === selectedVariant);
  if (!model) return <div className="space-y-1" />;

  const isActive =
    model.downloaded &&
    enginePhase === "ready";
  const showProgress =
    isDownloading &&
    (downloadingVariant === model.variant ||
      (downloadingVariant === null && model.variant === selectedVariant));

  return (
    <div className="space-y-1">
      <div className="flex items-center justify-between rounded-md px-2 py-1.5 hover:bg-muted/50">
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

        <div className="flex items-center gap-1">
          {showProgress ? (
            <div className="w-24">
              <Progress value={modelDownloadProgress ?? 0} className="h-2" />
            </div>
          ) : isActive ? (
            <Badge
              variant="outline"
              className="border-green-600 text-xs text-green-600"
            >
              Active
            </Badge>
          ) : model.downloaded ? (
            <Button
              variant="ghost"
              size="icon-xs"
              disabled={isDownloading}
              onClick={() => handleDelete(model.variant)}
            >
              <Trash2 className="h-3.5 w-3.5" />
            </Button>
          ) : (
            <Button
              variant="ghost"
              size="icon-xs"
              disabled={isDownloading}
              onClick={() => handleDownload(model.variant)}
            >
              <Download className="h-3.5 w-3.5" />
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}
