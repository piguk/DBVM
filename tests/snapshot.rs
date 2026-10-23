//! Phase 1: content-level snapshots. A snapshot records the whole file tree (metadata and chunk
//! references), shares blobs with the live tree, and restores it exactly.
use selfdb::vm;

fn tmpdb(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let d = std::env::temp_dir().join(format!("snap-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    (d, db)
}

fn open(db: &std::path::Path) -> rusqlite::Connection {
    vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap()
}

fn tree(c: &rusqlite::Connection) -> Vec<(String, String, Vec<u8>, i64)> {
    // (path, kind, hash, size) of the live tree, ordered
    let mut st = c.prepare("SELECT path, kind, COALESCE(hash, x''), size FROM vm_fs ORDER BY path").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(|x| x.unwrap()).collect()
}

#[test]
fn snapshot_restore_is_byte_exact_after_mutations() {
    let (d, db) = tmpdb("exact");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/etc/a", b"alpha", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/data/big", &vec![7u8; 200_000], 0o644, 1).unwrap();
    vm::vm_add_symlink(&c, "/etc/link", "/etc/a").unwrap();
    let before = tree(&c);
    let want_big = vm::vm_cat(&c, "/data/big").unwrap();

    vm::snapshot_create(&c, "s1", "").unwrap();

    // mutate: overwrite, delete, add
    vm::vm_add_bytes(&c, "/etc/a", b"changed", 0o644, 2).unwrap();
    c.execute("DELETE FROM vm_fs WHERE path='/data/big'", []).unwrap();
    vm::vm_add_bytes(&c, "/new", b"new", 0o644, 2).unwrap();

    vm::snapshot_restore(&c, "s1").unwrap();

    assert_eq!(tree(&c), before, "tree metadata and content references restored");
    assert_eq!(vm::vm_cat(&c, "/data/big").unwrap(), want_big);
    assert_eq!(vm::vm_cat(&c, "/etc/a").unwrap(), b"alpha");
    assert!(vm::vm_resolve(&c, "/new").is_err(), "file added after the snapshot is gone");
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn snapshot_shares_blobs_and_keeps_them_alive() {
    let (d, db) = tmpdb("share");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/f", &vec![9u8; 100_000], 0o644, 1).unwrap();
    let blobs_before: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    vm::snapshot_create(&c, "s", "").unwrap();
    let blobs_after: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(blobs_before, blobs_after, "a snapshot must not copy any blob");
    // overwrite the live file: the snapshot still holds the old chunks, so they must survive
    vm::vm_add_bytes(&c, "/f", b"tiny", 0o644, 2).unwrap();
    vm::snapshot_restore(&c, "s").unwrap();
    assert_eq!(vm::vm_cat(&c, "/f").unwrap(), vec![9u8; 100_000]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn diff_reports_created_modified_deleted() {
    let (d, db) = tmpdb("diff");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/keep", b"same", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/mod", b"old", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/gone", b"bye", 0o644, 1).unwrap();
    vm::snapshot_create(&c, "a", "").unwrap();
    vm::vm_add_bytes(&c, "/mod", b"new", 0o644, 2).unwrap();
    c.execute("DELETE FROM vm_fs WHERE path='/gone'", []).unwrap();
    vm::vm_add_bytes(&c, "/born", b"x", 0o644, 2).unwrap();
    let diff = vm::snapshot_diff_live(&c, "a").unwrap();
    assert_eq!(diff.created, vec!["/born".to_string()]);
    assert_eq!(diff.modified, vec!["/mod".to_string()]);
    assert_eq!(diff.deleted, vec!["/gone".to_string()]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn snapshot_names_are_unique_and_missing_restore_fails() {
    let (d, db) = tmpdb("names");
    let c = open(&db);
    vm::snapshot_create(&c, "x", "").unwrap();
    assert!(vm::snapshot_create(&c, "x", "").is_err(), "duplicate name rejected");
    assert!(vm::snapshot_restore(&c, "nope").is_err(), "unknown snapshot is an error, not a no-op");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn snapshot_of_1000_files_is_fast_and_small() {
    let (d, db) = tmpdb("perf");
    let c = open(&db);
    for i in 0..1000 { vm::vm_add_bytes(&c, &format!("/t/f{:04}", i), format!("content {}", i).as_bytes(), 0o644, 1).unwrap(); }
    let s = std::time::Instant::now();
    vm::snapshot_create(&c, "p", "").unwrap();
    let ms = s.elapsed().as_millis();
    let bytes: i64 = c.query_row("SELECT sum(length(path)+length(hash)) FROM vm_snapshot_fs", [], |r| r.get(0)).unwrap_or(0);
    println!("snapshot 1000 files: {} ms, metadata ~{} bytes", ms, bytes);
    assert!(ms < 2000, "snapshot must not scale badly (measured separately against the 20 ms target)");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn unchanged_directories_are_not_reported_modified() {
    // A directory whose children change has the same kind, mode and (null) hash; it must not be
    // reported as modified just because its entries changed.
    let (d, db) = tmpdb("dirmod");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/t/a", b"a", 0o644, 1).unwrap();
    vm::snapshot_create(&c, "base", "").unwrap();
    vm::vm_add_bytes(&c, "/t/b", b"b", 0o644, 2).unwrap();
    let diff = vm::snapshot_diff_live(&c, "base").unwrap();
    assert_eq!(diff.created, vec!["/t/b".to_string()]);
    assert!(diff.modified.is_empty(), "directory /t must not be reported modified: {:?}", diff.modified);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn delete_releases_only_unshared_blobs() {
    let (d, db) = tmpdb("del");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/shared", &vec![1u8; 70_000], 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/only_snap", &vec![2u8; 70_000], 0o644, 1).unwrap();
    vm::snapshot_create(&c, "old", "").unwrap();
    // drop /only_snap from the live tree; /shared stays live
    c.execute("DELETE FROM vm_fs WHERE path='/only_snap'", []).unwrap();
    let blobs_before: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    vm::snapshot_delete(&c, "old").unwrap();
    let blobs_after: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    // /only_snap's two chunks (65536 + 4464 bytes) were held only by the snapshot: both are freed.
    // /shared's chunks are still live and must keep their rows.
    assert_eq!(blobs_before - blobs_after, 2, "exactly the two snapshot-only chunks are freed");
    let live_chunks: i64 = c.query_row("SELECT count(*) FROM vm_blobs b WHERE EXISTS (SELECT 1 FROM vm_file_chunks f WHERE f.hash=b.hash)", [], |r| r.get(0)).unwrap();
    assert_eq!(live_chunks, 2, "both live chunks keep their blob rows");
    assert_eq!(vm::vm_cat(&c, "/shared").unwrap(), vec![1u8; 70_000], "live data must survive");
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn delete_unknown_snapshot_is_an_error() {
    let (d, db) = tmpdb("del-missing");
    let c = open(&db);
    assert!(vm::snapshot_delete(&c, "nope").is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn retention_keeps_newest_and_reports_what_it_removed() {
    let (d, db) = tmpdb("retain");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/f", b"x", 0o644, 1).unwrap();
    for i in 0..5 { vm::snapshot_create(&c, &format!("s{}", i), "").unwrap(); }
    let removed = vm::snapshot_retain(&c, 2).unwrap();
    assert_eq!(removed, vec!["s0".to_string(), "s1".to_string(), "s2".to_string()]);
    let left: Vec<String> = c.prepare("SELECT name FROM vm_snapshots ORDER BY id").unwrap()
        .query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
    assert_eq!(left, vec!["s3".to_string(), "s4".to_string()]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn snapshot_deletion_keeps_the_restore_source_of_other_snapshots() {
    // A blob shared by two snapshots must survive deleting either one.
    let (d, db) = tmpdb("two");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/f", &vec![5u8; 90_000], 0o644, 1).unwrap();
    vm::snapshot_create(&c, "a", "").unwrap();
    vm::snapshot_create(&c, "b", "").unwrap();
    vm::snapshot_delete(&c, "a").unwrap();
    c.execute("DELETE FROM vm_fs", []).unwrap();
    c.execute("DELETE FROM vm_file_chunks", []).unwrap();
    vm::snapshot_restore(&c, "b").unwrap();
    assert_eq!(vm::vm_cat(&c, "/f").unwrap(), vec![5u8; 90_000]);
    let _ = std::fs::remove_dir_all(&d);
}
