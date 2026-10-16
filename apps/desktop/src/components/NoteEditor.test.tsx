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
import { log } from "@/lib/logger";

function renderEditor(sessionId = "s1") {
  return render(
    <TooltipProvider>
      <NoteEditor sessionId={sessionId} />
    </TooltipProvider>,
  );
}

/** The ProseMirror contenteditable host. */
function editorDom(): HTMLElement {
  return document.querySelector(".tiptap-editor") as HTMLElement;
}

/**
 * Type into the editor the way ProseMirror sees it: mutate the contenteditable
 * DOM and let the view's DOM observer turn it into a transaction (which is what
 * fires `onUpdate`). userEvent's synthetic key events don't reach ProseMirror in
 * jsdom, so this is the honest low-level equivalent of a keystroke.
 */
async function typeInEditor(text: string) {
  const dom = editorDom();
  const p = dom.querySelector("p") ?? dom;
  p.textContent = text;
  fireEvent.input(dom, { bubbles: true });
  await waitFor(() =>
    expect(useAppStore.getState().noteEditingSessionId).toBe("s1"),
  );
}

function publishRemote(content: string, sessionId = "s1") {
  const prev = useAppStore.getState().remoteNoteUpdate;
  useAppStore.setState({
    remoteNoteUpdate: { sessionId, content, seq: (prev?.seq ?? 0) + 1 },
  });
}

beforeAll(() => {
  // jsdom has no layout: ProseMirror asks a Range for its rects when it scrolls
  // the selection into view after a transaction. Without this the (irrelevant)
  // scroll path throws asynchronously and fails the run.
  const empty = Object.assign([], { item: () => null }) as unknown as DOMRectList;
  Range.prototype.getClientRects = () => empty;
  Range.prototype.getBoundingClientRect = () => new DOMRect();
});

beforeEach(() => {
  vi.clearAllMocks();
  getNoteMock.mockResolvedValue({ content: "<p>original</p>" });
  saveNoteMock.mockResolvedValue(undefined);
  useAppStore.setState({ noteEditingSessionId: null, remoteNoteUpdate: null });
});

afterEach(() => cleanup());

describe("NoteEditor — sync-down of an open note (A1c)", () => {
  it("applies a remote note update to an idle open note, in place", async () => {
    renderEditor();
    await screen.findByText("original");

    publishRemote("<p>edited on the other device</p>");

    await screen.findByText("edited on the other device");
    // In place: no note-refresh counter bump was needed (D4 normative).
    expect(saveNoteMock).not.toHaveBeenCalled();
    // Silent to the user (it only shows already-saved content) but traceable.
    expect(toast.info).not.toHaveBeenCalled();
    expect(log.info).toHaveBeenCalledWith(
      "remote update applied to open note s1",
      "note-editor",
    );
  });

  it("ignores an update published for a different session", async () => {
    renderEditor();
    await screen.findByText("original");

    publishRemote("<p>someone else's note</p>", "s2");

    await waitFor(() => expect(screen.getByText("original")).toBeTruthy());
    expect(screen.queryByText("someone else's note")).toBeNull();
  });

  it("does NOT replace content while the user is typing (A1b)", async () => {
    renderEditor();
    await screen.findByText("original");

    await typeInEditor("half a thought");
    publishRemote("<p>remote text</p>");

    await waitFor(() => expect(screen.getByText("half a thought")).toBeTruthy());
    expect(screen.queryByText("remote text")).toBeNull();
  });
});

describe("NoteEditor — save guards (A1a/A1d)", () => {
  it("writes nothing on blur when the content is unchanged", async () => {
    renderEditor();
    await screen.findByText("original");

    fireEvent.blur(editorDom());

    await waitFor(() => expect(getNoteMock).toHaveBeenCalled());
    expect(saveNoteMock).not.toHaveBeenCalled();
  });

  it("saves on blur when the user actually changed the note", async () => {
    renderEditor();
    await screen.findByText("original");

    await typeInEditor("my edit");
    fireEvent.blur(editorDom());

    await waitFor(() => expect(saveNoteMock).toHaveBeenCalled());
    expect(saveNoteMock.mock.calls[0][0]).toBe("s1");
    expect(saveNoteMock.mock.calls[0][1]).toContain("my edit");
    expect(toast.info).not.toHaveBeenCalled();
    // The editing window is released so a suppressed refresh can drain (B5).
    await waitFor(() =>
      expect(useAppStore.getState().noteEditingSessionId).toBeNull(),
    );
  });

  it("says so when a remote edit landed under a real save (visible LWW)", async () => {
    renderEditor();
    await screen.findByText("original");

    await typeInEditor("my edit");
    // The stored row moved while the user was typing.
    getNoteMock.mockResolvedValue({ content: "<p>peer wrote this</p>" });
    fireEvent.blur(editorDom());

    await waitFor(() => expect(saveNoteMock).toHaveBeenCalled());
    // LWW still proceeds — the local version is written…
    expect(saveNoteMock.mock.calls[0][1]).toContain("my edit");
    // …but never silently.
    expect(toast.info).toHaveBeenCalledWith(
      "Note updated on another device — your version was saved",
    );
  });

  it("names a remote DELETE for what it is when the save resurrects the note", async () => {
    renderEditor();
    await screen.findByText("original");

    await typeInEditor("my edit");
    // The peer deleted the note row entirely while the user was typing.
    getNoteMock.mockResolvedValue(null);
    fireEvent.blur(editorDom());

    await waitFor(() => expect(saveNoteMock).toHaveBeenCalled());
    expect(saveNoteMock.mock.calls[0][1]).toContain("my edit");
    expect(toast.info).toHaveBeenCalledWith(
      "Note was deleted on another device — your version was restored",
    );
    expect(toast.info).not.toHaveBeenCalledWith(
      "Note updated on another device — your version was saved",
    );
  });

  it("says nothing on the FIRST save of a note that never existed", async () => {
    getNoteMock.mockResolvedValue(null); // no row yet
    renderEditor();
    await waitFor(() => expect(getNoteMock).toHaveBeenCalled());

    await typeInEditor("first words");
    fireEvent.blur(editorDom());

    await waitFor(() => expect(saveNoteMock).toHaveBeenCalled());
    expect(toast.info).not.toHaveBeenCalled();
  });
});
