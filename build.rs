//! Builds the session supervisor (src/session/init.c) as a static executable. It runs inside every
//! session's sandbox, so it must not depend on the VM's C library (an Alpine VM has musl, not glibc).
//! Needs a C compiler and a static C library (Debian: libc6-dev).
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let src = "src/session/init.c";
    println!("cargo:rerun-if-changed={}", src);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("session-init");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-static", "-Os", "-s", "-Wall", "-Wextra", "-o"]).arg(&out).arg(src)
        .status()
        .unwrap_or_else(|e| panic!("run {} to build the session supervisor: {}", cc, e));
    assert!(status.success(), "building the session supervisor failed: a C compiler and a static C library (libc.a) are required");
}
