/**
 * Mount-window regression suite. Between mount and the first load's
 * applyContent, the baselines describe nothing: docBaseline starts as ""
 * while a fresh TipTap doc serializes as "<p></p>", so the editor reads as
 * dirty with zero user edits. Before the loadedSessionRef gate, any save
 * trigger in that window (blur flush, unmount flush, typing+debounce) wrote
 * "<p></p>" over the stored row — wiping e.g. a just-written AI summary —
 * and misattributed the diff to "another device" (toast). The window is
 * widest exactly when it matters: a summarized note's load does citation
 * conversion (getNote → getSessionSegments → saveNote) before applying.
 */
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

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((res) => (resolve = res));
  return { promise, resolve };
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

beforeAll(() => {
  const empty = Object.assign([], { item: () => null }) as unknown as DOMRectList;
  Range.prototype.getClientRects = () => empty;
  Range.prototype.getBoundingClientRect = () => new DOMRect();
});

beforeEach(() => {
  vi.clearAllMocks();
  useAppStore.setState({
    noteEditingSessionId: null,
    remoteNoteUpdate: null,
    noteRefreshCounter: 0,
    noteRefreshSessionId: null,
  });
});

afterEach(() => cleanup());

describe("NoteEditor mount window — first load not yet applied", () => {
  it("a blur before the first load completes must not wipe the stored note", async () => {
    // First getNote call = the mount load (parked, e.g. slow citation
    // conversion). Later calls (the flush's stored-row read) resolve with the
    // summary the AI tool wrote while the user was on the session list.
    const loadRead = deferred<{ content: string } | null>();
    getNoteMock
      .mockImplementationOnce(() => loadRead.promise)
      .mockResolvedValue({ content: "<p>AI summary</p>" });
    saveNoteMock.mockResolvedValue(undefined);

    render(
      <TooltipProvider>
        <NoteEditor sessionId="A" />
      </TooltipProvider>,
    );
    await waitFor(() => expect(editorDom()).toBeTruthy());
    await waitFor(() => expect(getNoteMock).toHaveBeenCalledTimes(1));

    // Editor autofocuses on mount; the user clicks anywhere else → blur.
    fireEvent.blur(editorDom());
    await new Promise((r) => setTimeout(r, 20));

    // The stored note must survive; no phantom "another device" attribution.
    expect(saveNoteMock).not.toHaveBeenCalled();
    expect(toast.info).not.toHaveBeenCalled();

    // Green half: once the load applies, real edits save normally — the gate
    // must not leave the editor permanently read-only.
    loadRead.resolve({ content: "<p>AI summary</p>" });
    await screen.findByText("AI summary");
    await typeInEditor("my real edit", "A");
    fireEvent.blur(editorDom());
    await waitFor(() =>
      expect(saveNoteMock).toHaveBeenCalledWith(
        "A",
        expect.stringContaining("my real edit"),
      ),
    );
    expect(saveNoteMock).not.toHaveBeenCalledWith("A", "<p></p>");
  });

  it("an unmount before the first load completes must not wipe the stored note", async () => {
    const loadRead = deferred<{ content: string } | null>();
    getNoteMock
      .mockImplementationOnce(() => loadRead.promise)
      .mockResolvedValue({ content: "<p>AI summary</p>" });
    saveNoteMock.mockResolvedValue(undefined);

    const { unmount } = render(
      <TooltipProvider>
        <NoteEditor sessionId="A" />
      </TooltipProvider>,
    );
    await waitFor(() => expect(editorDom()).toBeTruthy());
    await waitFor(() => expect(getNoteMock).toHaveBeenCalledTimes(1));

    // User opens the session and immediately backs out to the list.
    unmount();
    await new Promise((r) => setTimeout(r, 20));

    expect(saveNoteMock).not.toHaveBeenCalled();
    expect(toast.info).not.toHaveBeenCalled();

    loadRead.resolve({ content: "<p>AI summary</p>" });
  });
});
