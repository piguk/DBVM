//! VM store round-trip tests. Each test uses its own temp DB.
use selfdb::vm;
use std::path::PathBuf;

fn tmpdb(name: &str) -> (tempdir_guard::Guard, PathBuf) {
    let g = tempdir_guard::Guard::new(name);
    let p = g.path().join("t.db");
    (g, p)
}

mod tempdir_guard {
    use std::path::{Path, PathBuf};
    pub struct Guard(PathBuf);
    impl Guard {
        pub fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("selfdb-test-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Guard(d)
        }
        pub fn path(&self) -> &Path { &self.0 }
    }
    impl Drop for Guard { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
}

fn open(db: &PathBuf) -> rusqlite::Connection {
    vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap()
}

#[test]
fn add_then_cat_is_byte_identical() {
    let (_g, db) = tmpdb("cat");
    let c = open(&db);
    let data: Vec<u8> = (0..20_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    vm::vm_add_bytes(&c, "/etc/blob", &data, 0o644, 1).unwrap();
    assert_eq!(vm::vm_cat(&c, "/etc/blob").unwrap(), data);
}

#[test]
fn tiny_and_empty_files_roundtrip() {
    let (_g, db) = tmpdb("tiny");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/a", b"", 0o600, 1).unwrap();
    vm::vm_add_bytes(&c, "/b", b"x", 0o600, 1).unwrap();
    assert_eq!(vm::vm_cat(&c, "/a").unwrap(), b"");
    assert_eq!(vm::vm_cat(&c, "/b").unwrap(), b"x");
}

#[test]
fn identical_content_dedups_and_refcounts() {
    let (_g, db) = tmpdb("dedup");
    let c = open(&db);
    let data = vec![7u8; 4096];
    vm::vm_add_bytes(&c, "/x/1", &data, 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/x/2", &data, 0o644, 1).unwrap();
    let n: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    let rc: i64 = c.query_row("SELECT refcnt FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
    assert_eq!(rc, 2);
}

#[test]
fn overwrite_releases_old_blob() {
    let (_g, db) = tmpdb("overwrite");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/f", &vec![1u8; 2000], 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/f", &vec![2u8; 2000], 0o644, 2).unwrap();
    let n: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "old blob should be garbage-collected");
    assert_eq!(vm::vm_cat(&c, "/f").unwrap(), vec![2u8; 2000]);
}

#[test]
fn symlink_resolves_for_cat() {
    let (_g, db) = tmpdb("link");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/lib/real.so", b"ELFish", 0o755, 1).unwrap();
    vm::vm_add_symlink(&c, "/lib/alias.so", "/lib/real.so").unwrap();
    assert_eq!(vm::vm_cat(&c, "/lib/alias.so").unwrap(), b"ELFish");
}

#[test]
fn verify_reports_integrity_ok() {
    let (_g, db) = tmpdb("verify");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/f", b"hello", 0o644, 1).unwrap();
    assert!(vm::vm_verify(&c, true).unwrap().contains("integrity=ok"));
}

#[test]
fn sync_from_host_picks_up_changes() {
    let (_g, db) = tmpdb("sync");
    let host = _g.path().join("root");
    std::fs::create_dir_all(host.join("etc")).unwrap();
    std::fs::write(host.join("etc/motd"), b"v1").unwrap();
    let c = open(&db);
    vm::vm_sync_from_host(&c, &host).unwrap();
    assert_eq!(vm::vm_cat(&c, "/etc/motd").unwrap(), b"v1");
    std::fs::write(host.join("etc/motd"), b"version two").unwrap();
    vm::vm_sync_from_host(&c, &host).unwrap();
    assert_eq!(vm::vm_cat(&c, "/etc/motd").unwrap(), b"version two");
}

#[test]
fn materialize_tree_matches_db() {
    let (_g, db) = tmpdb("mat");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/bin/tool", &vec![9u8; 5000], 0o755, 1).unwrap();
    vm::vm_add_symlink(&c, "/bin/t2", "tool").unwrap();
    let dest = _g.path().join("out");
    vm::vm_materialize_tree(&c, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("bin/tool")).unwrap(), vec![9u8; 5000]);
    assert_eq!(std::fs::read_link(dest.join("bin/t2")).unwrap(), PathBuf::from("tool"));
}

#[test]
fn blob_key_is_full_sha256() {
    let k = vm::fx_hash_u64(b"abc");
    assert_eq!(k.len(), 64, "content key must be sha256 hex");
    assert_eq!(k, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}

#[test]
fn dict_trains_from_many_small_blobs() {
    let (_g, db) = tmpdb("dict");
    let c = open(&db);
    for i in 0..60u32 {
        let body = format!("{{\"id\":{},\"name\":\"item-{}\",\"tags\":[\"alpha\",\"beta\",\"gamma\"],\"pad\":\"{}\"}}", i, i, "x".repeat(300 + i as usize));
        vm::vm_add_bytes(&c, &format!("/data/{}.json", i), body.as_bytes(), 0o644, 1).unwrap();
    }
    let d = vm::vm_train_dict(&c, 16384).unwrap();
    assert!(d.is_some(), "expected a dictionary from 60 similar small blobs");
    // reads still byte-exact after training
    let got = vm::vm_cat(&c, "/data/7.json").unwrap();
    assert!(String::from_utf8(got).unwrap().contains("item-7"));
}

#[test]
fn materialize_streams_large_and_refreshes_changed() {
    let (_g, db) = tmpdb("mat-big");
    let c = open(&db);
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(1103515245) >> 16) as u8).collect();
    vm::vm_add_bytes(&c, "/big.bin", &big, 0o644, 100).unwrap();
    let dest = _g.path().join("out");
    vm::vm_materialize_tree(&c, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("big.bin")).unwrap(), big);
    // same size, different content and new mtime -> must be rewritten
    let new: Vec<u8> = big.iter().map(|b| b.wrapping_add(1)).collect();
    vm::vm_add_bytes(&c, "/big.bin", &new, 0o644, 5000).unwrap();
    vm::vm_materialize_tree(&c, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("big.bin")).unwrap(), new);
}

/// Invariant: every blob's refcnt equals the number of vm_fs rows referencing it, and
/// no unreferenced blob survives. Checked after each mutation.
fn assert_refcounts(c: &rusqlite::Connection) {
    let bad: i64 = c.query_row(
        "SELECT count(*) FROM vm_blobs b WHERE b.refcnt != (SELECT count(*) FROM vm_fs f WHERE f.hash = b.hash)",
        [], |r| r.get(0)).unwrap();
    assert_eq!(bad, 0, "refcnt drift detected");
}

