use anyhow::{anyhow, Result};
#[cfg(unix)] use std::os::unix::fs::PermissionsExt;
use rusqlite::{Connection, params, OptionalExtension};
use std::path::{Path, PathBuf};
use std::io::{Read, Write};
use std::sync::{Arc, OnceLock, Mutex};
use lru::LruCache;
use std::num::NonZeroUsize;

pub const VM_APP_ID: u32 = 0x564D5351; // 'VMSQ'
/// Schema versions (PRAGMA user_version):
///   1 = legacy: FxHash text keys, no triggers, manual refcnt
///   2 = triggers maintain vm_blobs.refcnt
///   3 = triggers maintain vm_meta counters (blob_bytes/file_count/logical_bytes)
///   4 = blob keys are raw 32-byte SHA-256 BLOBs (UNIQUE); vm_blobs keeps a rowid so large
///       content is not stored inside a key B-tree (WITHOUT ROWID measured no smaller)
pub const VM_USER_VERSION: u32 = 4;

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
pub fn decompress_cache() -> &'static Mutex<LruCache<String, Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<LruCache<String, Arc<Vec<u8>>>>> = OnceLock::new();
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


fn ensure_dict_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vm_dict(\n  id INTEGER PRIMARY KEY CHECK(id=1),\n  dict BLOB NOT NULL,\n  samples INTEGER NOT NULL,\n  dict_size INTEGER NOT NULL,\n  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))\n);"
    )?;
    Ok(())
}
fn ensure_disk_blocks_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vm_disk_blocks(block_id INTEGER PRIMARY KEY, content BLOB NOT NULL, compressed INTEGER NOT NULL DEFAULT 0, raw_size INTEGER NOT NULL);"
    )?;
    Ok(())
}

/// Upgrade schema in order. Each step runs in its own transaction; user_version is
/// bumped inside the same transaction so a crash leaves the DB at the last good version.
fn vm_migrate(conn: &Connection) -> Result<()> {
    let mut ver: u32 = conn.prepare_cached("PRAGMA user_version")?.query_row( [], |r| r.get(0))?;
    if ver >= VM_USER_VERSION { return Ok(()); }
    if ver == 0 { ver = 1; } // pre-versioned DBs are schema 1
    while ver < VM_USER_VERSION {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let step: Result<()> = (|| {
            match ver {
                1 => migrate_1_to_2(conn)?,
                2 => migrate_2_to_3(conn)?,
                3 => migrate_3_to_4(conn)?,
                v => return Err(anyhow!("no migration from schema {}", v)),
            }
            conn.execute_batch(&format!("PRAGMA user_version={};", ver + 1))?;
            Ok(())
        })();
        match step {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => { let _ = conn.execute_batch("ROLLBACK;"); return Err(e); }
        }
        ver += 1;
    }
    Ok(())
}

/// 1 -> 2: triggers keep refcnt in sync with vm_fs; counters in vm_meta.
fn migrate_1_to_2(conn: &Connection) -> Result<()> {
    // Rebuild refcnt from scratch so any drift left by manual counting is corrected.
    conn.execute_batch("UPDATE vm_blobs SET refcnt=0;
        UPDATE vm_blobs SET refcnt=(SELECT count(*) FROM vm_fs f WHERE f.hash=vm_blobs.hash);
        DELETE FROM vm_blobs WHERE refcnt=0;")?;
    conn.execute_batch(VM_TRIGGERS_SQL)?;
    Ok(())
}

/// 3 -> 4: re-key every blob by sha256(raw content) as a 32-byte BLOB, rebuild vm_blobs
/// as WITHOUT ROWID, and rewrite vm_fs.hash. Triggers are dropped during the rewrite and
/// recreated afterwards; refcnt is recomputed from vm_fs, so no drift survives.
fn migrate_3_to_4(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        DROP TRIGGER IF EXISTS vm_fs_hash_ins; DROP TRIGGER IF EXISTS vm_fs_hash_del; DROP TRIGGER IF EXISTS vm_fs_hash_upd;
        DROP TRIGGER IF EXISTS vm_cnt_blob_ins; DROP TRIGGER IF EXISTS vm_cnt_blob_del; DROP TRIGGER IF EXISTS vm_cnt_blob_upd;
        DROP TABLE IF EXISTS vm_key_map;
        CREATE TEMP TABLE vm_key_map(old_key TEXT PRIMARY KEY, new_key BLOB NOT NULL);
        CREATE TABLE vm_blobs_v4(
            id INTEGER PRIMARY KEY,
            hash BLOB UNIQUE NOT NULL,
            content BLOB NOT NULL,
            compressed INTEGER NOT NULL DEFAULT 0,
            raw_size INTEGER NOT NULL,
            refcnt INTEGER NOT NULL DEFAULT 0
        );")?;

    // Re-key blob by sha256 of raw bytes. Stored bytes are kept as-is (same compression).
    let old: Vec<(String, Vec<u8>, i64)> = {
        let mut st = conn.prepare("SELECT hash, content, compressed FROM vm_blobs")?;
        let rows = st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (old_key, content, comp) in old {
        let raw = decompress_bytes_raw(&content, comp)
            .map_err(|e| anyhow!("migration: blob {} undecodable: {}", old_key, e))?;
        let key = blob_key(&raw);
        conn.prepare_cached("INSERT OR IGNORE INTO vm_blobs_v4(hash, content, compressed, raw_size, refcnt) VALUES (?1,?2,?3,?4,0)")?
            .execute(params![key.to_vec(), content, comp, raw.len() as i64])?;
        conn.prepare_cached("INSERT OR REPLACE INTO vm_key_map(old_key, new_key) VALUES (?1,?2)")?
            .execute(params![old_key, key.to_vec()])?;
    }

    // Rewrite references. Unmapped (dangling) hashes become NULL rather than pointing nowhere.
    conn.execute_batch("
        UPDATE vm_fs SET hash = (SELECT new_key FROM vm_key_map m WHERE m.old_key = vm_fs.hash)
          WHERE hash IS NOT NULL;
        DROP TABLE vm_blobs;
        ALTER TABLE vm_blobs_v4 RENAME TO vm_blobs;
        DROP TABLE vm_key_map;
        UPDATE vm_blobs SET refcnt = (SELECT count(*) FROM vm_fs f WHERE f.hash = vm_blobs.hash);
        DELETE FROM vm_blobs WHERE refcnt = 0;
        CREATE INDEX IF NOT EXISTS idx_vm_blobs_refcnt ON vm_blobs(refcnt);
        CREATE INDEX IF NOT EXISTS idx_vm_fs_hash ON vm_fs(hash);
    ")?;
    conn.execute_batch(VM_TRIGGERS_SQL)?;
    conn.execute_batch(VM_COUNTER_TRIGGERS_SQL)?;
    conn.execute_batch(VM_COUNTERS_INIT_SQL)?;
    Ok(())
}

/// 2 -> 3: O(1) aggregate counters maintained by triggers, initialised from current data.
fn migrate_2_to_3(conn: &Connection) -> Result<()> {
    conn.execute_batch(VM_COUNTERS_INIT_SQL)?;
    conn.execute_batch(VM_COUNTER_TRIGGERS_SQL)?;
    Ok(())
}

const VM_TRIGGERS_SQL: &str = "
CREATE TRIGGER IF NOT EXISTS vm_fs_hash_ins AFTER INSERT ON vm_fs WHEN NEW.hash IS NOT NULL
BEGIN UPDATE vm_blobs SET refcnt=refcnt+1 WHERE hash=NEW.hash; END;
CREATE TRIGGER IF NOT EXISTS vm_fs_hash_del AFTER DELETE ON vm_fs WHEN OLD.hash IS NOT NULL
BEGIN
  UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=OLD.hash;
  DELETE FROM vm_blobs WHERE hash=OLD.hash AND refcnt<=0;
END;
CREATE TRIGGER IF NOT EXISTS vm_fs_hash_upd AFTER UPDATE OF hash ON vm_fs WHEN OLD.hash IS NOT NEW.hash
BEGIN
  UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=OLD.hash AND OLD.hash IS NOT NULL;
  DELETE FROM vm_blobs WHERE hash=OLD.hash AND refcnt<=0 AND OLD.hash IS NOT NULL;
  UPDATE vm_blobs SET refcnt=refcnt+1 WHERE hash=NEW.hash AND NEW.hash IS NOT NULL;
END;
";

/// Counter maintenance. Values are TEXT in vm_meta; updated with CAST arithmetic.
const VM_COUNTER_TRIGGERS_SQL: &str = "
CREATE TRIGGER IF NOT EXISTS vm_cnt_fs_ins AFTER INSERT ON vm_fs
BEGIN
  UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='count_' || NEW.kind;
  UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)+NEW.size AS TEXT) WHERE key='logical_bytes' AND NEW.kind='file';
END;
CREATE TRIGGER IF NOT EXISTS vm_cnt_fs_del AFTER DELETE ON vm_fs
BEGIN
  UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)-1 AS TEXT) WHERE key='count_' || OLD.kind;
  UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)-OLD.size AS TEXT) WHERE key='logical_bytes' AND OLD.kind='file';
