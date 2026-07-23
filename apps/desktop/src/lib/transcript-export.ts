import { save } from "@tauri-apps/plugin-dialog";
import { toast } from "sonner";
import { commands } from "@/lib/tauri";
import { revealAudioFile } from "@/lib/reveal-file";

/**
 * Save transcript text to a user-chosen file and offer to reveal it. Returns
 * `true` when a file was written. Toasts on success/failure — callers don't
 * add their own.
 *
 * The write goes through the `write_text_file` app command so the renderer's
 * `fs` plugin grant stays read-only.
 */
export async function exportTranscriptToFile(
  content: string,
  defaultFileName: string,
): Promise<boolean> {
  if (content.length === 0) {
    toast.info("Nothing to export");
    return false;
  }
  try {
    const path = await save({
      defaultPath: defaultFileName,
      filters: [
        { name: "Markdown", extensions: ["md"] },
        { name: "Text", extensions: ["txt"] },
      ],
    });
    if (!path) return false; // user cancelled the dialog
    const res = await commands.writeTextFile(path, content);
    if (res.status !== "ok") {
      toast.error("Export failed");
      return false;
    }
    toast.success("Transcript exported", {
      action: { label: "Reveal", onClick: () => void revealAudioFile(path) },
    });
    return true;
  } catch {
    toast.error("Export failed");
    return false;
  }
}