#[test]
fn refcounts_follow_every_mutation() {
    let (_g, db) = tmpdb("rc");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/a", &vec![1u8; 3000], 0o644, 1).unwrap(); assert_refcounts(&c);
    vm::vm_add_bytes(&c, "/b", &vec![1u8; 3000], 0o644, 1).unwrap(); assert_refcounts(&c);
    vm::vm_add_bytes(&c, "/a", &vec![2u8; 3000], 0o644, 2).unwrap(); assert_refcounts(&c);
    vm::vm_add_bytes(&c, "/a", &vec![2u8; 3000], 0o600, 3).unwrap(); assert_refcounts(&c); // same content
    vm::vm_add_symlink(&c, "/b", "/a").unwrap(); assert_refcounts(&c);                     // file -> symlink
    vm::vm_add_bytes(&c, "/b", &vec![9u8; 500], 0o644, 4).unwrap(); assert_refcounts(&c);  // symlink -> file
    vm::vm_add_dir(&c, "/d", 0o755, 1).unwrap(); assert_refcounts(&c);
    vm::vm_add_bytes(&c, "/d/x", &vec![3u8; 900], 0o644, 1).unwrap(); assert_refcounts(&c);
    c.execute("DELETE FROM vm_fs WHERE path='/d' OR substr(path,1,3)='/d/'", []).unwrap(); assert_refcounts(&c);
    c.execute("DELETE FROM vm_fs WHERE path='/a'", []).unwrap(); assert_refcounts(&c);
    let n: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "only /b's blob should remain");
}

#[test]
fn sync_updates_keep_refcounts_exact() {
    let (_g, db) = tmpdb("rc-sync");
    let host = _g.path().join("root");
    std::fs::create_dir_all(host.join("etc")).unwrap();
    std::fs::write(host.join("etc/a"), b"one").unwrap();
    std::fs::write(host.join("etc/b"), b"one").unwrap();
    let c = open(&db);
    vm::vm_sync_from_host(&c, &host).unwrap(); assert_refcounts(&c);
    std::fs::write(host.join("etc/a"), b"two two").unwrap();
    vm::vm_sync_from_host(&c, &host).unwrap(); assert_refcounts(&c);
    std::fs::remove_file(host.join("etc/b")).unwrap();
    vm::vm_sync_from_host(&c, &host).unwrap(); assert_refcounts(&c);
    let n: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn counters_match_raw_aggregates_through_mutations() {
    let (_g, db) = tmpdb("cnt");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/a", &vec![1u8; 700], 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/b", &vec![2u8; 9000], 0o644, 1).unwrap();
    vm::vm_add_symlink(&c, "/l", "/a").unwrap();
    vm::vm_add_dir(&c, "/d", 0o755, 1).unwrap();
    vm::vm_add_bytes(&c, "/a", &vec![3u8; 50], 0o644, 2).unwrap();
    c.execute("DELETE FROM vm_fs WHERE path='/b'", []).unwrap();
    let rep = vm::vm_verify(&c, true).unwrap();
    assert!(rep.contains("deep=ok"), "{}", rep);
}

#[test]
fn legacy_v3_database_migrates_to_blob_keys() {
    // Build a genuine schema-3 database by hand: TEXT blob keys (legacy FxHash-style 16 chars),
    // counter triggers present, user_version=3. Then open with current code.
    let (_g, db) = tmpdb("legacy");
    let raw_a: Vec<u8> = b"hello legacy world, this is file A with some padding padding padding".to_vec();
    let raw_b: Vec<u8> = vec![7u8; 3000];
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(&vm::vm_schema_sql().replace("PRAGMA user_version = 4;", "PRAGMA user_version = 3;")).ok();
        // rebuild vm_blobs with the legacy TEXT key layout
        // remove every v5 trigger/table first: a trigger that names vm_blobs would error on use
        c.execute_batch("DROP TRIGGER IF EXISTS vm_fc_ins; DROP TRIGGER IF EXISTS vm_fc_del; DROP TRIGGER IF EXISTS vm_fc_upd;
            DROP TRIGGER IF EXISTS vm_fs_del_chunks; DROP TABLE IF EXISTS vm_file_chunks;
            DROP TRIGGER IF EXISTS vm_snc_ins; DROP TRIGGER IF EXISTS vm_snc_del; DROP TABLE IF EXISTS vm_snapshot_chunks; DROP TABLE IF EXISTS vm_snapshot_fs;
            DROP TABLE vm_blobs; DROP INDEX IF EXISTS idx_vm_blobs_refcnt;
            CREATE TABLE vm_blobs(hash TEXT PRIMARY KEY, content BLOB NOT NULL, compressed INTEGER NOT NULL DEFAULT 0,
              raw_size INTEGER NOT NULL, refcnt INTEGER NOT NULL DEFAULT 1);
            DROP TRIGGER IF EXISTS vm_fs_hash_ins; DROP TRIGGER IF EXISTS vm_fs_hash_del; DROP TRIGGER IF EXISTS vm_fs_hash_upd;
            DROP TRIGGER IF EXISTS vm_cnt_blob_ins; DROP TRIGGER IF EXISTS vm_cnt_blob_del; DROP TRIGGER IF EXISTS vm_cnt_blob_upd;
            PRAGMA user_version=3;").unwrap();
        let ka = "aaaaaaaaaaaaaaaa"; let kb = "bbbbbbbbbbbbbbbb";
        c.execute("INSERT INTO vm_blobs(hash,content,compressed,raw_size,refcnt) VALUES (?1,?2,0,?3,1)", rusqlite::params![ka, raw_a, raw_a.len() as i64]).unwrap();
        c.execute("INSERT INTO vm_blobs(hash,content,compressed,raw_size,refcnt) VALUES (?1,?2,0,?3,1)", rusqlite::params![kb, raw_b, raw_b.len() as i64]).unwrap();
        c.execute("INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,compressed) VALUES ('/a','file',420,1,?1,?2,0)", rusqlite::params![raw_a.len() as i64, ka]).unwrap();
        c.execute("INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,compressed) VALUES ('/b','file',420,1,?1,?2,0)", rusqlite::params![raw_b.len() as i64, kb]).unwrap();
        c.execute("INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,compressed) VALUES ('/c','file',420,1,?1,?2,0)", rusqlite::params![raw_b.len() as i64, kb]).unwrap();
    }
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let ver: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(ver, vm::VM_USER_VERSION as i64);
    // keys are now 32-byte sha256 of content
    let k: Vec<u8> = c.query_row("SELECT hash FROM vm_blobs WHERE raw_size=?1", rusqlite::params![raw_a.len() as i64], |r| r.get(0)).unwrap();
    assert_eq!(k.len(), 32);
    assert_eq!(k.as_slice(), vm::blob_key(&raw_a).as_slice());
    // content still reads back exactly, and the shared blob is refcounted twice
    assert_eq!(vm::vm_cat(&c, "/a").unwrap(), raw_a);
    assert_eq!(vm::vm_cat(&c, "/c").unwrap(), raw_b);
    let rc: i64 = c.query_row("SELECT refcnt FROM vm_blobs WHERE raw_size=?1", rusqlite::params![raw_b.len() as i64], |r| r.get(0)).unwrap();
    assert_eq!(rc, 2);
    let rep = vm::vm_verify(&c, true).unwrap();
    assert!(rep.contains("deep=ok"), "{}", rep);
}