END;
CREATE TRIGGER IF NOT EXISTS vm_cnt_fs_upd AFTER UPDATE OF size, kind ON vm_fs
BEGIN
  UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)-OLD.size AS TEXT) WHERE key='logical_bytes' AND OLD.kind='file';
  UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)+NEW.size AS TEXT) WHERE key='logical_bytes' AND NEW.kind='file';
  UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)-1 AS TEXT) WHERE key='count_' || OLD.kind AND OLD.kind IS NOT NEW.kind;
  UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='count_' || NEW.kind AND OLD.kind IS NOT NEW.kind;
END;
CREATE TRIGGER IF NOT EXISTS vm_cnt_blob_ins AFTER INSERT ON vm_blobs
BEGIN UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)+length(NEW.content) AS TEXT) WHERE key='blob_bytes'; END;
CREATE TRIGGER IF NOT EXISTS vm_cnt_blob_del AFTER DELETE ON vm_blobs
BEGIN UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)-length(OLD.content) AS TEXT) WHERE key='blob_bytes'; END;
CREATE TRIGGER IF NOT EXISTS vm_cnt_blob_upd AFTER UPDATE OF content ON vm_blobs
BEGIN UPDATE vm_meta SET value=CAST(CAST(value AS INTEGER)-length(OLD.content)+length(NEW.content) AS TEXT) WHERE key='blob_bytes'; END;
";

const VM_COUNTERS_INIT_SQL: &str = "
INSERT OR REPLACE INTO vm_meta(key,value) VALUES
  ('count_file',    CAST((SELECT count(*) FROM vm_fs WHERE kind='file') AS TEXT)),
  ('count_dir',     CAST((SELECT count(*) FROM vm_fs WHERE kind='dir') AS TEXT)),
  ('count_symlink', CAST((SELECT count(*) FROM vm_fs WHERE kind='symlink') AS TEXT)),
  ('logical_bytes', CAST((SELECT COALESCE(sum(size),0) FROM vm_fs WHERE kind='file') AS TEXT)),
  ('blob_bytes',    CAST((SELECT COALESCE(sum(length(content)),0) FROM vm_blobs) AS TEXT));
";

/// O(1) read of an aggregate counter maintained by triggers (see VM_COUNTER_TRIGGERS_SQL).
pub fn vm_counter(conn: &Connection, key: &str) -> Result<i64> {
    let v: Option<String> = conn.prepare_cached("SELECT value FROM vm_meta WHERE key=?1")?.query_row( params![key], |r| r.get(0)).optional()?;
    Ok(v.and_then(|x| x.parse().ok()).unwrap_or(0))
}

