//! Read-only Base + Delta overlay via ATTACH. Base is never written. Delta holds additions,
//! overwrites and whiteouts (deletions). A path is resolved in Delta first, then Base.
use selfdb::vm;
use selfdb::overlay::Layer;
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

fn setup_tree(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let d = std::env::temp_dir().join(format!("overlay-tree-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_dir(&b, "/work", 0o755, 1).unwrap();
    vm::vm_add_bytes(&b, "/work/src/a.rs", b"fn a(){}", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/work/src/deep/b.rs", b"fn b(){}", 0o644, 1).unwrap();
    vm::vm_add_symlink(&b, "/work/link", "src/a.rs").unwrap();
    drop(b);
    let delta = d.join("delta.db");
    (d, base, delta)
}

#[test]
fn directories_are_listed_from_the_merged_view() {
    let (d, base, delta) = setup_tree("list");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    assert_eq!(ov.locate("/work/src").unwrap(), Layer::Base);
    ov.write("/work/src/new.rs", b"fn n(){}", 0o644, 2).unwrap();
    let names = ov.list("/work/src").unwrap();
    assert_eq!(names, vec!["a.rs".to_string(), "deep".to_string(), "new.rs".to_string()]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn symlinks_are_read_as_targets_not_followed_by_read() {
    let (d, base, delta) = setup_tree("link");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    assert_eq!(ov.readlink("/work/link").unwrap(), "src/a.rs");
    assert!(ov.read("/work/link").is_err(), "read() refuses a symlink; callers use readlink()");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn removing_a_directory_hides_its_whole_base_subtree() {
    let (d, base, delta) = setup_tree("rmdir");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.remove("/work/src").unwrap();
    assert_eq!(ov.locate("/work/src").unwrap(), Layer::Missing);
    assert_eq!(ov.locate("/work/src/a.rs").unwrap(), Layer::Missing, "base child must be hidden");
    assert_eq!(ov.locate("/work/src/deep/b.rs").unwrap(), Layer::Missing, "base grandchild must be hidden");
    // a removed directory is "not found", not an empty directory
    assert!(ov.list("/work/src").is_err(), "listing a removed directory is an error");
    // the base is untouched
    let c = rusqlite::Connection::open(&base).unwrap();
    // the subtree is /work/src itself, a.rs, deep, deep/b.rs: four rows, all still present
    let n: i64 = c.query_row("SELECT count(*) FROM vm_fs WHERE path = '/work/src' OR path LIKE '/work/src/%'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 4);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn writing_under_a_removed_directory_makes_it_visible_again() {
    let (d, base, delta) = setup_tree("revive");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.remove("/work/src").unwrap();
    ov.write("/work/src/fresh.rs", b"x", 0o644, 2).unwrap();
    assert_eq!(ov.locate("/work/src").unwrap(), Layer::Delta, "ancestor created in delta is visible");
    assert_eq!(ov.locate("/work/src/a.rs").unwrap(), Layer::Missing, "old base children stay hidden");
    assert_eq!(ov.list("/work/src").unwrap(), vec!["fresh.rs".to_string()]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn symlink_in_delta_overrides_base_and_removal_hides_it() {
    let (d, base, delta) = setup_tree("dlink");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.symlink("/work/link", "src/deep/b.rs").unwrap();
    assert_eq!(ov.readlink("/work/link").unwrap(), "src/deep/b.rs");
    ov.remove("/work/link").unwrap();
    assert_eq!(ov.locate("/work/link").unwrap(), Layer::Missing);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn mkdir_then_list_shows_empty_directory() {
    let (d, base, delta) = setup_tree("mkdir");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.mkdir("/work/empty").unwrap();
    assert_eq!(ov.locate("/work/empty").unwrap(), Layer::Delta);
    assert!(ov.list("/work/empty").unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn list_matches_per_entry_resolution_on_a_mixed_tree() {
    // Every child that list() returns must resolve, and every resolvable direct child must be
    // returned. This pins the merged listing so an optimised list cannot change its result.
    let (d, base, delta) = setup_tree("listeq");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    // mixed state: a Delta override, a Delta addition, a whiteout, and a nested addition
    ov.write("/work/src/a.rs", b"override", 0o644, 2).unwrap();
    ov.write("/work/src/extra.rs", b"e", 0o644, 2).unwrap();
    ov.remove("/work/link").unwrap();
    ov.mkdir("/work/src/newdir").unwrap();
    ov.write("/work/src/newdir/inner", b"i", 0o644, 2).unwrap();
    let listed = ov.list("/work/src").unwrap();
    // reference answer: every direct child of /work/src that locate() resolves
    let mut want: Vec<String> = Vec::new();
    for cand in ["/work/src/a.rs", "/work/src/deep", "/work/src/extra.rs", "/work/src/newdir"] {
        if ov.locate(cand).unwrap() != Layer::Missing {
            want.push(cand.rsplit('/').next().unwrap().to_string());
        }
    }
    want.sort();
    assert_eq!(listed, want);
    assert!(!listed.contains(&"inner".to_string()), "grandchildren are not direct children");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn delta_directory_over_base_file_is_not_readable_as_a_file() {
    // Base has /data/item as a file; Delta replaces it with a directory. The Delta entry wins,
    // so reading /data/item as a file must fail, not fall through to the base file.
    let d = std::env::temp_dir().join(format!("overlay-typeover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/data/item", b"base file", 0o644, 1).unwrap();
    drop(b);
    let ov = vm::Overlay::open(&d.join("delta.db"), &base).unwrap();
    assert_eq!(ov.read("/data/item").unwrap(), b"base file");
    ov.remove("/data/item").unwrap();
    ov.mkdir("/data/item").unwrap();
    assert!(ov.read("/data/item").is_err(), "a Delta directory shadows the base file");
    assert_eq!(ov.locate("/data/item").unwrap(), Layer::Delta);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn whiteout_matches_exact_ancestors_only() {
    // a whiteout hides its path and descendants; it must not hide an unrelated sibling that
    // merely shares a prefix (/work/src vs /work/srcx)
    let d = std::env::temp_dir().join(format!("overlay-prefix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/work/src/a", b"a", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/work/srcx/b", b"b", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/top", b"t", 0o644, 1).unwrap();
    drop(b);
    let ov = vm::Overlay::open(&d.join("delta.db"), &base).unwrap();
    ov.remove("/work/src").unwrap();
    assert_eq!(ov.locate("/work/src/a").unwrap(), Layer::Missing, "descendant of removed dir hidden");
    assert_eq!(ov.read("/work/srcx/b").unwrap(), b"b", "sibling sharing a text prefix is not hidden");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn read_and_locate_agree_on_every_state() {
    // read() and locate() are computed by different code paths now; they must agree on the
    // layer for each state of a path (base, delta over base, whited out, recreated, missing).
    let (d, base, delta) = setup_tree("agree");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let check = |ov: &vm::Overlay, p: &str| {
        let loc = ov.locate(p).unwrap();
        let r = ov.read(p);
        match loc {
            Layer::Missing => assert!(r.is_err(), "{} missing but read succeeded", p),
            _ => assert!(r.is_ok(), "{} located {:?} but read failed: {:?}", p, loc, r.err()),
        }
    };
    check(&ov, "/work/src/a.rs");                 // base
    ov.write("/work/src/a.rs", b"o", 0o644, 2).unwrap();
    check(&ov, "/work/src/a.rs");                 // delta over base
    ov.remove("/work/src/a.rs").unwrap();
    check(&ov, "/work/src/a.rs");                 // whited out
    ov.write("/work/src/a.rs", b"r", 0o644, 3).unwrap();
    check(&ov, "/work/src/a.rs");                 // recreated
    ov.remove("/work/src").unwrap();
    check(&ov, "/work/src/deep/b.rs");            // under a removed dir
    let _ = std::fs::remove_dir_all(&d);
}
