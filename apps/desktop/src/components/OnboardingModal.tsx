import { useState, useEffect, useRef, useCallback } from "react";
import { useAppStore } from "@/stores/appStore";
import type { ThemeMode } from "@/stores/appStore";
import { getActiveFlow, type StepNav } from "./onboarding-utils";
import { testConnection, fetchCustomModels } from "@/lib/ai";
import type { AIProviderKind, Connection, Profile } from "@/lib/ai";
import {
  CustomBaseUrlField,
  CustomModelField,
} from "@/components/ai/CustomProviderFields";
import {
  eventToGlobalBinding,
  shortcutCaptureActive,
  getBinding,
} from "@/lib/shortcuts";
import {
  suspendGlobalShortcuts,
  resumeGlobalShortcuts,
} from "@/hooks/useGlobalShortcuts";
import { formatGlobalShortcutDisplay, formatShortcutDisplay } from "@/lib/utils";
import {
  Dialog,
  DialogOverlay,
  DialogPortal,
} from "@/components/ui/dialog";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Mic,
  Monitor,
  AudioLines,
  Sparkles,
  Eye,
  EyeOff,
  Loader2,
  Check,
  ChevronRight,
  ChevronLeft,
  Sun,
  Moon,
  Laptop,
  Keyboard,
  X,
  ExternalLink,
} from "lucide-react";
import { commands, type CaptureSourceDto } from "@/lib/tauri";
import { YapStackIcon } from "@/components/YapStackIcon";
import { ProgressDots } from "@/components/ProgressDots";
import { DevicesIllustration } from "@/components/sync/DevicesIllustration";

// --- Step 1: Welcome ---

function WelcomeStep({
  onNext,
}: {
  onNext: () => void;
}) {
  const theme = useAppStore((s) => s.settings.theme);
  const updateSettings = useAppStore((s) => s.updateSettings);

  const themeOptions: { value: ThemeMode; label: string; icon: typeof Sun }[] = [
    { value: "light", label: "Light", icon: Sun },
    { value: "dark", label: "Dark", icon: Moon },
    { value: "system", label: "System", icon: Laptop },
  ];

  return (
    <div className="flex flex-col items-center text-center">
      <YapStackIcon className="w-16 h-16 mb-5 text-foreground" />

      <h2 className="text-xl font-semibold mb-0.5">Welcome to YapStack</h2>
      <p className="text-sm text-muted-foreground mb-8">
        Capture, transcribe, and organize your thoughts
      </p>

      {/* Theme picker */}
      <div className="w-full space-y-2 mb-8">
        <Label className="text-xs text-muted-foreground">Appearance</Label>
        <div className="flex gap-2">
          {themeOptions.map((t) => (
            <Button
              key={t.value}
              size="sm"
              variant={theme === t.value ? "default" : "outline"}
              className="flex-1 text-xs gap-1.5"
              onClick={() => updateSettings({ theme: t.value })}
            >
              <t.icon className="h-3.5 w-3.5" />
              {t.label}
            </Button>
          ))}
        </div>
      </div>

      <Button className="w-full" onClick={onNext}>
        Get Started
        <ChevronRight className="ml-1.5 h-4 w-4" />
      </Button>
    </div>
  );
}

// --- Step 2: Audio ---

const CAPTURE_SOURCES: {
  value: CaptureSourceDto;
  label: string;
  description: string;
  icon: typeof Mic;
  recommended?: boolean;
}[] = [
  {
    value: "MicOnly",
    label: "Microphone",
    description: "Record your voice",
    icon: Mic,
  },
  {
    value: "SystemOnly",
    label: "System Audio",
    description: "Capture desktop audio",
    icon: Monitor,
  },
  {
    value: "Mixed",
    label: "Mixed",
    description: "Both mic and system audio",
    icon: AudioLines,
    recommended: true,
  },
];

