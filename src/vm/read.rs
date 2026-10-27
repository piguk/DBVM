use super::*;

/// One `vm_ls` row. `hash` is the lower-case hex content hash, empty for directories and links.
#[derive(Debug, Clone)]
pub struct LsEntry { pub path: String, pub kind: String, pub mode: i64, pub size: i64, pub hash: String }

fn ls_entry(r: &rusqlite::Row) -> rusqlite::Result<LsEntry> {
    Ok(LsEntry { path: r.get(0)?, kind: r.get(1)?, mode: r.get(2)?, size: r.get(3)?, hash: r.get(4)? })
}

/// The entry at `vm_path` if it exists as a path, otherwise its direct children.
pub fn vm_ls(conn: &Connection, vm_path: &str) -> Result<Vec<LsEntry>> {
    finalize_dirty(conn)?;
    let p = normalize_vm_path(vm_path);
    let cnt: i64 = conn.prepare_cached("SELECT count(*) FROM vm_fs WHERE path=?1")?.query_row( params![p], |r| r.get(0))?;
    if cnt>0 {
        let row = conn.prepare_cached("SELECT path,kind,mode,size,COALESCE(NULLIF(lower(hex(hash)),''),'') FROM vm_fs WHERE path=?1")?.query_row( params![p], ls_entry)?;
        return Ok(vec![row]);
    }
    let like = if p == "/" { "/%".to_string() } else { format!("{}/%", p) };
    let mut st = conn.prepare("SELECT path,kind,mode,size,COALESCE(NULLIF(lower(hex(hash)),''),'') FROM vm_fs WHERE path LIKE ?1 AND path != ?2 ORDER BY path")?;
    let mut direct = Vec::new();
    for r in st.query_map(params![like, p], ls_entry)? {
        let e = r?;
        let rel = if p=="/" { e.path.trim_start_matches('/') } else { e.path.strip_prefix(&format!("{}/", p)).unwrap_or(&e.path) };
        if !rel.contains('/') { direct.push(e); }
    }
    Ok(direct)
}

/// Decoded chunks keyed by content hash. A key names exactly one byte string, so an entry never goes
/// stale: it stays valid across writes, files and databases, and needs no invalidation.
pub struct ChunkCache { inner: Mutex<LruCache<BlobKey, Arc<Vec<u8>>>> }

impl ChunkCache {
    /// A cache holding up to `max_chunks` chunks (each at most CHUNK_SIZE bytes).
    pub fn new(max_chunks: usize) -> Self {
        ChunkCache { inner: Mutex::new(LruCache::new(NonZeroUsize::new(max_chunks.max(1)).unwrap())) }
    }
    fn get(&self, key: &BlobKey) -> Option<Arc<Vec<u8>>> { self.inner.lock().unwrap().get(key).cloned() }
    pub(super) fn put(&self, key: BlobKey, piece: Arc<Vec<u8>>) { self.inner.lock().unwrap().put(key, piece); }
}

/// Chunk `idx` of `path`, decoded and checked against its key, so a corrupt or mis-keyed chunk is an
/// error, never silently wrong data. Served from `cache` when it holds the key.
pub(super) fn chunk_bytes(conn: &Connection, path: &str, idx: u64, cache: Option<&ChunkCache>) -> Result<Vec<u8>> {
    let key: Vec<u8> = conn.prepare_cached("SELECT hash FROM vm_file_chunks WHERE path=?1 AND idx=?2")?
        .query_row(params![path, idx as i64], |r| r.get(0)).optional()?
        .ok_or_else(|| anyhow!("{}: missing chunk {}", path, idx))?;
    Ok(chunk_by_key(conn, path, idx, &key, cache)?.to_vec())
}

