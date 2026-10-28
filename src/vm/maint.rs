use super::*;

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
        for x in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,Option<i64>>(2)?)))?.flatten() { rows.push(x); }
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for (path, data, comp) in rows {
        let cflag = comp.unwrap_or(0);
        let raw = decompress_legacy(&data, cflag)?;
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

/// True when a readable `base` schema with a vm_fs table is attached (an Overlay Delta).
fn base_has_fs(conn: &Connection) -> bool {
    conn.query_row("SELECT count(*) FROM pragma_database_list WHERE name='base'", [], |r| r.get::<_, i64>(0))
        .map(|n| n > 0).unwrap_or(false)
        && conn.query_row("SELECT count(*) FROM base.sqlite_master WHERE type='table' AND name='vm_fs'", [], |r| r.get::<_, i64>(0))
            .map(|n| n > 0).unwrap_or(false)
}

/// Quick check uses counters; `deep` recomputes every counter and refcount from raw rows
/// and reports any drift (use after crashes or manual SQL edits).
pub fn vm_verify(conn: &Connection, deep: bool) -> Result<String> {
    // a hash left stale by a deferred write is pending work, not drift; a failure here is reported
    finalize_dirty(conn)?;
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
        // refcnt must equal live chunk references plus snapshot (v8) and undo record (v13) chunk references
        let bad_rc: i64 = conn.prepare_cached(
            "SELECT count(*) FROM vm_blobs b WHERE b.refcnt != (
               (SELECT count(*) FROM vm_file_chunks c WHERE c.hash=b.hash) +
               (SELECT count(*) FROM vm_snapshot_chunks s WHERE s.hash=b.hash) +
               (SELECT count(*) FROM vm_undo_chunks u WHERE u.hash=b.hash))")?.query_row( [], |r| r.get(0))?;
        if bad_rc > 0 { drift.push(format!("{} blobs with bad refcnt", bad_rc)); }
        // every chunk, live or in a snapshot, must point at an existing blob
        let orphan: i64 = conn.prepare_cached("SELECT count(*) FROM vm_file_chunks c WHERE NOT EXISTS (SELECT 1 FROM vm_blobs b WHERE b.hash=c.hash)")?.query_row( [], |r| r.get(0))?;
        let orphan_s: i64 = conn.prepare_cached("SELECT count(*) FROM vm_snapshot_chunks c WHERE NOT EXISTS (SELECT 1 FROM vm_blobs b WHERE b.hash=c.hash)")?.query_row( [], |r| r.get(0))?;
        let orphan_u: i64 = conn.prepare_cached("SELECT count(*) FROM vm_undo_chunks c WHERE NOT EXISTS (SELECT 1 FROM vm_blobs b WHERE b.hash=c.hash)")?.query_row( [], |r| r.get(0))?;
        if orphan + orphan_s + orphan_u > 0 { drift.push(format!("{} chunks reference missing blobs", orphan + orphan_s + orphan_u)); }
        // a snapshot's chunk list must be contiguous per file, like the live list
        let snap_gaps: i64 = conn.prepare_cached("SELECT count(*) FROM (SELECT snap_id, path, count(*) AS n, max(idx) AS mx FROM vm_snapshot_chunks GROUP BY snap_id, path) WHERE mx + 1 != n")?.query_row( [], |r| r.get(0))?;
        if snap_gaps > 0 { drift.push(format!("{} snapshot files with chunk index gaps", snap_gaps)); }
        // Chunks must form a contiguous 0..n sequence, and every non-empty file must have chunks — for a
        // self-contained database. A Delta with a readable Base attached is a different case: it stores
        // only the chunks a write touched, and the rest of each file lives in Base. There the check is
        // that Delta's own chunks exist and each file's total (Delta + Base) covers its size.
        let has_base = base_has_fs(conn);
        if !has_base {
            let gaps: i64 = conn.prepare_cached("SELECT count(*) FROM (SELECT path, count(*) AS n, max(idx) AS mx FROM vm_file_chunks GROUP BY path) WHERE mx + 1 != n")?.query_row( [], |r| r.get(0))?;
            if gaps > 0 { drift.push(format!("{} files with chunk index gaps", gaps)); }
            let no_chunks: i64 = conn.prepare_cached("SELECT count(*) FROM vm_fs f WHERE f.kind='file' AND f.size>0 AND NOT EXISTS (SELECT 1 FROM vm_file_chunks c WHERE c.path=f.path)")?.query_row( [], |r| r.get(0))?;
            if no_chunks > 0 { drift.push(format!("{} non-empty files without chunks", no_chunks)); }
        } else {
            // a COW row must not claim bytes neither layer holds
            let uncovered: i64 = conn.prepare_cached(
                "SELECT count(*) FROM vm_fs f WHERE f.kind='file' AND f.size>0 AND NOT EXISTS (
                   SELECT 1 FROM main.vm_file_chunks c WHERE c.path=f.path
                   UNION ALL
                   SELECT 1 FROM base.vm_file_chunks c WHERE c.path=f.path)")?.query_row( [], |r| r.get(0))?;
            if uncovered > 0 { drift.push(format!("{} Delta files with no chunks in either layer", uncovered)); }
            let short: i64 = conn.prepare_cached(
                "SELECT count(*) FROM (
                   SELECT f.path, f.size AS sz,
                     (SELECT COALESCE(sum(b.raw_size),0) FROM main.vm_file_chunks c JOIN main.vm_blobs b ON b.hash=c.hash WHERE c.path=f.path) AS d,
                     (SELECT COALESCE(sum(b.raw_size),0) FROM base.vm_file_chunks c JOIN base.vm_blobs b ON b.hash=c.hash WHERE c.path=f.path) AS b
                   FROM main.vm_fs f WHERE f.kind='file') WHERE d + b < sz")?.query_row( [], |r| r.get(0))?;
            if short > 0 { drift.push(format!("{} Delta files whose chunks do not cover their size", short)); }
        }
        out.push_str(if drift.is_empty() { " deep=ok" } else { " deep=DRIFT" });
        for d in drift { out.push_str(&format!("\n  {}", d)); }
    }
    Ok(out)
}
