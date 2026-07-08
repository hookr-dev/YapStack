import OpenAI from "openai";
import { fetch as tauriFetch } from "@tauri-apps/plugin-http";
import type {
  ChatCompletionMessageParam,
  ChatCompletionTool,
} from "openai/resources/chat/completions";
import { marked } from "marked";
import DOMPurify from "dompurify";
import type { DbSegment } from "./db";
import type { DbDictationHistory } from "./db";
import type { ToolCallResult } from "./ai-tools";
import type { FolderTreeNode } from "./folder-tree";

// ----- ChatContext -----

export type ChatContext =
  | { type: "session"; sessionId: string }
  | { type: "folder"; folderId: string }
  | { type: "all" }
  | { type: "pinned" }
  | { type: "dictation" };

export function chatContextKey(ctx: ChatContext): string {
  switch (ctx.type) {
    case "session":
      return ctx.sessionId;
    case "folder":
      return `folder:${ctx.folderId}`;
    case "all":
      return "global";
    case "pinned":
      return "pinned";
    case "dictation":
      return "dictation";
  }
}

// ----- Connection / Profile -----

export type AIProviderKind = "openai" | "openrouter" | "custom";

export interface Connection {
  id: string;
  name: string;
  kind: AIProviderKind;
  baseUrl: string;
  apiKey: string;
  availableModels?: string[];
  fetchedAt?: string;
  fetchError?: string;
}

export interface Profile {
  id: string;
  name: string;
  connectionId: string;
  model: string;
}

export interface AIAssignments {
  chatProfileId: string | null;
  aiActionsProfileId: string | null;
}

export interface AIConfig {
  connections: Connection[];
  profiles: Profile[];
  assignments: AIAssignments;
}

export const DEFAULT_AI_CONFIG: AIConfig = {
  connections: [],
  profiles: [],
  assignments: {
    chatProfileId: null,
    aiActionsProfileId: null,
  },
};

// ----- Vault-wrapping AI connection secrets (CRYPTO_SPEC §4, sync deliverable E) -----

/** The vault-wrapped form of a Connection's credential. `apiKey`/`baseUrl` are
 *  replaced by an opaque base64 committing envelope so they can never reach a
 *  syncable surface in plaintext. */
export interface WrappedConnection
  extends Omit<Connection, "apiKey" | "baseUrl"> {
  /** base64 committing envelope of `apiKey` (empty string when no key). */
  wrappedApiKey: string;
  /** base64 committing envelope of `baseUrl`. */
  wrappedBaseUrl: string;
}

export interface WrappedAIConfig extends Omit<AIConfig, "connections"> {
  connections: WrappedConnection[];
}

/**
 * Strip credentials from an AIConfig, leaving structure/labels intact. Used
 * anywhere an AIConfig might be logged, exported, or handed to a non-secret
 * surface. Never emits `apiKey`/`baseUrl`.
 */
export function stripAiSecrets(
  config: AIConfig,
): Omit<WrappedAIConfig, "connections"> & {
  connections: Omit<Connection, "apiKey" | "baseUrl">[];
} {
  return {
    ...config,
    connections: config.connections.map(({ apiKey: _k, baseUrl: _b, ...rest }) => rest),
  };
}

/**
 * Vault-wrap every connection credential before the config can be synced. The
 * plaintext `apiKey`/`baseUrl` are wrapped under the vault key held in the OS
 * keychain — the plaintext never enters a syncable payload. This is the
 * enforcement point for deliverable E: the AI apiKey is envelope-wrapped, not
 * persisted raw, before any path that could reach the relay.
 *
 * `wrap` is injected (not imported) so this stays pure/testable and callers
 * choose the vault binding. The Rust vault-wrap command seam was tree-shaken
 * while unused (SYNC_REMEDIATION.md §3) and will be recreated when the AI-sync
 * feature actually lands and needs it.
 */
export async function prepareAiConfigForSync(
  config: AIConfig,
  wrap: (plaintext: string) => Promise<string>,
): Promise<WrappedAIConfig> {
  const connections: WrappedConnection[] = [];
  for (const c of config.connections) {
    const { apiKey, baseUrl, ...rest } = c;
    connections.push({
      ...rest,
      wrappedApiKey: apiKey ? await wrap(apiKey) : "",
      wrappedBaseUrl: baseUrl ? await wrap(baseUrl) : "",
    });
  }
  return { ...config, connections };
}

export type ToolExecutionStatus = "running" | "done" | "error";

export interface ToolExecution {
  name: string;
  label: string;
  detail?: string;
  status: ToolExecutionStatus;
  /** True if the user reverted this call via Undo. Renderer applies a
   * grayed/strike-through style; the call still appears in chat history. */
  undone?: boolean;
}

