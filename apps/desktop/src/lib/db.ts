import Database, { type DbConnection } from "./db-backend";
import { log } from "./logger";
import { stripHtml } from "./utils";

// --- Types ---

export type SessionStatus = "recording" | "completed";
export type SessionType = "manual" | "transcription";

export interface DbSession {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  source: string;
  status: SessionStatus;
  duration_seconds: number | null;
  total_segments: number;
  folder_id: string | null;
  is_pinned: number;
  pinned_at: string | null;
  session_type: SessionType;
  sort_order: number;
}

export type AudioPartFormat = "wav" | "mp3";

/**
 * One finalized recording run for a session. A session's audio is the
 * ordered concatenation of its parts (`part_index` 0..N). Parts are immutable
 * once written; resume appends a new part rather than mutating any prior one.
 */
export interface DbAudioPart {
  id: string;
  session_id: string;
  part_index: number;
  file_path: string;
  format: AudioPartFormat;
  duration_seconds: number;
  sample_rate: number;
  created_at: string;
}

/**
 * Whether a Session is in a state where the Resume Recording action should
 * be offered. Pure predicate so the same rule can drive multiple UI surfaces.
 */
export function canResumeSession(
  session: DbSession,
  parts: DbAudioPart[],
  liveTranscriptionActive: boolean,
  sessionStopping: boolean,
): boolean {
  // A part with duration_seconds <= 0 means we can't compute a continuous
  // offset for the next run. The migration backfill writes 0 for legacy rows
  // that never had wav_duration_seconds populated; offering Resume on those
  // would overlap existing transcript timestamps.
  return (
    session.session_type !== "manual" &&
    session.status === "completed" &&
    parts.length > 0 &&
    parts.every((p) => p.duration_seconds > 0) &&
    !liveTranscriptionActive &&
    !sessionStopping
  );
}

export interface DbNote {
  id: string;
  session_id: string;
  content: string;
  updated_at: string;
}

export interface DbNoteVersion {
  id: string;
  note_id: string;
  content: string;
  created_at: string;
}

export interface DbSegment {
  id: string;
  session_id: string;
  source: "Mic" | "System";
  text: string;
  audio_offset_seconds: number;
  chunk_duration_seconds: number;
  confidence: number;
  created_at: string;
  chunk_index: number;
  original_text: string | null;
  edited_at: string | null;
  deleted_at: string | null;
  hidden: number;
  // Populated when Parakeet + Sortformer diarization tagged this segment.
  // NULL/undefined for Whisper-transcribed segments and for any row written
  // before migration v11.
  speaker_id?: number | null;
}

export interface DbFolder {
  id: string;
  name: string;
  parent_id: string | null;
  sort_order: number;
  icon: string | null;
  color: string | null;
  description: string | null;
  created_at: string;
  updated_at: string;
}

export interface DbSessionFolder {
  session_id: string;
  folder_id: string;
  created_at: string;
}

export interface DbTag {
  id: string;
  name: string;
  color: string | null;
  created_at: string;
}

export interface DbSessionTag {
  session_id: string;
  tag_id: string;
  source: "manual" | "auto" | "ai";
  confidence: number | null;
  created_at: string;
}

export interface DbChatMessage {
  id: string;
  context_key: string;
  session_id: string | null;
  role: "user" | "assistant" | "tool";
  /**
   * Empty string for assistant rows that emit only `tool_calls` (the
   * underlying SQLite column is NOT NULL). The TS layer treats `""` as
   * "no prose" — see `chat-history.ts`.
   */
  content: string;
  action: string | null;
  created_at: string;
  /**
   * Legacy column (v13). For the per-LLM-response shape (v14+), assistant
   * rows carry their OpenAI tool_calls list as JSON here:
   *   `[{ id, name, arguments, label, status, detail, observation }]`
   * `id` matches the `tool_call_id` of the corresponding tool row.
   * Pre-v14 rows may carry the older flat shape (no `id`); the assembler
   * treats those as legacy and skips tool replay.
   */
  tool_calls: string | null;
  /** Groups all rows derived from one user send. NULL on pre-v14 rows. */
  send_id: string | null;
  /** Order within `send_id`. NULL on pre-v14 rows. */
  sequence: number | null;
  /** Set on `role='tool'`; references the parent assistant's tool_calls[].id. */
  tool_call_id: string | null;
  /** Structured ToolObservation JSON. Set on `role='tool'`. */
  observation: string | null;
  /** `'done' | 'error'`. Set on `role='tool'`. */
  status: string | null;
}

// --- Singleton ---

let dbInstance: DbConnection | null = null;

async function getDb(): Promise<DbConnection> {
  if (!dbInstance) {
    dbInstance = await Database.load("sqlite:yapstack.db");
    await ensureRuntimeSchema(dbInstance);
  }
  return dbInstance;
}

/**
 * True if `table` is cr-sqlite CRR-tracked, detected by the presence of its clock
 * shadow table `<table>__crsql_clock` (the same signal the Rust side uses,
 * `schema::is_crr`). If the probe itself fails we return `false` so the caller falls
 * back to the plain non-CRR ALTER path rather than skipping a needed column.
 */
async function isCrrTracked(db: DbConnection, table: string): Promise<boolean> {
  try {
    const rows = await db.select<{ n: number }[]>(
      "SELECT count(*) AS n FROM sqlite_master WHERE type='table' AND name = $1",
      [`${table}__crsql_clock`],
    );
    return (rows[0]?.n ?? 0) > 0;
  } catch {
    return false;
  }
}

