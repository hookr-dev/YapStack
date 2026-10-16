export type ShortcutCategory = "Recording" | "Navigation" | "Editor" | "General" | "Dictation";

export interface ShortcutDefinition {
  id: string;
  label: string;
  description: string;
  category: ShortcutCategory;
  defaultBinding: string;
  /** Global shortcuts work even when the app is unfocused. */
  isGlobal?: boolean;
  /** Dictation shortcuts use hold-to-talk (Pressed/Released). */
  isDictation?: boolean;
  /** When true, the shortcut still fires while focus is in an input or editor. */
  allowInEditor?: boolean;
}

export const SHORTCUT_CATEGORIES: ShortcutCategory[] = [
  "Recording",
  "Navigation",
  "Editor",
  "General",
  "Dictation",
];

export const SHORTCUTS: ShortcutDefinition[] = [
  // --- Global ---
  {
    id: "global.new-session",
    label: "New Session",
    description: "Start a new recording session",
    category: "Recording",
    defaultBinding: "CommandOrControl+Alt+N",
    isGlobal: true,
  },
  {
    id: "global.new-session-backfill",
    label: "New Session (Rewind)",
    description: "Start session with full buffer rewind",
    category: "Recording",
    defaultBinding: "CommandOrControl+Alt+R",
    isGlobal: true,
  },
  {
    id: "global.stop-recording",
    label: "Stop Recording",
    description: "Stop the active recording",
    category: "Recording",
    defaultBinding: "CommandOrControl+Alt+S",
    isGlobal: true,
  },
  {
    id: "global.new-note",
    label: "New Note",
    description: "Create a new manual note",
    category: "Recording",
    defaultBinding: "CommandOrControl+Alt+.",
    isGlobal: true,
  },
  // --- In-app ---
  {
    id: "command-palette",
    label: "Search",
    description: "Open search / command palette",
    category: "Navigation",
    defaultBinding: "mod+k",
    allowInEditor: true,
  },
  {
    id: "toggle-sidebar",
    label: "Toggle Sidebar",
    description: "Show or hide the sidebar",
    category: "Navigation",
    defaultBinding: "mod+\\",
    allowInEditor: true,
  },
  {
    id: "open-settings",
    label: "Settings",
    description: "Open settings panel",
    category: "Navigation",
    defaultBinding: "mod+,",
    allowInEditor: true,
  },
  {
    id: "go-back",
    label: "Go Back",
    description: "Return to note list",
    category: "Navigation",
    defaultBinding: "escape",
    // No allowInEditor: Escape dismisses menus / clears selection in the editor.
  },
  {
    id: "filter-all",
    label: "All Sessions",
    description: "Show all sessions",
    category: "Navigation",
    defaultBinding: "mod+1",
    allowInEditor: true,
  },
  {
    id: "filter-pinned",
    label: "Pinned",
    description: "Show pinned notes",
    category: "Navigation",
    defaultBinding: "mod+2",
    allowInEditor: true,
  },
  {
    id: "new-note",
    label: "New Note",
    description: "Create a new manual note",
    category: "Editor",
    defaultBinding: "mod+n",
    allowInEditor: true,
  },
  {
    id: "stop-recording",
    label: "Stop Recording",
    description: "Stop active recording (in-app)",
    category: "Recording",
    defaultBinding: "mod+.",
    allowInEditor: true,
  },
  {
    id: "toggle-chat",
    label: "Toggle Chat",
    description: "Open or close AI chat bar",
    category: "Editor",
    defaultBinding: "mod+j",
    allowInEditor: true,
  },
  {
    id: "pin-session",
    label: "Pin / Unpin",
    description: "Pin or unpin current session",
    category: "Editor",
    defaultBinding: "mod+d",
    allowInEditor: true,
  },
  {
    id: "delete-session",
    label: "Delete Session",
    description: "Delete the current session",
    category: "Editor",
    defaultBinding: "mod+backspace",
    // No allowInEditor: mod+backspace deletes to line start in the editor.
  },
];

/** Map shortcut ID → definition for fast lookup. */
export const SHORTCUT_MAP = new Map<string, ShortcutDefinition>(
  SHORTCUTS.map((s) => [s.id, s]),
);

/**
 * Get the effective binding for a shortcut, using custom overrides or the default.
 */
export function getBinding(
  id: string,
  overrides: Record<string, string>,
): string {
  if (id in overrides) return overrides[id];
  return SHORTCUT_MAP.get(id)?.defaultBinding ?? "";
}

/**
 * Shared flag: when true, shortcut capture is active (recording a new binding).
 * In-app and global shortcut handlers check this to suppress actions during capture.
 */
export const shortcutCaptureActive = { current: false };

