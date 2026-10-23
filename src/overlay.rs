//! Read-only Base + writable Delta overlay, built on SQLite ATTACH.
//!
//! * Base is attached as `base` and is never written.
//! * Delta is `main`. It holds additions and overwrites in its own vm_fs / vm_file_chunks /
//!   vm_blobs, and deletions as rows in `vm_whiteouts`.
//! * A path resolves in Delta first (a whiteout hides it entirely), then in Base.
//!
//! Scope of this prototype: regular files only. Directories are not represented in the overlay
//! yet, and symlinks are not followed.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

use crate::vm::{decode_block_in, normalize_vm_path, vm_open, blob_key};

pub struct Overlay { conn: Connection }

/// Which layer answered a lookup.
#[derive(Debug, PartialEq, Eq)]
pub enum Layer { Delta, Base, Missing }

impl Overlay {
    /// Open (or create) a Delta and attach a Base read-only.
    pub fn open(delta: &Path, base: &Path) -> Result<Self> {
        let delta_s = delta.to_str().ok_or_else(|| anyhow!("non-utf8 delta path"))?;
        // a missing Delta is created empty; an existing file must already be a VM database
        let conn = if delta.exists() {
            vm_open(delta_s)?
        } else {
            crate::vm::init_vm_db_with_opts(delta_s, false, true)?
        };
        conn.execute_batch("CREATE TABLE IF NOT EXISTS vm_whiteouts(path TEXT PRIMARY KEY) WITHOUT ROWID;")?;
        // Attach the base through a read-only URI. SQLite enforces this in the engine: any write
        // to the `base` schema fails with "attempt to write a readonly database", whatever the
        // caller does with the connection. This is not a convention in this module.
        let base_s = base.to_str().ok_or_else(|| anyhow!("non-utf8 base path"))?;
        if base_s.contains('?') || base_s.contains('#') { return Err(anyhow!("base path must not contain '?' or '#'")); }
        let uri = format!("file:{}?mode=ro", base_s);
        conn.execute("ATTACH DATABASE ?1 AS base", params![uri])?;
        // a base must be a vm database; reject anything else before reading through it
        let app: i64 = conn.query_row("PRAGMA base.application_id", [], |r| r.get(0))?;
        if app as u32 != crate::vm::VM_APP_ID {
            return Err(anyhow!("base is not a VM database (application_id {:#x})", app));
        }
        Ok(Overlay { conn })
    }

    pub fn conn(&self) -> &Connection { &self.conn }

    /// Where a path currently resolves.
    pub fn locate(&self, vm_path: &str) -> Result<Layer> {
        let p = normalize_vm_path(vm_path);
        if self.whited_out(&p)? { return Ok(Layer::Missing); }
        if self.in_layer("main", &p)? { return Ok(Layer::Delta); }
        if self.in_layer("base", &p)? { return Ok(Layer::Base); }
        Ok(Layer::Missing)
    }

    pub fn exists(&self, vm_path: &str) -> Result<bool> { Ok(self.locate(vm_path)? != Layer::Missing) }

    /// Read a regular file through the overlay.
    pub fn read(&self, vm_path: &str) -> Result<Vec<u8>> {
        let p = normalize_vm_path(vm_path);
        match self.locate(&p)? {
            Layer::Delta => self.read_from("main", &p),
            Layer::Base => self.read_from("base", &p),
            Layer::Missing => Err(anyhow!("not found in overlay: {}", p)),
        }
    }

    /// Write a regular file. Always goes to Delta; clears any whiteout for the path.
    pub fn write(&self, vm_path: &str, data: &[u8], mode: i64, mtime: i64) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.conn.execute_batch("SAVEPOINT ov_write;")?;
        let res: Result<()> = (|| {
            self.conn.execute("DELETE FROM main.vm_whiteouts WHERE path=?1", params![p])?;
            crate::vm::vm_add_bytes(&self.conn, &p, data, mode, mtime)?;
            Ok(())
        })();
        match res {
            Ok(()) => { self.conn.execute_batch("RELEASE ov_write;")?; Ok(()) }
            Err(e) => { let _ = self.conn.execute_batch("ROLLBACK TO ov_write; RELEASE ov_write;"); Err(e) }
        }
    }

    /// Delete a path as seen through the overlay. Base is untouched: a whiteout hides it.
    pub fn remove(&self, vm_path: &str) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.conn.execute_batch("SAVEPOINT ov_remove;")?;
        let res: Result<()> = (|| {
            self.conn.execute("DELETE FROM main.vm_fs WHERE path=?1", params![p])?;
            if self.in_layer("base", &p)? {
                self.conn.execute("INSERT OR IGNORE INTO main.vm_whiteouts(path) VALUES (?1)", params![p])?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => { self.conn.execute_batch("RELEASE ov_remove;")?; Ok(()) }
            Err(e) => { let _ = self.conn.execute_batch("ROLLBACK TO ov_remove; RELEASE ov_remove;"); Err(e) }
        }
    }

    fn whited_out(&self, p: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT 1 FROM main.vm_whiteouts WHERE path=?1", params![p], |_| Ok(())).optional()?.is_some())
    }

    fn in_layer(&self, schema: &str, p: &str) -> Result<bool> {
        let sql = format!("SELECT 1 FROM {}.vm_fs WHERE path=?1", schema);
        Ok(self.conn.query_row(&sql, params![p], |_| Ok(())).optional()?.is_some())
    }

    /// Assemble a file's bytes from one schema's chunk list. Every chunk is verified against its
    /// key, so a corrupt block is an error rather than wrong data.
    fn read_from(&self, schema: &str, p: &str) -> Result<Vec<u8>> {
        let kind: String = self.conn.query_row(&format!("SELECT kind FROM {}.vm_fs WHERE path=?1", schema), params![p], |r| r.get(0))?;
        if kind != "file" { return Err(anyhow!("not a regular file: {}", p)); }
        let sql = format!(
            "SELECT c.idx, b.content, b.compressed, b.dict_id, c.hash FROM {s}.vm_file_chunks c
             JOIN {s}.vm_blobs b ON b.hash=c.hash WHERE c.path=?1 ORDER BY c.idx", s = schema);
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![p], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, Vec<u8>>(4)?)))?;
        let mut out = Vec::new();
        let mut expect = 0i64;
        for row in rows {
            let (idx, body, comp, dict_id, key) = row?;
            if idx != expect { return Err(anyhow!("{}: chunk index gap at {}", p, expect)); }
            expect += 1;
            // a block's dictionary lives in the same schema as the block (ids are per database)
            let piece = decode_block_in(&self.conn, schema, &body, comp, dict_id)?;
            if blob_key(&piece).as_slice() != key.as_slice() { return Err(anyhow!("{}: chunk {} hash mismatch", p, idx)); }
            out.extend_from_slice(&piece);
        }
        Ok(out)
    }
}