/**
 * Apply one runtime `ALTER TABLE <table> ADD COLUMN` — but ONLY on a non-CRR
 * (pre-sync) DB. On a sync-enabled (cr-sqlite CRR) DB the Rust boot self-heal owns
 * schema evolution: it adds the column through the crsql alter dance so the column
 * is CRR-tracked. A bare ALTER here would RACE that self-heal, and — since we cannot
 * rely on cr-sqlite rejecting a bare `ADD COLUMN` on a CRR table (unverified) — an
 * accepted bare ALTER would land an UN-CRR-tracked column that `apply_out_of_band_alters`
 * then skips, resurrecting the indefinite-quarantine bug. So we GATE: when `table` is
 * CRR-tracked we skip the bare ALTER entirely and let the Rust path own it (logged at
 * debug/info, not error — this is the correct, expected branch on a synced DB).
 *
 * On a non-CRR DB the bare ALTER is correct and expected to no-op once the column
 * exists ("duplicate column name" is the silent idempotent case). Every OTHER rejection
 * there is logged loudly (devtools `console.error` + the backend `log.error` seam)
 * instead of vanishing.
 */
async function addColumnIfMissing(
  db: DbConnection,
  table: string,
  sql: string,
): Promise<void> {
  if (await isCrrTracked(db, table)) {
    log.info(
      `ensureRuntimeSchema: ${table} is CRR-tracked; deferring ADD COLUMN to the Rust boot self-heal (skipping bare ALTER): ${sql}`,
      "db",
    );
    return;
  }
  try {
    await db.execute(sql);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    if (/duplicate column name/i.test(message)) {
      return; // expected idempotent no-op: the column already exists on a non-CRR DB
    }
    console.error(`ensureRuntimeSchema: runtime ALTER failed: ${sql}: ${message}`);
    log.error(
      `runtime ALTER rejected (column not applied — incoming sync changes for it may quarantine until the Rust boot self-heal runs): ${sql}: ${message}`,
      "db",
    );
  }
}

/**
 * Idempotent runtime schema patches. tauri-plugin-sql can skip a migration
 * on a dev DB whose `_sqlx_migrations` history was written under a prior
 * version-numbering scheme (see CLAUDE.md). Re-applying these on every load
 * ensures the columns/tables/triggers exist; CREATE/ALTER variants use
 * `IF NOT EXISTS` or are wrapped with `.catch()` so the duplicate-column /
 * duplicate-table error on subsequent runs is a silent no-op.
 */
