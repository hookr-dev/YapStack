import { useState, useRef, useEffect, useCallback, useMemo } from "react";
import type { AIContextValue } from "@/lib/ai-context";
import { assembleFolderTreeForActions } from "@/lib/ai-context";
import type { ChatMessage, FileAttachment, ToolExecution } from "@/lib/ai";
import type { ActionDefinition } from "@/lib/ai-actions";
import type {
  ExecutedTool,
  PersistedToolCall,
  ToolCallResult,
  ToolObservation,
} from "@/lib/ai-tools";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { DbChatMessage } from "@/lib/db";
import {
  getChatMessages,
  insertChatMessage,
  updateChatMessageContent,
  deleteChatMessages,
  getChatMessageById,
  markToolCallsAsUndone,
  getChatContextProfileId,
  clearChatContextProfile,
} from "@/lib/db";
import {
  resolveAndCreateClient,
  streamChatWithTools,
  DEFAULT_AI_CONFIG,
} from "@/lib/ai";
import { GENERAL_DIRECTIVE } from "@/lib/ai-prompts";
import { getToolsById, executeTool, undoToolCalls, getToolKind } from "@/lib/ai-tools";
import { assembleHistoryForRequest } from "@/lib/chat-history";
import { useAppStore } from "@/stores/appStore";
import { toast } from "sonner";
import {
  trackChatMessageSent,
  trackChatToolExecuted,
  trackChatToolUndone,
  trackChatCleared,
} from "@/lib/analytics";

/**
 * Fold persisted rows into renderer-facing ChatMessages.
 *
 * One DB row per LLM response (v14 shape) means a multi-round send is N
 * assistant rows + M tool rows. The renderer collapses each `send_id` group
 * back into one assistant bubble: the final prose comes from the last
 * assistant row, the tool-execution chips are aggregated from every
 * assistant row's `tool_calls` JSON across the send, and tool rows are
 * dropped (their content is replayed to the LLM via `tool_call_id` matching;
 * the UI doesn't render them as standalone bubbles).
 *
 * Pre-v14 rows have `send_id` null; the helper falls back to per-row
 * grouping so legacy chats still render correctly.
 */
function dbRowsToChatMessages(rows: DbChatMessage[]): ChatMessage[] {
  const bySendId = new Map<string, DbChatMessage[]>();
  const order: string[] = [];
  for (const row of rows) {
    const key = row.send_id ?? row.id;
    let group = bySendId.get(key);
    if (!group) {
      group = [];
      bySendId.set(key, group);
      order.push(key);
    }
    group.push(row);
  }

  const out: ChatMessage[] = [];
  for (const key of order) {
    const group = bySendId.get(key)!;
    const userRow = group.find((r) => r.role === "user");
    if (userRow) {
      out.push({
        id: userRow.id,
        role: "user",
        content: userRow.content,
        action: userRow.action ?? undefined,
      });
    }

    const assistantRows = group.filter((r) => r.role === "assistant");
    if (assistantRows.length === 0) continue;

    const finalAssistant = assistantRows[assistantRows.length - 1];
    const toolExecutions: ToolExecution[] = [];
    for (const ar of assistantRows) {
      if (!ar.tool_calls) continue;
      try {
        const parsed = JSON.parse(ar.tool_calls) as PersistedToolCall[];
        if (!Array.isArray(parsed)) continue;
        for (const p of parsed) {
          toolExecutions.push({
            name: p.name,
            label: p.label,
            status: p.status,
            detail: p.detail,
            undone: p.undone === true,
          });
        }
      } catch (e) {
        console.warn(`[chat] failed to parse tool_calls JSON for ${ar.id}: ${e}`);
      }
    }

    out.push({
      id: finalAssistant.id,
      role: "assistant",
      content: cleanToolBadges(finalAssistant.content),
      action: finalAssistant.action ?? undefined,
      toolExecutions: toolExecutions.length > 0 ? toolExecutions : undefined,
    });
  }

  return out;
}

const UNDO_TIMEOUT_MS = 10_000;

const TOOL_DISPLAY_LABELS: Record<string, string> = {
  update_title: "Updating title",
  save_to_notes: "Saving notes",
  pin_session: "Pinning session",
  tag_session: "Adding tags",
  search_folders: "Searching folders",
  add_session_to_folder: "Classifying session",
  search_sessions: "Searching sessions",
  get_session_context: "Reading sessions",
  search_dictations: "Searching dictations",
  replace_in_transcript: "Editing transcript",
};

