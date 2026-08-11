import type { ChatCompletionTool } from "openai/resources/chat/completions";
import {
  updateSessionTitle,
  saveNote,
  createNoteVersion,
  togglePin,
  getSession,
  getTagByName,
  createTag,
  addSessionTag,
  removeSessionTag,
  getSessionTagRows,
  addSessionToFolder as dbAddSessionToFolder,
  removeSessionFromFolder as dbRemoveSessionFromFolder,
  listFolders,
  searchSegments,
  searchNotes,
  searchSessionsByTitle,
  searchDictations,
  getSessionsByIds,
  updateSegmentText,
  listAllSessionFolders,
  getNote,
  getSessionSegments,
} from "./db";
import type { DbNote, DbSegment, DbFolder } from "./db";
import {
  markdownToBasicHtml,
  formatSegmentSpeaker,
  isVisibleSegment,
} from "./ai";
import { formatTime, stripHtml } from "./utils";
import { findBranchConflicts, getFolderPath } from "./folder-tree";
import { useAppStore } from "@/stores/appStore";

// --- Core types ---

export interface ToolCallResult {
  id: string;
  name: string;
  arguments: Record<string, unknown>;
}

export interface SegmentRef {
  type: "segment";
  segmentId: string;
  sessionId: string;
  audioOffsetSeconds?: number;
}

export interface SessionRef {
  type: "session";
  sessionId: string;
  title?: string;
}

/**
 * Persisted shape of a Tool call on a Chat assistant row, v14+.
 *
 * Stored as JSON in `chat_messages.tool_calls`. `id` and `arguments` are
 * the load-bearing fields for replay — they reconstruct the OpenAI
 * `assistant.tool_calls[].function.arguments` and `tool_call_id` exactly
 * as the model emitted them. The remaining fields are convenience for
 * the renderer so it doesn't need to traverse multiple rows.
 *
 * `running` is intentionally not part of the status union — only
 * finalized calls are persisted.
 *
 * Pre-v14 rows may carry an older flat shape that lacks `id`/`arguments`;
 * the assembler treats those as legacy and skips tool replay.
 */
export interface PersistedToolCall {
  /** OpenAI `tool_call_id`. Matches the `tool_call_id` of the paired tool row. */
  id: string;
  name: string;
  /** JSON-stringified arguments as the model emitted them. */
  arguments: string;
  label: string;
  status: "done" | "error";
  detail?: string;
  observation?: ToolObservation;
  /**
   * True after the user reverts this call via the Undo window. The chip
   * stays visible (rendered with strike-through) and the call is still
   * replayed to the LLM, but the paired tool row's content is rewritten
   * to "(undone by user)" so the model understands the action no longer
   * stands.
   */
  undone?: boolean;
}

/**
 * Structured payload produced by a Tool. `summary` is required and shown
 * in compact UI; the rest is optional metadata for the renderer and for
 * future provenance/citation propagation. Tools that don't set
 * `observation` directly get one synthesized from their `detail`/`result`
 * strings inside `executeTool`.
 */
export interface ToolObservation {
  summary: string;
  evidence?: string;
  affectedIds?: string[];
  provenance?: Array<SegmentRef | SessionRef>;
}

export interface ExecutedTool {
  name: string;
  label: string;
  detail: string;
  toolCallId?: string;
  result?: string;
  /** Structured Tool result. Synthesized in `executeTool` if not set by the Tool. */
  observation?: ToolObservation;
  /**
   * Pre-state captured by mutating tools so the change can be reversed.
   * Leave undefined for read-only tools and for no-op paths in mutating
   * tools (e.g. pin_session when already pinned). The Chat orchestrator
   * filters Undo-window inclusion on `kind === "mutate" && undoData !== undefined`.
   */
  undoData?: unknown;
}

/**
 * Context for tools that mutate a single session's state. Carries the
 * pre-image fields (`currentTitle`, `currentNote`, `isPinned`, `tags`,
 * `folderIds`) that mutating tools snapshot into `undoData`.
 */
export interface SessionToolContext {
  scope: "session";
  sessionId: string;
  currentTitle: string;
  currentNote: DbNote | null;
  isPinned: boolean;
  segments?: DbSegment[];
  tags?: string[];
  folderNames?: string[];
  folderIds?: string[];
  /**
   * When defined, retrieval tools dispatched from this session-scoped chat
   * must still restrict their results to these session IDs. Currently
   * unused for single-session chats but reserved for future filtered views.
   */
  allowedSessionIds?: string[];
}

/**
 * Context for retrieval-only chats (folder / pinned / all). Carries no
 * single-session pre-image — only the allow-list that pins retrieval tools
 * to the chat's filter so the model can't reach outside the user's view.
 */
export interface RetrievalToolContext {
  scope: "retrieval";
  allowedSessionIds: string[];
}

/**
 * Discriminated by `scope`. Mutating tools must narrow via
 * `requireSessionContext(ctx)`; retrieval tools that work in both scopes
 * (e.g. `search_sessions`) can read the optional fields directly.
 */
export type ToolContext = SessionToolContext | RetrievalToolContext;

/**
 * Narrow a `ToolContext` to its session-scoped variant. Mutating tools call
 * this at the top of `execute` instead of fabricating values when the chat
 * is multi-session — a session-scoped tool dispatched in retrieval scope is
 * a wiring bug and should fail loudly.
 */
export function requireSessionContext(ctx: ToolContext): SessionToolContext {
  if (ctx.scope !== "session") {
    throw new Error(
      `Tool requires a session-scoped context (got scope='${ctx.scope}')`,
    );
  }
  return ctx;
}

// --- Modular tool definition ---

export type ToolEffect = "session-meta" | "notes" | "organization" | "transcript";

/**
 * Compile-time discriminator for tool intent.
 * - "read": pure retrieval. Never enters the Undo window, never triggers
 *   the "Session updated" toast or content-refresh callbacks.
 * - "mutate": writes session state. Must populate `undoData` so the
 *   change can be reversed within the Undo window.
 */
export type ToolKind = "read" | "mutate";