#[test]
fn chunk_boundaries_roundtrip_and_range_read() {
    use vm::CHUNK_SIZE;
    let (_g, db) = tmpdb("chunks");
    let c = open(&db);
    let sizes = [0usize, 1, CHUNK_SIZE - 1, CHUNK_SIZE, CHUNK_SIZE + 1, 3 * CHUNK_SIZE + 17];
    for (i, &n) in sizes.iter().enumerate() {
        let data: Vec<u8> = (0..n).map(|k| ((k * 31 + i) % 251) as u8).collect();
        let p = format!("/sz/{}", n);
        vm::vm_add_bytes(&c, &p, &data, 0o644, 1).unwrap();
        assert_eq!(vm::vm_cat(&c, &p).unwrap(), data, "size {}", n);
        // a window straddling the first chunk boundary
        if n > CHUNK_SIZE {
            let off = CHUNK_SIZE - 10;
            let got = vm::read_range(&c, &p, off as u64, 40).unwrap();
            assert_eq!(got, data[off..(off + 40).min(n)].to_vec(), "window at size {}", n);
        }
    }
    let rep = vm::vm_verify(&c, true).unwrap();
    assert!(rep.contains("deep=ok"), "{}", rep);
}

#[test]
fn identical_chunks_are_stored_once() {
    use vm::CHUNK_SIZE;
    let (_g, db) = tmpdb("chunk-dedup");
    let c = open(&db);
    // two files sharing their first chunk, differing in the second
    let mut a = vec![5u8; CHUNK_SIZE];
    a.extend(vec![6u8; 100]);
    let mut b = vec![5u8; CHUNK_SIZE];
    b.extend(vec![7u8; 100]);
    vm::vm_add_bytes(&c, "/a", &a, 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/b", &b, 0o644, 1).unwrap();
    let blobs: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(blobs, 3, "shared 64K chunk + two distinct tails");
    let shared: i64 = c.query_row("SELECT refcnt FROM vm_blobs WHERE raw_size=?1", rusqlite::params![CHUNK_SIZE as i64], |r| r.get(0)).unwrap();
    assert_eq!(shared, 2);
    assert_eq!(vm::vm_cat(&c, "/a").unwrap(), a);
    assert_eq!(vm::vm_cat(&c, "/b").unwrap(), b);
}

#[test]
fn overwrite_and_delete_release_chunk_references() {
    use vm::CHUNK_SIZE;
    let (_g, db) = tmpdb("chunk-release");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/f", &vec![1u8; 2 * CHUNK_SIZE + 5], 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/f", &vec![2u8; 10], 0o644, 2).unwrap();
    let n: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "old chunks must be released on overwrite");
    c.execute("DELETE FROM vm_fs WHERE path='/f'", []).unwrap();
    let n: i64 = c.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "no orphan chunks after delete");
    let rows: i64 = c.query_row("SELECT count(*) FROM vm_file_chunks", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn rechunk_splits_legacy_single_chunk_files_idempotently() {
    use vm::CHUNK_SIZE;
    let (_g, db) = tmpdb("rechunk");
    let c = open(&db);
    let big: Vec<u8> = (0..(3 * CHUNK_SIZE + 999)).map(|i| (i * 7 % 253) as u8).collect();
    let small = vec![9u8; 100];
    vm::vm_add_bytes(&c, "/big", &big, 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/small", &small, 0o644, 1).unwrap();
    // simulate the legacy layout: /big is ONE chunk holding the whole file.
    // Store that blob directly, then point /big at it (the release of the 3-chunk
    // layout happens through the normal trigger path).
    c.execute_batch("DELETE FROM vm_file_chunks WHERE path='/big';").unwrap();
    let key = vm::blob_key(&big);
    let z = zstd::bulk::compress(&big, 3).unwrap();
    c.execute("INSERT INTO vm_blobs(hash,content,compressed,raw_size,refcnt) VALUES (?1,?2,2,?3,0)",
        rusqlite::params![key.as_slice(), z, big.len() as i64]).unwrap();
    c.execute("INSERT INTO vm_file_chunks(path,idx,hash) VALUES ('/big',0,?1)", rusqlite::params![key.as_slice()]).unwrap();
    let exists: i64 = c.query_row("SELECT refcnt FROM vm_blobs WHERE hash=?1", rusqlite::params![key.as_slice()], |r| r.get(0)).unwrap();
    assert_eq!(exists, 1, "legacy single blob referenced once");
    let (done, skipped, failed) = vm::vm_rechunk(&c).unwrap();
    assert_eq!((done, failed), (1, 0));
    // /small is below CHUNK_SIZE and not a candidate; no candidate is already multi-chunk yet
    assert_eq!(skipped, 0);
    let chunks: i64 = c.query_row("SELECT count(*) FROM vm_file_chunks WHERE path='/big'", [], |r| r.get(0)).unwrap();
    assert_eq!(chunks, 4);
    assert_eq!(vm::vm_cat(&c, "/big").unwrap(), big);
    assert_eq!(vm::read_range(&c, "/big", (CHUNK_SIZE - 3) as u64, 10).unwrap(), big[CHUNK_SIZE - 3..CHUNK_SIZE + 7].to_vec());
    // whole-file blob released: no 3*CHUNK+999 sized orphan remains
    let orphan: i64 = c.query_row("SELECT count(*) FROM vm_blobs WHERE raw_size=?1", rusqlite::params![big.len() as i64], |r| r.get(0)).unwrap();
    assert_eq!(orphan, 0);
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
    // second run is a no-op: /big is now multi-chunk, so it is counted as skipped
    let (done2, skipped2, failed2) = vm::vm_rechunk(&c).unwrap();
    assert_eq!((done2, skipped2, failed2), (0, 1, 0));
}

#[test]
fn sync_reports_real_counts_for_dir_type_change() {
    // file -> dir type change: must count exactly one update, not two.
    let (_g, db) = tmpdb("sync-count");
    let host = _g.path().join("root");
    std::fs::create_dir_all(host.join("etc")).unwrap();
    std::fs::write(host.join("etc/x"), b"file").unwrap();
    let c = open(&db);
    vm::vm_sync_from_host(&c, &host).unwrap();
    std::fs::remove_file(host.join("etc/x")).unwrap();
    std::fs::create_dir_all(host.join("etc/x")).unwrap();
    let (created, updated, deleted) = vm::vm_sync_from_host(&c, &host).unwrap();
    assert_eq!((created, updated, deleted), (0, 1, 0));
}

#[test]
fn sync_counts_file_to_symlink_once() {
    let (_g, db) = tmpdb("sync-count2");
    let host = _g.path().join("root");
    std::fs::create_dir_all(host.join("etc")).unwrap();
    std::fs::write(host.join("etc/x"), b"file body").unwrap();
    let c = open(&db);
    vm::vm_sync_from_host(&c, &host).unwrap();
    std::fs::remove_file(host.join("etc/x")).unwrap();
    std::os::unix::fs::symlink("/etc/y", host.join("etc/x")).unwrap();
    let (created, updated, deleted) = vm::vm_sync_from_host(&c, &host).unwrap();
    assert_eq!((created, updated, deleted), (0, 1, 0), "one type change = one update");
}

#[test]
fn sync_counts_symlink_to_file_once() {
    let (_g, db) = tmpdb("sync-count3");
    let host = _g.path().join("root");
    std::fs::create_dir_all(host.join("etc")).unwrap();
    std::os::unix::fs::symlink("/etc/y", host.join("etc/x")).unwrap();
    let c = open(&db);
    vm::vm_sync_from_host(&c, &host).unwrap();
    std::fs::remove_file(host.join("etc/x")).unwrap();
    std::fs::write(host.join("etc/x"), b"now a file").unwrap();
    let (created, updated, deleted) = vm::vm_sync_from_host(&c, &host).unwrap();
    assert_eq!((created, updated, deleted), (0, 1, 0), "one type change = one update");
}

#[test]
fn stream_write_matches_whole_write() {
    use vm::CHUNK_SIZE;
    let (_g, db) = tmpdb("stream-eq");
    let c = open(&db);
    for (i, n) in [0usize, 1, CHUNK_SIZE - 1, CHUNK_SIZE, CHUNK_SIZE + 1, 5 * CHUNK_SIZE + 3].into_iter().enumerate() {
        let data: Vec<u8> = (0..n).map(|k| ((k * 13 + i) % 251) as u8).collect();
        vm::vm_add_bytes(&c, &format!("/w{}", i), &data, 0o644, 1).unwrap();
        // a reader that returns short reads (1..=7 bytes) to exercise the fill loop
        struct Dribble<'a>(&'a [u8], usize);
        impl<'a> std::io::Read for Dribble<'a> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let k = out.len().min(self.0.len()).min(1 + self.1 % 7);
                self.1 += 1;
                out[..k].copy_from_slice(&self.0[..k]);
                self.0 = &self.0[k..];
                Ok(k)
            }
        }
        let got = vm::fs_put_file_stream(&c, &format!("/s{}", i), 0o644, 1, &mut Dribble(&data, 0)).unwrap();
        assert_eq!(got as usize, n);
        assert_eq!(vm::vm_cat(&c, &format!("/s{}", i)).unwrap(), data, "size {}", n);
        let a: (i64, Vec<u8>) = c.query_row("SELECT size, hash FROM vm_fs WHERE path=?1", rusqlite::params![format!("/w{}", i)], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let b: (i64, Vec<u8>) = c.query_row("SELECT size, hash FROM vm_fs WHERE path=?1", rusqlite::params![format!("/s{}", i)], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(a, b, "whole-file hash and size identical, size {}", n);
    }
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
}

#[test]
fn failed_stream_keeps_previous_version() {
    use vm::CHUNK_SIZE;
    let (_g, db) = tmpdb("stream-fail");
    let c = open(&db);
    let old = vec![3u8; 2 * CHUNK_SIZE + 9];
    vm::vm_add_bytes(&c, "/f", &old, 0o644, 1).unwrap();
    // reader that fails after one chunk
    struct Bad { left: usize }
    impl std::io::Read for Bad {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.left == 0 { return Err(std::io::Error::new(std::io::ErrorKind::Other, "disk gone")); }
            let k = out.len().min(self.left);
            for b in &mut out[..k] { *b = 4; }
            self.left -= k;
            Ok(k)
        }
    }
    let r = vm::fs_put_file_stream(&c, "/f", 0o644, 2, &mut Bad { left: CHUNK_SIZE + 5 });
    assert!(r.is_err());
    assert_eq!(vm::vm_cat(&c, "/f").unwrap(), old, "previous content must survive a failed write");
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
}

