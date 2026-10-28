//! "Everything below a directory" is matched by bytes: names with `_` or `%`, and names that differ only
//! in case, are other directories. (LIKE 'dir/%' read `_` as a wildcard and ignored ASCII case: deleting
//! `/a_b` took `/aXb` along, and `/Docs` took `/docs`.)
use selfdb::vm;
use std::path::PathBuf;

struct Dir(PathBuf);
impl Drop for Dir { fn drop(&mut self) { let _ = vm::remove_tree(&self.0); } }

fn dir(name: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("prefix-{}-{}", name, std::process::id()));
    let _ = vm::remove_tree(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

fn db(d: &Dir, name: &str) -> rusqlite::Connection {
    let c = vm::init_vm_db_with_opts(d.0.join(name).to_str().unwrap(), true, true).unwrap();
    for p in ["/a_b/x", "/aXb/y", "/Docs/a", "/docs/b", "/p%q/z", "/pXXq/w"] { vm::vm_add_bytes(&c, p, b"1", 0o644, 1).unwrap(); }
    c
}

fn exists(c: &rusqlite::Connection, p: &str) -> bool { vm::vm_cat(c, p).is_ok() }

#[test]
fn whiteouts_in_an_upper_delete_only_their_own_subtree() {
    let d = dir("upper");
    let c = db(&d, "v.db");
    let upper = d.0.join("upper");
    std::fs::create_dir_all(&upper).unwrap();
    for name in ["a_b", "Docs", "p%q"] {
        let p = std::ffi::CString::new(upper.join(name).to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mknod(p.as_ptr(), libc::S_IFCHR, 0) }, 0, "a whiteout: {}", std::io::Error::last_os_error());
    }
    vm::vm_apply_upper(&c, &upper).unwrap();
    assert!(!exists(&c, "/a_b/x") && !exists(&c, "/Docs/a") && !exists(&c, "/p%q/z"));
    assert!(exists(&c, "/aXb/y") && exists(&c, "/docs/b") && exists(&c, "/pXXq/w"), "other directories stay");
}

#[test]
fn a_read_only_rule_covers_its_own_path_only() {
    let d = dir("policy");
    let c = db(&d, "v.db");
    selfdb::policy::add(&c, &selfdb::policy::Rule::ReadonlyPrefix("/Docs".into())).unwrap();
    selfdb::policy::add(&c, &selfdb::policy::Rule::ReadonlyPrefix("/a_b".into())).unwrap();
    assert!(vm::vm_add_bytes(&c, "/Docs/new", b"x", 0o644, 1).is_err(), "the protected directory");
    assert!(vm::vm_add_bytes(&c, "/a_b/new", b"x", 0o644, 1).is_err());
    vm::vm_add_bytes(&c, "/docs/new", b"x", 0o644, 1).expect("another directory, differing in case");
    vm::vm_add_bytes(&c, "/aXb/new", b"x", 0o644, 1).expect("another directory, matching the old wildcard");
}

#[test]
fn removing_through_an_overlay_keeps_other_directories() {
    let d = dir("overlay");
    drop(db(&d, "base.db"));
    let ov = vm::Overlay::open(&d.0.join("delta.db"), &d.0.join("base.db")).unwrap();
    ov.write("/a_b/delta", b"d", 0o644, 1).unwrap();
    ov.write("/aXb/delta", b"d", 0o644, 1).unwrap();
    ov.remove("/a_b").unwrap();
    ov.remove("/Docs").unwrap();
    assert!(!ov.exists("/a_b/x").unwrap() && !ov.exists("/a_b/delta").unwrap() && !ov.exists("/Docs/a").unwrap());
    assert!(ov.exists("/aXb/y").unwrap() && ov.exists("/aXb/delta").unwrap() && ov.exists("/docs/b").unwrap());
    assert_eq!(ov.list("/aXb").unwrap(), vec!["delta".to_string(), "y".to_string()]);
}
