import { useState } from "react";
import { useAppStore } from "@/stores/appStore";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ArrowLeft } from "lucide-react";
import { GeneralTab } from "@/components/settings/GeneralTab";
import { AudioTab } from "@/components/settings/AudioTab";
import { TranscriptionTab } from "@/components/settings/TranscriptionTab";
import { ShortcutsTab } from "@/components/settings/ShortcutsTab";
import { AITab } from "@/components/settings/AITab";
import { DictationTab } from "@/components/settings/DictationTab";
import { InsightsTab } from "@/components/settings/InsightsTab";
import { SyncTab } from "@/components/sync/SyncTab";

export function SettingsPanel() {
  const navigateTo = useAppStore((s) => s.navigateTo);
  // Seed the initial tab from a one-shot settingsRequest. AITab is responsible
  // for clearing the request once it's consumed (so the request doesn't fire
  // again on the next mount).
  const initialTab = useAppStore.getState().settingsRequest === "ai-add-connection"
    ? "ai"
    : "general";
  const [tab, setTab] = useState(initialTab);

  return (
    <div className="flex flex-1 flex-col min-h-0">
      {/* Header */}
      <div className="flex items-center gap-2 border-b px-4 py-2">
        <button
          className="rounded p-1 text-muted-foreground hover:bg-muted"
          onClick={() => navigateTo("note-list")}
          aria-label="Back to notes"
        >
          <ArrowLeft className="h-4 w-4" />
        </button>
        <span className="text-sm font-bold">Settings</span>
      </div>

      {/* Tabs */}
      <Tabs
        value={tab}
        onValueChange={setTab}
        className="flex flex-1 flex-col min-h-0"
      >
        <TabsList variant="line" className="px-4 pt-1 w-full">
          <TabsTrigger value="general">General</TabsTrigger>
          <TabsTrigger value="audio">Audio</TabsTrigger>
          <TabsTrigger value="transcription">Transcription</TabsTrigger>
          <TabsTrigger value="ai">AI</TabsTrigger>
          <TabsTrigger value="dictation">Dictation</TabsTrigger>
          <TabsTrigger value="insights">Insights</TabsTrigger>
          <TabsTrigger value="sync">Sync</TabsTrigger>
          <TabsTrigger value="shortcuts">Shortcuts</TabsTrigger>
        </TabsList>

        <TabsContent value="general" className="flex-1 min-h-0">
          <ScrollArea className="h-full">
            <div className="space-y-5 p-4">
              <GeneralTab />
            </div>
          </ScrollArea>
        </TabsContent>

        <TabsContent value="audio" className="flex-1 min-h-0">
          <ScrollArea className="h-full">
            <div className="space-y-5 p-4">
              <AudioTab />
            </div>
          </ScrollArea>
        </TabsContent>

        <TabsContent value="transcription" className="flex-1 min-h-0">
          <ScrollArea className="h-full">
            <div className="space-y-5 p-4">
              <TranscriptionTab />
            </div>
          </ScrollArea>
        </TabsContent>

        <TabsContent value="ai" className="flex-1 min-h-0">
          <ScrollArea className="h-full">
            <div className="space-y-5 p-4">
              <AITab />
            </div>
          </ScrollArea>
        </TabsContent>

        <TabsContent value="dictation" className="flex-1 min-h-0">
          <ScrollArea className="h-full">
            <div className="space-y-5 p-4">
              <DictationTab />
            </div>
          </ScrollArea>
        </TabsContent>

        <TabsContent value="insights" className="flex-1 min-h-0">
          <ScrollArea className="h-full">
            <div className="space-y-5 p-4">
              <InsightsTab />
            </div>
          </ScrollArea>
        </TabsContent>

        <TabsContent value="sync" className="flex-1 min-h-0">
          <ScrollArea className="h-full">
            <div className="space-y-5 p-4">
              <SyncTab />
            </div>
          </ScrollArea>
        </TabsContent>

        <TabsContent value="shortcuts" className="flex-1 min-h-0">
          <ScrollArea className="h-full">
            <ShortcutsTab />
          </ScrollArea>
        </TabsContent>
      </Tabs>
    </div>
  );
}
