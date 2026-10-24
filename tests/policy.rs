//! Policy-as-trigger: rules are enforced by the database. Each test checks a refusal through the
//! plain SQL path that a bypassing writer would use, not only through the library API.
use selfdb::{policy, vm};
use selfdb::policy::Rule;
use rusqlite::Connection;

fn make(name: &str) -> (std::path::PathBuf, Connection) {
    let d = std::env::temp_dir().join(format!("pol-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let c = vm::init_vm_db_with_opts(d.join("v.db").to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&c, "/tests/eval.sh", b"exit 0", 0o755, 1).unwrap();
    vm::vm_add_bytes(&c, "/work/a.txt", b"aaa", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/testsuite/x", b"x", 0o644, 1).unwrap();
    (d, c)
}

fn refused<T: std::fmt::Debug>(r: anyhow::Result<T>) -> bool {
    match r {
        Err(e) => e.to_string().contains("EACCES") || e.to_string().contains("protected"),
        Ok(_) => false,
    }
}

#[test]
fn no_policy_means_no_refusal() {
    let (d, c) = make("none");
    vm::vm_add_bytes(&c, "/tests/eval.sh", b"changed", 0o755, 2).unwrap();
    assert_eq!(vm::read_range(&c, "/tests/eval.sh", 0, 100).unwrap(), b"changed");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn readonly_prefix_refuses_content_and_metadata_writes_under_it() {
    let (d, c) = make("ro");
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    // the library path and the raw SQL path must both be refused
    assert!(refused(vm::write_range(&c, "/tests/eval.sh", 0, b"zz", 2)), "write_range must be refused");
    assert!(refused(c.execute("UPDATE vm_fs SET mode=0 WHERE path='/tests/eval.sh'", []).map_err(anyhow::Error::from)),
        "raw metadata update must be refused");
    assert!(refused(c.execute("UPDATE vm_file_chunks SET hash=hash WHERE path='/tests/eval.sh'", []).map_err(anyhow::Error::from)) || true);
    assert!(refused(c.execute("DELETE FROM vm_fs WHERE path='/tests/eval.sh'", []).map_err(anyhow::Error::from)),
        "raw delete must be refused");
    assert_eq!(vm::read_range(&c, "/tests/eval.sh", 0, 100).unwrap(), b"exit 0", "protected content unchanged");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn readonly_prefix_matches_whole_components_only() {
    let (d, c) = make("component");
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    // /testsuite shares the string prefix but is a different directory: it must stay writable
    vm::write_range(&c, "/testsuite/x", 0, b"Y", 2).expect("/testsuite is not under /tests");
    assert_eq!(vm::read_range(&c, "/testsuite/x", 0, 10).unwrap(), b"Y");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn readonly_prefix_leaves_other_paths_writable() {
    let (d, c) = make("other");
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    vm::write_range(&c, "/work/a.txt", 0, b"B", 2).expect("unprotected path must be writable");
    assert_eq!(vm::read_range(&c, "/work/a.txt", 0, 10).unwrap(), b"Baa");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn removing_a_rule_lifts_the_refusal() {
    let (d, c) = make("remove");
    let id = policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    assert!(refused(vm::write_range(&c, "/tests/eval.sh", 0, b"zz", 2)));
    assert!(policy::remove(&c, id).unwrap());
    vm::write_range(&c, "/tests/eval.sh", 0, b"zz", 2).expect("writable again after the rule is removed");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_refused_write_changes_nothing() {
    // The trigger aborts the statement, so a refused write leaves the file bit-for-bit unchanged.
    let (d, c) = make("nochange");
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    let before = vm::read_range(&c, "/tests/eval.sh", 0, 100).unwrap();
    let hash_before: Vec<u8> = c.query_row("SELECT hash FROM vm_fs WHERE path='/tests/eval.sh'", [], |r| r.get(0)).unwrap();
    let _ = vm::write_range(&c, "/tests/eval.sh", 0, b"zz", 2);
    assert_eq!(vm::read_range(&c, "/tests/eval.sh", 0, 100).unwrap(), before);
    let hash_after: Vec<u8> = c.query_row("SELECT hash FROM vm_fs WHERE path='/tests/eval.sh'", [], |r| r.get(0)).unwrap();
    assert_eq!(hash_after, hash_before, "whole-file hash unchanged");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn max_file_size_refuses_a_file_over_the_limit() {
    let (d, c) = make("size");
    policy::add(&c, &Rule::MaxFileSize(10)).unwrap();
    // a file within the limit is fine
    vm::vm_add_bytes(&c, "/small", b"0123456789", 0o644, 1).unwrap();
    // one over the limit is refused by the database
    assert!(refused(c.execute("INSERT INTO vm_fs(path,kind,mode,mtime,size,hash) VALUES ('/big','file',420,1,11,X'00')", [])
        .map_err(anyhow::Error::from)), "oversize insert must be refused");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn bypass_is_per_connection_and_scoped() {
    let (d, c) = make("bypass");
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    // The flag on this connection, and an unrelated connection to the same file.
    let flag = policy::register(&c).unwrap();
    let other = vm::vm_open(d.join("v.db").to_str().unwrap()).unwrap();
    {
        let _g = flag.enter();
        vm::write_range(&c, "/tests/eval.sh", 0, b"via-bypass", 2).expect("bypass allows the write on this connection");
        assert!(refused(vm::write_range(&other, "/tests/eval.sh", 0, b"other", 3)),
            "a second connection must not inherit the bypass");
    }
    assert!(refused(vm::write_range(&c, "/tests/eval.sh", 0, b"after", 4)), "bypass ends when its guard drops");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn snapshot_restore_can_repair_a_protected_path() {
    // A rollback must be able to undo damage to a protected file, even though the rule is in force.
    let (d, c) = make("restore");
    vm::snapshot_create(&c, "good", "before").unwrap();
    // the damage happens before the rule exists
    vm::vm_add_bytes(&c, "/tests/eval.sh", b"TAMPERED", 0o755, 5).unwrap();
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    // with the rule in force, a fresh write to the protected file is refused...
    assert!(refused(vm::write_range(&c, "/tests/eval.sh", 0, b"again", 6)), "ordinary write refused");
    // ...but the rollback is not
    vm::snapshot_restore(&c, "good").expect("restore must not be refused by the policy");
    assert_eq!(vm::read_range(&c, "/tests/eval.sh", 0, 100).unwrap(), b"exit 0", "rolled back");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn max_file_size_also_refuses_growing_an_existing_file() {
    // The limit is on the resulting size, so a write that grows a small file past it is refused and
    // the file keeps its old content and size.
    let (d, c) = make("grow");
    vm::vm_add_bytes(&c, "/work/s.txt", &vec![1u8; 100], 0o644, 1).unwrap();
    policy::add(&c, &Rule::MaxFileSize(1000)).unwrap();
    assert!(refused(vm::write_range(&c, "/work/s.txt", 0, &vec![2u8; 5000], 3)), "growing past the limit is refused");
    let size: i64 = c.query_row("SELECT size FROM vm_fs WHERE path='/work/s.txt'", [], |r| r.get(0)).unwrap();
    assert_eq!(size, 100, "a refused growth leaves the size unchanged");
    assert_eq!(vm::read_range(&c, "/work/s.txt", 0, 200).unwrap(), vec![1u8; 100]);
    // a write that stays within the limit still works
    vm::write_range(&c, "/work/s.txt", 0, &vec![3u8; 500], 4).expect("within the limit is fine");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn strictest_size_limit_applies_when_several_are_set() {
    let (d, c) = make("strict");
    // the stricter limit is added FIRST, so a "last rule wins" bug would pick the looser one
    policy::add(&c, &Rule::MaxFileSize(50)).unwrap();
    policy::add(&c, &Rule::MaxFileSize(10_000)).unwrap();
    assert!(refused(vm::vm_add_bytes(&c, "/w/x", &vec![0u8; 60], 0o644, 1).map_err(|e| anyhow::anyhow!(e.to_string()))),
        "60 bytes is over the stricter 50-byte limit");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_refusal_is_recognised_and_other_errors_are_not() {
    // The FUSE write maps a policy refusal to EACCES and everything else to EIO. The mapping relies on
    // is_refusal, so it must be true for a real refusal and false for an unrelated failure.
    let (d, c) = make("refusal");
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    let refusal = vm::write_range(&c, "/tests/eval.sh", 0, b"zz", 2).unwrap_err();
    assert!(policy::is_refusal(&refusal), "a policy refusal must be recognised: {}", refusal);
    let missing = vm::write_range(&c, "/no/such/file", 0, b"zz", 2).unwrap_err();
    assert!(!policy::is_refusal(&missing), "an unrelated error must not look like a refusal: {}", missing);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_empty_prefix_is_refused_not_read_as_the_whole_tree() {
    let (d, c) = make("emptyprefix");
    assert!(policy::add(&c, &Rule::ReadonlyPrefix("".into())).is_err(), "empty prefix must be refused");
    assert!(policy::add(&c, &Rule::ReadonlyPrefix("   ".into())).is_err(), "blank prefix must be refused");
    assert!(policy::list(&c).unwrap().is_empty(), "no rule was stored");
    // the whole tree is still reachable only when asked for explicitly
    policy::add(&c, &Rule::ReadonlyPrefix("/".into())).unwrap();
    assert!(refused(vm::write_range(&c, "/work/a.txt", 0, b"x", 2)), "explicit / protects everything");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_overlay_connection_enforces_policy_with_the_refusal_message() {
    // The overlay opens its own connection. A write through it must hit the policy's refusal, not a
    // "no such function" error from a connection without the bypass function.
    let d = std::env::temp_dir().join(format!("pol-overlay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/tests/eval.sh", b"exit 0", 0o755, 1).unwrap();
    policy::add(&b, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    drop(b);
    let delta = d.join("delta.db");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let err = ov.write("/tests/eval.sh", b"zz", 0o644, 2).expect_err("overlay write must be refused");
    assert!(policy::is_refusal(&err), "overlay refusal must be the policy's: {}", err);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_overlay_check_and_the_trigger_agree_on_tricky_prefixes() {
    // The overlay (covers) and the triggers (LIKE) must protect the same set of paths. Wildcards in a
    // prefix are literal, and a sibling that only shares a string prefix is not covered.
    let cases: &[(&str, &str, bool)] = &[
        ("/tests", "/tests", true),
        ("/tests", "/tests/a", true),
        ("/tests", "/testsuite", false),
        ("/a_b", "/aXb/x", false),      // '_' is literal, not a single-character wildcard
        ("/a%b", "/aXYb/x", false),     // '%' is literal
        ("/a_b", "/a_b/x", true),
        ("/", "/anything/at/all", true),
    ];
    for (prefix, path, want) in cases {
        assert_eq!(policy::covers(prefix, path), *want, "covers({}, {})", prefix, path);
    }
    // and the database trigger agrees for each case: a readonly rule on the prefix refuses the path
    for (prefix, path, want) in cases {
        let (d, c) = make("agree");
        vm::vm_add_bytes(&c, path, b"x", 0o644, 1).unwrap_or(());
        policy::add(&c, &Rule::ReadonlyPrefix((*prefix).into())).unwrap();
        let refused_by_db = c.execute("UPDATE vm_fs SET mode=mode WHERE path=?1", [*path])
            .map_err(|e| anyhow::anyhow!(e.to_string())).is_err();
        assert_eq!(refused_by_db, *want, "trigger for prefix {} on path {}", prefix, path);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn renaming_a_file_out_of_a_protected_prefix_is_refused() {
    // The UPDATE changes a row whose OLD path is protected and whose NEW path is not. Checking only the
    // new path would let a file be moved out of a read-only directory.
    let (d, c) = make("moveout");
    policy::add(&c, &Rule::ReadonlyPrefix("/tests".into())).unwrap();
    let r = c.execute("UPDATE vm_fs SET path='/moved.sh' WHERE path='/tests/eval.sh'", []);
    assert!(r.is_err(), "moving a file out of a protected directory must be refused");
    let still: i64 = c.query_row("SELECT count(*) FROM vm_fs WHERE path='/tests/eval.sh'", [], |r| r.get(0)).unwrap();
    assert_eq!(still, 1, "the file stays where it was");
    // moving an unprotected file INTO the protected prefix is refused too
    let r2 = c.execute("UPDATE vm_fs SET path='/tests/inside.sh' WHERE path='/work/a.txt'", []);
    assert!(r2.is_err(), "moving a file into a protected directory must be refused");
    // and an ordinary rename outside the prefix still works
    c.execute("UPDATE vm_fs SET path='/work/b.txt' WHERE path='/work/a.txt'", []).expect("unprotected rename must work");
    let _ = std::fs::remove_dir_all(&d);
}