export interface ChatMessage {
  id: string;
  role: "user" | "assistant";
  content: string;
  action?: AIActionType;
  isStreaming?: boolean;
  toolExecutions?: ToolExecution[];
}

export interface FileAttachment {
  name: string;
  content: string;
}

/** Any action ID. Built-in: "summarize" | "key-points" | "action-items" | "meeting-minutes" | "general" */
export type AIActionType = string;

// ----- Defaults -----

// ----- Client -----

/**
 * Construct an OpenAI client from a Connection. Local OpenAI-compatible
 * servers (custom kind) accept a blank apiKey; the placeholder satisfies
 * the SDK constructor without misleading the remote server.
 */
export function createAIClientForConnection(connection: Connection): OpenAI {
  const headers: Record<string, string> = {};
  if (connection.kind === "openrouter") {
    headers["HTTP-Referer"] = "https://yapstack.app";
    headers["X-Title"] = "YapStack";
  }
  const apiKey =
    connection.kind === "custom" && !connection.apiKey
      ? "sk-no-key-required"
      : connection.apiKey;
  return new OpenAI({
    apiKey,
    baseURL: connection.baseUrl,
    dangerouslyAllowBrowser: true,
    defaultHeaders: Object.keys(headers).length > 0 ? headers : undefined,
    fetch: tauriFetch,
  });
}

/**
 * Resolve a Profile reference into a usable (client, model) pair. Throws
 * if the profileId can't be resolved (missing Profile, missing Connection,
 * or null input) — per locked design decision #8, AI feature consumers
 * surface this error rather than silently retrying through another Profile.
 *
 * Error message is actionable so the feature can route the user to fix
 * the underlying configuration.
 */
export function resolveAndCreateClient(
  config: AIConfig,
  profileId: string | null,
): { client: OpenAI; model: string; connection: Connection } {
  if (!profileId) {
    throw new Error(
      "No AI Profile assigned. Open Settings → AI to set one up.",
    );
  }
  const profile = config.profiles.find((p) => p.id === profileId);
  if (!profile) {
    throw new Error(
      `AI Profile "${profileId}" not found. Open Settings → AI to reassign.`,
    );
  }
  const connection = config.connections.find(
    (c) => c.id === profile.connectionId,
  );
  if (!connection) {
    throw new Error(
      `Profile "${profile.name}" points at a deleted Connection. Open Settings → AI to fix.`,
    );
  }
  return {
    client: createAIClientForConnection(connection),
    model: profile.model,
    connection,
  };
}

/**
 * Fetch the `/models` catalog from an OpenAI-compatible endpoint.
 *
 * OpenAI requires `Authorization: Bearer <key>` on this endpoint (returns
 * 401 without it). OpenRouter accepts the same header but works unauthed.
 * Local OpenAI-compatible servers (Ollama, llama.cpp, LM Studio, vLLM)
 * typically don't require auth — passing the header is a no-op for them.
 * Send it whenever the caller has a key; omit otherwise.
 */
export async function fetchCustomModels(
  baseUrl: string,
  apiKey?: string,
): Promise<string[]> {
  const url = baseUrl.replace(/\/$/, "") + "/models";
  const trimmedKey = apiKey?.trim();
  const res = trimmedKey
    ? await tauriFetch(url, {
        headers: { Authorization: `Bearer ${trimmedKey}` },
      })
    : await tauriFetch(url);
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const json = (await res.json()) as { data?: Array<{ id?: unknown }> };
  if (!Array.isArray(json.data)) throw new Error("Unexpected response shape");
  return json.data
    .map((m) => (typeof m.id === "string" ? m.id : null))
    .filter((id): id is string => !!id);
}

/**
 * Decide whether the Connection editor should kick off a background model
 * re-fetch when the dialog is saved.
 *
 *  - On create: refresh only if the user hasn't already fetched a catalog in
 *    the dialog (otherwise we'd duplicate the fetch they just ran).
 *  - On edit: refresh when the endpoint OR the credentials changed since the
 *    dialog opened — a corrected API key must re-validate just like a changed
 *    base URL, otherwise a stale `fetchError` / model list sticks around. A
 *    manual "Refresh" inside the dialog moves the baseline forward (the caller
 *    resets the baseline to the just-fetched values), so this returns false
 *    and we don't fire a redundant second fetch on save.
 *  - Never while a fetch is already in flight.
 */
export function shouldAutoRefreshModels(params: {
  mode: "create" | "edit";
  hasLocalModels: boolean;
  fetching: boolean;
  baseUrl: string;
  baseUrlBaseline: string;
  apiKey: string;
  apiKeyBaseline: string;
}): boolean {
  if (params.fetching) return false;
  if (params.mode === "create") return !params.hasLocalModels;
  return (
    params.baseUrl !== params.baseUrlBaseline ||
    params.apiKey !== params.apiKeyBaseline
  );
}

