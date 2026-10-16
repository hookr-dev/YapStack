// SPDX-License-Identifier: AGPL-3.0-only
//! The `crsql_changes` row model, its wire codec, and the read/apply/merge paths.
//!
//! A changeset is a batch of `crsql_changes` rows (T001: the 9-column vtab
//! `table, pk, cid, val, col_version, db_version, site_id, cl, seq`). Applying via
//! `INSERT INTO crsql_changes` preserves `col_version`/`site_id`/`seq` verbatim and
//! re-bases `db_version` onto the local clock (T001; proven by T003 scenario 5).
//! The codec is deterministic length-prefixed binary so the SAME bytes are what the
//! outbox encrypts and what a peer decrypts and replays.

use rusqlite::types::{Value, ValueRef};
use rusqlite::Connection;

use crate::SyncError;

/// cr-sqlite sentinel `cid` for a row-level (create/delete) change (T001:
/// `INSERT_SENTINEL` / `DELETE_SENTINEL` = `"-1"`, c.rs:10-11). It is NEVER a real
/// column, so it must always apply and is never quarantined.
pub const SENTINEL_CID: &str = "-1";

/// One `crsql_changes` row.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeRow {
    pub table: String,
    pub pk: Vec<u8>,
    pub cid: String,
    pub val: Value,
    pub col_version: i64,
    pub db_version: i64,
    pub site_id: Option<Vec<u8>>,
    pub cl: i64,
    pub seq: i64,
}

/// A serialisable batch of change rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changeset {
    pub rows: Vec<ChangeRow>,
}

// ---- codec -----------------------------------------------------------------

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

