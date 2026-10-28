//! The EDA pack end to end: a template from this host's packages (packs/eda/scripts/host-rootfs.sh), tasks
//! generated and checked by packs/eda/build_tasks.py, and the judges against known ways of gaming them
//! (packs/eda/check_pack.py). Needs a Debian or Ubuntu host with verilator, ngspice, g++, make, perl and python3.
//!
//! `cargo test --release --test eda_pack -- --ignored --nocapture` (a few minutes: a 650 MB template)
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;
use std::path::Path;
use std::process::Command;

fn have(cmd: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {} >/dev/null", cmd)).status().map(|s| s.success()).unwrap_or(false)
}

#[test]
#[ignore]
fn the_eda_pack_builds_checked_tasks() {
    if !bwrap_ok() { return; }
    for c in ["dpkg", "verilator", "ngspice", "g++", "make", "perl", "python3"] {
        if !have(c) { eprintln!("skip: {} not found", c); return; }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let s = start("eda");
    let rootfs = s.dir.join("eda-rootfs.tar");
    let st = Command::new(root.join("packs/eda/scripts/host-rootfs.sh")).arg(&rootfs).env("TMPDIR", &s.dir).status().unwrap();
    assert!(st.success());
    let tpl = s.svc.store().import_image(s.svc.control(), "eda", &rootfs).unwrap();
    std::fs::remove_file(&rootfs).unwrap();
    let c = tenant(&s, "eda", Quotas::default());
    let (tasks, refs) = (s.dir.join("tasks.json"), s.dir.join("refs.json"));
    let run = |script: &str, args: &[&str]| {
        let out = Command::new("python3").arg("-B").arg(root.join("packs/eda").join(script)).args(args)
            .env("PYTHONPATH", root.join("sdk/python")).env("SELFDB_ENV_API_KEY", &c.key).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        eprintln!("{}", text);
        assert!(out.status.success(), "{}", text);
        text
    };
    let built = run("build_tasks.py", &[&s.base, &tpl.id, "--rtl-per-design", "1", "--analog-per-kind", "1",
                                        "--out", tasks.to_str().unwrap(), "--references", refs.to_str().unwrap()]);
    assert!(built.contains("short of the count asked: none"), "{}", built);
    let list: serde_json::Value = serde_json::from_slice(&std::fs::read(&tasks).unwrap()).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 12, "8 designs and 4 circuit kinds");
    run("check_pack.py", &[&s.base, tasks.to_str().unwrap(), refs.to_str().unwrap()]);
}