// ----- Context Assembly -----

/**
 * Speaker label for a segment in transcript text rendered for the LLM.
 *
 * The convention is the same everywhere a segment is shown to the model
 * (single-session transcript context, `get_session_context` tool output,
 * `search_sessions` snippets). The model relies on this label — without
 * it, statements from other speakers look indistinguishable from things
 * the user said.
 *
 *  - `Mic`  → "You" (the human running the app, captured via microphone).
 *  - `System` with `speaker_id` set → custom name from `speakerNames` if
 *    present, else `Speaker N` (1-indexed for human-friendly numbering).
 *    System audio is everyone else: people on a video call, podcast
 *    audio, etc.
 *  - `System` without diarization → "Other" (we know it isn't the user
 *    but we have no further identification).
 *
 * Returning a flat string (not optional) means callers can always wrap
 * it in `(...)` without conditional formatting.
 */
export function formatSegmentSpeaker(
  seg: DbSegment,
  speakerNames?: Record<number, string>,
): string {
  if (seg.source === "Mic") return "You";
  if (seg.speaker_id != null) {
    return speakerNames?.[seg.speaker_id] ?? `Speaker ${seg.speaker_id + 1}`;
  }
  return "Other";
}

/**
 * The single visibility gate for AI processing. A segment is eligible to reach
 * any LLM / AI feature only if it is neither hidden nor soft-deleted. Hidden is
 * a user action meaning "keep this in the transcript UI but exclude it from AI";
 * deleted is gone entirely. EVERY path that feeds segment text to a model
 * (transcript context, insights, chat tools, auto-tag suggestions) must gate on
 * this — segments are loaded unfiltered for the transcript UI (which renders and
 * un-hides them), so exclusion happens here at the AI boundary, not at load.
 */
export function isVisibleSegment(seg: DbSegment): boolean {
  return seg.hidden !== 1 && !seg.deleted_at;
}

export function assembleTranscriptContext(
  segments: DbSegment[],
  speakerNames?: Record<number, string>,
): string {
  return segments
    .filter(isVisibleSegment)
    .map((s) => {
      const mins = Math.floor(s.audio_offset_seconds / 60);
      const secs = Math.floor(s.audio_offset_seconds % 60);
      const ts = `${mins}:${secs.toString().padStart(2, "0")}`;
      const label = formatSegmentSpeaker(s, speakerNames);
      return `[seg:${s.id} ${ts}] (${label}) ${s.text}`;
    })
    .join("\n");
}

/**
 * True for any transcript with at least one segment — speaker labels are
 * now applied universally (see `formatSegmentSpeaker`), so the
 * SPEAKER_INSTRUCTION prompt always applies when a transcript is in
 * scope. Kept as a function (not inlined) so callers stay readable and
 * we have one place to flip if the convention ever needs to be gated
 * again (e.g. for an engine that produces no source-tagged segments).
 */
export function transcriptHasSpeakers(segments: DbSegment[]): boolean {
  return segments.length > 0;
}

export function assembleNoteContext(noteHtml: string): string {
  // Strip HTML tags to get plain text
  return noteHtml
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<\/p>/gi, "\n")
    .replace(/<\/h[1-6]>/gi, "\n")
    .replace(/<\/li>/gi, "\n")
    .replace(/<[^>]+>/g, "")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

// ----- Folder Tree Context -----

export function assembleFolderTreeContext(tree: FolderTreeNode[]): string {
  function renderNode(node: FolderTreeNode, depth: number): string {
    const indent = "  ".repeat(depth);
    const desc = node.folder.description
      ? ` — "${node.folder.description}"`
      : "";
    let line = `${indent}- [${node.folder.id}] ${node.folder.name}${desc}`;
    for (const child of node.children) {
      line += "\n" + renderNode(child, depth + 1);
    }
    return line;
  }
  return tree.map((root) => renderNode(root, 0)).join("\n");
}

// ----- Multi-session context -----

/**
 * Compact, retrieval-friendly listing of session candidates. Used by
 * multi-session Chats so the LLM sees a candidate list (id + title + date
 * + folder path) and decides which sessions to expand via the
 * `get_session_context` tool, instead of being handed every session's
 * notes/transcript up front.
 */
export function assembleSessionCandidates(
  candidates: { id: string; title: string; date: string; folderPath: string | null }[],
): string {
  if (candidates.length === 0) return "";
  const lines = candidates.map((c) => {
    const date = c.date ? new Date(c.date).toLocaleDateString() : "";
    const folder = c.folderPath ? ` folder="${c.folderPath}"` : "";
    return `- session_id=${c.id} title="${c.title || "Untitled"}" date=${date}${folder}`;
  });
  return lines.join("\n");
}

// ----- Dictation Context Assembly -----