fn take_bytes(buf: &[u8], pos: &mut usize) -> Result<Vec<u8>, SyncError> {
    if buf.len() < *pos + 4 {
        return Err(SyncError::Codec("truncated length".into()));
    }
    let len = u32::from_be_bytes(buf[*pos..*pos + 4].try_into().unwrap()) as usize;
    *pos += 4;
    if buf.len() < *pos + len {
        return Err(SyncError::Codec("truncated bytes".into()));
    }
    let v = buf[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(v)
}

fn take_i64(buf: &[u8], pos: &mut usize) -> Result<i64, SyncError> {
    if buf.len() < *pos + 8 {
        return Err(SyncError::Codec("truncated i64".into()));
    }
    let v = i64::from_be_bytes(buf[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    Ok(v)
}

fn take_u8(buf: &[u8], pos: &mut usize) -> Result<u8, SyncError> {
    if buf.len() < *pos + 1 {
        return Err(SyncError::Codec("truncated u8".into()));
    }
    let v = buf[*pos];
    *pos += 1;
    Ok(v)
}

impl Changeset {
    /// Deterministic binary encoding (see module docs).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.rows.len() as u32).to_be_bytes());
        for r in &self.rows {
            put_bytes(&mut out, r.table.as_bytes());
            put_bytes(&mut out, &r.pk);
            put_bytes(&mut out, r.cid.as_bytes());
            match &r.val {
                Value::Null => out.push(0),
                Value::Integer(i) => {
                    out.push(1);
                    out.extend_from_slice(&i.to_be_bytes());
                }
                Value::Real(f) => {
                    out.push(2);
                    out.extend_from_slice(&f.to_bits().to_be_bytes());
                }
                Value::Text(s) => {
                    out.push(3);
                    put_bytes(&mut out, s.as_bytes());
                }
                Value::Blob(b) => {
                    out.push(4);
                    put_bytes(&mut out, b);
                }
            }
            out.extend_from_slice(&r.col_version.to_be_bytes());
            out.extend_from_slice(&r.db_version.to_be_bytes());
            match &r.site_id {
                None => out.push(0),
                Some(s) => {
                    out.push(1);
                    put_bytes(&mut out, s);
                }
            }
            out.extend_from_slice(&r.cl.to_be_bytes());
            out.extend_from_slice(&r.seq.to_be_bytes());
        }
        out
    }

    /// Decode bytes produced by [`Changeset::encode`].
    pub fn decode(buf: &[u8]) -> Result<Self, SyncError> {
        let mut pos = 0usize;
        if buf.len() < 4 {
            return Err(SyncError::Codec("truncated header".into()));
        }
        let n = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
        pos += 4;
        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            let table = String::from_utf8(take_bytes(buf, &mut pos)?)
                .map_err(|_| SyncError::Codec("table not utf8".into()))?;
            let pk = take_bytes(buf, &mut pos)?;
            let cid = String::from_utf8(take_bytes(buf, &mut pos)?)
                .map_err(|_| SyncError::Codec("cid not utf8".into()))?;
            let tag = take_u8(buf, &mut pos)?;
            let val = match tag {
                0 => Value::Null,
                1 => Value::Integer(take_i64(buf, &mut pos)?),
                2 => Value::Real(f64::from_bits(take_i64(buf, &mut pos)? as u64)),
                3 => Value::Text(
                    String::from_utf8(take_bytes(buf, &mut pos)?)
                        .map_err(|_| SyncError::Codec("text not utf8".into()))?,
                ),
                4 => Value::Blob(take_bytes(buf, &mut pos)?),
                other => return Err(SyncError::Codec(format!("bad value tag {other}"))),
            };
            let col_version = take_i64(buf, &mut pos)?;
            let db_version = take_i64(buf, &mut pos)?;
            let site_id = match take_u8(buf, &mut pos)? {
                0 => None,
                1 => Some(take_bytes(buf, &mut pos)?),
                other => return Err(SyncError::Codec(format!("bad site tag {other}"))),
            };
            let cl = take_i64(buf, &mut pos)?;
            let seq = take_i64(buf, &mut pos)?;
            rows.push(ChangeRow {
                table,
                pk,
                cid,
                val,
                col_version,
                db_version,
                site_id,
                cl,
                seq,
            });
        }
        Ok(Self { rows })
    }
}

fn value_from_ref(r: ValueRef<'_>) -> Value {
    match r {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Integer(i),
        ValueRef::Real(f) => Value::Real(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// Read locally-authored changes newer than `since_db_version`.
///
/// Local rows carry `site_id = crsql_site_id()` (verified in bring-up), so this
/// selects exactly this device's own writes and never re-emits applied remote
/// changes (whose `site_id` is a peer's). Ordered by `(db_version, seq)`.
pub fn read_local_changes_since(
    conn: &Connection,
    since_db_version: i64,
) -> Result<Changeset, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT \"table\", pk, cid, val, col_version, db_version, site_id, cl, seq \
         FROM crsql_changes \
         WHERE site_id = crsql_site_id() AND db_version > ?1 \
         ORDER BY db_version, seq",
    )?;
    let rows = stmt
        .query_map([since_db_version], |r| {
            Ok(ChangeRow {
                table: r.get(0)?,
                pk: r.get(1)?,
                cid: r.get(2)?,
                val: value_from_ref(r.get_ref(3)?),
                col_version: r.get(4)?,
                db_version: r.get(5)?,
                site_id: r.get(6)?,
                cl: r.get(7)?,
                seq: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Changeset { rows })
}

/// The current local max `db_version` (the outbox push watermark).
pub fn max_local_db_version(conn: &Connection) -> Result<i64, SyncError> {
    let v: i64 = conn.query_row(
        "SELECT coalesce(max(db_version),0) FROM crsql_changes WHERE site_id = crsql_site_id()",
        [],
        |r| r.get(0),
    )?;
    Ok(v)
}

/// Apply a single change row via `INSERT INTO crsql_changes`, preserving its clock.
/// Caller guarantees the target column exists (else quarantine first — see
/// `quarantine`).
pub fn apply_change(conn: &Connection, r: &ChangeRow) -> Result<(), SyncError> {
    conn.execute(
        "INSERT INTO crsql_changes \
         (\"table\", pk, cid, val, col_version, db_version, site_id, cl, seq) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        rusqlite::params![
            r.table,
            r.pk,
            r.cid,
            r.val,
            r.col_version,
            r.db_version,
            r.site_id,
            r.cl,
            r.seq
        ],
    )?;
    Ok(())
}
