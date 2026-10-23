//! Shared bundle library cache: content-addressed entries, reference-held while in use.
//!
//! A launch links its private libraries to cache entries and must keep them alive until the
//! guest exits. It takes a shared `flock` on each entry it uses; eviction takes an exclusive
//! lock and skips anything that is held. Locks are per open file description, so they are
//! released automatically if the process dies.

use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Held shared lock on one cache entry. Dropping it releases the lock.
pub struct Hold { _file: File }

/// Take a shared lock on `entry` for the lifetime of the returned guard.
pub fn hold(entry: &Path) -> Result<Hold> {
    let f = OpenOptions::new().read(true).open(entry)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(Hold { _file: f })
}

/// Evict least-recently-used bundle entries under `root` until the total is at most `max_bytes`.
/// Entries currently held by a launch are skipped. Returns (removed, bytes freed).
pub fn enforce_bundle_limit(root: &Path, max_bytes: u64) -> Result<(usize, u64)> {
    if !root.exists() { return Ok((0, 0)); }
    let mut items: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for e in std::fs::read_dir(root)?.filter_map(|e| e.ok()) {
        let p = e.path();
        let md = match e.metadata() { Ok(m) => m, Err(_) => continue };
        if !md.is_file() || e.file_name().to_string_lossy().starts_with('.') { continue; }
        items.push((p, md.len(), md.modified().unwrap_or(std::time::UNIX_EPOCH)));
    }
    let mut total: u64 = items.iter().map(|(_, b, _)| *b).sum();
    if total <= max_bytes { return Ok((0, 0)); }
    items.sort_by_key(|(_, _, t)| *t);
    let (mut removed, mut freed) = (0usize, 0u64);
    for (p, b, _) in items {
        if total <= max_bytes { break; }
        let f = match OpenOptions::new().read(true).open(&p) { Ok(f) => f, Err(_) => continue };
        // exclusive, non-blocking: fails if any launch holds a shared lock on this entry
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 { continue; }
        // Remove while still holding the exclusive lock. Releasing first would open a window in
        // which a launch takes a shared lock on a file that is about to disappear.
        if std::fs::remove_file(&p).is_ok() { total -= b; freed += b; removed += 1; }
        drop(f);
    }
    Ok((removed, freed))
}
