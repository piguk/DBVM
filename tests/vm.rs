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
    assert!(vm::vm_verify(&c).unwrap().contains("integrity=ok"));
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