#[test]
fn sync_is_incremental_and_reports_exact_counts() {
    let (_g, db) = tmpdb("sync-inc");
    let host = _g.path().join("root");
    std::fs::create_dir_all(host.join("d/sub")).unwrap();
    std::fs::write(host.join("d/a"), b"aaa").unwrap();
    std::fs::write(host.join("d/sub/b"), b"bbbb").unwrap();
    std::os::unix::fs::symlink("a", host.join("d/l")).unwrap();
    let c = open(&db);
    let r1 = vm::vm_sync_report(&c, &host).unwrap();
    assert_eq!((r1.created, r1.updated, r1.unchanged, r1.deleted, r1.failed), (5, 0, 0, 0, 0));
    // nothing changed: every entry is unchanged, nothing rewritten
    let r2 = vm::vm_sync_report(&c, &host).unwrap();
    assert_eq!((r2.created, r2.updated, r2.unchanged, r2.deleted, r2.failed), (0, 0, 5, 0, 0));
    // one edit, one delete
    std::fs::write(host.join("d/a"), b"aaa-changed-longer").unwrap();
    std::fs::remove_file(host.join("d/sub/b")).unwrap();
    let r3 = vm::vm_sync_report(&c, &host).unwrap();
    assert_eq!((r3.created, r3.updated, r3.unchanged, r3.deleted, r3.failed), (0, 1, 3, 1, 0));
    assert_eq!(vm::vm_cat(&c, "/d/a").unwrap(), b"aaa-changed-longer");
    assert!(vm::vm_cat(&c, "/d/sub/b").is_err());
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
}

/// Records that share a long template (where a dictionary helps) and differ by id. Sized above
/// the trainer's 256-byte sampling floor so they are eligible samples.
fn small_record(tag: &str, i: u32) -> String {
    format!("{{\"schema\":\"com.example.record.v1\",\"kind\":\"inventory-item\",\"owner\":\"{}\",\"id\":{},\"status\":\"active\",\"flags\":[\"audited\",\"synced\",\"archived-false\",\"public-false\"],\"labels\":{{\"region\":\"eu-west-1\",\"tier\":\"gold\",\"team\":\"platform-storage\",\"env\":\"production\"}},\"history\":[{{\"at\":\"2025-01-01T00:00:00Z\",\"op\":\"create\"}},{{\"at\":\"2025-02-01T00:00:00Z\",\"op\":\"update\"}}],\"note\":\"generated record {} for owner {} in the nightly batch run\"}}", tag, i, i, tag)
}

