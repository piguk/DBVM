use super::*;

/// Undo records: the rows a step changed, as they were before it.
///
/// A content snapshot copies every row of the tree, so taking one costs time in proportion to the whole
/// tree: 330 ms at 30k entries, and with the full sync and diff around it a session command cost 1.25 s at
/// 49k entries whatever it changed (measured). An undo record holds only the paths a step touched, captured
/// just before they are written. Applying it restores the state before the step, and its rows against the
/// live rows are the step's change list.
pub(super) const VM_UNDO_SQL: &str = "
CREATE TABLE IF NOT EXISTS vm_undo(
    id INTEGER PRIMARY KEY,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    note TEXT
);
CREATE TABLE IF NOT EXISTS vm_undo_fs(
    undo_id INTEGER NOT NULL REFERENCES vm_undo(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    existed INTEGER NOT NULL,
    kind TEXT,
    mode INTEGER,
    mtime INTEGER,
    mtime_ns INTEGER,
    size INTEGER,
    link_target TEXT,
    hash BLOB,
    PRIMARY KEY(undo_id, path)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS vm_undo_chunks(
    undo_id INTEGER NOT NULL REFERENCES vm_undo(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    idx INTEGER NOT NULL,
    hash BLOB NOT NULL,
    PRIMARY KEY(undo_id, path, idx)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_vm_undo_chunks_hash ON vm_undo_chunks(hash);
CREATE TRIGGER IF NOT EXISTS vm_unc_ins AFTER INSERT ON vm_undo_chunks
BEGIN UPDATE vm_blobs SET refcnt=refcnt+1 WHERE hash=NEW.hash; END;
CREATE TRIGGER IF NOT EXISTS vm_unc_del AFTER DELETE ON vm_undo_chunks
BEGIN
  UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=OLD.hash;
  DELETE FROM vm_blobs WHERE hash=OLD.hash AND refcnt<=0;
END;
";

/// Open a new, empty undo record.
pub fn undo_begin(conn: &Connection, note: &str) -> Result<i64> {
    conn.prepare_cached("INSERT INTO vm_undo(note) VALUES (?1)")?.execute(params![note])?;
    Ok(conn.last_insert_rowid())
}

/// Record `path` as it is now, before it is written. Only the first capture of a path in a record counts:
/// that is its state before the step.
pub fn undo_capture(conn: &Connection, undo: i64, path: &str) -> Result<()> {
    let seen = conn.prepare_cached("SELECT 1 FROM vm_undo_fs WHERE undo_id=?1 AND path=?2")?
        .query_row(params![undo, path], |_| Ok(())).optional()?.is_some();
    if seen { return Ok(()); }
    let n = conn.prepare_cached(
        "INSERT INTO vm_undo_fs(undo_id, path, existed, kind, mode, mtime, mtime_ns, size, link_target, hash)
         SELECT ?1, path, 1, kind, mode, mtime, mtime_ns, size, link_target, hash FROM vm_fs WHERE path=?2")?
        .execute(params![undo, path])?;
    if n == 0 {
        conn.prepare_cached("INSERT INTO vm_undo_fs(undo_id, path, existed) VALUES (?1, ?2, 0)")?.execute(params![undo, path])?;
    } else {
        conn.prepare_cached("INSERT INTO vm_undo_chunks(undo_id, path, idx, hash) SELECT ?1, path, idx, hash FROM vm_file_chunks WHERE path=?2")?
            .execute(params![undo, path])?;
    }
    Ok(())
}

/// Record `path` and everything below it, before they are deleted.
pub fn undo_capture_tree(conn: &Connection, undo: i64, path: &str) -> Result<()> {
    let (lo, hi) = crate::vm::below_bounds(path);
    let below: Vec<String> = conn.prepare_cached("SELECT path FROM vm_fs WHERE path=?1 OR (path > ?2 AND path < ?3)")?
        .query_map(params![path, lo, hi], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for p in below { undo_capture(conn, undo, &p)?; }
    Ok(())
}

/// The change list of a step: its undo record against the live rows. A path that was written but ended up
/// as it was (same kind, content, mode and link target) is not a change.
pub fn undo_diff(conn: &Connection, undo: i64) -> Result<SnapDiff> {
    let mut d = SnapDiff::default();
    let mut st = conn.prepare_cached(
        "SELECT u.path, u.existed, cur.path IS NOT NULL,
                u.kind IS NOT cur.kind OR u.hash IS NOT cur.hash OR u.mode IS NOT cur.mode OR u.link_target IS NOT cur.link_target
         FROM vm_undo_fs u LEFT JOIN vm_fs cur ON cur.path = u.path
         WHERE u.undo_id = ?1 ORDER BY u.path")?;
    for r in st.query_map(params![undo], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, bool>(2)?, r.get::<_, bool>(3)?)))? {
        match r? {
            (p, false, true, _) => d.created.push(p),
            (p, true, false, _) => d.deleted.push(p),
            (p, true, true, true) => d.modified.push(p),
            _ => {}
        }
    }
    Ok(d)
}

/// Put every path of the record back as it was before its step. Runs with the write policy bypassed, as a
/// snapshot restore does: undoing must be able to repair a protected path.
pub fn undo_apply(conn: &Connection, undo: i64) -> Result<()> {
    let flag = crate::policy::register(conn)?;
    let _bypass = flag.enter();
    in_savepoint(conn, "undo_apply", || {
        let rows: Vec<(String, bool)> = conn.prepare_cached("SELECT path, existed FROM vm_undo_fs WHERE undo_id=?1 ORDER BY length(path) DESC, path")?
            .query_map(params![undo], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        // deepest first: a directory is emptied before it is removed
        for (p, _) in &rows {
            conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1")?.execute(params![p])?;
        }
        // shallowest first: a parent exists before its children
        for (p, existed) in rows.iter().rev() {
            if !existed { continue; }
            conn.prepare_cached(
                "INSERT INTO vm_fs(path, kind, mode, mtime, mtime_ns, size, link_target, hash, content, compressed)
                 SELECT path, kind, mode, mtime, mtime_ns, size, link_target, hash, NULL, 0 FROM vm_undo_fs WHERE undo_id=?1 AND path=?2")?
                .execute(params![undo, p])?;
            conn.prepare_cached("INSERT INTO vm_file_chunks(path, idx, hash) SELECT path, idx, hash FROM vm_undo_chunks WHERE undo_id=?1 AND path=?2")?
                .execute(params![undo, p])?;
        }
        conn.prepare_cached("INSERT INTO vm_log(op, detail) VALUES ('undo', ?1)")?.execute(params![undo.to_string()])?;
        Ok(())
    })
}

/// Drop an undo record; the blobs only it referenced are released.
pub fn undo_delete(conn: &Connection, undo: i64) -> Result<()> {
    conn.prepare_cached("DELETE FROM vm_undo WHERE id=?1")?.execute(params![undo])?;
    Ok(())
}
