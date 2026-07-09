import { invoke } from "@tauri-apps/api/core";

/**
 * Thin adapter over the repo-owned Rust DB command backend (`db_service`),
 * replacing `@tauri-apps/plugin-sql` (Option A′ stage 2). It exposes the exact
 * subset of the plugin's `Database` API that `db.ts` uses — `load`, `execute`,
 * `select` — with identical shapes, so nothing else in the frontend changes.
 *
 * Backend swap only: the served database is the same plain `yapstack.db`; the
 * CRR cutover is A3. On a SQL error the underlying command rejects, matching the
 * plugin so `db.ts`'s idempotent `.catch()` runtime patches keep working.
 *
 * The default-export `{ load }` shape mirrors the plugin's default export so the
 * shared test mock (`tauriSqlMock`) keeps the same seam.
 */

export interface QueryResult {
  /** Rows affected by the last INSERT/UPDATE/DELETE. */
  rowsAffected: number;
  /** `last_insert_rowid()` after the statement. */
  lastInsertId: number;
}

export interface DbConnection {
  execute(query: string, bindValues?: unknown[]): Promise<QueryResult>;
  select<T>(query: string, bindValues?: unknown[]): Promise<T>;
}

/**
 * Sentinel the Rust backend returns while the DB is briefly unavailable during the
 * A3 CRR cutover file swap (see `db_service::swap_in_progress_err`). The swap window
 * is sub-second, so instead of failing the caller we retry with a short backoff.
 */
const SWAP_SENTINEL = "DB_SWAP_IN_PROGRESS";
const SWAP_RETRY_DELAY_MS = 250;
const SWAP_MAX_RETRIES = 10;

/**
 * Transient SQLite write-lock contention. Post-cutover (Option A′) the sync drain's
 * CrsqlDb writer and db_service's writer share one WAL file, so while the drain is
 * merging a large pulled backlog a frontend `db_select`/`db_execute` can lose the race
 * for the write lock past the Rust `busy_timeout` and surface `SQLITE_BUSY` as
 * "database is locked". The drain now commits one changeset per batch and yields the
 * lock between batches, so the next attempt almost always wins — we retry with a bounded
 * capped backoff and only surface the lock error if it stays stuck (R6 item 1b).
 */
const LOCK_MAX_RETRIES = 8;
const LOCK_RETRY_BASE_MS = 60;
const LOCK_RETRY_MAX_MS = 500;

function errMessage(err: unknown): string {
  return typeof err === "string"
    ? err
    : err instanceof Error
      ? err.message
      : String(err ?? "");
}

function isSwapInProgress(err: unknown): boolean {
  return errMessage(err).includes(SWAP_SENTINEL);
}

/**
 * A transient SQLite busy/locked error (not a schema/constraint failure). Matches the
 * two verbatim SQLite strings — "database is locked" (SQLITE_BUSY) and "database table
 * is locked" (SQLITE_LOCKED) — case-insensitively, so only genuine contention is retried
 * while real SQL errors still reject immediately.
 */
function isDatabaseLocked(err: unknown): boolean {
  const msg = errMessage(err).toLowerCase();
  return msg.includes("database is locked") || msg.includes("database table is locked");
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Invoke a DB command, transparently retrying two distinct transient conditions:
 *   1. the cutover swap window (F6) — the backend reports `DB_SWAP_IN_PROGRESS`;
 *   2. write-lock contention with the sync drain (R6) — "database is locked".
 * Any OTHER error (SQL/constraint/schema) rejects immediately. After the respective
 * retry budget the last transient error is surfaced so a genuinely stuck DB still fails
 * loudly rather than hanging forever.
 */
async function invokeWithSwapRetry<T>(
  command: string,
  args: Record<string, unknown>,
): Promise<T> {
  let swapAttempts = 0;
  let lockAttempts = 0;
  for (;;) {
    try {
      return await invoke<T>(command, args);
    } catch (err) {
      if (isSwapInProgress(err)) {
        if (swapAttempts >= SWAP_MAX_RETRIES) throw err;
        swapAttempts++;
        await sleep(SWAP_RETRY_DELAY_MS);
        continue;
      }
      if (isDatabaseLocked(err)) {
        if (lockAttempts >= LOCK_MAX_RETRIES) throw err;
        // Capped exponential backoff: 60, 120, 240, 480, 500, 500, ...
        const delay = Math.min(
          LOCK_RETRY_BASE_MS * 2 ** lockAttempts,
          LOCK_RETRY_MAX_MS,
        );
        lockAttempts++;
        await sleep(delay);
        continue;
      }
      throw err;
    }
  }
}

const connection: DbConnection = {
  async execute(query: string, bindValues: unknown[] = []): Promise<QueryResult> {
    return await invokeWithSwapRetry<QueryResult>("db_execute", {
      query,
      values: bindValues,
    });
  },
  async select<T>(query: string, bindValues: unknown[] = []): Promise<T> {
    return await invokeWithSwapRetry<T>("db_select", {
      query,
      values: bindValues,
    });
  },
};

/**
 * Mirrors `Database.load(url)`. The connection pool lives in Rust and is opened
 * once at startup, so this just hands back the shared adapter; the URL argument
 * is accepted for API compatibility and otherwise unused.
 */
async function load(_url: string): Promise<DbConnection> {
  return connection;
}

export default { load };
