//! Concurrency: several writers share one Delta and one read-only Base. Every operation must
//! either succeed and be visible afterwards, or report an error. No write may vanish silently,
//! and the database must still pass the deep check.
use selfdb::vm;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};

fn make(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("ovconc-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/shared/seed", b"seed", 0o644, 1).unwrap();
    drop(b);
    (d.clone(), base, d.join("delta.db"))
}

#[test]
fn concurrent_writers_lose_no_acknowledged_write() {
    let (d, base, delta) = make("writers");
    // create the delta once so every thread opens an existing database
    drop(vm::Overlay::open(&delta, &base).unwrap());
    let threads = 6usize;
    let per = 40usize;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for t in 0..threads {
        let (base, delta, barrier) = (base.clone(), delta.clone(), barrier.clone());
        handles.push(std::thread::spawn(move || {
            let ov = vm::Overlay::open(&delta, &base).unwrap();
            barrier.wait();
            let mut acked = Vec::new();
            let mut failed = 0usize;
            for i in 0..per {
                let p = format!("/t{}/f{:03}", t, i);
                match ov.write(&p, format!("{}-{}", t, i).as_bytes(), 0o644, 2) {
                    Ok(()) => acked.push((p, format!("{}-{}", t, i))),
                    Err(e) => { failed += 1; if failed <= 2 { eprintln!("ERR {}", e); } }
                }
            }
            (acked, failed)
        }));
    }
    let mut all_acked = Vec::new();
    let mut total_failed = 0usize;
    for h in handles {
        let (acked, failed) = h.join().unwrap();
        all_acked.extend(acked);
        total_failed += failed;
    }
    // every acknowledged write must be readable with its own content
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    for (p, want) in &all_acked {
        assert_eq!(ov.read(p).unwrap(), want.as_bytes(), "acknowledged write lost: {}", p);
    }
    let c = vm::vm_open(delta.to_str().unwrap()).unwrap();
    let rep = vm::vm_verify(&c, true).unwrap();
    assert!(rep.contains("deep=ok"), "delta inconsistent after contention: {}", rep);
    eprintln!("concurrent writers: {} acknowledged, {} reported errors", all_acked.len(), total_failed);
    assert_eq!(all_acked.len() + total_failed, threads * per, "every attempt is accounted for");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn readers_see_consistent_snapshots_while_a_writer_runs() {
    let (d, base, delta) = make("readers");
    drop(vm::Overlay::open(&delta, &base).unwrap());
    let writer = {
        let (base, delta) = (base.clone(), delta.clone());
        std::thread::spawn(move || {
            let ov = vm::Overlay::open(&delta, &base).unwrap();
            for i in 0..200 {
                // each write is one file; a reader must never observe a torn file
                let _ = ov.write(&format!("/live/f{:03}", i), format!("v{:03}", i).repeat(50).as_bytes(), 0o644, 3);
            }
        })
    };
    let mut torn = 0usize;
    let mut reads = 0usize;
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    for _ in 0..400 {
        for i in 0..200 {
            if let Ok(bytes) = ov.read(&format!("/live/f{:03}", i)) {
                reads += 1;
                let want = format!("v{:03}", i).repeat(50);
                if bytes != want.as_bytes() { torn += 1; }
            }
        }
    }
    writer.join().unwrap();
    assert_eq!(torn, 0, "a reader saw a partial file ({} of {} reads)", torn, reads);
    let _ = std::fs::remove_dir_all(&d);
}