async function ensureRuntimeSchema(db: DbConnection): Promise<void> {
  // Single-column patches (column-add only). These bare ALTERs are correct on a
  // pre-sync (non-CRR) DB and are expected to no-op once the column exists. On a
  // sync-enabled (cr-sqlite CRR) DB, however, `addColumnIfMissing` skips the bare
  // ALTER entirely: schema evolution there is owned by the Rust boot self-heal
  // (apply_out_of_band_alters via the crsql alter dance), which CRR-tracks the new
  // column. Issuing a bare ALTER on a CRR table would race that self-heal and risk an
  // un-CRR-tracked column that quarantines incoming changes forever.
  await addColumnIfMissing(db, "segments", "ALTER TABLE segments ADD COLUMN speaker_id INTEGER");
  await addColumnIfMissing(db, "chat_messages", "ALTER TABLE chat_messages ADD COLUMN tool_calls TEXT");
  // v14: per-LLM-response shape for tool replay.
  await addColumnIfMissing(db, "chat_messages", "ALTER TABLE chat_messages ADD COLUMN send_id TEXT");
  await addColumnIfMissing(db, "chat_messages", "ALTER TABLE chat_messages ADD COLUMN sequence INTEGER");
  await addColumnIfMissing(
    db,
    "chat_messages",
    "ALTER TABLE chat_messages ADD COLUMN tool_call_id TEXT",
  );
  await addColumnIfMissing(db, "chat_messages", "ALTER TABLE chat_messages ADD COLUMN observation TEXT");
  await addColumnIfMissing(db, "chat_messages", "ALTER TABLE chat_messages ADD COLUMN status TEXT");
  // Backfill legacy rows so they have a self-consistent send group.
  await db
    .execute("UPDATE chat_messages SET send_id = id WHERE send_id IS NULL")
    .catch(() => {});
  await db
    .execute("UPDATE chat_messages SET sequence = 0 WHERE sequence IS NULL")
    .catch(() => {});
  await db
    .execute(
      "CREATE INDEX IF NOT EXISTS idx_chat_messages_send ON chat_messages(context_key, send_id, sequence)",
    )
    .catch(() => {});

  // FTS5 search tables. Created idempotently; backfilled only when the FTS
  // table is empty (i.e. on its very first run). Triggers keep them in sync
  // with the source tables thereafter.
  const tables: { name: string; columns: string; backfill: string }[] = [
    {
      name: "segments_fts",
      columns:
        "segment_id UNINDEXED, session_id UNINDEXED, text, tokenize = 'porter unicode61 remove_diacritics 2'",
      backfill:
        "INSERT INTO segments_fts (segment_id, session_id, text) SELECT id, session_id, text FROM segments WHERE deleted_at IS NULL",
    },
    {
      name: "notes_fts",
      columns:
        "note_id UNINDEXED, session_id UNINDEXED, content, tokenize = 'porter unicode61 remove_diacritics 2'",
      backfill:
        "INSERT INTO notes_fts (note_id, session_id, content) SELECT id, session_id, content FROM notes",
    },
    {
      name: "sessions_fts",
      columns:
        "session_id UNINDEXED, title, tokenize = 'porter unicode61 remove_diacritics 2'",
      backfill:
        "INSERT INTO sessions_fts (session_id, title) SELECT id, title FROM sessions",
    },
    {
      name: "dictations_fts",
      columns:
        "dictation_id UNINDEXED, output_text, input_text, tokenize = 'porter unicode61 remove_diacritics 2'",
      backfill:
        "INSERT INTO dictations_fts (dictation_id, output_text, input_text) SELECT id, output_text, input_text FROM dictation_history",
    },
  ];
  for (const t of tables) {
    await db
      .execute(`CREATE VIRTUAL TABLE IF NOT EXISTS ${t.name} USING fts5(${t.columns})`)
      .catch(() => {});
    const rows = await db
      .select<{ c: number }[]>(`SELECT count(*) AS c FROM ${t.name}`)
      .catch(() => [{ c: 1 }]);
    if (rows[0]?.c === 0) {
      await db.execute(t.backfill).catch(() => {});
    }
  }

  // Triggers: keep FTS in sync with source tables. CREATE TRIGGER IF NOT
  // EXISTS is a single statement, so we run them one-by-one.
  const triggers: string[] = [
    `CREATE TRIGGER IF NOT EXISTS segments_ai AFTER INSERT ON segments
       WHEN new.deleted_at IS NULL
     BEGIN
       INSERT INTO segments_fts (segment_id, session_id, text)
         VALUES (new.id, new.session_id, new.text);
     END`,
    `CREATE TRIGGER IF NOT EXISTS segments_ad AFTER DELETE ON segments
     BEGIN
       DELETE FROM segments_fts WHERE segment_id = old.id;
     END`,
    `CREATE TRIGGER IF NOT EXISTS segments_au AFTER UPDATE ON segments
     BEGIN
       DELETE FROM segments_fts WHERE segment_id = old.id;
       INSERT INTO segments_fts (segment_id, session_id, text)
         SELECT new.id, new.session_id, new.text WHERE new.deleted_at IS NULL;
     END`,
    `CREATE TRIGGER IF NOT EXISTS notes_ai AFTER INSERT ON notes
     BEGIN
       INSERT INTO notes_fts (note_id, session_id, content)
         VALUES (new.id, new.session_id, new.content);
     END`,
    `CREATE TRIGGER IF NOT EXISTS notes_ad AFTER DELETE ON notes
     BEGIN
       DELETE FROM notes_fts WHERE note_id = old.id;
     END`,
    `CREATE TRIGGER IF NOT EXISTS notes_au AFTER UPDATE ON notes
     BEGIN
       DELETE FROM notes_fts WHERE note_id = old.id;
       INSERT INTO notes_fts (note_id, session_id, content)
         VALUES (new.id, new.session_id, new.content);
     END`,
    `CREATE TRIGGER IF NOT EXISTS sessions_ai AFTER INSERT ON sessions
     BEGIN
       INSERT INTO sessions_fts (session_id, title) VALUES (new.id, new.title);
     END`,
    `CREATE TRIGGER IF NOT EXISTS sessions_ad AFTER DELETE ON sessions
     BEGIN
       DELETE FROM sessions_fts WHERE session_id = old.id;
     END`,
    `CREATE TRIGGER IF NOT EXISTS sessions_au AFTER UPDATE OF title ON sessions
     BEGIN
       DELETE FROM sessions_fts WHERE session_id = old.id;
       INSERT INTO sessions_fts (session_id, title) VALUES (new.id, new.title);
     END`,
    `CREATE TRIGGER IF NOT EXISTS dictations_ai AFTER INSERT ON dictation_history
     BEGIN
       INSERT INTO dictations_fts (dictation_id, output_text, input_text)
         VALUES (new.id, new.output_text, new.input_text);
     END`,
    `CREATE TRIGGER IF NOT EXISTS dictations_ad AFTER DELETE ON dictation_history
     BEGIN
       DELETE FROM dictations_fts WHERE dictation_id = old.id;
     END`,
    `CREATE TRIGGER IF NOT EXISTS dictations_au AFTER UPDATE ON dictation_history
     BEGIN
       DELETE FROM dictations_fts WHERE dictation_id = old.id;
       INSERT INTO dictations_fts (dictation_id, output_text, input_text)
         VALUES (new.id, new.output_text, new.input_text);
     END`,
  ];
  for (const sql of triggers) {
    await db.execute(sql).catch(() => {});
  }

  // v15 — session_audio_parts. Created here in addition to the migration so
  // dev DBs whose `_sqlx_migrations` history is misaligned (and where
  // tauri-plugin-sql is therefore skipping new migrations) still get the
  // table. Idempotent: the second run is a no-op.
  await db
    .execute(
      `CREATE TABLE IF NOT EXISTS session_audio_parts (
         id TEXT PRIMARY KEY,
         session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
         part_index INTEGER NOT NULL,
         file_path TEXT NOT NULL,
         format TEXT NOT NULL CHECK (format IN ('wav','mp3')),
         duration_seconds REAL NOT NULL,
         sample_rate INTEGER NOT NULL,
         created_at TEXT NOT NULL,
         UNIQUE (session_id, part_index)
       )`,
    )
    .catch(() => {});
  await db
    .execute(
      `CREATE INDEX IF NOT EXISTS idx_audio_parts_session
         ON session_audio_parts(session_id, part_index)`,
    )
    .catch(() => {});
  // Backfill: every existing session with a wav_file_path becomes
  // part_index=0. INSERT OR IGNORE makes this safe to re-run; the
  // (session_id, part_index) UNIQUE constraint short-circuits duplicates.
  // Wrapped in catch() because pre-v5 DBs don't have wav_file_path yet —
  // they have no audio to backfill anyway.
  await db
    .execute(
      `INSERT OR IGNORE INTO session_audio_parts (
         id, session_id, part_index, file_path, format,
         duration_seconds, sample_rate, created_at
       )
       SELECT
         lower(hex(randomblob(16))),
         id,
         0,
         wav_file_path,
         CASE WHEN wav_file_path LIKE '%.mp3' THEN 'mp3' ELSE 'wav' END,
         COALESCE(wav_duration_seconds, 0),
         48000,
         COALESCE(updated_at, created_at)
       FROM sessions
       WHERE wav_file_path IS NOT NULL`,
    )
    .catch(() => {});

  // chat_context_settings — per-chat-context Profile override for the
  // AI Connection/Profile refactor. profile_id NULL means "use the live
  // default Chat Assignment"; non-null persists an explicit override.
  //
  // Intentionally defined here (frontend runtime schema) rather than in the
  // Rust `migrations()` list — same as `segments.speaker_id` and the ALTERs
  // above. Post-"ghost v11" (see db.rs), new incremental schema is added via
  // this idempotent IF-NOT-EXISTS path because a higher-numbered sqlx
  // migration can be silently refused on dev DBs with inconsistent history.
  await db
    .execute(
      `CREATE TABLE IF NOT EXISTS chat_context_settings (
         context_key TEXT PRIMARY KEY,
         profile_id  TEXT NULL,
         updated_at  TEXT NOT NULL
       )`,
    )
    .catch(() => {});
}

