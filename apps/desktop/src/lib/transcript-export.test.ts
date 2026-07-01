import { describe, it, expect, vi, beforeEach } from "vitest";

// Hoisted so the vi.mock factories (themselves hoisted above imports) can
// reference these without a temporal-dead-zone error.
const {
  saveMock,
  writeTextFileMock,
  revealItemInDirMock,
  toastInfo,
  toastSuccess,
  toastError,
} = vi.hoisted(() => ({
  saveMock: vi.fn(),
  writeTextFileMock: vi.fn(),
  revealItemInDirMock: vi.fn(),
  toastInfo: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ save: saveMock }));
vi.mock("@tauri-apps/plugin-opener", () => ({
  revealItemInDir: revealItemInDirMock,
}));
vi.mock("sonner", () => ({
  toast: { info: toastInfo, success: toastSuccess, error: toastError },
}));
vi.mock("@/lib/tauri", () => ({
  commands: { writeTextFile: writeTextFileMock },
}));

import { exportTranscriptToFile } from "./transcript-export";

beforeEach(() => {
  vi.clearAllMocks();
  revealItemInDirMock.mockResolvedValue(undefined);
  writeTextFileMock.mockResolvedValue({ status: "ok", data: null });
});

describe("exportTranscriptToFile", () => {
  it("does nothing for empty content", async () => {
    const ok = await exportTranscriptToFile("", "x.md");
    expect(ok).toBe(false);
    expect(saveMock).not.toHaveBeenCalled();
    expect(toastInfo).toHaveBeenCalled();
  });

  it("returns false when the user cancels the dialog", async () => {
    saveMock.mockResolvedValue(null);
    const ok = await exportTranscriptToFile("hello", "x.md");
    expect(ok).toBe(false);
    expect(writeTextFileMock).not.toHaveBeenCalled();
  });

  it("writes the content to the chosen path and succeeds", async () => {
    saveMock.mockResolvedValue("/tmp/out.md");
    const ok = await exportTranscriptToFile("hello", "x.md");
    expect(ok).toBe(true);
    expect(writeTextFileMock).toHaveBeenCalledWith("/tmp/out.md", "hello");
    expect(toastSuccess).toHaveBeenCalled();
  });

  it("reports failure when the write command errors", async () => {
    saveMock.mockResolvedValue("/tmp/out.md");
    writeTextFileMock.mockResolvedValue({
      status: "error",
      error: { kind: "Internal", message: "nope" },
    });
    const ok = await exportTranscriptToFile("hello", "x.md");
    expect(ok).toBe(false);
    expect(toastError).toHaveBeenCalled();
  });
});
