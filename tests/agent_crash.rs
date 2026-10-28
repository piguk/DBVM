//! A step killed mid-flight must not corrupt the database. What it leaves behind is a snapshot with no
//! matching agent_steps row; that orphan is what `snapshot_retain` exists to clean up.
use selfdb::vm;
use std::process::Command;

fn step_available() -> bool {
    Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn a_killed_step_leaves_a_usable_database_and_an_orphan_snapshot() {
    if !step_available() { eprintln!("skip: bwrap not available"); return; }
    let d = std::env::temp_dir().join(format!("crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_import_closure(&c, "/bin/sh", "/").unwrap();
    vm::vm_import_closure(&c, "/bin/sleep", "/").unwrap();
    vm::vm_add_bytes(&c, "/keep.txt", b"keep", 0o644, 1).unwrap();
    drop(c);

    // start a step that would take 10 s and kill it after 2.5 s
    let existing = scratch_dirs();
    child_runner::kill_mid_step(&db).unwrap();
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    // the database is intact and the untouched file survived
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "database must verify after a killed step: {}", v);
    assert_eq!(vm::read_range(&c, "/keep.txt", 0, 10).unwrap(), b"keep");
    // the killed step left a snapshot with no step row: an orphan to be reclaimed
    let snaps: i64 = c.query_row("SELECT count(*) FROM vm_snapshots", [], |r| r.get(0)).unwrap();
    let steps: i64 = c.query_row("SELECT count(*) FROM agent_steps", [], |r| r.get(0)).unwrap();
    eprintln!("crash: snapshots={} steps={}", snaps, steps);
    assert_eq!(steps, 0, "a killed step records nothing");
    assert_eq!(snaps, 1, "and leaves exactly one orphan snapshot");
    // retaining nothing reclaims both kinds
    let removed = vm::snapshot_retain(&c, 0).unwrap();
    assert_eq!(removed.len(), 1, "the orphan is reclaimed by retain");
    drop(c);
    // the killed step could not remove its materialized tree; the next step reclaims it
    // only the killed step's own tree: other runs may have left directories that predate owner locks
    let left: Vec<_> = scratch_dirs().into_iter().filter(|p| !existing.contains(p)).collect();
    assert_eq!(left.len(), 1, "the killed step left its tree (this is what the next step reclaims)");
    let r = selfdb::agent::run_step(&db, &["/bin/sh".into(), "-c".into(), "true".into()], &selfdb::agent::StepOptions::new(30)).unwrap();
    assert_eq!(r.exit_code, 0);
    let still: Vec<_> = scratch_dirs().into_iter().filter(|p| left.contains(p)).collect();
    assert!(still.is_empty(), "abandoned trees are reclaimed: {:?}", still);
    let _ = std::fs::remove_dir_all(&d);
}

fn scratch_dirs() -> Vec<std::path::PathBuf> {
    std::fs::read_dir(std::env::temp_dir()).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("agent-step-") && e.path().is_dir())
        .map(|e| e.path()).collect()
}

#[test]
fn a_live_work_dir_is_never_reclaimed_and_a_dropped_one_is_gone() {
    let a = vm::WorkDir::create("workdir-test-").unwrap();
    std::fs::write(a.path().join("f"), b"x").unwrap();
    // another directory with the same prefix runs the reclaim pass; `a` is locked by its owner
    let b = vm::WorkDir::create("workdir-test-").unwrap();
    assert!(a.path().join("f").exists(), "a live directory is kept");
    let (pa, pb) = (a.path().to_path_buf(), b.path().to_path_buf());
    drop(a);
    drop(b);
    assert!(!pa.exists() && !pb.exists(), "dropping removes the directory");
    assert!(!std::path::Path::new(&format!("{}.lock", pa.display())).exists(), "and its lock");
}

mod child_runner {
    use std::path::Path;
    /// Run one step in a child process (the real `self` binary) and kill it mid-run.
    pub fn kill_mid_step(db: &Path) -> std::io::Result<()> {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_self"))
            .args(["vm-step", db.to_str().unwrap(), "--", "/bin/sh", "-c", "echo x > /a.txt; sleep 10"])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn()?;
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let _ = c.kill();
        let _ = c.wait();
        std::thread::sleep(std::time::Duration::from_millis(300));
        Ok(())
    }
}