export interface ToolDefinition {
  kind: ToolKind;
  schema: ChatCompletionTool;
  execute: (
    args: Record<string, unknown>,
    ctx: ToolContext,
  ) => Promise<ExecutedTool | null>;
  undo: (undoData: unknown, ctx: { sessionId: string }) => Promise<void>;
  affects?: ToolEffect[];
}

// --- Citation conversion ---

const CITE_REGEX = /\[\[seg:([a-zA-Z0-9_-]+)\]\]/g;

/**
 * Replace [[seg:ID]] text citations with <span data-segment-ref> HTML nodes
 * that the Tiptap SegmentReference extension can render as interactive pills.
 */
export function convertCitationsToSegmentRefs(
  html: string,
  segments: DbSegment[],
): string {
  return html.replace(CITE_REGEX, (_match, segId: string) => {
    const segment = segments.find((s) => s.id === segId);
    const ts = segment
      ? formatTime(Math.max(0, segment.audio_offset_seconds))
      : segId.slice(0, 8);
    const offset = segment ? segment.audio_offset_seconds : 0;
    return `<span data-segment-ref="" data-segment-id="${segId}" data-timestamp="${ts}" data-offset="${offset}">${ts}</span>`;
  });
}

// --- Registry ---

const toolRegistry = new Map<string, ToolDefinition>();

export function registerTool(def: ToolDefinition): void {
  const name =
    def.schema.type === "function" ? def.schema.function.name : undefined;
  if (name) toolRegistry.set(name, def);
}

export function getRegisteredTools(): ChatCompletionTool[] {
  return Array.from(toolRegistry.values()).map((t) => t.schema);
}

export function getToolsById(toolIds: string[]): ChatCompletionTool[] {
  return toolIds
    .map((id) => toolRegistry.get(id))
    .filter((def): def is ToolDefinition => def !== undefined)
    .map((def) => def.schema);
}

export function getToolKind(toolName: string): ToolKind | undefined {
  return toolRegistry.get(toolName)?.kind;
}

export function getToolEffects(toolNames: string[]): Set<ToolEffect> {
  const effects = new Set<ToolEffect>();
  for (const name of toolNames) {
    const def = toolRegistry.get(name);
    if (def?.affects) for (const e of def.affects) effects.add(e);
  }
  return effects;
}

export async function executeTool(
  name: string,
  args: Record<string, unknown>,
  ctx: ToolContext,
): Promise<ExecutedTool | null> {
  const def = toolRegistry.get(name);
  if (!def) throw new Error(`Unknown tool: ${name}`);
  const result = await def.execute(args, ctx);
  if (result && !result.observation) {
    // Mirror legacy detail/result strings into the structured shape so
    // downstream consumers (UI renderer, persisted tool_calls column,
    // eval harness) can read one shape regardless of tool age.
    result.observation = {
      summary: result.detail,
      evidence: result.result,
    };
  }
  return result;
}

/**
 * Capture pre-state for a mutating Tool. The returned value goes into
 * `ExecutedTool.undoData` and is later passed to the Tool's `undo` handler.
 * Use this anywhere a mutating Tool reads state it's about to overwrite
 * (ctx fields, DB rows, any pre-image) so snapshot points are explicit
 * and greppable across the registry.
 */
export async function captureUndoSnapshot<T>(
  load: () => Promise<T> | T,
): Promise<T> {
  return await load();
}

export async function undoToolCalls(
  executed: ExecutedTool[],
  ctx: { sessionId: string },
): Promise<void> {
  for (const tool of [...executed].reverse()) {
    const def = toolRegistry.get(tool.name);
    if (def?.undo && tool.undoData !== undefined) {
      await def.undo(tool.undoData, ctx);
    }
  }
}

// --- Tool: update_title ---