export interface ShortcutConflict {
  kind: "static" | "dictation";
  label: string;
}

/**
 * Returns the conflicting shortcut if `binding` is already used in the same
 * scope as `recordingId`, or null when the binding is free. Callers reject
 * the rebind on conflict — overlapping bindings are never silently stolen.
 */
export function findShortcutConflict(
  binding: string,
  recordingId: string,
  isGlobal: boolean,
  overrides: Record<string, string>,
  dictationSlots: { id: string; name: string; defaultBinding?: string }[],
): ShortcutConflict | null {
  for (const shortcut of SHORTCUTS) {
    if (shortcut.id === recordingId) continue;
    if (shortcut.isGlobal !== isGlobal) continue;
    const otherBinding = getBinding(shortcut.id, overrides);
    if (otherBinding && otherBinding === binding) {
      return { kind: "static", label: shortcut.label };
    }
  }

  if (isGlobal) {
    for (const slot of dictationSlots) {
      const slotShortcutId = `global.dictation-${slot.id}`;
      if (slotShortcutId === recordingId) continue;
      const otherBinding = overrides[slotShortcutId] ?? slot.defaultBinding ?? "";
      if (otherBinding && otherBinding === binding) {
        return { kind: "dictation", label: slot.name };
      }
    }
  }

  return null;
}

import { isMac } from "@/lib/utils";

/**
 * Normalise a KeyboardEvent to a binding string like "mod+shift+k".
 * Returns empty string for modifier-only presses.
 */
export function eventToBinding(e: KeyboardEvent): string {
  // Windows/WebView2 can dispatch keydown events with an undefined `key` (media/
  // hardware keys, IME "Process" key, host-synthesized events). WKWebView on macOS
  // always populates `key`, which is why this only crashed on Windows. An event with
  // no key cannot correspond to any binding, so treat it like a modifier-only press.
  const key = e.key?.toLowerCase();
  if (!key) return "";

  // Ignore modifier-only keypresses
  if (["control", "meta", "shift", "alt"].includes(key)) return "";

  const parts: string[] = [];
  const mod = isMac ? e.metaKey : e.ctrlKey;
  if (mod) parts.push("mod");
  if (e.shiftKey) parts.push("shift");
  if (e.altKey) parts.push("alt");

  // Normalise key names
  if (key === ",") parts.push(",");
  else if (key === ".") parts.push(".");
  else if (key === "escape") parts.push("escape");
  else if (key === "backspace") parts.push("backspace");
  else parts.push(key);

  return parts.join("+");
}

/** Map e.code → display key for global bindings. */
export const CODE_TO_KEY: Record<string, string> = {
  Period: ".",
  Comma: ",",
  Slash: "/",
  Backslash: "\\",
  BracketLeft: "[",
  BracketRight: "]",
  Semicolon: ";",
  Quote: "'",
  Backquote: "`",
  Minus: "-",
  Equal: "=",
  Space: "Space",
  Enter: "Enter",
  Backspace: "Backspace",
  Tab: "Tab",
  Escape: "Escape",
  ArrowUp: "Up",
  ArrowDown: "Down",
  ArrowLeft: "Left",
  ArrowRight: "Right",
};

/** Modifier e.code values to filter out. */
const MODIFIER_CODES = new Set([
  "ControlLeft", "ControlRight",
  "MetaLeft", "MetaRight",
  "ShiftLeft", "ShiftRight",
  "AltLeft", "AltRight",
]);

/**
 * Normalise a KeyboardEvent into the Tauri global shortcut format.
 * e.g. "CommandOrControl+Shift+N"
 *
 * Uses e.code (physical key) instead of e.key so that Shift+. produces "."
 * not ">".
 */
export function eventToGlobalBinding(e: KeyboardEvent): string {
  // Same Windows/WebView2 hazard as eventToBinding: `code` can be undefined for
  // host-synthesized/media keys. An event with no physical code maps to no binding.
  const code = e.code;
  if (!code || MODIFIER_CODES.has(code)) return "";

  const parts: string[] = [];
  if (e.metaKey || e.ctrlKey) parts.push("CommandOrControl");
  if (e.shiftKey) parts.push("Shift");
  if (e.altKey) parts.push("Alt");

  if (code.startsWith("Key") && code.length === 4) {
    parts.push(code.charAt(3).toUpperCase());
  } else if (code.startsWith("Digit") && code.length === 6) {
    parts.push(code.charAt(5));
  } else if (code.startsWith("F") && /^F\d{1,2}$/.test(code)) {
    parts.push(code);
  } else if (CODE_TO_KEY[code]) {
    parts.push(CODE_TO_KEY[code]);
  } else {
    parts.push(code);
  }

  return parts.join("+");
}
