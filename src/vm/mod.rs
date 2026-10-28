//! Single-file VM store: a SQLite database (`application_id = "VMSQ"`) holding a file tree,
//! its content blobs, snapshots, a memory image and a sparse block device.
//!
//! Submodules by concern; everything public is re-exported here, so callers use `vm::name`.
//! - `schema`: connection pragmas, schema creation, migrations, counters
//! - `codec`: zstd dictionaries, block compression and decoding
//! - `store`: blob, chunk and node writes; ranged writes and truncation
//! - `read`: listing, path resolution, whole-file and ranged reads
//! - `import`: ELF closure import, host directory packing, tar import
//! - `tree`: materializing the tree on the host and syncing host changes back
//! - `snapshot`: content snapshots, diff, restore, snapshot files
//! - `maint`: rechunk, recompress, gc, status, verify
//! - `cache`: host content cache and its size limit
//! - `mem`: memory image (`vm_mem`)
//! - `disk`: sparse block device (`vm_disk_blocks`)
use anyhow::{anyhow, Result};
#[cfg(unix)] use std::os::unix::fs::PermissionsExt;
use rusqlite::{Connection, params, OptionalExtension};
use std::path::{Path, PathBuf};
use std::io::{Read, Write};
use std::sync::{Arc, OnceLock, Mutex};
use lru::LruCache;
use std::num::NonZeroUsize;

mod cache;
mod schema;
mod codec;
mod store;
mod read;
mod import;
mod tree;
mod maint;
mod snapshot;
mod mem;
mod disk;
mod undo;
pub use cache::*;
pub use schema::*;
pub use codec::*;
pub use store::*;
pub use read::*;
pub use import::*;
pub use tree::*;
pub use maint::*;
pub use snapshot::*;
pub use mem::*;
pub use disk::*;
pub use undo::*;

pub const VM_APP_ID: u32 = 0x564D5351; // 'VMSQ'

/// Schema versions (PRAGMA user_version):
///   1 = legacy: FxHash text keys, no triggers, manual refcnt
///   2 = triggers maintain vm_blobs.refcnt
///   3 = triggers maintain vm_meta counters (blob_bytes/file_count/logical_bytes)
///   4 = blob keys are raw 32-byte SHA-256 BLOBs (UNIQUE); vm_blobs keeps a rowid so large
///       content is not stored inside a key B-tree (WITHOUT ROWID measured no smaller)
///   5 = file content is split into CHUNK_SIZE chunks listed in vm_file_chunks; a file's
///       refcount on vm_blobs comes from its chunk rows. vm_fs.hash = whole-file sha256
///       (informational, no refcounting). Legacy single-blob files keep working via vm_fs.hash.
///   6 = zstd dictionaries are append-only rows in vm_dicts; every flag-3 blob records its
///       dictionary in vm_blobs.dict_id. Retraining no longer makes older blocks unreadable.
///   7 = vm_fs.mtime_ns: host nanosecond mtime used only to detect changes during sync (0 = unknown)
///   8 = content snapshots: vm_snapshot_fs / vm_snapshot_chunks hold a copy of the file tree's
///       metadata and chunk references (never blob bytes). Keys carry no row ids, so a snapshot
///       is portable to another database under an ATTACH prefix (Base/Delta design).
///   9 = agent_steps: one row per agent command, with output, exit code, file changes and the
///       snapshots taken before and after it
///  10 = agent_steps.stdout_truncated / stderr_truncated
///  11 = vm_policies: write rules compiled into triggers (see `policy`)
///  12 = vm_dirty: files whose whole-file hash a deferred write left stale (see `finalize_dirty`)
///  13 = vm_undo*: per-step undo records (see `undo`); agent_steps.undo_id
pub const VM_USER_VERSION: u32 = 14;

pub use crate::overlay::Overlay;

pub fn now_secs() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 }

/// The paths strictly below directory `vm_dir`, as bounds for `path > ?lo AND path < ?hi`: each of them
/// starts with `vm_dir/`, and '0' is the byte after '/'. `LIKE 'dir/%'` would read `_` and `%` in names
/// as wildcards and ignore ASCII case (deleting `/a_b` took `/aXb` along, `/Docs` took `/docs`).
pub fn below_bounds(vm_dir: &str) -> (String, String) {
    let d = vm_dir.trim_end_matches('/');
    (format!("{}/", d), format!("{}0", d))
}

pub fn normalize_vm_path(p: &str) -> String {
    // names may begin or end with spaces: the path is taken as it is
    let mut s = p.to_string();
    if !s.starts_with('/') { s = format!("/{}", s); }
    if s.len() > 1 && s.ends_with('/') { s.pop(); }
    while s.contains("//") { s = s.replace("//", "/"); }
    if s.is_empty() { s = "/".to_string(); }
    let mut parts: Vec<String> = Vec::new();
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." { continue; }
        if seg == ".." { parts.pop(); continue; }
        parts.push(seg.to_string());
    }
    if parts.is_empty() { return "/".to_string(); }
    format!("/{}", parts.join("/"))
}

pub fn ensure_parent_dirs(conn: &Connection, vm_path: &str) -> Result<()> {
    let p = normalize_vm_path(vm_path);
    let dir = Path::new(&p).parent().map(|x| x.to_string_lossy().to_string()).unwrap_or("/".to_string());
    if dir == "/" || dir.is_empty() { return Ok(()); }
    let mut cur = String::new();
    for comp in dir.split('/').filter(|s| !s.is_empty()) {
        cur.push('/'); cur.push_str(comp);
        conn.prepare_cached("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES (?1,'dir',493,?2,0,0)")?.execute( params![cur, now_secs()])?;
    }
    Ok(())
}

pub fn fx_hash(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    data.hash(&mut h);
    h.finish()
}

/// Content identity: raw SHA-256 (32 bytes). Stored as BLOB in vm_blobs / vm_fs.
pub type BlobKey = [u8; 32];

pub fn blob_key(data: &[u8]) -> BlobKey {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

pub fn key_hex(k: &BlobKey) -> String { k.iter().map(|b| format!("{:02x}", b)).collect() }

/// Parse 64-char hex back into a key.
pub fn hex_to_key(h: &str) -> Option<BlobKey> {
    if h.len() != 64 { return None; }
    let mut k = [0u8; 32];
    for i in 0..32 { k[i] = u8::from_str_radix(&h[2*i..2*i+2], 16).ok()?; }
    Some(k)
}

/// Hex text of the content hash (CLI output and cache file names only).
pub fn fx_hash_u64(data: &[u8]) -> String { key_hex(&blob_key(data)) }

/// Short non-crypto key for a DB path (history/cache dir naming only).
pub fn path_key(p: &str) -> String { format!("{:x}", fx_hash(p.as_bytes())) }

pub fn fx_hash_u64_static(data: &[u8]) -> String { fx_hash_u64(data) }

/// Chunk size for file content. Each chunk is an independent blob (compressed, deduplicated).
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Run `f` in an immediate write transaction: committed if `f` succeeds, rolled back otherwise.
/// The write lock is taken up front, so a competing writer makes this wait (up to the busy
/// timeout) instead of failing mid-way with "database is locked", which a deferred transaction
/// does when it must upgrade its lock.
pub(crate) fn write_tx<T>(conn: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    match f() {
        Ok(v) => { conn.execute_batch("COMMIT;")?; Ok(v) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK;"); Err(e) }
    }
}
