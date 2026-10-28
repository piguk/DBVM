//! The EDA pack end to end: a template from this host's packages (packs/packkit/host-rootfs.sh), tasks
//! generated and kept by `packkit build`, and the judges checked against known ways of gaming them
//! (`packkit check`). Needs a Debian or Ubuntu host with verilator, ngspice, g++, make, perl and python3.
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
    let st = Command::new(root.join("packs/packkit/host-rootfs.sh")).arg(&rootfs).args(["verilator", "ngspice", "g++", "make", "perl", "python3"])
        .env("TMPDIR", &s.dir).status().unwrap();
    assert!(st.success());
    let tpl = s.svc.store().import_image(s.svc.control(), "eda", &rootfs).unwrap();
    std::fs::remove_file(&rootfs).unwrap();
    let c = tenant(&s, "eda", Quotas::default());
    let (tasks, refs) = (s.dir.join("tasks.json"), s.dir.join("refs.json"));
    let run = |args: &[&str]| {
        let path = std::env::join_paths([root.join("packs"), root.join("sdk/python")]).unwrap();
        let out = Command::new("python3").args(["-B", "-m", "packkit"]).args(args)
            .env("PYTHONPATH", path).env("SELFDB_ENV_API_KEY", &c.key).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        eprintln!("{}", text);
        assert!(out.status.success(), "{}", text);
        text
    };
    let pack = root.join("packs/eda");
    let built = run(&["build", pack.to_str().unwrap(), &s.base, &tpl.id, "--per-family", "1",
                      "--out", tasks.to_str().unwrap(), "--references", refs.to_str().unwrap()]);
    assert!(built.contains("short of the count asked: none"), "{}", built);
    let list: serde_json::Value = serde_json::from_slice(&std::fs::read(&tasks).unwrap()).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 12, "8 designs and 4 circuit kinds");
    let checked = run(&["check", &s.base, tasks.to_str().unwrap(), refs.to_str().unwrap()]);
    assert!(checked.contains("checked 12 families: ok"), "{}", checked);
}

/// The fpga tasks: a template with YoWASP, its compile cache warmed and frozen, tasks whose targets are measured
/// with YoWASP on this host, checked like the others. Runs when YOWASP names the directory yowasp-yosys and
/// yowasp-nextpnr-ice40 are installed in (`pip install --target`).
#[test]
#[ignore]
fn the_fpga_tasks_build_and_check() {
    if !bwrap_ok() { return; }
    let Some(yowasp) = std::env::var_os("YOWASP") else { eprintln!("skip: YOWASP is not set"); return };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let s = start("eda-fpga");
    let rootfs = s.dir.join("fpga-rootfs.tar");
    assert!(Command::new(root.join("packs/eda/fpga-template.sh")).arg(&rootfs).env("YOWASP", &yowasp).env("TMPDIR", &s.dir).status().unwrap().success());
    let tpl = s.svc.store().import_image(s.svc.control(), "eda-yowasp", &rootfs).unwrap();
    std::fs::remove_file(&rootfs).unwrap();
    let c = tenant(&s, "eda", Quotas::default());
    let path = std::env::join_paths([root.join("packs"), root.join("sdk/python"), root.join("packs/eda")]).unwrap();
    let run = |args: Vec<&std::ffi::OsStr>| {
        let out = Command::new("python3").arg("-B").args(args).env("PYTHONPATH", &path).env("SELFDB_ENV_API_KEY", &c.key)
            .env("YOWASP", &yowasp).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        eprintln!("{}", text);
        assert!(out.status.success(), "{}", text);
        text
    };
    let warm = root.join("packs/eda/warm_fpga.py");
    let warmed = run(vec![warm.as_os_str(), s.base.as_ref(), tpl.id.as_ref()]).trim().lines().last().unwrap().to_string();
    let (tasks, refs, pack) = (s.dir.join("tasks.json"), s.dir.join("refs.json"), root.join("packs/eda"));
    let built = run(vec!["-m".as_ref(), "packkit".as_ref(), "build".as_ref(), pack.as_os_str(), s.base.as_ref(), warmed.as_ref(),
                         "--per-family".as_ref(), "1".as_ref(), "--families".as_ref(), "fpga/".as_ref(),
                         "--out".as_ref(), tasks.as_os_str(), "--references".as_ref(), refs.as_os_str()]);
    assert!(built.contains("3 tasks") && built.contains("short of the count asked: none"), "{}", built);
    let checked = run(vec!["-m".as_ref(), "packkit".as_ref(), "check".as_ref(), s.base.as_ref(), tasks.as_os_str(), refs.as_os_str()]);
    assert!(checked.contains("checked 3 families: ok"), "{}", checked);
}
