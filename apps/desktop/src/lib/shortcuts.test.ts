import { describe, it, expect } from "vitest";
import {
  SHORTCUTS,
  SHORTCUT_MAP,
  getBinding,
  eventToBinding,
  eventToGlobalBinding,
} from "./shortcuts";

describe("SHORTCUTS / SHORTCUT_MAP", () => {
  it("has shortcuts defined", () => {
    expect(SHORTCUTS.length).toBeGreaterThan(10);
  });

  it("looks up a known ID", () => {
    const def = SHORTCUT_MAP.get("command-palette");
    expect(def).toBeDefined();
    expect(def!.defaultBinding).toBe("mod+k");
  });

  it("returns undefined for unknown ID", () => {
    expect(SHORTCUT_MAP.get("nonexistent")).toBeUndefined();
  });
});

describe("allowInEditor flag", () => {
  it("marks command-palette as allowed in editor", () => {
    expect(SHORTCUT_MAP.get("command-palette")?.allowInEditor).toBe(true);
  });

  it("rebinds toggle-sidebar to mod+\\ (Notion convention) and allows it in the editor", () => {
    const def = SHORTCUT_MAP.get("toggle-sidebar");
    expect(def?.defaultBinding).toBe("mod+\\");
    expect(def?.allowInEditor).toBe(true);
  });

  it("does not mark go-back (escape) as allowed in editor", () => {
    expect(SHORTCUT_MAP.get("go-back")?.allowInEditor).toBeFalsy();
  });

  it("does not mark delete-session (mod+backspace) as allowed in editor — collides with line delete", () => {
    expect(SHORTCUT_MAP.get("delete-session")?.allowInEditor).toBeFalsy();
  });

  it("marks navigation/global-feeling shortcuts as allowed in editor", () => {
    for (const id of [
      "toggle-sidebar",
      "open-settings",
      "filter-all",
      "filter-pinned",
      "new-note",
      "stop-recording",
      "toggle-chat",
      "pin-session",
    ]) {
      expect(SHORTCUT_MAP.get(id)?.allowInEditor, `${id} should be allowInEditor`).toBe(true);
    }
  });
});

describe("getBinding", () => {
  it("returns default binding when no override", () => {
    expect(getBinding("command-palette", {})).toBe("mod+k");
  });

  it("returns override when provided", () => {
    expect(getBinding("command-palette", { "command-palette": "mod+l" })).toBe(
      "mod+l",
    );
  });

  it("returns empty string for unknown ID with no override", () => {
    expect(getBinding("nonexistent", {})).toBe("");
  });

  it("returns override even for unknown ID", () => {
    expect(getBinding("nonexistent", { nonexistent: "mod+x" })).toBe("mod+x");
  });
});

describe("eventToBinding", () => {
  const makeEvent = (
    overrides: Partial<KeyboardEvent>,
  ): KeyboardEvent =>
    ({
      key: "",
      code: "",
      metaKey: false,
      ctrlKey: false,
      shiftKey: false,
      altKey: false,
      ...overrides,
    }) as KeyboardEvent;

  it("returns empty string for modifier-only keypresses", () => {
    expect(eventToBinding(makeEvent({ key: "Control" }))).toBe("");
    expect(eventToBinding(makeEvent({ key: "Meta" }))).toBe("");
    expect(eventToBinding(makeEvent({ key: "Shift" }))).toBe("");
    expect(eventToBinding(makeEvent({ key: "Alt" }))).toBe("");
  });

  it("formats Ctrl+K (non-Mac uses ctrlKey as mod)", () => {
    expect(
      eventToBinding(makeEvent({ key: "k", ctrlKey: true })),
    ).toBe("mod+k");
  });

  it("formats Ctrl+Shift+N", () => {
    expect(
      eventToBinding(
        makeEvent({ key: "n", ctrlKey: true, shiftKey: true }),
      ),
    ).toBe("mod+shift+n");
  });

  it("formats Escape", () => {
    expect(eventToBinding(makeEvent({ key: "Escape" }))).toBe("escape");
  });

  it("formats Alt+P", () => {
    expect(eventToBinding(makeEvent({ key: "p", altKey: true }))).toBe(
      "alt+p",
    );
  });

  it("formats period key", () => {
    expect(
      eventToBinding(makeEvent({ key: ".", ctrlKey: true })),
    ).toBe("mod+.");
  });

  // Windows/WebView2 can dispatch keydown events with an undefined `key` (media/
  // hardware keys, IME "Process" key). This crashed the window-level keydown handler
  // with "Cannot read properties of undefined (reading 'toLowerCase')". A keyless
  // event maps to no binding and must not throw.
  it("returns empty string (no throw) when e.key is undefined (Windows/WebView2)", () => {
    const evt = { ...makeEvent({}), key: undefined } as unknown as KeyboardEvent;
    expect(() => eventToBinding(evt)).not.toThrow();
    expect(eventToBinding(evt)).toBe("");
  });
});

describe("eventToGlobalBinding", () => {
  const makeEvent = (
    overrides: Partial<KeyboardEvent>,
  ): KeyboardEvent =>
    ({
      key: "",
      code: "",
      metaKey: false,
      ctrlKey: false,
      shiftKey: false,
      altKey: false,
      ...overrides,
    }) as KeyboardEvent;

  it("formats KeyN+meta", () => {
    expect(
      eventToGlobalBinding(makeEvent({ code: "KeyN", metaKey: true })),
    ).toBe("CommandOrControl+N");
  });

  it("formats Digit1+meta", () => {
    expect(
      eventToGlobalBinding(makeEvent({ code: "Digit1", metaKey: true })),
    ).toBe("CommandOrControl+1");
  });

  it("formats F12", () => {
    expect(
      eventToGlobalBinding(makeEvent({ code: "F12" })),
    ).toBe("F12");
  });

  it("returns empty string for modifier-only codes", () => {
    expect(
      eventToGlobalBinding(makeEvent({ code: "MetaLeft" })),
    ).toBe("");
    expect(
      eventToGlobalBinding(makeEvent({ code: "ShiftRight" })),
    ).toBe("");
  });

  it("formats shift+period", () => {
    expect(
      eventToGlobalBinding(
        makeEvent({ code: "Period", metaKey: true, shiftKey: true }),
      ),
    ).toBe("CommandOrControl+Shift+.");
  });

  // Same Windows/WebView2 hazard: an undefined `code` must not throw.
  it("returns empty string (no throw) when e.code is undefined (Windows/WebView2)", () => {
    const evt = { ...makeEvent({}), code: undefined } as unknown as KeyboardEvent;
    expect(() => eventToGlobalBinding(evt)).not.toThrow();
    expect(eventToGlobalBinding(evt)).toBe("");
  });
});
