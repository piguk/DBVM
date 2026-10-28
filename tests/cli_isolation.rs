//! `vm-chroot` and `vm-exec` run VM content through the same sandbox as `vm-step`. Before, `vm-chroot`
//! passed the host environment into the sandbox and had no network namespace, and `vm-exec` ran the VM
//! binary directly on the host.
use selfdb::vm;
use std::path::PathBuf;
use std::process::Command;

fn bwrap_ok() -> bool {
    Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn vm_with_shell(name: &str) -> (PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("cli-iso-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    for prog in ["/bin/sh", "/bin/cat", "/bin/tail", "/bin/cut", "/bin/tr", "/usr/bin/env"] {
        if std::path::Path::new(prog).exists() { vm::vm_import_closure(&c, prog, "/").unwrap(); }
    }
    (d, db)
}

fn self_bin() -> &'static str { env!("CARGO_BIN_EXE_self") }

#[test]
fn vm_chroot_has_no_host_environment_and_no_network() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = vm_with_shell("chroot");
    let out = Command::new(self_bin())
        .args(["vm-chroot", db.to_str().unwrap(), "--ephemeral", "/bin/sh", "-c",
               "env; echo IFACES; tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '; echo marker > /created-in-session"])
        .env("CLI_ISOLATION_SECRET", "s3cr3t-token")
        .output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{}", text);
    assert!(!text.contains("s3cr3t-token"), "the host environment leaked into vm-chroot: {}", text);
    let ifaces: Vec<&str> = text.lines().skip_while(|l| *l != "IFACES").skip(1).take_while(|l| !l.is_empty()).collect();
    assert_eq!(ifaces, vec!["lo"], "vm-chroot has no network: {}", text);
    // --ephemeral: the session's write does not reach the database
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert!(vm::vm_cat(&c, "/created-in-session").is_err(), "an ephemeral session wrote to the database");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn vm_exec_runs_the_binary_in_a_sandbox() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let (d, db) = vm_with_shell("exec");
    let out = Command::new(self_bin())
        // shell builtins only: the sandbox root holds just the binary and the VM's libraries
        .args(["vm-exec", db.to_str().unwrap(), "/bin/sh", "-c",
               "echo SECRET=$CLI_ISOLATION_SECRET; read h < /proc/sys/kernel/hostname; echo HOSTNAME=$h; test -d /home && echo HOME_VISIBLE; echo done"])
        .env("CLI_ISOLATION_SECRET", "s3cr3t-token")
        .output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("done"), "{}", text);
    assert!(!text.contains("s3cr3t-token"), "the host environment leaked into vm-exec: {}", text);
    assert!(text.contains("HOSTNAME=vm"), "runs in the sandbox's UTS namespace: {}", text);
    assert!(!text.contains("HOME_VISIBLE"), "the host's /home is not visible: {}", text);
    let _ = std::fs::remove_dir_all(&d);
}
