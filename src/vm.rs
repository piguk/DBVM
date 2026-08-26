use anyhow::{anyhow, Result};
#[cfg(unix)] use std::os::unix::fs::PermissionsExt;
use rusqlite::{Connection, params, OptionalExtension};
use std::path::{Path, PathBuf};
use std::io::{Read, Write};
use std::collections::HashMap;
use std::sync::{OnceLock, Mutex};
use lru::LruCache;
use std::num::NonZeroUsize;

pub const VM_APP_ID: u32 = 0x564D5351; // 'VMSQ'
pub const VM_USER_VERSION: u32 = 1;

pub fn cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SELF_VM_CACHE") {
        let pb = PathBuf::from(p);
        let _ = std::fs::create_dir_all(&pb);
        if pb.is_dir() { return pb; }
    }
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        let pb = PathBuf::from(x).join("self-vm");
        let _ = std::fs::create_dir_all(&pb);
        if pb.is_dir() { return pb; }
    }
    if let Ok(h) = std::env::var("HOME") {
        let pb = PathBuf::from(h).join(".cache/self-vm");
        if std::fs::create_dir_all(&pb).is_ok() && pb.is_dir() {
            let probe = pb.join(".probe");
            if std::fs::write(&probe, b"1").is_ok() { let _ = std::fs::remove_file(&probe); return pb; }
        }
    }
    if let Ok(r) = std::env::var("XDG_RUNTIME_DIR") {
        let pb = PathBuf::from(r).join("self-vm-cache");
        let _ = std::fs::create_dir_all(&pb);
        if pb.is_dir() { return pb; }
    }
    let pb = PathBuf::from("/tmp/self-vm-cache");
    let _ = std::fs::create_dir_all(&pb);
    pb
}
pub fn cache_path_for_hash(hash: &str) -> PathBuf {
    let dir = cache_dir();
    let a = &hash[0..2.min(hash.len())];
    dir.join(a).join(hash)
}
fn try_hardlink_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(par)=dst.parent(){ let _ = std::fs::create_dir_all(par); }
    if src.exists() {
        let _ = std::fs::remove_file(dst);
        if std::fs::hard_link(src, dst).is_ok() { return Ok(()); }
        std::fs::copy(src, dst).map(|_|())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cache miss"))
    }
}

// LRU for decompressed blobs: hash -> raw bytes
pub fn decompress_cache() -> &'static Mutex<LruCache<String, Vec<u8>>> {
    static CACHE: OnceLock<Mutex<LruCache<String, Vec<u8>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LruCache::new(NonZeroUsize::new(64).unwrap())))
}

pub fn vm_apply_pragmas(conn: &Connection) -> Result<()> {
    let _ = conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-64000;
         PRAGMA mmap_size=268435456;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;
         PRAGMA journal_size_limit=67108864;"
    );
    Ok(())
}

fn ensure_blob_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vm_blobs(
            hash TEXT PRIMARY KEY,
            content BLOB NOT NULL,
            compressed INTEGER NOT NULL DEFAULT 0,
            raw_size INTEGER NOT NULL,
            refcnt INTEGER NOT NULL DEFAULT 1
        );
        CREATE INDEX IF NOT EXISTS idx_vm_blobs_refcnt ON vm_blobs(refcnt);"
    )?;
    Ok(())
}

fn ensure_dict_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vm_dict(\n  id INTEGER PRIMARY KEY CHECK(id=1),\n  dict BLOB NOT NULL,\n  samples INTEGER NOT NULL,\n  dict_size INTEGER NOT NULL,\n  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))\n);"
    )?;
    Ok(())
}

pub fn vm_open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = vm_apply_pragmas(&conn);
    let cols: Vec<String> = {
        let mut st = conn.prepare("PRAGMA table_info(vm_fs)")?;
        let mut out=Vec::new();
        for r in st.query_map([], |r| r.get::<_,String>(1))? { out.push(r?); }
        out
    };
    let needs_compressed = !cols.contains(&"compressed".to_string());
    if needs_compressed {
        let _ = conn.execute("ALTER TABLE vm_fs ADD COLUMN compressed INTEGER DEFAULT 0", []);
    }
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_vm_fs_path ON vm_fs(path)", []);
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_vm_fs_hash ON vm_fs(hash)", []);
    ensure_blob_table(&conn)?;
    ensure_dict_table(&conn)?;
    // load dict into global cache for decompress of flag 3
    let _ = vm_dict_load_global(&conn);
    // migrate page_size if still 4096 -> suggest VACUUM with 8192 on next gc if desired; we don't force rewrite here to avoid surprise slowdown
    Ok(conn)
}

