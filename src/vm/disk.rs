use super::*;

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