#[test]
fn dictionary_is_per_database_not_per_process() {
    // Two databases in one process with different trained dictionaries. Reading each back
    // must use its own dictionary; a process-global dictionary would mix them up.
    // A dictionary only applies to data written AFTER training, so: train first, then write.
    let (_g1, db1) = tmpdb("dict-a");
    let (_g2, db2) = tmpdb("dict-b");
    let mk = |db: &std::path::PathBuf, tag: &str| -> rusqlite::Connection {
        let c = open(db);
        for i in 0..60u32 {
            let body = small_record(tag, i);
            vm::vm_add_bytes(&c, &format!("/train/{}", i), body.as_bytes(), 0o644, 1).unwrap();
        }
        assert!(vm::vm_train_dict(&c, 16384).unwrap().is_some());
        // post-training data must be NEW content (same template, different ids), otherwise
        // dedup reuses the training blobs and the dictionary is never applied to them
        for i in 1000..1060u32 {
            let body = small_record(tag, i);
            vm::vm_add_bytes(&c, &format!("/d/{}", i - 1000), body.as_bytes(), 0o644, 1).unwrap();
        }
        c
    };
    let a = mk(&db1, "alpha");
    let b = mk(&db2, "beta");
    // the post-training files must actually use the dictionary (flag 3), or the test proves nothing
    let flag3: i64 = a.query_row("SELECT count(*) FROM vm_blobs WHERE compressed=3", [], |r| r.get(0)).unwrap();
    assert!(flag3 > 0, "dictionary path not exercised");
    for i in 0..60u32 {
        let got = vm::vm_cat(&a, &format!("/d/{}", i)).unwrap();
        assert!(String::from_utf8(got).unwrap().contains("\"owner\":\"alpha\""), "file {} read with wrong dictionary", i);
        let got = vm::vm_cat(&b, &format!("/d/{}", i)).unwrap();
        assert!(String::from_utf8(got).unwrap().contains("\"owner\":\"beta\""));
    }
}

#[test]
fn retraining_keeps_old_dictionary_blocks_readable() {
    // Blocks written under dictionary #1 must still read after dictionary #2 replaces it.
    let (_g, db) = tmpdb("dict-retrain");
    let c = open(&db);
    for i in 0..60u32 {
        vm::vm_add_bytes(&c, &format!("/t/{}", i), small_record("gen1", i).as_bytes(), 0o644, 1).unwrap();
    }
    vm::vm_train_dict(&c, 16384).unwrap();
    for i in 1000..1060u32 {
        vm::vm_add_bytes(&c, &format!("/g1/{}", i), small_record("gen1", i).as_bytes(), 0o644, 1).unwrap();
    }
    // second training with different data replaces the stored dictionary
    for i in 0..60u32 {
        vm::vm_add_bytes(&c, &format!("/u/{}", i), format!("unrelated {} {}", i, "z".repeat(300)).as_bytes(), 0o644, 1).unwrap();
    }
    vm::vm_train_dict(&c, 16384).unwrap();
    for i in 0..60u32 {
        let got = vm::vm_cat(&c, &format!("/g1/{}", 1000 + i)).unwrap();
        assert!(String::from_utf8(got).unwrap().contains("gen1"), "gen1 file {} unreadable after retrain", i);
    }
}

#[test]
fn recompress_runs_on_current_schema() {
    let (_g, db) = tmpdb("recompress");
    let c = open(&db);
    let data: Vec<u8> = (0..5000u32).map(|i| (i % 97) as u8).collect();
    vm::vm_add_bytes(&c, "/r", &data, 0o644, 1).unwrap();
    let r = vm::vm_recompress(&c);
    assert!(r.is_ok(), "vm_recompress failed on current schema: {:?}", r.err());
    assert_eq!(vm::vm_cat(&c, "/r").unwrap(), data);
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
}

/// Snapshot of everything that defines a packed VM: per-file metadata + chunk keys, and the
/// blob set with stored encodings. Two packs of the same tree must produce equal snapshots.
fn pack_snapshot(c: &rusqlite::Connection) -> (Vec<(String, i64, i64, Vec<u8>, Vec<Vec<u8>>)>, Vec<(Vec<u8>, i64, i64, i64)>) {
    let mut files = Vec::new();
    let mut st = c.prepare("SELECT path, mode, size, hash FROM vm_fs WHERE kind='file' ORDER BY path").unwrap();
    for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?, r.get::<_,Vec<u8>>(3)?))).unwrap() {
        let (p, m, sz, h) = r.unwrap();
        let mut ks = c.prepare("SELECT hash FROM vm_file_chunks WHERE path=?1 ORDER BY idx").unwrap();
        let keys: Vec<Vec<u8>> = ks.query_map([&p], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
        files.push((p, m, sz, h, keys));
    }
    let mut blobs = Vec::new();
    let mut st = c.prepare("SELECT hash, compressed, raw_size, dict_id FROM vm_blobs ORDER BY hash").unwrap();
    for r in st.query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?))).unwrap() {
        blobs.push(r.unwrap());
    }
    (files, blobs)
}

#[test]
fn parallel_pack_matches_serial_pack() {
    use vm::CHUNK_SIZE;
    let (_g, _db) = tmpdb("pack-eq");
    let src = _g.path().join("src");
    std::fs::create_dir_all(src.join("sub/deep")).unwrap();
    // mix: empty, tiny, dict-eligible text, multi-chunk, binary-ish, nested
    std::fs::write(src.join("empty"), b"").unwrap();
    std::fs::write(src.join("tiny"), b"t").unwrap();
    for i in 0..40 { std::fs::write(src.join(format!("sub/rec{}", i)), format!("{{\"schema\":\"v1\",\"id\":{},\"owner\":\"alpha\",\"note\":\"generated record {} for owner alpha in the batch\"}}", i, i)).unwrap(); }
    let big: Vec<u8> = (0..(2*CHUNK_SIZE+777)).map(|k| ((k*31)%251) as u8).collect();
    std::fs::write(src.join("sub/deep/big"), &big).unwrap();
    let mut x = 0x2545F4914F6CDD1Du64;
    let rnd: Vec<u8> = (0..70000).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; (x >> 24) as u8 }).collect();
    std::fs::write(src.join("sub/rnd"), &rnd).unwrap();

    let make = |name: &str, parallel: bool| -> rusqlite::Connection {
        let db = _g.path().join(format!("{}.db", name));
        let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
        // dictionary trained on the same kind of data first, so dict_id paths are exercised
        for i in 0..60u32 { vm::vm_add_bytes(&c, &format!("/train/{}", i), format!("{{\"schema\":\"v1\",\"id\":{},\"owner\":\"alpha\",\"note\":\"generated record {} for owner alpha in the batch\"}}", i, i).as_bytes(), 0o644, 1).unwrap(); }
        vm::vm_train_dict(&c, 16384).unwrap();
        let n = if parallel { vm::vm_pack_host_dir_parallel(&c, src.to_str().unwrap(), "/data").unwrap() }
                else { vm::vm_pack_host_dir(&c, src.to_str().unwrap(), "/data").unwrap() };
        assert_eq!(n, 44, "{} packed file count", name);
        c
    };
    let serial = make("serial", false);
    let parallel = make("parallel", true);
    assert_eq!(pack_snapshot(&serial), pack_snapshot(&parallel), "parallel pack must equal serial pack");
    // directory rows too (the snapshot covers files only)
    let dirs = |c: &rusqlite::Connection| -> Vec<String> {
        c.prepare("SELECT path FROM vm_fs WHERE kind='dir' ORDER BY path").unwrap().query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect()
    };
    assert_eq!(dirs(&serial), dirs(&parallel), "directory rows must match");
    // content round-trips exactly
    assert_eq!(vm::vm_cat(&parallel, "/data/sub/deep/big").unwrap(), big);
    assert_eq!(vm::vm_cat(&parallel, "/data/sub/rnd").unwrap(), rnd);
    assert_eq!(vm::vm_cat(&parallel, "/data/empty").unwrap(), b"");
    assert!(vm::vm_verify(&parallel, true).unwrap().contains("deep=ok"));
}