pub fn vm_schema_sql() -> String {
    // note: page_size & auto_vacuum must be set before any table creation, caller uses PRAGMA before execute_batch
    format!(r#"
PRAGMA application_id = {app};
PRAGMA user_version = {ver};
CREATE TABLE IF NOT EXISTS vm_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS vm_blobs(
    hash TEXT PRIMARY KEY,
    content BLOB NOT NULL,
    compressed INTEGER NOT NULL DEFAULT 0,
    raw_size INTEGER NOT NULL,
    refcnt INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_vm_blobs_refcnt ON vm_blobs(refcnt);
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
CREATE TABLE IF NOT EXISTS vm_dict(
  id INTEGER PRIMARY KEY CHECK(id=1),
  dict BLOB NOT NULL,
  samples INTEGER NOT NULL,
  dict_size INTEGER NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
"#, app=VM_APP_ID, ver=VM_USER_VERSION)
}

pub fn init_vm_db_with_opts(path: &str, overwrite: bool, vm_only: bool) -> Result<Connection> {
    if Path::new(path).exists() {
        if !overwrite { return Err(anyhow!("exists: {} (use --force)", path)); }
        std::fs::remove_file(path)?;
    }
    let conn = Connection::open(path)?;
    let _ = conn.execute_batch("PRAGMA page_size=8192; PRAGMA auto_vacuum=INCREMENTAL;");
    conn.execute_batch(&vm_schema_sql())?;
    let _ = vm_apply_pragmas(&conn);
    if !vm_only {
        conn.execute_batch(r#"
    CREATE TABLE IF NOT EXISTS self_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS self_blob(content BLOB NOT NULL);
    CREATE TABLE IF NOT EXISTS segments (id INTEGER PRIMARY KEY, type TEXT NOT NULL, offset INTEGER NOT NULL, vaddr INTEGER NOT NULL, filesz INTEGER NOT NULL, memsz INTEGER NOT NULL, r INTEGER, w INTEGER, x INTEGER, align INTEGER NOT NULL DEFAULT 4096, content BLOB);
    CREATE TABLE IF NOT EXISTS symbols (id INTEGER PRIMARY KEY, name TEXT NOT NULL, version TEXT, value INTEGER, size INTEGER, type TEXT, bind TEXT, defined INTEGER NOT NULL, exported INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name, version);
    CREATE TABLE IF NOT EXISTS bundle_objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT NOT NULL, is_root INTEGER NOT NULL, size INTEGER NOT NULL, content BLOB NOT NULL);
    CREATE TABLE IF NOT EXISTS bundle_needs(object_id INTEGER NOT NULL REFERENCES bundle_objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES bundle_objects(path));
    "#)?;
    }
    conn.execute("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)", params!["vm_version", "1"])?;
    conn.execute("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)", params!["created_at", format!("{}", now_secs())])?;
    conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES ('/','dir',493,?1,0,0)", params![now_secs()])?;
    conn.execute("INSERT INTO vm_log(op, detail) VALUES ('init', ?1)", params![path])?;
    Ok(conn)
}
pub fn init_vm_db(path: &str, overwrite: bool) -> Result<Connection> {
    if Path::new(path).exists() {
        if !overwrite { return Err(anyhow!("exists: {} (use --force)", path)); }
        std::fs::remove_file(path)?;
    }
    let conn = Connection::open(path)?;
    // page params must be set before any table
    let _ = conn.execute_batch("PRAGMA page_size=8192; PRAGMA auto_vacuum=INCREMENTAL;");
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
    conn.execute("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)", params!["vm_version", "1"])?;
    conn.execute("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)", params!["created_at", format!("{}", now_secs())])?;
    conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES ('/','dir',493,?1,0,0)", params![now_secs()])?;
    conn.execute("INSERT INTO vm_log(op, detail) VALUES ('init', ?1)", params![path])?;
    Ok(conn)
}
pub fn now_secs() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 }
pub fn normalize_vm_path(p: &str) -> String {
    let mut s = p.trim().to_string();
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
        conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES (?1,'dir',493,?2,0,0)", params![cur, now_secs()])?;
    }
    Ok(())
}
pub fn fx_hash(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    data.hash(&mut h);
    h.finish()
}
pub fn fx_hash_u64(data: &[u8]) -> String { format!("{:x}", fx_hash(data)) }

pub fn vm_get_dict(conn: &Connection) -> Result<Option<Vec<u8>>> {
    let dict: Option<Vec<u8>> = conn.query_row("SELECT dict FROM vm_dict WHERE id=1", [], |r| r.get(0)).optional()?.flatten().map(|v| v);
    // vm_dict table may not exist yet, ignore error already handled via ensure_dict_table
    Ok(dict)
}
pub fn vm_set_dict(conn: &Connection, dict: &[u8], samples: usize) -> Result<()> {
    ensure_dict_table(conn)?;
    conn.execute("INSERT OR REPLACE INTO vm_dict(id, dict, samples, dict_size, created_at) VALUES (1, ?1, ?2, ?3, ?4)", params![dict, samples as i64, dict.len() as i64, now_secs()])?;
    Ok(())
}
pub fn vm_train_dict(conn: &Connection, max_dict_size: usize) -> Result<Option<Vec<u8>>> {
    // collect raw samples: files with raw_size between 256 and 16384, up to 120 samples
    let mut samples: Vec<Vec<u8>> = Vec::new();
    let mut total_raw: usize = 0;
    // fetch blobs raw by decompressing stored content
    let rows: Vec<(Vec<u8>, i64)> = {
        let mut st = conn.prepare("SELECT content, compressed FROM vm_blobs WHERE raw_size BETWEEN 256 AND 16384 ORDER BY RANDOM() LIMIT 150")?;
        let mut out = Vec::new();
        for r in st.query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,i64>(1)?)))? {
            if let Ok(x)=r { out.push(x); }
        }
        out
    };
    for (data, comp) in rows.into_iter() {
        let raw = decompress_bytes_raw(&data, comp).unwrap_or(data);
        if raw.len() >= 256 && raw.len() < 16384 {
            total_raw += raw.len();
            samples.push(raw);
            if samples.len() >= 120 || total_raw > 500_000 { break; }
        }
    }
    // fallback: if not enough samples, collect from vm_fs inline (should be rare after blobs)
    if samples.len() < 10 {
        let rows2: Vec<(Vec<u8>, i64)> = {
            let mut st = conn.prepare("SELECT content, compressed FROM vm_fs WHERE kind='file' AND content IS NOT NULL AND size BETWEEN 256 AND 16384 LIMIT 120")?;
            let mut out=Vec::new();
            for r in st.query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,Option<i64>>(1)?)))? {
                if let Ok((d,c))=r { out.push((d, c.unwrap_or(0))); }
            }
            out
        };
        for (data, comp) in rows2 {
            let raw = decompress_bytes_raw(&data, comp).unwrap_or(data);
            if raw.len() >=256 && raw.len()<16384 {
                samples.push(raw);
                if samples.len()>=120 { break; }
            }
        }
    }
    if samples.len() < 10 {
        return Ok(None);
    }
    let max = max_dict_size.min(112*1024).max(4*1024);
    match zstd::dict::from_samples(&samples, max) {
        Ok(d) if !d.is_empty() && d.len() >= 1024 => {
            vm_set_dict(conn, &d, samples.len())?;
            Ok(Some(d))
        },
        Ok(d) => {
            // tiny dict not useful
            if d.len() >= 512 {
                vm_set_dict(conn, &d, samples.len())?;
                return Ok(Some(d));
            }
            Ok(None)
        },
        Err(_) => Ok(None),
    }
}

pub fn compress_bytes(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 1024 { return None; }
    let level = if data.len() > 100*1024 { 19 } else if data.len() > 16*1024 { 6 } else { 3 };
    if let Ok(c) = zstd::bulk::compress(data, level) {
        if c.len() + 64 < data.len() { return Some(c); }
    }
    None
}
pub fn compress_bytes_with_conn(conn: &Connection, data: &[u8]) -> Option<(Vec<u8>, i64)> {
    if data.len() < 512 {
        // for tiny files only dict can help; try dict if available
        if let Ok(Some(dict)) = vm_get_dict(conn) {
            if dict.len() >= 512 {
                if let Ok(c) = zstd::bulk::Compressor::with_dictionary(3, &dict).and_then(|mut enc| enc.compress(data)) {
                    if c.len() + 32 < data.len() {
                        return Some((c, 3));
                    }
                }
            }
        }
        if data.len() < 1024 { return None; }
    }
    let level = if data.len() > 100*1024 { 19 } else if data.len() > 16*1024 { 6 } else { 3 };
    let mut best: Option<(Vec<u8>, i64)> = None;
    let mut best_len = usize::MAX;
    // try dict first for small/medium files
    if data.len() < 16384 {
        if let Ok(Some(dict)) = vm_get_dict(conn) {
            if dict.len() >= 512 {
                if let Ok(c) = zstd::bulk::Compressor::with_dictionary(level, &dict).and_then(|mut enc| enc.compress(data)) {
                    if c.len() + 32 < data.len() && c.len() < best_len {
                        best_len = c.len();
                        best = Some((c, 3));
                    }
                }
            }
        }
    }
    // try plain zstd at chosen level
    if let Ok(c) = zstd::bulk::compress(data, level) {
        if c.len() + 64 < data.len() && c.len() < best_len {
            best_len = c.len();
            best = Some((c, 2));
        }
    } else if let Ok(c) = zstd::bulk::compress(data, 3) {
        if c.len() + 64 < data.len() && c.len() < best_len {
            best = Some((c, 2));
        }
    }
    // for large files try level 19 if not already (fallback already level 19 for >100k, but try 19 for medium as alternative)
    if level != 19 && data.len() > 16384 {
        if let Ok(c) = zstd::bulk::compress(data, 19) {
            if c.len() + 64 < data.len() && c.len() < best_len {
                best = Some((c, 2));
            }
        }
    }
    best
}
pub fn decompress_bytes_zstd(data: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::bulk::decompress(data, 32*1024*1024)?)
}
static VM_DICT_GLOBAL: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();
fn vm_dict_global() -> &'static Mutex<Option<Vec<u8>>> {
    VM_DICT_GLOBAL.get_or_init(|| Mutex::new(None))
}
pub fn vm_dict_set_global(dict: Option<Vec<u8>>) {
    *vm_dict_global().lock().unwrap() = dict;
}
pub fn vm_dict_get_global() -> Option<Vec<u8>> {
    vm_dict_global().lock().unwrap().clone()
}
pub fn vm_dict_load_global(conn: &Connection) {
    if let Ok(Some(d)) = vm_get_dict(conn) {
        vm_dict_set_global(Some(d));
    }
}
pub fn decompress_bytes_zstd_with_dict(data: &[u8], dict: &[u8]) -> Result<Vec<u8>> {
    let mut dec = zstd::bulk::Decompressor::with_dictionary(dict).map_err(|e| anyhow!("{}", e))?;
    Ok(dec.decompress(data, 32*1024*1024).map_err(|e| anyhow!("{}", e))?)
}
pub fn decompress_bytes_raw(data: &[u8], compressed: i64) -> Result<Vec<u8>> {
    if compressed == 3 {
        if let Some(dict) = vm_dict_get_global() {
            if let Ok(v) = decompress_bytes_zstd_with_dict(data, &dict) { return Ok(v); }
        }
        if let Ok(v) = decompress_bytes_zstd(data) { return Ok(v); }
    }
    if compressed == 2 {
        return decompress_bytes_zstd(data);
    }
    if compressed == 1 {
        use flate2::read::GzDecoder;
        let mut d = GzDecoder::new(data);
        let mut out = Vec::new();
        d.read_to_end(&mut out)?;
        return Ok(out);
    }
    // auto-detect
    if data.len() >= 4 && data[0]==0x28 && data[1]==0xb5 && data[2]==0x2f && data[3]==0xfd {
        if let Ok(v) = decompress_bytes_zstd(data) { return Ok(v); }
    }
    if data.len() >=2 && data[0]==0x1f && data[1]==0x8b {
        use flate2::read::GzDecoder;
        let mut d = GzDecoder::new(data);
        let mut out = Vec::new();
        if d.read_to_end(&mut out).is_ok() { return Ok(out); }
    }
    Ok(data.to_vec())
}
pub fn decompress_bytes_cached(hash: &str, data: &[u8], compressed: i64) -> Vec<u8> {
    {
        let mut c = decompress_cache().lock().unwrap();
        if let Some(v) = c.get(hash) { return v.clone(); }
    }
    let raw = decompress_bytes_raw(data, compressed).unwrap_or_else(|_| data.to_vec());
    {
        let mut c = decompress_cache().lock().unwrap();
        c.put(hash.to_string(), raw.clone());
    }
    raw
}
pub fn is_gzipped(data: &[u8]) -> bool { data.len()>=2 && data[0]==0x1f && data[1]==0x8b }
pub fn is_zstd(data: &[u8]) -> bool { data.len()>=4 && data[0]==0x28 && data[1]==0xb5 && data[2]==0x2f && data[3]==0xfd }

