//! Rollouts over overlay layers: per-action change lists from the upper directory, read-only paths
//! enforced by mounts, forks from a running rollout, and freezing the result into a database.
use selfdb::rollout::{self, Rollout, RolloutOptions};
use selfdb::vm;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// A template: a database with a shell and tools, and its materialized tree as the first layer.
struct Template { dir: PathBuf, db: PathBuf, layer: PathBuf }
impl Drop for Template { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.dir); } }

fn template(name: &str) -> Template {
    let dir = std::env::temp_dir().join(format!("rollout-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("base.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    for prog in ["/bin/sh", "/bin/cat", "/bin/rm", "/bin/mkdir", "/bin/mv", "/bin/sleep"] {
        if Path::new(prog).exists() { vm::vm_import_closure(&c, prog, "/").unwrap(); }
    }
    vm::vm_add_bytes(&c, "/etc/conf", b"original\n", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/etc/other", b"other\n", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/data/a", b"a\n", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/data/sub/b", b"b\n", 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/tests/test_x.sh", b"exit 0\n", 0o755, 1).unwrap();
    vm::vm_add_dir(&c, "/work", 0o755, 1).unwrap();
    let layer = dir.join("layer0");
    vm::vm_materialize_tree(&c, &layer).unwrap();
    Template { dir, db, layer }
}

fn sh(r: &mut Rollout, script: &str) -> rollout::ActionResult {
    r.exec(&["/bin/sh".into(), "-c".into(), script.into()], "/", Duration::from_secs(30), 1 << 20).unwrap()
}

fn sorted(v: &[String]) -> Vec<String> { let mut v = v.to_vec(); v.sort(); v }

#[test]
fn each_action_reports_what_it_changed_in_the_merged_view() {
    if !bwrap_ok() { return; }
    let t = template("diff");
    let mut r = Rollout::start(std::slice::from_ref(&t.layer), &RolloutOptions::default()).unwrap();
    let a = sh(&mut r, "echo new > /work/new");
    assert_eq!(a.changes.created, vec!["/work/new"]);
    assert!(a.changes.modified.is_empty() && a.changes.deleted.is_empty(), "{:?}", a.changes);
    // a lower file rewritten: modified; its directory only copied up: not a change
    let b = sh(&mut r, "echo changed > /etc/conf");
    assert_eq!(b.changes.modified, vec!["/etc/conf"], "{:?}", b.changes);
    assert!(b.changes.created.is_empty() && b.changes.deleted.is_empty());
    // an open for writing that writes nothing copies the file up but changes nothing
    let c = sh(&mut r, ": >> /etc/other");
    assert!(c.changes.created.is_empty() && c.changes.modified.is_empty() && c.changes.deleted.is_empty(), "{:?}", c.changes);
    // a lower directory removed: the directory and everything below it
    let d = sh(&mut r, "rm -rf /data");
    assert_eq!(sorted(&d.changes.deleted), vec!["/data", "/data/a", "/data/sub", "/data/sub/b"]);
    // recreated with new content: the old contents stay deleted, the new file is created
    let e = sh(&mut r, "mkdir /data && echo z > /data/z");
    assert_eq!(sorted(&e.changes.created), vec!["/data", "/data/z"], "{:?}", e.changes);
    // a file that only ever existed in the upper, removed again
    let f = sh(&mut r, "rm /work/new");
    assert_eq!(f.changes.deleted, vec!["/work/new"]);
    // the layer itself is never written
    assert_eq!(std::fs::read(t.layer.join("etc/conf")).unwrap(), b"original\n");
    assert!(t.layer.join("data/a").exists());
}

#[test]
fn a_read_only_path_refuses_changes_and_creation() {
    if !bwrap_ok() { return; }
    let t = template("ro");
    let opts = RolloutOptions { readonly: vec!["/tests".into(), "/absent".into()], ..RolloutOptions::default() };
    let mut r = Rollout::start(std::slice::from_ref(&t.layer), &opts).unwrap();
    let a = sh(&mut r, "echo hacked > /tests/test_x.sh");
    assert_ne!(a.outcome.exit_code, 0, "a write to a read-only path fails");
    let b = sh(&mut r, "rm /tests/test_x.sh; cat /tests/test_x.sh");
    assert_eq!(String::from_utf8_lossy(&b.outcome.stdout), "exit 0\n", "the file is untouched");
    let c = sh(&mut r, "mkdir -p /absent/x");
    assert_ne!(c.outcome.exit_code, 0, "a read-only path absent from the template cannot be created");
    let w = r.write_file("/tests/new.sh", b"x", 0o644).unwrap();
    assert_ne!(w.outcome.exit_code, 0, "the file tool honours the read-only mount too");
    assert!(a.changes.modified.is_empty() && b.changes.deleted.is_empty() && w.changes.created.is_empty());
}

#[test]
fn file_tools_read_write_and_list_inside_the_rollout() {
    if !bwrap_ok() { return; }
    let t = template("files");
    let mut r = Rollout::start(std::slice::from_ref(&t.layer), &RolloutOptions::default()).unwrap();
    let w = r.write_file("/work/deep/dir/f.txt", b"hello", 0o600).unwrap();
    assert_eq!(w.outcome.exit_code, 0, "{}", String::from_utf8_lossy(&w.outcome.stderr));
    assert_eq!(sorted(&w.changes.created), vec!["/work/deep", "/work/deep/dir", "/work/deep/dir/f.txt"]);
    assert_eq!(r.read_file("/work/deep/dir/f.txt", 1, 3).unwrap().unwrap(), b"ell");
    let names: Vec<String> = r.list_dir("/etc").unwrap().unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"conf".to_string()) && names.contains(&"other".to_string()), "{:?}", names);
    assert!(r.read_file("/nope", 0, 10).unwrap().is_err(), "a missing file is the sandbox's error");
}

