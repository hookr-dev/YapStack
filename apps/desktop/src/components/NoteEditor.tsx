import { useEffect, useRef, useCallback } from "react";
import { useEditor, EditorContent, useEditorState } from "@tiptap/react";
import { BubbleMenu } from "@tiptap/react/menus";
import StarterKit from "@tiptap/starter-kit";
import { Markdown } from "@tiptap/markdown";
import type { JSONContent } from "@tiptap/core";
import type { Slice } from "@tiptap/pm/model";
import Placeholder from "@tiptap/extension-placeholder";
import TaskList from "@tiptap/extension-task-list";
import TaskItem from "@tiptap/extension-task-item";
import Highlight from "@tiptap/extension-highlight";
import Underline from "@tiptap/extension-underline";
import Link from "@tiptap/extension-link";
import Typography from "@tiptap/extension-typography";
import {
  Bold,
  Italic,
  Underline as UnderlineIcon,
  Strikethrough,
  List,
  ListOrdered,
  ListChecks,
  Quote,
  Code,
  SquareCode,
  Highlighter,
  ChevronDown,
  Link as LinkIcon,
  ALargeSmall,
  Eraser,
} from "lucide-react";
import type { Editor } from "@tiptap/react";
import { toast } from "sonner";
import {
  saveNote,
  getNote,
  getSessionSegments,
} from "@/lib/db";
import { useAppStore } from "@/stores/appStore";
import { log } from "@/lib/logger";
import { SegmentReference } from "@/lib/tiptap-segment-ref";
import { convertCitationsToSegmentRefs } from "@/lib/ai-tools";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

// `value` is the literal string TipTap writes into mark inline `style="background-color: ..."`,
// so a `var(...)` reference resolves per theme at render time.
const HIGHLIGHT_COLORS = [
  { name: "Yellow", value: "var(--tt-color-highlight-yellow)" },
  { name: "Green", value: "var(--tt-color-highlight-green)" },
  { name: "Blue", value: "var(--tt-color-highlight-blue)" },
  { name: "Purple", value: "var(--tt-color-highlight-purple)" },
  { name: "Red", value: "var(--tt-color-highlight-red)" },
] as const;

type HighlightColor = (typeof HIGHLIGHT_COLORS)[number]["value"];

function ToolbarButton({
  onClick,
  isActive,
  tooltip,
  children,
}: {
  onClick: () => void;
  isActive: boolean;
  tooltip: string;
  children: React.ReactNode;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant="ghost"
          size="icon-xs"
          onClick={onClick}
          data-active={isActive || undefined}
          className="relative data-[active]:bg-accent data-[active]:text-accent-foreground data-[active]:after:absolute data-[active]:after:bottom-0.5 data-[active]:after:left-1/2 data-[active]:after:h-0.5 data-[active]:after:w-3 data-[active]:after:-translate-x-1/2 data-[active]:after:rounded-full data-[active]:after:bg-accent-foreground"
        >
          {children}
        </Button>
      </TooltipTrigger>
      <TooltipContent side="bottom">{tooltip}</TooltipContent>
    </Tooltip>
  );
}

function BubbleButton({
  onClick,
  isActive,
  children,
}: {
  onClick: () => void;
  isActive: boolean;
  children: React.ReactNode;
}) {
  return (
    <Button
      variant="ghost"
      size="icon-xs"
      onClick={onClick}
      data-active={isActive || undefined}
      className="relative h-7 w-7 data-[active]:bg-accent data-[active]:text-accent-foreground data-[active]:after:absolute data-[active]:after:bottom-0.5 data-[active]:after:left-1/2 data-[active]:after:h-0.5 data-[active]:after:w-2.5 data-[active]:after:-translate-x-1/2 data-[active]:after:rounded-full data-[active]:after:bg-accent-foreground"
    >
      {children}
    </Button>
  );
}

