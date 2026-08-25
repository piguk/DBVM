use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const VM_APP_ID: u32 = 0x564D5351; // 'VMSQ'
pub const VM_USER_VERSION: u32 = 1;

/// `(path, kind, mode, size, hash)`
pub type VmDirEntry = (String, String, i64, i64, String);
/// `(path, kind, mode, size, mtime, hash)`
pub type VmStat = (String, String, i64, i64, i64, String);
/// `(resolved_path, kind, content, link_target, mode)`
pub type VmResolved = (String, String, Option<Vec<u8>>, Option<String>, i64);
/// `(id, name, created_at, page_count, bytes, note)`
pub type VmSnapshot = (i64, String, i64, i64, i64, String);
/// `vm_fs` row as stored: `(path, kind, mode, size, mtime, link_target, content, compressed)`
type VmStatRow = (
    String,
    String,
    i64,
    i64,
    i64,
    Option<String>,
    Option<Vec<u8>>,
    Option<i64>,
);
/// `vm_fs` row for resolution: `(kind, content, link_target, mode, compressed)`
type VmResolveRow = (String, Option<Vec<u8>>, Option<String>, i64, Option<i64>);
/// `vm_fs` entry with content already decompressed: `(kind, content, link_target, mode)`
type VmFsEntry = (String, Option<Vec<u8>>, Option<String>, i64);

