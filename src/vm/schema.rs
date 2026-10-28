use super::*;

pub fn vm_apply_pragmas(conn: &Connection) -> Result<()> {
    harden(conn)?;
    // Every VM connection must know the policy bypass function: the policy triggers call it, and a
    // trigger whose function is missing fails with "no such function" instead of the policy's refusal.
    // The flag is dropped here; a caller that needs a bypass registers its own (see policy::register).
    let _ = crate::policy::register(conn);
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

/// SQLite's defenses for database files that may come from someone else (a package, a Base, an uploaded
/// VM). Applied to every connection this crate opens:
/// - DEFENSIVE: no `writable_schema`, no direct writes to shadow tables, so SQL cannot corrupt the file;
/// - trusted_schema=OFF: triggers and views stored in a file may call only functions marked innocuous, so a
///   hostile schema cannot reach functions with side effects;
/// - cell_size_check: page cells are bounds-checked on read, so a crafted page is an error, not an
///   out-of-bounds read.
pub fn harden(conn: &Connection) -> Result<()> {
    use rusqlite::config::DbConfig;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    conn.execute_batch("PRAGMA cell_size_check=ON;")?;
    Ok(())
}

pub(super) fn ensure_dict_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vm_dict(\n  id INTEGER PRIMARY KEY CHECK(id=1),\n  dict BLOB NOT NULL,\n  samples INTEGER NOT NULL,\n  dict_size INTEGER NOT NULL,\n  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))\n);"
    )?;
    Ok(())
}