#[test]
fn reused_scratch_dir_reflects_db_not_stale_host_edits() {
    // A stable materialize directory is reused across chroot runs. A file edited on the host
    // (size/mtime differ from the DB) must be rewritten from the DB on the next materialize,
    // and a file with a matching size+mtime must be left alone.
    let (_g, db) = tmpdb("scratch");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/etc/a", b"from-db-aaa", 0o644, 1000).unwrap();
    vm::vm_add_bytes(&c, "/etc/b", b"from-db-bbb", 0o644, 1000).unwrap();
    let scratch = _g.path().join("scratch");
    vm::vm_materialize_tree(&c, &scratch).unwrap();
    // host-side tamper with a: same size, different content, different mtime
    std::fs::write(scratch.join("etc/a"), b"TAMPER-----").unwrap();
    filetime_set(&scratch.join("etc/a"), 5000);
    let n = vm::vm_materialize_tree(&c, &scratch).unwrap();
    assert_eq!(std::fs::read(scratch.join("etc/a")).unwrap(), b"from-db-aaa", "tampered file must be restored from DB");
    assert_eq!(std::fs::read(scratch.join("etc/b")).unwrap(), b"from-db-bbb");
    assert!(n >= 2);
}

fn filetime_set(p: &std::path::Path, secs: i64) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let ts = libc::timespec { tv_sec: secs as libc::time_t, tv_nsec: 0 };
    unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [ts, ts].as_ptr(), 0); }
}

#[test]
fn reused_scratch_does_not_resurrect_deleted_files() {
    // The DB deletes /etc/gone between runs. The scratch dir still has it from the last run.
    // Materialize must not leave it behind, or the next sync-back would re-add it.
    let (_g, db) = tmpdb("scratch-del");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/etc/keep", b"k", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/etc/gone", b"g", 0o644, 1).unwrap();
    let scratch = _g.path().join("scratch");
    vm::vm_materialize_tree(&c, &scratch).unwrap();
    c.execute("DELETE FROM vm_fs WHERE path='/etc/gone'", []).unwrap();
    vm::vm_materialize_tree(&c, &scratch).unwrap();
    assert!(!scratch.join("etc/gone").exists(), "deleted file must not survive re-materialize");
    assert!(scratch.join("etc/keep").exists());
}

#[test]
fn prune_keeps_runtime_mountpoints_and_tmp() {
    let (_g, db) = tmpdb("prune-keep");
    let c = open(&db);
    vm::vm_add_bytes(&c, "/etc/a", b"a", 0o644, 1).unwrap();
    let s = _g.path().join("scratch");
    vm::vm_materialize_tree(&c, &s).unwrap();
    for m in ["dev", "proc", "sys", "tmp"] { std::fs::create_dir_all(s.join(m)).unwrap(); }
    std::fs::write(s.join("tmp/session-file"), b"x").unwrap();
    vm::vm_materialize_tree(&c, &s).unwrap();
    for m in ["dev", "proc", "sys", "tmp"] { assert!(s.join(m).is_dir(), "{} must survive", m); }
    assert!(s.join("tmp/session-file").exists());
}

#[test]
fn import_closure_includes_elf_interpreter() {
    // A dynamically linked executable needs its PT_INTERP (ld-linux) inside the VM, or it cannot
    // be exec'd there. DT_NEEDED alone does not list the interpreter.
    let host = "/bin/true";
    if !std::path::Path::new(host).exists() { return; }
    let (_g, db) = tmpdb("interp");
    let c = open(&db);
    vm::vm_import_closure(&c, host, "/").unwrap();
    let interp = elf_interp(host).expect("host /bin/true has an interpreter");
    let have: i64 = c.query_row("SELECT count(*) FROM vm_fs WHERE path=?1", rusqlite::params![interp], |r| r.get(0)).unwrap();
    assert_eq!(have, 1, "interpreter {} must be imported", interp);
}

/// PT_INTERP path of an ELF file, via goblin (already a dependency).
fn elf_interp(path: &str) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let elf = goblin::elf::Elf::parse(&data).ok()?;
    elf.interpreter.map(|s| s.to_string())
}

#[test]
fn imported_interpreter_resolves_inside_vm() {
    // The interpreter path must resolve to a real file inside the VM. A relative host symlink
    // (e.g. /lib64/ld-linux -> ../lib/...) breaks when the VM has no /lib alias.
    let host = "/bin/true";
    if !std::path::Path::new(host).exists() { return; }
    let (_g, db) = tmpdb("interp-resolve");
    let c = open(&db);
    vm::vm_import_closure(&c, host, "/").unwrap();
    let interp = elf_interp(host).expect("interpreter");
    let (real, kind, content, _l, _m) = vm::vm_resolve(&c, &interp).expect("interpreter must resolve in the VM");
    assert_eq!(kind, "file", "{} resolved to {} ({})", interp, real, kind);
    assert!(content.map(|b| b.len() > 0).unwrap_or(false), "interpreter has no content");
}

#[test]
fn needed_sonames_resolve_inside_vm() {
    // Every DT_NEEDED name of an imported object must resolve inside the VM. A soname that
    // differs from the real file name (libcap.so.2 -> libcap.so.2.75) needs its own link.
    let host = "/bin/ls";
    if !std::path::Path::new(host).exists() { return; }
    let (_g, db) = tmpdb("soname");
    let c = open(&db);
    vm::vm_import_closure(&c, host, "/").unwrap();
    let data = std::fs::read(host).unwrap();
    let elf = goblin::elf::Elf::parse(&data).unwrap();
    for lib in &elf.libraries {
        let dirs = ["/usr/lib/x86_64-linux-gnu", "/lib/x86_64-linux-gnu", "/usr/lib", "/lib"];
        let found = dirs.iter().any(|d| vm::vm_resolve(&c, &format!("{}/{}", d, lib)).is_ok());
        assert!(found, "DT_NEEDED {} does not resolve inside the VM", lib);
    }
}

/// Secondary indexes on the bulk-load tables (what `with_bulk_indexes_dropped` manages).
fn index_names(c: &rusqlite::Connection) -> Vec<String> {
    let sql = format!("SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL AND tbl_name IN ({}) ORDER BY name",
        vm::BULK_TABLES.iter().map(|t| format!("'{}'", t)).collect::<Vec<_>>().join(","));
    c.prepare(&sql).unwrap().query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect()
}

