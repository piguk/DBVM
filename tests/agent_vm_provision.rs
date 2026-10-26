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
    run_in_tree_at(root, prog, args, "/")
}

fn run_in_tree_at(root: &Path, prog: &str, args: &[&str], cwd: &str) -> (bool, String) {
    let out = std::process::Command::new("bwrap")
        .args(["--bind", root.to_str().unwrap(), "/", "--dev", "/dev", "--proc", "/proc",
               "--unshare-pid", "--die-with-parent", "--chdir", cwd,
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

/// The dev tool set: a coding agent needs more than a shell. Each command is run inside the VM.
#[test]
#[ignore]
fn the_dev_tool_set_provides_git_python_make_and_gcc() {
    let (Some(node), Some(pi)) = (node_binary(), pi_dir()) else { eprintln!("skip: node or pi missing"); return; };
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let d = std::env::temp_dir().join(format!("provdev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    let started = std::time::Instant::now();
    let rep = agent::provision_agent_with(&db, &node, &pi, agent::ToolSet::Dev).unwrap();
    let provision_s = started.elapsed().as_secs();
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify: {}", v);
    drop(c);
    let root = d.join("mnt");
    vm::vm_open(db.to_str().unwrap()).and_then(|c| vm::vm_materialize_tree(&c, &root)).unwrap();

    // each tool must actually run, not merely be present
    // Versions only prove the binary starts. gcc and make are checked by compiling and running real code,
    // because a gcc missing its cc1/as/ld helpers passes `--version` and fails every compile (measured).
    let checks: [(&str, &[&str], &str); 3] = [
        ("/bin/git", &["--version"], "git version"),
        ("/bin/python3", &["-c", "import json,http.client,ssl,sqlite3;print('PY_STDLIB_OK')"], "PY_STDLIB_OK"),
        ("/bin/gcc", &["--version"], "gcc"),
    ];
    for (prog, args, want) in checks {
        let (ok, out) = run_in_tree(&root, prog, args);
        assert!(ok, "{} must run in the VM: {}", prog, out);
        assert!(out.contains(want), "{} must report {:?}, got {:?}", prog, want, out.lines().next());
    }

    // a real C program, built and run inside the VM
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("h.c"), b"#include <stdio.h>\nint main(){printf(\"CC_OK\\n\");return 0;}\n").unwrap();
    let (ok, out) = run_in_tree_at(&root, "/bin/gcc", &["-o", "h", "h.c"], "/work");
    assert!(ok, "gcc must compile a program that includes stdio.h: {}", out);
    let (ok, out) = run_in_tree_at(&root, "/work/h", &[], "/work");
    assert!(ok && out.contains("CC_OK"), "the compiled program must run: {}", out);

    // and make must drive that same compiler, not just print its version
    std::fs::write(work.join("Makefile"), b"all: m\nm: m.c\n\tgcc -o m m.c\n").unwrap();
    std::fs::write(work.join("m.c"), b"#include <stdio.h>\nint main(){puts(\"MAKE_OK\");return 0;}\n").unwrap();
    let (ok, out) = run_in_tree_at(&root, "/bin/make", &[], "/work");
    assert!(ok, "make must build a C target: {}", out);
    let (ok, out) = run_in_tree_at(&root, "/work/m", &[], "/work");
    assert!(ok && out.contains("MAKE_OK"), "the program make built must run: {}", out);
    let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    eprintln!("dev tools: {} bytes, {} node objects, {} pi files, provision {} s",
        size, rep.objects, rep.files, provision_s);
    let _ = std::fs::remove_dir_all(&d);
}
