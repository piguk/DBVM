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

#[test]
fn step_runs_in_its_own_network_namespace() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("netns");
    // /proc/net/dev lists the interfaces of the current network namespace. A step must see only
    // loopback, not the host's interfaces (the host has at least one non-loopback NIC here).
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(),
        "while read l; do echo \"IF:$l\"; done < /proc/net/dev".into()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: agent::DEFAULT_MAX_OUTPUT_BYTES }).unwrap();
    assert_eq!(r.exit_code, 0);
    let out = String::from_utf8_lossy(&r.stdout);
    let ifaces: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("IF:"))
        .filter_map(|l| l.split(':').next()).map(|s| s.trim()).filter(|s| !s.is_empty() && !s.starts_with("Inter") && !s.starts_with("face")).collect();
    assert_eq!(ifaces, vec!["lo"], "step must see only loopback: {:?}", ifaces);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn agent_log_json_is_valid_and_keeps_exact_paths() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("alog");
    // a file whose name has a newline and a quote: the change list must carry it exactly
    // the shell builds the real newline itself: printf expands \n in the name
    let odd = "/odd\nname\"q.txt";
    let script = "printf y > \"$(printf '/odd\\nname\"q.txt')\"; exit 3".to_string();
    let r1 = agent::run_step(&db, &["/bin/sh".into(), "-c".into(), script.clone()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: agent::DEFAULT_MAX_OUTPUT_BYTES }).unwrap();
    assert_eq!(r1.exit_code, 3);
    let rows = agent::list_steps(&db, None).unwrap();
    assert_eq!(rows.len(), 1);
    let items: Vec<String> = rows.iter().map(agent::step_to_json).collect();
    let text = format!("[{}]", items.join(","));
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("agent-log --json must be valid JSON");
    let created = &parsed[0]["changes"]["created"];
    assert!(odd.contains('\n') && odd.contains('"'), "the test path must really contain a newline and a quote");
    assert!(created.as_array().unwrap().iter().any(|v| v == odd), "exact path must round-trip: {}", created);
    assert_eq!(parsed[0]["exit_code"], 3);
    assert_eq!(parsed[0]["rolled_back"], false);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn list_steps_limit_keeps_the_most_recent() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("alimit");
    for i in 0..3 {
        agent::run_step(&db, &["/bin/sh".into(), "-c".into(), format!("exit {}", i)],
            &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: agent::DEFAULT_MAX_OUTPUT_BYTES }).unwrap();
    }
    let all = agent::list_steps(&db, None).unwrap();
    assert_eq!(all.iter().map(|r| r.exit_code).collect::<Vec<_>>(), vec![0, 1, 2], "oldest first");
    let last2 = agent::list_steps(&db, Some(2)).unwrap();
    assert_eq!(last2.iter().map(|r| r.exit_code).collect::<Vec<_>>(), vec![1, 2], "limit keeps the most recent");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_write_the_policy_refuses_does_not_look_like_success() {
    // The sandbox lets the command write the file. The policy refuses the sync back. That refusal must
    // not be silently dropped: a step whose changes could not be applied must not report success with an
    // empty change list. Found by running the step against a protected path: exit 0, empty stderr, empty
    // change list, content unchanged -- the caller had no way to know the write was rejected.
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = make("policydeny");
    {
        let c = vm::vm_open(db.to_str().unwrap()).unwrap();
        vm::vm_add_bytes(&c, "/etc/app.conf", b"orig", 0o644, 1).unwrap();
        selfdb::policy::add(&c, &selfdb::policy::Rule::ReadonlyPrefix("/etc".into())).unwrap();
    }
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(), "echo tampered > /etc/app.conf".into()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 30, max_output_bytes: agent::DEFAULT_MAX_OUTPUT_BYTES });
    let content = {
        let c = vm::vm_open(db.to_str().unwrap()).unwrap();
        vm::vm_cat(&c, "/etc/app.conf").unwrap()
    };
    assert_eq!(content, b"orig", "the policy must keep protecting the file");
    match r {
        Err(e) => assert!(e.to_string().contains("EACCES") || e.to_string().contains("protected") || e.to_string().contains("failed"),
            "a refused write must surface as an error naming the refusal: {}", e),
        Ok(x) => panic!("a step whose write was refused must not report success (exit={}, stderr={:?}, changes={:?})",
            x.exit_code, String::from_utf8_lossy(&x.stderr), x.changes),
    }
    let _ = std::fs::remove_dir_all(&d);
}
