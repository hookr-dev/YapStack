import { useState, useEffect } from "react";
import { useAppStore } from "@/stores/appStore";
import type { ThemeMode } from "@/stores/appStore";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Switch } from "@/components/ui/switch";
import { Progress } from "@/components/ui/progress";
import { PlayCircle, RefreshCw, Download, Loader2, Trash2 } from "lucide-react";
import { getVersion } from "@tauri-apps/api/app";
import { toast } from "sonner";
import { checkForUpdate, downloadAndInstallUpdate } from "@/lib/updater";
import type { UpdateStatus } from "@/lib/updater";
import { trackUpdateInstallStarted, trackUpdateInstallFailed } from "@/lib/analytics";

export function GeneralTab() {
  const theme = useAppStore((s) => s.settings.theme);
  const showRecordingIndicator = useAppStore((s) => s.settings.showRecordingIndicator);
  const analyticsEnabled = useAppStore((s) => s.settings.analyticsEnabled);
  const updateSettings = useAppStore((s) => s.updateSettings);
  const clearAllSessions = useAppStore((s) => s.clearAllSessions);
  const isRecording = useAppStore((s) => s.liveTranscriptionActive);

  const [appVersion, setAppVersion] = useState("");
  const storeUpdate = useAppStore((s) => s.updateAvailable);
  const [checking, setChecking] = useState(false);
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus | null>(
    storeUpdate ? { available: true, version: storeUpdate.version, body: storeUpdate.body } : null,
  );
  const [installing, setInstalling] = useState(false);
  const [installProgress, setInstallProgress] = useState(0);
  const [showRecordingWarning, setShowRecordingWarning] = useState(false);

  useEffect(() => {
    getVersion().then(setAppVersion).catch(() => {});
  }, []);

  useEffect(() => {
    if (storeUpdate && !updateStatus?.available) {
      setUpdateStatus({ available: true, version: storeUpdate.version, body: storeUpdate.body });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- intentionally omit updateStatus to avoid feedback loop
  }, [storeUpdate]);

  const handleCheckForUpdate = async () => {
    setChecking(true);
    setUpdateStatus(null);
    try {
      const status = await checkForUpdate();
      setUpdateStatus(status);
      if (status.available) {
        useAppStore.getState().setUpdateAvailable({
          version: status.version,
          body: status.body,
        });
      } else {
        toast.success("You're on the latest version");
      }
    } catch {
      toast.error("Failed to check for updates");
    } finally {
      setChecking(false);
    }
  };

  const handleInstallUpdate = async () => {
    if (isRecording) {
      setShowRecordingWarning(true);
      return;
    }
    await doInstall();
  };

  const doInstall = async () => {
    if (!updateStatus?.available) return;
    setInstalling(true);
    setInstallProgress(0);
    trackUpdateInstallStarted({ version: updateStatus.version });
    try {
      await downloadAndInstallUpdate((progress) => {
        if (!progress.contentLength || progress.contentLength <= 0) return;
        const pct = Math.min(
          100,
          Math.round((progress.downloaded / progress.contentLength) * 100),
        );
        // Monotonic: a stale or repeated "Started" event must not pull the
        // bar backward. Same-value setState bails out, so this also avoids
        // re-renders for the many sub-1% chunk events.
        setInstallProgress((prev) => (pct > prev ? pct : prev));
      });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      trackUpdateInstallFailed({ version: updateStatus.version, error: msg });
      toast.error("Update failed: " + msg.slice(0, 100));
      setInstalling(false);
    }
  };

  const [autostart, setAutostart] = useState(false);
  useEffect(() => {
    invoke<boolean>("get_autostart_enabled").then(setAutostart).catch(() => {});
  }, []);

  const handleToggleAutostart = async (enabled: boolean) => {
    try {
      await invoke("set_autostart_enabled", { enabled });
      setAutostart(enabled);
    } catch {
      // revert on failure
    }
  };

  return (
    <>
      {/* Appearance */}
      <div className="space-y-2">
        <Label className="text-xs text-muted-foreground">Theme</Label>
        <div className="flex gap-1.5">
          {(["light", "dark", "system"] as ThemeMode[]).map((t) => (
            <Button
              key={t}
              size="sm"
              variant={theme === t ? "default" : "outline"}
              className="flex-1 text-xs capitalize"
              onClick={() => updateSettings({ theme: t })}
            >
              {t}
            </Button>
          ))}
        </div>
      </div>

      <Separator />

      {/* Recording Indicator */}
      <div className="flex items-center justify-between">
        <div className="space-y-0.5">
          <Label className="text-xs">Recording Indicator</Label>
          <p className="text-[10px] text-muted-foreground">
            Show floating indicator when recording and app is not focused
          </p>
        </div>
        <Switch
          size="sm"
          checked={showRecordingIndicator}
          onCheckedChange={(checked) =>
            updateSettings({ showRecordingIndicator: checked })
          }
        />
      </div>

      <Separator />

      {/* Launch at Login */}
      <div className="flex items-center justify-between">
        <div className="space-y-0.5">
          <Label className="text-xs">Launch at Login</Label>
          <p className="text-[10px] text-muted-foreground">
            Automatically start YapStack when you log in
          </p>
        </div>
        <Switch
          size="sm"
          checked={autostart}
          onCheckedChange={handleToggleAutostart}
        />
      </div>

      <Separator />

      {/* Updates */}
      <div className="space-y-2">
        <div className="flex items-center justify-between">
          <div className="space-y-0.5">
            <Label className="text-xs">Updates</Label>
            <p className="text-[10px] text-muted-foreground">
              {appVersion ? `Current version: ${appVersion}` : "Checking version..."}
            </p>
          </div>
          {!updateStatus?.available && (
            <Button
              size="sm"
              variant="outline"
              className="text-xs gap-1.5"
              disabled={checking}
              onClick={handleCheckForUpdate}
            >
              {checking ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <RefreshCw className="h-3.5 w-3.5" />
              )}
              Check for Updates
            </Button>
          )}
        </div>
        {updateStatus?.available && (
          <div className="space-y-2">
            <p className="text-xs text-muted-foreground">
              Version {updateStatus.version} is available
            </p>
            {installing ? (
              <div className="space-y-1.5">
                <div className="flex items-center gap-2">
                  <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />
                  <span className="text-xs text-muted-foreground">Installing update...</span>
                </div>
                <Progress value={installProgress} className="h-1.5" />
              </div>
            ) : (
              <Button
                size="sm"
                variant="default"
                className="text-xs gap-1.5"
                onClick={handleInstallUpdate}
              >
                <Download className="h-3.5 w-3.5" />
                Install Update
              </Button>
            )}
          </div>
        )}
      </div>

      <AlertDialog open={showRecordingWarning} onOpenChange={setShowRecordingWarning}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Recording in progress</AlertDialogTitle>
            <AlertDialogDescription>
              A recording session is currently active. Installing the update will
              restart the app and stop the recording. Do you want to continue?
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => {
                setShowRecordingWarning(false);
                doInstall();
              }}
            >
              Install Anyway
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <Separator />

      {/* Onboarding */}
      <div className="flex items-center justify-between">
        <div className="space-y-0.5">
          <Label className="text-xs">Onboarding</Label>
          <p className="text-[10px] text-muted-foreground">
            Walk through the setup guide again
          </p>
        </div>
        <Button
          size="sm"
          variant="outline"
          className="text-xs gap-1.5"
          onClick={() => {
            const { initial: _, ...rest } = useAppStore.getState().settings.onboarding?.completedFlows ?? {};
            updateSettings({ onboarding: { completedFlows: rest } });
            useAppStore.getState().navigateTo("note-list");
          }}
        >
          <PlayCircle className="h-3.5 w-3.5" />
          Replay
        </Button>
      </div>

      <Separator />

      {/* Analytics */}
      <div className="flex items-center justify-between gap-4">
        <div className="space-y-0.5">
          <Label className="text-xs">Share anonymous usage analytics</Label>
          <p className="text-[10px] text-muted-foreground">
            Anonymous product events only — never transcripts, audio, note
            content, or file names. Turning this off stops new events
            immediately; events already queued this session may still be sent
            within about a minute. This choice stays on this device and is never
            synced.
          </p>
        </div>
        <Switch
          size="sm"
          checked={analyticsEnabled}
          onCheckedChange={(checked) => updateSettings({ analyticsEnabled: checked })}
        />
      </div>

      <Separator />

      {/* Data */}
      <div className="flex items-center justify-between">
        <div className="space-y-0.5">
          <Label className="text-xs">Clear All Sessions</Label>
          <p className="text-[10px] text-muted-foreground">
            Permanently delete all sessions and transcripts
          </p>
        </div>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button
              variant="outline"
              size="sm"
              className="gap-1.5 border-destructive/40 text-xs text-destructive hover:bg-destructive hover:text-white"
            >
              <Trash2 className="h-3.5 w-3.5" />
              Clear
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Clear all sessions?</AlertDialogTitle>
              <AlertDialogDescription>
                This will permanently delete all recorded sessions and their
                transcriptions. This action cannot be undone.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction
                className="bg-destructive text-white hover:bg-destructive/90"
                onClick={() => {
                  clearAllSessions();
                  toast.success("All sessions cleared");
                }}
              >
                Delete All
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </div>

      <div className="flex-1" />
      <p className="text-center text-[10px] text-muted-foreground/40 pt-6">
        ✨ by{" "}
        <a
          href="https://hookr.dev"
          target="_blank"
          rel="noopener noreferrer"
          className="hover:text-muted-foreground transition-colors"
        >
          hookr.dev
        </a>
      </p>
    </>
  );
}
