use super::*;

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

/// Dictionary `id` of the main database (the common case).
fn dict_by_id(conn: &Connection, id: i64) -> Result<Arc<Vec<u8>>> {
    dict_by_id_in(conn, "main", id)
}

/// Dictionary `id` as stored in attached schema `schema` (`main`, or e.g. `base`). Dictionary ids
/// are per database, so the cache key is the schema's file path plus the id, never the id alone.
/// Dictionary rows are immutable, so a cached entry never goes stale.
fn dict_by_id_in(conn: &Connection, schema: &str, id: i64) -> Result<Arc<Vec<u8>>> {
    let file = schema_file(conn, schema)?;
    let key = (file, id);
    if let Some(d) = dict_cache().lock().unwrap().get(&key) { return Ok(d.clone()); }
    let sql = format!("SELECT dict FROM {}.vm_dicts WHERE id=?1", schema);
    let bytes: Vec<u8> = conn.prepare(&sql)?
        .query_row(params![id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| anyhow!("missing dictionary {} (block written with it cannot be read)", id))?;
    let d = Arc::new(bytes);
    dict_cache().lock().unwrap().put(key, d.clone());
    Ok(d)
}

/// Decoded dictionaries keyed by (database file, dict_id).
type DictCache = Mutex<LruCache<(String, i64), Arc<Vec<u8>>>>;

fn dict_cache() -> &'static DictCache {
    static C: OnceLock<DictCache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(LruCache::new(NonZeroUsize::new(16).unwrap())))
}

/// File path of an attached schema (`main` included), used as the identity of its dictionaries.
fn schema_file(conn: &Connection, schema: &str) -> Result<String> {
    let sql = "SELECT file FROM pragma_database_list WHERE name=?1";
    let f: Option<String> = conn.query_row(sql, params![schema], |r| r.get(0)).optional()?;
    f.filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("schema {} has no file", schema))
}

/// Decode a block that lives in attached schema `schema`, using that schema's dictionary.
pub fn decode_block_in(conn: &Connection, schema: &str, body: &[u8], flag: i64, dict_id: i64) -> Result<Vec<u8>> {
    if flag == 3 {
        let d = dict_by_id_in(conn, schema, dict_id)?;
        return decompress_bytes_zstd_with_dict(body, &d);
    }
    decompress_bytes_raw(body, flag)
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
    let max = max_dict_size.clamp(4*1024, 112*1024);
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

/// How hard a write tries to compress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    /// Smallest result: imports and packs, written once. Large blocks are also tried at level 19.
    Archive,
    /// Level 3 only: writes through a mount, where every write() re-encodes the chunks it touches.
    /// Archive effort cost 2.4 ms per 64 KiB chunk (measured), most of it the level-19 pass;
    /// `vm-recompress` raises such blobs to level 19 later.
    Interactive,
}

/// Pure encoding decision (no DB access, safe to run on worker threads). Returns
/// (stored bytes, flag, dict_id); None means store raw. The serial and parallel write paths
/// both call this, so they cannot diverge.
pub fn compress_with_dict(data: &[u8], dict: Option<&(i64, Arc<Vec<u8>>)>) -> Option<(Vec<u8>, i64, i64)> {
    compress_with_effort(data, dict, Effort::Archive)
}

/// `compress_with_dict` at a chosen effort.
pub fn compress_with_effort(data: &[u8], dict: Option<&(i64, Arc<Vec<u8>>)>, effort: Effort) -> Option<(Vec<u8>, i64, i64)> {
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
    let level = match effort {
        Effort::Interactive => 3,
        Effort::Archive => if data.len() > 100*1024 { 19 } else if data.len() > 16*1024 { 6 } else { 3 },
    };
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
    if data.len() > 16384 && effort == Effort::Archive {
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
    // flag 0: stored as it is. A file that is itself gzip or zstd (`.gz`, `.zst`, which do not compress
    // further) is stored this way, and must not be decompressed on the way out.
    Ok(data.to_vec())
}

/// Decode content written before flags were reliable (inline rows, blobs before schema 4): flag 0 may
/// still hold gzip or zstd, told by its magic bytes. Only for those legacy rows.
// TODO: remove with the schema-4 migration and the legacy inline-content paths.
pub fn decompress_legacy(data: &[u8], compressed: i64) -> Result<Vec<u8>> {
    if compressed == 0 {
        if data.len() >= 4 && data[0]==0x28 && data[1]==0xb5 && data[2]==0x2f && data[3]==0xfd {
            if let Ok(v) = decompress_bytes_zstd(data) { return Ok(v); }
        }
        if data.len() >= 2 && data[0]==0x1f && data[1]==0x8b {
            let mut d = flate2::read::GzDecoder::new(data);
            let mut out = Vec::new();
            if d.read_to_end(&mut out).is_ok() { return Ok(out); }
        }
    }
    decompress_bytes_raw(data, compressed)
}
