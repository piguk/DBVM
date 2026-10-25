#![cfg(feature = "fuse")]
//! A mounted VM must expose every path the database holds, at the path the kernel can reach.
//!
//! The bug this covers: `/bin` is a symlink to `/usr/bin`, so `/bin/sh` was registered under its literal
//! path, which no directory listing ever showed — the entry was unreachable, and `bwrap --bind <mount> /`
//! could not run `/bin/sh` at all.
use selfdb::vm;
use std::process::Command;

fn bwrap_ok() -> bool {
    Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

const ENV_IN_NS: &str = "MP_IN_NS";

/// The child re-runs this test inside a user+mount namespace, where FUSE may be mounted.
fn is_child() -> bool { std::env::var(ENV_IN_NS).is_ok() }

fn mount(db: &std::path::Path, mnt: &std::path::Path) -> fuser::BackgroundSession {
    std::fs::create_dir_all(mnt).unwrap();
    let h = selfdb::fuse::fuse_impl::mount_vm_background(db.to_str().unwrap(), mnt.to_str().unwrap()).unwrap();
    for _ in 0..40 {
        if std::fs::read_dir(mnt).map(|mut d| d.next().is_some()).unwrap_or(false) { break; }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    h
}

#[test]
fn a_link_under_a_directory_link_is_reachable_at_its_real_path() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    if !is_child() {
        let (ok, out) = {
            let exe = std::env::current_exe().unwrap();
            let o = Command::new("unshare")
                .args(["-r", "-m", exe.to_str().unwrap(), "--exact",
                       "a_link_under_a_directory_link_is_reachable_at_its_real_path", "--nocapture"])
                .env(ENV_IN_NS, "1").output().expect("unshare");
            (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
        };
        assert!(ok, "child failed:\n{}", out);
        return;
    }
    let d = std::env::temp_dir().join(format!("mp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    vm::vm_import_closure(&c, "/bin/sh", "/").unwrap();
    let db_rows: i64 = c.query_row("SELECT count(*) FROM vm_fs WHERE path<>'/'", [], |r| r.get(0)).unwrap();
    let bin: String = c.query_row("SELECT link_target FROM vm_fs WHERE path='/bin'", [], |r| r.get(0)).unwrap();
    assert_eq!(bin, "/usr/bin", "/bin is a directory link");
    drop(c);

    let mnt = d.join("mnt");
    let h = mount(&db, &mnt);
    // every path the database holds is reachable through the mount
    let mut view = 0usize;
    for entry in walkdir::WalkDir::new(&mnt).follow_links(false).min_depth(1) {
        let e = entry.expect("walk the mount");
        let rel: &std::path::Path = e.path().strip_prefix(&mnt).unwrap();
        if rel.to_string_lossy().starts_with("usr/bin/sh") || !rel.as_os_str().is_empty() { view += 1; }
    }
    assert_eq!(view as i64, db_rows, "every database path is reachable through the mount");
    // the alias under the directory link resolves
    assert_eq!(std::fs::read_link(mnt.join("usr/bin/sh")).unwrap().to_string_lossy(), "dash");
    // and the mount serves as a sandbox root
    let run = Command::new("bwrap")
        .args(["--bind", mnt.to_str().unwrap(), "/", "--dev", "/dev", "--proc", "/proc",
               "--unshare-pid", "--die-with-parent", "--chdir", "/", "/bin/sh", "-c", "echo BWRAP_OK"])
        .output().expect("bwrap");
    let out = format!("{}{}", String::from_utf8_lossy(&run.stdout), String::from_utf8_lossy(&run.stderr));
    assert!(out.contains("BWRAP_OK"), "the mount can serve as a sandbox root: {}", out);
    drop(h);
    let _ = std::fs::remove_dir_all(&d);
}
