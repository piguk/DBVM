use super::*;

/// Ensure a blob row exists for `key` (refcnt starts at 0; triggers count references).
pub fn blob_ensure(conn: &Connection, key: &BlobKey, data: &[u8]) -> Result<i64> {
    blob_ensure_with(conn, key, data, Effort::Archive)
}

/// `blob_ensure` at a chosen compression effort. Archive effort also places the content in the host
/// content cache, ahead of a materialize. Interactive effort does not: a mount rewrites a chunk on every
/// write() that touches it, and caching each intermediate version only filled the cache with content
/// nothing reads.
pub(super) fn blob_ensure_with(conn: &Connection, key: &BlobKey, data: &[u8], effort: Effort) -> Result<i64> {
    if let Some(flag) = conn.prepare_cached("SELECT compressed FROM vm_blobs WHERE hash=?1")?.query_row( params![key.as_slice()], |r| r.get::<_,i64>(0)).optional()? {
        return Ok(flag);
    }
    let enc = compress_with_effort(data, latest_dict(conn).as_ref(), effort).unwrap_or_else(|| (data.to_vec(), 0, 0));
    match effort {
        Effort::Archive => blob_insert_encoded(conn, key, data, enc),
        Effort::Interactive => blob_insert_row(conn, key, data.len(), enc),
    }
}

/// Insert a blob row from an already-computed encoding (serial writer only). Called by
/// `blob_ensure` and by the parallel pack, which computes encodings on worker threads.
pub(super) fn blob_insert_encoded(conn: &Connection, key: &BlobKey, data: &[u8], enc: (Vec<u8>, i64, i64)) -> Result<i64> {
    let flag = blob_insert_row(conn, key, data.len(), enc)?;
    cache_put(&cache_path_for_hash(&key_hex(key)), data);
    Ok(flag)
}

/// The vm_blobs row alone, without touching the host content cache.
fn blob_insert_row(conn: &Connection, key: &BlobKey, raw_len: usize, enc: (Vec<u8>, i64, i64)) -> Result<i64> {
    let (body, flag, dict_id) = enc;
    conn.prepare_cached("INSERT INTO vm_blobs(hash, content, compressed, raw_size, refcnt, dict_id) VALUES (?1,?2,?3,?4,0,?5)")?.execute(
        params![key.as_slice(), body, flag, raw_len as i64, dict_id])?;
    Ok(flag)
}

