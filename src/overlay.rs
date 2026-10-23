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

    /// Run `f` in an immediate write transaction. The write lock is taken up front, so a
    /// competing writer makes this wait (up to the busy timeout) instead of failing mid-way with
    /// "database is locked", which a deferred savepoint does when it must upgrade its lock.
    fn write_tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        match f() {
            Ok(v) => { self.conn.execute_batch("COMMIT;")?; Ok(v) }
            Err(e) => { let _ = self.conn.execute_batch("ROLLBACK;"); Err(e) }
        }
    }

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
        match self.lookup_file(&p)? {
            Some(schema) => self.read_from(schema, &p),
            None => Err(anyhow!("not found in overlay: {}", p)),
        }
    }

    /// Which schema holds a regular file the overlay exposes at `p`, or None.
    ///
    /// Two primary-key probes, Delta then Base. A Delta entry is never hidden (writes clear the
    /// whiteouts that could hide it), so it needs no whiteout check. A Base entry is checked
    /// against whiteouts only after it has matched.
    fn lookup_file(&self, p: &str) -> Result<Option<&'static str>> {
        if let Some(kind) = self.kind_in("main", p)? {
            return Ok(if kind == "file" { Some("main") } else { None });
        }
        if let Some(kind) = self.kind_in("base", p)? {
            if kind != "file" || self.whited_out(p)? { return Ok(None); }
            return Ok(Some("base"));
        }
        Ok(None)
    }

    /// Write a regular file. Always goes to Delta; clears any whiteout for the path.
    pub fn write(&self, vm_path: &str, data: &[u8], mode: i64, mtime: i64) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.write_tx(|| {
            self.clear_whiteouts_for(&p)?;
            crate::vm::vm_add_bytes(&self.conn, &p, data, mode, mtime)?;
            Ok(())
        })
    }

    /// Create a directory in Delta (clears whiteouts on it and its ancestors).
    pub fn mkdir(&self, vm_path: &str) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.write_tx(|| {
            self.clear_whiteouts_for(&p)?;
            crate::vm::vm_add_dir(&self.conn, &p, 0o755, crate::vm::now_secs())?;
            Ok(())
        })
    }

    /// Create a symlink in Delta. The target is stored verbatim, as in the VM.
    pub fn symlink(&self, vm_path: &str, target: &str) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.write_tx(|| {
            self.clear_whiteouts_for(&p)?;
            crate::vm::vm_add_symlink(&self.conn, &p, target)?;
            Ok(())
        })
    }

    /// Target of a symlink as seen through the overlay.
    pub fn readlink(&self, vm_path: &str) -> Result<String> {
        let p = normalize_vm_path(vm_path);
        let (schema, layer) = match self.locate(&p)? {
            Layer::Delta => ("main", ()),
            Layer::Base => ("base", ()),
            Layer::Missing => return Err(anyhow!("not found in overlay: {}", p)),
        };
        let _ = layer;
        let target: Option<String> = self.conn.query_row(
            &format!("SELECT link_target FROM {}.vm_fs WHERE path=?1 AND kind='symlink'", schema),
            params![p], |r| r.get(0)).optional()?.flatten();
        target.ok_or_else(|| anyhow!("not a symlink: {}", p))
    }

    /// Names of the direct children of a directory, merged across layers.
    ///
    /// Three queries, not one per entry: Delta names, Base names, and the whiteouts that can hide
    /// a Base name. A whiteout on the directory or any ancestor hides every Base child; a whiteout
    /// on a child path hides that child. Delta names are never hidden by whiteouts (a write clears
    /// them), so a name present in Delta always appears.
    pub fn list(&self, vm_path: &str) -> Result<Vec<String>> {
        let p = normalize_vm_path(vm_path);
        if self.kind_in_overlay(&p)?.as_deref() != Some("dir") {
            return Err(anyhow!("not a directory: {}", p));
        }
        let prefix = if p == "/" { "/".to_string() } else { format!("{}/", p) };
        let like = format!("{}%", prefix);
        let direct = |path: &str| -> Option<String> {
            let rest = path.get(prefix.len()..)?;
            if rest.is_empty() || rest.contains('/') { None } else { Some(rest.to_string()) }
        };

        let mut delta_names = std::collections::BTreeSet::new();
        {
            let mut st = self.conn.prepare("SELECT path FROM main.vm_fs WHERE path LIKE ?1")?;
            for r in st.query_map(params![like], |r| r.get::<_, String>(0))? {
                if let Some(n) = direct(&r?) { delta_names.insert(n); }
            }
        }
        let mut base_names = std::collections::BTreeSet::new();
        {
            let mut st = self.conn.prepare("SELECT path FROM base.vm_fs WHERE path LIKE ?1")?;
            for r in st.query_map(params![like], |r| r.get::<_, String>(0))? {
                if let Some(n) = direct(&r?) { base_names.insert(n); }
            }
        }
        // whiteouts that can affect this directory's Base children: on p, on an ancestor, or below p
        let mut ancestors = vec![p.clone()];
        {
            let mut cur = p.clone();
            while let Some(i) = cur.rfind('/') {
                if i == 0 { ancestors.push("/".to_string()); break; }
                cur.truncate(i);
                ancestors.push(cur.clone());
            }
        }
        let mut hide_all = false;
        let mut hidden_children = std::collections::BTreeSet::new();
        {
            let mut st = self.conn.prepare("SELECT path FROM main.vm_whiteouts WHERE path = ?1 OR path LIKE ?2")?;
            for r in st.query_map(params![p, like], |r| r.get::<_, String>(0))? {
                let w = r?;
                if ancestors.iter().any(|a| a == &w) { hide_all = true; }
                if let Some(n) = direct(&w) { hidden_children.insert(n); }
            }
        }
        // ancestors of p can be whited out too: check them directly
        for a in &ancestors[1..] {
            if self.whited_out_exact(a)? { hide_all = true; }
        }
        if hide_all {
            return Ok(delta_names.into_iter().collect());
        }
        let mut out = delta_names;
        for n in base_names {
            if !hidden_children.contains(&n) { out.insert(n); }
        }
        Ok(out.into_iter().collect())
    }

    /// True if exactly this path has a whiteout (no ancestor walk).
    fn whited_out_exact(&self, p: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT 1 FROM main.vm_whiteouts WHERE path=?1", params![p], |_| Ok(())).optional()?.is_some())
    }

    /// Kind of a path as seen through the overlay (Delta first), or None if missing.
    fn kind_in_overlay(&self, p: &str) -> Result<Option<String>> {
        match self.locate(p)? {
            Layer::Delta => self.kind_in("main", p),
            Layer::Base => self.kind_in("base", p),
            Layer::Missing => Ok(None),
        }
    }

    /// Delete a path as seen through the overlay. Base is untouched: a whiteout hides it.
    pub fn remove(&self, vm_path: &str) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.write_tx(|| {
            // drop the Delta subtree (rows and their chunk references go with them)
            let prefix = format!("{}/%", if p == "/" { "" } else { &p });
            self.conn.execute("DELETE FROM main.vm_fs WHERE path=?1 OR path LIKE ?2", params![p, prefix])?;
            // hide whatever Base still has under this path, including descendants
            if self.in_base_subtree(&p)? {
                self.conn.execute("INSERT OR IGNORE INTO main.vm_whiteouts(path) VALUES (?1)", params![p])?;
            }
            Ok(())
        })
    }

    /// True if `p` or any ancestor of it is whited out.
    fn whited_out(&self, p: &str) -> Result<bool> {
        // p and each of its ancestors, in one statement: a whiteout on any of them hides p
        let mut paths = vec![p.to_string()];
        let mut cur = p.to_string();
        while let Some(i) = cur.rfind('/') {
            if i == 0 { paths.push("/".to_string()); break; }
            cur.truncate(i);
            paths.push(cur.clone());
        }
        let sql = format!("SELECT 1 FROM main.vm_whiteouts WHERE path IN ({}) LIMIT 1",
            vec!["?"; paths.len()].join(","));
        let params_vec: Vec<&dyn rusqlite::ToSql> = paths.iter().map(|x| x as &dyn rusqlite::ToSql).collect();
        Ok(self.conn.query_row(&sql, params_vec.as_slice(), |_| Ok(())).optional()?.is_some())
    }

    /// Make `p` visible for writing. If an ancestor of `p` is whited out, that whiteout is what
    /// hides the Base subtree below it. Clearing it must not revive those Base paths, so each
    /// Base path under the ancestor (other than `p` and its ancestors) gets its own whiteout first.
    /// Then the ancestor whiteouts are cleared and `p` becomes visible.
    fn clear_whiteouts_for(&self, p: &str) -> Result<()> {
        // ancestors of p (excluding p itself), nearest first
        let mut ancestors = Vec::new();
        let mut cur = p.to_string();
        while let Some(i) = cur.rfind('/') {
            if i == 0 { break; }
            cur.truncate(i);
            ancestors.push(cur.clone());
        }
        // which ancestors are currently whited out (directly, not via a higher ancestor)?
        let whited: Vec<String> = ancestors.iter()
            .filter(|a| self.conn.query_row("SELECT 1 FROM main.vm_whiteouts WHERE path=?1", params![a], |_| Ok(())).optional().ok().flatten().is_some())
            .cloned().collect();
        if !whited.is_empty() {
            // keep every Base path under a whited-out ancestor hidden, except p and its ancestors
            let keep_prefix = format!("{}/", p);
            for anc in &whited {
                let prefix = format!("{}/%", anc);
                let mut st = self.conn.prepare("SELECT path FROM base.vm_fs WHERE path LIKE ?1")?;
                let base_paths: Vec<String> = st.query_map(params![prefix], |r| r.get(0))?.map(|x| x.unwrap()).collect();
                for bp in base_paths {
                    if bp == p || bp.starts_with(&keep_prefix) { continue; }
                    if ancestors.iter().any(|a| a == &bp) { continue; }
                    self.conn.execute("INSERT OR IGNORE INTO main.vm_whiteouts(path) VALUES (?1)", params![bp])?;
                }
            }
        }
        // now clear whiteouts on p and its ancestors so p is visible
        self.conn.execute("DELETE FROM main.vm_whiteouts WHERE path=?1", params![p])?;
        for a in &ancestors {
            self.conn.execute("DELETE FROM main.vm_whiteouts WHERE path=?1", params![a])?;
        }
        Ok(())
    }

    /// Kind of a path in one layer, or None.
    fn kind_in(&self, schema: &str, p: &str) -> Result<Option<String>> {
        let sql = format!("SELECT kind FROM {}.vm_fs WHERE path=?1", schema);
        Ok(self.conn.query_row(&sql, params![p], |r| r.get(0)).optional()?)
    }

    fn in_base_subtree(&self, p: &str) -> Result<bool> {
        let prefix = format!("{}/%", if p == "/" { "" } else { p });
        Ok(self.conn.query_row("SELECT 1 FROM base.vm_fs WHERE path=?1 OR path LIKE ?2 LIMIT 1", params![p, prefix], |_| Ok(())).optional()?.is_some())
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
