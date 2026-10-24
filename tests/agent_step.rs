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
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: selfdb::agent::DEFAULT_MAX_OUTPUT_BYTES }).unwrap();
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
        &agent::StepOptions { rollback_on_fail: true, timeout_secs: 30, max_output_bytes: selfdb::agent::DEFAULT_MAX_OUTPUT_BYTES }).unwrap();
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
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: selfdb::agent::DEFAULT_MAX_OUTPUT_BYTES }).unwrap();
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
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 1, max_output_bytes: selfdb::agent::DEFAULT_MAX_OUTPUT_BYTES }).unwrap();
    assert!(r.timed_out, "the step must be reported as timed out");
    assert_ne!(r.exit_code, 0);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn oversized_output_is_capped_and_the_child_still_finishes() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("cap");
    // ~200 KB to stdout and ~200 KB to stderr, far above the 1 KiB cap. The child must not block on a
    // full pipe: it has to run to completion and report its own exit code.
    let script = "i=0; while [ $i -lt 4000 ]; do echo 0123456789012345678901234567890123456789012345678901234; echo e0123456789012345678901234567890123456789012345678901234 >&2; i=$((i+1)); done; echo done-marker; exit 7";
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(), script.into()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: 1024 }).unwrap();
    assert_eq!(r.exit_code, 7, "child must finish and report its own exit code");
    assert!(!r.timed_out);
    assert!(r.stdout_truncated && r.stderr_truncated, "both streams exceed the cap");
    assert_eq!(r.stdout.len(), 1024, "stdout kept exactly up to the cap");
    assert_eq!(r.stderr.len(), 1024, "stderr kept exactly up to the cap");
    assert!(!r.stdout.windows(11).any(|w| w == b"done-marker"), "bytes past the cap are dropped");
    // the stored row carries the same flags and the capped bytes
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let (so, se, st, et): (i64, i64, i64, i64) = c.query_row(
        "SELECT length(stdout), length(stderr), stdout_truncated, stderr_truncated FROM agent_steps WHERE step_id=?1",
        [r.step_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap();
    assert_eq!((so, se, st, et), (1024, 1024, 1, 1));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn output_under_the_cap_is_kept_whole_and_not_flagged() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("nocap");
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(), "echo small-out; echo small-err >&2".into()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: 1024 }).unwrap();
    assert_eq!(r.stdout, b"small-out\n");
    assert_eq!(r.stderr, b"small-err\n");
    assert!(!r.stdout_truncated && !r.stderr_truncated);
    let _ = std::fs::remove_dir_all(&d);
}