registerTool({
  kind: "mutate",
  affects: ["session-meta"],
  schema: {
    type: "function",
    function: {
      name: "update_title",
      description:
        "Set the session title. Use a concise, descriptive title (max 80 chars) that captures the essence of the session.",
      parameters: {
        type: "object",
        properties: {
          title: {
            type: "string",
            description: "The new session title (max 80 characters)",
          },
        },
        required: ["title"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args, ctx) => {
    const sctx = requireSessionContext(ctx);
    const title = String(args.title).slice(0, 80);
    if (title === sctx.currentTitle) return null;
    const previousTitle = await captureUndoSnapshot(() => sctx.currentTitle);
    await updateSessionTitle(sctx.sessionId, title);
    // Adjacent to the write (no await between) so an open title edit yields
    // instead of misattributing this local rename as a peer edit. Sync never
    // bumps this (D4). pin_session and other non-title meta tools do NOT bump it.
    useAppStore.getState().incrementSessionMetaRefresh(sctx.sessionId);
    return {
      name: "update_title",
      label: "Title",
      detail: title,
      result: `Title updated from "${previousTitle}" to "${title}".`,
      undoData: previousTitle,
    };
  },
  undo: async (undoData, ctx) => {
    await updateSessionTitle(ctx.sessionId, String(undoData));
    useAppStore.getState().incrementSessionMetaRefresh(ctx.sessionId);
  },
});

// --- Tool: save_to_notes ---

/**
 * Substring replacement inside HTML that walks text nodes only.
 *
 * The naive approach — `note.replace(find, replace)` against the raw
 * HTML — risks matching text inside attributes (URLs, classnames) and
 * destroys formatting if the replacement contains HTML-significant
 * characters. By walking with `TreeWalker(SHOW_TEXT)`, we only touch
 * the text nodes the user actually sees, and the DOM serialiser
 * escapes the replacement string for us.
 *
 * Limitations: matches that span tag boundaries (e.g. `**bold word**`
 * where "word" is bolded but "bold " is not) won't match — the text
 * is split across two nodes. For inline word/phrase substitution this
 * is the right behaviour; for structural rewrites the model should
 * use `append` or `replace` instead.
 */
function replaceTextInHtml(
  html: string,
  find: string,
  replacement: string,
): { html: string; replacedCount: number } {
  if (!find) return { html, replacedCount: 0 };
  if (typeof DOMParser === "undefined") {
    // Tests / non-browser environments. Fall back to a string replace
    // bounded to text-looking regions: between `>` and `<`. Still
    // imperfect but unlikely to mangle attributes for the basic HTML
    // shapes the markdown converter produces.
    let count = 0;
    const out = html.replace(/>([^<]*)</g, (_match, text: string) => {
      const occurrences = text.split(find).length - 1;
      if (occurrences === 0) return _match;
      count += occurrences;
      return ">" + text.split(find).join(replacement) + "<";
    });
    return { html: out, replacedCount: count };
  }
  const doc = new DOMParser().parseFromString(html, "text/html");
  const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT);
  const updates: Text[] = [];
  let node: Node | null;
  while ((node = walker.nextNode())) {
    if (node.nodeValue && node.nodeValue.includes(find)) {
      updates.push(node as Text);
    }
  }
  let replacedCount = 0;
  for (const n of updates) {
    if (!n.nodeValue) continue;
    const occurrences = n.nodeValue.split(find).length - 1;
    n.nodeValue = n.nodeValue.split(find).join(replacement);
    replacedCount += occurrences;
  }
  return { html: doc.body.innerHTML, replacedCount };
}

registerTool({
  kind: "mutate",
  affects: ["notes"],
  schema: {
    type: "function",
    function: {
      name: "save_to_notes",
      description:
        `Edit the session note. Pick the mode that matches the user's intent — wrong choice can destroy hand-written content.

- "find_replace": Surgically replace a specific word, phrase, or sentence. Use whenever the user asks to "change", "fix", "edit", "update", or "rename" something inside their notes. Pass the exact text to find in 'find' and the new text in 'content'. The find string must be an exact substring of the current notes (visible in your prompt). Markdown does NOT render in the replacement — use plain text. Won't match across tag boundaries (e.g. across a heading break).
- "append": Add new content below the existing notes with a separator. Use when adding a summary, action items, or a follow-up section to existing notes.
- "prepend": Add new content above the existing notes with a separator. Use for a TL;DR or executive summary at the top.
- "replace": Overwrite ALL existing notes. Use ONLY when the notes are empty, or the user explicitly asks for a complete rewrite from scratch. NEVER use this to "update" or "change" something — that's find_replace or append.

If you're unsure between append and replace, choose append. The user's manual notes must be preserved.`,
      parameters: {
        type: "object",
        properties: {
          mode: {
            type: "string",
            enum: ["replace", "append", "prepend", "find_replace"],
          },
          content: {
            type: "string",
            description:
              "For replace/append/prepend: markdown content. For find_replace: plain-text replacement (markdown won't render).",
          },
          find: {
            type: "string",
            description:
              "Required for find_replace. Exact substring of the current notes to match.",
          },
        },
        required: ["mode", "content"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args, ctx) => {
    const sctx = requireSessionContext(ctx);
    const mode = String(args.mode) as
      | "replace"
      | "append"
      | "prepend"
      | "find_replace";
    const content = String(args.content);
    const find = args.find != null ? String(args.find) : "";

    const previousContent = await captureUndoSnapshot(
      () => sctx.currentNote?.content ?? null,
    );

    // Snapshot to note_versions before every save, regardless of mode.
    // The Undo window is short-lived (10s); note_versions is the durable
    // recovery path. createNoteVersion is a no-op when the user has no
    // current note (nothing to snapshot).
    if (sctx.currentNote?.id && previousContent) {
      await createNoteVersion(sctx.currentNote.id, previousContent);
    }

    let mergedHtml: string;
    let detail: string;
    let resultText: string;

    if (mode === "find_replace") {
      if (!find.trim()) {
        return {
          name: "save_to_notes",
          label: "Notes",
          detail: "find_replace requires a 'find' argument",
          result: "Error: find_replace requires a non-empty 'find' argument.",
        };
      }
      if (!previousContent || previousContent === "<p></p>") {
        return {
          name: "save_to_notes",
          label: "Notes",
          detail: "Notes are empty",
          result:
            "Error: notes are empty; nothing to find_replace. Use 'replace' or 'append' instead.",
        };
      }
      const { html: nextHtml, replacedCount } = replaceTextInHtml(
        previousContent,
        find,
        content,
      );
      if (replacedCount === 0) {
        return {
          name: "save_to_notes",
          label: "Notes",
          detail: `Could not find "${truncate(find, 40)}"`,
          result: `Error: could not find "${find}" in the current notes. Re-read the notes (visible in the system prompt) and try again with an exact match. Note that find_replace cannot match text spanning multiple paragraphs or formatting boundaries.`,
        };
      }
      mergedHtml = nextHtml;
      detail = `Replaced "${truncate(find, 24)}" → "${truncate(content, 24)}" (${replacedCount}×)`;
      resultText = `Replaced ${replacedCount} occurrence${replacedCount === 1 ? "" : "s"} of "${find}" with "${content}".`;
    } else {
      let html = markdownToBasicHtml(content);
      if (sctx.segments?.length) {
        html = convertCitationsToSegmentRefs(html, sctx.segments);
      }
      const hasExisting =
        !!previousContent && previousContent !== "<p></p>";

      if (mode === "append" && hasExisting) {
        mergedHtml = previousContent + "<hr>" + html;
        detail = "Appended to notes";
        resultText = "Appended to existing notes.";
      } else if (mode === "prepend" && hasExisting) {
        mergedHtml = html + "<hr>" + previousContent;
        detail = "Prepended to notes";
        resultText = "Prepended above existing notes.";
      } else {
        // replace, OR append/prepend against empty notes (same effect).
        mergedHtml = html;
        detail = hasExisting ? "Replaced all notes" : "Notes saved";
        resultText = hasExisting
          ? "Replaced all existing notes (previous content snapshotted to note_versions)."
          : "Notes saved.";
      }
    }

    await saveNote(sctx.sessionId, mergedHtml);
    // Announce the local write ADJACENT to it (no await between) so an open note
    // editor's in-flight save yields to it instead of clobbering — the atomic
    // signal a post-hoc refresh in handleToolsExecuted could race. Sync never
    // bumps this (D4), so it stays a local-vs-remote discriminator.
    useAppStore.getState().incrementNoteRefresh(sctx.sessionId);

    const wordCount = content.split(/\s+/).filter(Boolean).length;
    return {
      name: "save_to_notes",
      label: "Notes",
      detail,
      result: `${resultText} (${wordCount} words)`,
      undoData: previousContent,
    };
  },
  undo: async (undoData, ctx) => {
    if (undoData === null) {
      // No previous note — save empty to effectively clear
      await saveNote(ctx.sessionId, "<p></p>");
    } else {
      await saveNote(ctx.sessionId, String(undoData));
    }
    useAppStore.getState().incrementNoteRefresh(ctx.sessionId);
  },
});

function truncate(s: string, max: number): string {
  return s.length <= max ? s : s.slice(0, max - 1) + "…";
}

// --- Tool: pin_session ---

registerTool({
  kind: "mutate",
  affects: ["session-meta"],
  schema: {
    type: "function",
    function: {
      name: "pin_session",
      description: "Pin or unpin the session.",
      parameters: {
        type: "object",
        properties: {
          pinned: {
            type: "boolean",
            description: "Whether to pin (true) or unpin (false) the session",
          },
        },
        required: ["pinned"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args, ctx) => {
    const sctx = requireSessionContext(ctx);
    const wantPinned = Boolean(args.pinned);
    const wasPinned = await captureUndoSnapshot(() => sctx.isPinned);
    const changed = wantPinned !== wasPinned;

    if (changed) {
      await togglePin(sctx.sessionId);
    }

    return {
      name: "pin_session",
      label: "Pinned",
      detail: wantPinned ? "Session pinned" : "Session unpinned",
      result: changed
        ? wantPinned
          ? "Session pinned successfully."
          : "Session unpinned successfully."
        : `Session was already ${wantPinned ? "pinned" : "unpinned"}.`,
      undoData: changed ? wasPinned : undefined,
    };
  },
  undo: async (undoData, ctx) => {
    const wasPinned = Boolean(undoData);
    const session = await getSession(ctx.sessionId);
    const currentlyPinned = session ? session.is_pinned === 1 : false;
    if (wasPinned !== currentlyPinned) {
      await togglePin(ctx.sessionId);
    }
  },
});

// --- Tool: tag_session ---

registerTool({
  kind: "mutate",
  affects: ["organization"],
  schema: {
    type: "function",
    function: {
      name: "tag_session",
      description:
        "Add or remove tags from the current session. Creates new tags automatically if they don't exist yet. Pass an empty array for either side when you don't want to add or remove anything.",
      parameters: {
        type: "object",
        properties: {
          add: {
            type: "array",
            items: { type: "string" },
            description: "Tag names to add. Pass [] for no additions.",
          },
          remove: {
            type: "array",
            items: { type: "string" },
            description: "Tag names to remove. Pass [] for no removals.",
          },
        },
        required: ["add", "remove"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args, ctx) => {
    const sctx = requireSessionContext(ctx);
    const toAdd = (args.add as string[]) ?? [];
    const toRemove = (args.remove as string[]) ?? [];

    // Snapshot existing rows so undo only inverts real deltas. Without this,
    // INSERT-OR-IGNORE adds and unconditional removes would record phantom
    // deltas that, on undo, drop pre-existing manual tags or re-add tags
    // that were never on the session.
    const beforeRows = await captureUndoSnapshot(() =>
      getSessionTagRows(sctx.sessionId),
    );
    const beforeSourceById = new Map(
      beforeRows.map((r) => [r.tag_id, r.source]),
    );

    const addedTagIds: string[] = [];
    const removedTags: { tagId: string; source: "manual" | "auto" | "ai" }[] = [];
    const addedNames: string[] = [];
    const removedNames: string[] = [];

    for (const name of toAdd) {
      const trimmed = name.trim();
      if (!trimmed) continue;
      let tag = await getTagByName(trimmed);
      if (!tag) {
        const id = crypto.randomUUID();
        await createTag(id, trimmed);
        tag = { id, name: trimmed, color: null, created_at: new Date().toISOString() };
      }
      if (beforeSourceById.has(tag.id)) continue; // already on the session
      await addSessionTag(sctx.sessionId, tag.id, "ai");
      addedTagIds.push(tag.id);
      addedNames.push(trimmed);
    }

    for (const name of toRemove) {
      const trimmed = name.trim();
      if (!trimmed) continue;
      const tag = await getTagByName(trimmed);
      if (!tag) continue;
      const previousSource = beforeSourceById.get(tag.id);
      if (previousSource === undefined) continue; // wasn't on the session
      await removeSessionTag(sctx.sessionId, tag.id);
      removedTags.push({ tagId: tag.id, source: previousSource });
      removedNames.push(trimmed);
    }

    const parts: string[] = [];
    if (addedNames.length > 0) parts.push(`Tags added: ${addedNames.join(", ")}`);
    if (removedNames.length > 0)
      parts.push(`Tags removed: ${removedNames.join(", ")}`);
    const detail = parts.length > 0 ? parts.join(". ") : "No tag changes";
    const result =
      parts.length > 0 ? parts.join(". ") + "." : "Tags already up to date.";

    const didMutate = addedTagIds.length > 0 || removedTags.length > 0;
    return {
      name: "tag_session",
      label: "Tags",
      detail,
      result,
      undoData: didMutate ? { addedTagIds, removedTags } : undefined,
    };
  },
  undo: async (undoData, ctx) => {
    const data = undoData as {
      addedTagIds: string[];
      removedTags: { tagId: string; source: "manual" | "auto" | "ai" }[];
    };
    for (const tagId of data.addedTagIds) {
      await removeSessionTag(ctx.sessionId, tagId);
    }
    for (const { tagId, source } of data.removedTags) {
      await addSessionTag(ctx.sessionId, tagId, source);
    }
  },
});

// --- Folder context helpers ---

function formatFolderContextChain(folders: DbFolder[], folderId: string): string {
  const path = getFolderPath(folders, folderId);
  if (path.length === 0) return "";
  return path
    .map((f) => {
      const desc = f.description ? ` — ${f.description}` : "";
      return `${f.name}${desc}`;
    })
    .join(" > ");
}

function folderPathString(folders: DbFolder[], folderId: string): string {
  return getFolderPath(folders, folderId)
    .map((f) => f.name)
    .join(" > ");
}

function scoreFolderMatch(
  query: string,
  folder: DbFolder,
  pathString: string,
): number {
  const q = query.toLowerCase().trim();
  if (!q) return 0;
  const name = folder.name.toLowerCase();
  const desc = (folder.description ?? "").toLowerCase();
  const path = pathString.toLowerCase();

  let score = 0;
  if (name === q) score += 100;
  else if (name.includes(q)) score += 50;
  if (path.includes(q)) score += 20;
  if (desc.includes(q)) score += 10;

  // Per-token coverage in addition to the whole-query containment above.
  const qTokens = q.split(/\s+/).filter((t) => t.length > 1);
  for (const t of qTokens) {
    if (name.includes(t)) score += 5;
    else if (path.includes(t)) score += 3;
    else if (desc.includes(t)) score += 1;
  }

  return score;
}

// --- Tool: search_folders ---

registerTool({
  kind: "read",
  affects: [],
  schema: {
    type: "function",
    function: {
      name: "search_folders",
      description:
        "Search the user's folder tree by name, description, or hierarchical path. Returns up to 10 folders with their stable ID, full path (e.g. 'Work > Projects > Q4'), description, and a relevance score. Always call this before add_session_to_folder so you can pass a stable folder_id rather than a name (folder names can repeat across branches).",
      parameters: {
        type: "object",
        properties: {
          query: {
            type: "string",
            description:
              "A keyword or phrase to match against folder names, paths, and descriptions.",
          },
        },
        required: ["query"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args) => {
    const query = String(args.query).trim();
    const folders = await listFolders();

    const scored = folders
      .map((f) => {
        const path = folderPathString(folders, f.id);
        return {
          id: f.id,
          path,
          description: f.description ?? null,
          score: scoreFolderMatch(query, f, path),
        };
      })
      .filter((r) => r.score > 0)
      .sort((a, b) => b.score - a.score)
      .slice(0, 10);

    const detail =
      scored.length === 0
        ? `No folders match "${query}"`
        : `Matched ${scored.length} folder${scored.length === 1 ? "" : "s"}`;
    const result =
      scored.length === 0
        ? `No folders match "${query}". Available folders: ${folders.map((f) => f.name).join(", ") || "(none)"}`
        : `Matches:\n${scored
            .map(
              (r) =>
                `- id=${r.id} path="${r.path}" score=${r.score}${r.description ? ` description="${r.description}"` : ""}`,
            )
            .join("\n")}`;

    return {
      name: "search_folders",
      label: "Folders",
      detail,
      result,
      observation: {
        summary: detail,
        evidence: result,
        affectedIds: scored.map((r) => r.id),
      },
    };
  },
  undo: async () => {},
});

// --- Tool: add_session_to_folder ---

registerTool({
  kind: "mutate",
  affects: ["organization"],
  schema: {
    type: "function",
    function: {
      name: "add_session_to_folder",
      description:
        "Add the current session to a folder using its stable folder_id (returned by search_folders). If the session is already in a conflicting ancestor or descendant folder, that conflict is resolved by removing the session from the conflicting folder. Returns the folder's hierarchical context.",
      parameters: {
        type: "object",
        properties: {
          folder_id: {
            type: "string",
            description:
              "The stable folder ID returned by search_folders. Do not pass a folder name.",
          },
        },
        required: ["folder_id"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args, ctx) => {
    const sctx = requireSessionContext(ctx);
    const folderId = String(args.folder_id).trim();
    const folders = await listFolders();
    const target = folders.find((f) => f.id === folderId);
    if (!target) {
      return {
        name: "add_session_to_folder",
        label: "Folder",
        detail: `Folder ${folderId} not found`,
        result: `Error: No folder with id "${folderId}" exists. Call search_folders first to discover valid IDs.`,
      };
    }

    const currentFolderIds = await captureUndoSnapshot(
      () => sctx.folderIds ?? [],
    );
    if (currentFolderIds.includes(target.id)) {
      const contextChain = formatFolderContextChain(folders, target.id);
      return {
        name: "add_session_to_folder",
        label: "Folder",
        detail: `Already in "${target.name}"`,
        result: `Session is already in this folder. Context: ${contextChain}`,
      };
    }

    const conflicts = findBranchConflicts(folders, currentFolderIds, target.id);
    for (const cId of conflicts) {
      await dbRemoveSessionFromFolder(sctx.sessionId, cId);
    }
    await dbAddSessionToFolder(sctx.sessionId, target.id);

    const contextChain = formatFolderContextChain(folders, target.id);
    return {
      name: "add_session_to_folder",
      label: "Folder",
      detail: `Added to "${target.name}"`,
      result: `Session added to "${target.name}". Folder context: ${contextChain}. Use this context to inform your summary.`,
      undoData: { addedFolderId: target.id, removedConflicts: conflicts },
      observation: {
        summary: `Added to "${target.name}"`,
        evidence: contextChain,
        affectedIds: [target.id, ...conflicts],
      },
    };
  },
  undo: async (undoData, ctx) => {
    const data = undoData as {
      addedFolderId: string;
      removedConflicts: string[];
    };
    await dbRemoveSessionFromFolder(ctx.sessionId, data.addedFolderId);
    for (const folderId of data.removedConflicts) {
      await dbAddSessionToFolder(ctx.sessionId, folderId);
    }
  },
});

// --- Tool: search_sessions ---

interface SessionSearchCandidate {
  session_id: string;
  title: string;
  date: string;
  folder_path: string | null;
  snippet: string;
  source_type: "title" | "note" | "segment";
  score: number;
}

registerTool({
  kind: "read",
  affects: [],
  schema: {
    type: "function",
    function: {
      name: "search_sessions",
      description:
        "Search across all sessions by transcript content, note content, or session title. Returns up to N compact candidates with stable session_id, title, date, folder path, a snippet from the matching field, source_type (segment/note/title), and a relevance score. Use this to find sessions before answering — do NOT assume sessions exist without searching. Follow up with get_session_context for the candidates that look most relevant.",
      parameters: {
        type: "object",
        properties: {
          query: {
            type: "string",
            description: "Free-form keywords to search for.",
          },
          filters: {
            type: ["object", "null"],
            description:
              "Filters to narrow the search. Pass null for an unfiltered search, or an object with both keys (each may be null individually).",
            properties: {
              folder_id: {
                type: ["string", "null"],
                description:
                  "Restrict to sessions in this folder, or null for no folder filter.",
              },
              pinned: {
                type: ["boolean", "null"],
                description:
                  "Restrict to pinned (true) or unpinned (false) sessions, or null for no pin filter.",
              },
            },
            required: ["folder_id", "pinned"],
            additionalProperties: false,
          },
          limit: {
            type: ["integer", "null"],
            description: "Max results to return (default 10, max 25).",
          },
        },
        required: ["query", "filters", "limit"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args, ctx) => {
    const query = String(args.query).trim();
    const filters = (args.filters ?? null) as {
      folder_id: string | null;
      pinned: boolean | null;
    } | null;
    const limitArg = args.limit;
    const limit = Math.min(
      typeof limitArg === "number" && limitArg > 0 ? limitArg : 10,
      25,
    );

    if (!query) {
      return {
        name: "search_sessions",
        label: "Sessions",
        detail: "Empty query",
        result: "Error: search_sessions requires a non-empty query.",
      };
    }

    const scopeIds = ctx.allowedSessionIds;
    const scopeSet = scopeIds ? new Set(scopeIds) : null;

    // Hybrid retrieval: title hits beat note hits beat segment hits, ordered
    // within each tier by FTS5 bm25 (already applied by the search* helpers).
    // We dedupe by session_id keeping the highest-precedence hit.
    const [titleHits, noteHits, segmentHits, folders, sessionFolders] =
      await Promise.all([
        searchSessionsByTitle(query),
        searchNotes(query),
        searchSegments(query),
        listFolders(),
        listAllSessionFolders(),
      ]);

    const bySessionId = new Map<string, SessionSearchCandidate>();
    const score = (rank: number, tier: number) =>
      // tier ∈ {0=title, 1=note, 2=segment}; lower index = stronger.
      // Within a tier, earlier ranks score higher.
      Math.max(0, 1000 - tier * 100 - rank);

    const inScope = (sessionId: string) => !scopeSet || scopeSet.has(sessionId);

    titleHits.forEach((hit, i) => {
      if (!inScope(hit.sessionId)) return;
      bySessionId.set(hit.sessionId, {
        session_id: hit.sessionId,
        title: hit.sessionTitle,
        date: "",
        folder_path: null,
        snippet: hit.sessionTitle,
        source_type: "title",
        score: score(i, 0),
      });
    });
    noteHits.forEach((hit, i) => {
      if (!inScope(hit.sessionId)) return;
      if (bySessionId.has(hit.sessionId)) return;
      bySessionId.set(hit.sessionId, {
        session_id: hit.sessionId,
        title: hit.sessionTitle,
        date: "",
        folder_path: null,
        snippet: hit.snippet.slice(0, 240),
        source_type: "note",
        score: score(i, 1),
      });
    });
    segmentHits.forEach((hit, i) => {
      if (!inScope(hit.sessionId)) return;
      if (bySessionId.has(hit.sessionId)) return;
      // Prepend the speaker label so the model can tell who said this
      // before deciding whether to expand the session. The label uses the
      // same convention as `formatSegmentSpeaker`: Mic -> "You", System
      // with diarisation -> speaker name, System without -> "Other".
      // Per-session speaker overrides (`speakerNames`) are applied so a
      // renamed "Speaker 1 -> Alice" mapping shows through here too.
      const speakerNames =
        useAppStore.getState().settings.speakerNames[hit.sessionId];
      let label: string;
      if (hit.source === "Mic") label = "You";
      else if (hit.speakerId != null)
        label = speakerNames?.[hit.speakerId] ?? `Speaker ${hit.speakerId + 1}`;
      else label = "Other";
      bySessionId.set(hit.sessionId, {
        session_id: hit.sessionId,
        title: hit.sessionTitle,
        date: "",
        folder_path: null,
        snippet: `(${label}) ${hit.snippet.slice(0, 240)}`,
        source_type: "segment",
        score: score(i, 2),
      });
    });

    // Enrich with session metadata (date) and folder paths in one shot.
    const candidateIds = Array.from(bySessionId.keys());
    const sessions = await getSessionsByIds(candidateIds);
    const sessionById = new Map(sessions.map((s) => [s.id, s]));

    // Apply filters and enrich.
    const enriched: SessionSearchCandidate[] = [];
    for (const c of bySessionId.values()) {
      const s = sessionById.get(c.session_id);
      if (!s) continue;
      if (filters?.pinned !== null && filters?.pinned !== undefined) {
        const isPinned = s.is_pinned === 1;
        if (isPinned !== filters.pinned) continue;
      }
      if (filters?.folder_id) {
        const inFolder = sessionFolders.some(
          (sf) =>
            sf.session_id === c.session_id && sf.folder_id === filters.folder_id,
        );
        if (!inFolder) continue;
      }
      const folderIds = sessionFolders
        .filter((sf) => sf.session_id === c.session_id)
        .map((sf) => sf.folder_id);
      const folderPath =
        folderIds.length > 0 ? folderPathString(folders, folderIds[0]) : null;
      enriched.push({
        ...c,
        date: s.created_at,
        folder_path: folderPath,
      });
    }

    enriched.sort((a, b) => b.score - a.score);
    const top = enriched.slice(0, limit);

    const detail =
      top.length === 0
        ? `No sessions match "${query}"`
        : `Matched ${top.length} session${top.length === 1 ? "" : "s"}`;
    const result =
      top.length === 0
        ? `No sessions match "${query}".`
        : `Matches:\n${top
            .map(
              (r) =>
                `- session_id=${r.session_id} title="${r.title || "Untitled"}" date=${r.date.slice(0, 10)} ${r.folder_path ? `folder="${r.folder_path}" ` : ""}source=${r.source_type} score=${r.score}\n  snippet: ${r.snippet.slice(0, 160)}`,
            )
            .join("\n")}`;

    return {
      name: "search_sessions",
      label: "Sessions",
      detail,
      result,
      observation: {
        summary: detail,
        evidence: result,
        affectedIds: top.map((r) => r.session_id),
        provenance: top.map((r) => ({
          type: "session" as const,
          sessionId: r.session_id,
          title: r.title,
        })),
      },
    };
  },
  undo: async () => {},
});

// --- Tool: get_session_context ---

registerTool({
  kind: "read",
  affects: [],
  schema: {
    type: "function",
    function: {
      name: "get_session_context",
      description:
        "Expand a list of session_ids returned by search_sessions into structured context. Choose `scope` to control what is returned: 'segments' (transcript chunks with [seg:ID] for citation), 'notes' (the user's note content per session), 'summary' (currently null pending a future summarization step), or 'all' (segments + notes). At most 5 session_ids per call — anything more is rejected; pick the most relevant candidates and call again if you need more.",
      parameters: {
        type: "object",
        properties: {
          session_ids: {
            type: "array",
            items: { type: "string" },
            description: "Session IDs to expand. Use IDs from search_sessions.",
          },
          scope: {
            type: "string",
            enum: ["segments", "notes", "summary", "all"],
            description:
              "Which artifact to include per session. 'summary' is currently always null.",
          },
        },
        required: ["session_ids", "scope"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args, ctx) => {
    const sessionIds = (args.session_ids as string[]) ?? [];
    const scope = String(args.scope) as "segments" | "notes" | "summary" | "all";
    if (sessionIds.length === 0) {
      return {
        name: "get_session_context",
        label: "Sessions",
        detail: "No session_ids provided",
        result: "Error: get_session_context requires at least one session_id.",
      };
    }

    // Hard cap to keep a confused model from blowing up the next prompt.
    // The schema description nudges the model to stay ≤ 5; this enforces it.
    const MAX_SESSION_IDS = 5;
    if (sessionIds.length > MAX_SESSION_IDS) {
      return {
        name: "get_session_context",
        label: "Sessions",
        detail: `Too many session_ids (${sessionIds.length})`,
        result: `Error: get_session_context accepts at most ${MAX_SESSION_IDS} session_ids per call (got ${sessionIds.length}). Pick the most relevant candidates from search_sessions and call again.`,
      };
    }

    if (ctx.allowedSessionIds) {
      const scopeSet = new Set(ctx.allowedSessionIds);
      const outOfScope = sessionIds.filter((id) => !scopeSet.has(id));
      if (outOfScope.length > 0) {
        return {
          name: "get_session_context",
          label: "Sessions",
          detail: `${outOfScope.length} session_id${outOfScope.length === 1 ? "" : "s"} out of scope`,
          result: `Error: requested session_ids are outside the current chat scope: ${outOfScope.join(", ")}. Only call search_sessions and get_session_context with IDs returned from this chat's search_sessions.`,
        };
      }
    }

    const wantSegments = scope === "segments" || scope === "all";
    const wantNotes = scope === "notes" || scope === "all";

    const sessions = await getSessionsByIds(sessionIds);
    const sessionById = new Map(sessions.map((s) => [s.id, s]));

    const blocks: string[] = [];
    const provenance: Array<SegmentRef | SessionRef> = [];

    for (const sid of sessionIds) {
      const s = sessionById.get(sid);
      if (!s) {
        blocks.push(`### session_id=${sid}\nNot found.`);
        continue;
      }
      const lines: string[] = [
        `### session_id=${sid} title="${s.title || "Untitled"}" date=${s.created_at.slice(0, 10)}`,
      ];
      provenance.push({ type: "session", sessionId: sid, title: s.title });

      if (wantNotes) {
        const note = await getNote(sid);
        if (note?.content) {
          lines.push("**Notes:**");
          lines.push(stripHtml(note.content));
        } else {
          lines.push("**Notes:** (none)");
        }
      }

      if (wantSegments) {
        const segs = await getSessionSegments(sid);
        const live = segs.filter(isVisibleSegment);
        if (live.length === 0) {
          lines.push("**Transcript:** (no segments)");
        } else {
          // Per-session speaker name overrides — keyed by speaker_id within
          // that session. Pulled fresh per session because IDs are
          // session-local (the same speaker_id in two sessions is two
          // different people).
          const speakerNames =
            useAppStore.getState().settings.speakerNames[sid];
          lines.push("**Transcript:**");
          for (const seg of live.slice(0, 200)) {
            const ts = formatTime(Math.max(0, seg.audio_offset_seconds));
            const label = formatSegmentSpeaker(seg, speakerNames);
            lines.push(`[seg:${seg.id} ${ts}] (${label}) ${seg.text}`);
            provenance.push({
              type: "segment",
              segmentId: seg.id,
              sessionId: sid,
              audioOffsetSeconds: seg.audio_offset_seconds,
            });
          }
          if (live.length > 200) {
            lines.push(`… (${live.length - 200} more segments not shown)`);
          }
        }
      }

      if (scope === "summary") {
        // Summarization is not implemented yet — `get_session_context` returns
        // null on the wire for this scope. A future commit will populate it
        // from a persisted session summary column.
        lines.push("**Summary:** null");
      }

      blocks.push(lines.join("\n"));
    }

    const detail = `Expanded ${sessionIds.length} session${sessionIds.length === 1 ? "" : "s"} (${scope})`;
    const result = blocks.join("\n\n");

    return {
      name: "get_session_context",
      label: "Sessions",
      detail,
      result,
      observation: {
        summary: detail,
        evidence: result.slice(0, 500),
        affectedIds: sessionIds,
        provenance,
      },
    };
  },
  undo: async () => {},
});

// --- Tool: replace_in_transcript ---

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

registerTool({
  kind: "mutate",
  affects: ["transcript"],
  schema: {
    type: "function",
    function: {
      name: "replace_in_transcript",
      description:
        "Find and replace text within the current session's transcript segments. Use this when the user spots a misheard or misspelled word in the transcript and asks you to fix it (e.g. \"it should be 'kubernetes' not 'kubernet ease' everywhere\"). Matches every occurrence inside each segment's text; case-insensitive by default. Limited to 50 affected segments per call — if more match, the call fails and the model should narrow `find`.",
      parameters: {
        type: "object",
        properties: {
          find: {
            type: "string",
            description:
              "Substring to search for in segment text. Required and non-empty.",
          },
          replace: {
            type: "string",
            description:
              "Replacement text. Pass an empty string to delete every occurrence.",
          },
          case_sensitive: {
            type: ["boolean", "null"],
            description:
              "When true, match exact casing. When false or null, match case-insensitively (default).",
          },
        },
        required: ["find", "replace", "case_sensitive"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args, ctx) => {
    const sctx = requireSessionContext(ctx);
    const find = String(args.find ?? "");
    const replace = String(args.replace ?? "");
    const caseSensitive = args.case_sensitive === true;

    if (!find) {
      return {
        name: "replace_in_transcript",
        label: "Transcript",
        detail: "Empty find",
        result: "Error: replace_in_transcript requires a non-empty `find`.",
      };
    }
    if (find === replace) {
      return {
        name: "replace_in_transcript",
        label: "Transcript",
        detail: "No-op (find equals replace)",
        result: `No change — \`find\` and \`replace\` are identical ("${find}").`,
      };
    }
    if (!sctx.segments || sctx.segments.length === 0) {
      return {
        name: "replace_in_transcript",
        label: "Transcript",
        detail: "No segments",
        result: "Error: this session has no transcript segments to edit.",
      };
    }

    const flags = caseSensitive ? "g" : "gi";
    const pattern = new RegExp(escapeRegExp(find), flags);

    type EditPlan = { segmentId: string; previousText: string; newText: string };
    const plans: EditPlan[] = [];
    for (const seg of sctx.segments) {
      if (!isVisibleSegment(seg)) continue;
      pattern.lastIndex = 0;
      if (!pattern.test(seg.text)) continue;
      pattern.lastIndex = 0;
      const newText = seg.text.replace(pattern, replace);
      if (newText === seg.text) continue;
      plans.push({ segmentId: seg.id, previousText: seg.text, newText });
    }

    const MAX_AFFECTED = 50;
    if (plans.length > MAX_AFFECTED) {
      return {
        name: "replace_in_transcript",
        label: "Transcript",
        detail: `Too many matches (${plans.length})`,
        result: `Error: replace_in_transcript would change ${plans.length} segments, exceeding the cap of ${MAX_AFFECTED}. Narrow \`find\` (use surrounding words for context) and call again.`,
      };
    }
    if (plans.length === 0) {
      return {
        name: "replace_in_transcript",
        label: "Transcript",
        detail: `No matches for "${find}"`,
        result: `No segment in this session contains "${find}". Verify the spelling against the transcript before retrying.`,
      };
    }

    // Snapshot pre-edit state for undo before any DB writes — `updateSegmentText`
    // mutates a per-segment original_text snapshot too, but that captures the
    // very first transcription, not the pre-this-call state.
    const undoData = await captureUndoSnapshot(() =>
      plans.map((p) => ({ segmentId: p.segmentId, previousText: p.previousText })),
    );

    for (const p of plans) {
      await updateSegmentText(p.segmentId, p.newText);
    }

    const detail = `Replaced "${find}" with "${replace}" in ${plans.length} segment${plans.length === 1 ? "" : "s"}`;
    const result =
      `${detail}.\n` +
      plans
        .slice(0, 10)
        .map((p) => `- seg=${p.segmentId}: "${p.previousText.slice(0, 80)}" → "${p.newText.slice(0, 80)}"`)
        .join("\n") +
      (plans.length > 10 ? `\n… (${plans.length - 10} more)` : "");

    return {
      name: "replace_in_transcript",
      label: "Transcript",
      detail,
      result,
      undoData,
      observation: {
        summary: detail,
        evidence: result,
        affectedIds: plans.map((p) => p.segmentId),
      },
    };
  },
  undo: async (undoData) => {
    const data = undoData as { segmentId: string; previousText: string }[];
    for (const { segmentId, previousText } of data) {
      await updateSegmentText(segmentId, previousText);
    }
  },
});

// --- Tool: search_dictations ---

registerTool({
  kind: "read",
  affects: [],
  schema: {
    type: "function",
    function: {
      name: "search_dictations",
      description:
        "Search the user's dictation history — short voice-to-text snippets (typically 3–5 sentences each) the user has dictated outside of recording sessions. Useful as a personal knowledge base: facts, ideas, reminders, definitions the user wrote earlier. Each result includes the full dictated text since entries are short, so no separate expansion call is needed. Use when the user asks a question that might be answered by something they previously dictated, or when drafting notes that should incorporate their prior thinking.",
      parameters: {
        type: "object",
        properties: {
          query: {
            type: "string",
            description: "Free-form keywords to search dictation text.",
          },
          limit: {
            type: ["integer", "null"],
            description: "Max results to return (default 10, max 25).",
          },
        },
        required: ["query", "limit"],
        additionalProperties: false,
      },
    },
  },
  execute: async (args) => {
    const query = String(args.query).trim();
    const limitArg = args.limit;
    const limit = Math.min(
      typeof limitArg === "number" && limitArg > 0 ? limitArg : 10,
      25,
    );

    if (!query) {
      return {
        name: "search_dictations",
        label: "Dictations",
        detail: "Empty query",
        result: "Error: search_dictations requires a non-empty query.",
      };
    }

    const hits = await searchDictations(query);
    const top = hits.slice(0, limit);

    const detail =
      top.length === 0
        ? `No dictations match "${query}"`
        : `Matched ${top.length} dictation${top.length === 1 ? "" : "s"}`;

    const result =
      top.length === 0
        ? `No dictations match "${query}".`
        : `Matches:\n${top
            .map(
              (h) =>
                `- dictation_id=${h.dictationId} slot="${h.slotName}"${h.sessionId ? ` linked_session=${h.sessionId}` : ""}\n  ${h.snippet.slice(0, 600)}`,
            )
            .join("\n")}`;

    return {
      name: "search_dictations",
      label: "Dictations",
      detail,
      result,
      observation: {
        summary: detail,
        evidence: result,
        affectedIds: top.map((h) => h.dictationId),
      },
    };
  },
  undo: async () => {},
});
