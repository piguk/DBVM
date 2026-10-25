//! Two agents on one read-only Base, each with its own Delta. This is the "parallel tasks share an
//! environment" case: neither Delta may see or lose the other's writes, and Base must stay untouched.
use selfdb::vm;
use std::path::PathBuf;

fn setup(name: &str) -> (PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("par-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/app/shared.txt", b"BASE-CONTENT", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/app/big.bin", &vec![0x11u8; 2 * vm::CHUNK_SIZE], 0o644, 1).unwrap();
    drop(b);
    (d, base)
}

#[test]
fn two_deltas_materialize_the_merged_view_and_write_back_only_their_own_changes() {
    let (d, base) = setup("merged");
    let da = d.join("a.db");
    let db_ = d.join("b.db");
    let a = vm::Overlay::open(&da, &base).unwrap();
    let b = vm::Overlay::open(&db_, &base).unwrap();

    // both start from the same view
    let ta = d.join("ta");
    let tb = d.join("tb");
    a.materialize(&ta).unwrap();
    b.materialize(&tb).unwrap();
    assert_eq!(std::fs::read(ta.join("app/shared.txt")).unwrap(), b"BASE-CONTENT");
    assert_eq!(std::fs::read(tb.join("app/shared.txt")).unwrap(), b"BASE-CONTENT");

    // each agent edits the same file and adds one of its own
    std::fs::write(ta.join("app/shared.txt"), b"FROM-A").unwrap();
    std::fs::write(ta.join("app/only-a.txt"), b"A").unwrap();
    std::fs::write(tb.join("app/shared.txt"), b"FROM-B").unwrap();
    std::fs::write(tb.join("app/only-b.txt"), b"B").unwrap();

    a.absorb_tree(&ta).unwrap();
    b.absorb_tree(&tb).unwrap();

    // neither loses the other's file, and each keeps its own edit
    assert_eq!(a.read("/app/shared.txt").unwrap(), b"FROM-A");
    assert_eq!(b.read("/app/shared.txt").unwrap(), b"FROM-B");
    assert!(a.exists("/app/only-a.txt").unwrap());
    assert!(!a.exists("/app/only-b.txt").unwrap(), "A must not see B's file");
    assert!(b.exists("/app/only-b.txt").unwrap());
    assert!(!b.exists("/app/only-a.txt").unwrap(), "B must not see A's file");

    // Base is untouched by either
    let bc = vm::vm_open(base.to_str().unwrap()).unwrap();
    assert_eq!(vm::read_range(&bc, "/app/shared.txt", 0, 100).unwrap(), b"BASE-CONTENT");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_untouched_file_leaves_no_delta_entry() {
    let (d, base) = setup("sparse");
    let delta = d.join("delta.db");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let tree = d.join("t");
    ov.materialize(&tree).unwrap();
    // nothing changed at all
    let written = ov.absorb_tree(&tree).unwrap();
    assert_eq!(written, 0, "an unchanged tree writes nothing, got {}", written);
    let dc = rusqlite::Connection::open(&delta).unwrap();
    let rows: i64 = dc.query_row("SELECT count(*) FROM vm_fs WHERE path <> '/'", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 0, "Delta must stay empty, has {} rows", rows);

    // now change one byte in the middle of the 128 KiB file
    let f = tree.join("app/big.bin");
    let mut bytes = std::fs::read(&f).unwrap();
    bytes[vm::CHUNK_SIZE + 10] = 0x99;
    std::fs::write(&f, &bytes).unwrap();
    ov.absorb_tree(&tree).unwrap();
    let chunks: i64 = dc.query_row("SELECT count(*) FROM vm_file_chunks WHERE path='/app/big.bin'", [], |r| r.get(0)).unwrap();
    assert_eq!(chunks, 1, "a one-chunk change moves one chunk, got {}", chunks);
    assert_eq!(ov.read("/app/big.bin").unwrap(), bytes, "content round-trips");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_deleted_file_is_hidden_by_a_whiteout_not_by_touching_base() {
    let (d, base) = setup("del");
    let delta = d.join("delta.db");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let tree = d.join("t");
    ov.materialize(&tree).unwrap();
    std::fs::remove_file(tree.join("app/shared.txt")).unwrap();
    ov.absorb_tree(&tree).unwrap();
    assert!(!ov.exists("/app/shared.txt").unwrap(), "the deleted file is gone from the view");
    // a fresh materialize agrees: the whiteout survives a round trip
    let tree2 = d.join("t2");
    ov.materialize(&tree2).unwrap();
    assert!(!tree2.join("app/shared.txt").exists(), "the merge honours the whiteout");
    let bc = vm::vm_open(base.to_str().unwrap()).unwrap();
    assert_eq!(vm::read_range(&bc, "/app/shared.txt", 0, 100).unwrap(), b"BASE-CONTENT", "Base untouched");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_delta_verifies_deep_with_base_attached() {
    let (d, base) = setup("verify");
    let delta = d.join("delta.db");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let tree = d.join("t");
    ov.materialize(&tree).unwrap();
    std::fs::write(tree.join("app/shared.txt"), b"CHANGED").unwrap();
    std::fs::write(tree.join("app/new.txt"), b"NEW").unwrap();
    ov.absorb_tree(&tree).unwrap();
    let dc = ov.conn();
    let v = vm::vm_verify(dc, true).unwrap();
    assert!(v.contains("deep=ok"), "Delta with Base attached must verify deep: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

/// Two agent runs writing to separate Deltas over one Base, with no shared lock between them.
///
/// This is the property the Overlay path exists for: the tree-mirror path deletes any database path its
/// own tree does not show, so two concurrent runs on one database lose each other's writes (measured
/// earlier). With one Delta per task there is nothing to delete, and the lock only serialises runs that
/// share a Delta.
#[test]
fn separate_deltas_do_not_need_a_cross_task_lock() {
    let (d, base) = setup("nolock");
    let (da, db_) = (d.join("a.db"), d.join("b.db"));
    let a = vm::Overlay::open(&da, &base).unwrap();
    let b = vm::Overlay::open(&db_, &base).unwrap();
    let (ta, tb) = (d.join("ta"), d.join("tb"));
    a.materialize(&ta).unwrap();
    b.materialize(&tb).unwrap();
    // both write at the same time, from two threads
    std::fs::write(ta.join("app/a.txt"), b"A").unwrap();
    std::fs::write(tb.join("app/b.txt"), b"B").unwrap();
    let (ta2, tb2) = (ta.clone(), tb.clone());
    let ha = std::thread::spawn(move || a.absorb_tree(&ta2).map(|_| ()).map_err(|e| e.to_string()));
    let hb = std::thread::spawn(move || b.absorb_tree(&tb2).map(|_| ()).map_err(|e| e.to_string()));
    ha.join().unwrap().expect("A must write its own Delta");
    hb.join().unwrap().expect("B must write its own Delta");

    // reopen and check both survived, and neither saw the other's file
    let a = vm::Overlay::open(&da, &base).unwrap();
    let b = vm::Overlay::open(&db_, &base).unwrap();
    assert_eq!(a.read("/app/a.txt").unwrap(), b"A");
    assert_eq!(b.read("/app/b.txt").unwrap(), b"B");
    assert!(!a.exists("/app/b.txt").unwrap());
    assert!(!b.exists("/app/a.txt").unwrap());
    let bc = vm::vm_open(base.to_str().unwrap()).unwrap();
    assert_eq!(vm::read_range(&bc, "/app/shared.txt", 0, 100).unwrap(), b"BASE-CONTENT");
    let _ = std::fs::remove_dir_all(&d);
}

/// An unchanged tree must not white out the entries behind a directory link, and must leave Delta empty.
///
/// Found by the acceptance run: `seen` recorded view paths while the deletion pass compared database paths,
/// so /bin/sh and the other entries behind /bin -> /usr/bin looked deleted and were whited out.
#[test]
fn an_unchanged_tree_writes_no_whiteouts() {
    let (d, base) = setup("nowhiteout");
    let delta = d.join("delta.db");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    // a Base with a directory link, like a real rootfs: /bin -> /usr/bin
    {
        let bc = vm::vm_open(base.to_str().unwrap()).unwrap();
        vm::vm_add_bytes(&bc, "/usr/bin/tool", b"TOOL", 0o755, 1).unwrap();
        vm::vm_add_symlink(&bc, "/bin", "/usr/bin").unwrap();
        vm::vm_add_symlink(&bc, "/bin/tool", "/usr/bin/tool").unwrap();
    }
    let tree = d.join("t");
    ov.materialize(&tree).unwrap();
    ov.absorb_tree(&tree).unwrap();
    let dc = rusqlite::Connection::open(&delta).unwrap();
    let whites: i64 = dc.query_row("SELECT count(*) FROM vm_whiteouts", [], |r| r.get(0)).unwrap();
    assert_eq!(whites, 0, "an unchanged tree must not white anything out, got {}", whites);
    // and the view is unchanged. `/bin/tool` is a symlink in the view, so `read` on it is not the way to
    // reach the bytes: the kernel follows it to /usr/bin/tool. Both must still resolve.
    assert!(ov.exists("/bin/tool").unwrap(), "/bin/tool is still visible");
    assert!(ov.exists("/usr/bin/tool").unwrap(), "/usr/bin/tool is still visible");
    assert_eq!(ov.read("/usr/bin/tool").unwrap(), b"TOOL");
    let _ = std::fs::remove_dir_all(&d);
}