// --- Session CRUD ---

export async function createSession(
  id: string,
  source: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "INSERT INTO sessions (id, source) VALUES ($1, $2)",
    [id, source],
  );
}

export async function updateSessionTitle(
  id: string,
  title: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "UPDATE sessions SET title = $1, updated_at = datetime('now') WHERE id = $2",
    [title, id],
  );
}

/**
 * Marks a session as completed. `duration_seconds` is derived from the SUM
 * of `session_audio_parts.duration_seconds` — so resumed sessions correctly
 * carry the full stitched duration. `fallbackDurationSeconds` is used only
 * when no parts exist (e.g. WAV finalization failed before a part landed),
 * so we still record *something* useful instead of zero.
 */
export async function completeSession(
  id: string,
  fallbackDurationSeconds: number = 0,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    `UPDATE sessions
     SET status = 'completed',
         duration_seconds = COALESCE(
           NULLIF(
             (SELECT SUM(duration_seconds) FROM session_audio_parts WHERE session_id = $1),
             0
           ),
           $2
         ),
         total_segments = (SELECT COUNT(*) FROM segments WHERE session_id = $1),
         updated_at = datetime('now')
     WHERE id = $1`,
    [id, fallbackDurationSeconds],
  );
}

/**
 * Flips a completed session back to `recording` status — called after the
 * backend accepts a resume so the sidebar/UI sees the live state. The
 * reverse transition runs through `completeSession` at the next stop.
 */
export async function markSessionRecording(id: string): Promise<void> {
  const db = await getDb();
  await db.execute(
    `UPDATE sessions
     SET status = 'recording',
         updated_at = datetime('now')
     WHERE id = $1`,
    [id],
  );
}

export async function deleteSession(id: string): Promise<void> {
  const db = await getDb();
  await db.execute("DELETE FROM sessions WHERE id = $1", [id]);
}

export async function listSessions(): Promise<DbSession[]> {
  const db = await getDb();
  return await db.select<DbSession[]>(
    "SELECT * FROM sessions ORDER BY created_at DESC",
  );
}

export async function getSession(id: string): Promise<DbSession | null> {
  const db = await getDb();
  const rows = await db.select<DbSession[]>(
    "SELECT * FROM sessions WHERE id = $1",
    [id],
  );
  return rows[0] ?? null;
}

export async function getSessionsByIds(ids: string[]): Promise<DbSession[]> {
  if (ids.length === 0) return [];
  const db = await getDb();
  const placeholders = ids.map((_, i) => `$${i + 1}`).join(",");
  return await db.select<DbSession[]>(
    `SELECT * FROM sessions WHERE id IN (${placeholders})`,
    ids,
  );
}

export async function deleteAllSessions(): Promise<void> {
  const db = await getDb();
  // CASCADE handles child tables: segments, notes (→ note_versions),
  // chat_messages, session_folders
  await db.execute("DELETE FROM sessions");
}

// --- Pin operations ---

export async function togglePin(id: string): Promise<void> {
  const db = await getDb();
  const rows = await db.select<{ is_pinned: number }[]>(
    "SELECT is_pinned FROM sessions WHERE id = $1",
    [id],
  );
  if (rows.length === 0) return;

  const isPinned = rows[0].is_pinned;
  if (isPinned) {
    await db.execute(
      "UPDATE sessions SET is_pinned = 0, pinned_at = NULL, updated_at = datetime('now') WHERE id = $1",
      [id],
    );
  } else {
    await db.execute(
      "UPDATE sessions SET is_pinned = 1, pinned_at = datetime('now'), updated_at = datetime('now') WHERE id = $1",
      [id],
    );
  }
}

// --- Folder CRUD ---

export async function createFolder(
  id: string,
  name: string,
  parentId: string | null = null,
  icon: string | null = null,
  color: string | null = null,
  description: string | null = null,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "INSERT INTO folders (id, name, parent_id, icon, color, description) VALUES ($1, $2, $3, $4, $5, $6)",
    [id, name, parentId, icon, color, description],
  );
}

export async function updateFolder(
  id: string,
  updates: { name?: string; icon?: string | null; color?: string | null; description?: string | null },
): Promise<void> {
  const db = await getDb();
  const setClauses: string[] = [];
  const params: unknown[] = [];
  let paramIdx = 1;

  if (updates.name !== undefined) {
    setClauses.push(`name = $${paramIdx++}`);
    params.push(updates.name);
  }
  if (updates.icon !== undefined) {
    setClauses.push(`icon = $${paramIdx++}`);
    params.push(updates.icon);
  }
  if (updates.color !== undefined) {
    setClauses.push(`color = $${paramIdx++}`);
    params.push(updates.color);
  }
  if (updates.description !== undefined) {
    setClauses.push(`description = $${paramIdx++}`);
    params.push(updates.description);
  }

  if (setClauses.length === 0) return;

  setClauses.push(`updated_at = datetime('now')`);
  params.push(id);

  await db.execute(
    `UPDATE folders SET ${setClauses.join(", ")} WHERE id = $${paramIdx}`,
    params,
  );
}

export async function deleteFolder(id: string): Promise<void> {
  const db = await getDb();
  // Clean up chat messages for this folder context
  await db.execute(
    "DELETE FROM chat_messages WHERE context_key = $1",
    [`folder:${id}`],
  );
  // Junction rows cascade, sessions.folder_id set NULL via ON DELETE SET NULL
  await db.execute("DELETE FROM folders WHERE id = $1", [id]);
}

export async function updateFolderParent(
  folderId: string,
  newParentId: string | null,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "UPDATE folders SET parent_id = $1, updated_at = datetime('now') WHERE id = $2",
    [newParentId, folderId],
  );
}

export async function listFolders(): Promise<DbFolder[]> {
  const db = await getDb();
  return await db.select<DbFolder[]>(
    "SELECT * FROM folders ORDER BY sort_order ASC, name ASC",
  );
}

