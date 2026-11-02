# AI Context & Tooling

How the AI chat learns what's on screen, what tools it can call, and how the system prompt is assembled. Also covers folder-based organizational context and the tags primitive (live since v11).

For the streaming + tool-call plumbing and undo machinery, see [`ARCHITECTURE.md`](./ARCHITECTURE.md) § AI Chat & Tool Calling.

---

## Context Surfaces

YapStack has four chat contexts, each built by a dedicated factory in [`apps/desktop/src/lib/ai-context.ts`](../apps/desktop/src/lib/ai-context.ts). The factory returns an `AIContextValue` with sources (what goes into the prompt), tools (what the model can call), and a system-prompt builder.

| Context | Factory (sources) | System prompt | Tools |
| --- | --- | --- | --- |
| Session | `createSessionSources(sessionId, segmentCount, sessionType)` | `createSessionSystemPromptBuilder(sessionId)` → `getSystemPromptWithToolContext` | All 10 (mutating + retrieval): `update_title`, `save_to_notes`, `pin_session`, `tag_session`, `search_folders`, `add_session_to_folder`, `search_sessions`, `search_dictations`, `get_session_context`, `replace_in_transcript` |
| Multi-session (All / Pinned) | `createMultiSessionSources(sessionIds, count)` | `createMultiSessionSystemPromptBuilder()` → `getMultiSessionSystemPrompt` | Retrieval-only: `search_sessions`, `get_session_context`, `search_folders`, `search_dictations`. The `allowedSessionIds` array on the tool context pins retrieval to the chat's filter (all / pinned) so the model can't reach outside the user's view. |
| Folder | `createMultiSessionSources(...)` + folder-description layers | `createMultiSessionSystemPromptBuilder(layers)` | Retrieval-only (same set as multi-session), pinned to the folder's session ids. |
| Dictation | `createDictationSources(count)` | `createDictationSystemPromptBuilder()` → `getDictationSystemPrompt` | `search_dictations` only (deliberately scoped — folder/session retrieval is not surfaced here). |

`resolveListContext(chatContext, deps)` picks the right factory for non-session views. For sessions, the `AIContextProvider` composes the session factories directly.

### `AIContextValue` shape

```ts
interface AIContextValue {
  contextKey: string;                            // stable key for message history
  sources: ContextSource[];                      // togglable prompt-slot assemblers
  toggleSource: (sourceId: string) => void;
  tools: AIContextTools;                         // { availableToolIds, getToolContext, contextType }
  actions: ActionDefinition[];                   // preset directives (`ai-actions.ts`)
  segments: DbSegment[];                         // used for citation → pill conversion
  buildSystemPrompt: SystemPromptBuilder;        // (directive, parts, attachments) => string
  isSessionContext: boolean;
  sessionId: string | null;
  onToolsExecuted: (toolNames: string[]) => Promise<void>;
  placeholder: string;
}
```

A `ContextSource` owns an assembler function. Sources are togglable (`toggleable: true`) in the UI; disabled sources pass `""` to the builder.

---

## System Prompt Assembly

All prompt text lives in [`apps/desktop/src/lib/ai-prompts.ts`](../apps/desktop/src/lib/ai-prompts.ts). The `AIContextValue.buildSystemPrompt` builder calls these.

### Session prompt — `getSystemPromptWithToolContext`

Assembled in this order (see `getSystemPrompt` + the tool-context wrapper):

1. **Directive** — either the `GENERAL_DIRECTIVE` or a user-selected `ActionDefinition` from `ai-actions.ts`.
2. **Citation instruction** (`CITATION_INSTRUCTION`) — added only when transcript text is present. Tells the model to use `[[seg:ID]]` for references.
3. **Speaker instruction** (`SPEAKER_INSTRUCTION`) — added only when diarization data is present (`transcriptHasSpeakers(segments)`). Tells the model to attribute statements to `(Speaker N)` / `(Alice)` labels.
4. **Notes guidance** (`NOTES_GUIDANCE`) — how to choose `append` vs `replace` for `save_to_notes`.
5. **Content-scale guidance** — `getContentScaleGuidance(transcript)` computes a target word count based on transcript length (ratio 0.4 / 0.3 / 0.2 for short / medium / long).
6. **Transcript** — `## Session Transcript\n<assembled>` (segment lines, optionally with `(Speaker)` prefixes).
7. **Notes** — `## Notes\n<text>` (Tiptap HTML → text).
8. **Attached files** — `## Attached Files\n### name\n<content>` per attachment.
9. **Session metadata tail** — current title, pinned state, has-notes boolean.

### Multi-session prompt — `getMultiSessionSystemPrompt`

Tone: "viewing multiple sessions — compare, synthesize, don't use `#` headings." Includes:

