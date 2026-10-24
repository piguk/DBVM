//! Several Delta databases on one read-only Base: the "parallel agents share one environment" case.
//! Each Delta must be independent, every acknowledged write must stick, and the Base must never change.
use selfdb::vm;

#[test]
fn deltas_on_one_base_are_isolated_and_do_not_touch_it() {
    let d = std::env::temp_dir().join(format!("sharedbase-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/shared.txt", b"BASE", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/only-base.txt", b"B", 0o644, 1).unwrap();
    drop(b);

    // sequential: two deltas, same path, each keeps its own version
    {
        let d1 = vm::Overlay::open(&d.join("s1.db"), &base).unwrap();
        let d2 = vm::Overlay::open(&d.join("s2.db"), &base).unwrap();
        d1.write("/shared.txt", b"ONE", 0o644, 2).unwrap();
        d2.write("/shared.txt", b"TWO", 0o644, 3).unwrap();
        assert_eq!(d1.read("/shared.txt").unwrap(), b"ONE");
        assert_eq!(d2.read("/shared.txt").unwrap(), b"TWO");
        // a path only the Base has is visible from both, unchanged
        assert_eq!(d1.read("/only-base.txt").unwrap(), b"B");
        assert_eq!(d2.read("/only-base.txt").unwrap(), b"B");
    }
    // the Base file itself must not have been written by any of that
    let bc = vm::vm_open(base.to_str().unwrap()).unwrap();
    assert_eq!(vm::read_range(&bc, "/shared.txt", 0, 100).unwrap(), b"BASE");
    drop(bc);

    // concurrent: four deltas, 40 writes each, all acknowledged writes stick
    let mut handles = Vec::new();
    for i in 0..4u32 {
        let (base, dir) = (base.clone(), d.clone());
        handles.push(std::thread::spawn(move || {
            let ov = vm::Overlay::open(&dir.join(format!("c{}.db", i)), &base).unwrap();
            let mut acked = 0;
            for k in 0..40u32 {
                let val = format!("w{}-{}", i, k);
                ov.write("/shared.txt", val.as_bytes(), 0o644, k as i64).unwrap();
                acked += 1;
            }
            (i, acked, String::from_utf8_lossy(&ov.read("/shared.txt").unwrap()).to_string())
        }));
    }
    for h in handles {
        let (i, acked, last) = h.join().unwrap();
        assert_eq!(acked, 40, "delta {} lost writes", i);
        assert_eq!(last, format!("w{}-39", i), "delta {} must end on its own last write", i);
    }
    // and the Base is still pristine after all of it
    let bc = vm::vm_open(base.to_str().unwrap()).unwrap();
    assert_eq!(vm::read_range(&bc, "/shared.txt", 0, 100).unwrap(), b"BASE", "Base must never be written");
    let _ = std::fs::remove_dir_all(&d);
}