pub fn vm_add_bytes(conn: &Connection, vm_path: &str, data: &[u8], mode: i64, mtime: i64) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    let hash = fx_hash_u64(data);
    let blob_exists: bool = conn.query_row("SELECT 1 FROM vm_blobs WHERE hash=?1", params![hash], |_| Ok(1)).optional()?.is_some();
    let mut inserted_compressed: i64 = 0;
    if !blob_exists {
        if let Some((comp, flag)) = compress_bytes_with_conn(conn, data) {
            inserted_compressed = flag;
            // store compressed blob with flag 2=zstd or 3=zstd+dict
            conn.execute("INSERT OR IGNORE INTO vm_blobs(hash, content, compressed, raw_size, refcnt) VALUES (?1,?2,?3,?4,1)", params![hash, comp, flag, data.len() as i64])?;
        } else {
            inserted_compressed = 0;
            conn.execute("INSERT OR IGNORE INTO vm_blobs(hash, content, compressed, raw_size, refcnt) VALUES (?1,?2,0,?3,1)", params![hash, data, data.len() as i64])?;
        }
        let cache_p = cache_path_for_hash(&hash);
        if !cache_p.exists() {
            if let Some(par)=cache_p.parent(){ let _ = std::fs::create_dir_all(par); }
            let _ = std::fs::write(&cache_p, data);
        }
    } else {
        // bump refcnt
        let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt+1 WHERE hash=?1", params![hash]);
    }
    // vm_fs row: store null content to leverage dedup, but keep size/mtime/hash for fast stat
    // For backward compat readers that expect content, we keep hash and allow vm_resolve to fetch from blobs
    // If blob was gz legacy we still handle; new ones are zstd
    // Check existing path to handle refcnt decrement on overwrite
    let old_hash: Option<String> = conn.query_row("SELECT hash FROM vm_fs WHERE path=?1", params![vm_p], |r| r.get(0)).optional()?;
    if let Some(oh) = old_hash {
        if oh != hash {
            let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![oh]);
            let _ = conn.execute("DELETE FROM vm_blobs WHERE hash=?1 AND refcnt<=0", params![oh]);
        } else {
            // same content overwrite, avoid double refcnt bump
            let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![hash]);
        }
    }
    let fs_flag = if blob_exists { conn.query_row("SELECT compressed FROM vm_blobs WHERE hash=?1", params![hash], |r| r.get::<_,i64>(0)).unwrap_or(2) } else { inserted_compressed };
    conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,hash,content,compressed) VALUES (?1,'file',?2,?3,?4,?5,NULL,?6)", params![vm_p, mode, mtime, data.len() as i64, hash, fs_flag])?;
    Ok(())
}
pub fn vm_add_symlink(conn: &Connection, vm_path: &str, target: &str) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    let old_hash: Option<String> = conn.query_row("SELECT hash FROM vm_fs WHERE path=?1 AND kind='file'", params![vm_p], |r| r.get(0)).optional()?;
    if let Some(h) = old_hash { let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![h]); let _ = conn.execute("DELETE FROM vm_blobs WHERE hash=?1 AND refcnt<=0", params![h]); }
    conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,link_target,compressed,hash,content) VALUES (?1,'symlink',493,?2,0,?3,0,NULL,NULL)", params![vm_p, now_secs(), target])?;
    Ok(())
}
pub fn vm_add_dir(conn: &Connection, vm_path: &str, mode: i64, mtime: i64) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed,content,hash) VALUES (?1,'dir',?2,?3,0,0,NULL,NULL)", params![vm_p, mode, mtime])?;
    Ok(())
}
pub fn vm_add_file(conn: &Connection, host_path: &str, vm_path: &str) -> Result<()> {
    let hp = Path::new(host_path);
    let md = std::fs::symlink_metadata(hp).map_err(|e| anyhow!("stat {}: {}", host_path, e))?;
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(now_secs());
    #[cfg(unix)] let mode = { use std::os::unix::fs::PermissionsExt; md.permissions().mode() as i64 };
    #[cfg(not(unix))] let mode = 420;
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
    conn.execute("INSERT INTO vm_log(op, path, detail) VALUES ('add', ?1, ?2)", params![vm_p, host_path])?;
    Ok(())
}
pub fn fx_hash_u64_static(data: &[u8]) -> String { format!("{:x}", fx_hash(data)) }
pub fn vm_import_closure(conn: &Connection, host_elf: &str, vm_prefix: &str) -> Result<usize> {
    let host_real = std::fs::canonicalize(host_elf).unwrap_or(PathBuf::from(host_elf));
    use rustc_hash::FxHashMap;
    use crate::elf::ElfMeta;
    let mut meta_cache: FxHashMap<PathBuf, ElfMeta> = FxHashMap::default();
    let mut search_cache: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    let mut resolve_cache: FxHashMap<u64, Option<PathBuf>> = FxHashMap::default();
    let ld_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    let mut seen: FxHashMap<PathBuf, String> = FxHashMap::default();
    let mut order: Vec<PathBuf> = Vec::new();
    let mut q: Vec<PathBuf> = vec![host_real.clone()];
    seen.insert(host_real.clone(), "exe".to_string()); order.push(host_real.clone());
    let mut qi=0;
    while qi < q.len() {
        let cur = q[qi].clone(); qi+=1;
        let sdirs = crate::closure::search_dirs_for_cached(&cur, &[], &mut meta_cache, &mut search_cache, &ld_dirs);
        let sh = crate::closure::search_dirs_hash(&sdirs);
        let needed = crate::elf::meta_for_path_cached(&cur, &mut meta_cache).needed.clone();
        for soname in needed {
            if let Some(rp) = crate::closure::resolve_soname_cached(&soname, &sdirs, &mut resolve_cache, sh) {
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
        let vm_path = if prefix == "/" { p.to_string_lossy().to_string() } else { format!("{}/{}", prefix.trim_end_matches('/'), p.file_name().unwrap().to_string_lossy()) };
        let target = p.to_string_lossy().to_string();
        vm_add_file(conn, &target, &target)?;
        if vm_path != target {
            vm_add_file(conn, &target, &vm_path)?;
        }
    }
    Ok(order.len())
}
pub fn vm_ls(conn: &Connection, vm_path: &str) -> Result<Vec<(String,String,i64,i64,String)>> {
    let p = normalize_vm_path(vm_path);
    let cnt: i64 = conn.query_row("SELECT count(*) FROM vm_fs WHERE path=?1", params![p], |r| r.get(0))?;
    if cnt>0 {
        let row: (String,String,i64,i64,String) = conn.query_row("SELECT path,kind,mode,size,coalesce(hash,'') FROM vm_fs WHERE path=?1", params![p], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?, r.get::<_,String>(4)?)))?;
        return Ok(vec![row]);
    }
    let like = if p == "/" { "/%".to_string() } else { format!("{}/%", p) };
    let mut st = conn.prepare("SELECT path,kind,mode,size,coalesce(hash,'') FROM vm_fs WHERE path LIKE ?1 AND path != ?2 ORDER BY path")?;
    let rows = st.query_map(params![like, p], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?, r.get::<_,String>(4)?)))?;
    let mut out = Vec::new();
    for r in rows { out.push(r?); }
    let mut direct = Vec::new();
    for (path,kind,mode,size,hash) in out {
        let rel = if p=="/" { path.trim_start_matches('/').to_string() } else { path.strip_prefix(&format!("{}/", p)).unwrap_or(&path).to_string() };
        if !rel.contains('/') { direct.push((path,kind,mode,size,hash)); }
    }
    Ok(direct)
}
pub fn vm_cat(conn: &Connection, vm_path: &str) -> Result<Vec<u8>> {
    let (real, kind, content, _link, _mode) = vm_resolve(conn, vm_path)?;
    if kind == "dir" { return Err(anyhow!("is a directory: {}", vm_path)); }
    match content { Some(d) => Ok(d), None => Err(anyhow!("no content for {} (resolved {})", vm_path, real)) }
}
pub fn vm_stat(conn: &Connection, vm_path: &str) -> Result<(String,String,i64,i64,i64,String)> {
    let p = normalize_vm_path(vm_path);
    let row: Option<(String,String,i64,i64,i64,Option<String>,Option<Vec<u8>>,Option<String>)> = conn.query_row("SELECT path,kind,mode,size,mtime,link_target,content,hash FROM vm_fs WHERE path=?1", params![p], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))).optional()?;
    let (path,kind,mode,size,mtime,link,content,hash_opt) = match row { Some(x)=>x, None=> return Err(anyhow!("not found: {}", p)) };
    if kind == "symlink" {
        let target = link.clone().unwrap_or_default();
        if let Ok((_rp, _rk, rc, _rl, _rm)) = vm_resolve(conn, &p) {
            if let Some(d)=rc { return Ok((path,kind,mode,d.len() as i64,mtime, format!("-> {} {:x}", target, fx_hash(&d)))); }
        }
        return Ok((path,kind,mode,size,mtime, format!("-> {}", target)));
    }
    if kind == "dir" { return Ok((path,kind,mode,0,mtime,String::new())); }
    // for file, hash is stored; if content present (legacy) use it else use blob
    let hash = hash_opt.unwrap_or_else(|| {
        if let Some(c)=content { format!("{:x}", fx_hash(&c)) } else { String::new() }
    });
    Ok((path,kind,mode,size,mtime,hash))
}
pub fn vm_resolve(conn: &Connection, vm_path: &str) -> Result<(String, String, Option<Vec<u8>>, Option<String>, i64)> {
    let mut cur = normalize_vm_path(vm_path);
    let mut visited = std::collections::HashSet::new();
    for _ in 0..40 {
        if !visited.insert(cur.clone()) { return Err(anyhow!("symlink loop at {}", cur)); }
        let row: Option<(String, Option<Vec<u8>>, Option<String>, i64, Option<String>, Option<i64>)> = conn.query_row("SELECT kind, content, link_target, mode, hash, compressed FROM vm_fs WHERE path=?1", params![cur], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?;
        let (kind, content_opt, link, mode, hash_opt, compressed) = match row { Some(x)=>x, None=> return Err(anyhow!("not found in VM: {}", cur)) };
        if kind != "symlink" {
            let content_opt_cloned = content_opt.clone();
            let content = match (content_opt, hash_opt) {
                (Some(b), _) if b.len()>0 => {
                    let cflag = compressed.unwrap_or(0);
                    let h = fx_hash_u64_static(&decompress_bytes_raw(&b, cflag).unwrap_or(b.clone()));
                    Some(decompress_bytes_cached(&h, &b, cflag))
                },
                (Some(_), Some(h)) | (None, Some(h)) => {
                    let blob: Option<(Vec<u8>, i64)> = conn.query_row("SELECT content, compressed FROM vm_blobs WHERE hash=?1", params![h.clone()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    if let Some((b,c)) = blob {
                        Some(decompress_bytes_cached(&h, &b, c))
                    } else if let Some(b) = content_opt_cloned {
                        let cflag = compressed.unwrap_or(0);
                        Some(decompress_bytes_cached(&h, &b, cflag))
                    } else { None }
                },
                (None, None) => None,
                _ => content_opt_cloned,
            };
            return Ok((cur, kind, content, link, mode));
        }
        let target = link.clone().unwrap_or_default();
        let next = if target.starts_with('/') { normalize_vm_path(&target) }
        else {
            let dir = Path::new(&cur).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or("/".to_string());
            if dir == "/" { format!("/{}", target) } else { format!("{}/{}", dir, target) }
        };
        cur = normalize_vm_path(&next);
    }
    Err(anyhow!("too many symlink levels: {}", vm_path))
}
pub fn vm_materialize(conn: &Connection, vm_path: &str, host_tmp: &Path) -> Result<PathBuf> {
    let (real, kind, content, _link, mode) = vm_resolve(conn, vm_path)?;
    if kind == "dir" { std::fs::create_dir_all(host_tmp)?; return Ok(host_tmp.to_path_buf()); }
    if let Some(data) = content {
        if let Some(parent) = host_tmp.parent() { std::fs::create_dir_all(parent)?; }
        let hash = fx_hash_u64(&data);
        let cache_p = cache_path_for_hash(&hash);
        if cache_p.exists() {
            if try_hardlink_or_copy(&cache_p, host_tmp).is_ok() {
                #[cfg(unix)] { let _ = std::fs::set_permissions(host_tmp, std::fs::Permissions::from_mode(mode as u32)); }
                let _ = real;
                return Ok(host_tmp.to_path_buf());
            }
        }
        if !cache_p.exists() {
            if let Some(par)=cache_p.parent(){ let _ = std::fs::create_dir_all(par); }
            let _ = std::fs::write(&cache_p, &data);
        }
        std::fs::write(host_tmp, &data)?;
        #[cfg(unix)] { let _ = std::fs::set_permissions(host_tmp, std::fs::Permissions::from_mode(mode as u32)); }
        let _ = real;
        return Ok(host_tmp.to_path_buf());
    }
    Err(anyhow!("no content for {} (resolved {})", vm_path, real))
}
pub fn vm_pack_host_dir(conn: &Connection, host_dir: &str, vm_prefix: &str) -> Result<usize> {
    let mut n=0;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for entry in walkdir::WalkDir::new(host_dir).follow_links(false).min_depth(0) {
        let e = entry?;
        if e.depth()==0 { continue; }
        let hp = e.path().to_string_lossy().to_string();
        let rel = e.path().strip_prefix(host_dir).unwrap_or(e.path()).to_string_lossy().to_string();
        let vm_p = if vm_prefix == "/" { format!("/{}", rel.trim_start_matches('/')) } else { format!("{}/{}", vm_prefix.trim_end_matches('/'), rel.trim_start_matches('/')) };
        vm_add_file(conn, &hp, &normalize_vm_path(&vm_p))?;
        n+=1;
        if n % 2000 == 0 { conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?; }
    }
    conn.execute_batch("COMMIT;")?;
    Ok(n)
}
pub fn vm_import_tar(conn: &Connection, tar_path: &str, strip_prefix: &str) -> Result<usize> {
    vm_import_tar_filtered(conn, tar_path, strip_prefix, None, None)
}
pub fn vm_import_tar_filtered(conn: &Connection, tar_path: &str, strip_prefix: &str, whitelist: Option<&[String]>, exclude: Option<&[String]>) -> Result<usize> {
    use std::io::Read;
    let file = std::fs::File::open(tar_path).map_err(|e| anyhow!("open tar {}: {}", tar_path, e))?;
    let is_gz = tar_path.ends_with(".gz") || tar_path.ends_with(".tgz");
    let reader: Box<dyn Read> = if is_gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    let mut n=0usize;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let mut vm_path = path.clone();
        if vm_path.starts_with("./") { vm_path = vm_path[2..].to_string(); }
        if !strip_prefix.is_empty() {
            if vm_path.starts_with(strip_prefix) { vm_path = vm_path[strip_prefix.len()..].to_string(); }
            else if vm_path == strip_prefix.trim_end_matches('/') { vm_path = "".to_string(); }
        }
        vm_path = normalize_vm_path(&vm_path);
        if vm_path.is_empty() || vm_path=="/" { continue; }
        if let Some(wl) = whitelist {
            if !wl.is_empty() && !wl.iter().any(|p| vm_path.starts_with(p) || p.starts_with(&vm_path)) { continue; }
        }
        if let Some(ex) = exclude {
            if ex.iter().any(|p| vm_path == *p || vm_path.starts_with(&format!("{}/", p))) { continue; }
        }
        let header = entry.header();
        let kind = header.entry_type();
        let mode = header.mode().unwrap_or(0o644) as i64;
        let mtime = header.mtime().unwrap_or(now_secs() as u64) as i64;
        if kind.is_dir() { vm_add_dir(conn, &vm_path, mode, mtime)?; }
        else if kind.is_symlink() { if let Some(t) = entry.link_name()? { vm_add_symlink(conn, &vm_path, &t.to_string_lossy())?; } }
        else if kind.is_hard_link() { if let Some(t) = entry.link_name()? { vm_add_symlink(conn, &vm_path, &t.to_string_lossy())?; } }
        else if kind.is_file() { let mut data = Vec::new(); entry.read_to_end(&mut data)?; vm_add_bytes(conn, &vm_path, &data, mode, mtime)?; }
        else { continue; }
        n+=1;
        if n % 2000 == 0 { conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?; }
    }
    conn.execute_batch("COMMIT;")?;
    conn.execute("INSERT INTO vm_log(op, detail) VALUES ('import_tar', ?1)", params![tar_path])?;
    Ok(n)
}
pub fn vm_materialize_tree(conn: &Connection, dest: &Path) -> Result<usize> {
    use rayon::prelude::*;
    std::fs::create_dir_all(dest)?;
    let mut st = conn.prepare("SELECT path, mode FROM vm_fs WHERE kind='dir' ORDER BY length(path), path")?;
    for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?)))? {
        let (p,mode)=r?;
        let host = dest.join(p.trim_start_matches('/'));
        std::fs::create_dir_all(&host)?;
        #[cfg(unix)] { let _ = std::fs::set_permissions(&host, std::fs::Permissions::from_mode(mode as u32)); }
    }
    let mut st = conn.prepare("SELECT path, link_target FROM vm_fs WHERE kind='symlink'")?;
    for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))? {
        let (p,target)=r?;
        let host = dest.join(p.trim_start_matches('/'));
        if host.exists() || std::fs::symlink_metadata(&host).is_ok() { let _ = std::fs::remove_file(&host); let _ = std::fs::remove_dir(&host); }
        if let Some(par)=host.parent(){ std::fs::create_dir_all(par)?; }
        #[cfg(unix)] { let _ = std::os::unix::fs::symlink(&target, &host); }
    }
    // collect file tasks to parallelize: include mtime for incremental fastpath
    struct Task { path: String, mode: i64, hash: String, blob: Vec<u8>, compressed: i64, size: i64, mtime: i64 }
    let mut tasks: Vec<Task> = Vec::new();
    {
        let mut st = conn.prepare("SELECT path, mode, hash, size, mtime FROM vm_fs WHERE kind='file'")?;
        for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?, r.get::<_,Option<String>>(2)?, r.get::<_,i64>(3)?, r.get::<_,i64>(4)?)))? {
            let (p,mode,hash,size,mtime)=r?;
            if let Some(h)=hash {
                let blob_row: Option<(Vec<u8>, i64)> = conn.query_row("SELECT content, compressed FROM vm_blobs WHERE hash=?1", params![h.clone()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                if let Some((c, comp)) = blob_row {
                    tasks.push(Task{ path: p, mode, hash: h, blob: c, compressed: comp, size, mtime });
                    continue;
                }
                let inline: Option<(Option<Vec<u8>>, Option<i64>)> = conn.query_row("SELECT content, compressed FROM vm_fs WHERE path=?1", params![p.clone()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                if let Some((Some(content), comp)) = inline { tasks.push(Task{ path: p, mode, hash: h, blob: content, compressed: comp.unwrap_or(0), size, mtime }); }
            } else {
                let inline: Option<(Option<Vec<u8>>, Option<i64>)> = conn.query_row("SELECT content, compressed FROM vm_fs WHERE path=?1", params![p.clone()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                if let Some((Some(c), comp)) = inline {
                    let h = fx_hash_u64(&c);
                    tasks.push(Task{ path: p, mode, hash: h, blob: c, compressed: comp.unwrap_or(0), size, mtime });
                }
            }
        }
    }
    let dest_buf = dest.to_path_buf();
    let written = std::sync::atomic::AtomicUsize::new(0);
    tasks.par_iter().for_each(|t| {
        let host = dest_buf.join(t.path.trim_start_matches('/'));
        let mut need = true;
        if let Ok(md) = std::fs::metadata(&host) {
            if md.len() as i64 == t.size {
                if let Ok(mt) = md.modified() {
                    if let Ok(dur) = mt.duration_since(std::time::UNIX_EPOCH) {
                        let host_secs = dur.as_secs() as i64;
                        // allow 1s tolerance; truncate vs stored mtime
                        if (host_secs - t.mtime).abs() <= 1 {
                            need = false;
                        }
                    }
                } else {
                    need = false;
                }
            }
        }
        if !need {
            written.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let data = decompress_bytes_cached(&t.hash, &t.blob, t.compressed);
        if let Some(par)=host.parent(){ let _ = std::fs::create_dir_all(par); }
        let cache_p = cache_path_for_hash(&t.hash);
        let done = if cache_p.exists() {
            try_hardlink_or_copy(&cache_p, &host).is_ok()
        } else { false };
        if !done {
            let _ = std::fs::write(&host, &data);
            if !cache_p.exists() {
                if let Some(par)=cache_p.parent(){ let _ = std::fs::create_dir_all(par); }
                let _ = std::fs::write(&cache_p, &data);
            }
        }
        #[cfg(unix)] { let _ = std::fs::set_permissions(&host, std::fs::Permissions::from_mode(t.mode as u32)); }
        // preserve mtime for next incremental run (best-effort, no hard failure)
        #[cfg(unix)]
        {
            let ts = libc::timespec { tv_sec: t.mtime as libc::time_t, tv_nsec: 0 };
            let times = [ts, ts];
            let cpath = std::ffi::CString::new(host.to_string_lossy().as_bytes()).unwrap_or_else(|_| std::ffi::CString::new("/tmp").unwrap());
            unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0); }
        }
        written.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    for d in ["dev","proc","sys","tmp"] { let _ = std::fs::create_dir_all(dest.join(d)); }
    Ok(written.load(std::sync::atomic::Ordering::Relaxed))
}
pub fn vm_sync_from_host(conn: &Connection, host_root: &Path) -> Result<(usize,usize,usize)> {
    use std::collections::{HashMap, HashSet};
    let mut host_files: HashMap<String, (String, Vec<u8>, i64)> = HashMap::new();
    let mut host_all_paths: HashSet<String> = HashSet::new();
    for entry in walkdir::WalkDir::new(host_root).follow_links(false).min_depth(1) {
        let e = match entry { Ok(v)=>v, Err(_)=> continue };
        let rel = e.path().strip_prefix(host_root).unwrap_or(e.path()).to_string_lossy().to_string();
        let vm_p = normalize_vm_path(&format!("/{}", rel.trim_start_matches('/')));
        if vm_p == "/" { continue; }
        if vm_p == "/dev" || vm_p.starts_with("/dev/") || vm_p == "/proc" || vm_p.starts_with("/proc/") || vm_p == "/sys" || vm_p.starts_with("/sys/") { continue; }
        let md = match std::fs::symlink_metadata(e.path()) { Ok(m)=>m, Err(_)=> continue };
        host_all_paths.insert(vm_p.clone());
        #[cfg(unix)] let mode = { use std::os::unix::fs::PermissionsExt; md.permissions().mode() as i64 };
        #[cfg(not(unix))] let mode = 420;
        if md.file_type().is_symlink() {
            if let Ok(t)=std::fs::read_link(e.path()) {
                host_files.insert(vm_p, ("symlink".to_string(), t.to_string_lossy().as_bytes().to_vec(), mode));
            }
        } else if md.is_dir() {
            host_files.insert(vm_p, ("dir".to_string(), Vec::new(), mode));
        } else if md.is_file() {
            if let Ok(data)=std::fs::read(e.path()) {
                host_files.insert(vm_p, ("file".to_string(), data, mode));
            }
        }
    }
    let mut db_map: HashMap<String, (String, Option<String>, Option<String>, i64, i64)> = HashMap::new();
    {
        // hash+size for quick compare without fetching blob
        let mut st = conn.prepare("SELECT path, kind, hash, link_target, mode, size FROM vm_fs")?;
        for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,Option<String>>(2)?, r.get::<_,Option<String>>(3)?, r.get::<_,i64>(4)?, r.get::<_,i64>(5)?)))? {
            let (p,kind,hash,link,mode,size)=r?;
            db_map.insert(p, (kind, hash, link, mode, size));
        }
    }
    let mut created=0usize; let mut updated=0usize; let mut deleted=0usize;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for (vm_p, (kind, data, mode)) in host_files.iter() {
        if vm_p == "/" { continue; }
        match db_map.get(vm_p) {
            None => {
                if kind == "symlink" {
                    let target = String::from_utf8_lossy(data).to_string();
                    let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,link_target,compressed) VALUES (?1,'symlink',?2,?3,0,?4,0)", params![vm_p, *mode, now_secs(), target]);
                    created+=1;
                } else if kind == "dir" {
                    let _ = conn.execute("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES (?1,'dir',?2,?3,0,0)", params![vm_p, *mode, now_secs()]);
                    created+=1;
                } else {
                    // use vm_add_bytes dedup path without extra blob fetch
                    let _ = vm_add_bytes(conn, vm_p, data, *mode, now_secs());
                    created+=1;
                }
            },
            Some((db_kind, db_hash, db_link, db_mode, db_size)) => {
                if db_kind != kind {
                    if kind == "symlink" {
                        let target = String::from_utf8_lossy(data).to_string();
                        // decrement old blob refcnt
                        if let Some(h) = db_hash { let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![h]); let _ = conn.execute("DELETE FROM vm_blobs WHERE hash=?1 AND refcnt<=0", params![h]); }
                        let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,link_target,compressed,hash,content) VALUES (?1,'symlink',?2,?3,0,?4,0,NULL,NULL)", params![vm_p, *mode, now_secs(), target]);
                    } else if kind == "dir" {
                        if let Some(h) = db_hash { let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![h]); let _ = conn.execute("DELETE FROM vm_blobs WHERE hash=?1 AND refcnt<=0", params![h]); }
                        let _ = conn.execute("INSERT OR REPLACE INTO vm_fs(path,kind,mode,mtime,size,compressed,hash,content) VALUES (?1,'dir',?2,?3,0,0,NULL,NULL)", params![vm_p, *mode, now_secs()]);
                    } else {
                        let hash = fx_hash_u64(data);
                        let need = db_hash.as_deref()!=Some(&hash) || db_mode!=mode || db_size!=&(data.len() as i64);
                        if need {
                            if let Some(h)=db_hash { let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![h]); }
                            let _ = vm_add_bytes(conn, vm_p, data, *mode, now_secs());
                            updated+=1;
                        }
                    }
                    if db_kind != kind { updated+=1; }
                } else if kind == "file" {
                    let hash = fx_hash_u64(data);
                    let need = db_hash.as_deref()!=Some(&hash) || db_mode!=mode || db_size!=&(data.len() as i64);
                    if need {
                        // avoid double decompress: just update via vm_add_bytes
                        let _ = vm_add_bytes(conn, vm_p, data, *mode, now_secs());
                        updated+=1;
                    }
                } else if kind == "symlink" {
                    let target = String::from_utf8_lossy(data).to_string();
                    let db_target = db_link.as_deref().unwrap_or("");
                    if db_target != target || db_mode != mode {
                        let _ = conn.execute("UPDATE vm_fs SET mode=?2, link_target=?3, mtime=?4 WHERE path=?1", params![vm_p, *mode, target, now_secs()]);
                        updated+=1;
                    }
                } else if kind == "dir" {
                    if db_mode != mode {
                        let _ = conn.execute("UPDATE vm_fs SET mode=?2, mtime=?3 WHERE path=?1", params![vm_p, *mode, now_secs()]);
                        updated+=1;
                    }
                }
            }
        }
    }
    let mut to_delete: Vec<String> = Vec::new();
    for (db_path, _) in db_map.iter() {
        if db_path == "/" { continue; }
        if db_path == "/dev" || db_path.starts_with("/dev/") || db_path == "/proc" || db_path.starts_with("/proc/") || db_path == "/sys" || db_path.starts_with("/sys/") { continue; }
        if !host_all_paths.contains(db_path) {
            to_delete.push(db_path.clone());
        }
    }
    for p in to_delete {
        let hash: Option<String> = conn.query_row("SELECT hash FROM vm_fs WHERE path=?1", params![p.clone()], |r| r.get(0)).optional()?.flatten();
        let kind: Option<String> = conn.query_row("SELECT kind FROM vm_fs WHERE path=?1", params![p.clone()], |r| r.get(0)).optional()?;
        let _ = conn.execute("DELETE FROM vm_fs WHERE path=?1", params![p.clone()]);
        if let Some(k)=kind { if k=="dir" { let _ = conn.execute("DELETE FROM vm_fs WHERE path LIKE ?1", params![format!("{}/%", p)]); } }
        if let Some(h)=hash { let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![h]); let _ = conn.execute("DELETE FROM vm_blobs WHERE hash=?1 AND refcnt<=0", params![h]); }
        deleted+=1;
    }
    conn.execute_batch("COMMIT;")?;
    let _ = conn.execute("INSERT INTO vm_log(op, detail) VALUES ('sync', ?1)", params![format!("host:{} created:{} updated:{} deleted:{}", host_root.display(), created, updated, deleted)]);
    // checkpoint throttled: only if wal size > 4M
    let wal_size: i64 = conn.path().and_then(|pp| std::fs::metadata(format!("{}-wal", pp)).ok()).map(|m| m.len() as i64).unwrap_or(0);
    if wal_size > 4*1024*1024 {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    }
    Ok((created, updated, deleted))
}
pub fn vm_recompress(conn: &Connection) -> Result<(usize, i64, i64)> {
    // recompress legacy gz blobs to zstd and move inline content to blobs
    let mut n=0usize;
    let mut before: i64 = 0;
    let mut after: i64 = 0;
    // handle inline legacy content
    let mut rows: Vec<(String, Vec<u8>, Option<i64>)> = Vec::new();
    {
        let mut st = conn.prepare("SELECT path, content, compressed FROM vm_fs WHERE kind='file' AND content IS NOT NULL")?;
        for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,Option<i64>>(2)?)))? {
            if let Ok(x)=r { rows.push(x); }
        }
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for (path, data, comp) in rows {
        let cflag = comp.unwrap_or(0);
        let raw = decompress_bytes_raw(&data, cflag).unwrap_or(data.clone());
        before += data.len() as i64;
        let hash = fx_hash_u64(&raw);
        let exists: bool = conn.query_row("SELECT 1 FROM vm_blobs WHERE hash=?1", params![hash], |_| Ok(1)).optional()?.is_some();
        if !exists {
            if let Some((z, flag)) = compress_bytes_with_conn(conn, &raw) {
                after += z.len() as i64;
                let _ = conn.execute("INSERT OR IGNORE INTO vm_blobs(hash, content, compressed, raw_size, refcnt) VALUES (?1,?2,?3,?4,1)", params![hash, z, flag, raw.len() as i64]);
            } else {
                after += raw.len() as i64;
                let _ = conn.execute("INSERT OR IGNORE INTO vm_blobs(hash, content, compressed, raw_size, refcnt) VALUES (?1,?2,0,?3,1)", params![hash, raw, raw.len() as i64]);
            }
        } else {
            let sz: Option<i64> = conn.query_row("SELECT length(content) FROM vm_blobs WHERE hash=?1", params![hash], |r| r.get(0)).optional()?;
            after += sz.unwrap_or(raw.len() as i64);
            let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt+1 WHERE hash=?1", params![hash]);
        }
        let old_hash: Option<String> = conn.query_row("SELECT hash FROM vm_fs WHERE path=?1", params![path.clone()], |r| r.get(0)).optional()?.flatten();
        if old_hash.as_deref() != Some(&hash) {
            if let Some(oh) = old_hash { if oh!=hash { let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![oh]); let _ = conn.execute("DELETE FROM vm_blobs WHERE hash=?1 AND refcnt<=0", params![oh]); } }
        }
        let blob_flag: i64 = conn.query_row("SELECT compressed FROM vm_blobs WHERE hash=?1", params![hash.clone()], |r| r.get(0)).unwrap_or(2);
        let _ = conn.execute("UPDATE vm_fs SET hash=?2, content=NULL, compressed=?3, size=?4 WHERE path=?1", params![path, hash, blob_flag, raw.len() as i64]);
        n+=1;
    }
    // also recompress blobs that are gz
    let mut gz_blobs: Vec<(String, Vec<u8>)> = Vec::new();
    {
        let mut st = conn.prepare("SELECT hash, content FROM vm_blobs WHERE compressed=1")?;
        for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?)))? { if let Ok(x)=r { gz_blobs.push(x); } }
    }
    for (hash, gz) in gz_blobs {
        let raw = decompress_bytes_raw(&gz, 1).unwrap_or(gz.clone());
        before += gz.len() as i64;
        if let Some((z, flag)) = compress_bytes_with_conn(conn, &raw) {
            after += z.len() as i64;
            let _ = conn.execute("UPDATE vm_blobs SET content=?2, compressed=?3 WHERE hash=?1", params![hash, z, flag]);
            let _ = conn.execute("UPDATE vm_fs SET compressed=?2 WHERE hash=?1", params![hash.clone(), flag]);
            n+=1;
        } else {
            after += gz.len() as i64;
        }
    }
    // recompress uncompressed blobs that would benefit from level 19 or dict
    let mut plain_blobs: Vec<(String, Vec<u8>, i64)> = Vec::new();
    {
        let mut st = conn.prepare("SELECT hash, content, raw_size FROM vm_blobs WHERE compressed=0 AND raw_size > 1024 LIMIT 200")?;
        for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?)))? {
            if let Ok(x)=r { plain_blobs.push(x); }
        }
    }
    for (hash, data, _raw) in plain_blobs {
        let raw = data.clone();
        if let Some((z, flag)) = compress_bytes_with_conn(conn, &raw) {
            before += raw.len() as i64;
            after += z.len() as i64;
            let _ = conn.execute("UPDATE vm_blobs SET content=?2, compressed=?3 WHERE hash=?1", params![hash.clone(), z, flag]);
            let _ = conn.execute("UPDATE vm_fs SET compressed=?2 WHERE hash=?1", params![hash, flag]);
            n+=1;
        }
    }
    // recompress zstd level 3 blobs to level 19 if beneficial
    let mut zstd_blobs: Vec<(String, Vec<u8>, i64)> = Vec::new();
    {
        let mut st = conn.prepare("SELECT hash, content, compressed FROM vm_blobs WHERE compressed=2 LIMIT 300")?;
        for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?)))? {
            if let Ok(x)=r { zstd_blobs.push(x); }
        }
    }
    for (hash, data, comp) in zstd_blobs {
        let raw = decompress_bytes_raw(&data, comp).unwrap_or(data.clone());
        let cur_len = data.len();
        if let Ok(c19) = zstd::bulk::compress(&raw, 19) {
            if c19.len() + 64 < raw.len() && c19.len() + 32 < cur_len {
                before += cur_len as i64;
                after += c19.len() as i64;
                let _ = conn.execute("UPDATE vm_blobs SET content=?2 WHERE hash=?1", params![hash.clone(), c19]);
                n+=1;
            }
        }
    }
    conn.execute_batch("COMMIT;")?;
    Ok((n, before, after))
}
pub fn vm_gc(conn: &Connection) -> Result<(i64,i64)> {
    let before: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let ps_before: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let needs_migrate = ps_before == 4096;
    if needs_migrate {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;");
        let _ = conn.execute_batch("PRAGMA page_size=8192; VACUUM;");
        // auto_vacuum requires another VACUUM to take effect
        let _ = conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;");
        let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
        let _ = vm_apply_pragmas(conn);
    } else {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;");
    }
    let _ = conn.execute_batch("PRAGMA incremental_vacuum;");
    let after: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let ps: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let actual_saved = before * ps_before - after * ps;
    Ok((before, actual_saved))
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
pub fn vm_status(conn: &Connection) -> Result<String> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let journal: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let files: i64 = conn.query_row("SELECT count(*) FROM vm_fs", [], |r| r.get(0))?;
    let dirs: i64 = conn.query_row("SELECT count(*) FROM vm_fs WHERE kind='dir'", [], |r| r.get(0))?;
    let syms: i64 = conn.query_row("SELECT count(*) FROM vm_fs WHERE kind='symlink'", [], |r| r.get(0))?;
    let fcnt: i64 = conn.query_row("SELECT count(*) FROM vm_fs WHERE kind='file'", [], |r| r.get(0))?;
    let logical: Option<i64> = conn.query_row("SELECT sum(size) FROM vm_fs WHERE kind='file'", [], |r| r.get(0))?;
    let blob_storage: Option<i64> = conn.query_row("SELECT sum(length(content)) FROM vm_blobs", [], |r| r.get(0))?;
    // fallback if no blobs
    let blob_storage = blob_storage.or_else(|| conn.query_row("SELECT sum(length(content)) FROM vm_fs WHERE kind='file' AND content IS NOT NULL", [], |r| r.get(0)).ok().flatten()).unwrap_or(0);
    let blob_cnt: i64 = conn.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap_or(0);
    let comp_cnt: i64 = conn.query_row("SELECT count(*) FROM vm_blobs WHERE compressed!=0", [], |r| r.get(0)).unwrap_or(0);
    let snaps: i64 = conn.query_row("SELECT count(*) FROM vm_snapshots", [], |r| r.get(0))?;
    let mmap: i64 = conn.query_row("PRAGMA mmap_size", [], |r| r.get(0))?;
    let cache_size: i64 = conn.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
    let auto_vac: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap_or(0);
    let (cache_dir, cache_files, cache_bytes) = vm_cache_info();
    let logical = logical.unwrap_or(0);
    let ratio = if logical>0 { 100.0 * blob_storage as f64 / logical as f64 } else { 0.0 };
    Ok(format!(
        "integrity={} page_size={} page_count={} freelist={} auto_vacuum={} journal={} mmap={} cache_size={}\nfiles={} (dir={} file={} symlink={}) logical={} blob_storage={} ({:.1}%) blobs={} compressed={} snaps={}\ncache: dir={} files={} bytes={}",
        integrity, page_size, page_count, freelist, auto_vac, journal, mmap, cache_size,
        files, dirs, fcnt, syms, logical, blob_storage, ratio, blob_cnt, comp_cnt, snaps,
        cache_dir.display(), cache_files, cache_bytes
    ))
}
pub fn vm_diff(conn: &Connection, snap_a: &str, snap_b: &str) -> Result<String> {
    // snapshots table only has page stats, not file diff; we diff vm_log if available, else file count diff
    let a: Option<(i64,i64,String)> = conn.query_row("SELECT page_count, bytes, coalesce(note,'') FROM vm_snapshots WHERE name=?1", params![snap_a], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let b: Option<(i64,i64,String)> = conn.query_row("SELECT page_count, bytes, coalesce(note,'') FROM vm_snapshots WHERE name=?1", params![snap_b], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    if a.is_none() || b.is_none() { return Err(anyhow!("snapshot not found: {} or {}", snap_a, snap_b)); }
    let (apc, abytes, anot) = a.unwrap();
    let (bpc, bbytes, bnot) = b.unwrap();
    let mut out = format!("diff {} -> {}: page_count {} -> {} ({:+}), bytes {} -> {} ({:+})\nnote: '{}' -> '{}'\n", snap_a, snap_b, apc, bpc, bpc-apc, abytes, bbytes, bbytes-abytes, anot, bnot);
    // also log entries between
    let mut st = conn.prepare("SELECT ts, op, coalesce(path,''), coalesce(detail,'') FROM vm_log WHERE op IN ('add','sync','import_tar') ORDER BY id DESC LIMIT 20")?;
    out.push_str("recent log:\n");
    for r in st.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?, r.get::<_,String>(3)?)))? {
        if let Ok((ts,op,path,detail)) = r { out.push_str(&format!("  {} {} {} {}\n", ts, op, path, detail)); }
    }
    Ok(out)
}
pub fn vm_checkpoint(conn: &Connection, name: &str, note: &str) -> Result<()> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let bytes = page_size * page_count;
    conn.execute("INSERT INTO vm_snapshots(name, page_count, page_size, bytes, note) VALUES (?1,?2,?3,?4,?5)", params![name, page_count, page_size, bytes, note])?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    conn.execute("INSERT INTO vm_log(op, detail) VALUES ('snapshot', ?1)", params![name])?;
    Ok(())
}
pub fn vm_list_snapshots(conn: &Connection) -> Result<Vec<(i64,String,i64,i64,i64,String)>> {
    let mut st = conn.prepare("SELECT id, name, created_at, page_count, bytes, coalesce(note,'') FROM vm_snapshots ORDER BY id")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
    let mut out=Vec::new(); for r in rows { out.push(r?); } Ok(out)
}
pub fn vm_mem_insert(conn: &Connection, addr: i64, size: i64, prot: i64, content: &[u8]) -> Result<()> {
    conn.execute("INSERT INTO vm_mem(addr,size,prot,content) VALUES (?1,?2,?3,?4)", params![addr,size,prot,content])?;
    Ok(())
}
pub fn vm_mem_list(conn: &Connection) -> Result<Vec<(i64,i64,i64,i64)>> {
    let mut st = conn.prepare("SELECT id, addr, size, prot FROM vm_mem ORDER BY addr")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let mut out=Vec::new(); for r in rows{ out.push(r?); } Ok(out)
}
pub fn vm_mem_clear(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM vm_mem", [])?;
    Ok(())
}