pub fn vm_open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = vm_apply_pragmas(&conn);
    // Brand-new DBs get the current schema directly; existing ones are upgraded in order.
    let fresh = conn.prepare_cached("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='vm_fs'")?.query_row( [], |r| r.get::<_,i64>(0))? == 0;
    if fresh { return Err(anyhow!("not a VM database: {}", path)); }
    vm_migrate(&conn)?;
    let cols: Vec<String> = {
        let mut st = conn.prepare("PRAGMA table_info(vm_fs)")?;
        let mut out=Vec::new();
        for r in st.query_map([], |r| r.get::<_,String>(1))? { out.push(r?); }
        out
    };
    let needs_compressed = !cols.contains(&"compressed".to_string());
    if needs_compressed {
        let _ = conn.prepare_cached("ALTER TABLE vm_fs ADD COLUMN compressed INTEGER DEFAULT 0")?.execute( []);
    }
    let _ = conn.prepare_cached("CREATE INDEX IF NOT EXISTS idx_vm_fs_path ON vm_fs(path)")?.execute( []);
    let _ = conn.prepare_cached("CREATE INDEX IF NOT EXISTS idx_vm_fs_hash ON vm_fs(hash)")?.execute( []);
    ensure_dict_table(&conn)?;
    let _ = ensure_disk_blocks_table(&conn);
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
    id INTEGER PRIMARY KEY,
    hash BLOB UNIQUE NOT NULL,
    content BLOB NOT NULL,
    compressed INTEGER NOT NULL DEFAULT 0,
    raw_size INTEGER NOT NULL,
    refcnt INTEGER NOT NULL DEFAULT 0
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
CREATE TABLE IF NOT EXISTS vm_disk_blocks(
  block_id INTEGER PRIMARY KEY,
  content BLOB NOT NULL,
  compressed INTEGER NOT NULL DEFAULT 0,
  raw_size INTEGER NOT NULL
);
{triggers}
{counters}
{counter_triggers}
"#, app=VM_APP_ID, ver=VM_USER_VERSION, triggers=VM_TRIGGERS_SQL, counters=VM_COUNTERS_INIT_SQL, counter_triggers=VM_COUNTER_TRIGGERS_SQL)
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
    conn.prepare_cached("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)")?.execute( params!["vm_version", "1"])?;
    conn.prepare_cached("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)")?.execute( params!["created_at", format!("{}", now_secs())])?;
    conn.prepare_cached("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES ('/','dir',493,?1,0,0)")?.execute( params![now_secs()])?;
    conn.prepare_cached("INSERT INTO vm_log(op, detail) VALUES ('init', ?1)")?.execute( params![path])?;
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
    conn.prepare_cached("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)")?.execute( params!["vm_version", "1"])?;
    conn.prepare_cached("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)")?.execute( params!["created_at", format!("{}", now_secs())])?;
    conn.prepare_cached("INSERT OR IGNORE INTO vm_fs(path,kind,mode,mtime,size,compressed) VALUES ('/','dir',493,?1,0,0)")?.execute( params![now_secs()])?;
    conn.prepare_cached("INSERT INTO vm_log(op, detail) VALUES ('init', ?1)")?.execute( params![path])?;
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

pub fn vm_get_dict(conn: &Connection) -> Result<Option<Vec<u8>>> {
    let dict: Option<Vec<u8>> = conn.prepare_cached("SELECT dict FROM vm_dict WHERE id=1")?.query_row( [], |r| r.get(0)).optional()?.flatten().map(|v| v);
    // vm_dict table may not exist yet, ignore error already handled via ensure_dict_table
    Ok(dict)
}
pub fn vm_set_dict(conn: &Connection, dict: &[u8], samples: usize) -> Result<()> {
    ensure_dict_table(conn)?;
    conn.prepare_cached("INSERT OR REPLACE INTO vm_dict(id, dict, samples, dict_size, created_at) VALUES (1, ?1, ?2, ?3, ?4)")?.execute( params![dict, samples as i64, dict.len() as i64, now_secs()])?;
    Ok(())
}
pub fn vm_train_dict(conn: &Connection, max_dict_size: usize) -> Result<Option<Vec<u8>>> {
    // collect raw samples: files with raw_size between 256 and 16384, up to 120 samples
    let mut samples: Vec<Vec<u8>> = Vec::new();
    let mut total_raw: usize = 0;
    // Sample without a full-table sort. vm_blobs is WITHOUT ROWID (clustered by hash), so
    // start at a pseudo-random key prefix and read forward, wrapping once. Seeded from time:
    // only affects which samples are chosen, not correctness.
    let seed = (now_secs() as u64).wrapping_mul(0x9E3779B97F4A7C15);
    let start: Vec<u8> = seed.to_be_bytes().iter().chain(seed.rotate_left(17).to_be_bytes().iter()).cloned().chain(std::iter::repeat(0u8)).take(32).collect();
    let mut rows: Vec<(Vec<u8>, i64)> = Vec::new();
    {
        let mut st = conn.prepare_cached("SELECT content, compressed FROM vm_blobs WHERE hash >= ?1 AND raw_size BETWEEN 256 AND 16383 LIMIT 150")?;
        for r in st.query_map(params![start], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,i64>(1)?)))? { rows.push(r?); }
        if rows.len() < 150 {
            let mut st = conn.prepare_cached("SELECT content, compressed FROM vm_blobs WHERE hash < ?1 AND raw_size BETWEEN 256 AND 16383 LIMIT ?2")?;
            for r in st.query_map(params![start, 150 - rows.len() as i64], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,i64>(1)?)))? { rows.push(r?); }
        }
    }
    for (data, comp) in rows.into_iter() {
        let raw = decompress_bytes_raw(&data, comp).unwrap_or(data);
        if raw.len() >= 256 && raw.len() < 16384 {
            total_raw += raw.len();
            samples.push(raw);
            if samples.len() >= 120 || total_raw > 500_000 { break; }
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
/// Decompressed size from the zstd frame header. Never truncates: a missing or implausible
/// header is an error, and the output buffer is sized to exactly the declared content size.
fn zstd_frame_size(data: &[u8]) -> Result<usize> {
    match zstd::zstd_safe::get_frame_content_size(data) {
        Ok(Some(n)) => usize::try_from(n).map_err(|_| anyhow!("zstd frame size {} too large", n)),
        Ok(None) => Err(anyhow!("zstd frame has no content size; refusing to guess")),
        Err(_) => Err(anyhow!("not a zstd frame")),
    }
}
pub fn decompress_bytes_zstd(data: &[u8]) -> Result<Vec<u8>> {
    let n = zstd_frame_size(data)?;
    let out = zstd::bulk::decompress(data, n)?;
    if out.len() != n { return Err(anyhow!("zstd short read: {} of {}", out.len(), n)); }
    Ok(out)
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
    let n = zstd_frame_size(data)?;
    let mut dec = zstd::bulk::Decompressor::with_dictionary(dict).map_err(|e| anyhow!("{}", e))?;
    let out = dec.decompress(data, n).map_err(|e| anyhow!("{}", e))?;
    if out.len() != n { return Err(anyhow!("zstd+dict short read: {} of {}", out.len(), n)); }
    Ok(out)
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
/// Decompressed bytes keyed by content hash; LRU hit = refcount bump, no copy.
pub fn decompress_bytes_cached(hash: &str, data: &[u8], compressed: i64) -> Arc<Vec<u8>> {
    if let Some(v) = decompress_cache().lock().unwrap().get(hash) { return v.clone(); }
    let raw = Arc::new(decompress_bytes_raw(data, compressed).unwrap_or_else(|_| data.to_vec()));
    decompress_cache().lock().unwrap().put(hash.to_string(), raw.clone());
    raw
}

/// Ensure a blob row exists for `key` (refcnt starts at 0; triggers count references).
fn blob_ensure(conn: &Connection, key: &BlobKey, data: &[u8]) -> Result<i64> {
    if let Some(flag) = conn.prepare_cached("SELECT compressed FROM vm_blobs WHERE hash=?1")?.query_row( params![key.as_slice()], |r| r.get::<_,i64>(0)).optional()? {
        return Ok(flag);
    }
    let (body, flag) = match compress_bytes_with_conn(conn, data) {
        Some((c, f)) => (c, f),
        None => (data.to_vec(), 0),
    };
    conn.prepare_cached("INSERT INTO vm_blobs(hash, content, compressed, raw_size, refcnt) VALUES (?1,?2,?3,?4,0)")?.execute(
        params![key.as_slice(), body, flag, data.len() as i64])?;
    let cache_p = cache_path_for_hash(&key_hex(key));
    if !cache_p.exists() {
        if let Some(par)=cache_p.parent(){ let _ = std::fs::create_dir_all(par); }
        let _ = std::fs::write(&cache_p, data);
    }
    Ok(flag)
}

/// Upsert a regular-file row. Hash-change refcounting is done by vm_fs_hash_upd.
fn fs_put_file(conn: &Connection, vm_p: &str, mode: i64, mtime: i64, data: &[u8]) -> Result<()> {
    let hash = blob_key(data);
    let flag = blob_ensure(conn, &hash, data)?;
    conn.execute(
        "INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,content,compressed,link_target) VALUES (?1,'file',?2,?3,?4,?5,NULL,?6,NULL)
         ON CONFLICT(path) DO UPDATE SET kind='file', mode=excluded.mode, mtime=excluded.mtime, size=excluded.size,
            hash=excluded.hash, content=NULL, compressed=excluded.compressed, link_target=NULL",
        params![vm_p, mode, mtime, data.len() as i64, hash.as_slice(), flag])?;
    Ok(())
}

/// Upsert a non-file row (dir/symlink). Any previous hash is released by the UPDATE trigger.
fn fs_put_node(conn: &Connection, vm_p: &str, kind: &str, mode: i64, mtime: i64, link: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,content,compressed,link_target) VALUES (?1,?2,?3,?4,0,NULL,NULL,0,?5)
         ON CONFLICT(path) DO UPDATE SET kind=excluded.kind, mode=excluded.mode, mtime=excluded.mtime, size=0,
            hash=NULL, content=NULL, compressed=0, link_target=excluded.link_target",
        params![vm_p, kind, mode, mtime, link])?;
    Ok(())
}

pub fn vm_add_bytes(conn: &Connection, vm_path: &str, data: &[u8], mode: i64, mtime: i64) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    fs_put_file(conn, &vm_p, mode, mtime, data)
}