export function assembleDictationContext(entries: DbDictationHistory[]): string {
  return entries
    .map((e) => {
      const date = new Date(e.created_at);
      const dateStr = date.toLocaleDateString(undefined, {
        month: "short",
        day: "numeric",
      });
      const timeStr = date.toLocaleTimeString(undefined, {
        hour: "numeric",
        minute: "2-digit",
      });
      const dur = e.wav_duration_seconds
        ? `${Math.round(e.wav_duration_seconds)}s`
        : "";
      const meta = [dateStr, timeStr, dur].filter(Boolean).join(" · ");

      let block = `- **${e.slot_name}** (${meta})`;

      const inputDiffers = e.ai_enabled && e.input_text !== e.output_text;
      if (inputDiffers) {
        block += `\n  Input: "${e.input_text}"`;
        block += `\n  Output: "${e.output_text}"`;
      } else {
        block += `\n  Text: "${e.output_text || e.input_text}"`;
      }

      if (e.ai_enabled && e.ai_prompt) {
        block += `\n  AI prompt: "${e.ai_prompt}"`;
      }
      block += `\n  Action: ${e.output_action}`;

      return block;
    })
    .join("\n\n");
}

// ----- Streaming -----

export async function* streamChat(
  client: OpenAI,
  model: string,
  messages: ChatCompletionMessageParam[],
  signal?: AbortSignal,
): AsyncGenerator<string> {
  const stream = await client.chat.completions.create(
    {
      model,
      messages,
      stream: true,
    },
    { signal },
  );

  for await (const chunk of stream) {
    const delta = chunk.choices[0]?.delta?.content;
    if (delta) {
      yield delta;
    }
  }
}

// ----- Streaming with Tool Calls -----

export type StreamEvent =
  | { type: "token"; content: string }
  | { type: "tool_calls"; calls: ToolCallResult[] }
  | { type: "done" };

export async function* streamChatWithTools(
  client: OpenAI,
  model: string,
  messages: ChatCompletionMessageParam[],
  tools: ChatCompletionTool[],
  signal?: AbortSignal,
): AsyncGenerator<StreamEvent> {
  const stream = await client.chat.completions.create(
    {
      model,
      messages,
      tools: tools.length > 0 ? tools : undefined,
      tool_choice: tools.length > 0 ? "auto" : undefined,
      stream: true,
    },
    { signal },
  );

  // Accumulate tool calls across chunks
  const toolCallMap = new Map<
    number,
    { id: string; name: string; arguments: string }
  >();

  for await (const chunk of stream) {
    const choice = chunk.choices[0];
    if (!choice) continue;

    const delta = choice.delta;

    if (delta?.content) {
      yield { type: "token", content: delta.content };
    }

    if (delta?.tool_calls) {
      for (const tc of delta.tool_calls) {
        const existing = toolCallMap.get(tc.index);
        if (existing) {
          if (tc.function?.arguments) {
            existing.arguments += tc.function.arguments;
          }
        } else {
          toolCallMap.set(tc.index, {
            id: tc.id ?? "",
            name: tc.function?.name ?? "",
            arguments: tc.function?.arguments ?? "",
          });
        }
      }
    }
  }

  // Emit accumulated tool calls. If a parse fails we still emit the call so
  // the orchestrator can produce a tool-error result back to the model
  // (every tool_call must have a matching tool_result, otherwise
  // the next turn errors out).
  if (toolCallMap.size > 0) {
    const calls: ToolCallResult[] = [];
    for (const [, tc] of toolCallMap) {
      try {
        const parsed = JSON.parse(tc.arguments) as Record<string, unknown>;
        calls.push({ id: tc.id, name: tc.name, arguments: parsed });
      } catch (e) {
        console.warn(
          `[ai] tool ${tc.name} returned malformed JSON arguments: ${
            e instanceof Error ? e.message : String(e)
          }`,
        );
        calls.push({
          id: tc.id,
          name: tc.name,
          arguments: { __parseError: tc.arguments },
        });
      }
    }
    if (calls.length > 0) {
      yield { type: "tool_calls", calls };
    }
  }

  yield { type: "done" };
}

// ----- Connection Test -----

/**
 * Issue a minimal chat completion against a Connection to confirm it
 * reaches a working server with valid credentials. Used by both the
 * Connection editor's "Test Connection" affordance and the onboarding
 * AI step.
 */
export async function testConnection(
  connection: Connection,
  model: string,
): Promise<{ ok: boolean; error?: string }> {
  try {
    const client = createAIClientForConnection(connection);
    await client.chat.completions.create({
      model,
      messages: [{ role: "user", content: "Hi" }],
    });
    return { ok: true };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

// ----- Markdown to HTML -----

export function markdownToBasicHtml(md: string): string {
  const raw = marked.parse(md, { async: false }) as string;
  return DOMPurify.sanitize(raw);
}
