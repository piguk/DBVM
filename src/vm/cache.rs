use super::*;

/// Cache root: $SELF_VM_CACHE, else $XDG_CACHE_HOME/self-vm, else ~/.cache/self-vm.
pub fn cache_dir() -> PathBuf {
    let dir = std::env::var_os("SELF_VM_CACHE").map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(|x| PathBuf::from(x).join("self-vm")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/self-vm")))
        .unwrap_or_else(|| std::env::temp_dir().join("self-vm-cache"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Create unique temp dir `$TMPDIR/<prefix>XXXXXX` via mkdtemp(3).
pub fn make_tmp_dir(prefix: &str) -> Result<PathBuf> {
    let tmpl = std::env::temp_dir().join(format!("{}XXXXXX", prefix));
    let mut buf = tmpl.to_string_lossy().as_bytes().to_vec();
    buf.push(0);
    let p = unsafe { libc::mkdtemp(buf.as_mut_ptr() as *mut libc::c_char) };
    if p.is_null() { return Err(anyhow!("mkdtemp {}: {}", tmpl.display(), std::io::Error::last_os_error())); }
    buf.pop();
    Ok(PathBuf::from(String::from_utf8_lossy(&buf).to_string()))
}

/// A private working directory under $TMPDIR, removed when dropped.
///
/// A process that is killed cannot remove its directories, and materialized trees are large: killed runs
/// filled /tmp (measured). So each directory has an owner lock beside it (`<dir>.lock`, never inside: the
/// directory may be a VM's root, and its content is synced into the database). The lock is held for the
/// directory's lifetime; a lock nobody holds means the owner is gone, and `create` removes that directory
/// before making a new one with the same prefix.
pub struct WorkDir { path: PathBuf, lock: PathBuf, _owner: std::fs::File }

impl WorkDir {
    pub fn create(prefix: &str) -> Result<WorkDir> {
        reclaim_abandoned(prefix);
        let path = make_tmp_dir(prefix)?;
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        // locked under a temporary name and then renamed, so no unlocked `.lock` is ever visible
        let staging = PathBuf::from(format!("{}.lock-{}", path.display(), std::process::id()));
        let owner = std::fs::OpenOptions::new().write(true).create_new(true).open(&staging)
            .map_err(|e| anyhow!("create {}: {}", staging.display(), e))?;
        if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&owner), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(anyhow!("lock {}: {}", staging.display(), std::io::Error::last_os_error()));
        }
        std::fs::rename(&staging, &lock)?;
        Ok(WorkDir { path, lock, _owner: owner })
    }

    pub fn path(&self) -> &Path { &self.path }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_file(&self.lock);
    }
}

/// Remove `<prefix>*` directories under $TMPDIR whose owner lock is free.
fn reclaim_abandoned(prefix: &str) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(dir_name) = name.strip_suffix(".lock") else { continue };
        if !dir_name.starts_with(prefix) { continue; }
        let Ok(f) = std::fs::OpenOptions::new().write(true).open(e.path()) else { continue };
        if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&f), libc::LOCK_EX | libc::LOCK_NB) } != 0 { continue; }
        // the owner is gone: remove its directory, then the lock, while holding the lock
        let _ = std::fs::remove_dir_all(e.path().with_file_name(dir_name));
        let _ = std::fs::remove_file(e.path());
    }
}

pub fn cache_path_for_hash(hash: &str) -> PathBuf {
    let dir = cache_dir();
    let a = &hash[0..2.min(hash.len())];
    dir.join(a).join(hash)
}

/// Place a cached file into a materialised tree without letting the tree write back into the cache.
///
/// A hard link makes the tree entry and the cache entry the same inode, so an in-place write in the tree
/// (an editor, `sed`-style rewrite, an agent's script) changes the cached bytes. The next materialise then
/// hands those edited bytes to a database that still holds the originals — measured: after editing a tree,
/// materialising the same unmodified database returned the edited text, and the repository's own
/// `reused_scratch_dir_reflects_db_not_stale_host_edits` failed from then on.
///
/// A reflink (copy-on-write clone) keeps the sharing cheap and isolates writes, so it is tried first. When
/// the filesystem has no reflink support the file is copied: slower, but the cache can never be corrupted by
/// a tree.
pub(super) fn try_hardlink_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(par)=dst.parent(){ let _ = std::fs::create_dir_all(par); }
    if !src.exists() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cache miss"));
    }
    let _ = std::fs::remove_file(dst);
    if reflink(src, dst).is_ok() { return Ok(()); }
    std::fs::copy(src, dst).map(|_|())
}

