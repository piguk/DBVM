//! Provisioning a VM with node and pi, then running them inside the VM.
//!
//! Slow (it imports node's libraries and packs 15k pi files), so these are `#[ignore]`: run with
//! `cargo test --release --test agent_vm_provision -- --ignored --nocapture`.
use selfdb::{agent, vm};
use std::path::{Path, PathBuf};

fn node_binary() -> Option<PathBuf> {
    let out = std::process::Command::new("sh")
        .args(["-c", "readlink -f \"$(command -v node)\""]).output().ok()?;
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if p.exists() { Some(p) } else { None }
}

fn pi_dir() -> Option<PathBuf> {
    for c in [
        "/home/linuxbrew/.linuxbrew/lib/node_modules/@earendil-works/pi-coding-agent",
        "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent",
    ] {
        let p = PathBuf::from(c);
        if p.exists() { return Some(p); }
    }
    None
}

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Run a command inside the materialised tree, returning its stdout.
fn run_in_tree(root: &Path, prog: &str, args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("bwrap")
        .args(["--bind", root.to_str().unwrap(), "/", "--dev", "/dev", "--proc", "/proc",
               "--unshare-pid", "--die-with-parent", "--chdir", "/",
               "--setenv", "HOME", "/root", "--setenv", "PATH", "/usr/bin:/bin"])
        .arg(prog).args(args).output().expect("bwrap");
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
#[ignore]
fn node_and_pi_run_inside_the_vm() {
    let (Some(node), Some(pi)) = (node_binary(), pi_dir()) else {
        eprintln!("skip: node or pi not installed"); return;
    };
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let d = std::env::temp_dir().join(format!("prov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());

    let rep = agent::provision_agent(&db, &node, &pi).unwrap();
    assert!(rep.objects > 0, "node's library closure was imported");
    assert!(rep.files > 1000, "pi tree was packed, got {} files", rep.files);

    // the database stays consistent and compressed
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify: {}", v);
    drop(c);

    let root = d.join("mnt");
    vm::vm_open(db.to_str().unwrap()).and_then(|c| vm::vm_materialize_tree(&c, &root)).unwrap();

    // node runs with nothing from the host
    let (ok, out) = run_in_tree(&root, "/usr/bin/node", &["--version"]);
    assert!(ok, "node must run in the VM: {}", out);
    assert!(out.trim().starts_with('v'), "node prints its version, got {:?}", out.trim());

    // and so does pi, through the well-known path
    let (ok, out) = run_in_tree(&root, "/usr/bin/node", &["/opt/pi/dist/bundle/cli.js", "--version"]);
    assert!(ok, "pi must run in the VM: {}", out);
    assert!(!out.trim().is_empty(), "pi prints its version, got {:?}", out.trim());

    let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    eprintln!("provisioned vm: {} bytes, node objects={}, pi files={}, pi version={:?}", size, rep.objects, rep.files, out.trim());
    let _ = std::fs::remove_dir_all(&d);
}