#[test]
fn a_fork_starts_from_the_running_rollout_and_diverges() {
    if !bwrap_ok() { return; }
    let t = template("fork");
    let mut parent = Rollout::start(std::slice::from_ref(&t.layer), &RolloutOptions::default()).unwrap();
    sh(&mut parent, "echo step1 > /work/state; rm /etc/other");
    let frozen = t.dir.join("fork-layer");
    parent.snapshot_upper(&frozen).unwrap();
    let layers = vec![t.layer.clone(), frozen.clone()];
    let mut kids: Vec<Rollout> = (0..3).map(|_| Rollout::start(&layers, &RolloutOptions::default()).unwrap()).collect();
    for (i, k) in kids.iter_mut().enumerate() {
        let r = sh(k, &format!("cat /work/state; test -e /etc/other || echo NO_OTHER; echo kid{} > /work/state", i));
        assert_eq!(String::from_utf8_lossy(&r.outcome.stdout), "step1\nNO_OTHER\n", "a child starts from the parent's state");
        assert_eq!(r.changes.modified, vec!["/work/state"], "the parent's changes are the child's starting point, not its changes");
    }
    // siblings and parent do not see each other
    let r = sh(&mut kids[0], "cat /work/state");
    assert_eq!(String::from_utf8_lossy(&r.outcome.stdout), "kid0\n");
    let r = sh(&mut parent, "cat /work/state; echo step2 > /work/state");
    assert_eq!(String::from_utf8_lossy(&r.outcome.stdout), "step1\n", "the parent is unaffected and keeps running");
}

#[test]
fn freezing_a_rollout_writes_its_changes_into_a_database() {
    if !bwrap_ok() { return; }
    let t = template("freeze");
    let mut r = Rollout::start(std::slice::from_ref(&t.layer), &RolloutOptions::default()).unwrap();
    sh(&mut r, "echo changed > /etc/conf; rm /etc/other; rm -rf /data; mkdir /data; echo z > /data/z; echo n > /work/n; mkdir -p /work/d/e");
    let up = t.dir.join("frozen");
    r.snapshot_upper(&up).unwrap();
    let frozen_db = t.dir.join("frozen.db");
    std::fs::copy(&t.db, &frozen_db).unwrap();
    let c = vm::vm_open(frozen_db.to_str().unwrap()).unwrap();
    vm::vm_apply_upper(&c, &up).unwrap();
    assert_eq!(vm::vm_cat(&c, "/etc/conf").unwrap(), b"changed\n");
    assert!(vm::vm_cat(&c, "/etc/other").is_err());
    assert!(vm::vm_cat(&c, "/data/a").is_err() && vm::vm_cat(&c, "/data/sub/b").is_err(), "an opaque directory replaced its contents");
    assert_eq!(vm::vm_cat(&c, "/data/z").unwrap(), b"z\n");
    assert_eq!(vm::vm_cat(&c, "/work/n").unwrap(), b"n\n");
    let kind: String = c.query_row("SELECT kind FROM vm_fs WHERE path='/work/d/e'", [], |r| r.get(0)).unwrap();
    assert_eq!(kind, "dir");
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
    // the frozen database materializes to what the rollout saw
    let tree = t.dir.join("tree2");
    vm::vm_materialize_tree(&c, &tree).unwrap();
    let mut r2 = Rollout::start(&[tree], &RolloutOptions::default()).unwrap();
    let out = sh(&mut r2, "cat /etc/conf /data/z; test -e /data/a || echo NO_A");
    assert_eq!(String::from_utf8_lossy(&out.outcome.stdout), "changed\nz\nNO_A\n");
}

#[test]
fn a_closed_rollout_leaves_nothing_behind() {
    // overlayfs makes work/work mode 000, and an agent can make its own directories unreadable: removing
    // the rollout's directory must not stop at either (every rollout directory stayed behind, measured)
    if !bwrap_ok() { return; }
    let t = template("cleanup");
    let root = t.dir.join("runs");
    let opts = RolloutOptions { work_root: Some(root.clone()), ..RolloutOptions::default() };
    let mut r = Rollout::start(std::slice::from_ref(&t.layer), &opts).unwrap();
    sh(&mut r, "mkdir -p /work/locked/inner && echo x > /work/locked/inner/f && chmod 000 /work/locked/inner /work/locked");
    let frozen = t.dir.join("frozen");
    r.snapshot_upper(&frozen).unwrap();
    drop(r);
    let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "the rollout's directory and lock are removed: {:?}", left);
    vm::remove_tree(&frozen).unwrap();
    assert!(!frozen.exists(), "a layer with unreadable directories is removable too");
}