/// Store `data` as the content-cache entry `path`, unless one exists. The entry is written under a
/// temporary name and renamed into place, so no reader sees a partly written entry. Failure is
/// ignored: the cache only saves work, and a missing entry is rebuilt from the database.
pub(super) fn cache_put(path: &Path, data: &[u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if path.exists() { return; }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return };
    let _ = std::fs::create_dir_all(dir);
    let tmp = dir.join(format!(".{}.{}.{}", name.to_string_lossy(), std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    if std::fs::write(&tmp, data).is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Copy the content-cache entry `path` to `dst` if it holds `len` bytes. Returns whether it did.
///
/// An entry is trusted by its name, so its length is checked before use: a 0-byte entry under the
/// hash of /usr/bin/git produced an empty /usr/bin/git in the VM (measured; how the entry was left
/// empty was not established). An entry of the wrong length is removed, so the next writer replaces it.
pub(super) fn cache_copy_out(path: &Path, len: u64, dst: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.len() == len => try_hardlink_or_copy(path, dst).is_ok(),
        Ok(_) => { let _ = std::fs::remove_file(path); false }
        Err(_) => false,
    }
}

/// Clone `src` to `dst` with FICLONE (copy-on-write on btrfs/xfs, a full copy elsewhere but still isolated).
/// Writes to either file stay private, unlike a hard link.
fn reflink(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let s = std::fs::File::open(src)?;
    let d = std::fs::File::create(dst)?;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let rc = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE, s.as_raw_fd()) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

// LRU for decompressed blobs: hash -> raw bytes
pub fn decompress_cache() -> &'static Mutex<LruCache<String, Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<LruCache<String, Arc<Vec<u8>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LruCache::new(NonZeroUsize::new(64).unwrap())))
}

/// Default content-cache limit: 2 GiB. Chosen as a size that keeps materialize fast for typical
/// trees without letting the cache grow unbounded; override with SELF_VM_CACHE_MAX.
pub const DEFAULT_CACHE_MAX: u64 = 2 * 1024 * 1024 * 1024;

/// Parse a size like "512M", "2G", "1048576". None if malformed.
pub fn parse_size_bytes(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    let (num, mul) = match t.chars().last()? {
        'k' => (&t[..t.len()-1], 1024u64),
        'm' => (&t[..t.len()-1], 1024 * 1024),
        'g' => (&t[..t.len()-1], 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (t.as_str(), 1),
        _ => return None,
    };
    num.parse::<u64>().ok()?.checked_mul(mul)
}

/// Effective content-cache limit: SELF_VM_CACHE_MAX if set and valid, else the default.
/// A set-but-invalid value is an error on stderr, not a silent fallback.
pub fn cache_limit_from_env() -> u64 {
    match std::env::var("SELF_VM_CACHE_MAX") {
        Ok(v) => match parse_size_bytes(&v) {
            Some(n) => n,
            None => { eprintln!("warning: SELF_VM_CACHE_MAX={:?} is not a size; using default", v); DEFAULT_CACHE_MAX }
        },
        Err(_) => DEFAULT_CACHE_MAX,
    }
}

// Cache / status helpers
pub fn vm_cache_info() -> (PathBuf, usize, u64) {
    let dir = cache_dir();
    let mut count=0usize; let mut bytes=0u64;
    if dir.exists() {
        for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() { count+=1; bytes+=entry.metadata().map(|m| m.len()).unwrap_or(0); }
        }
    }
    (dir, count, bytes)
}

/// Evict least-recently-used content-cache files under `root` until its total size is at most
/// `max_bytes`. Returns (files removed, bytes freed). Every cache file is rebuildable from the
/// database, so eviction only costs time on the next materialize, never data.
///
/// Recency is file mtime, which the cache refreshes on each write. This is a full scan, so it
/// is meant to run once per import, not per file.
pub fn enforce_cache_limit(root: &Path, max_bytes: u64) -> Result<(usize, u64)> {
    if !root.exists() { return Ok((0, 0)); }
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for e in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if e.file_type().is_file() {
            let md = match e.metadata() { Ok(m) => m, Err(_) => continue };
            files.push((e.path().to_path_buf(), md.len(), md.modified().unwrap_or(std::time::UNIX_EPOCH)));
        }
    }
    let mut total: u64 = files.iter().map(|(_, b, _)| *b).sum();
    if total <= max_bytes { return Ok((0, 0)); }
    files.sort_by_key(|(_, _, t)| *t);
    let (mut removed, mut freed) = (0usize, 0u64);
    for (p, b, _) in files {
        if total <= max_bytes { break; }
        if std::fs::remove_file(&p).is_ok() { total -= b; freed += b; removed += 1; }
    }
    Ok((removed, freed))
}

pub fn vm_cache_prune(max_bytes: u64) -> Result<(usize, u64)> {
    let dir = cache_dir();
    if !dir.exists() { return Ok((0,0)); }
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for e in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        if e.file_type().is_file() {
            let md = match e.metadata() { Ok(m)=>m, Err(_)=>continue };
            let mt = md.modified().unwrap_or(std::time::UNIX_EPOCH);
            files.push((e.path().to_path_buf(), md.len(), mt));
        }
    }
    files.sort_by_key(|(_,_,t)| *t);
    let mut total: u64 = files.iter().map(|(_,b,_)| *b).sum();
    let mut removed=0usize; let mut freed=0u64;
    for (p,b,_) in files {
        if total <= max_bytes { break; }
        if std::fs::remove_file(&p).is_ok() { total-=b; freed+=b; removed+=1; }
    }
    // rmdir empty
    let _ = std::fs::remove_dir_all(dir.join(".tmp"));
    Ok((removed, freed))
}