#[test]
fn bulk_indexes_restored_after_failed_import() {
    let (_g, db) = tmpdb("s7-fail");
    let c = open(&db);
    let before = index_names(&c);
    assert!(!before.is_empty());
    let r: anyhow::Result<()> = vm::with_bulk_indexes_dropped(&c, || {
        // indexes are gone during the load
        assert!(index_names(&c).is_empty());
        Err(anyhow::anyhow!("simulated failure mid-load"))
    });
    assert!(r.is_err());
    assert_eq!(index_names(&c), before, "indexes must come back after an error");
}

#[test]
fn bulk_indexes_restored_after_panic() {
    let (_g, db) = tmpdb("s7-panic");
    let c = open(&db);
    let before = index_names(&c);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: anyhow::Result<()> = vm::with_bulk_indexes_dropped(&c, || panic!("boom mid-load"));
    }));
    assert!(res.is_err(), "panic must propagate");
    assert_eq!(index_names(&c), before, "indexes must come back after a panic");
}

#[test]
fn bulk_index_definitions_are_unchanged() {
    let (_g, db) = tmpdb("s7-same");
    let c = open(&db);
    let defs = |c: &rusqlite::Connection| -> Vec<(String, String)> {
        c.prepare("SELECT name, sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL AND tbl_name IN ('vm_fs','vm_blobs','vm_file_chunks') ORDER BY name").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect()
    };
    let before = defs(&c);
    vm::with_bulk_indexes_dropped(&c, || Ok(())).unwrap();
    assert_eq!(defs(&c), before);
}

#[test]
fn sync_detects_same_size_edit_within_one_second() {
    // Two writes of equal length inside the same second. A real edit moves the nanosecond mtime
    // but not necessarily the second. The old check (size + second mtime) misses it; the ns check
    // catches it. Pin only the SECOND so the nanoseconds reflect the second write.
    let (_g, db) = tmpdb("mtime-same-sec");
    let host = _g.path().join("root");
    std::fs::create_dir_all(&host).unwrap();
    let f = host.join("cfg");
    std::fs::write(&f, b"AAAA").unwrap();
    let c = open(&db);
    vm::vm_sync_from_host(&c, &host).unwrap();
    let t1 = std::fs::metadata(&f).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap();
    std::fs::write(&f, b"BBBB").unwrap();
    let t2 = std::fs::metadata(&f).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap();
    // only meaningful if the two writes landed in the same second with different nanoseconds
    if t1.as_secs() != t2.as_secs() || t1.subsec_nanos() == t2.subsec_nanos() {
        eprintln!("skip: writes not in the same second with distinct ns on this filesystem");
        return;
    }
    let pinned = std::time::UNIX_EPOCH + std::time::Duration::new(t2.as_secs(), 0);
    let times = std::fs::FileTimes::new().set_modified(pinned);
    std::fs::File::options().write(true).open(&f).unwrap().set_times(times).unwrap();
    // whole-second pin: same second as before, but the nanoseconds now differ from the stored ones
    vm::vm_sync_from_host(&c, &host).unwrap();
    assert_eq!(vm::vm_cat(&c, "/cfg").unwrap(), b"BBBB", "same-size same-second edit must be picked up");
}

#[test]
fn write_during_read_leaves_ns_unknown() {
    // A reader that rewrites the host file in the middle of the read. The ns stamp must not be
    // recorded as the post-write value, or the next sync would trust content that is partly old.
    use std::io::Read;
    let (_g, db) = tmpdb("race-read");
    let host = _g.path().join("root");
    std::fs::create_dir_all(&host).unwrap();
    let f = host.join("big");
    std::fs::write(&f, vec![b'a'; 3 * 65536]).unwrap();
    struct Mutator<'a> { inner: std::fs::File, path: &'a std::path::Path, done: bool, n: usize }
    impl Read for Mutator<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if !self.done && self.n >= 65536 {
                // concurrent writer changes the file after the first chunk has been read
                std::fs::write(self.path, vec![b'b'; 3 * 65536])?;
                self.done = true;
            }
            let k = self.inner.read(out)?;
            self.n += k;
            Ok(k)
        }
    }
    let c = open(&db);
    let mut m = Mutator { inner: std::fs::File::open(&f).unwrap(), path: &f, done: false, n: 0 };
    vm::write_file_checked_pub(&c, &f, "/big", 0o644, 1, &mut m).unwrap();
    let ns: i64 = c.query_row("SELECT mtime_ns FROM vm_fs WHERE path='/big'", [], |r| r.get(0)).unwrap();
    assert_eq!(ns, 0, "a file that changed during the read must not carry a trusted ns stamp");
}