pub fn vm_add_symlink(conn: &Connection, vm_path: &str, target: &str) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    fs_put_node(conn, &vm_p, "symlink", 493, now_secs(), Some(target))
}
pub fn vm_add_dir(conn: &Connection, vm_path: &str, mode: i64, mtime: i64) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    fs_put_node(conn, &vm_p, "dir", mode, mtime, None)
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
        fs_put_node(conn, &vm_p, "dir", mode, mtime, None)?;
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
        fs_put_node(conn, &vm_p, "symlink", 0o777, mtime, Some(&target))?;
        return Ok(());
    }
    let data = std::fs::read(hp)?;
    vm_add_bytes(conn, &vm_p, &data, mode, mtime)?;
    conn.prepare_cached("INSERT INTO vm_log(op, path, detail) VALUES ('add', ?1, ?2)")?.execute( params![vm_p, host_path])?;
    Ok(())
}
pub fn fx_hash_u64_static(data: &[u8]) -> String { fx_hash_u64(data) }
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
    let cnt: i64 = conn.prepare_cached("SELECT count(*) FROM vm_fs WHERE path=?1")?.query_row( params![p], |r| r.get(0))?;
    if cnt>0 {
        let row: (String,String,i64,i64,String) = conn.prepare_cached("SELECT path,kind,mode,size,COALESCE(NULLIF(lower(hex(hash)),''),'') FROM vm_fs WHERE path=?1")?.query_row( params![p], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?, r.get::<_,String>(4)?)))?;
        return Ok(vec![row]);
    }
    let like = if p == "/" { "/%".to_string() } else { format!("{}/%", p) };
    let mut st = conn.prepare("SELECT path,kind,mode,size,COALESCE(NULLIF(lower(hex(hash)),''),'') FROM vm_fs WHERE path LIKE ?1 AND path != ?2 ORDER BY path")?;
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
/// Chunk size for file content. Each chunk is an independent blob (compressed, deduplicated).
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Unified file-range reader. Every content read goes through here so the storage layout
/// (single blob today, chunks after schema v5) is decided in one place.
/// Returns bytes in [off, off+len) clamped to the file size.
pub fn read_range(conn: &Connection, vm_path: &str, off: u64, len: usize) -> Result<Vec<u8>> {
    let (real, kind, content, _link, _mode) = vm_resolve(conn, vm_path)?;
    if kind == "dir" { return Err(anyhow!("is a directory: {}", vm_path)); }
    let data = content.ok_or_else(|| anyhow!("no content for {} (resolved {})", vm_path, real))?;
    let start = (off as usize).min(data.len());
    let end = start.saturating_add(len).min(data.len());
    Ok(data[start..end].to_vec())
}
pub fn vm_cat(conn: &Connection, vm_path: &str) -> Result<Vec<u8>> {
    let (real, kind, content, _link, _mode) = vm_resolve(conn, vm_path)?;
    if kind == "dir" { return Err(anyhow!("is a directory: {}", vm_path)); }
    match content { Some(d) => Ok(d), None => Err(anyhow!("no content for {} (resolved {})", vm_path, real)) }
}
pub fn vm_stat(conn: &Connection, vm_path: &str) -> Result<(String,String,i64,i64,i64,String)> {
    let p = normalize_vm_path(vm_path);
    let row: Option<(String,String,i64,i64,i64,Option<String>,Option<Vec<u8>>,Option<String>)> = conn.prepare_cached("SELECT path,kind,mode,size,mtime,link_target,content,NULLIF(lower(hex(hash)),'') FROM vm_fs WHERE path=?1")?.query_row( params![p], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))).optional()?;
    let (path,kind,mode,size,mtime,link,content,hash_opt) = match row { Some(x)=>x, None=> return Err(anyhow!("not found: {}", p)) };
    if kind == "symlink" {
        let target = link.clone().unwrap_or_default();
        if let Ok((_rp, _rk, rc, _rl, _rm)) = vm_resolve(conn, &p) {
            if let Some(d)=rc { return Ok((path,kind,mode,d.len() as i64,mtime, format!("-> {} {}", target, fx_hash_u64(&d)))); }
        }
        return Ok((path,kind,mode,size,mtime, format!("-> {}", target)));
    }
    if kind == "dir" { return Ok((path,kind,mode,0,mtime,String::new())); }
    // for file, hash is stored; if content present (legacy) use it else use blob
    let hash = hash_opt.unwrap_or_else(|| {
        if let Some(c)=content { fx_hash_u64(&c) } else { String::new() }
    });
    Ok((path,kind,mode,size,mtime,hash))
}
pub fn vm_resolve(conn: &Connection, vm_path: &str) -> Result<(String, String, Option<Vec<u8>>, Option<String>, i64)> {
    let mut cur = normalize_vm_path(vm_path);
    let mut visited = std::collections::HashSet::new();
    for _ in 0..40 {
        if !visited.insert(cur.clone()) { return Err(anyhow!("symlink loop at {}", cur)); }
        let row: Option<(String, Option<Vec<u8>>, Option<String>, i64, Option<Vec<u8>>, Option<i64>)> = conn.prepare_cached("SELECT kind, content, link_target, mode, hash, compressed FROM vm_fs WHERE path=?1")?.query_row( params![cur], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?;
        let (kind, content_opt, link, mode, hash_opt, compressed) = match row { Some(x)=>x, None=> return Err(anyhow!("not found in VM: {}", cur)) };
        if kind != "symlink" {
            let cflag = compressed.unwrap_or(0);
            let content: Option<Vec<u8>> = match (hash_opt, content_opt) {
                // content-addressed: key cache by stored hash, decompress only on miss
                (Some(h), inline) => {
                    let hk = key_hex(&h.as_slice().try_into().map_err(|_| anyhow!("corrupt hash in {}", cur))?);
                    let blob: Option<(Vec<u8>, i64)> = conn.prepare_cached("SELECT content, compressed FROM vm_blobs WHERE hash=?1")?.query_row( params![h], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    match (blob, inline) {
                        (Some((b,c)), _) => Some(decompress_bytes_cached(&hk, &b, c).to_vec()),
                        (None, Some(b)) => Some(decompress_bytes_cached(&hk, &b, cflag).to_vec()),
                        (None, None) => None,
                    }
                },
                // legacy rows: inline content, no hash column -> key by decompressed hash
                (None, Some(b)) if !b.is_empty() => {
                    let raw = decompress_bytes_raw(&b, cflag).unwrap_or_else(|_| b.clone());
                    Some(raw)
                },
                _ => None,
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
            let _ = std::fs::write(&cache_p, data.as_slice());
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
    conn.prepare_cached("INSERT INTO vm_log(op, detail) VALUES ('import_tar', ?1)")?.execute( params![tar_path])?;
    Ok(n)
}
thread_local! {
    // one read connection per rayon worker; rusqlite::Connection is not Sync
    static WORKER_CONN: std::cell::RefCell<Option<Connection>> = const { std::cell::RefCell::new(None) };
}

/// Load one file's decompressed bytes inside a worker. Falls back to legacy inline content.
fn load_task_bytes<T: TaskLike>(db_path: Option<&str>, t: &T) -> Option<Vec<u8>> {
    let path = db_path?;
    WORKER_CONN.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() { *slot = Connection::open(path).ok(); }
        let c = slot.as_ref()?;
        if let Some(h) = t.hash_ref() {
            let key = hex_to_key(h)?;
            let row: Option<(Vec<u8>, i64)> = c.query_row("SELECT content, compressed FROM vm_blobs WHERE hash=?1", params![key.as_slice()], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok()?;
            if let Some((b, comp)) = row { return Some(decompress_bytes_cached(h, &b, comp).to_vec()); }
        }
        let inline: Option<(Option<Vec<u8>>, Option<i64>)> = c.query_row("SELECT content, compressed FROM vm_fs WHERE path=?1", params![t.path_ref()], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok()?;
        match inline {
            Some((Some(b), comp)) => Some(decompress_bytes_raw(&b, comp.unwrap_or(0)).unwrap_or(b)),
            _ => None,
        }
    })
}

trait TaskLike { fn hash_ref(&self) -> Option<&str>; fn path_ref(&self) -> &str; }

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
    // Metadata only: blobs are loaded per file inside the worker, so memory stays O(one file).
    struct Task { path: String, mode: i64, hash: Option<String>, size: i64, mtime: i64 }  // hash: hex text
    impl TaskLike for Task {
        fn hash_ref(&self) -> Option<&str> { self.hash.as_deref() }
        fn path_ref(&self) -> &str { &self.path }
    }
    let tasks: Vec<Task> = {
        let mut st = conn.prepare("SELECT path, mode, NULLIF(lower(hex(hash)),''), size, mtime FROM vm_fs WHERE kind='file'")?;
        let rows = st.query_map([], |r| Ok(Task{ path: r.get(0)?, mode: r.get(1)?, hash: r.get(2)?, size: r.get(3)?, mtime: r.get(4)? }))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let db_path = conn.path().map(|p| p.to_string());
    let dest_buf = dest.to_path_buf();
    let written = std::sync::atomic::AtomicUsize::new(0);
    tasks.par_iter().for_each(|t| {
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
        let Some(data) = load_task_bytes(db_path.as_deref(), t) else { return; };
        if let Some(par)=host.parent(){ let _ = std::fs::create_dir_all(par); }
        let key = t.hash.clone().unwrap_or_else(|| fx_hash_u64(&data));
        let cache_p = cache_path_for_hash(&key);
        let done = cache_p.exists() && try_hardlink_or_copy(&cache_p, &host).is_ok();
        if !done {
            let _ = std::fs::write(&host, &data);
            if !cache_p.exists() {
                if let Some(par)=cache_p.parent(){ let _ = std::fs::create_dir_all(par); }
                let _ = std::fs::write(&cache_p, &data);
            }
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
        let mut st = conn.prepare("SELECT path, kind, NULLIF(lower(hex(hash)),''), link_target, mode, size FROM vm_fs")?;
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
                    let _ = fs_put_node(conn, vm_p, "symlink", *mode, now_secs(), Some(&target));
                    created+=1;
                } else if kind == "dir" {
                    let _ = fs_put_node(conn, vm_p, "dir", *mode, now_secs(), None);
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
                        let _ = fs_put_node(conn, vm_p, "symlink", *mode, now_secs(), Some(&target));
                    } else if kind == "dir" {
                        let _ = fs_put_node(conn, vm_p, "dir", *mode, now_secs(), None);
                    } else {
                        let hash = fx_hash_u64(data);
                        let need = db_hash.as_deref()!=Some(&hash) || db_mode!=mode || db_size!=&(data.len() as i64);
                        if need {
                                let _ = fs_put_file(conn, vm_p, *mode, now_secs(), data);
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
                        let _ = conn.prepare_cached("UPDATE vm_fs SET mode=?2, link_target=?3, mtime=?4 WHERE path=?1")?.execute( params![vm_p, *mode, target, now_secs()]);
                        updated+=1;
                    }
                } else if kind == "dir" {
                    if db_mode != mode {
                        let _ = conn.prepare_cached("UPDATE vm_fs SET mode=?2, mtime=?3 WHERE path=?1")?.execute( params![vm_p, *mode, now_secs()]);
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
        // DELETE fires vm_fs_hash_del, which releases the blob reference.
        // prefix match via substr (LIKE treats _ and % as wildcards)
        let _ = conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1 OR substr(path,1,?2)=?3")?.execute( params![p.clone(), p.len() as i64 + 1, format!("{}/", p)]);
        deleted+=1;
    }
    conn.execute_batch("COMMIT;")?;
    let _ = conn.prepare_cached("INSERT INTO vm_log(op, detail) VALUES ('sync', ?1)")?.execute( params![format!("host:{} created:{} updated:{} deleted:{}", host_root.display(), created, updated, deleted)]);
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
        let hash = blob_key(&raw);
        let sz_before: Option<i64> = conn.prepare_cached("SELECT length(content) FROM vm_blobs WHERE hash=?1")?.query_row( params![hash.as_slice()], |r| r.get(0)).optional()?;
        match sz_before {
            Some(sz) => after += sz,
            None => {
                // blob row missing (legacy hash): create it; refcount follows from vm_fs rows via triggers
                blob_ensure(conn, &hash, &raw)?;
                after += raw.len() as i64;
            }
        }
        // move inline content onto the blob: hash column now references it, content becomes NULL.
        // The UPDATE trigger adds the reference (old hash was NULL for legacy rows).
        let blob_flag: i64 = conn.prepare_cached("SELECT compressed FROM vm_blobs WHERE hash=?1")?.query_row( params![hash.as_slice()], |r| r.get(0)).unwrap_or(2);
        conn.prepare_cached("UPDATE vm_fs SET hash=?2, content=NULL, compressed=?3, size=?4 WHERE path=?1")?.execute( params![path, hash.as_slice(), blob_flag, raw.len() as i64])?;
        n+=1;
    }
    // also recompress blobs that are gz
    let mut gz_blobs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    {
        let mut st = conn.prepare("SELECT hash, content FROM vm_blobs WHERE compressed=1")?;
        for r in st.query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,Vec<u8>>(1)?)))? { if let Ok(x)=r { gz_blobs.push(x); } }
    }
    for (hash, gz) in gz_blobs {
        let raw = decompress_bytes_raw(&gz, 1).unwrap_or(gz.clone());
        before += gz.len() as i64;
        if let Some((z, flag)) = compress_bytes_with_conn(conn, &raw) {
            after += z.len() as i64;
            let _ = conn.prepare_cached("UPDATE vm_blobs SET content=?2, compressed=?3 WHERE hash=?1")?.execute( params![hash, z, flag]);
            let _ = conn.prepare_cached("UPDATE vm_fs SET compressed=?2 WHERE hash=?1")?.execute( params![hash.clone(), flag]);
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
            let _ = conn.prepare_cached("UPDATE vm_blobs SET content=?2, compressed=?3 WHERE hash=?1")?.execute( params![hash.clone(), z, flag]);
            let _ = conn.prepare_cached("UPDATE vm_fs SET compressed=?2 WHERE hash=?1")?.execute( params![hash, flag]);
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
                let _ = conn.prepare_cached("UPDATE vm_blobs SET content=?2 WHERE hash=?1")?.execute( params![hash.clone(), c19]);
                n+=1;
            }
        }
    }
    conn.execute_batch("COMMIT;")?;
    Ok((n, before, after))
}
pub fn vm_gc(conn: &Connection) -> Result<(i64,i64)> {
    let before: i64 = conn.prepare_cached("PRAGMA page_count")?.query_row( [], |r| r.get(0))?;
    let ps_before: i64 = conn.prepare_cached("PRAGMA page_size")?.query_row( [], |r| r.get(0))?;
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
    let after: i64 = conn.prepare_cached("PRAGMA page_count")?.query_row( [], |r| r.get(0))?;
    let ps: i64 = conn.prepare_cached("PRAGMA page_size")?.query_row( [], |r| r.get(0))?;
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
    let page_size: i64 = conn.prepare_cached("PRAGMA page_size")?.query_row( [], |r| r.get(0))?;
    let page_count: i64 = conn.prepare_cached("PRAGMA page_count")?.query_row( [], |r| r.get(0))?;
    let freelist: i64 = conn.prepare_cached("PRAGMA freelist_count")?.query_row( [], |r| r.get(0))?;
    let journal: String = conn.prepare_cached("PRAGMA journal_mode")?.query_row( [], |r| r.get(0))?;
    let integrity: String = conn.prepare_cached("PRAGMA integrity_check")?.query_row( [], |r| r.get(0))?;
    // aggregates come from vm_meta counters (trigger-maintained, O(1))
    let dirs = vm_counter(conn, "count_dir")?;
    let syms = vm_counter(conn, "count_symlink")?;
    let fcnt = vm_counter(conn, "count_file")?;
    let files = dirs + syms + fcnt;
    let logical = vm_counter(conn, "logical_bytes")?;
    let blob_storage = vm_counter(conn, "blob_bytes")?;
    let blob_cnt: i64 = conn.prepare_cached("SELECT count(*) FROM vm_blobs")?.query_row( [], |r| r.get(0)).unwrap_or(0);
    let comp_cnt: i64 = conn.prepare_cached("SELECT count(*) FROM vm_blobs WHERE compressed!=0")?.query_row( [], |r| r.get(0)).unwrap_or(0);
    let snaps: i64 = conn.prepare_cached("SELECT count(*) FROM vm_snapshots")?.query_row( [], |r| r.get(0))?;
    let mmap: i64 = conn.prepare_cached("PRAGMA mmap_size")?.query_row( [], |r| r.get(0))?;
    let cache_size: i64 = conn.prepare_cached("PRAGMA cache_size")?.query_row( [], |r| r.get(0))?;
    let auto_vac: i64 = conn.prepare_cached("PRAGMA auto_vacuum")?.query_row( [], |r| r.get(0)).unwrap_or(0);
    let (cache_dir, cache_files, cache_bytes) = vm_cache_info();
    let ratio = if logical>0 { 100.0 * blob_storage as f64 / logical as f64 } else { 0.0 };
    let disk_line = vm_disk_info(conn).unwrap_or_else(|_| "disk: none".to_string());
    Ok(format!(
        "integrity={} page_size={} page_count={} freelist={} auto_vacuum={} journal={} mmap={} cache_size={}\nfiles={} (dir={} file={} symlink={}) logical={} blob_storage={} ({:.1}%) blobs={} compressed={} snaps={}\n{}\ncache: dir={} files={} bytes={}",
        integrity, page_size, page_count, freelist, auto_vac, journal, mmap, cache_size,
        files, dirs, fcnt, syms, logical, blob_storage, ratio, blob_cnt, comp_cnt, snaps,
        disk_line,
        cache_dir.display(), cache_files, cache_bytes
    ))
}
pub fn vm_diff(conn: &Connection, snap_a: &str, snap_b: &str) -> Result<String> {
    // snapshots table only has page stats, not file diff; we diff vm_log if available, else file count diff
    let a: Option<(i64,i64,String)> = conn.prepare_cached("SELECT page_count, bytes, coalesce(note,'') FROM vm_snapshots WHERE name=?1")?.query_row( params![snap_a], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let b: Option<(i64,i64,String)> = conn.prepare_cached("SELECT page_count, bytes, coalesce(note,'') FROM vm_snapshots WHERE name=?1")?.query_row( params![snap_b], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
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
    let page_size: i64 = conn.prepare_cached("PRAGMA page_size")?.query_row( [], |r| r.get(0))?;
    let page_count: i64 = conn.prepare_cached("PRAGMA page_count")?.query_row( [], |r| r.get(0))?;
    let bytes = page_size * page_count;
    conn.prepare_cached("INSERT INTO vm_snapshots(name, page_count, page_size, bytes, note) VALUES (?1,?2,?3,?4,?5)")?.execute( params![name, page_count, page_size, bytes, note])?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    conn.prepare_cached("INSERT INTO vm_log(op, detail) VALUES ('snapshot', ?1)")?.execute( params![name])?;
    Ok(())
}
pub fn vm_list_snapshots(conn: &Connection) -> Result<Vec<(i64,String,i64,i64,i64,String)>> {
    let mut st = conn.prepare("SELECT id, name, created_at, page_count, bytes, coalesce(note,'') FROM vm_snapshots ORDER BY id")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
    let mut out=Vec::new(); for r in rows { out.push(r?); } Ok(out)
}
pub fn vm_mem_insert(conn: &Connection, addr: i64, size: i64, prot: i64, content: &[u8]) -> Result<()> {
    conn.prepare_cached("INSERT INTO vm_mem(addr,size,prot,content) VALUES (?1,?2,?3,?4)")?.execute( params![addr,size,prot,content])?;
    Ok(())
}
pub fn vm_mem_list(conn: &Connection) -> Result<Vec<(i64,i64,i64,i64)>> {
    let mut st = conn.prepare("SELECT id, addr, size, prot FROM vm_mem ORDER BY addr")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let mut out=Vec::new(); for r in rows{ out.push(r?); } Ok(out)
}
pub fn vm_mem_clear(conn: &Connection) -> Result<()> {
    conn.prepare_cached("DELETE FROM vm_mem")?.execute( [])?;
    Ok(())
}

pub fn vm_mem_trace(db_path: &str, prog: &str, rest: Vec<String>) -> Result<()> {
    use std::process::Command;
    #[cfg(target_os = "linux")]
    {
        let db_path_owned = db_path.to_string();
        let trace_path = std::env::temp_dir().join(format!("self-vm-trace-{}", std::process::id())).to_string_lossy().to_string();
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
    if let Ok(conn) = vm_open(db_path) {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;");
        let _ = conn.execute_batch("PRAGMA incremental_vacuum;");
    }
    Ok(())
}
/// Quick check uses counters; `deep` recomputes every counter and refcount from raw rows
/// and reports any drift (use after crashes or manual SQL edits).
pub fn vm_verify(conn: &Connection, deep: bool) -> Result<String> {
    let page_size: i64 = conn.prepare_cached("PRAGMA page_size")?.query_row( [], |r| r.get(0))?;
    let page_count: i64 = conn.prepare_cached("PRAGMA page_count")?.query_row( [], |r| r.get(0))?;
    let freelist: i64 = conn.prepare_cached("PRAGMA freelist_count")?.query_row( [], |r| r.get(0))?;
    let integrity: String = conn.prepare_cached("PRAGMA integrity_check")?.query_row( [], |r| r.get(0))?;
    let files = vm_counter(conn, "count_file")? + vm_counter(conn, "count_dir")? + vm_counter(conn, "count_symlink")?;
    let bytes = vm_counter(conn, "logical_bytes")?;
    let mut out = format!("integrity={} page_size={} page_count={} freelist={} files={} bytes={}", integrity, page_size, page_count, freelist, files, bytes);
    if deep {
        let mut drift = Vec::new();
        let real_files: i64 = conn.prepare_cached("SELECT count(*) FROM vm_fs")?.query_row( [], |r| r.get(0))?;
        let real_bytes: i64 = conn.prepare_cached("SELECT COALESCE(sum(size),0) FROM vm_fs WHERE kind='file'")?.query_row( [], |r| r.get(0))?;
        let real_blob: i64 = conn.prepare_cached("SELECT COALESCE(sum(length(content)),0) FROM vm_blobs")?.query_row( [], |r| r.get(0))?;
        if real_files != files { drift.push(format!("files {}!={}", files, real_files)); }
        if real_bytes != bytes { drift.push(format!("logical {}!={}", bytes, real_bytes)); }
        if real_blob != vm_counter(conn, "blob_bytes")? { drift.push(format!("blob_bytes {}!={}", vm_counter(conn, "blob_bytes")?, real_blob)); }
        let bad_rc: i64 = conn.prepare_cached("SELECT count(*) FROM vm_blobs b WHERE b.refcnt != (SELECT count(*) FROM vm_fs f WHERE f.hash=b.hash)")?.query_row( [], |r| r.get(0))?;
        if bad_rc > 0 { drift.push(format!("{} blobs with bad refcnt", bad_rc)); }
        let orphan: i64 = conn.prepare_cached("SELECT count(*) FROM vm_fs f WHERE f.hash IS NOT NULL AND NOT EXISTS (SELECT 1 FROM vm_blobs b WHERE b.hash=f.hash)")?.query_row( [], |r| r.get(0))?;
        if orphan > 0 { drift.push(format!("{} files reference missing blobs", orphan)); }
        out.push_str(if drift.is_empty() { " deep=ok" } else { " deep=DRIFT" });
        for d in drift { out.push_str(&format!("\n  {}", d)); }
    }
    Ok(out)
}

pub const DISK_BLOCK_SIZE: i64 = 4096;
pub fn vm_disk_init(conn: &Connection, size_bytes: i64) -> Result<()> {
    ensure_disk_blocks_table(conn)?;
    if size_bytes % DISK_BLOCK_SIZE != 0 { return Err(anyhow!("size must be multiple of {}", DISK_BLOCK_SIZE)); }
    conn.prepare_cached("INSERT OR IGNORE INTO vm_meta VALUES (?1,?2)")?.execute( params!["disk_size", size_bytes.to_string()])?;
    conn.prepare_cached("UPDATE vm_meta SET value=?1 WHERE key='disk_size'")?.execute( params![size_bytes.to_string()])?;
    let bs: i64 = DISK_BLOCK_SIZE;
    if let Ok(v) = conn.prepare_cached("SELECT value FROM vm_meta WHERE key='disk_block_size'")?.query_row( [], |r| r.get::<_,String>(0)) {
        if v != bs.to_string() { conn.prepare_cached("UPDATE vm_meta SET value=?1 WHERE key='disk_block_size'")?.execute( params![bs.to_string()])?; }
    } else {
        conn.prepare_cached("INSERT INTO vm_meta VALUES (?1,?2)")?.execute( params!["disk_block_size", bs.to_string()])?;
    }
    Ok(())
}
pub fn vm_disk_size(conn: &Connection) -> Result<i64> {
    ensure_disk_blocks_table(conn)?;
    let v: Option<String> = conn.prepare_cached("SELECT value FROM vm_meta WHERE key='disk_size'")?.query_row( [], |r| r.get(0)).optional()?;
    if let Some(s) = v { if let Ok(n) = s.parse::<i64>() { return Ok(n); } }
    Ok(0)
}
fn is_all_zero(b: &[u8]) -> bool { b.iter().all(|&x| x==0) }
pub fn vm_disk_import_raw(conn: &Connection, raw_path: &str, disk_size: i64) -> Result<(i64,i64)> {
    let existing = vm_disk_size(conn).unwrap_or(0);
    let target = if existing>0 && existing>disk_size { existing } else { disk_size };
    vm_disk_init(conn, target)?;
    let mut f = std::fs::File::open(raw_path)?;
    let mut buf = vec![0u8; DISK_BLOCK_SIZE as usize];
    let mut bid: i64 = 0;
    let mut nonzero: i64 = 0;
    let mut total: i64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 { break; }
        if n < buf.len() { buf[n..].fill(0); }
        let slice = &buf[..];
        if !is_all_zero(slice) {
            let comp_opt = compress_bytes(slice);
            let (content, compressed, raw_size): (Vec<u8>, i64, i64) = if let Some(c)=comp_opt { (c, 2, DISK_BLOCK_SIZE) } else { (slice.to_vec(), 0, DISK_BLOCK_SIZE) };
            conn.prepare_cached("INSERT OR REPLACE INTO vm_disk_blocks(block_id, content, compressed, raw_size) VALUES (?1,?2,?3,?4)")?.execute( params![bid, content, compressed, raw_size])?;
            nonzero += 1;
        } else {
            conn.prepare_cached("DELETE FROM vm_disk_blocks WHERE block_id=?1")?.execute( params![bid])?;
        }
        total +=1;
        bid+=1;
        if total%1024==0 { let _=conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);"); }
        if bid*DISK_BLOCK_SIZE >= disk_size { break; }
    }
    for id in bid..(disk_size/DISK_BLOCK_SIZE) {
        conn.prepare_cached("DELETE FROM vm_disk_blocks WHERE block_id=?1")?.execute( params![id])?;
    }
    Ok((total, nonzero))
}
pub fn vm_disk_export_raw(conn: &Connection, raw_path: &str) -> Result<(i64,i64)> {
    ensure_disk_blocks_table(conn)?;
    let disk = vm_disk_size(conn)?;
    let mut f = std::fs::File::create(raw_path)?;
    f.set_len(disk as u64)?;
    let bs = DISK_BLOCK_SIZE as usize;
    let blocks = disk/DISK_BLOCK_SIZE;
    if blocks > 10_000_000 {
        return Err(anyhow!("refusing to export huge disk {} blocks: use NBD or limit size", blocks));
    }
    // sparse: iterate only stored blocks, so 20G (5.2M blocks) with <1% stored is fine
    let mut nonzero=0i64;
    use std::io::{Seek, SeekFrom};
    let mut stmt = conn.prepare("SELECT block_id, content, compressed FROM vm_disk_blocks ORDER BY block_id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?)))?;
    for row in rows {
        let (bid, data, comp) = row?;
        let bytes: Vec<u8> = if comp==2 { zstd::bulk::decompress(&data, bs).map_err(|e| anyhow!("zstd decompress export {}: {}", bid, e))? } else if comp==1 { let mut d=flate2::read::GzDecoder::new(&data[..]); let mut o=Vec::new(); d.read_to_end(&mut o)?; o } else { data };
        let mut b = bytes; if b.len()<bs { b.resize(bs, 0); }
        if is_all_zero(&b) { continue; }
        f.seek(SeekFrom::Start((bid*DISK_BLOCK_SIZE) as u64))?;
        f.write_all(&b)?;
        nonzero+=1;
    }
    Ok((blocks, nonzero))
}
pub fn vm_disk_info(conn: &Connection) -> Result<String> {
    ensure_disk_blocks_table(conn)?;
    let disk = vm_disk_size(conn)?;
    let blocks = disk/DISK_BLOCK_SIZE;
    let stored: i64 = conn.prepare_cached("SELECT count(*) FROM vm_disk_blocks")?.query_row( [], |r| r.get(0))?;
    let raw_storage: Option<i64> = conn.prepare_cached("SELECT sum(length(content)) FROM vm_disk_blocks")?.query_row( [], |r| r.get(0))?;
    let raw_storage = raw_storage.unwrap_or(0);
    let logical = stored*DISK_BLOCK_SIZE;
    Ok(format!("disk size={} bytes ({:.2}G) blocks={} stored_blocks={} logical_stored={} sparse_hole={} bytes compressed_storage={} bytes ratio={:.1}% page_size={} page_count={}",
        disk, disk as f64/1024.0/1024.0/1024.0, blocks, stored, logical, disk-logical, raw_storage, if disk>0 {100.0*raw_storage as f64/disk as f64} else {0.0},
        conn.prepare_cached("PRAGMA page_size")?.query_row( [], |r| r.get::<_,i64>(0)).unwrap_or(0),
        conn.prepare_cached("PRAGMA page_count")?.query_row( [], |r| r.get::<_,i64>(0)).unwrap_or(0)))
}

