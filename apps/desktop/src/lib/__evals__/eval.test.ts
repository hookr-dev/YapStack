/**
 * Eval harness for the Chat Tool layer. Each case in `./cases/` declares a
 * fixture (sessions, notes, segments, folders, tags) and a sequence of
 * Tool executions with expectations. The runner installs an in-memory DB
 * stub via vi.mock, then executes each step through the real
 * `executeTool` entry point — so tool selection, observation shape, and
 * mutation behavior are all checked end-to-end without spinning up a
 * real SQLite, real LLM, or real Tauri runtime.
 *
 * Adding cases: drop a new `*.json` into `./cases/`. Run with
 * `pnpm --filter @yapstack/desktop exec vitest run src/lib/__evals__`.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import type { EvalDbStub } from "./runner";
import type { EvalCase } from "./types";

declare global {
  var __evalStub: EvalDbStub | undefined;
}

vi.mock("@/lib/db", () => {
  const get =
    (name: string) =>
    (...args: unknown[]) => {
      const stub = globalThis.__evalStub;
      if (!stub) {
        throw new Error(`eval DB stub not initialized; tried ${name}`);
      }
      const fn = stub.module[name];
      if (typeof fn !== "function") {
        throw new Error(`eval DB stub missing function: ${name}`);
      }
      return (fn as (...a: unknown[]) => unknown)(...args);
    };
  return {
    listSessions: get("listSessions"),
    getSession: get("getSession"),
    getSessionsByIds: get("getSessionsByIds"),
    updateSessionTitle: get("updateSessionTitle"),
    togglePin: get("togglePin"),
    listFolders: get("listFolders"),
    listAllSessionFolders: get("listAllSessionFolders"),
    addSessionToFolder: get("addSessionToFolder"),
    removeSessionFromFolder: get("removeSessionFromFolder"),
    getNote: get("getNote"),
    saveNote: get("saveNote"),
    createNoteVersion: get("createNoteVersion"),
    getSessionSegments: get("getSessionSegments"),
    listTags: get("listTags"),
    getTagByName: get("getTagByName"),
    createTag: get("createTag"),
    addSessionTag: get("addSessionTag"),
    removeSessionTag: get("removeSessionTag"),
    getSessionTagIds: get("getSessionTagIds"),
    getSessionTagRows: get("getSessionTagRows"),
    searchSegments: get("searchSegments"),
    searchNotes: get("searchNotes"),
    searchSessionsByTitle: get("searchSessionsByTitle"),
    searchFolders: get("searchFolders"),
    searchDictations: get("searchDictations"),
    listDictationHistory: get("listDictationHistory"),
    updateSegmentText: get("updateSegmentText"),
  };
});

vi.mock("@/lib/db-backend", () => ({
  default: { load: vi.fn() },
}));

// Imports must come AFTER vi.mock blocks.
const { executeTool, getToolKind } = await import("@/lib/ai-tools");
type ToolContext = import("@/lib/ai-tools").ToolContext;
const { buildEvalDbStub } = await import("./runner");

const cases = import.meta.glob<EvalCase>("./cases/*.json", {
  eager: true,
  import: "default",
});

describe("Chat agent eval cases", () => {
  beforeEach(() => {
    globalThis.__evalStub = undefined;
  });

  for (const [path, evalCase] of Object.entries(cases)) {
    it(`${evalCase.name} (${path})`, async () => {
      const stub = buildEvalDbStub(evalCase.fixture);
      globalThis.__evalStub = stub;

      const defaultSessionId = stub.state.sessions[0]?.id ?? "";

      for (let i = 0; i < evalCase.steps.length; i++) {
        const step = evalCase.steps[i];
        const sessionId = step.ctxSessionId ?? defaultSessionId;
        const session = stub.state.sessions.find((s) => s.id === sessionId);
        const note = stub.state.notes.get(sessionId) ?? null;
        const folderIds = stub.state.sessionFolders
          .filter((sf) => sf.session_id === sessionId)
          .map((sf) => sf.folder_id);

        const ctx: ToolContext = {
          scope: "session",
          sessionId,
          currentTitle: session?.title ?? "",
          currentNote: note
            ? {
                id: `note-${sessionId}`,
                session_id: sessionId,
                content: note,
                updated_at: "",
              }
            : null,
          isPinned: session?.is_pinned === 1,
          folderIds,
          allowedSessionIds: step.ctxAllowedSessionIds,
          // `replace_in_transcript` reads ctx.segments; expose the live
          // mutable copy so a second tool call after it sees the edits.
          segments: stub.state.segments.get(sessionId) ?? [],
        };

        let result;
        let threw = false;
        try {
          result = await executeTool(step.tool, step.args, ctx);
        } catch {
          threw = true;
        }

        const where = `step ${i} (${step.tool})`;
        if (step.expect.throws) {
          expect(threw, where).toBe(true);
          continue;
        }
        expect(threw, where).toBe(false);
        expect(result, where).not.toBeNull();
        const r = result!;

        if (step.expect.observationContains !== undefined) {
          const haystack = `${r.observation?.summary ?? ""} ${r.observation?.evidence ?? ""} ${r.detail} ${r.result ?? ""}`;
          expect(haystack, where).toContain(step.expect.observationContains);
        }
        if (step.expect.affectedIdsIncludes) {
          for (const id of step.expect.affectedIdsIncludes) {
            expect(r.observation?.affectedIds ?? [], `${where}: includes ${id}`).toContain(id);
          }
        }
        if (step.expect.affectedIdsExcludes) {
          for (const id of step.expect.affectedIdsExcludes) {
            expect(r.observation?.affectedIds ?? [], `${where}: excludes ${id}`).not.toContain(id);
          }
        }
        if (step.expect.mutated !== undefined) {
          const isMutate =
            getToolKind(step.tool) === "mutate" && r.undoData !== undefined;
          expect(isMutate, `${where}: mutated`).toBe(step.expect.mutated);
        }
      }
    });
  }
});
