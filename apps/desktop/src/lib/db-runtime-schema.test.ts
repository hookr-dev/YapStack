import { describe, it, expect, vi, beforeEach } from "vitest";
// Node 22+ builtin. The app tsconfig is DOM-only (no @types/node), so the import
// is untyped and the two methods used below are declared locally as `SqliteDb`.
// @ts-expect-error -- untyped Node builtin
import { DatabaseSync } from "node:sqlite";

interface SqliteDb {
  exec(sql: string): void;
  prepare(sql: string): { all(): unknown[] };
  close(): void;
}

/**
 * `ensureRuntimeSchema` regressions. Two classes are covered:
 *
 *  1. The `session_audio_parts` backfill must be idempotent WITHOUT the
 *     `(session_id, part_index)` UNIQUE — the CRR sync migration strips every
 *     non-PK UNIQUE, so `INSERT OR IGNORE` alone re-minted the whole backfill
 *     (fresh `randomblob` PK) on every launch. Exercised against a real SQLite
 *     built in the post-CRR shape.
 *  2. The runtime ALTER pass must fire only on a POSITIVE "column absent"
 *     probe. Treating a probe error (or a missing table) as "absent" is what
 *     produced the per-boot ALTER storm.
 */

const { executeMock, selectMock } = vi.hoisted(() => ({
  executeMock: vi.fn(),
  selectMock: vi.fn(),
}));

vi.mock("@/lib/db-backend", () => ({
  default: {
    load: vi.fn(async () => ({ execute: executeMock, select: selectMock })),
  },
}));

vi.mock("@/lib/tauri", () => ({
  commands: {
    backendReady: vi.fn().mockResolvedValue(true),
    logFrontend: vi.fn().mockResolvedValue(null),
  },
}));

type SelectRouter = (query: string) => unknown[];

/** Every column the v14 chat_messages shape carries after migration. */
const CHAT_MESSAGE_COLUMNS = [
  "id",
  "context_key",
  "session_id",
  "role",
  "content",
  "action",
  "created_at",
  "tool_calls",
  "send_id",
  "sequence",
  "tool_call_id",
  "observation",
  "status",
];

function router(overrides: Partial<{ crrClock: number; chatColumns: string[] }> = {}) {
  return (query: string): unknown[] => {
    // The crsql-clock probe binds the shadow-table name, so match on the FROM.
    if (query.includes("sqlite_master")) return [{ n: overrides.crrClock ?? 0 }];
    if (query.includes("pragma_table_info('chat_messages')")) {
      return (overrides.chatColumns ?? CHAT_MESSAGE_COLUMNS).map((name) => ({ name }));
    }
    return [];
  };
}

/**
 * Run one full runtime-schema pass against fresh module state (the patch is
 * single-flight per module instance) and return every statement it executed.
 */
async function runSchemaPass(
  select: SelectRouter | "reject" = router(),
): Promise<string[]> {
  vi.resetModules();
  executeMock.mockReset().mockResolvedValue({ rowsAffected: 0 });
  selectMock.mockReset();
  if (select === "reject") {
    selectMock.mockRejectedValue(new Error("state not managed for field `service`"));
  } else {
    selectMock.mockImplementation(async (q: string) => select(q));
  }
  const db = await import("@/lib/db");
  // A patched read awaits the schema pass, so this resolves only once it is done.
  await db.listSessionAudioParts("s1").catch(() => {});
  return executeMock.mock.calls.map(([q]) => q as string);
}

function backfillStatements(statements: string[]): string[] {
  return statements.filter((q) =>
    /INSERT OR IGNORE INTO session_audio_parts/i.test(q),
  );
}

/**
 * `session_audio_parts` as the CRR migration leaves it: PK survives, the
 * `UNIQUE (session_id, part_index)` and the `format` CHECK are stripped.
 */
