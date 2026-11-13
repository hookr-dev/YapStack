# Frontend Guide

Tailwind tokens, component inventory, framework stack, and the UX interaction language used in YapStack's desktop app.

Scope: `apps/desktop/src/**`. For backend/transcription, see [`ARCHITECTURE.md`](./ARCHITECTURE.md); for how the AI chat reads from this UI, see [`AI_CONTEXT.md`](./AI_CONTEXT.md).

---

## Tailwind v4 + Design Tokens

Styling is [Tailwind v4](https://tailwindcss.com) with **config-in-CSS**. There is no `tailwind.config.*` file — everything lives in [`apps/desktop/src/index.css`](../apps/desktop/src/index.css).

- `@import "tailwindcss"` loads the engine.
- `@theme inline { ... }` declares `--color-*` and `--radius-*` variables bound to the runtime CSS custom properties. Tailwind utility classes (`bg-background`, `text-foreground`, `rounded-lg`, …) resolve to these variables.
- `@custom-variant dark (&:is(.dark *))` is the dark-mode selector. Toggle by adding/removing `.dark` on a root ancestor.

### Color palette

All colors use [OKLch](https://oklch.com). Runtime values are defined twice: once on `:root` (light), once on `.dark`. Do not inline hex values in components — always consume the tokens.

| Group | Tokens |
| --- | --- |
| Surface | `--background`, `--foreground`, `--card`, `--card-foreground`, `--popover`, `--popover-foreground` |
| Primary | `--primary`, `--primary-foreground` |
| Secondary / muted / accent | `--secondary`, `--secondary-foreground`, `--muted`, `--muted-foreground`, `--accent`, `--accent-foreground` |
| Destructive | `--destructive` |
| Form | `--border`, `--input`, `--ring` |
| Charts | `--chart-1` … `--chart-5` |
| Sidebar | `--sidebar`, `--sidebar-foreground`, `--sidebar-primary`, `--sidebar-primary-foreground`, `--sidebar-accent`, `--sidebar-accent-foreground`, `--sidebar-border`, `--sidebar-ring` |

### Radius scale

One base, everything else calc'd off it:

```css
--radius: 0.625rem;     /* base */
--radius-sm:  calc(var(--radius) - 4px);
--radius-md:  calc(var(--radius) - 2px);
--radius-lg:  var(--radius);
--radius-xl:  calc(var(--radius) + 4px);
--radius-2xl: calc(var(--radius) + 8px);
--radius-3xl: calc(var(--radius) + 12px);
--radius-4xl: calc(var(--radius) + 16px);
```

Change `--radius` in `:root` to retheme corner rounding globally.

### Dark mode

Class-based. The toggle lives on the Zustand settings store (`stores/appStore.ts::settings.theme`) and is applied by adding `.dark` to `<html>`. Per-token overrides for dark mode are the only things that matter — consuming components don't branch on theme.

---

## Component Inventory

All 25 shadcn-style primitives live under [`apps/desktop/src/components/ui/`](../apps/desktop/src/components/ui). Each wraps a Radix primitive (or thin utility) and consumes design tokens:

| File | Purpose |
| --- | --- |
| `alert-dialog.tsx` | Destructive confirmation (delete, etc.) |
| `alert.tsx` | Inline banners (info / warn / error) |
| `badge.tsx` | Compact status chip |
| `button.tsx` | Primary button variants (CVA-driven) |
| `card.tsx` | Surface container (`bg-card`) |
| `collapsible.tsx` | Used by `FloatingChatBar` expand/collapse |
| `command.tsx` | `cmdk` wrapper for the search palette |
| `context-menu.tsx` | Right-click menus (session rows, folders) |
| `dialog.tsx` | Modal surface (folder CRUD, settings sub-flows) |
| `dropdown-menu.tsx` | Button-triggered menus |
| `input.tsx` | Single-line text field |
| `label.tsx` | Form label primitive |
| `popover.tsx` | Non-modal floating content |
| `progress.tsx` | Download / backfill progress bars |
| `resizable.tsx` | `react-resizable-panels` wrapper (transcript ↔ editor split) |
| `scroll-area.tsx` | Custom-scrollbar container |
| `select.tsx` | Dropdown select (model / language / device pickers) |
| `separator.tsx` | Horizontal / vertical rule |
| `sheet.tsx` | Side drawers |
| `slider.tsx` | Volume, VAD threshold, playback speed |
| `sonner.tsx` | Toast provider |
| `switch.tsx` | Boolean settings (diarization, theme, etc.) |
| `tabs.tsx` | Settings panel tabs |
| `textarea.tsx` | Multi-line text field |
| `tooltip.tsx` | Hover / focus tips |

**Adding a new primitive:** prefer `npx shadcn add <name>` so the wrapper inherits the project's token conventions. Hand-rolling is fine for app-specific composites (e.g. `ContextPill`, `ModelPickerPill` under `components/chat/`) — those don't belong in `ui/`.

### Settings slot cards

Repeatable per-item config cards in the Settings tabs (Dictation slots, Insights) share one layout. The **canonical implementation is `SlotCard` in [`DictationTab.tsx`](../apps/desktop/src/components/settings/DictationTab.tsx)**; `InsightCard` mirrors it. New "list of configurable things" tabs should match this so the tabs feel like one family:

- **Card:** `rounded-lg border p-3 space-y-2`.
- **Row 1 — identity:** `flex items-center justify-between gap-2` → a compact name `<input>` (`h-6 text-xs font-medium rounded border border-border bg-transparent px-2 max-w-[160px]`) on the left, a `Trash2` delete button (`h-3 w-3`, muted → `hover:text-destructive`) on the right.
- **Row 2 — controls:** `flex items-center justify-between gap-3`. Each control is its own group — `flex items-center gap-1.5 shrink-0` — with an `text-[11px] text-muted-foreground` label followed by a compact control (`Select` triggers are `!h-6 … text-[11px] px-2 py-0`; AI assignment is `<ProfilePicker variant="pill">`). **Use `justify-between` to spread the groups edge-to-edge — do not left-pack with `flex-wrap`/`gap-x-*`.** This even distribution is the signature of the pattern.
- **Body — freeform:** a full-width `textarea` (`w-full rounded-md border bg-muted/50 px-2.5 py-1.5 text-xs resize-none`), shown always or conditionally.

Tab chrome around the cards also matches: a master `Switch` header row (`flex items-center justify-between`), `Separator`s between sections, a `text-xs text-muted-foreground` section label above the card list, and a full-width outline `Button` (`Plus` icon) to add a new card.

### Tiptap + chat markdown

Rich-text styling lives in `@layer components` of `index.css`:

- `.tiptap-editor` — headings, lists, blockquotes, code, task lists, segment-ref pills (`span[data-segment-ref]`), link hover, highlight `<mark>`. Themed checklist checkboxes (accent fill, contrast checkmark, focus-visible ring, first-line alignment) and a 5-color highlight palette (yellow/green/blue/purple/red) stored as CSS variable references so highlights re-theme automatically across light/dark. Used by `NoteEditor`.
- `.ai-chat-markdown` — smaller type scale, compact margins. Used by the chat response bubble. Headings are intentionally demoted in size (`h1` → `0.875rem`) and the system prompts tell the model to prefer `**bold**` over `#` headings.

**Toolbar / bubble-menu split** mirrors Notion / Linear / Novel:

- **Static toolbar** holds block-level formatting — heading dropdown (shows the active level: H1 / H2 / Normal), lists, blockquote, code block, link.
- **Selection bubble menu** is scoped to inline marks — bold, italic, underline, strike, code, highlight, link. Floating UI's `flip` / `shift` use the editor's contenteditable as the boundary so the bubble can't land on the toolbar above or the floating chat bar below; renders at `z-50` so it sits above other floating UI.
- Both surfaces re-render on selection changes via `useEditorState` and gain an accent underline when their mark/block is active, so cases like "bold persists on a new line" are visible.

**In-editor shortcuts.** Command palette (⌘K), sidebar (⌘\\), settings (⌘,), filter switches (⌘1/⌘2), new note (⌘N), stop recording (⌘.), toggle chat (⌘J), and pin (⌘D) fire while the editor has focus. Escape and ⌘⌫ still defer to the editor. The default sidebar binding is **⌘\\** (not ⌘B) so it doesn't fight Tiptap's bold binding inside notes.

---

## Framework & FE Preferences

React 19.0 + TypeScript 5.7 on Vite 6. Single Zustand store for UI state; SQLite for everything persisted.

### Libraries (from [`apps/desktop/package.json`](../apps/desktop/package.json))

| Concern | Library | Notes |
| --- | --- | --- |
| UI framework | `react@19` / `react-dom@19` | Strict mode + React Compiler compatible. |
| Language | `typescript@5.7` | `tsc --noEmit` in `pnpm check`. |
| Build | `vite@6` + `@tailwindcss/vite@4` | No Webpack, no Next. |
| State | `zustand@5` with `persist` | Single global store at `stores/appStore.ts`. **No React Query / SWR.** |
| DB | `@tauri-apps/plugin-sql@2` | Authoritative for session / segment / note / folder / chat / dictation writes. |
| Routing | — | Client-side view state on the Zustand store. `appStore.navigateTo()` switches between `"note-list" \| "note-detail" \| "settings"`. |
| Rich text | `@tiptap/*@3.19` | starter-kit + bubble-menu, highlight, link, placeholder, task-item, task-list, typography, underline, markdown. |
| Markdown display | `marked@17`, `react-markdown@10` | `marked` in `markdownToBasicHtml` for AI → Tiptap conversion; `react-markdown` for chat bubbles. |
| Drag-drop | `@dnd-kit/core`, `@dnd-kit/sortable`, `@dnd-kit/utilities` | Folder reorder + session-to-folder drop. |
| Command palette | `cmdk@1.1` | `components/SearchCommand.tsx`. |
| Split panes | `react-resizable-panels@4.6` | `NoteDetailView`, `AppLayout`. |
| Toasts | `sonner@2` | Used for undo toasts on AI tool calls. |
| Drawers | `vaul@1.1` | Used by dictation history. |
| Icons | `lucide-react@0.563` | Every icon across the app — do not mix icon sets. |
| Class utils | `class-variance-authority`, `clsx`, `tailwind-merge` | CVA for button/badge variants, `cn()` utility for merging. |
| HTML safety | `dompurify@3` | Sanitizes AI-authored HTML before it hits Tiptap. |
| OpenAI client | `openai@6` | Used against OpenAI, OpenRouter, and any compatible endpoint (see `LOCAL_LLM.md`). |
| Audio visualizer | `react-audio-visualize` | Waveform on the audio player. |

### Tauri plugins

`@tauri-apps/plugin-{sql, dialog, fs, global-shortcut, http, opener, process, updater}@2` plus `@tauri-apps/api@2.10`. All backend IO flows through these — the frontend does not do raw network calls for app data.

### Testing

`vitest@4` + `@testing-library/react@16`, `jsdom@27`. Tauri commands are stubbed via [`apps/desktop/src/test/tauri-mocks.ts`](../apps/desktop/src/test/tauri-mocks.ts). See [`PRINCIPLES.md`](./PRINCIPLES.md) for testing posture.

---

## UX / Interaction Language

### Keyboard shortcuts

Registry: [`apps/desktop/src/lib/shortcuts.ts`](../apps/desktop/src/lib/shortcuts.ts) — `SHORTCUTS` array. Customization via `getBinding(id, overrides)`. Event normalization via `eventToBinding()` (in-app) and `eventToGlobalBinding()` (Tauri plugin-global-shortcut format).

Two-tier dispatch:

1. **Global** (`isGlobal: true`) — handled by `tauri-plugin-global-shortcut`; fire even when the app is unfocused. Registered in `useGlobalShortcuts` in `App.tsx`.
2. **In-app** — DOM keydown listener on the window, routed through `useKeyboardShortcuts` in `AppLayout`. Suppressed while `shortcutCaptureActive.current === true` (rebinding mode).

Defaults (Mac bindings shown; `mod` = ⌘ on Mac, Ctrl elsewhere):

| ID | Default | Category | Scope |
| --- | --- | --- | --- |
| `global.new-session` | ⌘⌥N | Recording | Global |
| `global.new-session-backfill` | ⌘⌥R | Recording | Global |
| `global.stop-recording` | ⌘⌥S | Recording | Global |
| `global.new-note` | ⌘⌥. | Recording | Global |
| `command-palette` | ⌘K | Navigation | In-app |
| `toggle-sidebar` | ⌘\ | Navigation | In-app |
| `open-settings` | ⌘, | Navigation | In-app |
| `go-back` | Esc | Navigation | In-app |
| `filter-all` | ⌘1 | Navigation | In-app |
| `filter-pinned` | ⌘2 | Navigation | In-app |
| `new-note` | ⌘N | Editor | In-app |
| `stop-recording` | ⌘. | Recording | In-app |
| `toggle-chat` | ⌘J | Editor | In-app |
| `pin-session` | ⌘D | Editor | In-app |
| `delete-session` | ⌘Backspace | Editor | In-app |

Dictation shortcuts (hold-to-talk) live per-slot on the settings store, not in `SHORTCUTS` — each slot carries its own `keybind` string and the dispatcher treats them as `isDictation: true`.

### Interaction patterns

- **Command palette** — ⌘K opens `SearchCommand` (`cmdk`). Searches session titles, notes, folder names, and segment text. No tags today; see [`AI_CONTEXT.md`](./AI_CONTEXT.md) § Tags (pending).
- **Floating chat bar** — ⌘J toggles, also triggered by the custom event `yapstack:toggle-chat`. Collapse animation is CSS-driven (see `[data-slot="collapsible-content"]` in `index.css`).
- **Split pane (`NoteDetailView`)** — transcript left, editor right for completed sessions; full-width editor for manual notes. Resize state is **not persisted** across sessions — intentional (see `PRINCIPLES.md` on avoiding speculative persistence).
- **Transcript list (`ChatView`)** — virtualized with `@tanstack/react-virtual`: only rows near the viewport are mounted, built from the flat row model in `components/transcript/rows.ts` (segment, speaker-header and backfill-divider rows, each with a stable key). The segment being edited (`editingSegmentId`) and the row holding focus stay mounted wherever the viewport is, because the contentEditable and the D4 edit guard live in the row. Scroll anchoring is key-based (the virtualizer's `anchorTo: "end"`): an insert keeps the row being read in place, and a viewport at the bottom stays at the bottom. Anything that needs every row (drag-select, playback follow) reads the virtualizer's measurements, not the DOM. Tests that render the transcript need the opt-in jsdom layout in `src/test/virtual-layout.ts`.
- **Drag-drop** — `@dnd-kit` with mouse + keyboard sensors. Sessions → folders, folders reorder within the sidebar. Branch conflicts (dropping a session into both an ancestor and a descendant folder) are detected by `findBranchConflicts()` in `folder-tree.ts`.
- **Citations** — AI output containing `[[seg:ID]]` is converted to `<span data-segment-ref …>` via `convertCitationsToSegmentRefs()` in `ai-tools.ts`. The pill shows `MM:SS`; clicking seeks the audio player.
- **Tool feedback** — AI tool calls surface as `sonner` toasts with a 10 s undo window. `executeTool` + `undoToolCalls` in `ai-tools.ts` handle both sides.
- **Dictation bubble** — separate Tauri window (NSPanel on macOS). Floats above all spaces while dictation is active; shows listening → transcribing → processing → done states.
- **Recording indicator** — separate 56×120 transparent always-on-top window; visible only when recording *and* the main window is unfocused. Click returns to the active session.

### Overlay windows

Dictation and recording-indicator are real Tauri WebviewWindows. On macOS they are converted to NSPanels (`tauri-nspanel`) so they float non-activating at the `MainMenu` panel level. On other platforms, `visibleOnAllWorkspaces: true` in `tauri.conf.json` gives the equivalent behavior. Frontend always calls `commands.showOverlayPanel(label)` / `commands.hideOverlayPanel(label)` — don't branch on platform in component code.

---

## Conventions

- **Token discipline.** No inline hex. If you need a new color, add a token in `index.css` first.
- **One icon set.** `lucide-react` throughout. Don't import emoji, heroicons, or inline SVGs for iconography.
- **State locality.** Component-local state first; reach for the Zustand store only for cross-component or persisted state. Derived state belongs in selectors, not effects.
- **DB reads are async.** There is no query cache. If you're reading the same data in two components, lift it into the store or into a single parent — don't double-fetch.
- **Serialize segment writes.** Live and backfill both write segments; race protection lives in the `segmentQueueTail` promise queue in `stores/appStore.ts`. Don't bypass it.
- **DTOs for types.** Types sent across the Tauri boundary are generated into `src/lib/types.ts` by Specta. Do not hand-edit that file — change the backend DTO in `apps/desktop/src-tauri/src/commands/` and regenerate.

See [`PRINCIPLES.md`](./PRINCIPLES.md) for deeper design, testing, and coding rationale.
