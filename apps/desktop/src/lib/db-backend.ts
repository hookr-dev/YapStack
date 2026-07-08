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

function isSwapInProgress(err: unknown): boolean {
  const msg =
    typeof err === "string"
      ? err
      : err instanceof Error
        ? err.message
        : String(err ?? "");
  return msg.includes(SWAP_SENTINEL);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Invoke a DB command, transparently retrying while the backend reports the cutover
 * swap window (F6). Any other error rejects immediately; after `SWAP_MAX_RETRIES`
 * the last swap error is surfaced so a genuinely stuck swap still fails loudly.
 */
async function invokeWithSwapRetry<T>(
  command: string,
  args: Record<string, unknown>,
): Promise<T> {
  let lastError: unknown;
  for (let attempt = 0; attempt <= SWAP_MAX_RETRIES; attempt++) {
    try {
      return await invoke<T>(command, args);
    } catch (err) {
      lastError = err;
      if (!isSwapInProgress(err) || attempt === SWAP_MAX_RETRIES) {
        throw err;
      }
      await sleep(SWAP_RETRY_DELAY_MS);
    }
  }
  throw lastError;
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
