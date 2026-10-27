//! Tool injection: merge a package database (a VM DB holding a tool and its library closure) into a
//! target VM database, in one transaction.
//!
//! The package is attached read-only. Every copy names its columns explicitly and never copies the
//! `id` columns: `vm_blobs.id` and `vm_fs.id` are the target's own row ids, and copying a package's id
//! collides with an existing target row. `INSERT OR IGNORE` then drops the row silently, leaving
//! chunks that point at a blob that is not there (found by testing the plan's `SELECT *` form).
//!
//! Dictionaries are remapped the same way. Dictionary ids are per database, so the package's dictionary 1
//! and the target's dictionary 1 are different dictionaries. Copying by id would make the target decode
//! its own blocks with the wrong dictionary. Each package dictionary the injected blobs use is appended
//! to the target under a new id, and those blobs get the new id. A blob's key is the SHA-256 of its
//! uncompressed content, so the key does not change.
//!
//! A path already in the target is replaced: its old row is deleted first (its chunk references are
//! released by the normal cascade), then the package row and its chunks are inserted. The whole merge
//! is verified (`vm_verify --deep`) before commit; any failure rolls back and leaves the target as it
//! was.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use std::path::Path;

use crate::vm;

/// What an injection changed.
#[derive(Debug, Default, PartialEq)]
pub struct InjectReport {
    pub paths: usize,
    pub replaced: usize,
    pub blobs_added: usize,
    pub dicts_added: usize,
}

/// Injects every path of `pkg` into `target` (except the root). Atomic: a failed injection leaves
/// `target` unchanged. Returns counts for reporting.
pub fn inject(target: &Connection, pkg: &Path) -> Result<InjectReport> {
    let pkg_str = pkg.to_str().ok_or_else(|| anyhow!("non-utf8 package path"))?;
    // Check the package on a read-only connection: vm_open would migrate it in place, and a package
    // is an input, not something this command may change.
    {
        let ro = Connection::open_with_flags(pkg, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| anyhow!("open package {}: {}", pkg_str, e))?;
        let has_fs: i64 = ro.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='vm_fs'", [], |r| r.get(0))?;
        if has_fs == 0 { return Err(anyhow!("{} is not a VM database", pkg_str)); }
    }
    target.execute("ATTACH DATABASE ?1 AS pkg", params![pkg_str])
        .map_err(|e| anyhow!("attach {}: {}", pkg_str, e))?;
    // the package's hashes are copied as they are, and a package is never written, so stale ones
    // cannot be finalized here
    let res = match crate::vm::pending_hashes(target, "pkg") {
        Ok(0) => merge(target),
        Ok(n) => Err(anyhow!("{} has {} files with pending hashes; open it read-write once (e.g. vm-verify) to finalize them", pkg_str, n)),
        Err(e) => Err(e),
    };
    let detach = target.execute_batch("DETACH DATABASE pkg;");
    let report = res?;
    detach?;
    Ok(report)
}

fn merge(target: &Connection) -> Result<InjectReport> {
    target.execute_batch("BEGIN IMMEDIATE;")?;
    let inner: Result<InjectReport> = (|| {
        let mut rep = InjectReport::default();

        // 1) dictionaries the package's blobs use: append each to the target under a new id
        let needed: Vec<i64> = {
            let mut st = target.prepare("SELECT DISTINCT dict_id FROM pkg.vm_blobs WHERE dict_id <> 0 ORDER BY dict_id")?;
            let rows = st.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut dict_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        rep.dicts_added = needed.len();
        for pid in needed {
            let bytes: Vec<u8> = target.query_row(
                "SELECT dict FROM pkg.vm_dicts WHERE id=?1", params![pid], |r| r.get(0))
                .map_err(|_| anyhow!("package blob uses dictionary {} which the package does not contain", pid))?;
            let samples: i64 = target.query_row("SELECT samples FROM pkg.vm_dicts WHERE id=?1", params![pid], |r| r.get(0))?;
            let new_id = vm::vm_set_dict(target, &bytes, samples as usize)?;
            dict_map.insert(pid, new_id);
        }


        // 2) the package's paths, excluding the root. Replace any existing row at the same path.
        //    A path's blobs are copied immediately before its chunk rows are attached, never earlier.
        //    A copied blob has refcnt 0 until a chunk row references it, and the delete of any other
        //    path releases blobs whose refcnt reaches zero. Copying all blobs up front lets another
        //    path's delete drop a blob the package still needs (found by replaying both orders: 35 of 40
        //    blobs lost up front, none lost in per-path order).
        let paths: Vec<String> = {
            let mut st = target.prepare("SELECT path FROM pkg.vm_fs WHERE path <> '/' ORDER BY length(path), path")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        rep.paths = paths.len();
        for p in &paths {
            let existed: i64 = target.query_row("SELECT count(*) FROM main.vm_fs WHERE path=?1", params![p], |r| r.get(0))?;
            if existed > 0 {
                // deleting the row releases its chunk references through the normal triggers
                target.execute("DELETE FROM main.vm_fs WHERE path=?1", params![p])?;
                rep.replaced += 1;
            }
            target.execute(
                "INSERT INTO main.vm_fs(path, kind, mode, uid, gid, mtime, mtime_ns, size, link_target, hash, compressed)
                 SELECT path, kind, mode, uid, gid, mtime, mtime_ns, size, link_target, hash, compressed
                 FROM pkg.vm_fs WHERE path=?1", params![p])?;
            // this path's blobs, with dictionary ids remapped, before its chunk rows reference them
            let blobs_before: i64 = target.query_row("SELECT count(*) FROM main.vm_blobs", [], |r| r.get(0))?;
            // (hash, content, compressed, raw_size, dict_id)
            type BlobRow = (Vec<u8>, Vec<u8>, i64, i64, i64);
            let blobs: Vec<BlobRow> = {
                let mut st = target.prepare(
                    "SELECT DISTINCT b.hash, b.content, b.compressed, b.raw_size, b.dict_id
                     FROM pkg.vm_file_chunks c JOIN pkg.vm_blobs b ON b.hash = c.hash WHERE c.path = ?1")?;
                let rows = st.query_map(params![p], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for (hash, content, compressed, raw_size, dict_id) in blobs {
                let mapped = if dict_id == 0 { 0 } else {
                    *dict_map.get(&dict_id).ok_or_else(|| anyhow!("blob references unmapped dictionary {}", dict_id))?
                };
                target.execute(
                    "INSERT OR IGNORE INTO main.vm_blobs(hash, content, compressed, raw_size, refcnt, dict_id)
                     VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                    params![hash, content, compressed, raw_size, mapped])?;
            }
            rep.blobs_added += (target.query_row::<i64, _, _>("SELECT count(*) FROM main.vm_blobs", [], |r| r.get(0))? - blobs_before) as usize;
            target.execute(
                "INSERT INTO main.vm_file_chunks(path, idx, hash)
                 SELECT path, idx, hash FROM pkg.vm_file_chunks WHERE path=?1", params![p])?;
        }

        // 3) the merged database must verify deep before the change is kept
        let report = vm::vm_verify(target, true)?;
        if !report.contains("deep=ok") {
            return Err(anyhow!("inject aborted: target does not verify after merge: {}", report));
        }
        Ok(rep)
    })();
    match inner {
        Ok(rep) => { target.execute_batch("COMMIT;")?; Ok(rep) }
        Err(e) => { let _ = target.execute_batch("ROLLBACK;"); Err(e) }
    }
}
