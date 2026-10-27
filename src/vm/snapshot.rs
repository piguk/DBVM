use super::*;

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
    for (ts,op,path,detail) in st.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?, r.get::<_,String>(3)?)))?.flatten() { out.push_str(&format!("  {} {} {} {}\n", ts, op, path, detail)); }
    Ok(out)
}

/// Differences between a snapshot and the live tree, by path.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SnapDiff { pub created: Vec<String>, pub modified: Vec<String>, pub deleted: Vec<String> }

fn snapshot_id(conn: &Connection, name: &str) -> Result<i64> {
    conn.prepare_cached("SELECT id FROM vm_snapshots WHERE name=?1")?
        .query_row(params![name], |r| r.get(0))
        .optional()?
        .ok_or_else(|| anyhow!("no such snapshot: {}", name))
}

/// Capture the whole live tree as a named content snapshot. Copies metadata and chunk
/// references only, so no blob is duplicated. One transaction: all or nothing.
/// Create a snapshot whose name is derived from its row id: `<prefix>-<id>`.
///
/// The name must not be computed by the caller from `MAX(id)+1`. Two callers computing it at the same
/// time pick the same number and the second insert fails on the UNIQUE name. Here the row is inserted
/// under a unique placeholder, its id is read back, and the name is set from that id, all inside one
/// savepoint. Returns (id, name).
pub fn snapshot_create_named(conn: &Connection, prefix: &str, note: &str) -> Result<(i64, String)> {
    // a snapshot records vm_fs.hash; a deferred write must not be recorded as the old content
    finalize_dirty(conn)?;
    conn.execute_batch("SAVEPOINT snap_named;")?;
    let res: Result<(i64, String)> = (|| {
        // a placeholder that no other row can take, even if two callers insert at once
        conn.prepare_cached("INSERT INTO vm_snapshots(name, page_count, page_size, bytes, note) VALUES (?1, 0, 0, 0, ?2)")?
            .execute(params![format!("{}", uuid_like()), note])?;
        let id = conn.last_insert_rowid();
        let name = format!("{}-{}", prefix, id);
        conn.prepare_cached("UPDATE vm_snapshots SET name=?1 WHERE id=?2")?.execute(params![name, id])?;
        conn.prepare_cached(
            "INSERT INTO vm_snapshot_fs(snap_id, path, kind, mode, mtime, mtime_ns, size, link_target, hash)
             SELECT ?1, path, kind, mode, mtime, mtime_ns, size, link_target, hash FROM vm_fs")?
            .execute(params![id])?;
        conn.prepare_cached("INSERT INTO vm_snapshot_chunks(snap_id, path, idx, hash)
             SELECT ?1, path, idx, hash FROM vm_file_chunks")?
            .execute(params![id])?;
        conn.prepare_cached("INSERT INTO vm_log(op, path, detail) VALUES ('snapshot', ?1, 'content')")?
            .execute(params![name])?;
        Ok((id, name))
    })();
    match res {
        Ok(v) => { conn.execute_batch("RELEASE snap_named;")?; Ok(v) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK TO snap_named; RELEASE snap_named;"); Err(e) }
    }
}

/// A name no other caller will choose: process id, a counter, and the time in nanoseconds.
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("pending-{}-{}-{}", std::process::id(), n, t)
}

pub fn snapshot_create(conn: &Connection, name: &str, note: &str) -> Result<i64> {
    // a snapshot records vm_fs.hash; a deferred write must not be recorded as the old content
    finalize_dirty(conn)?;
    conn.execute_batch("SAVEPOINT snap_create;")?;
    let res: Result<i64> = (|| {
        conn.prepare_cached("INSERT INTO vm_snapshots(name, page_count, page_size, bytes, note) VALUES (?1, 0, 0, 0, ?2)")?
            .execute(params![name, note])?;
        let id = conn.last_insert_rowid();
        conn.prepare_cached(
            "INSERT INTO vm_snapshot_fs(snap_id, path, kind, mode, mtime, mtime_ns, size, link_target, hash)
             SELECT ?1, path, kind, mode, mtime, mtime_ns, size, link_target, hash FROM vm_fs")?
            .execute(params![id])?;
        conn.prepare_cached("INSERT INTO vm_snapshot_chunks(snap_id, path, idx, hash)
             SELECT ?1, path, idx, hash FROM vm_file_chunks")?
            .execute(params![id])?;
        conn.prepare_cached("INSERT INTO vm_log(op, path, detail) VALUES ('snapshot', ?1, 'content')")?
            .execute(params![name])?;
        Ok(id)
    })();
    match res {
        Ok(id) => { conn.execute_batch("RELEASE snap_create;")?; Ok(id) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK TO snap_create; RELEASE snap_create;"); Err(e) }
    }
}

/// Replace the live tree with a snapshot's tree. The snapshot keeps its chunk references
/// while the live tree is cleared, so no blob it needs is freed mid-restore.
pub fn snapshot_restore(conn: &Connection, name: &str) -> Result<()> {
    // A rollback must be able to undo damage to protected paths, so it runs under the policy bypass.
    // The guard lives for the whole restore; an early error leaves the bypass on the connection only
    // until the guard drops, and the savepoint rollback below restores the rows.
    let bypass = crate::policy::register(conn)?;
    let _guard = bypass.enter();
    let id = snapshot_id(conn, name)?;
    conn.execute_batch("SAVEPOINT snap_restore;")?;
    let res: Result<()> = (|| {
        // remove the live chunk references first (triggers release their blobs' refcounts),
        // then the rows; the snapshot's own references keep every needed blob alive
        conn.execute_batch("DELETE FROM vm_file_chunks;")?;
        conn.execute_batch("DELETE FROM vm_fs;")?;
        conn.prepare_cached(
            "INSERT INTO vm_fs(path, kind, mode, mtime, mtime_ns, size, link_target, hash, content, compressed)
             SELECT path, kind, mode, mtime, mtime_ns, size, link_target, hash, NULL, 0
             FROM vm_snapshot_fs WHERE snap_id=?1 ORDER BY length(path), path")?
            .execute(params![id])?;
        conn.prepare_cached("INSERT INTO vm_file_chunks(path, idx, hash)
             SELECT path, idx, hash FROM vm_snapshot_chunks WHERE snap_id=?1")?
            .execute(params![id])?;
        conn.prepare_cached("INSERT INTO vm_log(op, path, detail) VALUES ('restore', ?1, 'content')")?
            .execute(params![name])?;
        Ok(())
    })();
    match res {
        Ok(()) => { conn.execute_batch("RELEASE snap_restore;")?; Ok(()) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK TO snap_restore; RELEASE snap_restore;"); Err(e) }
    }
}

/// Delete a content snapshot. Its chunk rows go with it (ON DELETE CASCADE), and the
/// refcount triggers release each blob it referenced. A blob still held by the live tree or by
/// another snapshot keeps its row.
pub fn snapshot_delete(conn: &Connection, name: &str) -> Result<()> {
    let id = snapshot_id(conn, name)?;
    conn.execute_batch("SAVEPOINT snap_delete;")?;
    let res: Result<()> = (|| {
        conn.prepare_cached("DELETE FROM vm_snapshots WHERE id=?1")?.execute(params![id])?;
        conn.prepare_cached("INSERT INTO vm_log(op, path, detail) VALUES ('snapshot_delete', ?1, 'content')")?
            .execute(params![name])?;
        Ok(())
    })();
    match res {
        Ok(()) => { conn.execute_batch("RELEASE snap_delete;")?; Ok(()) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK TO snap_delete; RELEASE snap_delete;"); Err(e) }
    }
}

/// Keep the newest `keep` snapshots (by creation order) and delete the rest. Returns the names
/// removed, oldest first, so the caller can report exactly what was deleted.
pub fn snapshot_retain(conn: &Connection, keep: usize) -> Result<Vec<String>> {
    let names: Vec<String> = conn.prepare_cached("SELECT name FROM vm_snapshots ORDER BY id")?
        .query_map([], |r| r.get(0))?.map(|x| x.unwrap()).collect();
    let drop_n = names.len().saturating_sub(keep);
    let mut removed = Vec::with_capacity(drop_n);
    for name in names.into_iter().take(drop_n) {
        snapshot_delete(conn, &name)?;
        removed.push(name);
    }
    Ok(removed)
}

/// Compare the live tree with a snapshot. A path counts as modified when its kind, content hash,
/// mode, or link target differs; deleted paths exist only in the snapshot.
pub fn snapshot_diff_live(conn: &Connection, name: &str) -> Result<SnapDiff> {
    // modified = hash changed, so stale hashes would hide deferred writes
    finalize_dirty(conn)?;
    let id = snapshot_id(conn, name)?;
    let mut d = SnapDiff::default();
    {
        let mut st = conn.prepare_cached(
            "SELECT cur.path,
                    CASE WHEN old.path IS NULL THEN 1 ELSE 0 END AS is_new,
                    CASE WHEN old.path IS NOT NULL AND (old.kind IS NOT cur.kind OR old.hash IS NOT cur.hash
                         OR old.mode IS NOT cur.mode OR old.link_target IS NOT cur.link_target) THEN 1 ELSE 0 END AS is_mod
             FROM vm_fs cur LEFT JOIN vm_snapshot_fs old ON old.snap_id = ?1 AND old.path = cur.path
             WHERE old.path IS NULL OR old.kind IS NOT cur.kind OR old.hash IS NOT cur.hash
                   OR old.mode IS NOT cur.mode OR old.link_target IS NOT cur.link_target
             ORDER BY cur.path")?;
        for r in st.query_map(params![id], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?)))? {
            let (p, is_new, is_mod) = r?;
            if is_new == 1 { d.created.push(p) } else if is_mod == 1 { d.modified.push(p) }
        }
    }
    {
        let mut st = conn.prepare_cached(
            "SELECT old.path FROM vm_snapshot_fs old
             WHERE old.snap_id = ?1 AND NOT EXISTS (SELECT 1 FROM vm_fs cur WHERE cur.path = old.path) ORDER BY old.path")?;
        for r in st.query_map(params![id], |r| r.get::<_,String>(0))? { d.deleted.push(r?); }
    }
    Ok(d)
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

/// One `vm_snapshots` row. `note` is empty when none was given.
#[derive(Debug, Clone)]
pub struct SnapshotInfo { pub id: i64, pub name: String, pub created_at: i64, pub page_count: i64, pub bytes: i64, pub note: String }

pub fn vm_list_snapshots(conn: &Connection) -> Result<Vec<SnapshotInfo>> {
    let mut st = conn.prepare("SELECT id, name, created_at, page_count, bytes, coalesce(note,'') FROM vm_snapshots ORDER BY id")?;
    let rows = st.query_map([], |r| Ok(SnapshotInfo {
        id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)?, page_count: r.get(3)?, bytes: r.get(4)?, note: r.get(5)?,
    }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