function HeadingDropdown({
  editor,
  level,
}: {
  editor: Editor;
  level: 1 | 2 | 3 | 4 | null;
}) {
  const label = level == null ? "Normal" : `H${level}`;
  const isActive = level != null;
  return (
    <DropdownMenu>
      <Tooltip>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <Button
              variant="ghost"
              size="xs"
              data-active={isActive || undefined}
              className="gap-0.5 text-xs font-medium data-[active]:bg-accent data-[active]:text-accent-foreground"
            >
              <ALargeSmall className="h-4 w-4" />
              <span className="ml-0.5 tabular-nums">{label}</span>
              <ChevronDown className="h-3 w-3 opacity-50" />
            </Button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent side="bottom">Heading level</TooltipContent>
      </Tooltip>
      <DropdownMenuContent align="start">
        <DropdownMenuItem
          data-active={level == null || undefined}
          className="text-xs data-[active]:bg-accent data-[active]:text-accent-foreground"
          onClick={() => editor.chain().focus().setParagraph().run()}
        >
          Normal text
        </DropdownMenuItem>
        <DropdownMenuItem
          data-active={level === 1 || undefined}
          className="text-base font-bold data-[active]:bg-accent data-[active]:text-accent-foreground"
          onClick={() => editor.chain().focus().toggleHeading({ level: 1 }).run()}
        >
          Heading 1
        </DropdownMenuItem>
        <DropdownMenuItem
          data-active={level === 2 || undefined}
          className="text-sm font-semibold data-[active]:bg-accent data-[active]:text-accent-foreground"
          onClick={() => editor.chain().focus().toggleHeading({ level: 2 }).run()}
        >
          Heading 2
        </DropdownMenuItem>
        <DropdownMenuItem
          data-active={level === 3 || undefined}
          className="text-[13px] font-medium data-[active]:bg-accent data-[active]:text-accent-foreground"
          onClick={() => editor.chain().focus().toggleHeading({ level: 3 }).run()}
        >
          Heading 3
        </DropdownMenuItem>
        <DropdownMenuItem
          data-active={level === 4 || undefined}
          className="text-xs font-medium data-[active]:bg-accent data-[active]:text-accent-foreground"
          onClick={() => editor.chain().focus().toggleHeading({ level: 4 }).run()}
        >
          Heading 4
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function HighlightDropdown({
  editor,
  isActive,
  currentColor,
  size = "toolbar",
}: {
  editor: Editor;
  isActive: boolean;
  currentColor: HighlightColor | null;
  size?: "toolbar" | "bubble";
}) {
  const apply = (color: HighlightColor) => {
    editor.chain().focus().setHighlight({ color }).run();
  };
  const remove = () => {
    editor.chain().focus().unsetHighlight().run();
  };
  const trigger =
    size === "toolbar" ? (
      <Tooltip>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <Button
              variant="ghost"
              size="icon-xs"
              data-active={isActive || undefined}
              className="data-[active]:bg-accent data-[active]:text-accent-foreground"
            >
              <Highlighter />
            </Button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent side="bottom">Highlight</TooltipContent>
      </Tooltip>
    ) : (
      <DropdownMenuTrigger asChild>
        <Button
          variant="ghost"
          size="icon-xs"
          data-active={isActive || undefined}
          className="h-7 w-7 data-[active]:bg-accent data-[active]:text-accent-foreground"
        >
          <Highlighter className="h-3.5 w-3.5" />
        </Button>
      </DropdownMenuTrigger>
    );
  return (
    <DropdownMenu>
      {trigger}
      <DropdownMenuContent align="start" portal={size === "toolbar"} className="min-w-0 p-1">
        <div className="flex items-center gap-1 px-1 py-0.5">
          {HIGHLIGHT_COLORS.map((c) => (
            <button
              key={c.value}
              type="button"
              onClick={() => apply(c.value)}
              aria-label={`Highlight ${c.name.toLowerCase()}`}
              data-active={currentColor === c.value || undefined}
              className="size-5 rounded-full border border-border ring-offset-1 ring-offset-popover transition-shadow hover:ring-2 hover:ring-ring data-[active]:ring-2 data-[active]:ring-ring"
              style={{ background: c.value }}
            />
          ))}
          <div className="mx-0.5 h-5 w-px bg-border" />
          <button
            type="button"
            onClick={remove}
            aria-label="Remove highlight"
            className="flex size-5 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-accent-foreground"
          >
            <Eraser className="h-3 w-3" />
          </button>
        </div>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** Debounced autosave delay after the last keystroke. */
const NOTE_SAVE_DEBOUNCE_MS = 1000;
/** Backoff schedule for retrying a save that failed (e.g. a transient DB lock). */
const NOTE_SAVE_RETRY_BASE_MS = 500;
const NOTE_SAVE_RETRY_MAX_MS = 15000;
const NOTE_SAVE_MAX_RETRIES = 5;
/**
 * Cap on how many times `closeWhenIdle` re-arms while a save is still pending.
 * A save that never resolves (neither succeeds nor rejects) must not pin the
 * note-editing window — and therefore suppress sync-down — indefinitely.
 */
const NOTE_CLOSE_MAX_REARMS = 10;
/**
 * How long after the last keystroke the note-editing window stays open once the
 * pending save has flushed. Long enough that a normal typing pause never lets a
 * sync-down land mid-sentence, short enough that a note left open and idle (the
 * editor autofocuses on open) still receives remote edits.
 */
const NOTE_EDIT_WINDOW_IDLE_MS = 3000;

export function NoteEditor({
  sessionId,
  refreshKey,
  onSeekTime,
}: {
  sessionId: string;
  refreshKey?: number;
  onSeekTime?: (seconds: number) => void;
}) {
  // Two baselines, deliberately: `lastSavedContent` is the DB-space string we
  // last read or wrote (compared against the stored row to detect a remote
  // edit), while `docBaseline` is the editor's own serialization of that same
  // document (compared against getHTML() to detect a LOCAL edit). They differ
  // whenever TipTap normalizes — an empty note is "" on disk and "<p></p>" in
  // the editor — and conflating them yields either phantom conflict toasts or a
  // permanently "dirty" editor that never accepts a remote update.
  const lastSavedContent = useRef<string>("");
  const docBaseline = useRef<string>("");
  const saveTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const windowTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // `useEditor` builds its options once per mount, so the `onUpdate` closure
  // would capture the FIRST sessionId. NoteDetailView swaps `sessionId` without
  // remounting, so the save path reads the live id from a ref instead — a save
  // must never land on the previously-open note.
  const sessionIdRef = useRef(sessionId);
  sessionIdRef.current = sessionId;
  // Highest `remoteNoteUpdate.seq` this editor has accounted for. Seeded at load
  // so a publication that predates the content we just read can never be applied
  // back over it.
  const appliedRemoteSeq = useRef(0);
  // Monotonic load generation, bumped every time editor content is replaced
  // programmatically (load, A1c sync-down). An autosave that was issued for one
  // (session, generation) and resolves after a newer load must NOT write its
  // stale HTML back into the in-memory baselines — that would corrupt the
  // baselines the load just set and reject legitimate sync-downs afterwards.
  const loadGenRef = useRef(0);
  // Live editor handle for the unmount flush and the failed-save backoff retry,
  // both of which run outside the render/`useEditor` closure.
  const editorRef = useRef<Editor | null>(null);
  // Failed-save state: true once a save has actually rejected, so `closeWhenIdle`
  // can release the editing window instead of pinning it on a write that won't
  // land. Cleared on the next successful save.
  const saveFailedRef = useRef(false);
  const saveRetryRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const saveRetryCountRef = useRef(0);

  const setNoteEditingSessionId = useAppStore((s) => s.setNoteEditingSessionId);
  const remoteNoteUpdate = useAppStore((s) => s.remoteNoteUpdate);

  /**
   * Save invariants (sync-UX A1). `notes.content` is ONE last-writer-wins CRDT
   * cell, so every write we make wins over whatever a peer wrote before it:
   *
   *  (a) Never write unchanged content. A blur or a debounce tick that carries
   *      the same HTML we loaded would re-assert our copy over a remote edit
   *      that landed in between — silent data loss with no user action at all.
   *  (b) When we DO write and the stored row moved underneath us, LWW still
   *      proceeds (v1 has no merge UI) but it is made VISIBLE: logged + toasted,
   *      so the user knows another device's version was superseded. A row that
   *      vanished is a DIFFERENT event from a row that changed — the save
   *      resurrects a note a peer deleted, which is correct LWW but must be
   *      described accurately.
   */
  const persistIfChanged = useCallback(async (html: string): Promise<boolean> => {
    const id = sessionIdRef.current;
    const gen = loadGenRef.current;
    // A LOCAL writer of this note (the AI `save_to_notes` tool, the chat's
    // save-to-notes, the citation conversion) announces itself by bumping
    // `noteRefreshCounter` (the sync path never does — D4). Captured here so we can
    // tell a same-machine local write apart from a genuine peer edit below.
    const localRefreshAtStart = useAppStore.getState().noteRefreshCounter;
    if (html === docBaseline.current) return false; // (a)
    // The editor has moved on (session switched, or content reloaded) since this
    // save was issued: the write to `id` is still correct for `id`, but its
    // baselines/conflict-toast belong to a document that is no longer on screen.
    const editorMovedOn = () =>
      sessionIdRef.current !== id || loadGenRef.current !== gen;
    try {
      const stored = await getNote(id);
      // A local writer touched this note while our save was in flight: the DB now holds
      // THEIR content (e.g. an AI summary) and a reload is pending to show it. Our `html`
      // is stale — writing it would clobber the local write, and it is NOT a peer edit, so
      // the "another device" toast would also be wrong. Yield: skip the write and the toast.
      // Session-scoped: only yield when the local write targeted THIS note — a
      // save-to-notes on a different session must not make this editor drop its text.
      const localRefreshStore = useAppStore.getState();
      if (
        localRefreshStore.noteRefreshCounter !== localRefreshAtStart &&
        localRefreshStore.noteRefreshSessionId === id
      ) {
        log.info(
          `note ${id}: a local write superseded this in-flight save; yielding to the reload (no clobber)`,
          "note-editor",
        );
        return false;
      }
      if (!editorMovedOn()) {
        if (stored === null && lastSavedContent.current !== "") {
          // (b, delete variant) — the row we loaded is gone; this save recreates it.
          // (A note that never existed loads as "" and takes neither branch.)
          log.warn(
            `note ${id}: row deleted on another device since load; local version resurrects it (LWW)`,
            "note-editor",
          );
          toast.info(
            "Note was deleted on another device — your version was restored",
          );
        } else if ((stored?.content ?? "") !== lastSavedContent.current) {
          // (b) — remote edit landed since we loaded this note.
          log.warn(
            `note ${id}: remote edit landed since load; local version wins (LWW)`,
            "note-editor",
          );
          toast.info("Note updated on another device — your version was saved");
        }
      }
      await saveNote(id, html);
      // Save landed — clear any failure/backoff state for this editor.
      saveFailedRef.current = false;
      saveRetryCountRef.current = 0;
      if (saveRetryRef.current) {
        clearTimeout(saveRetryRef.current);
        saveRetryRef.current = null;
      }
      // Only advance the in-memory baselines if they still describe THIS save's
      // document; otherwise a newer load already set them and we must not clobber.
      if (!editorMovedOn()) {
        lastSavedContent.current = html;
        docBaseline.current = html;
      }
      return true;
    } catch (e) {
      // The save was lost — surface it (toast + file log) instead of swallowing,
      // keep the content dirty, and schedule a bounded backoff retry so a
      // transient lock does not silently wedge the editor.
      const detail = e instanceof Error ? e.message : String(e);
      log.error(`note ${id}: failed to save: ${detail}`, "note-editor");
      toast.error(`Couldn't save note: ${detail}`, { id: `note-save-${id}` });
      saveFailedRef.current = true;
      if (
        !saveRetryRef.current &&
        saveRetryCountRef.current < NOTE_SAVE_MAX_RETRIES
      ) {
        const delay = Math.min(
          NOTE_SAVE_RETRY_BASE_MS * 2 ** saveRetryCountRef.current,
          NOTE_SAVE_RETRY_MAX_MS,
        );
        saveRetryRef.current = setTimeout(() => {
          saveRetryRef.current = null;
          saveRetryCountRef.current += 1;
          const ed = editorRef.current;
          if (ed && !ed.isDestroyed) void persistIfChanged(ed.getHTML());
        }, delay);
      }
      return false;
    }
  }, []);

  const editor: Editor | null = useEditor({
    autofocus: "end",
    extensions: [
      StarterKit,
      Markdown,
      Placeholder.configure({
        placeholder: "Write your notes here...",
      }),
      TaskList,
      TaskItem.configure({ nested: true }),
      Highlight.configure({ multicolor: true }),
      Underline,
      Link.configure({
        openOnClick: false,
        autolink: true,
        HTMLAttributes: { class: "text-primary underline" },
      }),
      Typography,
      SegmentReference,
    ],
    content: "",
    editorProps: {
      attributes: {
        class:
          "tiptap-editor max-w-none focus:outline-none min-h-[200px] px-4 py-3 text-sm",
      },
      clipboardTextSerializer: (slice: Slice) => {
        const nodes: JSONContent[] = [];
        slice.content.forEach((node) => nodes.push(node.toJSON()));
        const manager = editor?.storage.markdown?.manager;
        if (!manager) return slice.content.textBetween(0, slice.content.size);
        return manager.serialize({ type: "doc", content: nodes });
      },
      // When the clipboard payload is plain-text markdown (no rich HTML
      // alternative) and contains a code fence, parse it as markdown so
      // ```code blocks``` and other markdown survive the paste.
      handlePaste: (_view, event) => {
        const text = event.clipboardData?.getData("text/plain");
        if (!text || !text.includes("```")) return false;
        if (event.clipboardData?.getData("text/html")) return false;
        if (!editor) return false;
        editor.commands.insertContent(text, { contentType: "markdown" });
        return true;
      },
    },
    onUpdate: ({ editor }) => {
      // A keystroke opens the store-visible note-editing window (A1b): while it
      // is open the sync refresh will not publish note content for this session,
      // so an in-flight edit can't be replaced under the caret.
      const id = sessionIdRef.current;
      const store = useAppStore.getState();
      if (store.noteEditingSessionId !== id) store.setNoteEditingSessionId(id);

      if (saveTimeoutRef.current) clearTimeout(saveTimeoutRef.current);
      saveTimeoutRef.current = setTimeout(() => {
        saveTimeoutRef.current = null;
        void persistIfChanged(editor.getHTML());
      }, NOTE_SAVE_DEBOUNCE_MS);

      // Close the window once typing has gone idle AND the pending save flushed.
      // Re-armed on every keystroke; blur closes it immediately.
      if (windowTimeoutRef.current) clearTimeout(windowTimeoutRef.current);
      let rearms = 0;
      const closeWhenIdle = () => {
        if (editor.isDestroyed) return;
        if (
          editor.getHTML() !== docBaseline.current &&
          // A save that has actually FAILED must not pin the window: the content
          // stays dirty and the backoff retry keeps trying, but leaving the
          // window open would suppress sync-down for this session forever.
          !saveFailedRef.current &&
          rearms < NOTE_CLOSE_MAX_REARMS
        ) {
          // Save still in flight — keep the window open rather than exposing
          // unsaved text to a sync-down.
          rearms += 1;
          windowTimeoutRef.current = setTimeout(
            closeWhenIdle,
            NOTE_SAVE_DEBOUNCE_MS,
          );
          return;
        }
        windowTimeoutRef.current = null;
        const s = useAppStore.getState();
        if (s.noteEditingSessionId === sessionIdRef.current) {
          s.setNoteEditingSessionId(null);
        }
      };
      windowTimeoutRef.current = setTimeout(
        closeWhenIdle,
        NOTE_EDIT_WINDOW_IDLE_MS,
      );
    },
  });

  // Keep a live handle for callbacks that run outside this render (unmount
  // flush, failed-save retry) without adding `editor` to their dependency lists.
  editorRef.current = editor;

  /**
   * The single place editor content is replaced programmatically (load, and the
   * A1c sync-down). `emitUpdate: false` because a programmatic replacement is
   * not a user edit: letting it emit would open the editing window and schedule
   * a save that writes the just-loaded content straight back.
   */
  const applyContent = useCallback(
    (content: string) => {
      if (!editor) return;
      if (saveTimeoutRef.current) {
        clearTimeout(saveTimeoutRef.current);
        saveTimeoutRef.current = null;
      }
      // Replacing the document invalidates any autosave already in flight: bump
      // the generation so a late save continuation cannot write its stale HTML
      // back over the baselines we set here.
      loadGenRef.current += 1;
      editor.commands.setContent(content, { emitUpdate: false });
      lastSavedContent.current = content;
      docBaseline.current = editor.getHTML();
    },
    [editor],
  );

  // Load note content on mount and when refreshKey changes. `refreshKey` is a
  // LOCAL signal (AI tool wrote the note, etc.) — the sync path never bumps it.
  useEffect(() => {
    // Cancellation token: a load for a superseded session/refreshKey must not
    // apply its content after a newer load has started (fast session switch).
    let cancelled = false;
    async function load() {
      if (!editor) return;
      // Anything published before this read is, by definition, not newer than
      // what we are about to read.
      appliedRemoteSeq.current =
        useAppStore.getState().remoteNoteUpdate?.seq ?? 0;
      const note = await getNote(sessionId);
      if (cancelled) return;
      let content = note?.content ?? "";
      // Convert [[seg:ID]] text citations to <span data-segment-ref> nodes
      if (content.includes("[[seg:")) {
        const segments = await getSessionSegments(sessionId);
        if (cancelled) return;
        content = convertCitationsToSegmentRefs(content, segments);
        // Persist the converted content so future loads don't need re-conversion
        if (note && content !== note.content) {
          await saveNote(sessionId, content);
          if (cancelled) return;
        }
      }
      applyContent(content);
    }
    load();
    return () => {
      cancelled = true;
    };
  }, [sessionId, editor, refreshKey, applyContent]);

  // Blur = end of the edit window: flush a real save (no-op when unchanged) and
  // release the guard so any refresh it suppressed drains (B5).
  const handleBlur = useCallback(async () => {
    if (!editor) return;
    if (saveTimeoutRef.current) {
      clearTimeout(saveTimeoutRef.current);
      saveTimeoutRef.current = null;
    }
    if (windowTimeoutRef.current) {
      clearTimeout(windowTimeoutRef.current);
      windowTimeoutRef.current = null;
    }
    await persistIfChanged(editor.getHTML());
    if (useAppStore.getState().noteEditingSessionId === sessionIdRef.current) {
      setNoteEditingSessionId(null);
    }
  }, [editor, persistIfChanged, setNoteEditingSessionId]);

  useEffect(() => {
    if (!editor) return;
    editor.on("blur", handleBlur);
    return () => {
      editor.off("blur", handleBlur);
    };
  }, [editor, handleBlur]);

  /**
   * A1c: apply a note row the sync refresh published for THIS session, in place,
   * without going through `refreshKey`/`noteRefreshCounter` (D4 normative: sync
   * never bumps that counter). Guarded three ways — the store window, the
   * publication sequence, and a direct unsaved-content check — because the only
   * unrecoverable outcome here is replacing text the user has typed.
   *
   * Deliberately silent to the user: the local-wins direction toasts because it
   * SUPERSEDES someone's work, while this direction only shows content that is
   * already saved — toasting every inbound update would be constant noise during
   * normal two-device use. It is logged so the direction stays traceable.
   */
  useEffect(() => {
    if (!editor || !remoteNoteUpdate) return;
    if (remoteNoteUpdate.sessionId !== sessionId) return;
    if (remoteNoteUpdate.seq <= appliedRemoteSeq.current) return;
    if (useAppStore.getState().noteEditingSessionId === sessionId) return;
    if (editor.getHTML() !== docBaseline.current) return; // unsaved local edit
    appliedRemoteSeq.current = remoteNoteUpdate.seq;
    if (remoteNoteUpdate.content === lastSavedContent.current) return;
    log.info(`remote update applied to open note ${sessionId}`, "note-editor");
    applyContent(remoteNoteUpdate.content);
  }, [editor, sessionId, remoteNoteUpdate, applyContent]);

  // Cleanup on unmount: flush any pending text, drop pending timers, and release
  // the editing window if this editor still owns it (a stuck window would
  // suppress sync-down forever).
  useEffect(() => {
    return () => {
      // Flush unsaved text before the editor is torn down so an unmount
      // mid-debounce (fast session switch, view close) doesn't drop it.
      const ed = editorRef.current;
      if (ed && !ed.isDestroyed) {
        const html = ed.getHTML();
        if (html !== docBaseline.current) void persistIfChanged(html);
      }
      if (saveTimeoutRef.current) clearTimeout(saveTimeoutRef.current);
      if (windowTimeoutRef.current) clearTimeout(windowTimeoutRef.current);
      if (saveRetryRef.current) clearTimeout(saveRetryRef.current);
      const store = useAppStore.getState();
      if (store.noteEditingSessionId === sessionIdRef.current) {
        store.setNoteEditingSessionId(null);
      }
    };
  }, [persistIfChanged]);

  // Listen for segment reference insertion events
  useEffect(() => {
    const handleInsertRef = (e: Event) => {
      if (!editor) return;
      const detail = (e as CustomEvent).detail;
      editor
        .chain()
        .focus()
        .insertContent({
          type: "segmentReference",
          attrs: {
            segmentId: detail.segmentId,
            timestamp: detail.timestamp,
            offsetSeconds: detail.offsetSeconds,
          },
        })
        .insertContent(" ")
        .run();
    };
    window.addEventListener("yapstack:insert-segment-ref", handleInsertRef);
    return () => window.removeEventListener("yapstack:insert-segment-ref", handleInsertRef);
  }, [editor]);

  // Listen for seek events from segment reference clicks
  useEffect(() => {
    if (!onSeekTime) return;
    const handleSeek = (e: Event) => {
      const detail = (e as CustomEvent).detail;
      onSeekTime(detail.offsetSeconds);
    };
    window.addEventListener("yapstack:seek-segment", handleSeek);
    return () => window.removeEventListener("yapstack:seek-segment", handleSeek);
  }, [onSeekTime]);

  const handleSetLink = useCallback(() => {
    if (!editor) return;
    const previousUrl = editor.getAttributes("link").href ?? "";
    const url = window.prompt("URL", previousUrl);
    if (url === null) return;
    if (url === "") {
      editor.chain().focus().extendMarkRange("link").unsetLink().run();
    } else {
      editor.chain().focus().extendMarkRange("link").setLink({ href: url }).run();
    }
  }, [editor]);

  // `editor.isActive(...)` is not reactive in React; this hook re-renders on transactions.
  const state = useEditorState({
    editor,
    selector: ({ editor: e }) =>
      e
        ? {
            isBold: e.isActive("bold"),
            isItalic: e.isActive("italic"),
            isUnderline: e.isActive("underline"),
            isStrike: e.isActive("strike"),
            isCode: e.isActive("code"),
            isCodeBlock: e.isActive("codeBlock"),
            isHighlight: e.isActive("highlight"),
            isLink: e.isActive("link"),
            isBulletList: e.isActive("bulletList"),
            isOrderedList: e.isActive("orderedList"),
            isTaskList: e.isActive("taskList"),
            isBlockquote: e.isActive("blockquote"),
            headingLevel: (e.getAttributes("heading").level ?? null) as
              | 1 | 2 | 3 | 4 | null,
            highlightColor:
              (e.getAttributes("highlight").color as HighlightColor | undefined) ?? null,
          }
        : null,
  });

  if (!editor || !state) return null;

  return (
    <div className="flex flex-1 flex-col min-h-0">
      <div className="flex items-center gap-0.5 border-b bg-muted/30 px-3 py-1.5">
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleBold().run()}
          isActive={state.isBold}
          tooltip="Bold"
        >
          <Bold />
        </ToolbarButton>
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleItalic().run()}
          isActive={state.isItalic}
          tooltip="Italic"
        >
          <Italic />
        </ToolbarButton>
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleUnderline().run()}
          isActive={state.isUnderline}
          tooltip="Underline"
        >
          <UnderlineIcon />
        </ToolbarButton>
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleStrike().run()}
          isActive={state.isStrike}
          tooltip="Strikethrough"
        >
          <Strikethrough />
        </ToolbarButton>
        <ToolbarButton onClick={handleSetLink} isActive={state.isLink} tooltip="Link">
          <LinkIcon />
        </ToolbarButton>
        <div className="mx-1 h-4 w-px bg-border" />
        <HeadingDropdown editor={editor} level={state.headingLevel} />
        <div className="mx-1 h-4 w-px bg-border" />
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleBulletList().run()}
          isActive={state.isBulletList}
          tooltip="Bullet List"
        >
          <List />
        </ToolbarButton>
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleOrderedList().run()}
          isActive={state.isOrderedList}
          tooltip="Ordered List"
        >
          <ListOrdered />
        </ToolbarButton>
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleTaskList().run()}
          isActive={state.isTaskList}
          tooltip="Checklist"
        >
          <ListChecks />
        </ToolbarButton>
        <div className="mx-1 h-4 w-px bg-border" />
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleBlockquote().run()}
          isActive={state.isBlockquote}
          tooltip="Blockquote"
        >
          <Quote />
        </ToolbarButton>
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleCode().run()}
          isActive={state.isCode}
          tooltip="Inline code"
        >
          <Code />
        </ToolbarButton>
        <ToolbarButton
          onClick={() => editor.chain().focus().toggleCodeBlock().run()}
          isActive={state.isCodeBlock}
          tooltip="Code block"
        >
          <SquareCode />
        </ToolbarButton>
        <HighlightDropdown
          editor={editor}
          isActive={state.isHighlight}
          currentColor={state.highlightColor}
          size="toolbar"
        />
        <div className="flex-1" />
      </div>
      <BubbleMenu
        editor={editor}
        shouldShow={({ editor: e, state: s }) => {
          const { from, to } = s.selection;
          if (from === to) return false;
          if (e.isActive("codeBlock")) return false;
          let hasSegRef = false;
          s.doc.nodesBetween(from, to, (node) => {
            if (node.type.name === "segmentReference") hasSegRef = true;
            return !hasSegRef;
          });
          return !hasSegRef;
        }}
        options={{
          placement: "top",
          offset: { mainAxis: 8 },
          // Bound flip/shift to the editor's contenteditable so the bubble can't
          // escape into the static toolbar above or the FloatingChatBar below.
          flip: {
            boundary: editor.view.dom,
            fallbackPlacements: ["bottom", "top"],
            padding: 8,
          },
          shift: { boundary: editor.view.dom, padding: 8 },
        }}
      >
        <div className="z-50 flex items-center gap-0.5 rounded-lg border bg-popover p-1 shadow-lg">
          <BubbleButton
            onClick={() => editor.chain().focus().toggleBold().run()}
            isActive={state.isBold}
          >
            <Bold className="h-3.5 w-3.5" />
          </BubbleButton>
          <BubbleButton
            onClick={() => editor.chain().focus().toggleItalic().run()}
            isActive={state.isItalic}
          >
            <Italic className="h-3.5 w-3.5" />
          </BubbleButton>
          <BubbleButton
            onClick={() => editor.chain().focus().toggleUnderline().run()}
            isActive={state.isUnderline}
          >
            <UnderlineIcon className="h-3.5 w-3.5" />
          </BubbleButton>
          <BubbleButton
            onClick={() => editor.chain().focus().toggleStrike().run()}
            isActive={state.isStrike}
          >
            <Strikethrough className="h-3.5 w-3.5" />
          </BubbleButton>
          <BubbleButton
            onClick={() => editor.chain().focus().toggleCode().run()}
            isActive={state.isCode}
          >
            <Code className="h-3.5 w-3.5" />
          </BubbleButton>
          <HighlightDropdown
            editor={editor}
            isActive={state.isHighlight}
            currentColor={state.highlightColor}
            size="bubble"
          />
          <BubbleButton onClick={handleSetLink} isActive={state.isLink}>
            <LinkIcon className="h-3.5 w-3.5" />
          </BubbleButton>
        </div>
      </BubbleMenu>
      <div className="flex-1 overflow-auto pb-16 select-text">
        <EditorContent editor={editor} />
      </div>
    </div>
  );
}
