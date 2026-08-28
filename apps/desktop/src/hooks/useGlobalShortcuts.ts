import { useEffect } from "react";
import { register, unregisterAll } from "@tauri-apps/plugin-global-shortcut";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useAppStore } from "@/stores/appStore";
import type { DictationSlot } from "@/stores/appStore";
import { SHORTCUTS, SHORTCUT_MAP, getBinding, shortcutCaptureActive, CODE_TO_KEY } from "@/lib/shortcuts";
import { trackShortcutUsed } from "@/lib/analytics";
import { toast } from "sonner";

/**
 * Build a map from Tauri global-shortcut binding → shortcut ID.
 * Only includes global shortcuts from the static SHORTCUTS array.
 */
function buildGlobalBindingMap(
  overrides: Record<string, string>,
  slots: DictationSlot[],
): { bindingMap: Map<string, string>; dictationSlotMap: Map<string, string> } {
  const bindingMap = new Map<string, string>();
  const dictationSlotMap = new Map<string, string>(); // shortcut ID → slot ID

  // Static global shortcuts (non-dictation)
  for (const shortcut of SHORTCUTS) {
    if (!shortcut.isGlobal) continue;
    const binding = getBinding(shortcut.id, overrides);
    if (binding) bindingMap.set(normalizeEventShortcut(binding), shortcut.id);
  }

  // Dynamic dictation slot shortcuts
  for (const slot of slots) {
    if (!slot.enabled) continue;
    const shortcutId = `global.dictation-${slot.id}`;
    const binding = overrides[shortcutId] ?? slot.defaultBinding;
    if (binding) {
      bindingMap.set(normalizeEventShortcut(binding), shortcutId);
      dictationSlotMap.set(shortcutId, slot.id);
    }
  }

  return { bindingMap, dictationSlotMap };
}

/**
 * The Tauri global-hotkey Rust crate's `HotKey::into_string()` produces a
 * canonical format that differs from the registration string:
 *   Registered:  "CommandOrControl+Shift+N"
 *   Event gives: "shift+super+KeyN"  (macOS)
 *
 * This function normalizes both formats to a single canonical form so that
 * the binding map lookup always succeeds.
 */
const MODIFIER_ORDER: Record<string, number> = {
  CommandOrControl: 0,
  Shift: 1,
  Alt: 2,
};

function normalizeEventShortcut(canonical: string): string {
  const parts = canonical.split("+");
  const modifiers: string[] = [];
  let key = "";

  for (const part of parts) {
    const lower = part.toLowerCase();
    if (lower === "super" || lower === "control" || lower === "commandorcontrol") {
      if (!modifiers.includes("CommandOrControl")) {
        modifiers.push("CommandOrControl");
      }
    } else if (lower === "shift") {
      if (!modifiers.includes("Shift")) modifiers.push("Shift");
    } else if (lower === "alt") {
      if (!modifiers.includes("Alt")) modifiers.push("Alt");
    } else {
      // Key code — strip "Key" prefix for letters, map specials
      if (part.startsWith("Key") && part.length === 4) {
        key = part.charAt(3).toUpperCase();
      } else if (part.startsWith("Digit") && part.length === 6) {
        key = part.charAt(5);
      } else if (CODE_TO_KEY[part]) {
        key = CODE_TO_KEY[part];
      } else if (part.startsWith("F") && /^F\d{1,2}$/.test(part)) {
        key = part; // F1-F12
      } else {
        key = part; // fallback: use as-is
      }
    }
  }

  modifiers.sort((a, b) => (MODIFIER_ORDER[a] ?? 99) - (MODIFIER_ORDER[b] ?? 99));
  return [...modifiers, key].join("+");
}

async function focusWindow() {
  try {
    const win = getCurrentWindow();
    await win.show();
    await win.setFocus();
  } catch {
    // Graceful degradation
  }
}

/** Tracks slots currently active in toggle mode (first press starts, second stops). */
const toggleActiveSlots = new Set<string>();

/** Module-level handle so recording components can suspend/resume. */
let reregisterFn: (() => void) | null = null;

/** Only show the registration failure toast once per app session. */
let toastShownForFailures = false;

/**
 * Unregister all global shortcuts so the webview can receive key events
 * during shortcut capture (rebinding in settings).
 */
export function suspendGlobalShortcuts() {
  unregisterAll().catch(() => {});
}

/**
 * Re-register global shortcuts after shortcut capture ends.
 */
export function resumeGlobalShortcuts() {
  reregisterFn?.();
}