pub(super) fn ensure_disk_blocks_table(conn: &Connection) -> Result<()> {
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
                7 => migrate_7_to_8(conn)?,
                8 => migrate_8_to_9(conn)?,
                9 => migrate_9_to_10(conn)?,
                10 => migrate_10_to_11(conn)?,
                11 => migrate_11_to_12(conn)?,
                12 => migrate_12_to_13(conn)?,
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
const VM_SNAPSHOT_SQL: &str = "
CREATE TABLE IF NOT EXISTS vm_snapshot_fs(
    snap_id INTEGER NOT NULL REFERENCES vm_snapshots(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    mode INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL DEFAULT 0,
    size INTEGER NOT NULL,
    link_target TEXT,
    hash BLOB,
    PRIMARY KEY(snap_id, path)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS vm_snapshot_chunks(
    snap_id INTEGER NOT NULL REFERENCES vm_snapshots(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    idx INTEGER NOT NULL,
    hash BLOB NOT NULL,
    PRIMARY KEY(snap_id, path, idx)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_vm_snapshot_chunks_hash ON vm_snapshot_chunks(hash);
CREATE TRIGGER IF NOT EXISTS vm_snc_ins AFTER INSERT ON vm_snapshot_chunks
BEGIN UPDATE vm_blobs SET refcnt=refcnt+1 WHERE hash=NEW.hash; END;
CREATE TRIGGER IF NOT EXISTS vm_snc_del AFTER DELETE ON vm_snapshot_chunks
BEGIN
  UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=OLD.hash;
  DELETE FROM vm_blobs WHERE hash=OLD.hash AND refcnt<=0;
END;
";

/// Agent step log (schema 9). One row per executed step.
const VM_AGENT_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_steps(
    step_id INTEGER PRIMARY KEY,
    cmd TEXT NOT NULL,
    exit_code INTEGER,
    timed_out INTEGER NOT NULL DEFAULT 0,
    rolled_back INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER NOT NULL,
    stdout BLOB,
    stderr BLOB,
    stdout_truncated INTEGER NOT NULL DEFAULT 0,
    stderr_truncated INTEGER NOT NULL DEFAULT 0,
    snap_before TEXT NOT NULL,
    snap_after TEXT,
    fs_diff_json TEXT NOT NULL,
    -- a session step's undo record (vm_undo.id); NULL for steps recorded with snapshots
    undo_id INTEGER,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
";

/// 8 -> 9: agent step log.
fn migrate_8_to_9(conn: &Connection) -> Result<()> {
    conn.execute_batch(VM_AGENT_SQL)?;
    Ok(())
}

/// 10 -> 11: the policy table and its triggers (see policy.rs). Empty until a rule is added.
fn migrate_10_to_11(conn: &Connection) -> Result<()> {
    crate::policy::ensure_schema(conn)
}

/// 11 -> 12: vm_dirty, the files whose whole-file hash a deferred write left stale.
fn migrate_11_to_12(conn: &Connection) -> Result<()> {
    conn.execute_batch(VM_DIRTY_SQL)?;
    Ok(())
}

/// 12 -> 13: undo records (see undo.rs), and the step that each one belongs to.
fn migrate_12_to_13(conn: &Connection) -> Result<()> {
    conn.execute_batch(VM_UNDO_SQL)?;
    let cols: Vec<String> = conn.prepare("PRAGMA table_info(agent_steps)")?
        .query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<_>>()?;
    if !cols.iter().any(|c| c == "undo_id") {
        conn.execute_batch("ALTER TABLE agent_steps ADD COLUMN undo_id INTEGER;")?;
    }
    Ok(())
}

/// Files whose `vm_fs.hash` is stale. A deferred write (`write_range_deferred`) changes a file's
/// chunks and size but not its whole-file hash, which costs a read of the whole file; the path is
/// recorded here instead, and `finalize_dirty` recomputes the hash before anything reads it. The
/// foreign key keeps the row with its file through a rename and drops it with the file.
pub(super) const VM_DIRTY_SQL: &str = "
CREATE TABLE IF NOT EXISTS vm_dirty(
  path TEXT PRIMARY KEY REFERENCES vm_fs(path) ON DELETE CASCADE ON UPDATE CASCADE
) WITHOUT ROWID;";

/// 9 -> 10: agent_steps gains the output-truncation flags. A v9 database created by the first
/// agent-step build lacks them; add each column only if it is missing, so a fresh v9 table is fine.
fn migrate_9_to_10(conn: &Connection) -> Result<()> {
    let cols: Vec<String> = conn.prepare("PRAGMA table_info(agent_steps)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    for col in ["stdout_truncated", "stderr_truncated"] {
        if !cols.iter().any(|c| c == col) {
            conn.execute_batch(&format!("ALTER TABLE agent_steps ADD COLUMN {} INTEGER NOT NULL DEFAULT 0;", col))?;
        }
    }
    Ok(())
}

/// 7 -> 8: content snapshot tables. Only metadata and references are copied; no blob bytes.
fn migrate_7_to_8(conn: &Connection) -> Result<()> {
    conn.execute_batch(VM_SNAPSHOT_SQL)?;
    Ok(())
}

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
    vm_apply_pragmas(&conn)?;
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
    // Hashes left stale by a writer that stopped before finalizing (a crashed mount). Not fatal here:
    // a failure leaves the rows in vm_dirty, and every reader of hashes finalizes again and reports it.
    if let Err(e) = finalize_dirty(&conn) { eprintln!("warning: {}: pending file hashes not finalized: {}", path, e); }
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
{snapshot}
{agent}
{dirty}
{undo}
"#, app=VM_APP_ID, ver=VM_USER_VERSION, triggers=VM_TRIGGERS_SQL, counters=VM_COUNTERS_INIT_SQL, counter_triggers=VM_COUNTER_TRIGGERS_SQL, snapshot=VM_SNAPSHOT_SQL, agent=VM_AGENT_SQL, dirty=VM_DIRTY_SQL, undo=VM_UNDO_SQL)
}

pub fn init_vm_db_with_opts(path: &str, overwrite: bool, vm_only: bool) -> Result<Connection> {
    if Path::new(path).exists() {
        if !overwrite { return Err(anyhow!("exists: {} (use --force)", path)); }
        std::fs::remove_file(path)?;
    }
    let conn = Connection::open(path)?;
    let _ = conn.execute_batch("PRAGMA page_size=8192; PRAGMA auto_vacuum=INCREMENTAL;");
    conn.execute_batch(&vm_schema_sql())?;
    vm_apply_pragmas(&conn)?;
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
    vm_apply_pragmas(&conn)?;
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
