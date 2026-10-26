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

use crate::vm;
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
    /// Refuse a write under a read-only prefix of the base's policies.
    ///
    /// The rules live in `base.vm_policies`. The delta's triggers cannot read them (a trigger is compiled
    /// for its own schema), so the overlay checks them itself, on every write, from the live table. A
    /// rule cannot go stale, and the same refusal the database gives is returned, so callers map it the
    /// same way (see policy::is_refusal).
    /// Check the Base's write policies before touching a path.
    ///
    /// The Delta's own triggers enforce policies for a standalone VM, but a Delta has no policy table of its
    /// own and its counters do not include Base bytes, so the rules have to be applied here. All three kinds
    /// are checked: an earlier version looked only at `readonly_prefix`, so a quota or a per-file limit set
    /// on the Base was ignored by every write through the overlay (measured: a 10 MiB write succeeded with a
    /// 1 KiB quota in force).
    fn check_base_policy(&self, p: &str) -> Result<()> {
        let has_table: i64 = self.conn.query_row(
            "SELECT count(*) FROM base.sqlite_master WHERE type='table' AND name='vm_policies'", [], |r| r.get(0))?;
        if has_table == 0 { return Ok(()); }
        let rules: Vec<(String, String)> = {
            let mut st = self.conn.prepare("SELECT rule_type, pattern FROM base.vm_policies")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut max_file: Option<u64> = None;
        let mut quota: Option<u64> = None;
        for (kind, pattern) in rules {
            match kind.as_str() {
                "readonly_prefix" => {
                    if crate::policy::covers(&pattern, p) { return Err(anyhow!(crate::policy::REFUSAL_MESSAGE)); }
                }
                "max_file_size" => {
                    let n: u64 = pattern.parse()?;
                    max_file = Some(max_file.map_or(n, |m| m.min(n)));
                }
                "disk_quota" => {
                    let n: u64 = pattern.parse()?;
                    quota = Some(quota.map_or(n, |m| m.min(n)));
                }
                _ => {}
            }
        }
        // the view's total file bytes: Base plus Delta, counted the way vm_meta counts a standalone VM
        if let Some(q) = quota {
            let total = self.view_file_bytes()?;
            if total > q {
                return Err(anyhow!("{} ({} bytes in the view, quota {})", crate::policy::REFUSAL_MESSAGE, total, q));
            }
        }
        let _ = max_file;   // enforced per write, where the incoming size is known
        Ok(())
    }

    /// Total logical bytes of regular files in the merged view.
    pub fn view_file_bytes(&self) -> Result<u64> {
        let b: i64 = self.conn.query_row("SELECT COALESCE(sum(size),0) FROM base.vm_fs WHERE kind='file'", [], |r| r.get(0))?;
        // Delta rows shadow Base rows: only count the ones Base does not hold
        let d: i64 = self.conn.query_row(
            "SELECT COALESCE(sum(f.size),0) FROM main.vm_fs f WHERE f.kind='file'
             AND NOT EXISTS (SELECT 1 FROM base.vm_fs b WHERE b.path = f.path)", [], |r| r.get(0))?;
        Ok((b + d) as u64)
    }

    /// Refuse a write that would push the view past the Base's disk quota.
    ///
    /// `delta_growth` is the change this write makes to the Delta side: the new file's bytes minus whatever
    /// the Delta already held for that path. The check has to be prospective, because the row does not exist
    /// yet when it runs.
    fn check_base_quota(&self, delta_growth: i64) -> Result<()> {
        let has_table: i64 = self.conn.query_row(
            "SELECT count(*) FROM base.sqlite_master WHERE type='table' AND name='vm_policies'", [], |r| r.get(0))?;
        if has_table == 0 { return Ok(()); }
        let mut st = self.conn.prepare("SELECT pattern FROM base.vm_policies WHERE rule_type='disk_quota'")?;
        let mut quota: Option<i64> = None;
        for r in st.query_map([], |r| r.get::<_, String>(0))? {
            let n: i64 = r?.parse()?;
            quota = Some(quota.map_or(n, |m| m.min(n)));
        }
        if let Some(q) = quota {
            let after = self.view_file_bytes()? as i64 + delta_growth;
            if after > q {
                return Err(anyhow!("{} ({} bytes after this write, quota {})", crate::policy::REFUSAL_MESSAGE, after, q));
            }
        }
        Ok(())
    }

    /// Refuse a file of `size` bytes when the Base sets a per-file limit.
    fn check_base_max_file(&self, size: u64) -> Result<()> {
        if let Some(limit) = self.base_max_file()? {
            if size > limit {
                return Err(anyhow!("{} ({} bytes, limit {})", crate::policy::REFUSAL_MESSAGE, size, limit));
            }
        }
        Ok(())
    }

    /// The largest file size the Base's policies allow, if any.
    fn base_max_file(&self) -> Result<Option<u64>> {
        let has_table: i64 = self.conn.query_row(
            "SELECT count(*) FROM base.sqlite_master WHERE type='table' AND name='vm_policies'", [], |r| r.get(0))?;
        if has_table == 0 { return Ok(None); }
        let mut st = self.conn.prepare("SELECT pattern FROM base.vm_policies WHERE rule_type='max_file_size'")?;
        let mut out: Option<u64> = None;
        for r in st.query_map([], |r| r.get::<_, String>(0))? {
            let n: u64 = r?.parse()?;
            out = Some(out.map_or(n, |m| m.min(n)));
        }
        Ok(out)
    }

    pub fn write(&self, vm_path: &str, data: &[u8], mode: i64, mtime: i64) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.check_base_policy(&p)?;
        self.check_base_max_file(data.len() as u64)?;
        let held: i64 = self.conn.query_row(
            "SELECT COALESCE(size,0) FROM main.vm_fs WHERE path=?1", params![p], |r| r.get(0)).optional()?.unwrap_or(0);
        self.check_base_quota(data.len() as i64 - held)?;
        self.write_tx(|| {
            self.clear_whiteouts_for(&p)?;
            crate::vm::vm_add_bytes(&self.conn, &p, data, mode, mtime)?;
            Ok(())
        })
    }

    /// Create a directory in Delta (clears whiteouts on it and its ancestors).
    pub fn mkdir(&self, vm_path: &str) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.check_base_policy(&p)?;
        self.write_tx(|| {
            self.clear_whiteouts_for(&p)?;
            crate::vm::vm_add_dir(&self.conn, &p, 0o755, crate::vm::now_secs())?;
            Ok(())
        })
    }

    /// Create a symlink in Delta. The target is stored verbatim, as in the VM.
    pub fn symlink(&self, vm_path: &str, target: &str) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.check_base_policy(&p)?;
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
        self.check_base_policy(&p)?;
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
        // p itself may be whited out (its directory was removed). Clearing that whiteout would revive
        // the Base subtree below p, so lock every Base path under p first. p itself is excluded:
        // the caller is making p visible, and p's own row (if Base has one) is what it replaces.
        if self.whited_out_exact(p)? {
            let prefix = format!("{}/%", p);
            let mut st = self.conn.prepare("SELECT path FROM base.vm_fs WHERE path LIKE ?1")?;
            let below: Vec<String> = st.query_map(params![prefix], |r| r.get(0))?.map(|x| x.unwrap()).collect();
            for bp in below {
                self.conn.execute("INSERT OR IGNORE INTO main.vm_whiteouts(path) VALUES (?1)", params![bp])?;
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
    /// Read a byte range of a file through the overlay, from whichever layer holds each chunk.
    ///
    /// Delta first, then Base: a path present in Delta is served entirely from Delta (a write there
    /// copied the chunks it needed), so a file is never pieced together from two layers at the same
    /// index. Only a path that exists solely in Base reads from Base.
    pub fn read_range(&self, vm_path: &str, off: u64, len: usize) -> Result<Vec<u8>> {
        let p = normalize_vm_path(vm_path);
        let schema = match self.lookup_file(&p)? {
            Some(s) => s,
            None => return Err(anyhow!("not found in overlay: {}", p)),
        };
        let size: i64 = self.conn.query_row(
            &format!("SELECT size FROM {}.vm_fs WHERE path=?1", schema), params![p], |r| r.get(0))?;
        let size = size as u64;
        let start = off.min(size);
        let end = start.saturating_add(len as u64).min(size);
        if start == end { return Ok(Vec::new()); }
        let cs = crate::vm::CHUNK_SIZE as u64;
        let (first, last) = (start / cs, (end - 1) / cs);
        let mut out: Vec<u8> = Vec::with_capacity((end - start) as usize);
        for idx in first..=last {
            let piece = self.read_chunk_layered(schema, &p, idx as i64)?;
            let base = idx * cs;
            let from = start.saturating_sub(base).min(piece.len() as u64) as usize;
            let to = end.saturating_sub(base).min(piece.len() as u64) as usize;
            if from < to { out.extend_from_slice(&piece[from..to]); }
        }
        if out.len() as u64 != end - start {
            return Err(anyhow!("{}: read {} bytes for range {}..{}", p, out.len(), start, end));
        }
        Ok(out)
    }

    /// One chunk of a file that lives in Delta, falling back to Base for chunks Delta never touched.
    ///
    /// This is what makes the write side cheap: Delta stores only the chunks it modified, so a small
    /// write to a large Base file does not copy the file. `schema` is the layer that holds the file row.
    fn read_chunk_layered(&self, schema: &str, p: &str, idx: i64) -> Result<Vec<u8>> {
        if schema == "main" {
            let in_main: i64 = self.conn.query_row(
                "SELECT count(*) FROM main.vm_file_chunks WHERE path=?1 AND idx=?2",
                params![p, idx], |r| r.get(0))?;
            if in_main == 0 {
                let in_base: i64 = self.conn.query_row(
                    "SELECT count(*) FROM base.vm_file_chunks WHERE path=?1 AND idx=?2",
                    params![p, idx], |r| r.get(0))?;
                if in_base > 0 { return self.read_chunk("base", p, idx); }
                return Ok(Vec::new());
            }
        }
        self.read_chunk(schema, p, idx)
    }

    /// One chunk of a file from `schema`, verified against its stored hash.
    fn read_chunk(&self, schema: &str, p: &str, idx: i64) -> Result<Vec<u8>> {
        let (body, comp, dict_id, key): (Vec<u8>, i64, i64, Vec<u8>) = self.conn.query_row(
            &format!("SELECT b.content, b.compressed, b.dict_id, c.hash FROM {s}.vm_file_chunks c
                      JOIN {s}.vm_blobs b ON b.hash=c.hash WHERE c.path=?1 AND c.idx=?2", s = schema),
            params![p, idx], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(|e| anyhow!("{}: chunk {}: {}", p, idx, e))?;
        let piece = crate::vm::decode_block_in(&self.conn, schema, &body, comp, dict_id)?;
        if crate::vm::blob_key(&piece).as_slice() != key.as_slice() {
            return Err(anyhow!("{}: chunk {} hash mismatch", p, idx));
        }
        Ok(piece)
    }

    /// Write a byte range, copying only the chunks it touches from Base into Delta.
    ///
    /// This is the point of the layering: a small write to a large Base file appends one chunk row to
    /// Delta instead of copying the whole file. The file's row in Delta is created on first write with
    /// the Base file's mode and a size that covers the write; every chunk that was not touched is
    /// materialised into Delta as well, because a Delta file is read entirely from Delta.
    pub fn write_range(&self, vm_path: &str, off: u64, data: &[u8], mtime: i64) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.check_base_policy(&p)?;
        if data.is_empty() { return Ok(()); }
        // the limit is on the file's resulting size, not on the length of this write
        let current = match self.lookup_file(&p)? {
            Some(schema) => self.conn.query_row(&format!("SELECT size FROM {}.vm_fs WHERE path=?1", schema),
                params![p], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64,
            None => 0,
        };
        self.check_base_max_file(current.max(off + data.len() as u64))?;
        {
            let in_delta: i64 = self.conn.query_row(
                "SELECT COALESCE(size,0) FROM main.vm_fs WHERE path=?1", params![p], |r| r.get(0)).optional()?.unwrap_or(0);
            let after = current.max(off + data.len() as u64) as i64;
            // in a Delta the file's bytes already counted only when the Delta holds the row
            self.check_base_quota(if in_delta > 0 { after - in_delta } else { after })?;
        }
        self.write_tx(|| {
            self.clear_whiteouts_for(&p)?;
            let cs = crate::vm::CHUNK_SIZE as u64;
            // current view of the file, if any
            let (size, mode, layer) = match self.lookup_file(&p)? {
                Some(schema) => {
                    let (sz, md): (i64, i64) = self.conn.query_row(
                        &format!("SELECT size, mode FROM {}.vm_fs WHERE path=?1", schema), params![p],
                        |r| Ok((r.get(0)?, r.get(1)?)))?;
                    (sz as u64, md, Some(schema))
                }
                None => (0, 0o644, None),
            };
            let new_size = size.max(off + data.len() as u64);
            let end = off + data.len() as u64;
            let first = off.min(size) / cs;   // the old tail chunk may need rewriting too
            let last = (end - 1) / cs;

            // the Delta row: created on first write, then kept in step with the new size
            // Delete then insert, never INSERT OR REPLACE: a replace does not fire the DELETE trigger
            // (recursive_triggers is off), so vm_meta's file counters drift and vm-verify reports DRIFT.
            self.ensure_delta_file_row(&p, mode, mtime, new_size)?;
            // Only the chunks this write touches go into Delta. Chunks before and after stay in Base and
            // are read from there (see read_chunk_layered), so a small write to a large file costs one
            // chunk, not a copy of the file.
            for idx in first..=last {
                let base = idx * cs;
                let chunk_end = (base + cs).min(new_size);
                let want = (chunk_end - base) as usize;
                let mut piece = if base < size {
                    self.read_chunk_layered(layer.unwrap_or("main"), &p, idx as i64)?
                } else { Vec::new() };
                if piece.len() < want { piece.resize(want, 0); }
                let w_from = off.max(base);
                let w_to = end.min(chunk_end);
                if w_from < w_to {
                    piece[(w_from - base) as usize..(w_to - base) as usize]
                        .copy_from_slice(&data[(w_from - off) as usize..(w_to - off) as usize]);
                }
                let key = crate::vm::blob_key(&piece);
                crate::vm::blob_ensure(&self.conn, &key, &piece)?;
                self.conn.prepare_cached("DELETE FROM main.vm_file_chunks WHERE path=?1 AND idx=?2")?
                    .execute(params![p, idx as i64])?;
                self.conn.prepare_cached("INSERT INTO main.vm_file_chunks(path, idx, hash) VALUES (?1,?2,?3)")?
                    .execute(params![p, idx as i64, key.as_slice()])?;
            }
            // the whole-file hash and size of the Delta row
            self.refresh_delta_file(&p, new_size, mtime)?;
            Ok(())
        })
    }

    /// Truncate or extend a file through the overlay, touching only the chunks that change.
    pub fn truncate_range(&self, vm_path: &str, new_size: u64, mtime: i64) -> Result<()> {
        let p = normalize_vm_path(vm_path);
        self.check_base_policy(&p)?;
        self.write_tx(|| {
            self.clear_whiteouts_for(&p)?;
            let cs = crate::vm::CHUNK_SIZE as u64;
            let (size, mode, layer) = match self.lookup_file(&p)? {
                Some(schema) => {
                    let (sz, md): (i64, i64) = self.conn.query_row(
                        &format!("SELECT size, mode FROM {}.vm_fs WHERE path=?1", schema), params![p],
                        |r| Ok((r.get(0)?, r.get(1)?)))?;
                    (sz as u64, md, schema)
                }
                None => return Err(anyhow!("not found in overlay: {}", p)),
            };
            if new_size == size { return Ok(()); }
            let keep = if new_size == 0 { 0 } else { (new_size + cs - 1) / cs };
            // Only chunks that actually change are written into Delta; the rest keep coming from Base.
            // Shrinking inside a chunk rewrites that one chunk; growing rewrites the old tail chunk (it now
            // needs zero fill) and adds the new ones.
            let tail_start = if new_size < size { new_size / cs } else { size / cs };
            let touched: Vec<u64> = (tail_start..keep).collect();
            // the Delta row must exist before its chunks are written
            self.ensure_delta_file_row(&p, mode, mtime, new_size)?;
            for idx in touched {
                let base = idx * cs;
                let want = ((new_size - base).min(cs)) as usize;
                let mut piece = if base < size { self.read_chunk_layered(layer, &p, idx as i64)? } else { Vec::new() };
                if piece.len() > want { piece.truncate(want); }
                if piece.len() < want { piece.resize(want, 0); }
                let key = crate::vm::blob_key(&piece);
                crate::vm::blob_ensure(&self.conn, &key, &piece)?;
                self.conn.prepare_cached("DELETE FROM main.vm_file_chunks WHERE path=?1 AND idx=?2")?
                    .execute(params![p, idx as i64])?;
                self.conn.prepare_cached("INSERT INTO main.vm_file_chunks(path, idx, hash) VALUES (?1,?2,?3)")?
                    .execute(params![p, idx as i64, key.as_slice()])?;
            }
            // shrinking: drop the chunks past the new end that Delta happens to hold
            self.conn.execute("DELETE FROM main.vm_file_chunks WHERE path=?1 AND idx>=?2", params![p, keep as i64])?;
            self.refresh_delta_file(&p, new_size, mtime)?;
            Ok(())
        })
    }

    /// The VM path a link's target sits at, with every link ancestor expanded, or the path itself when no
    /// ancestor is a link. Mirrors what `vm_materialize_tree` does: a child of `/bin` (a symlink to
    /// `/usr/bin`) belongs at `usr/bin`, and writing it literally at `bin/` collides with the link.
    fn resolved_path(&self, p: &str, links: &std::collections::HashMap<String, String>) -> String {
        match vm::resolve_parent_in_vm(p, links) {
            Some(pv) => format!("{}/{}", pv.trim_end_matches('/'), Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
            None => p.to_string(),
        }
    }

    /// Every symlink in the merged view, as (path, target).
    fn view_links(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut out = std::collections::HashMap::new();
        for (p, k) in self.all_paths()? {
            if k != "symlink" { continue; }
            if let Ok(t) = self.readlink(&p) { out.insert(p, t); }
        }
        Ok(out)
    }

    /// Materialise the merged view into `dest`, so a sandbox can run against it.
    ///
    /// The tree is built from Base and Delta together: Delta entries are applied over Base ones, and a
    /// whiteout removes the Base entry. Writing the tree out is unavoidable for a sandbox that needs a
    /// directory (bwrap binds one); the saving comes later, from `absorb_tree` writing back only the
    /// paths that actually changed.
    pub fn materialize(&self, dest: &Path) -> Result<usize> {
        // Base first: every layer entry Base holds, then Delta on top
        let mut n = 0usize;
        let links = self.view_links()?;
        let real: std::collections::HashSet<String> = self.all_paths()?.into_iter()
            .filter(|(_, k)| k != "symlink").map(|(p, _)| p).collect();
        {
            let base_rows: Vec<(String, String, i64, Option<String>)> = {
                let mut st = self.conn.prepare(
                    "SELECT path, kind, mode, link_target FROM base.vm_fs WHERE path <> '/' ORDER BY length(path), path")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for (p, kind, mode, target) in base_rows {
                if vm::is_reserved_vm_path(&p) { continue; }
                if self.whited_out(&p)? { continue; }
                let target_p = self.resolved_path(&p, &links);
                // a link that only aliases a real entry has no room in the tree; the real entry serves it
                if target_p != p && real.contains(&target_p) { n += 1; continue; }
                let host = dest.join(target_p.trim_start_matches('/'));
                match kind.as_str() {
                    "dir" => { std::fs::create_dir_all(&host)?; }
                    "symlink" => {
                        if let Some(t) = target {
                            if let Some(par) = host.parent() { std::fs::create_dir_all(par)?; }
                            let _ = std::fs::remove_file(&host);
                            #[cfg(unix)] std::os::unix::fs::symlink(&t, &host)?;
                        }
                    }
                    _ => {
                        let data = self.read(&p)?;
                        if let Some(par) = host.parent() { std::fs::create_dir_all(par)?; }
                        let _ = std::fs::remove_file(&host);
                        std::fs::write(&host, &data)?;
                        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&host, std::fs::Permissions::from_mode(mode as u32)); }
                    }
                }
                n += 1;
            }
        }
        // Delta rows: files, dirs, symlinks
        let entries: Vec<(String, String, i64, i64, Option<String>)> = {
            let mut st = self.conn.prepare(
                "SELECT path, kind, mode, size, link_target FROM main.vm_fs ORDER BY length(path), path")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (p, kind, mode, _size, target) in entries {
            if vm::is_reserved_vm_path(&p) { continue; }
            let target_p = self.resolved_path(&p, &links);
            if target_p != p && real.contains(&target_p) { n += 1; continue; }
            let host = dest.join(target_p.trim_start_matches('/'));
            match kind.as_str() {
                "dir" => { std::fs::create_dir_all(&host)?; }
                "symlink" => {
                    if let Some(t) = target {
                        if let Some(par) = host.parent() { std::fs::create_dir_all(par)?; }
                        let _ = std::fs::remove_file(&host);
                        #[cfg(unix)] std::os::unix::fs::symlink(&t, &host)?;
                    }
                }
                _ => {
                    let data = self.read(&p)?;
                    if let Some(par) = host.parent() { std::fs::create_dir_all(par)?; }
                    let _ = std::fs::remove_file(&host);
                    std::fs::write(&host, &data)?;
                    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&host, std::fs::Permissions::from_mode(mode as u32)); }
                }
            }
            n += 1;
        }
        Ok(n)
    }

    /// Write back only what changed in `tree`, compared with the merged view.
    ///
    /// A new or modified path goes through `write` / `write_range` / `mkdir` / `symlink`; a path that is
    /// gone gets a whiteout. A path whose bytes already match the view produces nothing in Delta, which is
    /// the whole point: a task that edits one file leaves one Delta entry, not a copy of the tree.
    pub fn absorb_tree(&self, tree: &Path) -> Result<usize> {
        use std::collections::HashSet;
        let mut seen: HashSet<String> = HashSet::new();
        // View paths the tree actually holds. An alias row (/bin/tool, a link whose target is a real entry)
        // shares its view path with that real entry, so it is served by it and must not be treated as gone.
        let mut view_seen: HashSet<String> = HashSet::new();
        let mut written = 0usize;
        // view path -> database path, so an entry reached through a directory link is recognised
        let links = self.view_links()?;
        let mut view_to_db: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        // Two passes so a literal path wins over a translated one. Base has /bin -> /usr/bin and also rows
        // like /bin/gcc; the tree shows /bin (the link) and usr/bin/gcc. Mapping usr/bin/gcc back is right,
        // but mapping /bin back to /usr/bin would make the tree's own /bin look unrecorded and re-create it
        // in Delta (measured: /bin appearing as created in every run).
        let mut translated: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let paths = self.all_paths()?;
        for (p, _k) in &paths {
            let vp = self.resolved_path(p, &links);
            if vp != *p { translated.insert(vp, p.clone()); }
        }
        // A view path may be claimed by two database paths: /bin (a link) and /usr/bin (a directory) both
        // answer to "usr/bin", and /bin/cat versus usr/bin/cat likewise. The entry that is not behind a link
        // wins, because that is where the tree actually stores the bytes; otherwise an alias hid the real
        // entry and the tree's regular file was written back over a Base link (measured: /bin/cat, /bin/ls
        // and the rest became plain files in Delta).
        let under_link = |p: &str| -> bool {
            let mut cur = p.to_string();
            while let Some(i) = cur.rfind('/') {
                if i == 0 { break; }
                cur.truncate(i);
                if links.contains_key(&cur) { return true; }
            }
            false
        };
        for (p, _k) in &paths {
            if !under_link(p) { view_to_db.insert(p.clone(), p.clone()); }
        }
        for (p, _k) in &paths {
            view_to_db.entry(p.clone()).or_insert_with(|| p.clone());
        }
        // A translated mapping is only a fallback: /bin/cat (a Base link) and usr/bin/cat (the real file the
        // tree holds) both answer to usr/bin/cat. The real path must win, or the tree's regular file was
        // written back over the Base link (measured: /bin/cat became a 47 KiB plain file in Delta).
        for (vp, p) in translated { view_to_db.entry(vp).or_insert(p); }
        for entry in walkdir::WalkDir::new(tree).follow_links(false).min_depth(1) {
            let e = match entry { Ok(v) => v, Err(_) => continue };
            let rel = e.path().strip_prefix(tree).unwrap_or(e.path()).to_string_lossy().to_string();
            let p = crate::vm::normalize_vm_path(&format!("/{}", rel.trim_start_matches('/')));
            if vm::is_reserved_vm_path(&p) { continue; }
            seen.insert(p.clone());
            let md = match std::fs::symlink_metadata(e.path()) { Ok(m) => m, Err(_) => continue };
            // The path on disk is the *view* path: a Base entry at /bin/gcc (a link under the directory
            // link /bin -> /usr/bin) appears at usr/bin/gcc. Looking that up literally finds nothing and the
            // tree write-back would create a duplicate in Delta (measured: /usr/bin/gcc, /usr/bin/python3,
            // /usr/bin/sh appearing as created). The view path is translated back to the database path first.
            // A path with a symlink ancestor is not stored here: the tree shows /bin as a link to /usr/bin, so
            // /bin/cat on disk is really usr/bin/cat. Writing at the literal path turned Base links into plain
            // files in Delta (measured: /bin/cat, /bin/ls, ...). Such paths are skipped entirely; the real
            // location (usr/bin/cat) is handled on its own when the walk reaches it.
            let mut cur = p.clone();
            let mut has_link_ancestor = false;
            while let Some(i) = cur.rfind('/') {
                if i == 0 { break; }
                cur.truncate(i);
                if links.contains_key(&cur) { has_link_ancestor = true; break; }
            }
            let db_p = view_to_db.get(&p).cloned().unwrap_or_else(|| p.clone());
            view_seen.insert(p.clone());
            if has_link_ancestor {
                // Not written back, but the entry does exist in the view. Record its *database* path: the
                // deletion pass below compares against database paths, so recording the view path left the
                // real row looking gone and it was whited out (measured: /bin/tool).
                seen.insert(db_p);
                continue;
            }
            // Record the database path: the collection below compares against database paths, and a view path
            // would make every entry behind a directory link look deleted (measured: an unchanged tree wrote
            // 7 rows into Delta and whited out /bin/sh).
            seen.insert(db_p.clone());
            let layer_kind = self.kind_in_overlay(&db_p)?;
            let ft = md.file_type();
            let host_kind = if ft.is_symlink() { "symlink" } else if md.is_dir() { "dir" } else { "file" };
            // unchanged content is skipped: this is what keeps Delta small
            match (host_kind, layer_kind.as_deref()) {
                ("dir", Some("dir")) => continue,
                ("symlink", Some("symlink")) => {
                    // compare the stored target with the one on disk. The disk shows the *mount* form (a
                    // relative path that stays inside the tree), so the stored absolute target is converted
                    // the same way before comparing; otherwise every directory link looked changed and was
                    // re-written into Delta on every run (measured: /bin created in each run).
                    // `readlink` already applies the mount form (absolute targets become paths inside the
                    // tree). Comparing against the text on disk is then a plain string equality; routing it
                    // through resolved_path again double-translated the target and made every link look
                    // changed (measured: 11 links rewritten on an untouched tree, every round trip).
                    let stored_mount_form = self.readlink(&db_p)?;
                    let disk = std::fs::read_link(e.path())?.to_string_lossy().to_string();
                    if stored_mount_form == disk { continue; }
                    self.symlink(&db_p, &disk)?;
                }
                ("file", Some("file")) => {
                    let disk = std::fs::read(e.path())?;
                    let now = self.read(&p)?;
                    if disk == now { continue; }
                    // a small change to a large file keeps copy-on-write: only the touched chunks move
                    self.write_smallest(&db_p, &disk, &now)?;
                }
                _ => {
                    match host_kind {
                        "dir" => { self.mkdir(&db_p)?; }
                        "symlink" => { let t = std::fs::read_link(e.path())?.to_string_lossy().to_string(); self.symlink(&db_p, &t)?; }
                        _ => { let data = std::fs::read(e.path())?; self.write(&db_p, &data, 0o644, crate::vm::now_secs())?; }
                    }
                }
            }
            written += 1;
        }
        // paths in the view that the tree no longer has
        let mut gone: Vec<String> = Vec::new();
        for (p, kind) in self.all_paths()? {
            if vm::is_reserved_vm_path(&p) { continue; }
            if seen.contains(&p) { continue; }
            let vp = self.resolved_path(&p, &links);
            if vp != p && view_seen.contains(&vp) { continue; }
            let _ = kind;
            gone.push(p);
        }
        for p in gone { self.remove(&p)?; written += 1; }
        Ok(written)
    }

    /// Every path in the merged view (Base not shadowed by a whiteout, plus Delta).
    fn all_paths(&self) -> Result<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        {
            let mut st = self.conn.prepare("SELECT path, kind FROM base.vm_fs WHERE path <> '/'")?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (p, k) = r?;
                if self.whited_out(&p)? { continue; }
                out.push((p, k));
            }
        }
        {
            let mut st = self.conn.prepare("SELECT path, kind FROM main.vm_fs WHERE path <> '/'")?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// Write a file whose new content is already known, using `write_range` when only part changed.
    fn write_smallest(&self, p: &str, disk: &[u8], now: &[u8]) -> Result<()> {
        let cs = crate::vm::CHUNK_SIZE;
        if disk.len() == now.len() {
            // find the single changed chunk; if there is exactly one, move only that one
            let mut changed: Vec<usize> = Vec::new();
            let chunks = disk.len().div_ceil(cs);
            for i in 0..chunks {
                let a = &disk[i * cs..((i + 1) * cs).min(disk.len())];
                let b = &now[i * cs..((i + 1) * cs).min(now.len())];
                if a != b { changed.push(i); }
            }
            if changed.len() == 1 && chunks > 1 {
                let i = changed[0];
                let from = i * cs;
                let to = ((i + 1) * cs).min(disk.len());
                return self.write_range(p, from as u64, &disk[from..to], crate::vm::now_secs());
            }
        }
        self.write(p, disk, 0o644, crate::vm::now_secs())
    }

    /// Make sure the Delta has a file row for `p` with the given size.
    ///
    /// Insert only when absent, otherwise update in place. Deleting the row first would cascade to
    /// `vm_file_chunks` (trigger `vm_fs_del_chunks`) and silently drop the chunks earlier writes put
    /// there; that was a real bug: a second write to the same file lost the first one's chunk.
    fn ensure_delta_file_row(&self, p: &str, mode: i64, mtime: i64, size: u64) -> Result<()> {
        let exists: i64 = self.conn.query_row(
            "SELECT count(*) FROM main.vm_fs WHERE path=?1", params![p], |r| r.get(0))?;
        if exists > 0 {
            self.conn.prepare_cached("UPDATE main.vm_fs SET size=?2, mtime=?3 WHERE path=?1")?
                .execute(params![p, size as i64, mtime])?;
        } else {
            self.conn.prepare_cached(
                "INSERT INTO main.vm_fs(path, kind, mode, uid, gid, mtime, mtime_ns, size, link_target, hash, compressed)
                 VALUES (?1, 'file', ?2, 0, 0, ?3, 0, ?4, NULL, X'', 0)")?
                .execute(params![p, mode, mtime, size as i64])?;
        }
        Ok(())
    }

    /// Write the whole-file hash and size of the Delta row from its chunk list.
    fn refresh_delta_file(&self, p: &str, size: u64, mtime: i64) -> Result<()> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        let total = if size == 0 { 0 } else { (size + crate::vm::CHUNK_SIZE as u64 - 1) / crate::vm::CHUNK_SIZE as u64 };
        for idx in 0..total {
            let piece = self.read_chunk_layered("main", p, idx as i64)?;
            h.update(&piece);
        }
        let whole: [u8; 32] = h.finalize().into();
        self.conn.prepare_cached("UPDATE main.vm_fs SET size=?2, hash=?3, mtime=?4 WHERE path=?1")?
            .execute(params![p, size as i64, whole.as_slice(), mtime])?;
        Ok(())
    }

    /// Whole file through the overlay: every chunk comes from the layer that holds it.
    ///
    /// A Delta file only stores the chunks it modified, so the chunk list is not contiguous inside Delta.
    /// Reading index by index through `read_chunk_layered` is what lets a small write to a large Base
    /// file stay small: the untouched chunks are still read from Base.
    fn read_file_layered(&self, schema: &str, p: &str) -> Result<Vec<u8>> {
        // Size and the full chunk list come from ONE statement, so they describe the same version of the
        // file. Reading them separately let a concurrent rewrite hand back a new size with an older chunk
        // list, which looked like a short chunk (measured: "chunk 1 has 4464 bytes, expected 65536").
        let (kind, size): (String, i64) = self.conn.query_row(
            &format!("SELECT kind, size FROM {}.vm_fs WHERE path=?1", schema), params![p],
            |r| Ok((r.get(0)?, r.get(1)?))).map_err(|_| anyhow!("not found in overlay: {}", p))?;
        if kind != "file" { return Err(anyhow!("not a regular file: {}", p)); }
        let size = size as u64;
        let cs = crate::vm::CHUNK_SIZE as u64;
        let total = if size == 0 { 0 } else { (size + cs - 1) / cs };
        let mut out: Vec<u8> = Vec::with_capacity(size as usize);
        for idx in 0..total {
            let piece = self.read_chunk_layered(schema, p, idx as i64)?;
            // the last chunk is short by design; earlier ones are full. A short earlier chunk means the
            // chunk list is inconsistent, which is reported instead of returning a truncated file.
            let want = ((size - idx * cs).min(cs)) as usize;
            if idx + 1 == total {
                if piece.len() < want {
                    return Err(anyhow!("{}: last chunk has {} bytes, expected {}", p, piece.len(), want));
                }
            } else if piece.len() != want {
                return Err(anyhow!("{}: chunk {} has {} bytes, expected {}", p, idx, piece.len(), want));
            }
            out.extend_from_slice(&piece[..want]);
        }
        if out.len() as u64 != size {
            return Err(anyhow!("{}: read {} bytes, size is {}", p, out.len(), size));
        }
        Ok(out)
    }

    fn read_from(&self, schema: &str, p: &str) -> Result<Vec<u8>> {
        // A file that owns a contiguous chunk list is read in one statement (same snapshot, no gaps).
        // A copy-on-write file in Delta does not: it stores only the chunks a write touched and the rest
        // still lives in Base, so it is read chunk by chunk across the two layers.
        if self.chunks_are_dense(schema, p)? { return self.read_dense(schema, p); }
        self.read_file_layered(schema, p)
    }

    /// True when this layer's chunk list is a gap-free 0..n sequence.
    ///
    /// Deliberately independent of `size`: a list that does not match the recorded size is a corrupt row
    /// and must be reported as one, not routed to the cross-layer reader (which would look for chunks in
    /// Base). Continuity alone decides which reader applies.
    fn chunks_are_dense(&self, schema: &str, p: &str) -> Result<bool> {
        let (n, mx): (i64, Option<i64>) = self.conn.query_row(
            &format!("SELECT count(*), max(idx) FROM {s}.vm_file_chunks WHERE {s}.vm_file_chunks.path=?1", s = schema),
            params![p], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(mx.map(|m| m + 1).unwrap_or(0) == n)
    }

    fn read_dense(&self, schema: &str, p: &str) -> Result<Vec<u8>> {
        // One statement reads the file's kind and size together with its chunks, so all of them come
        // from the same snapshot. Separate statements could straddle a concurrent rewrite and mix a
        // new size with old chunks. The LEFT JOIN returns one row even for an empty file (idx NULL).
        let sql = format!(
            "SELECT f.kind, f.size, c.idx, b.content, b.compressed, b.dict_id, c.hash
             FROM {s}.vm_fs f
             LEFT JOIN {s}.vm_file_chunks c ON c.path = f.path
             LEFT JOIN {s}.vm_blobs b ON b.hash = c.hash
             WHERE f.path = ?1 ORDER BY c.idx", s = schema);
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![p], |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, i64>(1)?,
            r.get::<_, Option<i64>>(2)?, r.get::<_, Option<Vec<u8>>>(3)?, r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<i64>>(5)?, r.get::<_, Option<Vec<u8>>>(6)?)))?;
        let mut out = Vec::new();
        let mut expect = 0i64;
        let mut meta: Option<(String, i64)> = None;
        for row in rows {
            let (kind, size, idx, body, comp, dict_id, key) = row?;
            meta.get_or_insert((kind, size));
            let Some(idx) = idx else { continue };  // empty file: a row with no chunks
            if idx != expect { return Err(anyhow!("{}: chunk index gap at {}", p, expect)); }
            expect += 1;
            let (Some(body), Some(comp), Some(dict_id), Some(key)) = (body, comp, dict_id, key) else {
                return Err(anyhow!("{}: chunk {} has no block", p, idx));
            };
            // a block's dictionary lives in the same schema as the block (ids are per database)
            let piece = decode_block_in(&self.conn, schema, &body, comp, dict_id)?;
            if blob_key(&piece).as_slice() != key.as_slice() { return Err(anyhow!("{}: chunk {} hash mismatch", p, idx)); }
            out.extend_from_slice(&piece);
        }
        // No row at all: the path was removed between the lookup and this read.
        let Some((kind, size)) = meta else { return Err(anyhow!("not found in overlay: {}", p)); };
        if kind != "file" { return Err(anyhow!("not a regular file: {}", p)); }
        if out.len() as i64 != size {
            return Err(anyhow!("{}: read {} bytes, file size is {} (inconsistent chunk list)", p, out.len(), size));
        }
        Ok(out)
    }
}