/** Registers and manages global OS-level keyboard shortcuts via Tauri plugin. Mounted in App.tsx. */
export function useGlobalShortcuts() {
  const overrides = useAppStore((s) => s.settings.shortcutBindings);
  const slots = useAppStore((s) => s.settings.dictation.slots);

  // Clear toggle state when dictation goes idle (handles error/completion paths)
  useEffect(() => {
    const handler = () => toggleActiveSlots.clear();
    window.addEventListener("yapstack:dictation-idle", handler);
    return () => window.removeEventListener("yapstack:dictation-idle", handler);
  }, []);

  useEffect(() => {
    let cancelled = false;

    async function setup() {
      try {
        await unregisterAll();
      } catch {
        // May fail if nothing was registered
      }

      if (cancelled || shortcutCaptureActive.current) return;

      const { bindingMap, dictationSlotMap } = buildGlobalBindingMap(overrides, slots);
      const bindings = Array.from(bindingMap.keys());

      if (bindings.length === 0) return;

      const handler = (event: { shortcut: string; state: string }) => {
        if (shortcutCaptureActive.current) return;

        const normalized = normalizeEventShortcut(event.shortcut);
        const id = bindingMap.get(normalized);

        console.debug(
          `[global-shortcut] event="${event.shortcut}" state="${event.state}" normalized="${normalized}" id=${id ?? "none"}`,
        );

        if (!id) return;

        // Dictation shortcuts: hold or toggle mode
        const slotId = dictationSlotMap.get(id);
        if (slotId) {
          const activationMode = useAppStore.getState().settings.dictation.activationMode ?? "hold";

          if (activationMode === "toggle") {
            // Toggle: only react to Pressed
            if (event.state === "Pressed") {
              if (toggleActiveSlots.has(slotId)) {
                toggleActiveSlots.delete(slotId);
                window.dispatchEvent(new CustomEvent("yapstack:dictation-stop"));
              } else {
                toggleActiveSlots.add(slotId);
                window.dispatchEvent(
                  new CustomEvent("yapstack:dictation-start", { detail: { slotId } }),
                );
              }
            }
          } else {
            // Hold: Pressed starts, Released stops
            if (event.state === "Pressed") {
              window.dispatchEvent(
                new CustomEvent("yapstack:dictation-start", { detail: { slotId } }),
              );
            } else {
              window.dispatchEvent(new CustomEvent("yapstack:dictation-stop"));
            }
          }
          return;
        }

        // Non-dictation shortcuts: only handle key-down
        if (event.state !== "Pressed") return;

        trackShortcutUsed({ shortcut_id: id });
        const s = useAppStore.getState();

        switch (id) {
          // Both new-session cases are fire-and-forget: `createAndStartSession`
          // never rejects and toasts its own failures (it used to reject unhandled
          // here, so a start that lost the DB write lock showed the user nothing).
          case "global.new-session": {
            if (
              s.enginePhase === "ready" &&
              s.captureStatus?.state === "Capturing" &&
              !s.activeSessionId
            ) {
              void focusWindow().then(() => s.createAndStartSession(0, "shortcut"));
            }
            break;
          }
          case "global.new-session-backfill": {
            if (
              s.enginePhase === "ready" &&
              s.captureStatus?.state === "Capturing" &&
              !s.activeSessionId
            ) {
              void focusWindow().then(() => {
                const avail = Math.max(
                  s.bufferInfo?.mic?.available_seconds ?? 0,
                  s.bufferInfo?.system?.available_seconds ?? 0,
                );
                return s.createAndStartSession(avail, "shortcut");
              });
            }
            break;
          }
          case "global.stop-recording": {
            if (s.activeSessionId) {
              focusWindow().then(() => s.stopActiveSession());
            }
            break;
          }
          case "global.new-note": {
            focusWindow().then(() => s.createManualNote());
            break;
          }
        }
      };

      // Register each shortcut individually to prevent one conflict from
      // blocking all subsequent shortcuts (Win32 RegisterHotKey is exclusive).
      const failed: { binding: string; id: string }[] = [];
      for (const binding of bindings) {
        try {
          await register([binding], handler);
        } catch (e) {
          const id = bindingMap.get(binding) ?? binding;
          failed.push({ binding, id });
          console.warn(`[global-shortcut] Failed to register "${binding}" (${id}): ${e}`);
        }
      }

      if (failed.length > 0 && !toastShownForFailures) {
        toastShownForFailures = true;
        const names = failed
          .map((f) => SHORTCUT_MAP.get(f.id)?.label ?? f.id)
          .join(", ");
        toast.warning("Some shortcuts couldn't be registered", {
          description: `${names} — likely claimed by another app. Rebind in Settings > Shortcuts.`,
          duration: 8000,
        });
      }
      if (failed.length === 0) {
        toastShownForFailures = false;
      }
    }

    reregisterFn = () => { if (!cancelled) setup(); };
    setup();

    return () => {
      cancelled = true;
      reregisterFn = null;
      unregisterAll().catch(() => {});
    };
  }, [overrides, slots]);
}