function crrShapedDb(): SqliteDb {
  const db = new DatabaseSync(":memory:") as SqliteDb;
  db.exec(`
    CREATE TABLE sessions (
      id TEXT NOT NULL PRIMARY KEY,
      created_at TEXT NOT NULL,
      updated_at TEXT,
      wav_file_path TEXT,
      wav_duration_seconds REAL
    );
    CREATE TABLE session_audio_parts (
      id TEXT NOT NULL PRIMARY KEY,
      session_id TEXT NOT NULL,
      part_index INTEGER NOT NULL,
      file_path TEXT NOT NULL,
      format TEXT NOT NULL,
      duration_seconds REAL NOT NULL,
      sample_rate INTEGER NOT NULL,
      created_at TEXT NOT NULL
    );
    INSERT INTO sessions (id, created_at, updated_at, wav_file_path, wav_duration_seconds)
      VALUES ('s1', '2024-01-01', '2024-01-02', '/audio/s1.wav', 12.5);
  `);
  return db;
}

function partRows(db: SqliteDb): Record<string, unknown>[] {
  return db
    .prepare("SELECT id, session_id, part_index FROM session_audio_parts")
    .all() as Record<string, unknown>[];
}

describe("session_audio_parts backfill idempotency on a CRR database", () => {
  let backfill: string;

  beforeEach(async () => {
    const statements = await runSchemaPass();
    const found = backfillStatements(statements);
    expect(found).toHaveLength(1);
    backfill = found[0];
  });

  it("guards on the (session_id, part_index) pair, not the stripped UNIQUE", () => {
    expect(backfill).toMatch(/NOT EXISTS/i);
    expect(backfill).toMatch(/p\.session_id = sessions\.id/);
    expect(backfill).toMatch(/p\.part_index = 0/);
  });

  it("inserts the legacy part exactly once, however many times it runs", () => {
    const db = crrShapedDb();
    db.exec(backfill);
    db.exec(backfill);
    db.exec(backfill);
    expect(partRows(db)).toEqual([
      { id: expect.any(String), session_id: "s1", part_index: 0 },
    ]);
    db.close();
  });

  it("emits no row when a part 0 already exists under a different id", () => {
    const db = crrShapedDb();
    db.exec(`
      INSERT INTO session_audio_parts
        (id, session_id, part_index, file_path, format, duration_seconds, sample_rate, created_at)
      VALUES ('pre-existing', 's1', 0, '/audio/s1.wav', 'wav', 12.5, 48000, '2024-01-02');
    `);
    db.exec(backfill);
    expect(partRows(db)).toEqual([
      { id: "pre-existing", session_id: "s1", part_index: 0 },
    ]);
    db.close();
  });
});

describe("runtime ALTER pass", () => {
  it("emits no ALTER when the column already exists", async () => {
    const statements = await runSchemaPass();
    expect(statements.filter((q) => /^ALTER TABLE/i.test(q))).toEqual([]);
  });

  it("emits an ALTER only for the column a successful probe reports absent", async () => {
    const columns = CHAT_MESSAGE_COLUMNS.filter((c) => c !== "status");
    const statements = await runSchemaPass(router({ chatColumns: columns }));
    expect(statements.filter((q) => /^ALTER TABLE/i.test(q))).toEqual([
      "ALTER TABLE chat_messages ADD COLUMN status TEXT",
    ]);
  });

  it("emits no ALTER when the schema probe itself fails (backend not ready)", async () => {
    const statements = await runSchemaPass("reject");
    expect(statements.filter((q) => /^ALTER TABLE/i.test(q))).toEqual([]);
  });

  it("defers to the Rust boot self-heal on a CRR-tracked table", async () => {
    const columns = CHAT_MESSAGE_COLUMNS.filter((c) => c !== "status");
    const statements = await runSchemaPass(router({ crrClock: 1, chatColumns: columns }));
    expect(statements.filter((q) => /^ALTER TABLE/i.test(q))).toEqual([]);
  });
});

describe("schema patch is single-flight", () => {
  it("runs one pass however many callers race the first load", async () => {
    vi.resetModules();
    executeMock.mockReset().mockResolvedValue({ rowsAffected: 0 });
    selectMock.mockReset().mockImplementation(async (q: string) => router()(q));
    const db = await import("@/lib/db");
    await Promise.all([
      db.listSessions(),
      db.listSessionAudioParts("s1"),
      db.createSession("s2", "MicOnly"),
    ]);
    const statements = executeMock.mock.calls.map(([q]) => q as string);
    expect(backfillStatements(statements)).toHaveLength(1);
  });
});