#[test]
fn full_verify_catches_stale_rows_that_metadata_check_misses() {
    // Simulates a row from before schema v7: no ns stamp, and the stored content is stale while
    // size, second and mode still match the host. Normal sync trusts the metadata and misses it;
    // a full verify re-reads and rewrites it.
    let (_g, db) = tmpdb("full-verify");
    let host = _g.path().join("root");
    std::fs::create_dir_all(&host).unwrap();
    let f = host.join("cfg");
    std::fs::write(&f, b"BBBB").unwrap();
    let c = open(&db);
    vm::vm_sync_from_host(&c, &host).unwrap();
    // Corrupt the stored row into a stale, metadata-identical version: same mode and same mtime
    // as the host, but different content and no ns stamp. Capture the host's real mtime/mode first.
    let hmd = std::fs::symlink_metadata(&f).unwrap();
    let hmode = (std::os::unix::fs::PermissionsExt::mode(&hmd.permissions()) & 0o7777) as i64;  // VM stores permission bits only
    let hsec = hmd.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    vm::fs_put_file_stream_pub(&c, "/cfg", hmode, hsec, b"AAAA").unwrap();
    c.execute("UPDATE vm_fs SET mtime_ns=0 WHERE path='/cfg'", []).unwrap();
    let meta_only = vm::vm_sync_report(&c, &host).unwrap();
    assert_eq!(meta_only.updated, 0, "metadata-only sync misses the stale content");
    assert_eq!(vm::vm_cat(&c, "/cfg").unwrap(), b"AAAA");
    let row: (i64, i64, i64, i64) = c.query_row("SELECT size, mtime, mtime_ns, mode FROM vm_fs WHERE path='/cfg'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    assert_eq!((row.0, row.1, row.2, row.3), (4, hsec, 0, hmode), "precondition: size, second, mode match host; ns unknown");
    let full = vm::vm_sync_full(&c, &host).unwrap();
    assert_eq!(full.updated, 1, "full verify must rewrite the stale row");
    assert_eq!(vm::vm_cat(&c, "/cfg").unwrap(), b"BBBB");
}

#[test]
fn symlinked_directory_children_materialize_under_the_real_directory() {
    // /bin -> /usr/bin is a directory link, and /bin/sh is a child of it. The child must be
    // written under usr/bin (where the VM resolves it), never through the link into a path that
    // does not exist in the tree. Checked with lstat on the scratch tree, not through the link.
    let (_g, db) = tmpdb("symlink-child");
    let c = open(&db);
    vm::vm_add_symlink(&c, "/bin", "/usr/bin").unwrap();
    vm::vm_add_bytes(&c, "/usr/bin/dash", b"sh-bytes", 0o755, 1).unwrap();
    vm::vm_add_symlink(&c, "/bin/sh", "/usr/bin/dash").unwrap();
    let dest = _g.path().join("scratch");
    vm::vm_materialize_tree(&c, &dest).unwrap();
    let sh = dest.join("usr/bin/sh");
    let md = std::fs::symlink_metadata(&sh).expect("usr/bin/sh must exist as a link");
    assert!(md.file_type().is_symlink(), "usr/bin/sh must be a symlink");
    assert_eq!(std::fs::read_link(&sh).unwrap(), std::path::PathBuf::from("/usr/bin/dash"));
    // the link /bin itself must remain a link, not become a real directory holding copies
    assert!(std::fs::symlink_metadata(dest.join("bin")).unwrap().file_type().is_symlink(), "/bin must stay a link");
    assert!(!std::fs::symlink_metadata(dest.join("usr/bin")).unwrap().file_type().is_symlink(), "usr/bin must be a real directory");
}

#[test]
fn noop_round_trip_keeps_alias_links_and_real_files() {
    // /bin -> /usr/bin with /bin/sh and /bin/rm links, plus real files at /usr/bin/rm and
    // /usr/bin/dash. A materialize followed by a sync must change nothing: the links are not
    // deleted, /usr/bin/sh is not created as a new row, and the real /usr/bin/rm is not deleted.
    let (_g, db) = tmpdb("noop-alias");
    let c = open(&db);
    vm::vm_add_symlink(&c, "/bin", "/usr/bin").unwrap();
    vm::vm_add_bytes(&c, "/usr/bin/dash", b"sh", 0o755, 1).unwrap();
    vm::vm_add_bytes(&c, "/usr/bin/rm", b"rm", 0o755, 1).unwrap();
    vm::vm_add_symlink(&c, "/bin/sh", "/usr/bin/dash").unwrap();
    vm::vm_add_symlink(&c, "/bin/rm", "/usr/bin/rm").unwrap();
    let snap = |c: &rusqlite::Connection| -> Vec<String> {
        c.prepare("SELECT path||' '||kind||' '||coalesce(link_target,'') FROM vm_fs ORDER BY path").unwrap()
            .query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect()
    };
    let before = snap(&c);
    let dest = _g.path().join("scratch");
    vm::vm_materialize_tree(&c, &dest).unwrap();
    let rep = vm::vm_sync_report(&c, &dest).unwrap();
    assert_eq!((rep.created, rep.updated, rep.deleted), (0, 0, 0), "no-op round trip must change nothing");
    assert_eq!(snap(&c), before, "DB rows must be identical after the round trip");
}

#[test]
fn v9_agent_steps_gains_truncation_columns_and_keeps_rows() {
    // A database written by the first agent-step build: agent_steps has no truncation columns and
    // user_version is 9. Opening it must add both columns, keep the existing row, and let a new step
    // record its truncation flags.
    let (_g, db) = tmpdb("v9-agent");
    {
        let c = open(&db);
        c.execute_batch("DROP TABLE agent_steps;
            CREATE TABLE agent_steps(step_id INTEGER PRIMARY KEY, cmd TEXT NOT NULL, exit_code INTEGER,
              timed_out INTEGER NOT NULL DEFAULT 0, rolled_back INTEGER NOT NULL DEFAULT 0, duration_ms INTEGER NOT NULL,
              stdout BLOB, stderr BLOB, snap_before TEXT NOT NULL, snap_after TEXT, fs_diff_json TEXT NOT NULL,
              created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')));
            INSERT INTO agent_steps(cmd, exit_code, duration_ms, stdout, stderr, snap_before, fs_diff_json)
              VALUES ('old-step', 0, 5, x'6f6c64', x'', 'step-before-1', '{}');
            PRAGMA user_version=9;").unwrap();
    }
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let ver: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(ver, vm::VM_USER_VERSION as i64);
    let cols: Vec<String> = c.prepare("PRAGMA table_info(agent_steps)").unwrap()
        .query_map([], |r| r.get::<_, String>(1)).unwrap().map(|x| x.unwrap()).collect();
    assert!(cols.iter().any(|x| x == "stdout_truncated") && cols.iter().any(|x| x == "stderr_truncated"));
    let (cmd, st): (String, i64) = c.query_row("SELECT cmd, stdout_truncated FROM agent_steps WHERE cmd='old-step'", [],
        |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((cmd.as_str(), st), ("old-step", 0), "old row kept, flag defaults to 0");
    c.execute("INSERT INTO agent_steps(cmd, exit_code, duration_ms, stdout, stderr, stdout_truncated, stderr_truncated, snap_before, fs_diff_json)
        VALUES ('new', 0, 1, x'', x'', 1, 0, 'b', '{}')", []).unwrap();
}

#[test]
fn editing_a_materialised_tree_cannot_change_what_the_next_materialise_returns() {
    // Regression: cached files were hard-linked into the tree, so an in-place edit in the tree changed the
    // cached bytes and the next materialise of the same unmodified database returned them.
    //
    // The tree and the cache must be on one filesystem for that sharing to happen at all: /tmp is usually a
    // separate tmpfs, so this test uses /var/tmp and its own cache root.
    let root = PathBuf::from(format!("/var/tmp/cache-isolation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let c = open(&root.join("v.db"));
    let body = format!("origin-bytes-{}", std::process::id());
    vm::vm_add_bytes(&c, "/f.txt", body.as_bytes(), 0o644, 1).unwrap();

    // warm the cache, then materialise again: only the second tree shares the cached entry
    let warm = root.join("t0");
    vm::vm_materialize_tree_with_cache(&c, &warm, &cache).unwrap();
    let first = root.join("t1");
    vm::vm_materialize_tree_with_cache(&c, &first, &cache).unwrap();
    assert_eq!(std::fs::read(first.join("f.txt")).unwrap(), body.as_bytes());

    // an agent rewrites the file in the tree, in place
    std::fs::write(first.join("f.txt"), b"EDITED-IN-THE-TREE").unwrap();

    // the database never changed, so a fresh materialise must still produce the original bytes
    let second = root.join("t2");
    vm::vm_materialize_tree_with_cache(&c, &second, &cache).unwrap();
    assert_eq!(std::fs::read(second.join("f.txt")).unwrap(), body.as_bytes(),
        "a tree edit must not reach the content cache");
    // and no cached entry was damaged
    let damaged = walkdir::WalkDir::new(&cache).into_iter().filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| std::fs::read(e.path()).map(|b| b == b"EDITED-IN-THE-TREE").unwrap_or(false))
        .count();
    assert_eq!(damaged, 0, "no cache entry may hold the tree edit");
    let _ = std::fs::remove_dir_all(&root);
}
