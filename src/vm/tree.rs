use super::*;

/// Remove host entries under `dest` that the DB no longer contains (files, dirs, symlinks).
/// Without this, a reused scratch directory would keep deleted files, and the next sync-back
/// would resurrect them. Only paths below `dest` are touched; children are removed before
/// their parents, and a path the DB still has is kept.
fn prune_scratch(conn: &Connection, dest: &Path) -> Result<usize> {
    use std::collections::HashSet;
    let mut keep: HashSet<String> = HashSet::new();
    let mut st = conn.prepare("SELECT path FROM vm_fs")?;
    for r in st.query_map([], |r| r.get::<_,String>(0))? { keep.insert(r?); }
    let mut victims: Vec<PathBuf> = Vec::new();
    for e in walkdir::WalkDir::new(dest).follow_links(false).min_depth(1).contents_first(true) {
        let e = match e { Ok(v) => v, Err(_) => continue };
        let rel = e.path().strip_prefix(dest).unwrap_or(e.path()).to_string_lossy().to_string();
        let vm_p = normalize_vm_path(&format!("/{}", rel));
        if vm_p == "/dev" || vm_p.starts_with("/dev/") || vm_p == "/proc" || vm_p.starts_with("/proc/")
            || vm_p == "/sys" || vm_p.starts_with("/sys/") || vm_p == "/tmp" || vm_p.starts_with("/tmp/") { continue; }
        if !keep.contains(&vm_p) { victims.push(e.path().to_path_buf()); }
    }
    let mut removed = 0usize;
    for v in victims {
        let md = match std::fs::symlink_metadata(&v) { Ok(m) => m, Err(_) => continue };
        let ok = if md.is_dir() { std::fs::remove_dir(&v).or_else(|_| std::fs::remove_dir_all(&v)).is_ok() } else { std::fs::remove_file(&v).is_ok() };
        if ok { removed += 1; }
    }
    Ok(removed)
}

pub fn vm_materialize_tree(conn: &Connection, dest: &Path) -> Result<usize> {
    vm_materialize_tree_with_cache(conn, dest, &cache_dir())
}

/// Translated path under which the materialize writes a DB link (inverse of `link_alias`).
fn link_alias_target_of(link_path: &str, alias: &std::collections::HashMap<String, String>) -> Option<String> {
    alias.iter().find(|(_, v)| v.as_str() == link_path).map(|(k, _)| k.clone())
}

/// DB link path that a walked host entry stands for, if the entry is a materialize alias.
fn link_alias_row(vm_p: &str, alias: &std::collections::HashMap<String, String>) -> Option<String> {
    alias.get(vm_p).cloned()
}

/// Parent directory of `p` with every ancestor that is itself a VM symlink replaced by its
/// target (absolute targets only; relative ones are resolved against the link's own parent).
/// Returns None when no ancestor is a link, so the caller uses the path as written.
pub(crate) fn resolve_parent_in_vm(p: &str, links: &std::collections::HashMap<String, String>) -> Option<String> {
    let parent = Path::new(p).parent()?.to_string_lossy().to_string();
    let mut cur = String::from("/");
    let mut changed = false;
    for comp in parent.split('/').filter(|c| !c.is_empty()) {
        let next = if cur == "/" { format!("/{}", comp) } else { format!("{}/{}", cur, comp) };
        match links.get(&next) {
            Some(t) => {
                changed = true;
                cur = if t.starts_with('/') { normalize_vm_path(t) } else {
                    let dir = Path::new(&cur).parent().map(|x| x.to_string_lossy().to_string()).unwrap_or("/".into());
                    normalize_vm_path(&format!("{}/{}", dir, t))
                };
            }
            None => cur = next,
        }
    }
    if changed { Some(cur) } else { None }
}

