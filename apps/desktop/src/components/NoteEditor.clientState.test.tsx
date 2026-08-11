import { render, screen, cleanup, fireEvent, waitFor } from "@testing-library/react";
import { describe, it, expect, vi, beforeAll, beforeEach, afterEach } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";

const { getNoteMock, saveNoteMock } = vi.hoisted(() => ({
  getNoteMock: vi.fn(),
  saveNoteMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => ({
  toast: { info: vi.fn(), error: vi.fn(), warning: vi.fn(), success: vi.fn() },
}));
vi.mock("@/lib/logger", () => ({
  log: { info: vi.fn(), warn: vi.fn(), error: vi.fn(), debug: vi.fn() },
}));
vi.mock("@/lib/db", async () => {
  const actual = await vi.importActual<typeof import("@/lib/db")>("@/lib/db");
  return {
    ...actual,
    getNote: getNoteMock,
    saveNote: saveNoteMock,
    getSessionSegments: vi.fn().mockResolvedValue([]),
  };
});

import { useAppStore } from "@/stores/appStore";
import { NoteEditor } from "./NoteEditor";
import { TooltipProvider } from "@/components/ui/tooltip";
import { toast } from "sonner";

function editorDom(): HTMLElement {
  return document.querySelector(".tiptap-editor") as HTMLElement;
}

/** Low-level ProseMirror keystroke: mutate the DOM and let the view observer
 * turn it into a transaction (this is what fires `onUpdate`). */
async function typeInEditor(text: string, sessionId: string) {
  const dom = editorDom();
  const p = dom.querySelector("p") ?? dom;
  p.textContent = text;
  fireEvent.input(dom, { bubbles: true });
  await waitFor(() =>
    expect(useAppStore.getState().noteEditingSessionId).toBe(sessionId),
  );
}

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

beforeAll(() => {
  const empty = Object.assign([], { item: () => null }) as unknown as DOMRectList;
  Range.prototype.getClientRects = () => empty;
  Range.prototype.getBoundingClientRect = () => new DOMRect();
});

beforeEach(() => {
  vi.clearAllMocks();
  useAppStore.setState({ noteEditingSessionId: null, remoteNoteUpdate: null });
});

afterEach(() => cleanup());

describe("NoteEditor — save that resolves after a session switch (baseline clobber)", () => {
  it("does not overwrite session B's baselines with session A's saved HTML", async () => {
    // Per-id note content; A's save is parked until the test releases it.
    getNoteMock.mockImplementation((id: string) =>
      id === "A"
        ? Promise.resolve({ content: "<p>note A</p>" })
        : Promise.resolve({ content: "<p>note B</p>" }),
    );
    const saveA = deferred<void>();
    saveNoteMock.mockImplementation((id: string) =>
      id === "A" ? saveA.promise : Promise.resolve(undefined),
    );

    const { rerender } = render(
      <TooltipProvider>
        <NoteEditor sessionId="A" />
      </TooltipProvider>,
    );
    await screen.findByText("note A");

    // (1) User edits A, then blurs → persistIfChanged("<p>edited A</p>") runs
    //     getNote(A) then saveNote(A), which parks in-flight.
    await typeInEditor("edited A", "A");
    fireEvent.blur(editorDom());
    await waitFor(() =>
      expect(saveNoteMock).toHaveBeenCalledWith("A", expect.stringContaining("edited A")),
    );

    // (2) User switches to session B on the SAME editor instance; its load
    //     effect applies B's content and sets B's baselines.
    rerender(
      <TooltipProvider>
        <NoteEditor sessionId="B" />
      </TooltipProvider>,
    );
    await screen.findByText("note B");

    // (3) The still-in-flight A save now resolves — on the current tree it
    //     writes lastSavedContent/docBaseline = A's HTML, clobbering B's.
    saveA.resolve();
    await waitFor(() =>
      expect(saveNoteMock.mock.calls.some((c) => c[0] === "A")).toBe(true),
    );
    // Let the resolved persist continuation run its baseline writes.
    await Promise.resolve();
    await Promise.resolve();

    // (4) A peer edit arrives for the open note B. With intact baselines the
    //     A1c apply guard (editor.getHTML() === docBaseline) passes and the
    //     editor shows the peer text. On the clobbered tree docBaseline holds
    //     A's HTML, the guard rejects it, and B keeps showing stale text.
    useAppStore.setState({
      noteEditingSessionId: null,
      remoteNoteUpdate: { sessionId: "B", content: "<p>peer B edit</p>", seq: 1 },
    });

    // FAILS on the unmodified tree: the update is rejected and "note B" stays.
    await screen.findByText("peer B edit", undefined, { timeout: 2000 });
  });

  // Positive control (PASSES now): identical flow, but A's save completes BEFORE
  // the switch, so no stale continuation clobbers B. Proves the red test above
  // fails specifically because of the late continuation, not because a peer edit
  // can never apply after a session switch.
  it("applies a peer edit for B when A's save completed before the switch (control)", async () => {
    getNoteMock.mockImplementation((id: string) =>
      id === "A"
        ? Promise.resolve({ content: "<p>note A</p>" })
        : Promise.resolve({ content: "<p>note B</p>" }),
    );
    saveNoteMock.mockResolvedValue(undefined);

    const { rerender } = render(
      <TooltipProvider>
        <NoteEditor sessionId="A" />
      </TooltipProvider>,
    );
    await screen.findByText("note A");

    await typeInEditor("edited A", "A");
    fireEvent.blur(editorDom());
    await waitFor(() =>
      expect(saveNoteMock).toHaveBeenCalledWith("A", expect.stringContaining("edited A")),
    );
    // A's save already resolved (not parked) before we switch.
    await Promise.resolve();

    rerender(
      <TooltipProvider>
        <NoteEditor sessionId="B" />
      </TooltipProvider>,
    );
    await screen.findByText("note B");

    useAppStore.setState({
      noteEditingSessionId: null,
      remoteNoteUpdate: { sessionId: "B", content: "<p>peer B edit</p>", seq: 1 },
    });

    await screen.findByText("peer B edit", undefined, { timeout: 2000 });
  });
});

describe("NoteEditor — a failed note save must surface to the user", () => {
  it("reports a rejected save instead of swallowing it silently", async () => {
    getNoteMock.mockResolvedValue({ content: "<p>original</p>" });
    // The debounced autosave exhausts its lock budget and rejects.
    saveNoteMock.mockRejectedValue(new Error("database is locked"));

    render(
      <TooltipProvider>
        <NoteEditor sessionId="s1" />
      </TooltipProvider>,
    );
    await screen.findByText("original");

    await typeInEditor("unsaved words", "s1");
    // Flush via blur so we don't wait on the 1s debounce.
    fireEvent.blur(editorDom());

    await waitFor(() => expect(saveNoteMock).toHaveBeenCalled());

    // FAILS on the unmodified tree: persistIfChanged's catch only console.errors,
    // so the user gets no toast/log surfacing that their note did not save.
    await waitFor(() => expect(toast.error).toHaveBeenCalled());
  });
});

describe("NoteEditor — a local note-write (e.g. AI summarize) must not be clobbered", () => {
  it("yields to a local write instead of overwriting it and mislabeling it a peer edit", async () => {
    // First getNote = the initial load; the SECOND (the in-flight persistIfChanged's
    // read) is parked so we can inject the local AI write before the save decides.
    const persistRead = deferred<{ content: string } | null>();
    let loaded = false;
    getNoteMock.mockImplementation(() => {
      if (!loaded) {
        loaded = true;
        return Promise.resolve({ content: "<p>original</p>" });
      }
      return persistRead.promise;
    });
    saveNoteMock.mockResolvedValue(undefined);

    render(
      <TooltipProvider>
        <NoteEditor sessionId="s1" refreshKey={0} />
      </TooltipProvider>,
    );
    await screen.findByText("original");

    // User types an unsaved edit, then blurs → persistIfChanged captures the refresh
    // counter and parks on getNote(s1).
    await typeInEditor("stale edit", "s1");
    fireEvent.blur(editorDom());
    await waitFor(() =>
      expect(getNoteMock.mock.calls.length).toBeGreaterThanOrEqual(2),
    );

    // The AI `save_to_notes` tool writes the summary and NoteDetailView.handleToolsExecuted
    // bumps noteRefreshCounter (incrementNoteRefresh). Model exactly that: the DB now holds
    // the summary, and the local-write signal is raised, THEN our parked read resolves.
    useAppStore.setState((s) => ({
      noteRefreshCounter: s.noteRefreshCounter + 1,
    }));
    persistRead.resolve({ content: "<p>AI SUMMARY</p>" });
    await new Promise((r) => setTimeout(r, 0));
    await Promise.resolve();

    // FAILS on the unmodified tree: persistIfChanged writes the editor's stale edit over
    // the summary and toasts "another device". The fix yields to the pending reload.
    expect(saveNoteMock).not.toHaveBeenCalledWith(
      "s1",
      expect.stringContaining("stale edit"),
    );
    expect(toast.info).not.toHaveBeenCalledWith(
      expect.stringContaining("another device"),
    );
  });
});
