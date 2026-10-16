// SPDX-License-Identifier: AGPL-3.0-only
//! Shared test fixtures: a faithful pre-CRR copy of the real local schema (FKs,
//! non-PK UNIQUEs, NOT-NULL-no-default columns — the exact `is_table_compatible`
//! hazards), a populator, and a deterministic content fingerprint.
#![allow(dead_code)]

use rusqlite::Connection;

/// Create the 12 sync tables in their ORIGINAL (pre-CRR) shape — column order
/// identical to `schema::rebuild_body` but carrying the real constraints the
/// migration must strip. This is what a `.backup` copy of the live DB looks like.
pub fn create_original_schema(conn: &Connection) {
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            source TEXT NOT NULL DEFAULT 'Mixed',
            status TEXT NOT NULL DEFAULT 'recording',
            duration_seconds REAL,
            total_segments INTEGER NOT NULL DEFAULT 0,
            folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
            is_pinned INTEGER NOT NULL DEFAULT 0,
            pinned_at TEXT,
            session_type TEXT NOT NULL DEFAULT 'transcription',
            wav_file_path TEXT,
            wav_duration_seconds REAL,
            sort_order INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE segments (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            source TEXT NOT NULL,
            text TEXT NOT NULL,
            audio_offset_seconds REAL NOT NULL,
            chunk_duration_seconds REAL NOT NULL,
            confidence REAL NOT NULL DEFAULT 1.0,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            chunk_index INTEGER NOT NULL DEFAULT 0,
            original_text TEXT,
            edited_at TEXT,
            deleted_at TEXT,
            hidden INTEGER NOT NULL DEFAULT 0,
            speaker_id INTEGER
        );
        CREATE TABLE notes (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id) ON DELETE CASCADE,
            content TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE note_versions (
            id TEXT PRIMARY KEY,
            note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE folders (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            parent_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
            sort_order INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            icon TEXT,
            color TEXT,
            description TEXT
        );
        CREATE TABLE session_folders (
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (session_id, folder_id)
        );
        CREATE TABLE chat_messages (
            id TEXT PRIMARY KEY,
            context_key TEXT NOT NULL,
            session_id TEXT REFERENCES sessions(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            action TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            tool_calls TEXT,
            send_id TEXT,
            sequence INTEGER,
            tool_call_id TEXT,
            observation TEXT,
            status TEXT
        );
        CREATE TABLE dictation_history (
            id TEXT PRIMARY KEY,
            slot_id TEXT NOT NULL,
            slot_name TEXT NOT NULL,
            input_text TEXT NOT NULL,
            output_text TEXT NOT NULL,
            ai_enabled INTEGER NOT NULL DEFAULT 0,
            ai_prompt TEXT,
            output_action TEXT NOT NULL,
            wav_file_path TEXT,
            wav_duration_seconds REAL,
            session_id TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE tags (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE,
            color TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE session_tags (
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            source TEXT NOT NULL DEFAULT 'manual',
            confidence REAL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (session_id, tag_id)
        );
        CREATE TABLE session_audio_parts (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            part_index INTEGER NOT NULL,
            file_path TEXT NOT NULL,
            format TEXT NOT NULL CHECK (format IN ('wav','mp3')),
            duration_seconds REAL NOT NULL,
            sample_rate INTEGER NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE (session_id, part_index)
        );
        CREATE TABLE shares (
            id TEXT PRIMARY KEY,
            folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
            shared_with_email TEXT,
            permission TEXT NOT NULL DEFAULT 'viewer',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            expires_at TEXT
        );
        "#,
    )
    .unwrap();
}

/// Populate the original schema with a few hundred inter-referential rows,
/// exercising nullable FKs, unicode, the CHECK-constrained format column, and the
/// original UNIQUE constraints (one note per session, unique tag names).
pub fn populate(conn: &Connection) {
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    conn.execute(
        "INSERT INTO folders(id,name) VALUES ('f1','Work'),('f2','Personal')",
        [],
    )
    .unwrap();
    conn.execute("UPDATE folders SET parent_id='f1' WHERE id='f2'", [])
        .unwrap();
    conn.execute(
        "INSERT INTO tags(id,name) VALUES ('t1','urgent'),('t2','idea'),('t3','café ☕')",
        [],
    )
    .unwrap();
    for s in 0..8 {
        let sid = format!("s{s}");
        conn.execute(
            "INSERT INTO sessions(id,title,folder_id) VALUES (?1,?2,?3)",
            rusqlite::params![
                sid,
                format!("Session {s} — naïve"),
                if s % 2 == 0 { Some("f1") } else { None }
            ],
        )
        .unwrap();
        for g in 0..25 {
            conn.execute(
                "INSERT INTO segments(id,session_id,source,text,audio_offset_seconds,chunk_duration_seconds) \
                 VALUES (?1,?2,'mic',?3,?4,1.0)",
                rusqlite::params![format!("seg-{s}-{g}"), sid, format!("hello {g} 世界"), g as f64],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO notes(id,session_id,content) VALUES (?1,?2,?3)",
            rusqlite::params![format!("n{s}"), sid, format!("note body {s}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO note_versions(id,note_id,content) VALUES (?1,?2,'v1')",
            rusqlite::params![format!("nv{s}"), format!("n{s}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_audio_parts(id,session_id,part_index,file_path,format,duration_seconds,sample_rate) \
             VALUES (?1,?2,0,?3,'wav',1.0,16000)",
            rusqlite::params![format!("ap{s}"), sid, format!("/audio/{s}.wav")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_tags(session_id,tag_id) VALUES (?1,'t1')",
            [&sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_messages(id,context_key,session_id,role,content) VALUES (?1,?2,?3,'user','hi')",
            rusqlite::params![format!("cm{s}"), format!("ctx{s}"), sid],
        )
        .unwrap();
    }
    conn.execute("INSERT INTO shares(id,folder_id) VALUES ('sh1','f1')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO dictation_history(id,slot_id,slot_name,input_text,output_text,output_action) \
         VALUES ('d1','slot','Slot','in','out','paste')",
        [],
    )
    .unwrap();
}

/// Deterministic (count, FNV-1a hash) fingerprint over ordered rows using `quote()`
/// of every column, so any count/PK/value drift changes the hash.
pub fn fingerprint(conn: &Connection, table: &str) -> (i64, u64) {
    let cols: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .unwrap();
        stmt.query_map([table], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect()
    };
    let pk: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info(?1) WHERE pk>0 ORDER BY pk")
            .unwrap();
        stmt.query_map([table], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect()
    };
    let concat = cols
        .iter()
        .map(|c| format!("quote(\"{c}\")"))
        .collect::<Vec<_>>()
        .join("||char(31)||");
    let order = if pk.is_empty() {
        "1".to_string()
    } else {
        pk.iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(",")
    };
    let sql = format!("SELECT {concat} AS r FROM \"{table}\" ORDER BY {order}");
    let mut stmt = conn.prepare(&sql).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut n: i64 = 0;
    let mut h: u64 = 0xcbf29ce484222325;
    while let Some(row) = rows.next().unwrap() {
        let r: String = row.get(0).unwrap();
        for b in r.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
        n += 1;
    }
    (n, h)
}

/// Merge locally-authored changes from `from` into `to` via `INSERT INTO
/// crsql_changes` (mirrors the runtime pull path for direct two-DB tests).
pub fn direct_merge(from: &Connection, to: &Connection, since: i64) -> i64 {
    let to_site: Vec<u8> = to
        .query_row("SELECT crsql_site_id()", [], |r| r.get(0))
        .unwrap();
    to.execute_batch("BEGIN;").unwrap();
    let mut ins = to
        .prepare(
            "INSERT INTO crsql_changes \
             (\"table\",pk,cid,val,col_version,db_version,site_id,cl,seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        )
        .unwrap();
    let mut sel = from
        .prepare(
            "SELECT \"table\",pk,cid,val,col_version,db_version,site_id,cl,seq \
             FROM crsql_changes WHERE db_version > ?1 AND site_id IS NOT ?2",
        )
        .unwrap();
    let mut rows = sel.query(rusqlite::params![since, to_site]).unwrap();
    let mut n = 0i64;
    while let Some(r) = rows.next().unwrap() {
        ins.execute(rusqlite::params![
            r.get::<_, String>(0).unwrap(),
            r.get::<_, Vec<u8>>(1).unwrap(),
            r.get::<_, String>(2).unwrap(),
            r.get::<_, rusqlite::types::Value>(3).unwrap(),
            r.get::<_, i64>(4).unwrap(),
            r.get::<_, i64>(5).unwrap(),
            r.get::<_, Option<Vec<u8>>>(6).unwrap(),
            r.get::<_, i64>(7).unwrap(),
            r.get::<_, i64>(8).unwrap(),
        ])
        .unwrap();
        n += 1;
    }
    drop(rows);
    drop(sel);
    drop(ins);
    to.execute_batch("COMMIT;").unwrap();
    n
}