/// Materialize with an explicit content-cache root (tests and callers that manage their own cache).
pub fn vm_materialize_tree_with_cache(conn: &Connection, dest: &Path, cache_root: &Path) -> Result<usize> {
    use rayon::prelude::*;
    // the content cache is keyed by vm_fs.hash: a stale hash would materialize the old content
    finalize_dirty(conn)?;
    std::fs::create_dir_all(dest)?;
    if dest.exists() { prune_scratch(conn, dest)?; }
    let mut st = conn.prepare("SELECT path, mode FROM vm_fs WHERE kind='dir' ORDER BY length(path), path")?;
    for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?)))? {
        let (p,mode)=r?;
        let host = dest.join(p.trim_start_matches('/'));
        std::fs::create_dir_all(&host)?;
        #[cfg(unix)] { let _ = std::fs::set_permissions(&host, std::fs::Permissions::from_mode(mode as u32)); }
    }
    // Symlinks are written in the VM's own path order, shallowest first, and each one is placed
    // under the real directory its parent resolves to inside the VM. Creating a child through a
    // parent link would write into the link's target instead: e.g. /bin/sh under /bin -> /usr/bin
    // must land at usr/bin/sh, not at bin/sh (which is a link, not a directory).
    let links: Vec<(String, String)> = {
        let mut st = conn.prepare("SELECT path, link_target FROM vm_fs WHERE kind='symlink' ORDER BY length(path), path")?;
        let rows = st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let link_map: std::collections::HashMap<String, String> = links.iter().cloned().collect();
    // Real entries (files, dirs) are materialized before links below, so a link never overwrites
    // a file that the VM holds at the same translated path. A link whose translated path is a
    // real entry is an alias with no room to exist, and is skipped (the real entry serves it).
    let real_paths: std::collections::HashSet<String> = {
        let mut st = conn.prepare("SELECT path FROM vm_fs WHERE kind <> 'symlink'")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (p, target) in &links {
        // the VM path of the directory this link sits in, with any link ancestor expanded to its target
        let parent_vm = resolve_parent_in_vm(p, &link_map);
        let translated = match &parent_vm {
            Some(pv) => format!("{}/{}", pv.trim_end_matches('/'), Path::new(p).file_name().unwrap().to_string_lossy()),
            None => p.clone(),
        };
        if translated != *p && real_paths.contains(&normalize_vm_path(&translated)) { continue; }
        let host = dest.join(translated.trim_start_matches('/'));
        if std::fs::symlink_metadata(&host).is_ok() { let _ = std::fs::remove_file(&host); let _ = std::fs::remove_dir(&host); }
        if let Some(par)=host.parent(){ std::fs::create_dir_all(par)?; }
        #[cfg(unix)] { let _ = std::os::unix::fs::symlink(target, &host); }
    }
    // Metadata only: blobs are loaded per file inside the worker, so memory stays O(one file).
    struct Task { path: String, mode: i64, hash: Option<String>, size: i64, mtime: i64 }  // hash: hex text
    let tasks: Vec<Task> = {
        let mut st = conn.prepare("SELECT path, mode, NULLIF(lower(hex(hash)),''), size, mtime FROM vm_fs WHERE kind='file'")?;
        let rows = st.query_map([], |r| Ok(Task{ path: r.get(0)?, mode: r.get(1)?, hash: r.get(2)?, size: r.get(3)?, mtime: r.get(4)? }))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let db_path = conn.path().map(|p| p.to_string()).ok_or_else(|| anyhow!("materialize needs a database file"))?;
    let dest_buf = dest.to_path_buf();
    let written = std::sync::atomic::AtomicUsize::new(0);
    // One read connection per worker, opened for this database by this call. A thread-local connection
    // opened once per thread was reused for whatever database the thread materialized next, so a second
    // VM in the same process was read from the first: its /usr/bin/git came out empty (measured).
    tasks.par_iter().for_each_init(|| Connection::open(&db_path), |worker, t| {
        let host = dest_buf.join(t.path.trim_start_matches('/'));
        let mut need = true;
        if let Ok(md) = std::fs::metadata(&host) {
            if md.len() as i64 == t.size {
                match md.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()) {
                    // 1s tolerance for fs timestamp granularity
                    Some(d) if (d.as_secs() as i64 - t.mtime).abs() <= 1 => need = false,
                    _ => {}
                }
            }
        }
        if !need { written.fetch_add(1, std::sync::atomic::Ordering::Relaxed); return; }
        // a read failure is reported and the file skipped, never written as empty or short
        let data = match worker.as_ref().map_err(|e| anyhow!("open {}: {}", db_path, e)).and_then(|c| file_bytes(c, &t.path)) {
            Ok(b) => b,
            Err(e) => { eprintln!("materialize {}: {}", t.path, e); return; }
        };
        if let Some(par)=host.parent(){ let _ = std::fs::create_dir_all(par); }
        let key = t.hash.clone().unwrap_or_else(|| fx_hash_u64(&data));
        let cache_p = cache_root.join(&key[0..2.min(key.len())]).join(&key);
        if !cache_copy_out(&cache_p, data.len() as u64, &host) {
            let _ = std::fs::write(&host, &data);
            cache_put(&cache_p, &data);
        }
        #[cfg(unix)]
        {
            let _ = std::fs::set_permissions(&host, std::fs::Permissions::from_mode(t.mode as u32));
            let ts = libc::timespec { tv_sec: t.mtime as libc::time_t, tv_nsec: 0 };
            let times = [ts, ts];
            if let Ok(cpath) = std::ffi::CString::new(host.to_string_lossy().as_bytes()) {
                unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0); }
            }
        }
        written.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    for d in ["dev","proc","sys","tmp"] { let _ = std::fs::create_dir_all(dest.join(d)); }
    Ok(written.load(std::sync::atomic::Ordering::Relaxed))
}

/// Summary of one sync pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport { pub created: usize, pub updated: usize, pub unchanged: usize, pub deleted: usize, pub failed: usize,
    /// Why each failed path failed. Kept so a caller can tell the user what was refused and cannot
    /// mistake a policy refusal for a successful step (see agent::run_step).
    pub failures: Vec<(String, String)> }