fn cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DBVM_CACHE") {
        return PathBuf::from(p);
    }
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("dbvm/blobs");
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".cache/dbvm/blobs");
    }
    PathBuf::from("/tmp/dbvm-blobs")
}
fn cache_path_for_hash(hash: &str) -> PathBuf {
    let dir = cache_dir();
    let a = &hash[0..2.min(hash.len())];
    dir.join(a).join(hash)
}
fn try_hardlink_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(par) = dst.parent() {
        let _ = std::fs::create_dir_all(par);
    }
    if src.exists() {
        let _ = std::fs::remove_file(dst);
        if std::fs::hard_link(src, dst).is_ok() {
            return Ok(());
        }
        std::fs::copy(src, dst).map(|_| ())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cache miss",
        ))
    }
}
pub fn vm_apply_pragmas(conn: &Connection) -> Result<()> {
    let _ = conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-64000;
         PRAGMA mmap_size=268435456;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;",
    );
    Ok(())
}
pub fn vm_open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = vm_apply_pragmas(&conn);
    // migrate compressed/hash indexes if needed
    let cols: Vec<String> = {
        let mut st = conn.prepare("PRAGMA table_info(vm_fs)")?;
        let mut out = Vec::new();
        for r in st.query_map([], |r| r.get::<_, String>(1))? {
            out.push(r?);
        }
        out
    };
    if !cols.contains(&"compressed".to_string()) {
        let _ = conn.execute(
            "ALTER TABLE vm_fs ADD COLUMN compressed INTEGER DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_vm_fs_hash ON vm_fs(hash)",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_vm_fs_path ON vm_fs(path)",
            [],
        );
    } else {
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_vm_fs_hash ON vm_fs(hash)",
            [],
        );
    }
    Ok(conn)
}
pub fn vm_schema_sql() -> String {
    format!(
        r#"
PRAGMA application_id = {app};
PRAGMA user_version = {ver};
CREATE TABLE IF NOT EXISTS vm_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS vm_fs(
  id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('file','dir','symlink')),
  mode INTEGER NOT NULL DEFAULT 420,
  uid INTEGER NOT NULL DEFAULT 0,
  gid INTEGER NOT NULL DEFAULT 0,
  mtime INTEGER NOT NULL DEFAULT 0,
  size INTEGER NOT NULL DEFAULT 0,
  link_target TEXT,
  content BLOB,
  hash TEXT,
  compressed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_vm_fs_path ON vm_fs(path);
CREATE INDEX IF NOT EXISTS idx_vm_fs_hash ON vm_fs(hash);
CREATE TABLE IF NOT EXISTS vm_mem(
  id INTEGER PRIMARY KEY,
  addr INTEGER NOT NULL,
  size INTEGER NOT NULL,
  prot INTEGER NOT NULL,
  content BLOB
);
CREATE TABLE IF NOT EXISTS vm_snapshots(
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  page_count INTEGER NOT NULL,
  page_size INTEGER NOT NULL,
  bytes INTEGER NOT NULL,
  note TEXT
);
CREATE TABLE IF NOT EXISTS vm_log(
  id INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  op TEXT NOT NULL,
  path TEXT,
  detail TEXT
);
"#,
        app = VM_APP_ID,
        ver = VM_USER_VERSION
    )
}
pub fn init_vm_db(path: &str, overwrite: bool) -> Result<Connection> {
    if Path::new(path).exists() {
        if !overwrite {
            return Err(anyhow!("exists: {} (use --force)", path));
        }
        std::fs::remove_file(path)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(&vm_schema_sql())?;
    let _ = vm_apply_pragmas(&conn);
    conn.execute_batch(r#"
    CREATE TABLE IF NOT EXISTS self_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS self_blob(content BLOB NOT NULL);
    CREATE TABLE IF NOT EXISTS segments (id INTEGER PRIMARY KEY, type TEXT NOT NULL, offset INTEGER NOT NULL, vaddr INTEGER NOT NULL, filesz INTEGER NOT NULL, memsz INTEGER NOT NULL, r INTEGER, w INTEGER, x INTEGER, align INTEGER NOT NULL DEFAULT 4096, content BLOB);
    CREATE TABLE IF NOT EXISTS symbols (id INTEGER PRIMARY KEY, name TEXT NOT NULL, version TEXT, value INTEGER, size INTEGER, type TEXT, bind TEXT, defined INTEGER NOT NULL, exported INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name, version);
    CREATE TABLE IF NOT EXISTS bundle_objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT NOT NULL, is_root INTEGER NOT NULL, size INTEGER NOT NULL, content BLOB NOT NULL);
    CREATE TABLE IF NOT EXISTS bundle_needs(object_id INTEGER NOT NULL REFERENCES bundle_objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES bundle_objects(path));
    "#)?;
    conn.execute(
        "INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)",
        params!["vm_version", "1"],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)",
        params!["created_at", format!("{}", now_secs())],
    )?;
    conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES ('/','dir',493,?1,0,0)", params![now_secs()])?;
    conn.execute(
        "INSERT INTO vm_log(op, detail) VALUES ('init', ?1)",
        params![path],
    )?;
    Ok(conn)
}
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn normalize_vm_path(p: &str) -> String {
    let mut s = p.trim().to_string();
    if !s.starts_with('/') {
        s = format!("/{}", s);
    }
    if s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    if s.is_empty() {
        s = "/".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            parts.pop();
            continue;
        }
        parts.push(seg.to_string());
    }
    if parts.is_empty() {
        return "/".to_string();
    }
    format!("/{}", parts.join("/"))
}
fn ensure_parent_dirs(conn: &Connection, vm_path: &str) -> Result<()> {
    let p = normalize_vm_path(vm_path);
    let dir = Path::new(&p)
        .parent()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or("/".to_string());
    if dir == "/" || dir.is_empty() {
        return Ok(());
    }
    let mut cur = String::new();
    for comp in dir.split('/').filter(|s| !s.is_empty()) {
        cur.push('/');
        cur.push_str(comp);
        conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES (?1,'dir',493,?2,0,0)", params![cur, now_secs()])?;
    }
    Ok(())
}
fn fx_hash(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    data.hash(&mut h);
    h.finish()
}
fn compress_bytes(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 2048 {
        return None;
    }
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let mut enc = GzEncoder::new(Vec::new(), Compression::new(3));
    if enc.write_all(data).is_err() {
        return None;
    }
    if let Ok(c) = enc.finish()
        && c.len() + 128 < data.len()
    {
        return Some(c);
    }
    None
}
fn decompress_bytes(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    let mut d = GzDecoder::new(data);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}
fn is_gzipped(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b
}
pub fn vm_add_bytes(
    conn: &Connection,
    vm_path: &str,
    data: &[u8],
    mode: i64,
    mtime: i64,
) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    let hash = format!("{:x}", fx_hash(data));
    let cache_p = cache_path_for_hash(&hash);
    if !cache_p.exists() {
        if let Some(par) = cache_p.parent() {
            let _ = std::fs::create_dir_all(par);
        }
        let _ = std::fs::write(&cache_p, data);
    }
    if let Some(comp) = compress_bytes(data) {
        conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,content,hash,compressed) VALUES (?1,'file',?2,?3,?4,?5,?6,1)", params![vm_p, mode, mtime, data.len() as i64, comp, hash])?;
    } else {
        conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,content,hash,compressed) VALUES (?1,'file',?2,?3,?4,?5,?6,0)", params![vm_p, mode, mtime, data.len() as i64, data, hash])?;
    }
    Ok(())
}
pub fn vm_add_symlink(conn: &Connection, vm_path: &str, target: &str) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,link_target,compressed) VALUES (?1,'symlink',493,?2,0,?3,0)", params![vm_p, now_secs(), target])?;
    Ok(())
}
pub fn vm_add_dir(conn: &Connection, vm_path: &str, mode: i64, mtime: i64) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES (?1,'dir',?2,?3,0,0)", params![vm_p, mode, mtime])?;
    Ok(())
}
pub fn vm_add_file(conn: &Connection, host_path: &str, vm_path: &str) -> Result<()> {
    let hp = Path::new(host_path);
    let md = std::fs::symlink_metadata(hp).map_err(|e| anyhow!("stat {}: {}", host_path, e))?;
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(now_secs());
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() as i64
    };
    #[cfg(not(unix))]
    let mode = 420;
    if md.is_dir() {
        conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES (?1,'dir',?2,?3,0,0)", params![vm_p, mode, mtime])?;
        for entry in std::fs::read_dir(hp)? {
            let e = entry?;
            let child_host = e.path().to_string_lossy().to_string();
            let child_name = e.file_name().to_string_lossy().to_string();
            let child_vm = format!("{}/{}", vm_p.trim_end_matches('/'), child_name);
            vm_add_file(conn, &child_host, &child_vm)?;
        }
        return Ok(());
    }
    if md.file_type().is_symlink() {
        let target = std::fs::read_link(hp)?.to_string_lossy().to_string();
        let perm = 0o777;
        conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,link_target,compressed) VALUES (?1,'symlink',?2,?3,0,?4,0)", params![vm_p, perm, mtime, target])?;
        return Ok(());
    }
    let data = std::fs::read(hp)?;
    vm_add_bytes(conn, &vm_p, &data, mode, mtime)?;
    conn.execute(
        "INSERT INTO vm_log(op, path, detail) VALUES ('add', ?1, ?2)",
        params![vm_p, host_path],
    )?;
    Ok(())
}
fn fx_hash_u64(data: &[u8]) -> String {
    format!("{:x}", fx_hash(data))
}
pub fn vm_import_closure(conn: &Connection, host_elf: &str, vm_prefix: &str) -> Result<usize> {
    let host_real = std::fs::canonicalize(host_elf).unwrap_or(PathBuf::from(host_elf));
    use crate::elf::ElfMeta;
    use rustc_hash::FxHashMap;
    let mut meta_cache: FxHashMap<PathBuf, ElfMeta> = FxHashMap::default();
    let mut search_cache: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    let mut resolve_cache: FxHashMap<u64, Option<PathBuf>> = FxHashMap::default();
    let ld_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let mut seen: FxHashMap<PathBuf, String> = FxHashMap::default();
    let mut order: Vec<PathBuf> = Vec::new();
    let mut q: Vec<PathBuf> = vec![host_real.clone()];
    seen.insert(host_real.clone(), "exe".to_string());
    order.push(host_real.clone());
    let mut qi = 0;
    while qi < q.len() {
        let cur = q[qi].clone();
        qi += 1;
        let sdirs = crate::closure::search_dirs_for_cached(
            &cur,
            &[],
            &mut meta_cache,
            &mut search_cache,
            &ld_dirs,
        );
        let sh = crate::closure::search_dirs_hash(&sdirs);
        let needed = crate::elf::meta_for_path_cached(&cur, &mut meta_cache)
            .needed
            .clone();
        for soname in needed {
            if let Some(rp) =
                crate::closure::resolve_soname_cached(&soname, &sdirs, &mut resolve_cache, sh)
            {
                let rp = std::fs::canonicalize(&rp).unwrap_or(rp);
                if !seen.contains_key(&rp) {
                    seen.insert(rp.clone(), "lib".to_string());
                    order.push(rp.clone());
                    q.push(rp);
                }
            }
        }
    }
    let prefix = normalize_vm_path(vm_prefix);
    for p in &order {
        let vm_path = if prefix == "/" {
            p.to_string_lossy().to_string()
        } else {
            format!(
                "{}/{}",
                prefix.trim_end_matches('/'),
                p.file_name().unwrap().to_string_lossy()
            )
        };
        let target = p.to_string_lossy().to_string();
        vm_add_file(conn, &target, &target)?;
        if vm_path != target {
            vm_add_file(conn, &target, &vm_path)?;
        }
    }
    Ok(order.len())
}
pub fn vm_ls(conn: &Connection, vm_path: &str) -> Result<Vec<VmDirEntry>> {
    let p = normalize_vm_path(vm_path);
    let entry: Option<VmDirEntry> = conn
        .query_row(
            "SELECT path,kind,mode,size,coalesce(hash,'') FROM vm_fs WHERE path=?1",
            params![p],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    // A directory lists its children; anything else lists as itself.
    match &entry {
        Some((_, kind, ..)) if kind != "dir" => return Ok(vec![entry.unwrap()]),
        None if p != "/" => return Err(anyhow!("not found: {}", p)),
        _ => {}
    }
    let like = if p == "/" {
        "/%".to_string()
    } else {
        format!("{}/%", p)
    };
    let mut st = conn.prepare("SELECT path,kind,mode,size,coalesce(hash,'') FROM vm_fs WHERE path LIKE ?1 AND path != ?2 ORDER BY path")?;
    let rows = st.query_map(params![like, p], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    let mut direct = Vec::new();
    for (path, kind, mode, size, hash) in out {
        let rel = if p == "/" {
            path.trim_start_matches('/').to_string()
        } else {
            path.strip_prefix(&format!("{}/", p))
                .unwrap_or(&path)
                .to_string()
        };
        if !rel.contains('/') {
            direct.push((path, kind, mode, size, hash));
        }
    }
    Ok(direct)
}
pub fn vm_cat(conn: &Connection, vm_path: &str) -> Result<Vec<u8>> {
    let (real, kind, content, _link, _mode) = vm_resolve(conn, vm_path)?;
    if kind == "dir" {
        return Err(anyhow!("is a directory: {}", vm_path));
    }
    match content {
        Some(d) => Ok(d),
        None => Err(anyhow!("no content for {} (resolved {})", vm_path, real)),
    }
}
pub fn vm_stat(conn: &Connection, vm_path: &str) -> Result<VmStat> {
    let p = normalize_vm_path(vm_path);
    let row: Option<VmStatRow> = conn.query_row("SELECT path,kind,mode,size,mtime,link_target,content,compressed FROM vm_fs WHERE path=?1", params![p], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))).optional()?;
    let (path, kind, mode, size, mtime, link, content, compressed) = match row {
        Some(x) => x,
        None => return Err(anyhow!("not found: {}", p)),
    };
    if kind == "symlink" {
        let target = link.clone().unwrap_or_default();
        if let Ok((_rp, _rk, Some(d), _rl, _rm)) = vm_resolve(conn, &p) {
            return Ok((
                path,
                kind,
                mode,
                d.len() as i64,
                mtime,
                format!("-> {} {:x}", target, fx_hash(&d)),
            ));
        }
        return Ok((path, kind, mode, size, mtime, format!("-> {}", target)));
    }
    let hash = if let Some(c) = content {
        let is_c = compressed.unwrap_or(0) != 0;
        let raw = if is_c {
            decompress_bytes(&c).unwrap_or(c.clone())
        } else {
            c.clone()
        };
        let raw2 = if !is_c && is_gzipped(&c) {
            decompress_bytes(&c).unwrap_or(raw)
        } else {
            raw
        };
        format!("{:x}", fx_hash(&raw2))
    } else {
        String::new()
    };
    Ok((path, kind, mode, size, mtime, hash))
}
pub fn vm_resolve(conn: &Connection, vm_path: &str) -> Result<VmResolved> {
    let mut cur = normalize_vm_path(vm_path);
    let mut visited = std::collections::HashSet::new();
    for _ in 0..40 {
        if !visited.insert(cur.clone()) {
            return Err(anyhow!("symlink loop at {}", cur));
        }
        let row: Option<VmResolveRow> = conn
            .query_row(
                "SELECT kind, content, link_target, mode, compressed FROM vm_fs WHERE path=?1",
                params![cur],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let (kind, content_opt, link, mode, compressed) = match row {
            Some(x) => x,
            None => return Err(anyhow!("not found: {}", cur)),
        };
        if kind != "symlink" {
            let content = match content_opt {
                Some(b) => {
                    let is_c = compressed.unwrap_or(0) != 0;
                    if is_c {
                        Some(decompress_bytes(&b).unwrap_or(b))
                    } else if is_gzipped(&b) {
                        match decompress_bytes(&b) {
                            Ok(v) => Some(v),
                            Err(_) => Some(b),
                        }
                    } else {
                        Some(b)
                    }
                }
                None => None,
            };
            return Ok((cur, kind, content, link, mode));
        }
        let target = link.clone().unwrap_or_default();
        let next = if target.starts_with('/') {
            normalize_vm_path(&target)
        } else {
            let dir = Path::new(&cur)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or("/".to_string());
            if dir == "/" {
                format!("/{}", target)
            } else {
                format!("{}/{}", dir, target)
            }
        };
        cur = normalize_vm_path(&next);
    }
    Err(anyhow!("too many symlink levels: {}", vm_path))
}
pub fn vm_materialize(conn: &Connection, vm_path: &str, host_tmp: &Path) -> Result<PathBuf> {
    let (real, kind, content, _link, mode) = vm_resolve(conn, vm_path)?;
    if kind == "dir" {
        std::fs::create_dir_all(host_tmp)?;
        return Ok(host_tmp.to_path_buf());
    }
    if let Some(data) = content {
        if let Some(parent) = host_tmp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let hash = fx_hash_u64(&data);
        let cache_p = cache_path_for_hash(&hash);
        if cache_p.exists() && try_hardlink_or_copy(&cache_p, host_tmp).is_ok() {
            #[cfg(unix)]
            {
                let _ = std::fs::set_permissions(
                    host_tmp,
                    std::fs::Permissions::from_mode(mode as u32),
                );
            }
            let _ = real;
            return Ok(host_tmp.to_path_buf());
        }
        if !cache_p.exists() {
            if let Some(par) = cache_p.parent() {
                let _ = std::fs::create_dir_all(par);
            }
            let _ = std::fs::write(&cache_p, &data);
        }
        std::fs::write(host_tmp, &data)?;
        #[cfg(unix)]
        {
            let _ =
                std::fs::set_permissions(host_tmp, std::fs::Permissions::from_mode(mode as u32));
        }
        let _ = real;
        return Ok(host_tmp.to_path_buf());
    }
    Err(anyhow!("no content for {} (resolved {})", vm_path, real))
}
pub fn vm_pack_host_dir(conn: &Connection, host_dir: &str, vm_prefix: &str) -> Result<usize> {
    let mut n = 0;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for entry in walkdir::WalkDir::new(host_dir)
        .follow_links(false)
        .min_depth(0)
    {
        let e = entry?;
        if e.depth() == 0 {
            continue;
        }
        let hp = e.path().to_string_lossy().to_string();
        let rel = e
            .path()
            .strip_prefix(host_dir)
            .unwrap_or(e.path())
            .to_string_lossy()
            .to_string();
        let vm_p = if vm_prefix == "/" {
            format!("/{}", rel.trim_start_matches('/'))
        } else {
            format!(
                "{}/{}",
                vm_prefix.trim_end_matches('/'),
                rel.trim_start_matches('/')
            )
        };
        vm_add_file(conn, &hp, &normalize_vm_path(&vm_p))?;
        n += 1;
        if n % 2000 == 0 {
            conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
        }
    }
    conn.execute_batch("COMMIT;")?;
    Ok(n)
}
pub fn vm_import_tar(conn: &Connection, tar_path: &str, strip_prefix: &str) -> Result<usize> {
    use std::io::Read;
    let file =
        std::fs::File::open(tar_path).map_err(|e| anyhow!("open tar {}: {}", tar_path, e))?;
    let is_gz = tar_path.ends_with(".gz") || tar_path.ends_with(".tgz");
    let reader: Box<dyn Read> = if is_gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    let mut n = 0usize;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let mut vm_path = path.clone();
        if vm_path.starts_with("./") {
            vm_path = vm_path[2..].to_string();
        }
        if !strip_prefix.is_empty() {
            if vm_path.starts_with(strip_prefix) {
                vm_path = vm_path[strip_prefix.len()..].to_string();
            } else if vm_path == strip_prefix.trim_end_matches('/') {
                vm_path = "".to_string();
            }
        }
        vm_path = normalize_vm_path(&vm_path);
        if vm_path.is_empty() || vm_path == "/" {
            continue;
        }
        let header = entry.header();
        let kind = header.entry_type();
        let mode = header.mode().unwrap_or(0o644) as i64;
        let mtime = header.mtime().unwrap_or(now_secs() as u64) as i64;
        if kind.is_dir() {
            vm_add_dir(conn, &vm_path, mode, mtime)?;
        } else if kind.is_symlink() || kind.is_hard_link() {
            // hard links are stored as symlinks: the VM only needs the target path
            if let Some(t) = entry.link_name()? {
                vm_add_symlink(conn, &vm_path, &t.to_string_lossy())?;
            }
        } else if kind.is_file() {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            vm_add_bytes(conn, &vm_path, &data, mode, mtime)?;
        } else {
            continue;
        }
        n += 1;
        if n.is_multiple_of(2000) {
            conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
        }
    }
    conn.execute_batch("COMMIT;")?;
    conn.execute(
        "INSERT INTO vm_log(op, detail) VALUES ('import_tar', ?1)",
        params![tar_path],
    )?;
    Ok(n)
}
pub fn vm_materialize_tree(conn: &Connection, dest: &Path) -> Result<usize> {
    std::fs::create_dir_all(dest)?;
    let mut st =
        conn.prepare("SELECT path, mode FROM vm_fs WHERE kind='dir' ORDER BY length(path), path")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (p, mode) = r?;
        let host = dest.join(p.trim_start_matches('/'));
        std::fs::create_dir_all(&host)?;
        #[cfg(unix)]
        {
            let _ = std::fs::set_permissions(&host, std::fs::Permissions::from_mode(mode as u32));
        }
    }
    let mut st = conn.prepare("SELECT path, link_target FROM vm_fs WHERE kind='symlink'")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (p, target) = r?;
        let host = dest.join(p.trim_start_matches('/'));
        if host.exists() || std::fs::symlink_metadata(&host).is_ok() {
            let _ = std::fs::remove_file(&host);
            let _ = std::fs::remove_dir(&host);
        }
        if let Some(par) = host.parent() {
            std::fs::create_dir_all(par)?;
        }
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&target, &host);
        }
    }
    let mut st =
        conn.prepare("SELECT path, content, mode, hash, compressed FROM vm_fs WHERE kind='file'")?;
    let mut n = 0usize;
    for r in st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<Vec<u8>>>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })? {
        let (p, content, mode, hash_opt, compressed) = r?;
        if let Some(blob) = content {
            let data: Vec<u8> = if compressed.unwrap_or(0) != 0 || is_gzipped(&blob) {
                decompress_bytes(&blob).unwrap_or_else(|_| blob.clone())
            } else {
                blob.clone()
            };
            let hash = hash_opt.unwrap_or_else(|| fx_hash_u64(&data));
            let host = dest.join(p.trim_start_matches('/'));
            let need_write = if host.exists() {
                if let Ok(md) = std::fs::metadata(&host) {
                    if md.len() as i64 == data.len() as i64 {
                        if let Ok(existing) = std::fs::read(&host) {
                            fx_hash_u64(&existing) != hash
                        } else {
                            true
                        }
                    } else {
                        true
                    }
                } else {
                    true
                }
            } else {
                true
            };
            if !need_write {
                n += 1;
                continue;
            }
            if let Some(par) = host.parent() {
                std::fs::create_dir_all(par)?;
            }
            let cache_p = cache_path_for_hash(&hash);
            if cache_p.exists() && try_hardlink_or_copy(&cache_p, &host).is_ok() {
                #[cfg(unix)]
                {
                    let _ = std::fs::set_permissions(
                        &host,
                        std::fs::Permissions::from_mode(mode as u32),
                    );
                }
                n += 1;
                continue;
            }
            std::fs::write(&host, &data)?;
            if !cache_p.exists() {
                if let Some(par) = cache_p.parent() {
                    let _ = std::fs::create_dir_all(par);
                }
                let _ = std::fs::write(&cache_p, &data);
            }
            #[cfg(unix)]
            {
                let _ =
                    std::fs::set_permissions(&host, std::fs::Permissions::from_mode(mode as u32));
            }
            n += 1;
        }
    }
    for d in ["dev", "proc", "sys", "tmp"] {
        let _ = std::fs::create_dir_all(dest.join(d));
    }
    Ok(n)
}
pub fn vm_sync_from_host(conn: &Connection, host_root: &Path) -> Result<(usize, usize, usize)> {
    use std::collections::{HashMap, HashSet};
    let mut host_files: HashMap<String, (String, Vec<u8>, i64)> = HashMap::new();
    let mut host_all_paths: HashSet<String> = HashSet::new();
    for entry in walkdir::WalkDir::new(host_root)
        .follow_links(false)
        .min_depth(1)
    {
        let e = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let rel = e
            .path()
            .strip_prefix(host_root)
            .unwrap_or(e.path())
            .to_string_lossy()
            .to_string();
        let vm_p = normalize_vm_path(&format!("/{}", rel.trim_start_matches('/')));
        if vm_p == "/" {
            continue;
        }
        if vm_p == "/dev"
            || vm_p.starts_with("/dev/")
            || vm_p == "/proc"
            || vm_p.starts_with("/proc/")
            || vm_p == "/sys"
            || vm_p.starts_with("/sys/")
        {
            continue;
        }
        let md = match std::fs::symlink_metadata(e.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        host_all_paths.insert(vm_p.clone());
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            md.permissions().mode() as i64
        };
        #[cfg(not(unix))]
        let mode = 420;
        if md.file_type().is_symlink() {
            if let Ok(t) = std::fs::read_link(e.path()) {
                host_files.insert(
                    vm_p,
                    (
                        "symlink".to_string(),
                        t.to_string_lossy().as_bytes().to_vec(),
                        mode,
                    ),
                );
            }
        } else if md.is_dir() {
            host_files.insert(vm_p, ("dir".to_string(), Vec::new(), mode));
        } else if md.is_file()
            && let Ok(data) = std::fs::read(e.path())
        {
            host_files.insert(vm_p, ("file".to_string(), data, mode));
        }
    }
    let mut db_map: HashMap<String, VmFsEntry> = HashMap::new();
    {
        let mut st =
            conn.prepare("SELECT path, kind, content, link_target, mode, compressed FROM vm_fs")?;
        for r in st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        })? {
            let (p, kind, blob, link, mode, compressed) = r?;
            let content = match blob {
                Some(b) => {
                    let is_c = compressed.unwrap_or(0) != 0;
                    if is_c {
                        decompress_bytes(&b).ok()
                    } else if is_gzipped(&b) {
                        decompress_bytes(&b).ok().or(Some(b))
                    } else {
                        Some(b)
                    }
                }
                None => None,
            };
            db_map.insert(p, (kind, content, link, mode));
        }
    }
    let mut created = 0usize;
    let mut updated = 0usize;
    let mut deleted = 0usize;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for (vm_p, (kind, data, mode)) in host_files.iter() {
        if vm_p == "/" {
            continue;
        }
        match db_map.get(vm_p) {
            None => {
                if kind == "symlink" {
                    let target = String::from_utf8_lossy(data).to_string();
                    let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,link_target,compressed) VALUES (?1,'symlink',?2,?3,0,?4,0)", params![vm_p, *mode, now_secs(), target]);
                    created += 1;
                } else if kind == "dir" {
                    let _ = conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES (?1,'dir',?2,?3,0,0)", params![vm_p, *mode, now_secs()]);
                    created += 1;
                } else {
                    let hash = fx_hash_u64(data);
                    if let Some(comp) = compress_bytes(data) {
                        let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,content,hash,compressed) VALUES (?1,'file',?2,?3,?4,?5,?6,1)", params![vm_p, *mode, now_secs(), data.len() as i64, comp, hash]);
                    } else {
                        let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,content,hash,compressed) VALUES (?1,'file',?2,?3,?4,?5,?6,0)", params![vm_p, *mode, now_secs(), data.len() as i64, data, hash]);
                    }
                    created += 1;
                }
            }
            Some((db_kind, db_content, db_link, db_mode)) => {
                if db_kind != kind {
                    if kind == "symlink" {
                        let target = String::from_utf8_lossy(data).to_string();
                        let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,link_target,compressed) VALUES (?1,'symlink',?2,?3,0,?4,0)", params![vm_p, *mode, now_secs(), target]);
                    } else if kind == "dir" {
                        let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES (?1,'dir',?2,?3,0,0)", params![vm_p, *mode, now_secs()]);
                    } else {
                        let hash = fx_hash_u64(data);
                        if let Some(comp) = compress_bytes(data) {
                            let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,content,hash,compressed) VALUES (?1,'file',?2,?3,?4,?5,?6,1)", params![vm_p, *mode, now_secs(), data.len() as i64, comp, hash]);
                        } else {
                            let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,content,hash,compressed) VALUES (?1,'file',?2,?3,?4,?5,?6,0)", params![vm_p, *mode, now_secs(), data.len() as i64, data, hash]);
                        }
                    }
                    updated += 1;
                } else if kind == "file" {
                    let db_data = db_content.as_deref().unwrap_or(&[]);
                    if db_data != data.as_slice() || db_mode != mode {
                        let hash = fx_hash_u64(data);
                        if let Some(comp) = compress_bytes(data) {
                            let _ = conn.execute("UPDATE vm_fs SET mode=?2, mtime=?3, size=?4, content=?5, hash=?6, compressed=1 WHERE path=?1", params![vm_p, *mode, now_secs(), data.len() as i64, comp, hash]);
                        } else {
                            let _ = conn.execute("UPDATE vm_fs SET mode=?2, mtime=?3, size=?4, content=?5, hash=?6, compressed=0 WHERE path=?1", params![vm_p, *mode, now_secs(), data.len() as i64, data, hash]);
                        }
                        updated += 1;
                    }
                } else if kind == "symlink" {
                    let target = String::from_utf8_lossy(data).to_string();
                    let db_target = db_link.as_deref().unwrap_or("");
                    if db_target != target || db_mode != mode {
                        let _ = conn.execute(
                            "UPDATE vm_fs SET mode=?2, link_target=?3, mtime=?4 WHERE path=?1",
                            params![vm_p, *mode, target, now_secs()],
                        );
                        updated += 1;
                    }
                } else if kind == "dir" && db_mode != mode {
                    let _ = conn.execute(
                        "UPDATE vm_fs SET mode=?2, mtime=?3 WHERE path=?1",
                        params![vm_p, *mode, now_secs()],
                    );
                    updated += 1;
                }
            }
        }
    }
    let mut to_delete: Vec<String> = Vec::new();
    for (db_path, (db_kind, _, _, _)) in db_map.iter() {
        if db_path == "/" {
            continue;
        }
        if db_path == "/dev"
            || db_path.starts_with("/dev/")
            || db_path == "/proc"
            || db_path.starts_with("/proc/")
            || db_path == "/sys"
            || db_path.starts_with("/sys/")
        {
            continue;
        }
        if !host_all_paths.contains(db_path) {
            to_delete.push(db_path.clone());
            if db_kind == "dir" {
                // children will be caught via their own path absence; but ensure prefix deletion
                let _ = conn.execute(
                    "DELETE FROM vm_fs WHERE path LIKE ?1",
                    params![format!("{}/%", db_path)],
                );
            }
        }
    }
    for p in to_delete {
        let _ = conn.execute("DELETE FROM vm_fs WHERE path=?1", params![p.clone()]);
        deleted += 1;
    }
    conn.execute_batch("COMMIT;")?;
    let _ = conn.execute(
        "INSERT INTO vm_log(op, detail) VALUES ('sync', ?1)",
        params![format!(
            "host:{} created:{} updated:{} deleted:{}",
            host_root.display(),
            created,
            updated,
            deleted
        )],
    );
    Ok((created, updated, deleted))
}
pub fn vm_gc(conn: &Connection) -> Result<(i64, i64)> {
    let before: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;");
    let after: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let saved = (before - after) * 4096;
    Ok((before, saved))
}
pub fn vm_checkpoint(conn: &Connection, name: &str, note: &str) -> Result<()> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let bytes = page_size * page_count;
    conn.execute("INSERT INTO vm_snapshots(name, page_count, page_size, bytes, note) VALUES (?1,?2,?3,?4,?5)", params![name, page_count, page_size, bytes, note])?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    conn.execute(
        "INSERT INTO vm_log(op, detail) VALUES ('snapshot', ?1)",
        params![name],
    )?;
    Ok(())
}
pub fn vm_list_snapshots(conn: &Connection) -> Result<Vec<VmSnapshot>> {
    let mut st = conn.prepare("SELECT id, name, created_at, page_count, bytes, coalesce(note,'') FROM vm_snapshots ORDER BY id")?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
pub fn vm_mem_insert(
    conn: &Connection,
    addr: i64,
    size: i64,
    prot: i64,
    content: &[u8],
) -> Result<()> {
    conn.execute(
        "INSERT INTO vm_mem(addr,size,prot,content) VALUES (?1,?2,?3,?4)",
        params![addr, size, prot, content],
    )?;
    Ok(())
}
pub fn vm_mem_list(conn: &Connection) -> Result<Vec<(i64, i64, i64, i64)>> {
    let mut st = conn.prepare("SELECT id, addr, size, prot FROM vm_mem ORDER BY addr")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
pub fn vm_mem_clear(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM vm_mem", [])?;
    Ok(())
}
pub fn vm_snapshot_file(conn: &Connection, db_path: &str, name: &str) -> Result<String> {
    let snap_path = format!("{}.snap.{}", db_path, name);
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let res = conn.execute(
        &format!("VACUUM INTO '{}'", snap_path.replace('\'', "''")),
        [],
    );
    if res.is_err() {
        std::fs::copy(db_path, &snap_path).map_err(|e| anyhow!("snapshot copy failed: {}", e))?;
    }
    Ok(snap_path)
}
pub fn vm_restore_file(db_path: &str, name: &str) -> Result<()> {
    let snap_path = format!("{}.snap.{}", db_path, name);
    if !Path::new(&snap_path).exists() {
        return Err(anyhow!("snapshot not found: {}", snap_path));
    }
    std::fs::copy(&snap_path, db_path).map_err(|e| anyhow!("restore failed: {}", e))?;
    Ok(())
}
pub fn vm_verify(conn: &Connection) -> Result<String> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let files: i64 = conn.query_row("SELECT count(*) FROM vm_fs", [], |r| r.get(0))?;
    let bytes: Option<i64> =
        conn.query_row("SELECT sum(size) FROM vm_fs WHERE kind='file'", [], |r| {
            r.get(0)
        })?;
    Ok(format!(
        "integrity={} page_size={} page_count={} freelist={} files={} bytes={}",
        integrity,
        page_size,
        page_count,
        freelist,
        files,
        bytes.unwrap_or(0)
    ))
}