1. A short directive.
2. **Organizational context** block when `folderContext` is non-empty — each layer is `- **{name}:** {description}`.
3. **Sessions** context — either titles-only or titles+notes depending on whether the `session-notes` source is enabled.
4. Attached files.

### Dictation prompt — `getDictationSystemPrompt`

Tone: "viewing dictation history." Plain list of entries, no tools, no citations.

---

## Keeping Context Size Down

The prompt can grow fast when sessions are long or folders aggregate many items. Rules we follow today:

- **Transcripts carry IDs, not timestamps.** `assembleTranscriptContext` emits `[seg:ID]` tokens; the model maps these back to moments without consuming tokens on `MM:SS` strings.
- **Speaker prefix only when diarized.** No `(Speaker 1)` on undiarized sessions — that's wasted budget when every line is the same unknown speaker.
- **Notes are passed as text, not HTML.** Tiptap's HTML is stripped before assembly (`assembleNoteContext`).
- **Multi-session aggregation is titles + notes, not transcripts.** If you're viewing a folder with 200 sessions, the prompt lists titles and (optionally) notes — it does **not** concatenate transcripts. Attempting to do so would blow past any reasonable context budget.
- **Folder-description layer is scoped to the active path.** `resolveListContext` walks `getFolderPath(folders, folderId)` and attaches only folders whose `description` is set. Deep empty trees contribute nothing.
- **Content-scale guidance tells the model to self-limit.** For a 5,000-word transcript, the prompt asks for ≤ 1,000 words out — don't also ask for "detailed" or "exhaustive" output in the directive.

**Rule of thumb when adding a field:** if the field pushes a 60-minute session's prompt past the conservative context budget (~100k tokens on a Parakeet transcript), gate it behind a user toggle or summarize it server-side first. Don't silently include it.

---

## Tool Registry — The Contract

Registry: [`apps/desktop/src/lib/ai-tools.ts`](../apps/desktop/src/lib/ai-tools.ts). A singleton `Map<name, ToolDefinition>` populated at module-load time by `registerTool(...)` calls.

### `ToolDefinition` shape

```ts
type ToolKind = "read" | "mutate";

type ToolEffect = "session-meta" | "notes" | "organization" | "transcript";

interface ToolDefinition {
  kind: ToolKind;                                                    // gates Undo + "Session updated" toast
  schema: ChatCompletionTool;                                        // OpenAI function-call schema
  execute: (args, ctx: ToolContext) => Promise<ExecutedTool | null>; // returns null to skip (no-op)
  undo:    (undoData, ctx: { sessionId: string }) => Promise<void>;  // no-op for "read" tools
  affects?: ToolEffect[];                                            // drives content-refresh callbacks
}

// ToolContext is a discriminated union — multi-session chats no longer
// fabricate empty session-meta values. Mutating tools narrow via
// `requireSessionContext(ctx)` and operate on `SessionToolContext`;
// retrieval tools that work in both scopes (e.g. `search_sessions`) read
// `allowedSessionIds` directly off the union.
type ToolContext = SessionToolContext | RetrievalToolContext;

interface SessionToolContext {
  scope: "session";
  sessionId: string;
  currentTitle: string;
  currentNote: DbNote | null;
  isPinned: boolean;
  segments?: DbSegment[];
  tags?: string[];
  folderNames?: string[];
  folderIds?: string[];
  allowedSessionIds?: string[]; // reserved for future filtered single-session views
}

interface RetrievalToolContext {
  scope: "retrieval";
  allowedSessionIds: string[];  // pins retrieval tools to the chat's filter (folder/pinned/all)
}

interface ExecutedTool {
  name: string;
  label: string;            // short badge label ("Title", "Notes", "Pinned")
  detail: string;           // one-line description shown in the toast
  observation?: string;     // text the LLM sees as the tool result for multi-turn chains
  undoData?: unknown;       // opaque blob passed back to undo() — required for kind: "mutate"
}
```

### Currently registered tools