// --- Session-Folder junction ---

export async function addSessionToFolder(
  sessionId: string,
  folderId: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "INSERT OR IGNORE INTO session_folders (session_id, folder_id) VALUES ($1, $2)",
    [sessionId, folderId],
  );
}

export async function removeSessionFromFolder(
  sessionId: string,
  folderId: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "DELETE FROM session_folders WHERE session_id = $1 AND folder_id = $2",
    [sessionId, folderId],
  );
}

export async function removeSessionFromAllFolders(
  sessionId: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "DELETE FROM session_folders WHERE session_id = $1",
    [sessionId],
  );
}

export async function listAllSessionFolders(): Promise<DbSessionFolder[]> {
  const db = await getDb();
  return await db.select<DbSessionFolder[]>(
    "SELECT * FROM session_folders",
  );
}

// --- Tag CRUD ---

export async function createTag(
  id: string,
  name: string,
  color: string | null = null,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "INSERT INTO tags (id, name, color) VALUES ($1, $2, $3)",
    [id, name, color],
  );
}

export async function listTags(): Promise<DbTag[]> {
  const db = await getDb();
  return await db.select<DbTag[]>("SELECT * FROM tags ORDER BY name");
}

export async function getTagByName(name: string): Promise<DbTag | null> {
  const db = await getDb();
  const rows = await db.select<DbTag[]>(
    "SELECT * FROM tags WHERE name = $1 COLLATE NOCASE LIMIT 1",
    [name],
  );
  return rows[0] ?? null;
}

export async function deleteTag(id: string): Promise<void> {
  const db = await getDb();
  await db.execute("DELETE FROM tags WHERE id = $1", [id]);
}

export async function addSessionTag(
  sessionId: string,
  tagId: string,
  source: "manual" | "auto" | "ai" = "manual",
  confidence: number | null = null,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "INSERT OR IGNORE INTO session_tags (session_id, tag_id, source, confidence) VALUES ($1, $2, $3, $4)",
    [sessionId, tagId, source, confidence],
  );
}

export async function removeSessionTag(
  sessionId: string,
  tagId: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "DELETE FROM session_tags WHERE session_id = $1 AND tag_id = $2",
    [sessionId, tagId],
  );
}

export async function listAllSessionTags(): Promise<DbSessionTag[]> {
  const db = await getDb();
  return await db.select<DbSessionTag[]>("SELECT * FROM session_tags");
}

export async function getSessionTagIds(sessionId: string): Promise<string[]> {
  const db = await getDb();
  const rows = await db.select<{ tag_id: string }[]>(
    "SELECT tag_id FROM session_tags WHERE session_id = $1",
    [sessionId],
  );
  return rows.map((r) => r.tag_id);
}

export async function getSessionTagRows(
  sessionId: string,
): Promise<DbSessionTag[]> {
  const db = await getDb();
  return await db.select<DbSessionTag[]>(
    "SELECT * FROM session_tags WHERE session_id = $1",
    [sessionId],
  );
}

// --- Segment CRUD ---

export async function insertSegment(segment: DbSegment): Promise<void> {
  const db = await getDb();
  await db.execute(
    `INSERT INTO segments (id, session_id, source, text, audio_offset_seconds, chunk_duration_seconds, confidence, chunk_index, speaker_id)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)`,
    [
      segment.id,
      segment.session_id,
      segment.source,
      segment.text,
      segment.audio_offset_seconds,
      segment.chunk_duration_seconds,
      segment.confidence,
      segment.chunk_index,
      segment.speaker_id ?? null,
    ],
  );
}

export async function getSessionSegments(
  sessionId: string,
): Promise<DbSegment[]> {
  const db = await getDb();
  return await db.select<DbSegment[]>(
    "SELECT * FROM segments WHERE session_id = $1 AND deleted_at IS NULL ORDER BY audio_offset_seconds ASC",
    [sessionId],
  );
}

// --- Segment editing ---

export async function updateSegmentText(
  id: string,
  newText: string,
): Promise<void> {
  const db = await getDb();
  // Preserve original text on first edit
  await db.execute(
    `UPDATE segments
     SET original_text = CASE WHEN original_text IS NULL THEN text ELSE original_text END,
         text = $1,
         edited_at = datetime('now')
     WHERE id = $2`,
    [newText, id],
  );
}

export async function softDeleteSegment(id: string): Promise<void> {
  const db = await getDb();
  await db.execute(
    "UPDATE segments SET deleted_at = datetime('now') WHERE id = $1",
    [id],
  );
}

export async function toggleSegmentHidden(id: string): Promise<void> {
  const db = await getDb();
  await db.execute(
    "UPDATE segments SET hidden = CASE WHEN hidden = 0 THEN 1 ELSE 0 END WHERE id = $1",
    [id],
  );
}

export async function softDeleteSegments(ids: string[]): Promise<void> {
  if (ids.length === 0) return;
  const db = await getDb();
  const placeholders = ids.map((_, i) => `$${i + 1}`).join(",");
  await db.execute(
    `UPDATE segments SET deleted_at = datetime('now') WHERE id IN (${placeholders})`,
    ids,
  );
}

export async function setSegmentsHidden(
  ids: string[],
  hidden: boolean,
): Promise<void> {
  if (ids.length === 0) return;
  const db = await getDb();
  const placeholders = ids.map((_, i) => `$${i + 2}`).join(",");
  await db.execute(
    `UPDATE segments SET hidden = $1 WHERE id IN (${placeholders})`,
    [hidden ? 1 : 0, ...ids],
  );
}

// --- Notes CRUD ---

export async function getNote(sessionId: string): Promise<DbNote | null> {
  const db = await getDb();
  const rows = await db.select<DbNote[]>(
    "SELECT * FROM notes WHERE session_id = $1",
    [sessionId],
  );
  return rows[0] ?? null;
}

