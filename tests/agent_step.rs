//! agent-step: one command, run against the VM, recorded with its output, exit code and the
//! file changes it made. A failing step can be rolled back to the state before it ran.
use selfdb::vm;
use selfdb::agent;

fn make(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let d = std::env::temp_dir().join(format!("astep-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&c, "/work/keep.txt", b"keep", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/work/change.txt", b"before", 0o644, 1).unwrap();
    // a shell and its libraries, so the commands in these tests can run inside the VM
    vm::vm_import_closure(&c, "/bin/sh", "/").unwrap();
    // rm is an external program; the step can only run what the VM contains
    vm::vm_import_closure(&c, "/bin/rm", "/").unwrap();
    vm::vm_import_closure(&c, "/bin/sleep", "/").unwrap();
    drop(c);
    (d, db)
}

fn step_available() -> bool {
    std::process::Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn step_records_output_exit_code_and_changes() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("record");
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(),
        "echo hello-out; echo hello-err >&2; echo after > /work/change.txt; echo new > /work/added.txt; rm /work/keep.txt".into()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30 }).unwrap();
    assert_eq!(r.exit_code, 0);
    assert!(String::from_utf8_lossy(&r.stdout).contains("hello-out"));
    assert!(String::from_utf8_lossy(&r.stderr).contains("hello-err"));
    assert!(r.changes.created.contains(&"/work/added.txt".to_string()), "{:?}", r.changes);
    assert!(r.changes.modified.contains(&"/work/change.txt".to_string()), "{:?}", r.changes);
    assert!(r.changes.deleted.contains(&"/work/keep.txt".to_string()), "{:?}", r.changes);
    // the step is in the log
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let n: i64 = c.query_row("SELECT count(*) FROM agent_steps", [], |x| x.get(0)).unwrap();
    assert_eq!(n, 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn failing_step_with_rollback_restores_state() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("rollback");
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(),
        "echo damage > /work/change.txt; rm /work/keep.txt; exit 3".into()],
        &agent::StepOptions { rollback_on_fail: true, timeout_secs: 30 }).unwrap();
    assert_eq!(r.exit_code, 3);
    assert!(r.rolled_back, "a failing step with rollback must be rolled back");
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/work/change.txt").unwrap(), b"before");
    assert_eq!(vm::vm_cat(&c, "/work/keep.txt").unwrap(), b"keep");
    // the failed step is still recorded, marked rolled back
    let rb: i64 = c.query_row("SELECT rolled_back FROM agent_steps WHERE step_id=?1", [r.step_id], |x| x.get(0)).unwrap();
    assert_eq!(rb, 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn failing_step_without_rollback_keeps_changes() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("keep");
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(), "echo changed > /work/change.txt; exit 1".into()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30 }).unwrap();
    assert_eq!(r.exit_code, 1);
    assert!(!r.rolled_back);
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/work/change.txt").unwrap(), b"changed\n");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn timeout_kills_the_step_and_is_recorded() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("timeout");
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 1 }).unwrap();
    assert!(r.timed_out, "the step must be reported as timed out");
    assert_ne!(r.exit_code, 0);
    let _ = std::fs::remove_dir_all(&d);
}