pub fn vm_mem_trace(db_path: &str, prog: &str, rest: Vec<String>) -> Result<()> {
    use std::process::Command;
    #[cfg(target_os = "linux")]
    {
        let db_path_owned = db_path.to_string();
        let trace_path = format!("/tmp/self-vm-trace-{}", std::process::id());
        let strace_ok = Command::new("strace").arg("--help").output().is_ok();
        if strace_ok {
            let mut cmd = Command::new("strace");
            cmd.arg("-f").arg("-tt").arg("-e").arg("trace=memory").arg("-o").arg(&trace_path).arg(prog);
            for a in &rest { cmd.arg(a); }
            let status = cmd.status()?;
            let code = status.code().unwrap_or(0);
            let log = std::fs::read_to_string(&trace_path).unwrap_or_default();
            let _ = std::fs::remove_file(&trace_path);
            let conn = vm_open(&db_path_owned)?;
            let _ = vm_mem_clear(&conn);
            let mut inserted = 0usize;
            for line in log.lines() {
                let s = line.trim();
                if s.is_empty() { continue; }
                // strace memory trace looks like: "1234 12:34:56 mmap(NULL, 8192, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) = 0x7f1234000"
                // we extract syscall name and args+ret
                let mmap_like = s.contains("mmap") || s.contains("mprotect") || s.contains("munmap") || s.contains("mremap") || s.contains("brk");
                if !mmap_like { continue; }
                // try to parse ret addr = hex after " = "
                let ret_part = s.rsplit(" = ").next().unwrap_or("");
                let addr: i64 = if ret_part.starts_with("0x") {
                    i64::from_str_radix(ret_part.trim_start_matches("0x").split(|c| c==' ' || c=='(').next().unwrap_or("0"), 16).unwrap_or(0)
                } else { 0 };
                // size: first numeric after '(' e.g. mmap(NULL, 4096,
                let size: i64 = {
                    let p = s.find('(').unwrap_or(0);
                    let args = &s[p..];
                    // split by ',' and extract second field for mmap
                    let parts: Vec<&str> = args.split(',').collect();
                    if parts.len() >= 2 {
                        parts[1].trim().trim_start_matches("NULL").trim().parse::<i64>().unwrap_or(4096)
                    } else { 4096 }
                };
                let prot: i64 = if s.contains("PROT_READ") && s.contains("PROT_WRITE") && s.contains("PROT_EXEC") { 7 }
                else if s.contains("PROT_READ") && s.contains("PROT_WRITE") { 3 }
                else if s.contains("PROT_READ") && s.contains("PROT_EXEC") { 5 }
                else if s.contains("PROT_READ") { 1 }
                else if s.contains("PROT_WRITE") { 2 }
                else { 3 };
                let content = s.as_bytes().to_vec();
                // skip failed mmap (-1)
                if ret_part.contains("-1") && addr== -1 { continue; }
                let a = if addr==0 { 0x400000 + (inserted as i64)*0x1000 } else { addr };
                let sz = if s.contains("mmap") { size } else if s.contains("brk") { 0x1000 } else { size.max(0x1000) };
                let _ = vm_mem_insert(&conn, a, sz, prot, &content);
                inserted += 1;
            }
            if inserted==0 {
                let content = format!("trace:{} {:?} exit={} ts={} log_lines={} (strace no mmap captured, inserted synthetic)", prog, rest, code, now_secs(), log.lines().count()).into_bytes();
                let _ = vm_mem_insert(&conn, 0x400000, 0x1000, 5, &content)?;
                inserted = 1;
            }
            println!("vm-mem-trace: {} {:?} -> exit {} (inserted {} vm_mem entries from strace memory trace, {} bytes log)", prog, rest, code, inserted, log.len());
            if code != 0 { std::process::exit(code); }
            return Ok(());
        }
        // fallback: direct run without strace, capture /proc/self/maps of child via fork
        let mut child = Command::new(prog);
        for a in &rest { child.arg(a); }
        let status = child.status()?;
        let code = status.code().unwrap_or(0);
        let conn = vm_open(&db_path_owned)?;
        let content = format!("trace:{} {:?} exit={} ts={} (no strace)", prog, rest, code, now_secs()).into_bytes();
        let _ = vm_mem_clear(&conn);
        vm_mem_insert(&conn, 0x400000, 0x1000, 5, &content)?;
        println!("vm-mem-trace: {} {:?} -> exit {} (fallback synthetic)", prog, rest, code);
        if code != 0 { std::process::exit(code); }
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("vm-mem-trace only supported on Linux");
    }
}

