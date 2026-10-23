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
///   7 = vm_fs.mtime_ns: host nanosecond mtime used only to detect changes during sync (0 = unknown)
///   6 = zstd dictionaries are append-only rows in vm_dicts; every flag-3 blob records its
///       dictionary in vm_blobs.dict_id. Retraining no longer makes older blocks unreadable.
///   5 = file content is split into CHUNK_SIZE chunks listed in vm_file_chunks; a file's
///       refcount on vm_blobs comes from its chunk rows. vm_fs.hash = whole-file sha256
///       (informational, no refcounting). Legacy single-blob files keep working via vm_fs.hash.
pub const VM_USER_VERSION: u32 = 7;

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
                4 => migrate_4_to_5(conn)?,
                5 => migrate_5_to_6(conn)?,
                6 => migrate_6_to_7(conn)?,
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
    conn.execute_batch(TRIGGERS_V2_SQL)?;
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
    // schema 4 has no vm_file_chunks yet and no refcount triggers: 4->5 installs them.
    conn.execute_batch(VM_COUNTER_TRIGGERS_SQL)?;
    conn.execute_batch(VM_COUNTERS_INIT_SQL)?;
    Ok(())
}

/// 4 -> 5: add vm_file_chunks; existing single-blob files become one-row chunk lists
/// (chunk = whole file). Keeping one chunk per file preserves bytes exactly; re-chunking
/// at CHUNK_SIZE is a separate, optional rewrite (see vm-rechunk).
fn migrate_4_to_5(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        DROP TRIGGER IF EXISTS vm_fs_hash_ins; DROP TRIGGER IF EXISTS vm_fs_hash_del; DROP TRIGGER IF EXISTS vm_fs_hash_upd;
        CREATE TABLE IF NOT EXISTS vm_file_chunks(
            path TEXT NOT NULL,
            idx INTEGER NOT NULL,
            hash BLOB NOT NULL,
            PRIMARY KEY(path, idx)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS idx_vm_file_chunks_hash ON vm_file_chunks(hash);
        -- every regular file currently referenced by vm_fs.hash becomes a single chunk
        INSERT INTO vm_file_chunks(path, idx, hash)
          SELECT path, 0, hash FROM vm_fs WHERE kind='file' AND hash IS NOT NULL;
        -- recount from chunk rows (old vm_fs-driven counts are discarded)
        UPDATE vm_blobs SET refcnt = (SELECT count(*) FROM vm_file_chunks c WHERE c.hash = vm_blobs.hash);
        DELETE FROM vm_blobs WHERE refcnt = 0;
    ")?;
    conn.execute_batch(VM_TRIGGERS_SQL)?;
    Ok(())
}

/// 6 -> 7: add the change-detection nanosecond column. Existing rows get 0 (unknown), so they
/// fall back to the second-granularity comparison until they are next written.
fn migrate_6_to_7(conn: &Connection) -> Result<()> {
    let has: i64 = conn.query_row("SELECT count(*) FROM pragma_table_info('vm_fs') WHERE name='mtime_ns'", [], |r| r.get(0))?;
    if has == 0 {
        conn.execute_batch("ALTER TABLE vm_fs ADD COLUMN mtime_ns INTEGER NOT NULL DEFAULT 0;")?;
    }
    Ok(())
}

/// 5 -> 6: dictionaries become append-only (vm_dicts) and blobs record their dictionary id.
/// The single legacy dictionary (vm_dict id=1) becomes vm_dicts row 1; every existing flag-3
/// blob was written with it, so they get dict_id=1.
fn migrate_5_to_6(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS vm_dicts(
            id INTEGER PRIMARY KEY,
            dict BLOB NOT NULL,
            samples INTEGER NOT NULL,
            dict_size INTEGER NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        );
        INSERT INTO vm_dicts(id, dict, samples, dict_size, created_at)
          SELECT 1, dict, samples, dict_size, created_at FROM vm_dict WHERE id=1;
    ")?;
    let has_col: i64 = conn.query_row("SELECT count(*) FROM pragma_table_info('vm_blobs') WHERE name='dict_id'", [], |r| r.get(0))?;
    if has_col == 0 {
        conn.execute_batch("ALTER TABLE vm_blobs ADD COLUMN dict_id INTEGER NOT NULL DEFAULT 0;")?;
    }
    conn.execute_batch("UPDATE vm_blobs SET dict_id=1 WHERE compressed=3 AND dict_id=0;")?;
    Ok(())
}

/// 2 -> 3: O(1) aggregate counters maintained by triggers, initialised from current data.
fn migrate_2_to_3(conn: &Connection) -> Result<()> {
    conn.execute_batch(VM_COUNTERS_INIT_SQL)?;
    conn.execute_batch(VM_COUNTER_TRIGGERS_SQL)?;
    Ok(())
}

/// Schema-2 refcount triggers (vm_fs.hash drives refcnt). Frozen for historical migrations.
const TRIGGERS_V2_SQL: &str = "
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

/// Refcounting for blobs. Chunk rows own references; deleting a file's rows releases them.
/// (Verified: nested trigger DELETEs on other tables fire; same-table recursion is not needed.)
const VM_TRIGGERS_SQL: &str = "
CREATE TRIGGER IF NOT EXISTS vm_fc_ins AFTER INSERT ON vm_file_chunks
BEGIN UPDATE vm_blobs SET refcnt=refcnt+1 WHERE hash=NEW.hash; END;
CREATE TRIGGER IF NOT EXISTS vm_fc_del AFTER DELETE ON vm_file_chunks
BEGIN
  UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=OLD.hash;
  DELETE FROM vm_blobs WHERE hash=OLD.hash AND refcnt<=0;
END;
CREATE TRIGGER IF NOT EXISTS vm_fc_upd AFTER UPDATE OF hash ON vm_file_chunks WHEN OLD.hash IS NOT NEW.hash
BEGIN
  UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=OLD.hash;
  DELETE FROM vm_blobs WHERE hash=OLD.hash AND refcnt<=0;
  UPDATE vm_blobs SET refcnt=refcnt+1 WHERE hash=NEW.hash;
END;
CREATE TRIGGER IF NOT EXISTS vm_fs_del_chunks AFTER DELETE ON vm_fs
BEGIN DELETE FROM vm_file_chunks WHERE path=OLD.path; END;
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
    refcnt INTEGER NOT NULL DEFAULT 0,
    dict_id INTEGER NOT NULL DEFAULT 0
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
  mtime_ns INTEGER NOT NULL DEFAULT 0,
  size INTEGER NOT NULL DEFAULT 0,
  link_target TEXT,
  content BLOB,
  hash TEXT,
  compressed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_vm_fs_path ON vm_fs(path);
CREATE INDEX IF NOT EXISTS idx_vm_fs_hash ON vm_fs(hash);
CREATE TABLE IF NOT EXISTS vm_file_chunks(
  path TEXT NOT NULL,
  idx INTEGER NOT NULL,
  hash BLOB NOT NULL,
  PRIMARY KEY(path, idx)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_vm_file_chunks_hash ON vm_file_chunks(hash);
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
CREATE TABLE IF NOT EXISTS vm_dicts(
  id INTEGER PRIMARY KEY,
  dict BLOB NOT NULL,
  samples INTEGER NOT NULL,
  dict_size INTEGER NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
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

/// Latest dictionary (CLI display and new writes). Dictionaries are append-only rows.
pub fn vm_get_dict(conn: &Connection) -> Result<Option<Vec<u8>>> {
    ensure_dict_table(conn)?;
    let d: Option<Vec<u8>> = conn.prepare_cached("SELECT dict FROM vm_dicts ORDER BY id DESC LIMIT 1")?
        .query_row([], |r| r.get(0)).optional()?;
    Ok(d)
}
/// Latest dictionary id, or None if no dictionary has been trained.
pub fn vm_latest_dict_id(conn: &Connection) -> Result<Option<i64>> {
    ensure_dict_table(conn)?;
    Ok(conn.prepare_cached("SELECT id FROM vm_dicts ORDER BY id DESC LIMIT 1")?
        .query_row([], |r| r.get(0)).optional()?)
}
/// Append a new dictionary. Existing dictionaries are never overwritten, so blobs written
/// under them stay readable. Returns the new dictionary id.
pub fn vm_set_dict(conn: &Connection, dict: &[u8], samples: usize) -> Result<i64> {
    ensure_dict_table(conn)?;
    conn.prepare_cached("INSERT INTO vm_dicts(dict, samples, dict_size, created_at) VALUES (?1, ?2, ?3, ?4)")?
        .execute(params![dict, samples as i64, dict.len() as i64, now_secs()])?;
    Ok(conn.last_insert_rowid())
}
/// Dictionary bytes for a given id. Cached per (database path, id): rows are immutable.
fn dict_by_id(conn: &Connection, id: i64) -> Result<Arc<Vec<u8>>> {
    let key = (conn.path().unwrap_or("").to_string(), id);
    if let Some(d) = dict_cache().lock().unwrap().get(&key) { return Ok(d.clone()); }
    let bytes: Vec<u8> = conn.prepare_cached("SELECT dict FROM vm_dicts WHERE id=?1")?
        .query_row(params![id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| anyhow!("missing dictionary {} (block written with it cannot be read)", id))?;
    let d = Arc::new(bytes);
    dict_cache().lock().unwrap().put(key, d.clone());
    Ok(d)
}
fn dict_cache() -> &'static Mutex<LruCache<(String, i64), Arc<Vec<u8>>>> {
    static C: OnceLock<Mutex<LruCache<(String, i64), Arc<Vec<u8>>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(LruCache::new(NonZeroUsize::new(16).unwrap())))
}
/// Decode one stored block. The only decode entry point: flag 3 uses the dictionary the block
/// was written with (dict_id), never a process-global one.
pub fn decode_block(conn: &Connection, body: &[u8], flag: i64, dict_id: i64) -> Result<Vec<u8>> {
    if flag == 3 {
        let d = dict_by_id(conn, dict_id)?;
        return decompress_bytes_zstd_with_dict(body, &d);
    }
    decompress_bytes_raw(body, flag)
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
    let mut rows: Vec<(Vec<u8>, i64, i64)> = Vec::new();
    {
        let mut st = conn.prepare_cached("SELECT content, compressed, dict_id FROM vm_blobs WHERE hash >= ?1 AND raw_size BETWEEN 256 AND 16383 LIMIT 150")?;
        for r in st.query_map(params![start], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?)))? { rows.push(r?); }
        if rows.len() < 150 {
            let mut st = conn.prepare_cached("SELECT content, compressed, dict_id FROM vm_blobs WHERE hash < ?1 AND raw_size BETWEEN 256 AND 16383 LIMIT ?2")?;
            for r in st.query_map(params![start, 150 - rows.len() as i64], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?)))? { rows.push(r?); }
        }
    }
    for (data, comp, dict_id) in rows.into_iter() {
        // A block that cannot be decoded is skipped: its compressed bytes must never become
        // training samples (they would silently poison the dictionary).
        let raw = match decode_block(conn, &data, comp, dict_id) { Ok(r) => r, Err(_) => continue };
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
/// Choose the smallest encoding. Returns (stored bytes, flag, dict_id); dict_id is non-zero
/// only for flag 3. None means store raw.
pub fn compress_bytes_with_conn(conn: &Connection, data: &[u8]) -> Option<(Vec<u8>, i64, i64)> {
    let dict = latest_dict(conn);
    compress_with_dict(data, dict.as_ref())
}

/// The dictionary new writes use: the latest one, loaded once. None if untrained.
pub fn latest_dict(conn: &Connection) -> Option<(i64, Arc<Vec<u8>>)> {
    let id = vm_latest_dict_id(conn).ok().flatten()?;
    let d = dict_by_id(conn, id).ok()?;
    Some((id, d))
}

/// Pure encoding decision (no DB access, safe to run on worker threads). Returns
/// (stored bytes, flag, dict_id); None means store raw. The serial and parallel write paths
/// both call this, so they cannot diverge.
pub fn compress_with_dict(data: &[u8], dict: Option<&(i64, Arc<Vec<u8>>)>) -> Option<(Vec<u8>, i64, i64)> {
    let dict = dict.filter(|(_, d)| d.len() >= 512);
    if data.len() < 512 {
        // tiny files only benefit from a dictionary
        if let Some((id, d)) = dict {
            if let Ok(c) = zstd::bulk::Compressor::with_dictionary(3, d).and_then(|mut e| e.compress(data)) {
                if c.len() + 32 < data.len() { return Some((c, 3, *id)); }
            }
        }
        if data.len() < 1024 { return None; }
    }
    let level = if data.len() > 100*1024 { 19 } else if data.len() > 16*1024 { 6 } else { 3 };
    let mut best: Option<(Vec<u8>, i64, i64)> = None;
    let mut best_len = usize::MAX;
    if data.len() < 16384 {
        if let Some((id, d)) = dict {
            if let Ok(c) = zstd::bulk::Compressor::with_dictionary(level, d).and_then(|mut e| e.compress(data)) {
                if c.len() + 32 < data.len() && c.len() < best_len {
                    best_len = c.len();
                    best = Some((c, 3, *id));
                }
            }
        }
    }
    let plain = zstd::bulk::compress(data, level).or_else(|_| zstd::bulk::compress(data, 3));
    if let Ok(c) = plain {
        if c.len() + 64 < data.len() && c.len() < best_len {
            best_len = c.len();
            best = Some((c, 2, 0));
        }
    }
    if data.len() > 16384 {
        if let Ok(c) = zstd::bulk::compress(data, 19) {
            if c.len() + 64 < data.len() && c.len() < best_len {
                best = Some((c, 2, 0));
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
pub fn decompress_bytes_zstd_with_dict(data: &[u8], dict: &[u8]) -> Result<Vec<u8>> {
    let n = zstd_frame_size(data)?;
    let mut dec = zstd::bulk::Decompressor::with_dictionary(dict).map_err(|e| anyhow!("{}", e))?;
    let out = dec.decompress(data, n).map_err(|e| anyhow!("{}", e))?;
    if out.len() != n { return Err(anyhow!("zstd+dict short read: {} of {}", out.len(), n)); }
    Ok(out)
}
/// Decode without database context. Flag 3 (dictionary) cannot be decoded here; callers must
/// use `decode_block` so the block's own dictionary is selected. Never falls back silently.
pub fn decompress_bytes_raw(data: &[u8], compressed: i64) -> Result<Vec<u8>> {
    if compressed == 3 {
        return Err(anyhow!("dictionary-compressed block needs decode_block (dict_id)"));
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
/// Ensure a blob row exists for `key` (refcnt starts at 0; triggers count references).
fn blob_ensure(conn: &Connection, key: &BlobKey, data: &[u8]) -> Result<i64> {
    if let Some(flag) = conn.prepare_cached("SELECT compressed FROM vm_blobs WHERE hash=?1")?.query_row( params![key.as_slice()], |r| r.get::<_,i64>(0)).optional()? {
        return Ok(flag);
    }
    let enc = compress_bytes_with_conn(conn, data).unwrap_or_else(|| (data.to_vec(), 0, 0));
    blob_insert_encoded(conn, key, data, enc)
}

/// Insert a blob row from an already-computed encoding (serial writer only). Called by
/// `blob_ensure` and by the parallel pack, which computes encodings on worker threads.
fn blob_insert_encoded(conn: &Connection, key: &BlobKey, data: &[u8], enc: (Vec<u8>, i64, i64)) -> Result<i64> {
    let (body, flag, dict_id) = enc;
    conn.prepare_cached("INSERT INTO vm_blobs(hash, content, compressed, raw_size, refcnt, dict_id) VALUES (?1,?2,?3,?4,0,?5)")?.execute(
        params![key.as_slice(), body, flag, data.len() as i64, dict_id])?;
    let cache_p = cache_path_for_hash(&key_hex(key));
    if !cache_p.exists() {
        if let Some(par)=cache_p.parent(){ let _ = std::fs::create_dir_all(par); }
        let _ = std::fs::write(&cache_p, data);
    }
    Ok(flag)
}

/// Store file content as CHUNK_SIZE chunks. Replaces any previous chunk list for `vm_p`;
/// the vm_fc_del trigger releases the old references. Empty file = zero chunks.
fn chunks_replace(conn: &Connection, vm_p: &str, data: &[u8]) -> Result<()> {
    conn.prepare_cached("DELETE FROM vm_file_chunks WHERE path=?1")?.execute(params![vm_p])?;
    for (i, piece) in data.chunks(CHUNK_SIZE).enumerate() {
        let key = blob_key(piece);
        blob_ensure(conn, &key, piece)?;
        conn.prepare_cached("INSERT INTO vm_file_chunks(path, idx, hash) VALUES (?1,?2,?3)")?
            .execute(params![vm_p, i as i64, key.as_slice()])?;
    }
    Ok(())
}

/// Streaming file write: reads `src` in CHUNK_SIZE pieces, storing each as a chunk while
/// hashing the whole file. Never holds more than one chunk in memory. Runs inside a
/// SAVEPOINT so a read or write error leaves the previous version of the file intact.
/// Returns the file size written.
pub fn fs_put_file_stream<R: Read>(conn: &Connection, vm_p: &str, mode: i64, mtime: i64, src: &mut R) -> Result<u64> {
    fs_put_file_stream_ns(conn, vm_p, mode, mtime, 0, src)
}

/// As `fs_put_file_stream`, also recording the host's nanosecond mtime (0 = unknown).
pub fn fs_put_file_stream_ns<R: Read>(conn: &Connection, vm_p: &str, mode: i64, mtime: i64, mtime_ns: i64, src: &mut R) -> Result<u64> {
    use sha2::{Digest, Sha256};
    conn.execute_batch("SAVEPOINT put_stream;")?;
    let res: Result<u64> = (|| {
        conn.prepare_cached("DELETE FROM vm_file_chunks WHERE path=?1")?.execute(params![vm_p])?;
        let mut whole = Sha256::new();
        let mut total: u64 = 0;
        let mut idx: i64 = 0;
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            // fill one full chunk (short reads are normal for files)
            let mut filled = 0usize;
            while filled < CHUNK_SIZE {
                let n = src.read(&mut buf[filled..])?;
                if n == 0 { break; }
                filled += n;
            }
            if filled == 0 { break; }
            let piece = &buf[..filled];
            whole.update(piece);
            let key = blob_key(piece);
            blob_ensure(conn, &key, piece)?;
            conn.prepare_cached("INSERT INTO vm_file_chunks(path, idx, hash) VALUES (?1,?2,?3)")?
                .execute(params![vm_p, idx, key.as_slice()])?;
            idx += 1;
            total += filled as u64;
            if filled < CHUNK_SIZE { break; }
        }
        let file_hash: [u8; 32] = whole.finalize().into();
        conn.prepare_cached(
            "INSERT INTO vm_fs(path,kind,mode,mtime,mtime_ns,size,hash,content,compressed,link_target) VALUES (?1,'file',?2,?3,?4,?5,?6,NULL,0,NULL)
             ON CONFLICT(path) DO UPDATE SET kind='file', mode=excluded.mode, mtime=excluded.mtime, mtime_ns=excluded.mtime_ns, size=excluded.size,
                hash=excluded.hash, content=NULL, compressed=0, link_target=NULL")?
            .execute(params![vm_p, mode, mtime, mtime_ns, total as i64, file_hash.as_slice()])?;
        Ok(total)
    })();
    match res {
        Ok(n) => { conn.execute_batch("RELEASE put_stream;")?; Ok(n) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK TO put_stream; RELEASE put_stream;"); Err(e) }
    }
}

/// Nanosecond mtime of a host path, 0 if unavailable.
fn host_mtime_ns(path: &Path) -> i64 {
    std::fs::symlink_metadata(path).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Two nanosecond stamps agree for change detection only when both are known. Unknown (0) on
/// either side means "cannot tell from ns", so the comparison falls back to the second-granularity
/// check that already passed.
fn ns_matches(stored: i64, host: i64) -> bool {
    stored == 0 || host == 0 || stored == host
}

/// Write a host file's content and record its nanosecond mtime, but only if the file did not
/// change while it was being read. The stamp is taken before the read and again after; if they
/// differ, the content may be a mix of old and new, so the row keeps ns = 0 and the next sync
/// re-reads it. Recording the pre-read stamp alone would let a concurrent write go unnoticed.
fn write_file_checked<R: Read>(conn: &Connection, host: &Path, vm_p: &str, mode: i64, mtime: i64, src: &mut R) -> Result<u64> {
    let before = host_mtime_ns(host);
    let n = fs_put_file_stream_ns(conn, vm_p, mode, mtime, before, src)?;
    let after = host_mtime_ns(host);
    if before != after || before == 0 {
        conn.prepare_cached("UPDATE vm_fs SET mtime_ns=0 WHERE path=?1")?.execute(params![vm_p])?;
    }
    Ok(n)
}

/// Test helper: write content directly, as an older writer would have.
pub fn fs_put_file_stream_pub(conn: &Connection, vm_p: &str, mode: i64, mtime: i64, data: &[u8]) -> Result<u64> {
    fs_put_file_stream(conn, vm_p, mode, mtime, &mut std::io::Cursor::new(data.to_vec()))
}

/// Public for tests: see `write_file_checked`.
pub fn write_file_checked_pub<R: Read>(conn: &Connection, host: &Path, vm_p: &str, mode: i64, mtime: i64, src: &mut R) -> Result<u64> {
    write_file_checked(conn, host, vm_p, mode, mtime, src)
}

/// Upsert a regular-file row. vm_fs.hash is the whole-file sha256 (sync/stat only).
fn fs_put_file(conn: &Connection, vm_p: &str, mode: i64, mtime: i64, data: &[u8]) -> Result<()> {
    chunks_replace(conn, vm_p, data)?;
    let file_hash = blob_key(data);
    conn.prepare_cached(
        "INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,content,compressed,link_target) VALUES (?1,'file',?2,?3,?4,?5,NULL,0,NULL)
         ON CONFLICT(path) DO UPDATE SET kind='file', mode=excluded.mode, mtime=excluded.mtime, size=excluded.size,
            hash=excluded.hash, content=NULL, compressed=0, link_target=NULL")?
        .execute(params![vm_p, mode, mtime, data.len() as i64, file_hash.as_slice()])?;
    Ok(())
}

/// Upsert a non-file row (dir/symlink). Any previous hash is released by the UPDATE trigger.
fn fs_put_node(conn: &Connection, vm_p: &str, kind: &str, mode: i64, mtime: i64, link: Option<&str>) -> Result<()> {
    // a file replaced by a dir/symlink must release its chunk references
    conn.prepare_cached("DELETE FROM vm_file_chunks WHERE path=?1")?.execute(params![vm_p])?;
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
    // (host path where the soname was found, soname). The loader looks these names up, so each
    // name that differs from its real file gets a link in the VM at the same location.
    let mut soname_links: Vec<(PathBuf, String)> = Vec::new();
    let mut q: Vec<PathBuf> = vec![host_real.clone()];
    seen.insert(host_real.clone(), "exe".to_string()); order.push(host_real.clone());
    let mut qi=0;
    while qi < q.len() {
        let cur = q[qi].clone(); qi+=1;
        let sdirs = crate::closure::search_dirs_for_cached(&cur, &[], &mut meta_cache, &mut search_cache, &ld_dirs);
        let sh = crate::closure::search_dirs_hash(&sdirs);
        // PT_INTERP (e.g. /lib64/ld-linux-x86-64.so.2) is not a DT_NEEDED entry, but the kernel
        // needs it at exactly that literal path to exec the object inside the VM. The literal
        // path is imported as-is (keeping the host symlink, which points at the real file);
        // the real file is queued like any other dependency so the link resolves.
        if let Ok(data) = std::fs::read(&cur) {
            if let Ok(elf) = goblin::elf::Elf::parse(&data) {
                if let Some(interp) = elf.interpreter {
                    let lit = PathBuf::from(interp);
                    if lit.exists() {
                        if !order.contains(&lit) {
                            seen.insert(lit.clone(), "interp".to_string());
                            order.push(lit.clone());
                        }
                        let real = std::fs::canonicalize(&lit).unwrap_or(lit.clone());
                        if !seen.contains_key(&real) {
                            seen.insert(real.clone(), "interp-target".to_string());
                            order.push(real.clone());
                            q.push(real);
                        }
                    }
                }
            }
        }
        let needed = crate::elf::meta_for_path_cached(&cur, &mut meta_cache).needed.clone();
        for soname in needed {
            if let Some(hit) = crate::closure::resolve_soname_cached(&soname, &sdirs, &mut resolve_cache, sh) {
                // the name the loader asks for (may differ from the real file, e.g. libcap.so.2)
                soname_links.push((hit.clone(), soname.clone()));
                let rp = std::fs::canonicalize(&hit).unwrap_or(hit);
                if !seen.contains_key(&rp) {
                    seen.insert(rp.clone(), "lib".to_string());
                    order.push(rp.clone());
                    q.push(rp);
                }
            }
        }
    }
    let prefix = normalize_vm_path(vm_prefix);
    // DT_NEEDED names: link each soname to the real file it resolved to, at its host location.
    // The name must exist in the VM: whether the host name equals the real file name does not
    // matter (libcap.so.2 is a host link even though its name equals the hit path).
    for (hit, name) in &soname_links {
        let Some(parent) = hit.parent() else { continue };
        let link_path = parent.join(name);
        let link_s = link_path.to_string_lossy().to_string();
        if vm_resolve(conn, &link_s).is_ok() { continue; }
        let real = std::fs::canonicalize(hit).unwrap_or(hit.clone());
        ensure_parent_dirs(conn, &link_s)?;
        vm_add_symlink(conn, &link_s, &real.to_string_lossy())?;
    }
    // Interpreter literal paths are host symlinks, often relative (../lib/...). Inside the VM the
    // target would resolve against a /lib alias that may not exist, so store the link with the
    // absolute path of the real file, which is imported below.
    let interp_literals: Vec<PathBuf> = order.iter().filter(|p| seen.get(*p).map(|k| k == "interp").unwrap_or(false)).cloned().collect();
    for lit in interp_literals {
        if let Ok(real) = std::fs::canonicalize(&lit) {
            let md = std::fs::symlink_metadata(&lit)?;
            if md.file_type().is_symlink() {
                let lit_s = lit.to_string_lossy().to_string();
                vm_add_symlink(conn, &lit_s, &real.to_string_lossy())?;
            }
        }
    }
    for p in &order {
        let vm_path = if prefix == "/" { p.to_string_lossy().to_string() } else { format!("{}/{}", prefix.trim_end_matches('/'), p.file_name().unwrap().to_string_lossy()) };
        let target = p.to_string_lossy().to_string();
        // interpreter literals were stored above as absolute links; do not overwrite them
        if seen.get(p).map(|k| k == "interp").unwrap_or(false) { continue; }
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

/// Unified file-range reader. Every content read goes through here.
/// Only the chunks overlapping [off, off+len) are fetched and verified.
/// Returns bytes clamped to the file size.
pub fn read_range(conn: &Connection, vm_path: &str, off: u64, len: usize) -> Result<Vec<u8>> {
    let real = resolve_path(conn, vm_path)?;
    let (kind, size, hash): (String, i64, Option<Vec<u8>>) = conn
        .prepare_cached("SELECT kind, size, hash FROM vm_fs WHERE path=?1")?
        .query_row(params![real], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or_else(|| anyhow!("not found in VM: {}", real))?;
    if kind == "dir" { return Err(anyhow!("is a directory: {}", vm_path)); }
    if kind != "file" { return Err(anyhow!("not a regular file: {}", vm_path)); }
    // a regular file without a whole-file hash is a corrupt row (same rule as vm_resolve)
    if hash.is_none() { return Err(anyhow!("corrupt row for {}: file without content", real)); }
    let size = size as u64;
    let start = off.min(size);
    let end = start.saturating_add(len as u64).min(size);
    if start == end { return Ok(Vec::new()); }
    let first = (start / CHUNK_SIZE as u64) as i64;
    let last = ((end - 1) / CHUNK_SIZE as u64) as i64;
    let mut st = conn.prepare_cached(
        "SELECT c.idx, b.content, b.compressed, c.hash, b.dict_id FROM vm_file_chunks c JOIN vm_blobs b ON b.hash=c.hash
         WHERE c.path=?1 AND c.idx BETWEEN ?2 AND ?3 ORDER BY c.idx")?;
    let rows = st.query_map(params![real, first, last], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?, r.get::<_,Vec<u8>>(3)?, r.get::<_,i64>(4)?)))?;
    let mut out: Vec<u8> = Vec::with_capacity((end - start) as usize);
    let mut got_last = false;
    for row in rows {
        let (idx, body, comp, key, dict_id) = row?;
        let piece = decode_block(conn, &body, comp, dict_id).map_err(|e| anyhow!("{}: chunk {} undecodable: {}", real, idx, e))?;
        if blob_key(&piece).as_slice() != key.as_slice() { return Err(anyhow!("{}: chunk {} hash mismatch", real, idx)); }
        // slice this chunk to the requested window
        let chunk_base = idx as u64 * CHUNK_SIZE as u64;
        let from = start.saturating_sub(chunk_base).min(piece.len() as u64) as usize;
        let to = end.saturating_sub(chunk_base).min(piece.len() as u64) as usize;
        if from < to { out.extend_from_slice(&piece[from..to]); }
        if idx == last { got_last = true; }
    }
    if !got_last { return Err(anyhow!("{}: missing chunk {} for requested range", real, last)); }
    Ok(out)
}

/// Follow symlinks and return the real path. Does not load any content.
pub fn resolve_path(conn: &Connection, vm_path: &str) -> Result<String> {
    let mut cur = normalize_vm_path(vm_path);
    let mut visited = std::collections::HashSet::new();
    for _ in 0..40 {
        if !visited.insert(cur.clone()) { return Err(anyhow!("symlink loop at {}", cur)); }
        let row: Option<(String, Option<String>)> = conn.prepare_cached("SELECT kind, link_target FROM vm_fs WHERE path=?1")?
            .query_row(params![cur], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (kind, link) = row.ok_or_else(|| anyhow!("not found in VM: {}", cur))?;
        if kind != "symlink" { return Ok(cur); }
        let target = link.unwrap_or_default();
        let next = if target.starts_with('/') { normalize_vm_path(&target) } else {
            let dir = Path::new(&cur).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or("/".to_string());
            if dir == "/" { format!("/{}", target) } else { format!("{}/{}", dir, target) }
        };
        cur = normalize_vm_path(&next);
    }
    Err(anyhow!("too many symlink levels: {}", vm_path))
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
/// Assemble a regular file's bytes from its chunk list. Chunks are verified against their
/// key (sha256) so a corrupt or mis-keyed chunk is an error, never silently wrong data.
pub fn file_bytes(conn: &Connection, path: &str) -> Result<Vec<u8>> {
    let mut st = conn.prepare_cached("SELECT c.idx, b.content, b.compressed, c.hash, b.dict_id FROM vm_file_chunks c JOIN vm_blobs b ON b.hash=c.hash WHERE c.path=?1 ORDER BY c.idx")?;
    let rows = st.query_map(params![path], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?, r.get::<_,Vec<u8>>(3)?, r.get::<_,i64>(4)?)))?;
    let mut out = Vec::new();
    let mut expect = 0i64;
    for row in rows {
        let (idx, body, comp, key, dict_id) = row?;
        if idx != expect { return Err(anyhow!("{}: chunk index gap at {}", path, expect)); }
        expect += 1;
        let piece = decode_block(conn, &body, comp, dict_id).map_err(|e| anyhow!("{}: chunk {} undecodable: {}", path, idx, e))?;
        if blob_key(&piece).as_slice() != key.as_slice() { return Err(anyhow!("{}: chunk {} hash mismatch", path, idx)); }
        out.extend_from_slice(&piece);
    }
    Ok(out)
}

/// Resolve a path (following symlinks) to (real path, kind, content, link target, mode).
/// Regular-file content comes from vm_file_chunks only; a file with a hash but no chunks is
/// an error (the v4->v5 migration gives every such file a chunk row).
pub fn vm_resolve(conn: &Connection, vm_path: &str) -> Result<(String, String, Option<Vec<u8>>, Option<String>, i64)> {
    let mut cur = normalize_vm_path(vm_path);
    let mut visited = std::collections::HashSet::new();
    for _ in 0..40 {
        if !visited.insert(cur.clone()) { return Err(anyhow!("symlink loop at {}", cur)); }
        let row: Option<(String, Option<String>, i64, Option<Vec<u8>>, Option<Vec<u8>>, i64)> = conn.prepare_cached("SELECT kind, link_target, mode, hash, content, compressed FROM vm_fs WHERE path=?1")?
            .query_row(params![cur], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_,Option<i64>>(5)?.unwrap_or(0)))).optional()?;
        let (kind, link, mode, hash, inline, comp) = match row { Some(x)=>x, None=> return Err(anyhow!("not found in VM: {}", cur)) };
        if kind != "symlink" {
            if kind == "dir" { return Ok((cur, kind, None, link, mode)); }
            let content = if hash.is_some() {
                file_bytes(conn, &cur)?
            } else if let Some(b) = inline {
                // legacy inline row (pre-blob); never produced by current code
                decompress_bytes_raw(&b, comp).map_err(|e| anyhow!("corrupt inline content for {}: {}", cur, e))?
            } else {
                // a regular file must have either a hash (chunked) or legacy inline bytes.
                // Neither means the row is corrupt: fail instead of returning an empty file.
                return Err(anyhow!("corrupt row for {}: file without content", cur));
            };
            return Ok((cur, kind, Some(content), link, mode));
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
        // count regular files only (the CLI reports "N files"); dirs are created implicitly
        let is_file = e.file_type().is_file();
        let hp = e.path().to_string_lossy().to_string();
        let rel = e.path().strip_prefix(host_dir).unwrap_or(e.path()).to_string_lossy().to_string();
        let vm_p = if vm_prefix == "/" { format!("/{}", rel.trim_start_matches('/')) } else { format!("{}/{}", vm_prefix.trim_end_matches('/'), rel.trim_start_matches('/')) };
        vm_add_file(conn, &hp, &normalize_vm_path(&vm_p))?;
        if is_file { n+=1; }
        if n % 2000 == 0 { conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?; }
    }
    conn.execute_batch("COMMIT;")?;
    Ok(n)
}
/// Secondary indexes on the tables a bulk load writes. Dropping them for the load and rebuilding
/// afterwards is much cheaper than maintaining them row by row. Definitions are read from
/// sqlite_master, so the rebuilt indexes are exactly the schema's, not a second copy.
fn bulk_index_defs(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut st = conn.prepare("SELECT name, sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL AND tbl_name IN ('vm_fs','vm_blobs','vm_file_chunks') ORDER BY name")?;
    let rows = st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Run `load` with the bulk-path indexes dropped and restored afterwards.
///
/// Restore happens on every exit path. A restore failure is reported as an error, never
/// swallowed: a database left without its indexes must not look like a successful import.
/// Panics unwind through the guard, which still restores the indexes (best effort, since a
/// panic cannot return an error; the panic itself is still propagated).
pub fn with_bulk_indexes_dropped<T>(conn: &Connection, load: impl FnOnce() -> Result<T>) -> Result<T> {
    let defs = bulk_index_defs(conn)?;
    for (name, _) in &defs {
        conn.execute(&format!("DROP INDEX IF EXISTS \"{}\"", name), [])?;
    }
    struct Restore<'a> { conn: &'a Connection, defs: &'a [(String, String)], armed: bool }
    impl Restore<'_> {
        fn run(&mut self) -> Result<()> {
            self.armed = false;
            for (_, sql) in self.defs {
                self.conn.execute_batch(&format!("{};", sql))
                    .map_err(|e| anyhow!("rebuild index failed (database left unindexed): {}", e))?;
            }
            Ok(())
        }
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            // reached only on panic (run() disarms it): best-effort restore
            if self.armed { for (_, sql) in self.defs { let _ = self.conn.execute_batch(&format!("{};", sql)); } }
        }
    }
    let mut guard = Restore { conn, defs: &defs, armed: true };
    let out = load();
    let restored = guard.run();
    match (out, restored) {
        (Ok(v), Ok(())) => Ok(v),
        // keep the import's own error; still surface that the database is unindexed
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(r)) => Err(anyhow!("{}; and {}", e, r)),
        (Ok(_), Err(r)) => Err(r),
    }
}

/// Parallel directory import. Compression (CPU) runs on rayon workers in bounded batches;
/// SQLite writes stay serial in walk order inside short transactions. The dictionary is read
/// once per run, so every file is encoded exactly as the serial path would encode it.
/// Output matches `vm_pack_host_dir` (verified by a test); only wall time differs.
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

pub fn vm_pack_host_dir_parallel(conn: &Connection, host_dir: &str, vm_prefix: &str) -> Result<usize> {
    let n = vm_pack_host_dir_parallel_inner(conn, host_dir, vm_prefix)?;
    // bound the content cache once per import (a full scan, so not per file)
    let _ = enforce_cache_limit(&cache_dir(), cache_limit_from_env());
    // a bulk load changes the data distribution the planner relies on; refresh statistics
    // (cheap: a few ms on a 30k-file tree). Failure here is harmless, so it is not an error.
    let _ = conn.execute_batch("PRAGMA optimize;");
    Ok(n)
}

fn vm_pack_host_dir_parallel_inner(conn: &Connection, host_dir: &str, vm_prefix: &str) -> Result<usize> {
    use rayon::prelude::*;
    const BATCH: usize = 256;
    // 1) plan: (host path, normalized vm path) in walk order, regular files only
    let mut plan: Vec<(PathBuf, String)> = Vec::new();
    for entry in walkdir::WalkDir::new(host_dir).follow_links(false).min_depth(1) {
        let e = entry?;
        if !e.file_type().is_file() { continue; }
        let rel = e.path().strip_prefix(host_dir).unwrap_or(e.path()).to_string_lossy().to_string();
        let vm_p = if vm_prefix == "/" { format!("/{}", rel.trim_start_matches('/')) }
                   else { format!("{}/{}", vm_prefix.trim_end_matches('/'), rel.trim_start_matches('/')) };
        plan.push((e.path().to_path_buf(), normalize_vm_path(&vm_p)));
    }
    // 2) one dictionary snapshot for the whole run
    let dict = latest_dict(conn);
    with_bulk_indexes_dropped(conn, || {
    let mut n = 0usize;
    for chunk in plan.chunks(BATCH) {
        // parallel: read + encode each file's chunks (pure CPU, no DB)
        let encoded: Vec<Result<(PathBuf, String, u32, i64, Vec<(BlobKey, usize, (Vec<u8>, i64, i64))>, Vec<u8>)>> = chunk
            .par_iter()
            .map(|(host, vm_p)| {
                let md = std::fs::metadata(host)?;
                #[cfg(unix)] let mode = { use std::os::unix::fs::PermissionsExt; md.permissions().mode() };
                #[cfg(not(unix))] let mode = 0o644u32;
                let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or_else(now_secs);
                let data = std::fs::read(host)?;
                let file_key = blob_key(&data);
                let pieces: Vec<(BlobKey, usize, (Vec<u8>, i64, i64))> = data.chunks(CHUNK_SIZE).map(|piece| {
                    let k = blob_key(piece);
                    let enc = compress_with_dict(piece, dict.as_ref()).unwrap_or_else(|| (piece.to_vec(), 0, 0));
                    (k, piece.len(), enc)
                }).collect();
                let _ = file_key;
                Ok((host.clone(), vm_p.clone(), mode, mtime, pieces, data))
            })
            .collect();
        // serial writes, walk order
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        for item in encoded {
            let (host, vm_p, mode, mtime, pieces, data) = item.map_err(|e: anyhow::Error| anyhow!("{}", e))?;
            ensure_parent_dirs(conn, &vm_p)?;
            conn.prepare_cached("DELETE FROM vm_file_chunks WHERE path=?1")?.execute(params![vm_p])?;
            for (i, (key, plen, enc)) in pieces.into_iter().enumerate() {
                let piece_start = i * CHUNK_SIZE;
                let piece = &data[piece_start..piece_start + plen];
                if conn.prepare_cached("SELECT 1 FROM vm_blobs WHERE hash=?1")?.query_row(params![key.as_slice()], |_| Ok(())).optional()?.is_none() {
                    blob_insert_encoded(conn, &key, piece, enc)?;
                }
                conn.prepare_cached("INSERT INTO vm_file_chunks(path, idx, hash) VALUES (?1,?2,?3)")?
                    .execute(params![vm_p, i as i64, key.as_slice()])?;
            }
            let file_hash = blob_key(&data);
            conn.prepare_cached(
                "INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,content,compressed,link_target) VALUES (?1,'file',?2,?3,?4,?5,NULL,0,NULL)
                 ON CONFLICT(path) DO UPDATE SET kind='file', mode=excluded.mode, mtime=excluded.mtime, size=excluded.size,
                    hash=excluded.hash, content=NULL, compressed=0, link_target=NULL")?
                .execute(params![vm_p, mode as i64, mtime, data.len() as i64, file_hash.as_slice()])?;
            let _ = host;
            n += 1;
        }
        conn.execute_batch("COMMIT;")?;
    }
    Ok(n)
    })
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

/// Load one file's bytes inside a worker through the chunk reader. A read failure is
/// reported on stderr and the file is skipped, never written as empty.
fn load_task_bytes<T: TaskLike>(db_path: Option<&str>, t: &T) -> Option<Vec<u8>> {
    let path = db_path?;
    WORKER_CONN.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() { *slot = Connection::open(path).ok(); }
        let c = slot.as_ref()?;
        match file_bytes(c, t.path_ref()) {
            Ok(b) => Some(b),
            Err(e) => { eprintln!("materialize {}: {}", t.path_ref(), e); None }
        }
    })
}

trait TaskLike { fn path_ref(&self) -> &str; }

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

/// Materialize with an explicit content-cache root (tests and callers that manage their own cache).
pub fn vm_materialize_tree_with_cache(conn: &Connection, dest: &Path, cache_root: &Path) -> Result<usize> {
    use rayon::prelude::*;
    std::fs::create_dir_all(dest)?;
    if dest.exists() { prune_scratch(conn, dest)?; }
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
        let cache_p = cache_root.join(&key[0..2.min(key.len())]).join(&key);
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
/// Summary of one sync pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport { pub created: usize, pub updated: usize, pub unchanged: usize, pub deleted: usize, pub failed: usize }

fn is_reserved_vm_path(p: &str) -> bool {
    p == "/" || p == "/dev" || p.starts_with("/dev/") || p == "/proc" || p.starts_with("/proc/") || p == "/sys" || p.starts_with("/sys/")
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

    // 2) Host side: walk, keep only metadata. Entries are processed as we go.
    let mut seen: HashSet<String> = HashSet::new();
    let mut rep = SyncReport::default();
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let now = now_secs();
    for entry in walkdir::WalkDir::new(host_root).follow_links(false).min_depth(1) {
        let e = match entry { Ok(v) => v, Err(_) => { rep.failed += 1; continue; } };
        let rel = e.path().strip_prefix(host_root).unwrap_or(e.path()).to_string_lossy().to_string();
        let vm_p = normalize_vm_path(&format!("/{}", rel.trim_start_matches('/')));
        if is_reserved_vm_path(&vm_p) { continue; }
        let md = match std::fs::symlink_metadata(e.path()) { Ok(m) => m, Err(_) => { rep.failed += 1; continue; } };
        #[cfg(unix)] let mode = md.permissions().mode() as i64;
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
            Err(err) => { eprintln!("sync {}: {}", vm_p, err); rep.failed += 1; }
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
/// Re-split legacy single-chunk files larger than CHUNK_SIZE into CHUNK_SIZE chunks.
/// Each file is rewritten in its own transaction and verified by re-reading the new chunk
/// list before commit; any mismatch rolls that file back and is reported. Idempotent:
/// files already chunked are skipped. Returns (rechunked, skipped_already_chunked, failed).
pub fn vm_rechunk(conn: &Connection) -> Result<(usize, usize, usize)> {
    let candidates: Vec<(String, i64)> = {
        let mut st = conn.prepare("SELECT f.path, f.size FROM vm_fs f WHERE f.kind='file' AND f.size > ?1 AND f.hash IS NOT NULL")?;
        let rows = st.query_map(params![CHUNK_SIZE as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let (mut done, mut skipped, mut failed) = (0usize, 0usize, 0usize);
    for (path, _size) in candidates {
        let nchunks: i64 = conn.prepare_cached("SELECT count(*) FROM vm_file_chunks WHERE path=?1")?.query_row(params![path], |r| r.get(0))?;
        if nchunks != 1 { skipped += 1; continue; }
        let before = file_bytes(conn, &path)?;
        let want = blob_key(&before);
        conn.execute_batch("SAVEPOINT rechunk_one;")?;
        let res: Result<()> = (|| {
            chunks_replace(conn, &path, &before)?;
            let after = file_bytes(conn, &path)?;
            if blob_key(&after) != want || after != before {
                return Err(anyhow!("verify failed after rechunk"));
            }
            Ok(())
        })();
        match res {
            Ok(()) => { conn.execute_batch("RELEASE rechunk_one;")?; done += 1; }
            Err(e) => {
                conn.execute_batch("ROLLBACK TO rechunk_one; RELEASE rechunk_one;")?;
                eprintln!("rechunk {}: {}", path, e);
                failed += 1;
            }
        }
    }
    Ok((done, skipped, failed))
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
        let raw = decompress_bytes_raw(&data, cflag)?;
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
        for r in st.query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,Vec<u8>>(1)?)))? { gz_blobs.push(r?); }
    }
    for (hash, gz) in gz_blobs {
        // undecodable input is an error, never a silent pass-through of compressed bytes
        let raw = decompress_bytes_raw(&gz, 1)?;
        before += gz.len() as i64;
        if let Some((z, flag, dict_id)) = compress_bytes_with_conn(conn, &raw) {
            after += z.len() as i64;
            conn.prepare_cached("UPDATE vm_blobs SET content=?2, compressed=?3, dict_id=?4 WHERE hash=?1")?.execute( params![hash, z, flag, dict_id])?;
            n+=1;
        } else {
            after += gz.len() as i64;
        }
    }
    // recompress uncompressed blobs that would benefit from level 19 or dict
    // (Previously read the BLOB key as String and dropped the error with `if let Ok`,
    // so this pass silently did nothing. Keys are now read as bytes and errors propagate.)
    let mut plain_blobs: Vec<(Vec<u8>, Vec<u8>, i64)> = Vec::new();
    {
        let mut st = conn.prepare("SELECT hash, content, raw_size FROM vm_blobs WHERE compressed=0 AND raw_size > 1024 LIMIT 200")?;
        for r in st.query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?)))? {
            plain_blobs.push(r?);
        }
    }
    for (hash, data, _raw) in plain_blobs {
        let raw = data.clone();
        if let Some((z, flag, dict_id)) = compress_bytes_with_conn(conn, &raw) {
            before += raw.len() as i64;
            after += z.len() as i64;
            conn.prepare_cached("UPDATE vm_blobs SET content=?2, compressed=?3, dict_id=?4 WHERE hash=?1")?.execute( params![hash.clone(), z, flag, dict_id])?;
            n+=1;
        }
    }
    // recompress zstd level 3 blobs to level 19 if beneficial
    let mut zstd_blobs: Vec<(Vec<u8>, Vec<u8>, i64)> = Vec::new();
    {
        let mut st = conn.prepare("SELECT hash, content, compressed FROM vm_blobs WHERE compressed=2 LIMIT 300")?;
        for r in st.query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?)))? {
            zstd_blobs.push(r?);
        }
    }
    for (hash, data, comp) in zstd_blobs {
        // flag 2 only (no dictionary): an undecodable block is an error, not a pass-through
        let raw = decompress_bytes_raw(&data, comp)?;
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
        // refcnt must equal the number of chunk rows referencing each blob
        let bad_rc: i64 = conn.prepare_cached("SELECT count(*) FROM vm_blobs b WHERE b.refcnt != (SELECT count(*) FROM vm_file_chunks c WHERE c.hash=b.hash)")?.query_row( [], |r| r.get(0))?;
        if bad_rc > 0 { drift.push(format!("{} blobs with bad refcnt", bad_rc)); }
        // every chunk must point at an existing blob
        let orphan: i64 = conn.prepare_cached("SELECT count(*) FROM vm_file_chunks c WHERE NOT EXISTS (SELECT 1 FROM vm_blobs b WHERE b.hash=c.hash)")?.query_row( [], |r| r.get(0))?;
        if orphan > 0 { drift.push(format!("{} chunks reference missing blobs", orphan)); }
        // chunks must form a contiguous 0..n sequence per file
        let gaps: i64 = conn.prepare_cached("SELECT count(*) FROM (SELECT path, count(*) AS n, max(idx) AS mx FROM vm_file_chunks GROUP BY path) WHERE mx + 1 != n")?.query_row( [], |r| r.get(0))?;
        if gaps > 0 { drift.push(format!("{} files with chunk index gaps", gaps)); }
        // every regular file with bytes must have chunks (size>0 => at least one chunk)
        let no_chunks: i64 = conn.prepare_cached("SELECT count(*) FROM vm_fs f WHERE f.kind='file' AND f.size>0 AND NOT EXISTS (SELECT 1 FROM vm_file_chunks c WHERE c.path=f.path)")?.query_row( [], |r| r.get(0))?;
        if no_chunks > 0 { drift.push(format!("{} non-empty files without chunks", no_chunks)); }
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

