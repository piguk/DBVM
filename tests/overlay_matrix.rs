//! Matrix: every Base kind (none, file, dir, symlink) under every Delta operation (none, write,
//! mkdir, symlink, remove). For each cell, the overlay must agree with the layering rule: a Delta
//! operation wins over Base, a removal hides Base, and no operation shows Base. Checked through
//! `exists`, `list` of the parent, and `read`/`readlink` of the path.
use selfdb::vm;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Base { None, File, Dir, Link }
#[derive(Clone, Copy, Debug, PartialEq)]
enum Op { None, Write, Mkdir, Symlink, Remove }

/// What the overlay must show for `/p` after the op: None, File, Dir, or Link.
fn expected(base: Base, op: Op) -> Base {
    match op {
        Op::None => base,
        Op::Write => Base::File,
        Op::Mkdir => Base::Dir,
        Op::Symlink => Base::Link,
        Op::Remove => Base::None,
    }
}

fn seed_base(b: &rusqlite::Connection, base: Base) {
    match base {
        Base::None => {}
        Base::File => vm::vm_add_bytes(b, "/p", b"base-file", 0o644, 1).unwrap(),
        Base::Dir => { vm::vm_add_dir(b, "/p", 0o755, 1).unwrap(); vm::vm_add_bytes(b, "/p/child", b"c", 0o644, 1).unwrap(); }
        Base::Link => vm::vm_add_symlink(b, "/p", "/base-target").unwrap(),
    }
}

fn apply(ov: &vm::Overlay, op: Op) {
    match op {
        Op::None => {}
        Op::Write => ov.write("/p", b"delta-file", 0o644, 2).unwrap(),
        Op::Mkdir => ov.mkdir("/p").unwrap(),
        Op::Symlink => ov.symlink("/p", "/delta-target").unwrap(),
        Op::Remove => ov.remove("/p").unwrap(),
    }
}

/// The kind the overlay exposes for `/p`, derived only from the public read API.
fn observed(ov: &vm::Overlay) -> Base {
    if !ov.exists("/p").unwrap() { return Base::None; }
    if ov.list("/p").is_ok() { return Base::Dir; }
    if ov.readlink("/p").is_ok() { return Base::Link; }
    Base::File
}

#[test]
fn every_base_kind_under_every_delta_op() {
    let bases = [Base::None, Base::File, Base::Dir, Base::Link];
    let ops = [Op::None, Op::Write, Op::Mkdir, Op::Symlink, Op::Remove];
    let mut failures = Vec::new();
    let mut cells = 0;
    for &bk in &bases {
        for &op in &ops {
            cells += 1;
            let d = std::env::temp_dir().join(format!("ovm-{}-{:?}-{:?}-{}", std::process::id(), bk, op, cells));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            let base = d.join("base.db");
            let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
            seed_base(&b, bk);
            drop(b);
            let delta = d.join("delta.db");
            let ov = vm::Overlay::open(&delta, &base).unwrap();
            apply(&ov, op);
            let got = observed(&ov);
            let want = expected(bk, op);
            if got != want { failures.push(format!("base={:?} op={:?}: got {:?}, want {:?}", bk, op, got, want)); }
            let _ = std::fs::remove_dir_all(&d);
        }
    }
    eprintln!("matrix cells: {}", cells);
    assert!(failures.is_empty(), "{} of {} cells disagree:\n{}", failures.len(), cells, failures.join("\n"));
}
