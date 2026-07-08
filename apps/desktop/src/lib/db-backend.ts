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

const connection: DbConnection = {
  async execute(query: string, bindValues: unknown[] = []): Promise<QueryResult> {
    return await invoke<QueryResult>("db_execute", {
      query,
      values: bindValues,
    });
  },
  async select<T>(query: string, bindValues: unknown[] = []): Promise<T> {
    return (await invoke("db_select", {
      query,
      values: bindValues,
    })) as T;
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