| Name | Affects | Params | Behavior | Undo |
| --- | --- | --- | --- | --- |
| `update_title` | `session-meta` | `title: string` (clamped to 80 chars) | Calls `updateSessionTitle`. No-op if unchanged. | Restores previous title. |
| `save_to_notes` | `notes` | `content: string` (markdown), `mode: "replace" \| "append" \| "prepend" \| "find_replace"`, `find?: string` (required for `find_replace`) | Converts markdown → HTML, runs `convertCitationsToSegmentRefs`. `replace` overwrites; `append` joins with `<hr>` below; `prepend` joins above; `find_replace` does a surgical substring swap inside the existing notes (plain-text replacement — markdown won't render). | Restores previous note content (or `<p></p>` if there was none). |
| `pin_session` | `session-meta` | `pinned: boolean` | Toggles pin if different from current state. | Restores previous pin state via `togglePin`. |
| `tag_session` | `organization` | `add: string[]`, `remove?: string[]` | Adds/removes tags. Creates new tags on-the-fly via `getTagByName` + `createTag`. `source = "ai"` so the UI can distinguish AI-applied tags. | Removes added tags + restores removed tags. |
| `search_folders` | (read-only) | `query?: string` | Returns the folder tree (or query-filtered subset) with `folder_id`, `name`, parent chain, and `description`. Used by Phase 1 of folder-aware actions before `add_session_to_folder`. | n/a |
| `add_session_to_folder` | `organization` | `folder_id: string` | Classifies session into a folder by id. Resolves branch conflicts, then returns the hierarchical description chain in `result` so Phase 2 of two-phase actions sees the parent context. | Removes the session from that folder. |
| `search_sessions` | (read-only) | `query: string`, `filters: { folder_id: string \| null, pinned: boolean \| null } \| null`, `limit?: number` | FTS5 search across session titles, notes, and segment text. `filters` is `null` for an unfiltered search, or an object whose two fields are individually nullable (each `null` skips that filter). Used when the user references a different session by name. | n/a |
| `search_dictations` | (read-only) | `query: string`, `limit?: number` | FTS5 search across `dictation_history`. | n/a |
| `get_session_context` | (read-only) | `session_ids: string[]`, `scope: "segments" \| "notes" \| "summary" \| "all"` | Expands the listed `session_ids` (max 5/call) into the requested artifact: transcript chunks (with `[seg:ID]` for citation), notes, summary (currently always null pending a future summarization step), or all. Out-of-scope ids are rejected when the chat carries `allowedSessionIds`. | n/a |
| `replace_in_transcript` | `transcript` (refresh) | `find: string`, `replace: string`, `case_sensitive: boolean` | Edits segment text in the durable DB rows — surgical fix for typos / mis-transcriptions. Default case-insensitive. Capped at 50 affected segments per call (call fails over the cap; narrow `find`). Preserves the per-segment `original_text` snapshot. | Restores each touched segment's pre-call text. |

Multi-session and folder contexts expose only the **retrieval** tools — `search_sessions`, `get_session_context`, `search_folders`, `search_dictations` — so the LLM can search and expand sessions on demand instead of having all candidates dumped into the prompt. Mutating tools are intentionally omitted there; they need a single `sessionId` in `ToolContext`, and applying them at folder scope is ambiguous ("pin which session?"). The dictation context narrows further to **`search_dictations` only** — folder/session retrieval tools are not surfaced because dictation lives in its own table and the surrounding session/folder lookups would just return empty (or worse, leak unrelated state).

### How to add a new tool

1. **Define the schema.** OpenAI function-calling shape. Be strict about `required`.
2. **Implement `execute`.** Read `ctx` for current state, perform the mutation via `lib/db.ts`, return an `ExecutedTool` with `undoData` so it can be reversed. Return `null` for no-ops (don't throw).
3. **Implement `undo`.** Must be idempotent — it runs only if `undoData !== undefined` and the user hits the undo toast within the window.
4. **Register at module bottom:** `registerTool({ schema, execute, undo, affects });`
5. **Citation handling.** If your tool writes markdown that will be saved as HTML (notes, docs), run the output through `convertCitationsToSegmentRefs(html, segments)` so `[[seg:ID]]` becomes a clickable pill.
6. **Expose it.** Add the tool's name to the relevant context's `availableToolIds` in `ai-context.ts`. Session context already lists all 10 tools — append yours. If the tool is retrieval-only and meaningful at folder/multi-session scope, also add it to `createMultiSessionTools` (the retrieval set is intentionally narrow there, so add carefully). Do not create a new registry.
7. **Declare effects.** Set `affects` so the UI can invalidate the right data after a run: `"session-meta"` invalidates the header, `"notes"` refreshes the note editor.

Undo window is 10 s; the user sees a toast. Your `undo` must work even if the user has navigated away — treat it as a DB mutation, not a UI revert.

### Streaming + tool execution

`streamChatWithTools()` (in `lib/ai.ts`) yields typed `StreamEvent`s:

- `token` — streaming response chunk
- `tool_calls` — the model requested one or more tool calls
- `done` — final assistant message

Consumed in [`apps/desktop/src/hooks/useChatMessages.ts`](../apps/desktop/src/hooks/useChatMessages.ts). On a `tool_calls` event the hook:

1. Fetches fresh `ToolContext` via the context's `getToolContext()`.
2. Runs `executeTool(name, args, ctx)` for each.
3. Shows a `sonner` toast with an "Undo" action wired to `undoToolCalls(executed, ctx)`.
4. Calls `onToolsExecuted(names)` so the context can invalidate derived state.

---

## Folder Hierarchy

Folders are YapStack's only organizational grouping today (see § Tags for pending work).

### Schema (on `main`)

```sql
folders(
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  parent_id    TEXT,                -- null for root; FK to folders(id)
  sort_order   INTEGER,
  icon         TEXT,
  color        TEXT,
  description  TEXT,                -- injected into multi-session system prompt
  created_at   TEXT,
  updated_at   TEXT
);

session_folders(
  session_id   TEXT NOT NULL,       -- FK sessions(id)
  folder_id    TEXT NOT NULL,       -- FK folders(id)
  created_at   TEXT,
  PRIMARY KEY (session_id, folder_id)
);
```

A session can belong to multiple folders (many-to-many). Folder nesting is unbounded in the schema.

### Traversal helpers

[`apps/desktop/src/lib/folder-tree.ts`](../apps/desktop/src/lib/folder-tree.ts) has everything you need — do not reimplement:

| Function | Use |
| --- | --- |
| `buildFolderTree(folders)` | Flat → nested tree for sidebar rendering. |
| `getFolderPath(folders, id)` | Ancestor chain `[root, …, parent, current]` — used for breadcrumbs and for the multi-session prompt's organizational layer. |
| `getRootFolder(folders, id)` | Root of a branch — used for filtering "All" / "Pinned" views. |
| `getAncestorIds(folders, id)` | Ancestors (exclusive). |
| `getDescendantIds(folders, id)` | All descendants (BFS). |
| `buildChildMap(folders)` | Parent → children lookup. |
| `isDescendantOf(folders, a, b)` | Transitive descendant check. |
| `findBranchConflicts(folders, sessionFolderIds, targetId)` | Detects when a session would end up in both an ancestor and descendant — used by drag-drop to warn the user. |
| `getDisplayFolders(...)` | Picks which folder badge to show on a session row depending on the current view context. |

### Folder context for AI

When the chat context is `{ type: "folder", folderId }`, `resolveListContext` walks `getFolderPath` and keeps only folders with a non-empty `description`:

```ts
const layers = folderPath
  .filter((f) => !!f.description)
  .map((f) => ({ name: f.name, description: f.description }));
```

These become the **Organizational context** bullet list in the multi-session system prompt. Prompt-budget note: we pass `name` + `description` only — not icons, colors, sort order, or member counts. Deep trees should surface in prompts as path breadcrumbs, not flattened lists.

---

## Tags

Tags are a flat, lightweight metadata primitive applied by the AI during summarization (via `tag_session`) or by the user. **Folders remain the primary organizational primitive** — hierarchical, with descriptions that flow into the AI system prompt. Tags are deliberately *not* hierarchical and don't carry descriptions.

### Schema (migration v11)

```sql
CREATE TABLE tags (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
  color       TEXT,
  created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_tags_name ON tags(name);

CREATE TABLE session_tags (
  session_id  TEXT NOT NULL,
  tag_id      TEXT NOT NULL,
  source      TEXT NOT NULL DEFAULT 'manual',   -- 'manual' | 'ai'
  confidence  REAL,                             -- populated for AI-applied tags
  created_at  TEXT NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (session_id, tag_id),
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
  FOREIGN KEY (tag_id)     REFERENCES tags(id)     ON DELETE CASCADE
);
CREATE INDEX idx_session_tags_tag     ON session_tags(tag_id);
CREATE INDEX idx_session_tags_session ON session_tags(session_id);
```

Design notes:

- Unique tag names are case-insensitive (`COLLATE NOCASE`). `tag_session` matches against existing tags by lowercased name and creates new tags on miss.
- `session_tags.source` distinguishes user-applied (`manual`) from AI-applied (`ai`) tags so the UI can render an AI badge or let the user confirm suggestions.
- `ON DELETE CASCADE` on both FKs: deleting a session removes its tag links; deleting a tag removes it from all sessions.

### Auto-folder suggestions during recording

[`apps/desktop/src/lib/auto-tag.ts`](../apps/desktop/src/lib/auto-tag.ts) builds a keyword map from folder names (length ≥ 4) and scans transcript text for whole-word matches. `FolderSuggestionTracker` requires ≥ 2 matches before surfacing a suggestion chip — this prevents noisy single-mention false positives. Suggestions land as folder chips below the live recording header; on accept, the session gains the folder via `dbAddSessionToFolder`.

Suggestions are **folder-only**, not tag-only. Folder names double as the vocabulary for auto-categorization, which keeps the organizational hierarchy coherent. Tags are applied *after* recording, by the AI during summarization, against the (now-classified) session.