impl SyncReport {
    /// First failure as a message, if any. Used by callers that must not ignore a partial sync.
    pub fn failure_summary(&self) -> Option<String> {
        let (path, why) = self.failures.first()?;
        Some(format!("{} path(s) could not be written, first: {}: {}", self.failed, path, why))
    }
}

/// Paths the host-side sync never mirrors. The runtime mount points (/dev, /proc, /sys, /tmp)
/// are scaffolding created by materialize, not VM state. Only the directories themselves are
/// reserved: a file a guest writes under /tmp is ordinary content and is synced.
pub(crate) fn is_reserved_vm_path(p: &str) -> bool {
    if p == "/" || p == "/dev" || p.starts_with("/dev/") || p == "/proc" || p.starts_with("/proc/")
        || p == "/sys" || p.starts_with("/sys/") || p == "/tmp" { return true; }
    // Runtime artefacts, not VM state. Each was measured leaking into a step's change list:
    // - node's compile cache: ~35 files per agent run, pure noise;
    // - the proxy socket bound into the sandbox: a socket file, not content;
    // - the forwarder's own log and pi's session transcripts.
    if p.starts_with("/tmp/") { return true; }
    // python writes .pyc into __pycache__ beside its sources at import time; that is a runtime artefact like
    // node's compile cache, not a change the task made (measured: a python task reported 20 such paths).
    if p.contains("/__pycache__") { return true; }
    if p.starts_with("/opt/piagent/") { return true; }
    p == "/opt/piagent"
}

/// Mirror a host directory tree into the VM. Streaming: file contents are read one chunk at a
/// time, never the whole tree. Metadata for the DB is read as small fields only.
///
/// A regular file is rewritten only when its size, mtime, or type changed (1-second mtime
/// granularity: a same-size edit within the same second is not detected; use vm-verify and
/// a fresh sync after such edits). A failure on one entry is counted in `failed` and does not
/// stop the rest of the sync.
pub fn vm_sync_from_host(conn: &Connection, host_root: &Path) -> Result<(usize,usize,usize)> {
    let r = vm_sync_report(conn, host_root)?;
    Ok((r.created, r.updated, r.deleted))
}

pub fn vm_sync_report(conn: &Connection, host_root: &Path) -> Result<SyncReport> {
    sync_report_impl(conn, host_root, false)
}

/// Full verification: like `vm_sync_report`, but a regular file is compared by content (the
/// stored whole-file sha256 against the host bytes), not by metadata. This catches rows that
/// metadata cannot prove current, such as rows written before schema v7.
pub fn vm_sync_full(conn: &Connection, host_root: &Path) -> Result<SyncReport> {
    sync_report_impl(conn, host_root, true)
}

