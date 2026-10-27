#![cfg(feature = "fuse")]
//! Writes through a mounted VM, end to end. These run the test binary again inside `unshare -r`, because
//! mounting needs a user namespace on this machine (no root, no passwordless sudo). The child does the
//! real work; the parent asserts on its exit status.
use std::process::Command;

const ENV_IN_NS: &str = "MW_IN_NS";

/// Run `test_name` again inside a user namespace, returning (exit success, child stderr).
fn in_userns(test_name: &str) -> (bool, String) {
    if std::env::var(ENV_IN_NS).is_ok() { panic!("child must not re-enter"); }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new("unshare")
        // -r maps us to root in a new user ns; -m gives a private mount ns so FUSE may mount there
        .args(["-r", "-m", exe.to_str().unwrap(), "--exact", test_name, "--nocapture"])
        .env(ENV_IN_NS, "1")
        .output()
        .expect("run unshare");
    (out.status.success(), String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout))
}

fn is_child() -> bool { std::env::var(ENV_IN_NS).is_ok() }

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("mw-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Mount the db at `mnt` in the background and wait until it is live. Dropping the handle unmounts.
fn mount(db: &std::path::Path, mnt: &std::path::Path) -> fuser::BackgroundSession {
    std::fs::create_dir_all(mnt).unwrap();
    let h = selfdb::fuse::fuse_impl::mount_vm_background(db.to_str().unwrap(), mnt.to_str().unwrap()).unwrap();
    for _ in 0..40 {
        if std::fs::read_dir(mnt).map(|mut d| d.next().is_some()).unwrap_or(false) { break; }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    h
}

// ---- rename must keep the file's content ------------------------------------------------

#[test]
fn rename_keeps_the_file_content() {
    if !is_child() { let (ok, out) = in_userns("rename_keeps_the_file_content"); assert!(ok, "child failed:\n{}", out); return; }
    use selfdb::vm;
    let d = tmp("rename");
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&c, "/data/f.txt", b"CONTENT-ABC-0123456789", 0o644, 1).unwrap();
    drop(c);
    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    std::fs::rename(mnt.join("data/f.txt"), mnt.join("data/g.txt")).expect("mv must succeed");
    let got = std::fs::read(mnt.join("data/g.txt")).expect("the renamed file must be readable");
    assert_eq!(got, b"CONTENT-ABC-0123456789", "content must survive the rename");
    assert!(!mnt.join("data/f.txt").exists(), "the old name must be gone");
    drop(h);
    // and the database agrees: the chunk list moved with the row
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/data/g.txt").unwrap(), b"CONTENT-ABC-0123456789");
    let stale: i64 = c.query_row("SELECT count(*) FROM vm_file_chunks WHERE path='/data/f.txt'", [], |r| r.get(0)).unwrap();
    assert_eq!(stale, 0, "no chunk may stay behind at the old path");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn renaming_a_directory_moves_its_children_and_their_content() {
    if !is_child() { let (ok, out) = in_userns("renaming_a_directory_moves_its_children_and_their_content"); assert!(ok, "child failed:\n{}", out); return; }
    use selfdb::vm;
    let d = tmp("renamedir");
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&c, "/old/a.txt", b"AAA", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/old/sub/b.txt", b"BBB", 0o644, 1).unwrap();
    drop(c);
    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    std::fs::rename(mnt.join("old"), mnt.join("new")).expect("mv dir must succeed");
    assert_eq!(std::fs::read(mnt.join("new/a.txt")).expect("child file readable"), b"AAA");
    assert_eq!(std::fs::read(mnt.join("new/sub/b.txt")).expect("nested child readable"), b"BBB");
    assert!(!mnt.join("old").exists(), "the old directory must be gone");
    drop(h);
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/new/sub/b.txt").unwrap(), b"BBB");
    let stale: i64 = c.query_row("SELECT count(*) FROM vm_file_chunks WHERE path LIKE '/old/%'", [], |r| r.get(0)).unwrap();
    assert_eq!(stale, 0, "no chunk may stay under the old directory");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_refused_unlink_reports_an_error_and_keeps_the_entry() {
    if !is_child() { let (ok, out) = in_userns("a_refused_unlink_reports_an_error_and_keeps_the_entry"); assert!(ok, "child failed:\n{}", out); return; }
    use selfdb::{vm, policy::{self, Rule}};
    let d = tmp("unlinkdeny");
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&c, "/tests/eval.sh", b"EVAL", 0o644, 1).unwrap();
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    drop(c);
    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    let err = std::fs::remove_file(mnt.join("tests/eval.sh")).expect_err("the policy must refuse the unlink");
    // a refused unlink is EACCES, not EIO and not success
    assert_eq!(err.raw_os_error(), Some(libc::EACCES), "a policy refusal is EACCES: {:?}", err);
    // and the file must still be there, in the mount view and in the database
    assert!(mnt.join("tests/eval.sh").exists(), "a refused unlink must not remove the entry from the view");
    assert_eq!(std::fs::read(mnt.join("tests/eval.sh")).unwrap(), b"EVAL");
    drop(h);
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/tests/eval.sh").unwrap(), b"EVAL");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_refused_rmdir_and_rename_report_eacces() {
    if !is_child() { let (ok, out) = in_userns("a_refused_rmdir_and_rename_report_eacces"); assert!(ok, "child failed:\n{}", out); return; }
    use selfdb::{vm, policy::{self, Rule}};
    let d = tmp("rmdirdeny");
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&c, "/tests/keep/x.txt", b"X", 0o644, 1).unwrap();
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    drop(c);
    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    // rename a file out of the protected prefix: the UPDATE is refused
    let r = std::fs::rename(mnt.join("tests/keep/x.txt"), mnt.join("moved.txt"));
    assert!(r.is_err(), "a refused rename must fail");
    assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EACCES));
    assert!(mnt.join("tests/keep/x.txt").exists(), "the file stays where it was");
    drop(h);
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/tests/keep/x.txt").unwrap(), b"X");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_refused_mkdir_and_symlink_report_eacces() {
    if !is_child() { let (ok, out) = in_userns("a_refused_mkdir_and_symlink_report_eacces"); assert!(ok, "child failed:\n{}", out); return; }
    use selfdb::{vm, policy::{self, Rule}};
    let d = tmp("mkdirdeny");
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&c, "/tests/x.txt", b"X", 0o644, 1).unwrap();
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    drop(c);
    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    let r = std::fs::create_dir(mnt.join("tests/new"));
    assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EACCES), "a refused mkdir is EACCES, not EIO");
    let r = std::os::unix::fs::symlink("x.txt", mnt.join("tests/link"));
    assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EACCES), "a refused symlink is EACCES, not EIO");
    drop(h);
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let created: i64 = c.query_row("SELECT count(*) FROM vm_fs WHERE path IN ('/tests/new', '/tests/link')", [], |r| r.get(0)).unwrap();
    assert_eq!(created, 0, "nothing was created");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_directory_under_a_linked_ancestor_lists_and_opens_its_children() {
    // /lib is a link to /usr/lib and /lib/sub exists only under its database path. The mount shows it at
    // usr/lib/sub. Listing it and opening its child used to join the database path with the name and
    // look the result up among view paths, so the directory showed up empty.
    if !is_child() { let (ok, out) = in_userns("a_directory_under_a_linked_ancestor_lists_and_opens_its_children"); assert!(ok, "child failed:\n{}", out); return; }
    use selfdb::vm;
    let d = tmp("linkedparent");
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_dir(&c, "/usr/lib", 0o755, 1).unwrap();
    vm::vm_add_symlink(&c, "/lib", "/usr/lib").unwrap();
    vm::vm_add_bytes(&c, "/lib/sub/x.txt", b"X", 0o644, 1).unwrap();
    drop(c);
    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    let names: Vec<String> = std::fs::read_dir(mnt.join("usr/lib/sub")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
    assert_eq!(names, vec!["x.txt".to_string()]);
    assert_eq!(std::fs::read(mnt.join("usr/lib/sub/x.txt")).unwrap(), b"X");
    drop(h);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_large_sequential_write_completes_and_its_hash_is_exact_after_unmount() {
    // Each write used to recompute the whole-file hash, so writing a file sequentially cost O(n²): a
    // 200 MB file did not finish in minutes. The deferral itself is asserted below; the time bound only
    // catches a gross regression.
    if !is_child() { let (ok, out) = in_userns("a_large_sequential_write_completes_and_its_hash_is_exact_after_unmount"); assert!(ok, "child failed:\n{}", out); return; }
    use selfdb::vm;
    use std::io::Write;
    let d = tmp("bigwrite");
    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    let mut x = 0x9E3779B97F4A7C15u64;
    let data: Vec<u8> = (0..32 * 1024 * 1024).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; (x >> 24) as u8 }).collect();
    let started = std::time::Instant::now();
    let mut f = std::fs::File::create(mnt.join("big")).unwrap();
    for piece in data.chunks(128 * 1024) { f.write_all(piece).unwrap(); }
    drop(f);
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(60), "32 MiB took {:?}", took);
    assert!(std::fs::read(mnt.join("big")).unwrap() == data, "content read back through the mount");
    // the mount defers the whole-file hash instead of recomputing it per write; a read-only look must
    // not finalize it, so it opens the file directly rather than through vm_open
    let ro = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let deferred: i64 = ro.query_row("SELECT count(*) FROM vm_dirty WHERE path='/big'", [], |r| r.get(0)).unwrap();
    assert_eq!(deferred, 1, "the hash of a file written through the mount is deferred");
    drop(ro);
    drop(h);
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let pending: i64 = c.query_row("SELECT count(*) FROM vm_dirty", [], |r| r.get(0)).unwrap();
    assert_eq!(pending, 0, "unmount finalizes pending hashes");
    let hash: Vec<u8> = c.query_row("SELECT hash FROM vm_fs WHERE path='/big'", [], |r| r.get(0)).unwrap();
    use sha2::{Digest, Sha256};
    assert_eq!(hash, Sha256::digest(&data).to_vec());
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_overwrite_truncates_and_an_exclusive_create_works() {
    // The mount announces FUSE_ATOMIC_O_TRUNC, so O_TRUNC is the open's job; ignoring it left the old
    // tail after `echo hi > f`. And the create reply carried the open(2) flags as FOPEN_* flags, so
    // O_EXCL (bit 7) read as FOPEN_PASSTHROUGH and every exclusive create failed with EIO.
    if !is_child() { let (ok, out) = in_userns("an_overwrite_truncates_and_an_exclusive_create_works"); assert!(ok, "child failed:\n{}", out); return; }
    use selfdb::vm;
    use std::io::Write;
    let d = tmp("trunc-excl");
    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    std::fs::write(mnt.join("f"), b"hello world\n").unwrap();
    std::fs::write(mnt.join("f"), b"hi\n").unwrap();
    assert_eq!(std::fs::read(mnt.join("f")).unwrap(), b"hi\n", "O_TRUNC on open");
    // O_CREAT|O_TRUNC on an existing file goes through `create` when the kernel has no cached entry
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(mnt.join("f")).unwrap();
    f.write_all(b"x").unwrap();
    drop(f);
    assert_eq!(std::fs::read(mnt.join("f")).unwrap(), b"x", "O_TRUNC on create of an existing file");
    let mut g = std::fs::OpenOptions::new().write(true).create_new(true).open(mnt.join("g")).expect("O_EXCL create");
    g.write_all(b"new").unwrap();
    drop(g);
    let again = std::fs::OpenOptions::new().write(true).create_new(true).open(mnt.join("g"));
    assert_eq!(again.unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
    drop(h);
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/f").unwrap(), b"x");
    assert_eq!(vm::vm_cat(&c, "/g").unwrap(), b"new");
    let _ = std::fs::remove_dir_all(&d);
}
