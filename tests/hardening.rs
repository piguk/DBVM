//! SQLite defenses for database files that may come from someone else (packages, Bases, uploaded VMs).
use selfdb::vm;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hard-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn connections_refuse_schema_tampering_and_untrusted_schema_functions() {
    let d = tmp("pragmas");
    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let trusted: i64 = c.query_row("PRAGMA trusted_schema", [], |r| r.get(0)).unwrap();
    assert_eq!(trusted, 0, "schema functions are not trusted");
    let cell_check: i64 = c.query_row("PRAGMA cell_size_check", [], |r| r.get(0)).unwrap();
    assert_eq!(cell_check, 1);
    // DEFENSIVE: sqlite_master cannot be edited, even after asking for writable_schema
    let _ = c.execute_batch("PRAGMA writable_schema=ON;");
    let r = c.execute("UPDATE sqlite_master SET sql=sql WHERE name='vm_fs'", []);
    assert!(r.is_err(), "sqlite_master must not be writable");
    // the policy triggers still work: their function is marked innocuous
    selfdb::policy::add(&c, &selfdb::policy::Rule::ReadonlyPrefix("/ro".into())).unwrap();
    assert!(vm::vm_add_bytes(&c, "/ro/x", b"x", 0o644, 1).is_err(), "the policy trigger runs and refuses");
    vm::vm_add_bytes(&c, "/rw/x", b"x", 0o644, 1).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_view_in_an_attached_file_cannot_call_a_function_with_side_effects() {
    // A hostile file defines a view that calls an application function. With trusted_schema off, only
    // functions marked innocuous may run from a stored schema.
    let d = tmp("view");
    let evil = d.join("evil.db");
    {
        let e = rusqlite::Connection::open(&evil).unwrap();
        e.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1); CREATE VIEW v AS SELECT side_effect(x) FROM t;").unwrap();
    }
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let h = hits.clone();
    c.create_scalar_function("side_effect", 1, rusqlite::functions::FunctionFlags::SQLITE_UTF8, move |_| {
        h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(1)
    }).unwrap();
    c.execute("ATTACH DATABASE ?1 AS evil", [evil.to_str().unwrap()]).unwrap();
    let r: rusqlite::Result<i64> = c.query_row("SELECT count(*) FROM evil.v", [], |r| r.get(0));
    assert!(r.is_err(), "the view's function call is refused");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "the function never ran");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_damaged_package_is_refused_and_a_package_is_never_written() {
    let d = tmp("pkg");
    let pkg = d.join("pkg.db");
    {
        let p = vm::init_vm_db_with_opts(pkg.to_str().unwrap(), true, true).unwrap();
        for i in 0..200 { vm::vm_add_bytes(&p, &format!("/usr/bin/t{}", i), format!("tool {}", i).repeat(50).as_bytes(), 0o755, 1).unwrap(); }
        p.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let target = vm::init_vm_db_with_opts(d.join("t.db").to_str().unwrap(), true, true).unwrap();
    // an intact package merges, and its file is unchanged afterwards
    let before = std::fs::read(&pkg).unwrap();
    selfdb::inject::inject(&target, &pkg).unwrap();
    assert!(std::fs::read(&pkg).unwrap() == before, "inject must not write the package");
    // overwrite the root page of vm_file_chunks: quick_check finds it before anything is merged
    let root: i64 = rusqlite::Connection::open(&pkg).unwrap()
        .query_row("SELECT rootpage FROM sqlite_master WHERE name='vm_file_chunks'", [], |r| r.get(0)).unwrap();
    let mut bytes = before.clone();
    let page = 8192;
    let at = (root as usize - 1) * page;
    for b in &mut bytes[at + 8..at + page] { *b = 0xA5; }
    std::fs::write(&pkg, &bytes).unwrap();
    let target2 = vm::init_vm_db_with_opts(d.join("t2.db").to_str().unwrap(), true, true).unwrap();
    let e = selfdb::inject::inject(&target2, &pkg).expect_err("a damaged package is refused");
    assert!(e.to_string().contains("quick_check") || e.to_string().contains("malformed"), "{}", e);
    let files: i64 = target2.query_row("SELECT count(*) FROM vm_fs WHERE path LIKE '/usr/bin/%'", [], |r| r.get(0)).unwrap();
    assert_eq!(files, 0, "nothing was merged from the damaged package");
    let _ = std::fs::remove_dir_all(&d);
}