fn chunk_by_key(conn: &Connection, path: &str, idx: u64, key: &[u8], cache: Option<&ChunkCache>) -> Result<Arc<Vec<u8>>> {
    let key: BlobKey = key.try_into().map_err(|_| anyhow!("{}: chunk {} has a malformed key", path, idx))?;
    if let Some(hit) = cache.and_then(|c| c.get(&key)) { return Ok(hit); }
    let (body, comp, dict_id): (Vec<u8>, i64, i64) = conn
        .prepare_cached("SELECT content, compressed, dict_id FROM vm_blobs WHERE hash=?1")?
        .query_row(params![key.as_slice()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?
        .ok_or_else(|| anyhow!("{}: chunk {} references a missing blob", path, idx))?;
    let piece = decode_block(conn, &body, comp, dict_id).map_err(|e| anyhow!("{}: chunk {} undecodable: {}", path, idx, e))?;
    if blob_key(&piece) != key { return Err(anyhow!("{}: chunk {} hash mismatch", path, idx)); }
    let piece = Arc::new(piece);
    if let Some(c) = cache { c.put(key, piece.clone()); }
    Ok(piece)
}

/// Unified file-range reader. Every content read goes through here.
/// Only the chunks overlapping [off, off+len) are fetched and verified.
/// Returns bytes clamped to the file size.
pub fn read_range(conn: &Connection, vm_path: &str, off: u64, len: usize) -> Result<Vec<u8>> {
    read_range_cached(conn, vm_path, off, len, None)
}

/// `read_range` that serves decoded chunks from `cache` and adds the ones it decodes. For a reader that
/// reads one file in many small requests (a mount): without it, every request decoded and verified a
/// whole chunk again.
pub fn read_range_cached(conn: &Connection, vm_path: &str, off: u64, len: usize, cache: Option<&ChunkCache>) -> Result<Vec<u8>> {
    in_read_snapshot(conn, || read_range_in_snapshot(conn, vm_path, off, len, cache))
}

fn read_range_in_snapshot(conn: &Connection, vm_path: &str, off: u64, len: usize, cache: Option<&ChunkCache>) -> Result<Vec<u8>> {
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
    let first = start / CHUNK_SIZE as u64;
    let last = (end - 1) / CHUNK_SIZE as u64;
    let keys: Vec<(i64, Vec<u8>)> = conn.prepare_cached(
        "SELECT idx, hash FROM vm_file_chunks WHERE path=?1 AND idx BETWEEN ?2 AND ?3 ORDER BY idx")?
        .query_map(params![real, first as i64, last as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out: Vec<u8> = Vec::with_capacity((end - start) as usize);
    for (expect, (idx, key)) in (first as i64..).zip(&keys) {
        if *idx != expect { return Err(anyhow!("{}: missing chunk {} for requested range", real, expect)); }
        let piece = chunk_by_key(conn, &real, *idx as u64, key, cache)?;
        // slice this chunk to the requested window
        let chunk_base = *idx as u64 * CHUNK_SIZE as u64;
        let from = start.saturating_sub(chunk_base).min(piece.len() as u64) as usize;
        let to = end.saturating_sub(chunk_base).min(piece.len() as u64) as usize;
        if from < to { out.extend_from_slice(&piece[from..to]); }
    }
    if keys.len() as u64 != last - first + 1 { return Err(anyhow!("{}: missing chunk {} for requested range", real, last)); }
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
    let r = vm_resolve(conn, vm_path)?;
    if r.kind == "dir" { return Err(anyhow!("is a directory: {}", vm_path)); }
    r.content.ok_or_else(|| anyhow!("no content for {} (resolved {})", vm_path, r.path))
}

pub fn vm_stat(conn: &Connection, vm_path: &str) -> Result<(String,String,i64,i64,i64,String)> {
    finalize_dirty(conn)?;
    let p = normalize_vm_path(vm_path);
    let row = conn.prepare_cached("SELECT path,kind,mode,size,mtime,link_target,content,NULLIF(lower(hex(hash)),'') FROM vm_fs WHERE path=?1")?
        .query_row(params![p], |r| Ok((
            r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?, r.get::<_,i64>(4)?,
            r.get::<_,Option<String>>(5)?, r.get::<_,Option<Vec<u8>>>(6)?, r.get::<_,Option<String>>(7)?))).optional()?;
    let (path,kind,mode,size,mtime,link,content,hash_opt) = match row { Some(x)=>x, None=> return Err(anyhow!("not found: {}", p)) };
    if kind == "symlink" {
        let target = link.clone().unwrap_or_default();
        if let Ok(Resolved { content: Some(d), .. }) = vm_resolve(conn, &p) {
            return Ok((path,kind,mode,d.len() as i64,mtime, format!("-> {} {}", target, fx_hash_u64(&d))));
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
    in_read_snapshot(conn, || file_bytes_in_snapshot(conn, path))
}

/// Run a multi-statement read against one snapshot of the database. In autocommit mode every statement
/// sees the latest commit, so a reader that took a file's size in one statement and its chunks in the
/// next, beside a writer, combined two versions (measured: "chunks hold 150000 bytes, vm_fs.size is
/// 3000"). Inside a caller's transaction the reads already share its snapshot.
fn in_read_snapshot<T>(conn: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if !conn.is_autocommit() { return f(); }
    conn.execute_batch("BEGIN DEFERRED;")?;
    let r = f();
    // nothing was written: ending the transaction only releases the snapshot
    let _ = conn.execute_batch("COMMIT;");
    r
}

fn file_bytes_in_snapshot(conn: &Connection, path: &str) -> Result<Vec<u8>> {
    // The chunk list alone cannot tell a missing file from an empty one: both have no rows. The size
    // recorded in vm_fs decides, so a path absent from this database is an error, never empty content.
    let size: i64 = conn.prepare_cached("SELECT size FROM vm_fs WHERE path=?1 AND kind='file'")?
        .query_row(params![path], |r| r.get(0)).optional()?
        .ok_or_else(|| anyhow!("not a regular file in this database: {}", path))?;
    let mut st = conn.prepare_cached("SELECT c.idx, b.content, b.compressed, c.hash, b.dict_id FROM vm_file_chunks c JOIN vm_blobs b ON b.hash=c.hash WHERE c.path=?1 ORDER BY c.idx")?;
    let rows = st.query_map(params![path], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,i64>(2)?, r.get::<_,Vec<u8>>(3)?, r.get::<_,i64>(4)?)))?;
    let mut out = Vec::new();
    for (expect, row) in (0i64..).zip(rows) {
        let (idx, body, comp, key, dict_id) = row?;
        if idx != expect { return Err(anyhow!("{}: chunk index gap at {}", path, expect)); }
        let piece = decode_block(conn, &body, comp, dict_id).map_err(|e| anyhow!("{}: chunk {} undecodable: {}", path, idx, e))?;
        if blob_key(&piece).as_slice() != key.as_slice() { return Err(anyhow!("{}: chunk {} hash mismatch", path, idx)); }
        out.extend_from_slice(&piece);
    }
    if out.len() as i64 != size { return Err(anyhow!("{}: chunks hold {} bytes, vm_fs.size is {}", path, out.len(), size)); }
    Ok(out)
}

/// Where a path ends up after following symlinks.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The final path, after every symlink hop.
    pub path: String,
    /// `file` or `dir`.
    pub kind: String,
    /// File content; `None` for a directory.
    pub content: Option<Vec<u8>>,
    pub mode: i64,
}

/// Resolve a path, following symlinks.
/// Regular-file content comes from vm_file_chunks only; a file with a hash but no chunks is
/// an error (the v4->v5 migration gives every such file a chunk row).
pub fn vm_resolve(conn: &Connection, vm_path: &str) -> Result<Resolved> {
    in_read_snapshot(conn, || resolve_in_snapshot(conn, vm_path))
}

fn resolve_in_snapshot(conn: &Connection, vm_path: &str) -> Result<Resolved> {
    let mut cur = normalize_vm_path(vm_path);
    let mut visited = std::collections::HashSet::new();
    for _ in 0..40 {
        if !visited.insert(cur.clone()) { return Err(anyhow!("symlink loop at {}", cur)); }
        let row = conn.prepare_cached("SELECT kind, link_target, mode, hash, content, compressed FROM vm_fs WHERE path=?1")?
            .query_row(params![cur], |r| Ok((
                r.get::<_,String>(0)?, r.get::<_,Option<String>>(1)?, r.get::<_,i64>(2)?,
                r.get::<_,Option<Vec<u8>>>(3)?, r.get::<_,Option<Vec<u8>>>(4)?, r.get::<_,Option<i64>>(5)?.unwrap_or(0)))).optional()?;
        let (kind, link, mode, hash, inline, comp) = match row { Some(x)=>x, None=> return Err(anyhow!("not found in VM: {}", cur)) };
        if kind != "symlink" {
            if kind == "dir" { return Ok(Resolved { path: cur, kind, content: None, mode }); }
            let content = if hash.is_some() {
                file_bytes_in_snapshot(conn, &cur)?
            } else if let Some(b) = inline {
                // legacy inline row (pre-blob); never produced by current code
                decompress_bytes_raw(&b, comp).map_err(|e| anyhow!("corrupt inline content for {}: {}", cur, e))?
            } else {
                // a regular file must have either a hash (chunked) or legacy inline bytes.
                // Neither means the row is corrupt: fail instead of returning an empty file.
                return Err(anyhow!("corrupt row for {}: file without content", cur));
            };
            return Ok(Resolved { path: cur, kind, content: Some(content), mode });
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
    let Resolved { path: real, kind, content, mode } = vm_resolve(conn, vm_path)?;
    if kind == "dir" { std::fs::create_dir_all(host_tmp)?; return Ok(host_tmp.to_path_buf()); }
    if let Some(data) = content {
        if let Some(parent) = host_tmp.parent() { std::fs::create_dir_all(parent)?; }
        let hash = fx_hash_u64(&data);
        let cache_p = cache_path_for_hash(&hash);
        if cache_copy_out(&cache_p, data.len() as u64, host_tmp) {
            #[cfg(unix)] { let _ = std::fs::set_permissions(host_tmp, std::fs::Permissions::from_mode(mode as u32)); }
            let _ = real;
            return Ok(host_tmp.to_path_buf());
        }
        cache_put(&cache_p, &data);
        std::fs::write(host_tmp, &data)?;
        #[cfg(unix)] { let _ = std::fs::set_permissions(host_tmp, std::fs::Permissions::from_mode(mode as u32)); }
        let _ = real;
        return Ok(host_tmp.to_path_buf());
    }
    Err(anyhow!("no content for {} (resolved {})", vm_path, real))
}
