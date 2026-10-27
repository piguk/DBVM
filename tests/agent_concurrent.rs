//! Two steps on one database at the same time. Tool calls from one assistant message can run in
//! parallel, so this is the normal case for an agent host, not an edge case. Both must succeed.
use selfdb::{agent, vm};
use std::sync::{Arc, Barrier};

fn step_available() -> bool {
    std::process::Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn two_steps_on_one_database_both_succeed() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let d = std::env::temp_dir().join(format!("conc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_import_closure(&c, "/bin/sh", "/").unwrap();
    vm::vm_add_bytes(&c, "/keep.txt", b"keep", 0o644, 1).unwrap();
    drop(c);

    // Start both at the same instant, and make the first one slow. Without a deliberate overlap the
    // two steps finish in sequence by luck and a missing lock would not be caught (verified: the first
    // version of this test passed with the lock removed).
    vm::vm_import_closure(&vm::vm_open(db.to_str().unwrap()).unwrap(), "/bin/sleep", "/").unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for i in 0..2 {
        let (db, barrier) = (db.clone(), barrier.clone());
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            // the slow one overlaps the other for certain
            let script = if i == 0 { "sleep 2; echo 0 > /out0.txt".to_string() } else { "echo 1 > /out1.txt".to_string() };
            agent::run_step(&db, &["/bin/sh".into(), "-c".into(), script],
                &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: agent::DEFAULT_MAX_OUTPUT_BYTES, ..agent::StepOptions::new(0) })
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let errs: Vec<String> = results.iter().filter_map(|r| r.as_ref().err().map(|e| e.to_string())).collect();
    assert!(errs.is_empty(), "both steps must succeed, got errors: {:?}", errs);
    // both steps recorded, and both writes landed
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let steps: i64 = c.query_row("SELECT count(*) FROM agent_steps", [], |r| r.get(0)).unwrap();
    assert_eq!(steps, 2, "both steps are recorded");
    for i in 0..2 { assert!(vm::vm_cat(&c, &format!("/out{}.txt", i)).is_ok(), "/out{}.txt must exist", i); }
    let _ = std::fs::remove_dir_all(&d);
}
