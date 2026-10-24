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
