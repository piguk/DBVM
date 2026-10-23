//! Read-only Base + Delta overlay via ATTACH. Base is never written. Delta holds additions,
//! overwrites and whiteouts (deletions). A path is resolved in Delta first, then Base.
use selfdb::vm;
use std::path::{Path, PathBuf};

fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("overlay-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/etc/motd", b"base motd", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/etc/hosts", b"127.0.0.1 localhost", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/data/big", &vec![4u8; 150_000], 0o644, 1).unwrap();
    drop(b);
    let delta = d.join("delta.db");
    (d, base, delta)
}

fn base_hash(base: &Path) -> Vec<u8> {
    // a fingerprint of the base file's logical content, to prove it was never written
    let c = rusqlite::Connection::open(base).unwrap();
    let mut st = c.prepare("SELECT path, hash FROM vm_fs ORDER BY path").unwrap();
    let mut out = Vec::new();
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))).unwrap() {
        let (p, h) = r.unwrap();
        out.extend_from_slice(p.as_bytes()); out.extend_from_slice(&h.unwrap_or_default());
    }
    out
}

#[test]
fn reads_fall_through_to_base_and_delta_overrides() {
    let (d, base, delta) = setup("fallthrough");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    assert_eq!(ov.read("/etc/motd").unwrap(), b"base motd");
    assert_eq!(ov.read("/data/big").unwrap(), vec![4u8; 150_000]);
    ov.write("/etc/motd", b"delta motd", 0o644, 2).unwrap();
    assert_eq!(ov.read("/etc/motd").unwrap(), b"delta motd", "delta wins over base");
    assert_eq!(ov.read("/etc/hosts").unwrap(), b"127.0.0.1 localhost", "untouched paths still read from base");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn base_is_never_written() {
    let (d, base, delta) = setup("readonly");
    let before = base_hash(&base);
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.write("/etc/motd", b"x", 0o644, 2).unwrap();
    ov.remove("/data/big").unwrap();
    ov.write("/new", b"y", 0o644, 2).unwrap();
    assert_eq!(base_hash(&base), before, "base file contents must be unchanged");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn whiteout_hides_a_base_path() {
    let (d, base, delta) = setup("whiteout");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    assert!(ov.read("/data/big").is_ok());
    ov.remove("/data/big").unwrap();
    assert!(ov.read("/data/big").is_err(), "deleted base path reads as not found");
    assert!(ov.exists("/data/big").unwrap() == false);
    // base still has it: the deletion is a whiteout, not an edit to base
    let b = rusqlite::Connection::open(&base).unwrap();
    let n: i64 = b.query_row("SELECT count(*) FROM vm_fs WHERE path='/data/big'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rewrite_after_whiteout_is_visible() {
    let (d, base, delta) = setup("rewrite");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.remove("/etc/motd").unwrap();
    ov.write("/etc/motd", b"back", 0o644, 3).unwrap();
    assert_eq!(ov.read("/etc/motd").unwrap(), b"back");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn delta_stores_only_changes() {
    let (d, base, delta) = setup("size");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.write("/etc/motd", b"tiny", 0o644, 2).unwrap();
    let blob_bytes = {
        let c = rusqlite::Connection::open(&delta).unwrap();
        c.query_row("SELECT COALESCE(sum(length(content)),0) FROM vm_blobs", [], |r| r.get::<_, i64>(0)).unwrap()
    };
    assert!(blob_bytes < 100, "one small change must not copy base content, delta blob bytes = {}", blob_bytes);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn base_rejects_direct_writes_at_the_engine() {
    // Not a convention: even a caller holding the raw connection cannot write the base schema.
    let (d, base, delta) = setup("engine-ro");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let up = ov.conn().execute("UPDATE base.vm_fs SET size=999 WHERE path='/etc/motd'", []);
    assert!(up.is_err(), "UPDATE through the base schema must fail");
    let ins = ov.conn().execute("INSERT INTO base.vm_fs(path,kind,mode,mtime,size,compressed) VALUES ('/x','file',420,1,0,0)", []);
    assert!(ins.is_err(), "INSERT through the base schema must fail");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn base_blocks_use_base_dictionaries() {
    // A base packed with a trained dictionary must read through the overlay. Dictionary ids are
    // per database: the base's dictionary is not in Delta, so reads must use the base's own.
    let d = std::env::temp_dir().join(format!("overlay-dict-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let delta = d.join("delta.db");
    let rec = |i: u32| format!("{{\"schema\":\"com.example.record.v1\",\"kind\":\"inventory-item\",\"owner\":\"alpha\",\"id\":{},\"status\":\"active\",\"flags\":[\"audited\",\"synced\",\"archived-false\",\"public-false\"],\"labels\":{{\"region\":\"eu-west-1\",\"tier\":\"gold\"}},\"note\":\"generated record {} for owner alpha in the nightly batch run\"}}", i, i);
    {
        let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
        for i in 0..60u32 { vm::vm_add_bytes(&b, &format!("/t/{}", i), rec(i).as_bytes(), 0o644, 1).unwrap(); }
        vm::vm_train_dict(&b, 16384).unwrap();
        for i in 1000..1060u32 { vm::vm_add_bytes(&b, &format!("/d/{}", i - 1000), rec(i).as_bytes(), 0o644, 1).unwrap(); }
        let flag3: i64 = b.query_row("SELECT count(*) FROM vm_blobs WHERE compressed=3", [], |r| r.get(0)).unwrap();
        assert!(flag3 > 0, "base must contain dictionary-compressed blocks for this test");
    }
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    for i in 0..60u32 {
        let got = ov.read(&format!("/d/{}", i)).unwrap();
        assert_eq!(got, rec(1000 + i).as_bytes(), "base file {} decodes with its own dictionary", i);
    }
    let _ = std::fs::remove_dir_all(&d);
}