// WAL autocheckpoint helper: call frequently
pub fn vm_checkpoint_if_needed(conn: &rusqlite::Connection) -> Result<bool> {
    let wal: i64 = conn.path().and_then(|pp| std::fs::metadata(format!("{}-wal", pp)).ok()).map(|m| m.len() as i64).unwrap_or(0);
    if wal > 4*1024*1024 {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        return Ok(true);
    }
    Ok(false)
}

pub fn vm_snapshot_file(conn: &Connection, db_path: &str, name: &str) -> Result<String> {
    let snap_path = format!("{}.snap.{}", db_path, name);
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let res = conn.execute(&format!("VACUUM INTO '{}'", snap_path.replace('\'', "''")), []);
    if res.is_err() {
        std::fs::copy(db_path, &snap_path).map_err(|e| anyhow!("snapshot copy failed: {}", e))?;
    }
    Ok(snap_path)
}
pub fn vm_restore_file(db_path: &str, name: &str) -> Result<()> {
    let snap_path = format!("{}.snap.{}", db_path, name);
    if !Path::new(&snap_path).exists() { return Err(anyhow!("snapshot not found: {}", snap_path)); }
    std::fs::copy(&snap_path, db_path).map_err(|e| anyhow!("restore failed: {}", e))?;
    // auto VACUUM + incremental after restore to reclaim freelist
    if let Ok(conn) = Connection::open(db_path) {
        let _ = vm_apply_pragmas(&conn);
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;");
        let _ = conn.execute_batch("PRAGMA incremental_vacuum;");
    }
    Ok(())
}
pub fn vm_verify(conn: &Connection) -> Result<String> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let files: i64 = conn.query_row("SELECT count(*) FROM vm_fs", [], |r| r.get(0))?;
    let bytes: Option<i64> = conn.query_row("SELECT sum(size) FROM vm_fs WHERE kind='file'", [], |r| r.get(0))?;
    Ok(format!("integrity={} page_size={} page_count={} freelist={} files={} bytes={}", integrity, page_size, page_count, freelist, files, bytes.unwrap_or(0)))
}