export async function saveNote(
  sessionId: string,
  content: string,
): Promise<void> {
  const db = await getDb();
  const existing = await getNote(sessionId);
  if (existing) {
    await db.execute(
      "UPDATE notes SET content = $1, updated_at = datetime('now') WHERE id = $2",
      [content, existing.id],
    );
  } else {
    const id = crypto.randomUUID();
    await db.execute(
      "INSERT INTO notes (id, session_id, content) VALUES ($1, $2, $3)",
      [id, sessionId, content],
    );
  }
}

export async function createNoteVersion(
  noteId: string,
  content: string,
): Promise<void> {
  const db = await getDb();
  const id = crypto.randomUUID();
  await db.execute(
    "INSERT INTO note_versions (id, note_id, content) VALUES ($1, $2, $3)",
    [id, noteId, content],
  );
}

export async function getNoteVersions(
  noteId: string,
): Promise<DbNoteVersion[]> {
  const db = await getDb();
  return await db.select<DbNoteVersion[]>(
    "SELECT * FROM note_versions WHERE note_id = $1 ORDER BY created_at DESC",
    [noteId],
  );
}

// --- Search ---

export interface SearchResult {
  type: "segment" | "note" | "session";
  sessionId: string;
  sessionTitle: string;
  snippet: string;
  /**
   * Audio source for `type === "segment"` hits. `Mic` = the user, anything
   * else came from system audio. Lets the caller render speaker
   * attribution in the snippet so the model doesn't have to assume.
   */
  source?: "Mic" | "System";
  /** Diarised speaker id for `type === "segment"` hits when present. */
  speakerId?: number | null;
}

export interface DictationSearchResult {
  dictationId: string;
  slotName: string;
  snippet: string;
  sessionId: string | null;
}

/**
 * Build a safe FTS5 MATCH expression from a free-form user query.
 * Tokenises on whitespace, drops FTS5 special characters from each token,
 * appends `*` for prefix matching, and joins tokens with implicit AND.
 * Returns null if no usable token survives — callers should short-circuit.
 */