/// Store file content as CHUNK_SIZE chunks. Replaces any previous chunk list for `vm_p`;
/// the vm_fc_del trigger releases the old references. Empty file = zero chunks.
pub(super) fn chunks_replace(conn: &Connection, vm_p: &str, data: &[u8]) -> Result<()> {
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
pub(super) fn host_mtime_ns(path: &Path) -> i64 {
    std::fs::symlink_metadata(path).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Two nanosecond stamps agree for change detection only when both are known. Unknown (0) on
/// either side means "cannot tell from ns", so the comparison falls back to the second-granularity
/// check that already passed.
pub(super) fn ns_matches(stored: i64, host: i64) -> bool {
    stored == 0 || host == 0 || stored == host
}

/// Write a host file's content and record its nanosecond mtime, but only if the file did not
/// change while it was being read. The stamp is taken before the read and again after; if they
/// differ, the content may be a mix of old and new, so the row keeps ns = 0 and the next sync
/// re-reads it. Recording the pre-read stamp alone would let a concurrent write go unnoticed.
pub(super) fn write_file_checked<R: Read>(conn: &Connection, host: &Path, vm_p: &str, mode: i64, mtime: i64, src: &mut R) -> Result<u64> {
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
    // One savepoint: the chunk rows are replaced and the file row updated as a single unit, so
    // another connection sees the old file or the new one, never a file with its chunks removed.
    // Nests inside a caller's transaction; commits on its own when called alone.
    conn.execute_batch("SAVEPOINT fs_put;")?;
    let res: Result<()> = (|| {
        chunks_replace(conn, vm_p, data)?;
        let file_hash = blob_key(data);
        conn.prepare_cached(
            "INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,content,compressed,link_target) VALUES (?1,'file',?2,?3,?4,?5,NULL,0,NULL)
             ON CONFLICT(path) DO UPDATE SET kind='file', mode=excluded.mode, mtime=excluded.mtime, size=excluded.size,
                hash=excluded.hash, content=NULL, compressed=0, link_target=NULL")?
            .execute(params![vm_p, mode, mtime, data.len() as i64, file_hash.as_slice()])?;
        Ok(())
    })();
    match res {
        Ok(()) => { conn.execute_batch("RELEASE fs_put;")?; Ok(()) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK TO fs_put; RELEASE fs_put;"); Err(e) }
    }
}

/// Upsert a non-file row (dir/symlink). Any previous hash is released by the UPDATE trigger.
pub(super) fn fs_put_node(conn: &Connection, vm_p: &str, kind: &str, mode: i64, mtime: i64, link: Option<&str>) -> Result<()> {
    // Savepoint: releasing a replaced file's chunks and updating its row are one unit, so no
    // other connection sees a regular-file row without content. Nests inside a caller's transaction.
    conn.execute_batch("SAVEPOINT fs_node;")?;
    let res: Result<()> = (|| {
        // a file replaced by a dir/symlink must release its chunk references
        conn.prepare_cached("DELETE FROM vm_file_chunks WHERE path=?1")?.execute(params![vm_p])?;
        conn.execute(
            "INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,content,compressed,link_target) VALUES (?1,?2,?3,?4,0,NULL,NULL,0,?5)
             ON CONFLICT(path) DO UPDATE SET kind=excluded.kind, mode=excluded.mode, mtime=excluded.mtime, size=0,
                hash=NULL, content=NULL, compressed=0, link_target=excluded.link_target",
            params![vm_p, kind, mode, mtime, link])?;
        Ok(())
    })();
    match res {
        Ok(()) => { conn.execute_batch("RELEASE fs_node;")?; Ok(()) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK TO fs_node; RELEASE fs_node;"); Err(e) }
    }
}

pub fn vm_add_bytes(conn: &Connection, vm_path: &str, data: &[u8], mode: i64, mtime: i64) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    fs_put_file(conn, &vm_p, mode, mtime, data)
}

pub fn vm_add_symlink(conn: &Connection, vm_path: &str, target: &str) -> Result<()> {
    let vm_p = normalize_vm_path(vm_path);
    ensure_parent_dirs(conn, &vm_p)?;
    // a symlink's own mode is always 0777 on Linux; match what the host reports, so a sync
    // does not see a mode mismatch on every symlink
    fs_put_node(conn, &vm_p, "symlink", 0o777, now_secs(), Some(target))
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
    #[cfg(unix)] let mode = { use std::os::unix::fs::PermissionsExt; (md.permissions().mode() & 0o7777) as i64 };
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

/// Run `f` inside a savepoint: released if `f` succeeds, rolled back otherwise. Unlike `write_tx`
/// it nests, so it can be used inside a caller's transaction.
pub(crate) fn in_savepoint<T>(conn: &Connection, name: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch(&format!("SAVEPOINT {};", name))?;
    match f() {
        Ok(v) => { conn.execute_batch(&format!("RELEASE {};", name))?; Ok(v) }
        Err(e) => { let _ = conn.execute_batch(&format!("ROLLBACK TO {0}; RELEASE {0};", name)); Err(e) }
    }
}

/// The regular file `vm_path` resolves to, with its size.
fn regular_file(conn: &Connection, vm_path: &str) -> Result<(String, u64)> {
    let real = resolve_path(conn, vm_path)?;
    let (kind, size): (String, i64) = conn
        .prepare_cached("SELECT kind, size FROM vm_fs WHERE path=?1")?
        .query_row(params![real], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| anyhow!("not found in VM: {}", real))?;
    if kind != "file" { return Err(anyhow!("not a regular file: {}", real)); }
    Ok((real, size as u64))
}

/// Point chunk `idx` of `path` at `piece`, storing the blob if it is new. An unchanged chunk is left
/// alone entirely: deleting its row would fire vm_fc_del and drop the very blob a re-insert needs
/// (dangling reference). Delete then insert, not INSERT OR REPLACE: a replace does not fire the
/// DELETE trigger (recursive_triggers is off), so the old blob's refcount would never drop.
fn put_chunk(conn: &Connection, path: &str, idx: u64, piece: &[u8], cache: Option<&ChunkCache>) -> Result<()> {
    let key = blob_key(piece);
    let old_hash: Option<Vec<u8>> = conn.prepare_cached("SELECT hash FROM vm_file_chunks WHERE path=?1 AND idx=?2")?
        .query_row(params![path, idx as i64], |r| r.get(0)).optional()?;
    if old_hash.as_deref() == Some(key.as_slice()) { return Ok(()); }
    blob_ensure_with(conn, &key, piece, Effort::Interactive)?;
    conn.prepare_cached("DELETE FROM vm_file_chunks WHERE path=?1 AND idx=?2")?
        .execute(params![path, idx as i64])?;
    conn.prepare_cached("INSERT INTO vm_file_chunks(path, idx, hash) VALUES (?1,?2,?3)")?
        .execute(params![path, idx as i64, key.as_slice()])?;
    if let Some(c) = cache { c.put(key, Arc::new(piece.to_vec())); }
    Ok(())
}

/// Record a deferred write: the new size and mtime now, the whole-file hash later (see vm_dirty).
fn mark_dirty(conn: &Connection, path: &str, size: u64, mtime: i64) -> Result<()> {
    conn.prepare_cached("UPDATE vm_fs SET size=?2, mtime=?3 WHERE path=?1")?
        .execute(params![path, size as i64, mtime])?;
    conn.prepare_cached("INSERT OR IGNORE INTO vm_dirty(path) VALUES (?1)")?.execute(params![path])?;
    Ok(())
}

/// Write `data` at `off` in a regular file, rewriting only the CHUNK_SIZE chunks it touches.
///
/// Untouched chunks keep their rows and blobs. A write past the end extends the file and zero-fills
/// any gap (the gap's chunks are written too). The whole-file sha256 is recomputed before returning,
/// so `vm_fs.hash` stays exact. Runs in one savepoint: a failure leaves the file unchanged.
pub fn write_range(conn: &Connection, vm_path: &str, off: u64, data: &[u8], mtime: i64) -> Result<()> {
    in_savepoint(conn, "write_range", || {
        write_range_deferred(conn, vm_path, off, data, mtime, None)?;
        finalize_file(conn, &resolve_path(conn, vm_path)?)?;
        Ok(())
    })
}

/// `write_range` without the whole-file hash: the file is recorded in vm_dirty, and the hash is
/// recomputed by `finalize_dirty` when something reads it.
///
/// For a writer that issues many small writes to one file (a mount). Recomputing the hash on every
/// write reads the whole file each time, so writing a file sequentially cost O(n²) (measured: a
/// 200 MB file did not finish in minutes). `cache`, if given, serves and receives decoded chunks.
pub fn write_range_deferred(conn: &Connection, vm_path: &str, off: u64, data: &[u8], mtime: i64, cache: Option<&ChunkCache>) -> Result<()> {
    let (real, old_size) = regular_file(conn, vm_path)?;
    if data.is_empty() { return Ok(()); }
    let new_size = old_size.max(off + data.len() as u64);
    let end = off + data.len() as u64;
    // Start at the chunk holding min(off, old_size). If the write extends the file, the old last chunk
    // may be short; it must be rewritten to full length (zero-filled) as well, not just the chunks
    // the write itself covers. Otherwise it stays short while the file grows past it.
    let first = off.min(old_size) / CHUNK_SIZE as u64;
    let last = (end - 1) / CHUNK_SIZE as u64;
    in_savepoint(conn, "write_range_deferred", || {
        for idx in first..=last {
            let base = idx * CHUNK_SIZE as u64;
            let chunk_end = (base + CHUNK_SIZE as u64).min(new_size);
            // the old bytes of this chunk (empty past the old end), then overlay the new bytes
            let mut piece = if base < old_size {
                read_range_cached(conn, &real, base, CHUNK_SIZE, cache)?
            } else {
                Vec::new()
            };
            let want = (chunk_end - base) as usize;
            if piece.len() < want { piece.resize(want, 0); }
            let w_from = off.max(base);
            let w_to = end.min(chunk_end);
            if w_from < w_to {
                let src = &data[(w_from - off) as usize..(w_to - off) as usize];
                piece[(w_from - base) as usize..(w_to - base) as usize].copy_from_slice(src);
            }
            put_chunk(conn, &real, idx, &piece, cache)?;
        }
        mark_dirty(conn, &real, new_size, mtime)
    })
}

/// Set a regular file's size. Shrinking drops the chunks past the new end and trims the last one;
/// growing zero-fills (write_range does the work). Whole-file hash and size are updated together.
pub fn truncate_range(conn: &Connection, vm_path: &str, new_size: u64, mtime: i64) -> Result<()> {
    in_savepoint(conn, "truncate_range", || {
        truncate_range_deferred(conn, vm_path, new_size, mtime, None)?;
        finalize_file(conn, &resolve_path(conn, vm_path)?)?;
        Ok(())
    })
}

/// `truncate_range` without the whole-file hash; see `write_range_deferred`.
pub fn truncate_range_deferred(conn: &Connection, vm_path: &str, new_size: u64, mtime: i64, cache: Option<&ChunkCache>) -> Result<()> {
    let (real, old_size) = regular_file(conn, vm_path)?;
    if new_size == old_size { return Ok(()); }
    if new_size > old_size {
        // extend: a zero-filled tail
        let zeros = vec![0u8; (new_size - old_size) as usize];
        return write_range_deferred(conn, &real, old_size, &zeros, mtime, cache);
    }
    in_savepoint(conn, "truncate_range_deferred", || {
        // chunks entirely past the new end are removed (their blob refcounts drop via the trigger)
        let keep_chunks = if new_size == 0 { 0 } else { new_size.div_ceil(CHUNK_SIZE as u64) };
        conn.prepare_cached("DELETE FROM vm_file_chunks WHERE path=?1 AND idx>=?2")?
            .execute(params![real, keep_chunks as i64])?;
        // the last kept chunk may hold bytes past the new end: rewrite it trimmed
        if new_size > 0 && !new_size.is_multiple_of(CHUNK_SIZE as u64) {
            let last = keep_chunks - 1;
            let base = last * CHUNK_SIZE as u64;
            let mut piece = chunk_bytes(conn, &real, last, cache)?;
            piece.truncate((new_size - base) as usize);
            put_chunk(conn, &real, last, &piece, cache)?;
        }
        mark_dirty(conn, &real, new_size, mtime)
    })
}

/// sha256 over a file's content, read chunk by chunk (one chunk in memory at a time). Each chunk is
/// checked against its key and against the length `size` implies, so a short or corrupt chunk is an
/// error, never a wrong hash.
fn whole_file_hash(conn: &Connection, path: &str, size: u64) -> Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for idx in 0..size.div_ceil(CHUNK_SIZE as u64) {
        let piece = chunk_bytes(conn, path, idx, None)?;
        let want = (size - idx * CHUNK_SIZE as u64).min(CHUNK_SIZE as u64) as usize;
        if piece.len() != want { return Err(anyhow!("{}: chunk {} has {} bytes, expected {}", path, idx, piece.len(), want)); }
        h.update(&piece);
    }
    Ok(h.finalize().into())
}

/// Recompute the whole-file hash of `path` if a deferred write left it stale. Returns whether it was.
pub fn finalize_file(conn: &Connection, path: &str) -> Result<bool> {
    let dirty = conn.prepare_cached("SELECT 1 FROM vm_dirty WHERE path=?1")?
        .query_row(params![path], |_| Ok(())).optional()?.is_some();
    if !dirty { return Ok(false); }
    in_savepoint(conn, "finalize_file", || {
        let size: i64 = conn.prepare_cached("SELECT size FROM vm_fs WHERE path=?1")?.query_row(params![path], |r| r.get(0))?;
        let whole = whole_file_hash(conn, path, size as u64)?;
        conn.prepare_cached("UPDATE vm_fs SET hash=?2 WHERE path=?1")?.execute(params![path, whole.as_slice()])?;
        conn.prepare_cached("DELETE FROM vm_dirty WHERE path=?1")?.execute(params![path])?;
        Ok(true)
    })
}

/// Recompute every stale whole-file hash (see vm_dirty). Anything that reads `vm_fs.hash` (snapshots,
/// diffs, the materialize cache key, sync, verify, listings) calls this first, so a deferred write is
/// never observed as an unchanged file. Returns the number of files finalized.
pub fn finalize_dirty(conn: &Connection) -> Result<usize> {
    let paths: Vec<String> = conn.prepare_cached("SELECT path FROM vm_dirty")?
        .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    for p in &paths { if finalize_file(conn, p)? { n += 1; } }
    Ok(n)
}

/// Number of files with a stale hash in attached schema `schema` (0 when it predates vm_dirty).
/// A database attached read-only cannot finalize them, so a caller that copies its hashes refuses it.
pub fn pending_hashes(conn: &Connection, schema: &str) -> Result<i64> {
    let has: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {}.sqlite_master WHERE type='table' AND name='vm_dirty'", schema), [], |r| r.get(0))?;
    if has == 0 { return Ok(0); }
    Ok(conn.query_row(&format!("SELECT count(*) FROM {}.vm_dirty", schema), [], |r| r.get(0))?)
}