fn sync_report_impl(conn: &Connection, host_root: &Path, verify_content: bool) -> Result<SyncReport> {
    use std::collections::{HashMap, HashSet};
    #[cfg(unix)] use std::os::unix::fs::PermissionsExt;
    // a full sync compares host content with vm_fs.hash
    finalize_dirty(conn)?;

    // 1) DB side: metadata only (no content, no hash).
    struct Row { kind: String, mode: i64, mtime: i64, mtime_ns: i64, size: i64, link: Option<String> }
    let mut db: HashMap<String, Row> = HashMap::new();
    {
        let mut st = conn.prepare("SELECT path, kind, mode, mtime, size, link_target, mtime_ns FROM vm_fs")?;
        for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, Row { kind: r.get(1)?, mode: r.get(2)?, mtime: r.get(3)?, size: r.get(4)?, link: r.get(5)?, mtime_ns: r.get(6)? })))? {
            let (p, row) = r?;
            db.insert(p, row);
        }
    }

    // Translated path -> DB link path, for links the materialize wrote under a resolved parent.
    let link_alias: HashMap<String, String> = {
        let link_rows: HashMap<String, String> = db.iter()
            .filter(|(_, r)| r.kind == "symlink").filter_map(|(p, r)| r.link.clone().map(|t| (p.clone(), t))).collect();
        let mut m = HashMap::new();
        for p in link_rows.keys() {
            if let Some(pv) = resolve_parent_in_vm(p, &link_rows) {
                let translated = format!("{}/{}", pv.trim_end_matches('/'), Path::new(p).file_name().unwrap().to_string_lossy());
                m.insert(normalize_vm_path(&translated), p.clone());
            }
        }
        m
    };
    // 2) Host side: walk, keep only metadata. Entries are processed as we go.
    let mut seen: HashSet<String> = HashSet::new();
    // A link whose translated path holds a real entry is never written by the materialize (it is
    // an alias served by that entry), so it is not missing: keep its row.
    for (vm_link_path, _) in db.iter().filter(|(_, r)| r.kind == "symlink") {
        if let Some(tp) = link_alias_target_of(vm_link_path, &link_alias) {
            if matches!(db.get(&tp), Some(r) if r.kind != "symlink") { seen.insert(vm_link_path.clone()); }
        }
    }
    let mut rep = SyncReport::default();
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let now = now_secs();
    for entry in walkdir::WalkDir::new(host_root).follow_links(false).min_depth(1) {
        let e = match entry { Ok(v) => v, Err(err) => { rep.failed += 1; rep.failures.push((String::from("<walk>"), err.to_string())); continue; } };
        let rel = e.path().strip_prefix(host_root).unwrap_or(e.path()).to_string_lossy().to_string();
        let vm_p = normalize_vm_path(&format!("/{}", rel.trim_start_matches('/')));
        if is_reserved_vm_path(&vm_p) { continue; }
        // A link the materialize placed under its translated path (e.g. usr/bin/sh for the VM's
        // /bin/sh) is the same row, not a new one: keep the DB's own path and do not create it.
        // Only a walked *symlink* is an alias: a real file or dir at the translated path is its own row.
        let is_link = std::fs::symlink_metadata(e.path()).map(|m| m.file_type().is_symlink()).unwrap_or(false);
        if is_link {
            if let Some(alias) = link_alias_row(&vm_p, &link_alias) {
                seen.insert(alias);
                continue;
            }
        }
        let md = match std::fs::symlink_metadata(e.path()) {
            Ok(m) => m,
            Err(err) => { rep.failed += 1; rep.failures.push((vm_p.clone(), err.to_string())); continue; }
        };
        // permission bits only: the VM stores modes as 0o7777 (the sync used to store the full
        // st_mode, so every file looked changed on the next sync)
        #[cfg(unix)] let mode = (md.permissions().mode() & 0o7777) as i64;
        #[cfg(not(unix))] let mode = 420i64;
        let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(now);
        let ns_before = host_mtime_ns(e.path());
        seen.insert(vm_p.clone());
        let ft = md.file_type();
        let host_kind = if ft.is_symlink() { "symlink" } else if md.is_dir() { "dir" } else if md.is_file() { "file" } else { continue };
        let prev = db.get(&vm_p);
        let res: Result<Option<bool>> = (|| {
            // Some(true)=updated, Some(false)=created, None=unchanged
            match (prev, host_kind) {
                (None, "dir") => { fs_put_node(conn, &vm_p, "dir", mode, now, None)?; Ok(Some(false)) }
                (None, "symlink") => {
                    let t = std::fs::read_link(e.path())?;
                    fs_put_node(conn, &vm_p, "symlink", mode, now, Some(&t.to_string_lossy()))?;
                    Ok(Some(false))
                }
                (None, _) => {
                    let mut f = std::fs::File::open(e.path())?;
                    write_file_checked(conn, e.path(), &vm_p, mode, mtime, &mut f)?;
                    Ok(Some(false))
                }
                (Some(p), "dir") if p.kind == "dir" => {
                    if p.mode == mode { Ok(None) } else {
                        conn.prepare_cached("UPDATE vm_fs SET mode=?2, mtime=?3 WHERE path=?1")?.execute(params![vm_p, mode, now])?;
                        Ok(Some(true))
                    }
                }
                (Some(p), "symlink") if p.kind == "symlink" => {
                    let t = std::fs::read_link(e.path())?.to_string_lossy().to_string();
                    if p.link.as_deref() == Some(t.as_str()) && p.mode == mode { Ok(None) } else {
                        conn.prepare_cached("UPDATE vm_fs SET mode=?2, link_target=?3, mtime=?4 WHERE path=?1")?.execute(params![vm_p, mode, t, now])?;
                        Ok(Some(true))
                    }
                }
                (Some(p), "file") if p.kind == "file" => {
                    let metadata_current = p.size == md.len() as i64 && p.mtime == mtime && p.mode == mode && ns_matches(p.mtime_ns, ns_before);
                    let content_current = if verify_content && metadata_current {
                        // compare against the stored whole-file hash; a mismatch means stale content
                        let stored: Option<Vec<u8>> = conn.prepare_cached("SELECT hash FROM vm_fs WHERE path=?1")?.query_row(params![vm_p], |r| r.get(0)).optional()?.flatten();
                        let mut f = std::fs::File::open(e.path())?;
                        let mut hasher = sha2::Sha256::new();
                        std::io::copy(&mut f, &mut hasher)?;
                        use sha2::Digest;
                        let host_hash: [u8; 32] = hasher.finalize().into();
                        stored.as_deref() == Some(&host_hash[..])
                    } else { metadata_current };
                    if content_current && (!verify_content || metadata_current) {
                        Ok(None)
                    } else {
                        // a write during the read leaves ns unknown (0), so the next sync re-reads
                        let mut f = std::fs::File::open(e.path())?;
                        write_file_checked(conn, e.path(), &vm_p, mode, mtime, &mut f)?;
                        Ok(Some(true))
                    }
                }
                // type changed (or unknown): replace the entry
                (Some(_), _) => {
                    match host_kind {
                        "dir" => fs_put_node(conn, &vm_p, "dir", mode, now, None)?,
                        "symlink" => { let t = std::fs::read_link(e.path())?; fs_put_node(conn, &vm_p, "symlink", mode, now, Some(&t.to_string_lossy()))? }
                        _ => { let mut f = std::fs::File::open(e.path())?; write_file_checked(conn, e.path(), &vm_p, mode, mtime, &mut f)?; }
                    }
                    Ok(Some(true))
                }
            }
        })();
        match res {
            Ok(Some(false)) => rep.created += 1,
            Ok(Some(true)) => rep.updated += 1,
            Ok(None) => rep.unchanged += 1,
            Err(err) => { rep.failed += 1; rep.failures.push((vm_p.clone(), err.to_string())); }
        }
    }

    // 3) Deletions: DB paths absent from the host (prefix match is exact; DELETE fires triggers).
    let mut gone: Vec<&String> = db.keys().filter(|p| !is_reserved_vm_path(p) && !seen.contains(*p)).collect();
    gone.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for p in gone {
        let n = conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1")?.execute(params![p])?;
        if n > 0 { rep.deleted += 1; }
    }
    conn.execute_batch("COMMIT;")?;
    conn.prepare_cached("INSERT INTO vm_log(op, detail) VALUES ('sync', ?1)")?
        .execute(params![format!("host:{} created:{} updated:{} unchanged:{} deleted:{} failed:{}", host_root.display(), rep.created, rep.updated, rep.unchanged, rep.deleted, rep.failed)])?;
    let wal_size: i64 = conn.path().and_then(|pp| std::fs::metadata(format!("{}-wal", pp)).ok()).map(|m| m.len() as i64).unwrap_or(0);
    if wal_size > 4 * 1024 * 1024 {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    }
    Ok(rep)
}
