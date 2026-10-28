//! Long-lived sessions: one sandbox across commands, every command still a recorded step.
//! Each test runs under bwrap, and under gVisor when runsc is available (`$SELF_RUNSC` or PATH).
use selfdb::sandbox::{gvisor, Isolation};
use selfdb::agent::RestorePoint;
use selfdb::session::{ExecOptions, Session, SessionOptions};
use selfdb::vm;
use std::path::PathBuf;
use std::time::Duration;

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn isolations() -> Vec<Isolation> {
    let mut v = Vec::new();
    if bwrap_ok() { v.push(Isolation::Bwrap); }
    if gvisor::runsc().is_some() { v.push(Isolation::Gvisor); }
    v
}

/// A VM with a shell and a few tools, and an empty /work.
fn vm(name: &str, iso: Isolation) -> (PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("sess-{}-{:?}-{}", name, iso, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    for prog in ["/bin/sh", "/bin/sleep", "/bin/cat", "/bin/rm", "/bin/mv", "/bin/mkdir"] {
        if std::path::Path::new(prog).exists() { vm::vm_import_closure(&c, prog, "/").unwrap(); }
    }
    vm::vm_add_dir(&c, "/work", 0o755, 1).unwrap();
    (d, db)
}

fn sh(script: &str) -> Vec<String> { vec!["/bin/sh".into(), "-c".into(), script.into()] }

fn opts(iso: Isolation) -> SessionOptions { SessionOptions { isolation: iso, ..SessionOptions::default() } }

#[test]
fn a_background_process_keeps_running_between_commands() {
    for iso in isolations() {
        let (d, db) = vm("bg", iso);
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        // the command returns at once; the loop it starts must outlive it
        let r = s.exec(&sh("(while true; do echo tick >> /work/log; sleep 0.1; done) >/dev/null 2>&1 &"), &ExecOptions::default()).unwrap();
        assert_eq!(r.exit_code, 0, "{:?}: {}", iso, String::from_utf8_lossy(&r.stderr));
        std::thread::sleep(Duration::from_millis(800));
        let r = s.exec(&sh("n=0; while read l; do n=$((n+1)); done < /work/log; echo $n"), &ExecOptions::default()).unwrap();
        let ticks: u32 = String::from_utf8_lossy(&r.stdout).trim().parse().unwrap_or(0);
        assert!(ticks >= 3, "{:?}: the background loop kept running ({} ticks)", iso, ticks);
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn each_command_is_a_recorded_step_with_its_own_changes() {
    for iso in isolations() {
        let (d, db) = vm("steps", iso);
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        let a = s.exec(&sh("echo one > /work/a"), &ExecOptions::default()).unwrap();
        let b = s.exec(&sh("echo two > /work/b; echo changed > /work/a"), &ExecOptions::default()).unwrap();
        assert_eq!(a.changes.created, vec!["/work/a".to_string()], "{:?}", iso);
        assert_eq!(b.changes.created, vec!["/work/b".to_string()], "{:?}: only this command's creation", iso);
        assert_eq!(b.changes.modified, vec!["/work/a".to_string()], "{:?}", iso);
        // each command has its own undo record, recorded with its step
        let (RestorePoint::Undo(ua), RestorePoint::Undo(ub)) = (&a.restore, &b.restore) else { panic!("session steps restore through undo records") };
        assert!(ub > ua, "{:?}", iso);
        // the database holds the result while the session is still running
        let c = vm::vm_open(db.to_str().unwrap()).unwrap();
        assert_eq!(vm::vm_cat(&c, "/work/a").unwrap(), b"changed\n");
        let steps: i64 = c.query_row("SELECT count(*) FROM agent_steps WHERE cmd LIKE 'session:%'", [], |r| r.get(0)).unwrap();
        assert_eq!(steps, 2);
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn a_rolled_back_command_restores_the_database_and_the_tree() {
    for iso in isolations() {
        let (d, db) = vm("rollback", iso);
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        s.exec(&sh("echo keep > /work/keep"), &ExecOptions::default()).unwrap();
        let fail = ExecOptions { rollback_on_fail: true, ..ExecOptions::default() };
        let r = s.exec(&sh("echo junk > /work/junk; rm /work/keep; exit 3"), &fail).unwrap();
        assert!(r.rolled_back && r.exit_code == 3, "{:?}", iso);
        let c = vm::vm_open(db.to_str().unwrap()).unwrap();
        assert!(vm::vm_cat(&c, "/work/junk").is_err(), "{:?}: the database is rolled back", iso);
        assert_eq!(vm::vm_cat(&c, "/work/keep").unwrap(), b"keep\n");
        // the session's tree was restored too: the next command sees the rolled-back state
        let r = s.exec(&sh("test -e /work/junk && echo JUNK; cat /work/keep"), &ExecOptions::default()).unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout), "keep\n", "{:?}: the tree is rolled back", iso);
        assert!(r.changes.created.is_empty() && r.changes.deleted.is_empty(), "{:?}: {:?}", iso, r.changes);
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn a_timeout_ends_the_command_but_not_the_session() {
    for iso in isolations() {
        let (d, db) = vm("timeout", iso);
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        let started = std::time::Instant::now();
        let r = s.exec(&sh("sleep 30 & sleep 30"), &ExecOptions { timeout_secs: 1, ..ExecOptions::default() }).unwrap();
        assert!(r.timed_out && started.elapsed() < Duration::from_secs(10), "{:?}: {:?}", iso, started.elapsed());
        let r = s.exec(&sh("echo alive"), &ExecOptions::default()).unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout), "alive\n", "{:?}: the session still answers", iso);
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn closing_a_session_removes_its_directory_and_frees_the_database() {
    for iso in isolations() {
        let (d, db) = vm("close", iso);
        let s = Session::start(&db, &opts(iso)).unwrap();
        let work = s.work_dir().to_path_buf();
        assert!(work.join("tree").is_dir());
        s.close();
        assert!(!work.exists(), "{:?}: the session directory is removed", iso);
        assert!(!std::path::Path::new(&format!("{}.lock", work.display())).exists(), "{:?}: and its lock", iso);
        // the lock is released: a step on the same database runs
        let r = selfdb::agent::run_step(&db, &sh("echo after"), &selfdb::agent::StepOptions::new(30)).unwrap();
        assert_eq!(r.exit_code, 0);
        let _ = std::fs::remove_dir_all(&d);
    }
}

fn verify(db: &std::path::Path) {
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "{}", v);
}

#[test]
fn rewind_returns_database_and_tree_to_an_earlier_command() {
    for iso in isolations() {
        let (d, db) = vm("rewind", iso);
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        s.exec(&sh("echo v1 > /work/f; echo keep > /work/k"), &ExecOptions::default()).unwrap();
        let second = s.exec(&sh("echo v2 > /work/f; mkdir /work/d; echo x > /work/d/y"), &ExecOptions::default()).unwrap();
        s.exec(&sh("echo v3 > /work/f; rm /work/k"), &ExecOptions::default()).unwrap();
        let RestorePoint::Undo(u2) = second.restore else { panic!() };
        s.rewind(u2).unwrap();
        let r = s.exec(&sh("cat /work/f /work/k; test -e /work/d && echo HAS_D"), &ExecOptions::default()).unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout), "v1\nkeep\n", "{:?}: the tree is back before the second command", iso);
        let c = vm::vm_open(db.to_str().unwrap()).unwrap();
        assert_eq!(vm::vm_cat(&c, "/work/f").unwrap(), b"v1\n");
        assert!(vm::vm_cat(&c, "/work/d/y").is_err(), "{:?}", iso);
        drop(c);
        drop(s);
        verify(&db);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn a_same_size_rewrite_within_the_second_is_rolled_back_in_the_tree() {
    // The materialize keeps a file whose size and mtime (to the second) match the row. Restoring only
    // through it left a same-size rewrite in the tree while the database held the old content.
    for iso in isolations() {
        let (d, db) = vm("samesec", iso);
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        s.exec(&sh("echo AAAA > /work/f"), &ExecOptions::default()).unwrap();
        let fail = ExecOptions { rollback_on_fail: true, ..ExecOptions::default() };
        let r = s.exec(&sh("echo BBBB > /work/f; exit 1"), &fail).unwrap();
        assert!(r.rolled_back);
        let r = s.exec(&sh("cat /work/f"), &ExecOptions::default()).unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout), "AAAA\n", "{:?}: the tree holds the rolled-back content", iso);
        assert!(r.changes.modified.is_empty(), "{:?}: tree and database agree: {:?}", iso, r.changes);
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn moving_a_directory_reports_its_whole_subtree() {
    // a directory move changes the path of everything below it; the watcher falls back to a full sync
    for iso in isolations() {
        let (d, db) = vm("mvdir", iso);
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        s.exec(&sh("mkdir -p /work/a/b; echo 1 > /work/a/b/f; echo 2 > /work/a/g"), &ExecOptions::default()).unwrap();
        let r = s.exec(&sh("mv /work/a /work/z"), &ExecOptions::default()).unwrap();
        assert_eq!(r.exit_code, 0, "{}", String::from_utf8_lossy(&r.stderr));
        let mut created = r.changes.created.clone(); created.sort();
        let mut deleted = r.changes.deleted.clone(); deleted.sort();
        assert_eq!(created, vec!["/work/z", "/work/z/b", "/work/z/b/f", "/work/z/g"], "{:?}", iso);
        assert_eq!(deleted, vec!["/work/a", "/work/a/b", "/work/a/b/f", "/work/a/g"], "{:?}", iso);
        // and the next command starts from a correct view: a change inside the moved tree is seen
        let r = s.exec(&sh("echo 3 > /work/z/b/f"), &ExecOptions::default()).unwrap();
        assert_eq!(r.changes.modified, vec!["/work/z/b/f".to_string()], "{:?}", iso);
        drop(s);
        verify(&db);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn a_write_between_commands_belongs_to_the_next_command() {
    for iso in isolations() {
        let (d, db) = vm("between", iso);
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        s.exec(&sh("(sleep 0.3; echo late > /work/late) >/dev/null 2>&1 &"), &ExecOptions::default()).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let r = s.exec(&sh("true"), &ExecOptions::default()).unwrap();
        assert_eq!(r.changes.created, vec!["/work/late".to_string()], "{:?}: the background write is recorded", iso);
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }
}

// ---- timings on a provisioned dev VM (slow: `cargo test --release --test session -- --ignored --nocapture`)

fn node_and_pi() -> Option<(PathBuf, PathBuf)> {
    let out = std::process::Command::new("sh").args(["-c", "readlink -f \"$(command -v node)\""]).output().ok()?;
    let node = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let pi = ["/home/linuxbrew/.linuxbrew/lib/node_modules/@earendil-works/pi-coding-agent",
              "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent"].iter().map(PathBuf::from).find(|p| p.exists())?;
    node.exists().then_some((node, pi))
}

/// What a session command costs: the fixed part (an empty command: request, sync, diff, snapshot) and
/// file-heavy work, under each isolation. gVisor sessions use shared file access, so these numbers include
/// its cost.
#[test]
#[ignore]
fn session_timings_on_a_dev_vm() {
    let Some((node, pi)) = node_and_pi() else { eprintln!("skip: node or pi missing"); return; };
    let d = std::env::temp_dir().join(format!("sess-perf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    drop(vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap());
    selfdb::agent::provision_agent_with(&base, &node, &pi, selfdb::agent::ToolSet::Dev).unwrap();
    let files: i64 = vm::vm_open(base.to_str().unwrap()).unwrap().query_row("SELECT count(*) FROM vm_fs", [], |r| r.get(0)).unwrap();
    eprintln!("dev VM: {} entries", files);
    const PREP: &str = "rm -rf /work/w /work/x /work/t.tar; mkdir -p /work; cp -r /usr/include/linux /work/w";
    let workloads: &[(&str, String)] = &[
        ("empty command", "true".into()),
        ("copy 2k-file tree", PREP.to_string()),
        ("git add+commit tree", format!("{}; cd /work/w && git init -q . && git -c user.email=a@b -c user.name=t add -A && git -c user.email=a@b -c user.name=t commit -qm m", PREP)),
        ("gcc compile x20", "cd /tmp && printf 'int main(){return 0;}' > m.c && i=0; while [ $i -lt 20 ]; do gcc -O2 m.c -o m; i=$((i+1)); done".into()),
    ];
    for iso in isolations() {
        let db = d.join(format!("{:?}.db", iso));
        std::fs::copy(&base, &db).unwrap();
        let t = std::time::Instant::now();
        let mut s = Session::start(&db, &opts(iso)).unwrap();
        eprintln!("{:?}: session start {:?}", iso, t.elapsed());
        for (name, script) in workloads {
            let best = (0..3).map(|_| {
                let r = s.exec(&sh(script), &ExecOptions { timeout_secs: 300, ..ExecOptions::default() }).unwrap();
                assert_eq!(r.exit_code, 0, "{:?} {}: {}", iso, name, String::from_utf8_lossy(&r.stderr));
                r.duration_ms
            }).min().unwrap();
            eprintln!("{:?}: {:22} {:>6} ms", iso, name, best);
        }
        drop(s);
    }
    let _ = std::fs::remove_dir_all(&d);
}