function useMicLevel(captureSource: CaptureSourceDto) {
  const [active, setActive] = useState(false);
  const [level, setLevel] = useState(0);
  const [detected, setDetected] = useState(false);

  useEffect(() => {
    if (!active) {
      setLevel(0);
      setDetected(false);
      return;
    }
    let cancelled = false;
    async function poll() {
      if (cancelled) return;
      try {
        const result = await commands.peekCaptureEnergy(0.1);
        if (cancelled) return;
        if (result.status === "ok") {
          const raw =
            captureSource === "SystemOnly"
              ? (result.data.system_rms ?? 0)
              : (result.data.mic_rms ?? 0);
          const rms = raw < 0.002 ? 0 : raw;
          const pct = Math.min(Math.sqrt(rms / 0.15) * 100, 100);
          setLevel(pct);
          if (rms > 0.01) setDetected(true);
        }
      } catch {
        // IPC failure — skip this tick, retry next poll
      }
      if (!cancelled) setTimeout(poll, 150);
    }
    poll();
    return () => {
      cancelled = true;
    };
  }, [active, captureSource]);

  return { active, setActive, level, detected };
}

function AudioStep({
  onNext,
  onBack,
}: {
  onNext: () => void;
  onBack: () => void;
}) {
  const captureSource = useAppStore((s) => s.settings.captureSource);
  const selectedMicDeviceId = useAppStore((s) => s.settings.selectedMicDeviceId);
  const devices = useAppStore((s) => s.devices);
  const updateSettings = useAppStore((s) => s.updateSettings);

  const inputDevices = devices.filter((d) => d.device_type === "Input");
  const mic = useMicLevel(captureSource);

  return (
    <div className="flex flex-col">
      <h2 className="text-lg font-semibold mb-0.5">Audio Setup</h2>
      <p className="text-sm text-muted-foreground mb-6">
        Choose how YapStack listens
      </p>

      {/* Capture Source Cards */}
      <div className="space-y-2 mb-5">
        <Label className="text-xs text-muted-foreground">Capture Source</Label>
        <div className="grid grid-cols-3 gap-2">
          {CAPTURE_SOURCES.map((src) => {
            const active = captureSource === src.value;
            return (
              <button
                key={src.value}
                type="button"
                onClick={() => updateSettings({ captureSource: src.value })}
                className={`relative flex flex-col items-center gap-1.5 rounded-lg border p-3 text-center transition-all ${
                  active
                    ? "border-primary bg-primary/5 shadow-sm shadow-primary/10"
                    : "border-border hover:border-muted-foreground/30"
                }`}
              >
                {src.recommended && (
                  <Badge
                    variant="secondary"
                    className="absolute -top-2 text-[9px] px-1.5 py-0"
                  >
                    Recommended
                  </Badge>
                )}
                <src.icon
                  className={`h-5 w-5 ${active ? "text-primary" : "text-muted-foreground"}`}
                />
                <span className="text-xs font-medium">{src.label}</span>
                <span className="text-[10px] text-muted-foreground leading-tight">
                  {src.description}
                </span>
              </button>
            );
          })}
        </div>
      </div>

      {/* Input Device + Test Mic */}
      <div className="space-y-2 mb-6">
        {captureSource !== "SystemOnly" && (
          <>
            <Label className="text-xs text-muted-foreground">Input Device</Label>
            <div className="flex items-center gap-2">
              <Select
                value={selectedMicDeviceId ?? "__default"}
                onValueChange={(v) =>
                  updateSettings({
                    selectedMicDeviceId: v === "__default" ? null : v,
                  })
                }
              >
                <SelectTrigger size="sm" className="text-xs flex-1">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="__default" className="text-xs">
                    Default
                  </SelectItem>
                  {inputDevices.map((d) => (
                    <SelectItem key={d.id ?? d.name} value={d.id ?? d.name} className="text-xs">
                      {d.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <Button
                size="sm"
                variant={mic.active ? "default" : "outline"}
                className="text-xs gap-1.5 shrink-0"
                onClick={() => mic.setActive(!mic.active)}
              >
                <Mic className="h-3.5 w-3.5" />
                {mic.active ? "Stop" : "Test Mic"}
              </Button>
            </div>
          </>
        )}
        {captureSource === "SystemOnly" && (
          <div className="flex items-center justify-end">
            <Button
              size="sm"
              variant={mic.active ? "default" : "outline"}
              className="text-xs gap-1.5"
              onClick={() => mic.setActive(!mic.active)}
            >
              <Monitor className="h-3.5 w-3.5" />
              {mic.active ? "Stop" : "Test Audio"}
            </Button>
          </div>
        )}
        {mic.active && (
          <div className="space-y-1">
            <div className="h-2 rounded-full bg-muted overflow-hidden">
              <div
                className="h-full rounded-full bg-primary transition-all duration-100"
                style={{ width: `${mic.level}%` }}
              />
            </div>
            <p className="text-[10px] text-muted-foreground text-right">
              {mic.detected ? "Sound detected!" : "Listening..."}
            </p>
          </div>
        )}
      </div>

      {/* Navigation */}
      <div className="flex gap-2 mt-auto pt-4">
        <Button variant="outline" className="flex-1" onClick={onBack}>
          <ChevronLeft className="mr-1.5 h-4 w-4" />
          Back
        </Button>
        <Button className="flex-1" onClick={onNext}>
          Next
          <ChevronRight className="ml-1.5 h-4 w-4" />
        </Button>
      </div>
    </div>
  );
}

// --- Step 3: AI Assistant ---

const PROVIDER_LABELS: Record<AIProviderKind, string> = {
  openai: "OpenAI",
  openrouter: "OpenRouter",
  custom: "Custom",
};

// Default baseUrls per kind. Mirrors the values used elsewhere
// (ConnectionEditorDialog) — kept inline to avoid coupling onboarding to a
// shared catalog that exists only for this purpose.
const DEFAULT_BASE_URLS: Record<AIProviderKind, string> = {
  openai: "https://api.openai.com/v1",
  openrouter: "https://openrouter.ai/api/v1",
  custom: "http://127.0.0.1:8080/v1",
};

// Small curated list of starter models per known kind. The catalog in
// the editor is sourced from /v1/models at runtime; onboarding favors a
// short, opinionated list so the user can move on without inspecting
// hundreds of model ids.
const STARTER_MODELS: Record<
  AIProviderKind,
  { id: string; label: string; recommended?: boolean }[]
> = {
  openai: [
    { id: "gpt-5.4-mini", label: "GPT-5.4 Mini", recommended: true },
    { id: "gpt-5.4", label: "GPT-5.4" },
    { id: "gpt-4o-mini", label: "GPT-4o Mini" },
  ],
  openrouter: [
    { id: "anthropic/claude-haiku-4.5", label: "Claude Haiku 4.5", recommended: true },
    { id: "anthropic/claude-sonnet-4.5", label: "Claude Sonnet 4.5" },
    { id: "openai/gpt-5.4-mini", label: "GPT-5.4 Mini" },
  ],
  custom: [],
};

function AIStep({
  onNext,
  onBack,
}: {
  onNext: () => void;
  onBack: () => void;
}) {
  const aiConfig = useAppStore((s) => s.settings.aiConfig);
  const updateSettings = useAppStore((s) => s.updateSettings);
  const [kind, setKind] = useState<AIProviderKind>("openai");
  const [apiKey, setApiKey] = useState("");
  const [baseUrl, setBaseUrl] = useState(DEFAULT_BASE_URLS.openai);
  const [model, setModel] = useState(STARTER_MODELS.openai[0]!.id);
  const [fetchedModels, setFetchedModels] = useState<string[] | undefined>(
    undefined,
  );
  const [showKey, setShowKey] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{
    ok: boolean;
    error?: string;
  } | null>(null);

  function handleKindChange(next: AIProviderKind) {
    setKind(next);
    setBaseUrl(DEFAULT_BASE_URLS[next]);
    setModel(STARTER_MODELS[next][0]?.id ?? "");
    setFetchedModels(undefined);
    setTestResult(null);
  }

  async function handleTestConnection() {
    setTesting(true);
    setTestResult(null);
    const probeConnection: Connection = {
      id: "__probe__",
      name: PROVIDER_LABELS[kind],
      kind,
      baseUrl,
      apiKey,
    };
    const result = await testConnection(probeConnection, model);
    setTestResult(result);
    setTesting(false);
  }

  async function handleNext() {
    if (!model.trim()) {
      onNext();
      return;
    }
    // Try to fetch the model catalog so the Profile picker in Settings has
    // something to show later. Non-blocking — failures are normal for
    // unreachable local servers and shouldn't stop onboarding.
    let available: string[] | undefined = fetchedModels;
    if (available === undefined) {
      try {
        available = await fetchCustomModels(baseUrl, apiKey);
      } catch {
        available = undefined;
      }
    }
    const connection: Connection = {
      id: crypto.randomUUID(),
      name: PROVIDER_LABELS[kind],
      kind,
      baseUrl,
      apiKey: apiKey.trim(),
      ...(available !== undefined && { availableModels: available }),
      ...(available !== undefined && { fetchedAt: new Date().toISOString() }),
    };
    const profile: Profile = {
      id: crypto.randomUUID(),
      name: `${PROVIDER_LABELS[kind]} · ${model.trim()}`,
      connectionId: connection.id,
      model: model.trim(),
    };
    updateSettings({
      aiConfig: {
        connections: [...aiConfig.connections, connection],
        profiles: [...aiConfig.profiles, profile],
        assignments: {
          chatProfileId: profile.id,
          aiActionsProfileId: profile.id,
        },
      },
    });
    onNext();
  }

  const starterModels = STARTER_MODELS[kind];
  // Match the Connection editor's guards: a base URL is always required, and a
  // non-blank key is required for non-custom providers (custom/local servers may
  // omit it). `.trim()` so whitespace-only input can't enable Add and then
  // persist as an empty key (handleNext saves `apiKey.trim()`).
  const canTest =
    !!model &&
    baseUrl.trim().length > 0 &&
    (kind === "custom" || apiKey.trim().length > 0);

  return (
    <div className="flex flex-col">
      <div className="flex items-center gap-2 mb-0.5">
        <Sparkles className="h-5 w-5 text-primary" />
        <h2 className="text-lg font-semibold">Connect an AI provider</h2>
      </div>
      <p className="text-sm text-muted-foreground mb-4">
        YapStack lets you wire up multiple AI connections — a cloud account
        for fast chat, a local server for private notes, or both at once.
        Different features can use different providers.
      </p>

      <div className="mb-5 rounded-md border border-border bg-muted/40 px-3 py-2.5">
        <p className="text-[11px] leading-relaxed text-muted-foreground">
          <span className="font-medium text-foreground">Your first Connection</span>
          {" "}will be assigned to Chat and AI actions by default. Add more
          Connections and Profiles anytime in{" "}
          <span className="font-medium text-foreground">Settings → AI</span>.
        </p>
      </div>

      <div className="space-y-3 mb-4">
        {/* Provider */}
        <div className="space-y-1.5">
          <Label className="text-xs text-muted-foreground">Provider</Label>
          <Select
            value={kind}
            onValueChange={(v) => handleKindChange(v as AIProviderKind)}
          >
            <SelectTrigger className="h-8 text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {(Object.keys(PROVIDER_LABELS) as AIProviderKind[]).map((p) => (
                <SelectItem key={p} value={p} className="text-xs">
                  {PROVIDER_LABELS[p]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        {/* API Key */}
        <div className="space-y-1.5">
          <Label className="text-xs text-muted-foreground">
            API Key
            {kind === "custom" && (
              <span className="ml-1 text-muted-foreground/60">(optional)</span>
            )}
          </Label>
          <div>
            <div className="relative">
              <Input
                type={showKey ? "text" : "password"}
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                placeholder={kind === "custom" ? "Leave blank if not required" : "sk-..."}
                className="h-8 text-xs pr-8"
              />
              <button
                type="button"
                className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                onClick={() => setShowKey(!showKey)}
              >
                {showKey ? (
                  <EyeOff className="h-3.5 w-3.5" />
                ) : (
                  <Eye className="h-3.5 w-3.5" />
                )}
              </button>
            </div>
            {(kind === "openai" || kind === "openrouter") && (
              <Button variant="link" size="inline" className="mt-1" asChild>
                <a
                  href={
                    kind === "openai"
                      ? "https://platform.openai.com/api-keys"
                      : "https://openrouter.ai/settings/keys"
                  }
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  Get your {kind === "openai" ? "OpenAI" : "OpenRouter"} API key
                  <ExternalLink />
                </a>
              </Button>
            )}
          </div>
        </div>

        {/* Base URL (custom only) + Model */}
        {kind === "custom" ? (
          <>
            <CustomBaseUrlField baseUrl={baseUrl} onChange={setBaseUrl} />
            <CustomModelField
              baseUrl={baseUrl}
              apiKey={apiKey}
              model={model}
              fetchedModels={fetchedModels}
              onModelChange={setModel}
              onFetchedModelsChange={setFetchedModels}
            />
          </>
        ) : (
          <div className="space-y-1.5">
            <Label className="text-xs text-muted-foreground">Model</Label>
            <Select value={model} onValueChange={(v) => setModel(v)}>
              <SelectTrigger className="h-8 text-xs">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {starterModels.map((m) => (
                  <SelectItem key={m.id} value={m.id} className="text-xs">
                    <span className="flex items-center gap-2">
                      {m.label}
                      {m.recommended && (
                        <Badge
                          variant="secondary"
                          className="text-[9px] px-1 py-0"
                        >
                          Recommended
                        </Badge>
                      )}
                    </span>
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        )}

        {/* Test Connection */}
        <div className="space-y-1.5">
          <Button
            size="sm"
            variant="outline"
            className="w-full text-xs"
            onClick={handleTestConnection}
            disabled={testing || !canTest}
          >
            {testing && <Loader2 className="mr-1.5 h-3 w-3 animate-spin" />}
            Test Connection
          </Button>
          {testResult && (
            <Badge
              variant={testResult.ok ? "default" : "destructive"}
              className="text-[10px] w-full justify-center"
            >
              {testResult.ok
                ? "Connected successfully"
                : testResult.error?.slice(0, 60) || "Connection failed"}
            </Badge>
          )}
        </div>
      </div>

      {/* Navigation */}
      <div className="flex gap-2 mt-auto pt-4">
        <Button variant="outline" className="flex-1" onClick={onBack}>
          <ChevronLeft className="mr-1.5 h-4 w-4" />
          Back
        </Button>
        <Button className="flex-1" onClick={handleNext} disabled={!canTest}>
          Add Connection
          <ChevronRight className="ml-1.5 h-4 w-4" />
        </Button>
      </div>
      <button
        type="button"
        className="text-xs text-muted-foreground hover:text-foreground mt-3 mx-auto transition-colors"
        onClick={onNext}
      >
        Skip — set up AI later
      </button>
    </div>
  );
}

// --- Step 4: Ready ---

const SHORTCUT_HINTS = [
  { id: "global.new-session", label: "New recording session" },
  { id: "global.stop-recording", label: "Stop recording" },
  { id: "command-palette", label: "Command palette" },
  { id: "toggle-sidebar", label: "Toggle sidebar" },
];

function KeybindCaptureInline({
  onCapture,
  onCancel,
}: {
  onCapture: (binding: string) => void;
  onCancel: () => void;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const onCaptureRef = useRef(onCapture);
  const onCancelRef = useRef(onCancel);
  onCaptureRef.current = onCapture;
  onCancelRef.current = onCancel;

  const pendingRef = useRef<string | null>(null);
  const [pendingDisplay, setPendingDisplay] = useState<string | null>(null);

  useEffect(() => {
    shortcutCaptureActive.current = true;
    suspendGlobalShortcuts();

    function onKeyDown(e: KeyboardEvent) {
      if (e.repeat) return;
      e.preventDefault();
      e.stopPropagation();

      if (e.key === "Escape") {
        onCancelRef.current();
        return;
      }

      const captured = eventToGlobalBinding(e);
      if (captured) {
        pendingRef.current = captured;
        setPendingDisplay(formatGlobalShortcutDisplay(captured));
      }
    }

    function onKeyUp(e: KeyboardEvent) {
      e.preventDefault();
      e.stopPropagation();
      if (
        !e.metaKey &&
        !e.ctrlKey &&
        !e.shiftKey &&
        !e.altKey &&
        pendingRef.current
      ) {
        onCaptureRef.current(pendingRef.current);
      }
    }

    function onMouseDown(e: MouseEvent) {
      if (
        containerRef.current &&
        !containerRef.current.contains(e.target as Node)
      ) {
        onCancelRef.current();
      }
    }

    function onWindowBlur() {
      if (pendingRef.current) {
        onCaptureRef.current(pendingRef.current);
      } else {
        onCancelRef.current();
      }
    }

    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("keyup", onKeyUp, true);
    window.addEventListener("mousedown", onMouseDown, true);
    window.addEventListener("blur", onWindowBlur);
    return () => {
      shortcutCaptureActive.current = false;
      resumeGlobalShortcuts();
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("keyup", onKeyUp, true);
      window.removeEventListener("mousedown", onMouseDown, true);
      window.removeEventListener("blur", onWindowBlur);
    };
  }, []);

  return (
    <div ref={containerRef} className="flex items-center gap-1">
      <span className="inline-flex items-center justify-center rounded border-2 border-primary bg-primary/5 px-1.5 py-0.5 keybind-display text-[10px] min-w-[56px] animate-pulse text-primary">
        {pendingDisplay ?? "Press keys..."}
      </span>
    </div>
  );
}

function ReadyStep({ onFinish }: { onFinish: () => void }) {
  const dictation = useAppStore((s) => s.settings.dictation);
  const shortcutBindings = useAppStore((s) => s.settings.shortcutBindings);
  const updateSettings = useAppStore((s) => s.updateSettings);

  const [recording, setRecording] = useState(false);

  const defaultSlot = dictation.slots[0];
  const defaultSlotBinding = defaultSlot
    ? shortcutBindings[`global.dictation-${defaultSlot.id}`] ?? defaultSlot.defaultBinding ?? ""
    : "";

  const handleDictationToggle = useCallback(
    (checked: boolean) => {
      updateSettings({
        dictation: { ...dictation, enabled: checked },
      });
    },
    [dictation, updateSettings],
  );

  const handleKeybindCapture = useCallback(
    (binding: string) => {
      if (!defaultSlot) return;
      setRecording(false);
      updateSettings({
        shortcutBindings: {
          ...shortcutBindings,
          [`global.dictation-${defaultSlot.id}`]: binding,
        },
      });
    },
    [defaultSlot, shortcutBindings, updateSettings],
  );

  return (
    <div className="flex flex-col items-center text-center">
      <div className="flex items-center justify-center w-12 h-12 rounded-full bg-primary/10 mb-4">
        <Check className="h-6 w-6 text-primary" />
      </div>

      <h2 className="text-lg font-semibold mb-0.5">You're all set</h2>
      <p className="text-sm text-muted-foreground mb-6">
        Here are a few shortcuts to get you started
      </p>

      {/* Shortcuts grid */}
      <div className="w-full grid gap-2 mb-6">
        {SHORTCUT_HINTS.map((s) => {
          const isGlobal = s.id.startsWith("global.");
          const binding = getBinding(s.id, shortcutBindings);
          return (
            <div
              key={s.id}
              className="flex items-center justify-between px-3 py-1.5 rounded-md bg-muted/50"
            >
              <span className="text-xs text-muted-foreground">{s.label}</span>
              <div className="flex items-center gap-1.5">
                {isGlobal && (
                  <span className="text-[9px] text-primary/60 font-medium uppercase tracking-wide">
                    Global
                  </span>
                )}
                <span className="keybind-display text-[11px] font-medium text-foreground bg-background border rounded px-1.5 py-0.5">
                  {isGlobal ? formatGlobalShortcutDisplay(binding) : formatShortcutDisplay(binding)}
                </span>
              </div>
            </div>
          );
        })}
      </div>

      {/* Dictation callout */}
      <div className="w-full rounded-lg border p-3 mb-6 text-left">
        <div className="flex items-center justify-between mb-1">
          <div className="flex items-center gap-2">
            <Keyboard className="h-4 w-4 text-muted-foreground" />
            <span className="text-sm font-medium">Voice Dictation</span>
          </div>
          <Switch
            size="sm"
            checked={dictation.enabled}
            onCheckedChange={handleDictationToggle}
          />
        </div>
        <p className="text-[11px] text-muted-foreground mb-2">
          Hold a key to dictate anywhere
        </p>
        {dictation.enabled && defaultSlot && (
          <div className="flex items-center justify-between pt-2 border-t">
            <span className="text-xs text-muted-foreground">Keybind</span>
            {recording ? (
              <KeybindCaptureInline
                onCapture={handleKeybindCapture}
                onCancel={() => setRecording(false)}
              />
            ) : (
              <button
                type="button"
                onClick={() => setRecording(true)}
                className="inline-flex items-center justify-center rounded border px-1.5 py-0.5 keybind-display text-[10px] min-w-[56px] hover:border-primary/50 transition-colors"
              >
                {defaultSlotBinding
                  ? formatGlobalShortcutDisplay(defaultSlotBinding)
                  : "Click to set"}
              </button>
            )}
          </div>
        )}
      </div>

      <Button className="w-full" onClick={onFinish}>
        Complete
      </Button>
    </div>
  );
}

// --- Step: Sync (optional) ---

/**
 * Optional: offer multi-device sync. "Set up sync" only records the intent — the
 * wizard itself opens in Settings → Sync once onboarding finishes, so first-run
 * stays short and a user without a server yet loses nothing by skipping.
 */
function SyncStep({
  onChoose,
  onBack,
}: {
  onChoose: (wantsSync: boolean) => void;
  onBack: () => void;
}) {
  return (
    <div className="flex flex-col items-center text-center">
      <DevicesIllustration className="mb-5" />
      <h2 className="text-lg font-semibold mb-0.5">Use YapStack on more than one device?</h2>
      <p className="text-sm text-muted-foreground mb-6">
        Sync keeps your notes and recordings the same everywhere, encrypted
        end-to-end on a server you run. You can set it up now or any time from
        Settings.
      </p>
      <Button className="w-full mb-2" onClick={() => onChoose(true)}>
        Set up sync after this
        <ChevronRight className="ml-1.5 h-4 w-4" />
      </Button>
      <div className="flex w-full gap-2">
        <Button variant="outline" className="flex-1" onClick={onBack}>
          <ChevronLeft className="mr-1.5 h-4 w-4" />
          Back
        </Button>
        <Button variant="outline" className="flex-1" onClick={() => onChoose(false)}>
          Not now
        </Button>
      </div>
    </div>
  );
}

// --- Main Modal ---

export function OnboardingModal() {
  const onboarding = useAppStore((s) => s.settings.onboarding);
  const completeFlow = useAppStore((s) => s.completeFlow);
  const navigateTo = useAppStore((s) => s.navigateTo);
  const setSettingsRequest = useAppStore((s) => s.setSettingsRequest);

  const activeFlow = getActiveFlow(onboarding);

  const [currentStep, setCurrentStep] = useState(0);
  const [direction, setDirection] = useState<"forward" | "backward">("forward");
  // Chosen on the sync step; acted on at finish (Settings → Sync opens the wizard).
  const [wantsSync, setWantsSync] = useState(false);

  // Reset step when active flow changes
  const activeFlowId = activeFlow?.id ?? null;
  useEffect(() => {
    setCurrentStep(0);
    setDirection("forward");
  }, [activeFlowId]);

  if (!activeFlow) return null;

  const totalSteps = activeFlow.steps.length;

  function goNext() {
    setDirection("forward");
    setCurrentStep((s) => Math.min(s + 1, totalSteps - 1));
  }

  function goBack() {
    setDirection("backward");
    setCurrentStep((s) => Math.max(s - 1, 0));
  }

  function handleFinish() {
    completeFlow(activeFlow!.id);
    if (wantsSync) {
      setSettingsRequest("sync-setup");
      navigateTo("settings");
    }
  }

  function handleDismiss() {
    if (!activeFlow!.blocking) {
      completeFlow(activeFlow!.id);
    }
  }

  const nav: StepNav = { onNext: goNext, onBack: goBack, onFinish: handleFinish };
  let stepContent;
  switch (activeFlow.steps[currentStep]) {
    case "welcome":
      stepContent = <WelcomeStep onNext={nav.onNext} />;
      break;
    case "audio":
      stepContent = <AudioStep onNext={nav.onNext} onBack={nav.onBack} />;
      break;
    case "ai":
      stepContent = <AIStep onNext={nav.onNext} onBack={nav.onBack} />;
      break;
    case "sync":
      stepContent = (
        <SyncStep
          onBack={nav.onBack}
          onChoose={(wants) => {
            setWantsSync(wants);
            nav.onNext();
          }}
        />
      );
      break;
    case "ready":
      stepContent = <ReadyStep onFinish={nav.onFinish} />;
      break;
    default:
      stepContent = <WelcomeStep onNext={nav.onNext} />;
      break;
  }

  return (
    <Dialog open modal>
      <DialogPortal>
        <DialogOverlay className="bg-black/60" />
        <DialogPrimitive.Content
          className="fixed left-[50%] top-[50%] z-50 w-full max-w-md translate-x-[-50%] translate-y-[-50%] border bg-background p-6 shadow-lg sm:rounded-lg duration-200 data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=open]:zoom-in-95 data-[state=open]:slide-in-from-left-1/2 data-[state=open]:slide-in-from-top-[48%]"
          onPointerDownOutside={(e) => {
            if (activeFlow.blocking) e.preventDefault();
            else handleDismiss();
          }}
          onEscapeKeyDown={(e) => {
            if (activeFlow.blocking) e.preventDefault();
            else handleDismiss();
          }}
          onInteractOutside={(e) => {
            if (activeFlow.blocking) e.preventDefault();
          }}
        >
          <DialogPrimitive.Title className="sr-only">Setup</DialogPrimitive.Title>
          <DialogPrimitive.Description className="sr-only">
            Walk through the initial app configuration
          </DialogPrimitive.Description>
          {!activeFlow.blocking && (
            <button
              type="button"
              onClick={handleDismiss}
              aria-label="Close"
              className="absolute right-3 top-3 rounded-sm opacity-70 transition-opacity hover:opacity-100"
            >
              <X className="h-4 w-4" />
            </button>
          )}
          <ProgressDots current={currentStep} total={totalSteps} />
          <div
            key={`${activeFlow.id}-${currentStep}`}
            className={
              direction === "forward"
                ? "onboarding-slide-forward"
                : "onboarding-slide-backward"
            }
          >
            {stepContent}
          </div>
        </DialogPrimitive.Content>
      </DialogPortal>
    </Dialog>
  );
}