function toFts5Match(query: string): string | null {
  const tokens = query
    .trim()
    .split(/\s+/)
    .map((t) => t.replace(/["()*^+\-:,]/g, "").trim())
    .filter((t) => t.length > 0)
    .map((t) => `"${t}"*`);
  return tokens.length > 0 ? tokens.join(" ") : null;
}

export async function searchSegments(
  query: string,
): Promise<SearchResult[]> {
  const match = toFts5Match(query);
  if (!match) return [];
  const db = await getDb();
  const rows = await db.select<
    {
      session_id: string;
      text: string;
      title: string;
      source: "Mic" | "System";
      speaker_id: number | null;
    }[]
  >(
    // `hidden = 0` matches the visibility filter used by
    // assembleTranscriptContext and get_session_context, so the AI
    // retrieval loop never surfaces a segment the user has explicitly
    // hidden from their own transcript view. `IS 0 OR IS NULL` covers
    // legacy rows where `hidden` may have been NULL before the column
    // got a default.
    `SELECT seg.session_id AS session_id, seg.text AS text, s.title AS title,
            seg.source AS source, seg.speaker_id AS speaker_id
     FROM segments_fts
     JOIN segments seg ON seg.id = segments_fts.segment_id
     JOIN sessions s ON seg.session_id = s.id
     WHERE segments_fts MATCH $1
       AND seg.deleted_at IS NULL
       AND (seg.hidden = 0 OR seg.hidden IS NULL)
     ORDER BY bm25(segments_fts)
     LIMIT 50`,
    [match],
  );
  return rows.map((r) => ({
    type: "segment",
    sessionId: r.session_id,
    sessionTitle: r.title || "Untitled",
    snippet: r.text,
    source: r.source,
    speakerId: r.speaker_id,
  }));
}

export async function searchNotes(
  query: string,
): Promise<SearchResult[]> {
  const match = toFts5Match(query);
  if (!match) return [];
  const db = await getDb();
  const rows = await db.select<
    { session_id: string; content: string; title: string }[]
  >(
    `SELECT n.session_id AS session_id, n.content AS content, s.title AS title
     FROM notes_fts
     JOIN notes n ON n.id = notes_fts.note_id
     JOIN sessions s ON n.session_id = s.id
     WHERE notes_fts MATCH $1
     ORDER BY bm25(notes_fts)
     LIMIT 50`,
    [match],
  );
  return rows.map((r) => ({
    type: "note",
    sessionId: r.session_id,
    sessionTitle: r.title || "Untitled",
    snippet: stripHtml(r.content),
  }));
}

export async function searchSessionsByTitle(
  query: string,
): Promise<SearchResult[]> {
  const match = toFts5Match(query);
  if (!match) return [];
  const db = await getDb();
  const rows = await db.select<{ id: string; title: string }[]>(
    `SELECT s.id AS id, s.title AS title
     FROM sessions_fts
     JOIN sessions s ON s.id = sessions_fts.session_id
     WHERE sessions_fts MATCH $1
     ORDER BY bm25(sessions_fts)
     LIMIT 20`,
    [match],
  );
  return rows.map((r) => ({
    type: "session",
    sessionId: r.id,
    sessionTitle: r.title || "Untitled",
    snippet: "",
  }));
}

export async function searchFolders(
  query: string,
): Promise<{ id: string; name: string }[]> {
  // Folder count is small and folder names are short; a substring LIKE is
  // simpler and good enough. Not worth the FTS5 indexing cost.
  const db = await getDb();
  const pattern = `%${query}%`;
  return await db.select<{ id: string; name: string }[]>(
    `SELECT id, name FROM folders WHERE name LIKE $1 ORDER BY name ASC LIMIT 20`,
    [pattern],
  );
}

export async function searchDictations(
  query: string,
): Promise<DictationSearchResult[]> {
  const match = toFts5Match(query);
  if (!match) return [];
  const db = await getDb();
  const rows = await db.select<
    {
      id: string;
      slot_name: string;
      input_text: string;
      output_text: string;
      session_id: string | null;
    }[]
  >(
    `SELECT d.id AS id, d.slot_name AS slot_name, d.input_text AS input_text,
            d.output_text AS output_text, d.session_id AS session_id
     FROM dictations_fts
     JOIN dictation_history d ON d.id = dictations_fts.dictation_id
     WHERE dictations_fts MATCH $1
     ORDER BY bm25(dictations_fts)
     LIMIT 50`,
    [match],
  );
  const q = query.toLowerCase();
  return rows.map((r) => {
    // Prefer the output_text snippet if the match lives there, else fall
    // back to input_text so the user sees which field actually hit.
    const source = r.output_text.toLowerCase().includes(q)
      ? r.output_text
      : r.input_text;
    return {
      dictationId: r.id,
      slotName: r.slot_name,
      snippet: source,
      sessionId: r.session_id,
    };
  });
}

// --- Sort order ---

export async function reorderFolders(
  updates: { id: string; sort_order: number }[],
): Promise<void> {
  const db = await getDb();
  for (const { id, sort_order } of updates) {
    await db.execute(
      "UPDATE folders SET sort_order = $1, updated_at = datetime('now') WHERE id = $2",
      [sort_order, id],
    );
  }
}

// --- Audio parts ---

export async function listSessionAudioParts(
  sessionId: string,
): Promise<DbAudioPart[]> {
  const db = await getDb();
  return await db.select<DbAudioPart[]>(
    "SELECT * FROM session_audio_parts WHERE session_id = $1 ORDER BY part_index ASC",
    [sessionId],
  );
}

/**
 * Returns every `file_path` registered in `session_audio_parts`. Used by
 * bulk-delete flows to avoid an N+1 of `listSessionAudioParts` per session.
 */
export async function listAllAudioPartPaths(): Promise<string[]> {
  const db = await getDb();
  const rows = await db.select<{ file_path: string }[]>(
    "SELECT file_path FROM session_audio_parts",
  );
  return rows.map((r) => r.file_path);
}

export async function insertAudioPart(part: DbAudioPart): Promise<void> {
  const db = await getDb();
  await db.execute(
    `INSERT INTO session_audio_parts (
       id, session_id, part_index, file_path, format,
       duration_seconds, sample_rate, created_at
     ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)`,
    [
      part.id,
      part.session_id,
      part.part_index,
      part.file_path,
      part.format,
      part.duration_seconds,
      part.sample_rate,
      part.created_at,
    ],
  );
}

/**
 * Returns the next `part_index` for a session — i.e. how many parts already
 * exist. Used by the resume flow to compute the next part's filename and the
 * Segment offset base.
 */
export async function nextAudioPartIndex(sessionId: string): Promise<number> {
  const db = await getDb();
  const rows = await db.select<{ count: number }[]>(
    "SELECT COUNT(*) AS count FROM session_audio_parts WHERE session_id = $1",
    [sessionId],
  );
  return rows[0]?.count ?? 0;
}

/**
 * Returns the cumulative duration of all parts for a session, used as the
 * `offset_base_seconds` for resumed Segments so their offsets stay continuous.
 */
export async function sumAudioPartsDuration(sessionId: string): Promise<number> {
  const db = await getDb();
  const rows = await db.select<{ total: number | null }[]>(
    "SELECT SUM(duration_seconds) AS total FROM session_audio_parts WHERE session_id = $1",
    [sessionId],
  );
  return rows[0]?.total ?? 0;
}

// --- Manual session ---

export async function createManualSession(
  id: string,
  title: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "INSERT INTO sessions (id, title, source, status, session_type) VALUES ($1, $2, 'MicOnly', 'completed', 'manual')",
    [id, title],
  );
}

// --- Chat messages ---

export async function insertChatMessage(msg: DbChatMessage): Promise<void> {
  const db = await getDb();
  await db.execute(
    `INSERT INTO chat_messages
       (id, context_key, session_id, role, content, action, tool_calls,
        send_id, sequence, tool_call_id, observation, status)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)`,
    [
      msg.id,
      msg.context_key,
      msg.session_id,
      msg.role,
      msg.content,
      msg.action,
      msg.tool_calls,
      msg.send_id,
      msg.sequence,
      msg.tool_call_id,
      msg.observation,
      msg.status,
    ],
  );
}

export async function getChatMessages(
  contextKey: string,
): Promise<DbChatMessage[]> {
  const db = await getDb();
  // (send_id, sequence) is the load-bearing order. Fall back to created_at
  // for legacy pre-v14 rows where sequence is NULL — COALESCE so nulls sort
  // first within their send group, matching insertion order.
  return await db.select<DbChatMessage[]>(
    `SELECT * FROM chat_messages
     WHERE context_key = $1
     ORDER BY created_at ASC, COALESCE(sequence, 0) ASC`,
    [contextKey],
  );
}

export async function updateChatMessageContent(
  id: string,
  content: string,
  toolCalls: string | null = null,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "UPDATE chat_messages SET content = $1, tool_calls = $2 WHERE id = $3",
    [content, toolCalls, id],
  );
}

export async function deleteChatMessages(
  contextKey: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "DELETE FROM chat_messages WHERE context_key = $1",
    [contextKey],
  );
}

export async function getChatMessageById(
  id: string,
): Promise<DbChatMessage | null> {
  const db = await getDb();
  const rows = await db.select<DbChatMessage[]>(
    "SELECT * FROM chat_messages WHERE id = $1 LIMIT 1",
    [id],
  );
  return rows[0] ?? null;
}

/**
 * Mark a set of tool calls as undone in a persisted send group. Used by
 * the Undo flow.
 *
 * The records stay in place so the chat keeps a visible receipt — the
 * UI renders an "undone" chip with strike-through, and the model still
 * sees the assistant.tool_calls when replaying history. What changes:
 *  - Each matching `assistant.tool_calls[]` JSON entry gets `undone: true`
 *    so the renderer can style it grayed out.
 *  - Each paired `role='tool'` row's `content` is rewritten to
 *    "(undone by user)" and `status` flipped to `"undone"`. On replay,
 *    the model sees the result was reverted and won't claim the action
 *    is still in effect.
 *
 * This is a balance: the previous implementation deleted the records
 * outright, which left assistant prose ("I pinned it") in the chat with
 * no corresponding tool chip — incoherent both visually and to the
 * model.
 */
