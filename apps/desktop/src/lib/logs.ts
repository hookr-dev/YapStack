import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import { commands, type LogEntry } from "./types";

export type { LogEntry };

/**
 * Size of the backend in-memory ring buffer (`LogBuffer::new(500)` in
 * `logging.rs`). This is the most history the backend can ever return, so it
 * doubles as the natural ceiling for "load older" re-fetches: requesting more
 * than this yields at most this many entries.
 */
export const LOG_BUFFER_CAPACITY = 500;

/**
 * Initial snapshot size on mount. Raised from the old hard-coded 200 to keep
 * first paint cheap while still showing plenty of recent context. It stays
 * below `LOG_BUFFER_CAPACITY` on purpose so "load older" has room to reach the
 * rest of the buffer; when the buffer holds fewer than this, the initial fetch
 * already returns everything and "load older" hides itself.
 */
export const INITIAL_LOG_FETCH = 250;

/**
 * How much further back each "load older" re-fetch reaches. The backend
 * returns the most-recent `limit` entries, so growing the limit surfaces more
 * history until we hit `LOG_BUFFER_CAPACITY`.
 */
export const LOG_FETCH_STEP = 250;

/** Last N entries from the backend ring buffer. */
export async function getRecentLogs(limit?: number): Promise<LogEntry[]> {
  const res = await commands.getRecentLogs(limit ?? null);
  if (res.status === "ok") return res.data;
  throw new Error(res.error.message);
}

export async function clearLogs(): Promise<void> {
  const res = await commands.clearLogs();
  if (res.status !== "ok") throw new Error(res.error.message);
}

export async function getLogDir(): Promise<string> {
  const res = await commands.getLogDir();
  if (res.status === "ok") return res.data;
  throw new Error(res.error.message);
}

export async function revealLogDir(): Promise<void> {
  const res = await commands.revealLogDir();
  if (res.status !== "ok") throw new Error(res.error.message);
}

/** Subscribe to live log events. Returns an unsubscribe fn. */
export function subscribeLogs(
  onEntry: (entry: LogEntry) => void,
): Promise<UnlistenFn> {
  return listen<LogEntry>("log://entry", (evt) => onEntry(evt.payload));
}

/** Render one entry as a single line for clipboard export. */
export function formatLogEntry(e: LogEntry): string {
  const ts = new Date(e.ts_ms).toISOString().replace("T", " ").replace("Z", "");
  return `${ts} ${e.level.padEnd(5)} ${e.target}: ${e.message}`;
}

/** Render a batch of entries for clipboard export. */
export function formatLogEntries(entries: LogEntry[]): string {
  return entries.map(formatLogEntry).join("\n");
}