function cleanToolBadges(content: string): string {
  return content.split("\n").filter((l) => !/^\[tool:\w+\]/.test(l)).join("\n").trimStart();
}

export interface UndoState {
  messageId: string;
  executed: ExecutedTool[];
  timer: ReturnType<typeof setTimeout>;
}

export interface UseChatMessagesReturn {
  messages: ChatMessage[];
  isStreaming: boolean;
  undoState: UndoState | null;
  handleSend: (actionDef?: ActionDefinition) => Promise<void>;
  handleUndo: (state: UndoState) => Promise<void>;
  handleClearChat: () => Promise<void>;
}

/** Manages AI chat message state, streaming, tool execution, and DB persistence. */
export function useChatMessages(
  ctx: AIContextValue | null,
  input: string,
  setInput: (value: string) => void,
  attachments: FileAttachment[],
  setIsExpanded: (value: boolean) => void,
): UseChatMessagesReturn {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [isStreaming, setIsStreaming] = useState(false);
  const [undoState, setUndoState] = useState<UndoState | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const messagesRef = useRef(messages);
  messagesRef.current = messages;
  const inputRef = useRef(input);
  inputRef.current = input;
  // Synchronous reentrancy guard. `isStreaming` (React state) doesn't flip in
  // the DOM until the next commit, so a double-click on Summarize or any
  // other action item fires `handleSend` twice before the disabled attribute
  // applies. The ref blocks the second call in the same tick.
  const sendingRef = useRef(false);

  const aiConfig = useAppStore((s) => s.settings.aiConfig) ?? DEFAULT_AI_CONFIG;

  const contextKey = ctx?.contextKey ?? "";
  // Memoize the empty fallback so identity is stable across renders — without
  // this `[] !== []` makes every render bust the useCallback deps below.
  const sources = useMemo(() => ctx?.sources ?? [], [ctx?.sources]);
  const ctxTools = ctx?.tools;
  const buildSystemPrompt = ctx?.buildSystemPrompt;
  const isSessionContext = ctx?.isSessionContext ?? false;
  const sessionId = ctx?.sessionId ?? null;
  const onToolsExecuted = ctx?.onToolsExecuted;

  // Load messages from DB on context change
  useEffect(() => {
    if (!contextKey) return;
    setIsStreaming(false);
    if (undoState) {
      clearTimeout(undoState.timer);
      setUndoState(null);
    }
    getChatMessages(contextKey).then((rows) => {
      setMessages(dbRowsToChatMessages(rows));
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [contextKey]);

  const reloadFromDb = useCallback(async () => {
    if (!contextKey) return;
    try {
      const rows = await getChatMessages(contextKey);
      setMessages(dbRowsToChatMessages(rows));
    } catch (e) {
      console.error(`[chat] failed to reload messages after sync: ${e}`);
    }
  }, [contextKey]);

  /**
   * B2: chat_messages is a synced table, but this hook only ever read it on
   * context change — a conversation continued on another device never appeared
   * until the chat was reopened. `syncAppliedSeq` is the store's counter of
   * committed sync-applied batches, so we re-read on exactly those batches.
   *
   * Never while a send is in flight: the streaming bubble lives in React state
   * and is persisted only after the round completes, so a mid-send reload would
   * wipe the visible response. The skipped reload is remembered and drained in
   * `handleSend`'s finally.
   */
  const syncAppliedSeq = useAppStore((s) => s.syncAppliedSeq);
  const pendingSyncReloadRef = useRef(false);
  useEffect(() => {
    if (!contextKey || syncAppliedSeq === 0) return;
    if (sendingRef.current) {
      pendingSyncReloadRef.current = true;
      return;
    }
    void reloadFromDb();
  }, [syncAppliedSeq, contextKey, reloadFromDb]);

  // Abort streaming on unmount
  useEffect(() => {
    return () => {
      abortRef.current?.abort();
    };
  }, []);

  const handleUndo = useCallback(
    async (state: UndoState) => {
      if (!isSessionContext || !sessionId) return;
      clearTimeout(state.timer);
      try {
        await undoToolCalls(state.executed, { sessionId });
        for (const t of state.executed) {
          trackChatToolUndone({ tool_name: t.name });
        }

        // Mark the persisted tool-call records as undone — the chip stays
        // visible (rendered grayed/struck-through) so the user sees a
        // receipt of what was attempted, and the model sees the tool
        // result content rewritten to "(undone by user)" on the next
        // turn so it understands the action no longer stands.
        const callIds = state.executed
          .map((t) => t.toolCallId)
          .filter((id): id is string => !!id);
        const ref = await getChatMessageById(state.messageId);
        if (ref?.send_id && callIds.length > 0) {
          await markToolCallsAsUndone(
            ref.context_key,
            ref.send_id,
            callIds,
          );
        }

        // Strip the legacy `[tool:NAME] …` text prefix on the assistant
        // row (no-op for v14 rows, kept for old chats). Then refresh
        // in-memory state from DB so the UI reflects the cleaned send.
        const msg = messagesRef.current.find((m) => m.id === state.messageId);
        if (msg) {
          await updateChatMessageContent(
            state.messageId,
            cleanToolBadges(msg.content),
          );
        }
        const refreshed = await getChatMessages(contextKey);
        setMessages(dbRowsToChatMessages(refreshed));

        if (onToolsExecuted) {
          await onToolsExecuted(state.executed.map((t) => t.name));
        }
        toast.success("Reverted");
      } catch (e) {
        toast.error(
          `Failed to undo: ${e instanceof Error ? e.message : String(e)}`,
        );
      }
      setUndoState(null);
    },
    [contextKey, isSessionContext, sessionId, onToolsExecuted],
  );

  const handleClearChat = useCallback(async () => {
    if (!contextKey) return;
    if (undoState) {
      clearTimeout(undoState.timer);
      setUndoState(null);
    }
    await deleteChatMessages(contextKey);
    trackChatCleared();
    setMessages([]);
    setIsExpanded(false);
  }, [contextKey, undoState, setIsExpanded]);

  const handleSend = useCallback(
    async (actionDef?: ActionDefinition) => {
      if (!buildSystemPrompt || !ctxTools) return;

      const userText = inputRef.current.trim();
      const directive = actionDef?.directive ?? GENERAL_DIRECTIVE;
      const actionId = actionDef?.id ?? "general";

      if (!actionDef && !userText) return;
      if (sendingRef.current) return;
      sendingRef.current = true;

      // For action invocations with no typed text, send the action's
      // verbose userPrompt as the model-visible content (falls back to
      // the label). The chat UI still renders the action chip via
      // `message.action`, so the user only sees "Summarize" while the
      // model sees an unambiguous instruction. Without this, a clicked
      // action mid-conversation reads to the model as a one-word
      // follow-up and triggers clarification questions.
      const actionUserContent =
        actionDef && !userText
          ? (actionDef.userPrompt ?? actionDef.label)
          : userText;
      const userMessage: ChatMessage = {
        id: crypto.randomUUID(),
        role: "user",
        content: actionUserContent,
        action: actionId,
      };

      const assistantMessage: ChatMessage = {
        id: crypto.randomUUID(),
        role: "assistant",
        content: "",
        isStreaming: true,
      };

      setMessages((prev) => [...prev, userMessage, assistantMessage]);
      setInput("");
      setIsStreaming(true);
      setIsExpanded(true);
      trackChatMessageSent({
        context: contextKey.split(":")[0] ?? "unknown",
        has_action: actionDef ? 1 : 0,
        action_id: actionId,
      });

      let flushTimer: ReturnType<typeof setInterval> | null = null;

      // Captures one LLM response and the tool executions that followed it.
      // After the loop completes (or errors), the orchestrator walks this
      // list and writes one assistant row per round plus one tool row per
      // call, matching the OpenAI message-array shape on disk so a future
      // turn can replay the full conversation including tool memory.
      type RoundRecord = {
        assistantRowId: string;
        content: string;
        toolCalls: PersistedToolCall[];
        toolResults: {
          rowId: string;
          callId: string;
          content: string;
          observation: ToolObservation | undefined;
          status: "done" | "error";
        }[];
      };

      const sendId = crypto.randomUUID();
      const persistedRounds: RoundRecord[] = [];

      try {
        // Persist the user row inside the try so any DB error (a missing
        // column on a stale dev DB, a locked file, etc.) routes through the
        // catch + finally and clears `isStreaming` instead of surfacing as
        // an unhandled rejection that strands the spinner. The assistant
        // and tool rows are written after the loop completes — local React
        // state (`messages`) drives the streaming UI in the meantime.
        await insertChatMessage({
          id: userMessage.id,
          context_key: contextKey,
          session_id: sessionId,
          role: "user",
          content: userMessage.content,
          action: userMessage.action ?? null,
          created_at: new Date().toISOString(),
          tool_calls: null,
          send_id: sendId,
          sequence: 0,
          tool_call_id: null,
          observation: null,
          status: null,
        });

        // Resolve which Profile to use for THIS send. AI-action sends
        // (Summarize / Key points / etc.) bind to `aiActionsProfileId`;
        // ordinary chat sends prefer the per-chat override in
        // chat_context_settings, falling back to `chatProfileId`. No
        // fallback chain on resolution failure — resolveAndCreateClient
        // throws and the outer catch surfaces the message as an in-chat
        // error row so the user can act on it.
        let profileId: string | null;
        if (actionDef) {
          profileId = aiConfig.assignments.aiActionsProfileId;
        } else {
          const override = await getChatContextProfileId(contextKey);
          // Self-heal a dangling override: settings (Profiles) and SQLite (this
          // override) are separate stores with no FK, and delete-time cleanup is
          // best-effort. If the override points at a Profile that no longer
          // exists, ignore it — fall back to the live Chat assignment — and
          // clear the stale row so chat doesn't keep erroring on a dead Profile.
          const overrideValid =
            override !== null &&
            aiConfig.profiles.some((p) => p.id === override);
          if (override !== null && !overrideValid) {
            void clearChatContextProfile(contextKey).catch(() => {});
          }
          profileId = overrideValid
            ? override
            : aiConfig.assignments.chatProfileId;
        }
        const { client, model: resolvedModel } = resolveAndCreateClient(
          aiConfig,
          profileId,
        );
        const abort = new AbortController();
        abortRef.current = abort;

        let accumulated = "";
        const executedTools: ExecutedTool[] = [];
        const allToolExecs: ToolExecution[] = [];
        const allCallIds: string[] = [];
        const observationsByCallId = new Map<string, ToolObservation>();
        // Read persisted rows for the chat and rebuild the OpenAI-shaped
        // message array including prior `assistant.tool_calls` and
        // `tool` rows. This is what makes follow-ups like "use the
        // sessions you just found" work — the model sees the previous
        // round's tool memory instead of just the assistant prose.
        // The user row for THIS send was just persisted above, so the
        // assembler picks it up naturally; we don't need to push it
        // separately.
        const persistedRows = await getChatMessages(contextKey);
        const priorHistory = assembleHistoryForRequest(persistedRows);

        const contextParts: Record<string, string> = {};
        for (const source of sources) {
          if (source.enabled) {
            contextParts[source.id] = await source.assembler();
          }
        }

        if (actionDef && isSessionContext) {
          const folderTree = await assembleFolderTreeForActions();
          if (folderTree) {
            contextParts["folder-tree"] = folderTree;
          }
        }

        const systemPrompt = await buildSystemPrompt(
          directive,
          contextParts,
          attachments,
        );

        const chatMessages: ChatCompletionMessageParam[] = [
          { role: "system", content: systemPrompt },
          ...priorHistory,
        ];

        const tools = getToolsById(ctxTools.availableToolIds);

        let needsFlush = false;
        const flush = () => {
          if (!needsFlush) return;
          needsFlush = false;
          const current = accumulated;
          setMessages((prev) =>
            prev.map((m) =>
              m.id === assistantMessage.id
                ? { ...m, content: current }
                : m,
            ),
          );
        };
        flushTimer = setInterval(flush, 50);

        // Multi-turn tool execution: after each LLM turn, if it emits tool_calls,
        // execute them and send results back for a follow-up turn. This lets the
        // LLM use tool results (e.g., folder context from add_session_to_folder)
        // to inform subsequent output (e.g., writing a summary).
        //
        // The cap allows up to MAX_TOOL_ROUNDS tool-calling rounds; after that,
        // one final round runs without tools so the model is forced to
        // synthesize a textual answer over the accumulated observations
        // instead of leaving the user with a row of tool badges and no prose.
        const MAX_TOOL_ROUNDS = 5;
        let turnMessages = chatMessages;

        for (let round = 0; round <= MAX_TOOL_ROUNDS; round++) {
          const allowToolsThisRound = round < MAX_TOOL_ROUNDS;
          const turnTools = allowToolsThisRound ? tools : [];
          let turnText = "";
          let turnToolCalls: ToolCallResult[] = [];

          for await (const event of streamChatWithTools(
            client,
            resolvedModel,
            turnMessages,
            turnTools,
            abort.signal,
          )) {
            if (event.type === "token") {
              accumulated += event.content;
              turnText += event.content;
              needsFlush = true;
            } else if (event.type === "tool_calls") {
              turnToolCalls = event.calls;
            }
          }

          // Open a record for this round. Even rounds with no tool calls
          // (the final synthesis turn) become an assistant row so the prose
          // is persisted; rounds with tool calls also persist tool rows.
          const roundRecord: RoundRecord = {
            assistantRowId: crypto.randomUUID(),
            content: turnText,
            toolCalls: [],
            toolResults: [],
          };
          persistedRounds.push(roundRecord);

          if (turnToolCalls.length === 0 || !ctxTools.getToolContext) break;
          if (!allowToolsThisRound) break;
          if (abort.signal.aborted) break;

          const pendingExecs: ToolExecution[] = turnToolCalls.map((c) => ({
            name: c.name,
            label: TOOL_DISPLAY_LABELS[c.name] ?? c.name,
            status: "running" as const,
          }));
          allToolExecs.push(...pendingExecs);
          allCallIds.push(...turnToolCalls.map((c) => c.id));
          // Slice point so we can fire `onToolsExecuted` for the tools that
          // ran *this round* once the inner loop finishes — the model's next
          // round sees a fresh world via `getToolContext()` (line below) and
          // the UI sees it via the per-round refresh after the loop. Avoids
          // a "batched at end" model where errors mid-conversation leave
          // committed DB writes invisible to the store.
          const roundStart = executedTools.length;

          setMessages((prev) =>
            prev.map((m) =>
              m.id === assistantMessage.id
                ? { ...m, content: accumulated, toolExecutions: [...allToolExecs] }
                : m,
            ),
          );

          // Re-fetch between mutating calls inside the same batch — see the
          // refresh after `executeTool` below — so the second mutation can
          // observe the first one's pre-state (folder memberships, tag rows,
          // pin state).
          let toolCtx = await ctxTools.getToolContext();
          const toolResultMessages: ChatCompletionMessageParam[] = [];

          const assistantToolCalls = turnToolCalls.map((call) => ({
            id: call.id,
            type: "function" as const,
            function: { name: call.name, arguments: JSON.stringify(call.arguments) },
          }));

          // Stub the persisted tool_calls list now so the assistant row
          // carries one entry per call. The execution loop below fills in
          // status/detail/observation as each call resolves.
          roundRecord.toolCalls = turnToolCalls.map((call) => ({
            id: call.id,
            name: call.name,
            arguments: JSON.stringify(call.arguments),
            label: TOOL_DISPLAY_LABELS[call.name] ?? call.name,
            status: "done",
          }));

          turnMessages = [
            ...turnMessages,
            {
              role: "assistant" as const,
              content: turnText || null,
              tool_calls: assistantToolCalls,
            },
          ];

          for (let ci = 0; ci < turnToolCalls.length; ci++) {
            const call = turnToolCalls[ci];
            if (abort.signal.aborted) break;
            const execIdx = allToolExecs.length - turnToolCalls.length + ci;
            // streamChatWithTools surfaces malformed JSON arguments by
            // attaching `__parseError`. Skip execution and tell the model
            // so it can retry instead of running a tool with garbage args.
            if (call.arguments.__parseError !== undefined) {
              const errMsg = `Invalid JSON for ${call.name}: ${String(call.arguments.__parseError)}`;
              allToolExecs[execIdx] = {
                ...allToolExecs[execIdx],
                status: "error",
                detail: errMsg,
              };
              const ptcEntry = roundRecord.toolCalls.find((p) => p.id === call.id);
              if (ptcEntry) {
                ptcEntry.status = "error";
                ptcEntry.detail = errMsg;
              }
              toolResultMessages.push({
                role: "tool" as const,
                tool_call_id: call.id,
                content: `Error: ${errMsg}`,
              });
              roundRecord.toolResults.push({
                rowId: crypto.randomUUID(),
                callId: call.id,
                content: `Error: ${errMsg}`,
                observation: undefined,
                status: "error",
              });
              continue;
            }
            try {
              const result = await executeTool(
                call.name,
                call.arguments,
                toolCtx,
              );
              // After a mutating tool runs, the snapshot the orchestrator
              // captured at the start of this batch (folder ids, tag rows,
              // pin state) is stale. Refresh before the next call so the
              // following tool sees the post-mutation world. Read-only tools
              // can't move state, so skip the round trip for them.
              if (
                getToolKind(call.name) === "mutate" &&
                ctxTools.getToolContext &&
                ci + 1 < turnToolCalls.length
              ) {
                toolCtx = await ctxTools.getToolContext();
              }
              if (result) {
                result.toolCallId = call.id;
                trackChatToolExecuted({ tool_name: call.name });
                allToolExecs[execIdx] = { ...allToolExecs[execIdx], status: "done", detail: result.detail };
                if (result.observation) {
                  observationsByCallId.set(call.id, result.observation);
                }
                const ptcEntry = roundRecord.toolCalls.find((p) => p.id === call.id);
                if (ptcEntry) {
                  ptcEntry.status = "done";
                  ptcEntry.detail = result.detail;
                  if (result.observation) ptcEntry.observation = result.observation;
                }
                // Read-only tools never enter the Undo window. Mutating
                // tools that returned without populating undoData (no-op
                // paths like pin_session-already-pinned, add_session_to_folder-
                // already-in-folder, tag_session-no-real-deltas) also skip
                // the undo state — the LLM still sees the result via
                // toolResultMessages, but the UI doesn't surface a
                // "Session updated" toast for a no-op.
                if (
                  getToolKind(call.name) === "mutate" &&
                  result.undoData !== undefined
                ) {
                  executedTools.push(result);
                }
                const toolMsgContent = result.result ?? result.detail;
                toolResultMessages.push({
                  role: "tool" as const,
                  tool_call_id: call.id,
                  content: toolMsgContent,
                });
                roundRecord.toolResults.push({
                  rowId: crypto.randomUUID(),
                  callId: call.id,
                  content: toolMsgContent,
                  observation: result.observation,
                  status: "done",
                });
              } else {
                allToolExecs[execIdx] = { ...allToolExecs[execIdx], status: "done" };
                toolResultMessages.push({
                  role: "tool" as const,
                  tool_call_id: call.id,
                  content: "No action needed.",
                });
                roundRecord.toolResults.push({
                  rowId: crypto.randomUUID(),
                  callId: call.id,
                  content: "No action needed.",
                  observation: undefined,
                  status: "done",
                });
              }
            } catch (e) {
              console.error(`Tool ${call.name} failed:`, e);
              const errMsg = e instanceof Error ? e.message : String(e);
              allToolExecs[execIdx] = {
                ...allToolExecs[execIdx],
                status: "error",
                detail: errMsg,
              };
              const ptcEntry = roundRecord.toolCalls.find((p) => p.id === call.id);
              if (ptcEntry) {
                ptcEntry.status = "error";
                ptcEntry.detail = errMsg;
              }
              toast.error(`Tool failed: ${errMsg}`);
              toolResultMessages.push({
                role: "tool" as const,
                tool_call_id: call.id,
                content: `Error: ${errMsg}`,
              });
              roundRecord.toolResults.push({
                rowId: crypto.randomUUID(),
                callId: call.id,
                content: `Error: ${errMsg}`,
                observation: undefined,
                status: "error",
              });
            }

            setMessages((prev) =>
              prev.map((m) =>
                m.id === assistantMessage.id
                  ? { ...m, toolExecutions: [...allToolExecs] }
                  : m,
              ),
            );
          }

          turnMessages = [...turnMessages, ...toolResultMessages];

          const roundExecuted = executedTools.slice(roundStart);
          if (roundExecuted.length > 0 && onToolsExecuted) {
            onToolsExecuted(roundExecuted.map((t) => t.name)).catch((err) => {
              console.error(`[chat] tool refresh failed: ${err}`);
            });
          }
        }

        clearInterval(flushTimer);
        flush();

        const finalToolExecs = allToolExecs.length > 0 ? [...allToolExecs] : undefined;
        setMessages((prev) =>
          prev.map((m) =>
            m.id === assistantMessage.id
              ? { ...m, content: accumulated, isStreaming: false, toolExecutions: finalToolExecs }
              : m,
          ),
        );

        // Persist per-LLM-response rows in (send_id, sequence) order. Each
        // round's assistant row carries that round's emitted tool_calls
        // (`PersistedToolCall[]` JSON, including stable OpenAI call IDs and
        // the raw arguments string), and each tool result becomes its own
        // `role='tool'` row whose `tool_call_id` matches an entry on the
        // preceding assistant row. Reload + replay reconstructs the
        // OpenAI message array verbatim from these rows.
        let nextSeq = 1;
        const writes: Promise<void>[] = [];
        const nowIso = new Date().toISOString();
        for (let r = 0; r < persistedRounds.length; r++) {
          const round = persistedRounds[r];
          const isLastRound = r === persistedRounds.length - 1;
          // Use the local React assistantMessage.id for the final round so
          // the in-memory bubble's id matches the persisted row that the
          // renderer fold will surface as the assistant ChatMessage. Other
          // rounds get fresh UUIDs.
          const assistantId = isLastRound ? assistantMessage.id : round.assistantRowId;
          writes.push(
            insertChatMessage({
              id: assistantId,
              context_key: contextKey,
              session_id: sessionId,
              role: "assistant",
              content: round.content,
              action: null,
              created_at: nowIso,
              tool_calls:
                round.toolCalls.length > 0
                  ? JSON.stringify(round.toolCalls)
                  : null,
              send_id: sendId,
              sequence: nextSeq++,
              tool_call_id: null,
              observation: null,
              status: null,
            }),
          );
          for (const tr of round.toolResults) {
            writes.push(
              insertChatMessage({
                id: tr.rowId,
                context_key: contextKey,
                session_id: sessionId,
                role: "tool",
                content: tr.content,
                action: null,
                created_at: nowIso,
                tool_calls: null,
                send_id: sendId,
                sequence: nextSeq++,
                tool_call_id: tr.callId,
                observation: tr.observation
                  ? JSON.stringify(tr.observation)
                  : null,
                status: tr.status,
              }),
            );
          }
        }
        await Promise.all(writes);

        if (executedTools.length > 0) {
          const toolNames = executedTools.map((t) => t.label).join(", ");

          const timer = setTimeout(() => {
            setUndoState(null);
          }, UNDO_TIMEOUT_MS);

          const newUndoState: UndoState = {
            messageId: assistantMessage.id,
            executed: executedTools,
            timer,
          };
          setUndoState(newUndoState);

          toast(`Session updated: ${toolNames}`, {
            action: {
              label: "Undo",
              onClick: () => handleUndo(newUndoState),
            },
            duration: UNDO_TIMEOUT_MS,
          });
        }
      } catch (e) {
        if ((e as Error).name === "AbortError") return;
        const errorText =
          e instanceof Error ? e.message : "An error occurred";
        setMessages((prev) =>
          prev.map((m) =>
            m.id === assistantMessage.id
              ? { ...m, content: `Error: ${errorText}`, isStreaming: false }
              : m,
          ),
        );
        // No placeholder assistant row to update — insert a fresh one so
        // the error survives reload. `sequence: 1` slots it right after the
        // user row in the same send group.
        await insertChatMessage({
          id: assistantMessage.id,
          context_key: contextKey,
          session_id: sessionId,
          role: "assistant",
          content: `Error: ${errorText}`,
          action: null,
          created_at: new Date().toISOString(),
          tool_calls: null,
          send_id: sendId,
          sequence: 1,
          tool_call_id: null,
          observation: null,
          status: null,
        }).catch((insertErr) => {
          console.error(`[chat] failed to persist error row: ${insertErr}`);
        });
      } finally {
        if (flushTimer) clearInterval(flushTimer);
        setIsStreaming(false);
        abortRef.current = null;
        sendingRef.current = false;
        // Drain a sync-applied reload this send suppressed (B2).
        if (pendingSyncReloadRef.current) {
          pendingSyncReloadRef.current = false;
          void reloadFromDb();
        }
      }
    },
    [
      contextKey,
      reloadFromDb,
      sessionId,
      isSessionContext,
      sources,
      ctxTools,
      attachments,
      aiConfig,
      buildSystemPrompt,
      onToolsExecuted,
      handleUndo,
      setInput,
      setIsExpanded,
    ],
  );

  return {
    messages,
    isStreaming,
    undoState,
    handleSend,
    handleUndo,
    handleClearChat,
  };
}