export async function markToolCallsAsUndone(
  contextKey: string,
  sendId: string,
  callIds: string[],
): Promise<void> {
  if (callIds.length === 0) return;
  const db = await getDb();

  // 1. Rewrite tool result rows for the undone calls.
  const undoneContent = "(undone by user)";
  for (const cid of callIds) {
    await db.execute(
      `UPDATE chat_messages
       SET content = $1, status = 'undone'
       WHERE context_key = $2 AND send_id = $3
         AND role = 'tool' AND tool_call_id = $4`,
      [undoneContent, contextKey, sendId, cid],
    );
  }

  // 2. Stamp each assistant row's tool_calls JSON entry with undone=true.
  const rows = await db.select<DbChatMessage[]>(
    `SELECT * FROM chat_messages
     WHERE context_key = $1 AND send_id = $2
       AND role = 'assistant' AND tool_calls IS NOT NULL`,
    [contextKey, sendId],
  );
  const callIdSet = new Set(callIds);
  for (const row of rows) {
    if (!row.tool_calls) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(row.tool_calls);
    } catch {
      continue;
    }
    if (!Array.isArray(parsed)) continue;
    let mutated = false;
    const next = parsed.map((p) => {
      if (
        typeof p === "object" &&
        p &&
        callIdSet.has((p as { id?: string }).id ?? "")
      ) {
        mutated = true;
        return { ...(p as object), undone: true };
      }
      return p;
    });
    if (!mutated) continue;
    await db.execute(
      "UPDATE chat_messages SET tool_calls = $1 WHERE id = $2",
      [JSON.stringify(next), row.id],
    );
  }
}

// --- Chat context settings (per-chat Profile override) ---

/**
 * Returns the explicit Profile override for a chat context, or null if the
 * context has no row (in which case callers should fall back to the live
 * Chat assignment from AIConfig).
 */
export async function getChatContextProfileId(
  contextKey: string,
): Promise<string | null> {
  const db = await getDb();
  const rows = await db.select<{ profile_id: string | null }[]>(
    "SELECT profile_id FROM chat_context_settings WHERE context_key = $1 LIMIT 1",
    [contextKey],
  );
  return rows[0]?.profile_id ?? null;
}

/**
 * Persist a per-chat Profile override. Passing `null` writes a NULL row
 * (treated as "use default" on read). Use `clearChatContextProfile` to
 * remove the row entirely.
 */
export async function setChatContextProfileId(
  contextKey: string,
  profileId: string | null,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    `INSERT INTO chat_context_settings (context_key, profile_id, updated_at)
     VALUES ($1, $2, $3)
     ON CONFLICT(context_key) DO UPDATE SET
       profile_id = excluded.profile_id,
       updated_at = excluded.updated_at`,
    [contextKey, profileId, new Date().toISOString()],
  );
}

/** Remove a chat's profile override entirely (reverts to the live default). */
export async function clearChatContextProfile(
  contextKey: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "DELETE FROM chat_context_settings WHERE context_key = $1",
    [contextKey],
  );
}

/**
 * Remove every per-chat override that points at a given Profile. Called when a
 * Profile (or a Connection's dependent Profiles) is deleted — without this,
 * stale overrides would keep targeting a non-existent Profile and chat would
 * fail instead of falling back to the live Chat assignment.
 *
 * Note this matches on `profile_id`, not `context_key`: the rows we want are
 * "any chat whose override IS this profile," which is the opposite column from
 * the per-chat getters above.
 */
export async function clearChatContextProfilesByProfileId(
  profileId: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "DELETE FROM chat_context_settings WHERE profile_id = $1",
    [profileId],
  );
}

// --- Dictation history ---

export interface DbDictationHistory {
  id: string;
  slot_id: string;
  slot_name: string;
  input_text: string;
  output_text: string;
  ai_enabled: number;
  ai_prompt: string | null;
  output_action: string;
  wav_file_path: string | null;
  wav_duration_seconds: number | null;
  session_id: string | null;
  created_at: string;
}

export async function insertDictationHistory(
  entry: Omit<DbDictationHistory, "created_at">,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    `INSERT INTO dictation_history (id, slot_id, slot_name, input_text, output_text, ai_enabled, ai_prompt, output_action, wav_file_path, wav_duration_seconds, session_id)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)`,
    [
      entry.id,
      entry.slot_id,
      entry.slot_name,
      entry.input_text,
      entry.output_text,
      entry.ai_enabled ? 1 : 0,
      entry.ai_prompt,
      entry.output_action,
      entry.wav_file_path,
      entry.wav_duration_seconds,
      entry.session_id,
    ],
  );
}

export async function listDictationHistory(
  limit = 200,
): Promise<DbDictationHistory[]> {
  const db = await getDb();
  return await db.select<DbDictationHistory[]>(
    "SELECT * FROM dictation_history ORDER BY created_at DESC LIMIT $1",
    [limit],
  );
}

export async function getDictationHistoryEntry(
  id: string,
): Promise<DbDictationHistory | null> {
  const db = await getDb();
  const rows = await db.select<DbDictationHistory[]>(
    "SELECT * FROM dictation_history WHERE id = $1",
    [id],
  );
  return rows[0] ?? null;
}

export async function deleteDictationHistoryEntry(
  id: string,
): Promise<void> {
  const db = await getDb();
  await db.execute("DELETE FROM dictation_history WHERE id = $1", [id]);
}

export async function clearDictationHistory(): Promise<void> {
  const db = await getDb();
  await db.execute("DELETE FROM dictation_history");
}

export async function updateDictationHistorySessionId(
  id: string,
  sessionId: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "UPDATE dictation_history SET session_id = $1 WHERE id = $2",
    [sessionId, id],
  );
}

// --- Dictation session link cleanup ---

export async function clearDictationSessionLink(
  sessionId: string,
): Promise<void> {
  const db = await getDb();
  await db.execute(
    "UPDATE dictation_history SET session_id = NULL WHERE session_id = $1",
    [sessionId],
  );
}

