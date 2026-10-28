//! The data and ops packs end to end on a python3 template from this host's packages: tasks generated and kept
//! by `packkit build`, judges checked by `packkit check` (solutions score 1, cheats 0, read-only paths hold).
//! Needs a Debian or Ubuntu host with python3.
//!
//! `cargo test --release --test packs -- --ignored --nocapture`
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;
use std::path::Path;
use std::process::Command;

fn pack_roundtrip(pack: &str, families: usize) {
    if !bwrap_ok() { return; }
    if !Command::new("sh").args(["-c", "command -v dpkg && command -v python3"]).output().map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skip: needs dpkg and python3");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let s = start(&format!("pack-{}", pack));
    let rootfs = s.dir.join("python-rootfs.tar");
    assert!(Command::new(root.join("packs/packkit/host-rootfs.sh")).arg(&rootfs).arg("python3").env("TMPDIR", &s.dir).status().unwrap().success());
    let tpl = s.svc.store().import_image(s.svc.control(), "python", &rootfs).unwrap();
    std::fs::remove_file(&rootfs).unwrap();
    let c = tenant(&s, "a", Quotas::default());
    let run = |args: &[&str]| {
        let path = std::env::join_paths([root.join("packs"), root.join("sdk/python")]).unwrap();
        let out = Command::new("python3").args(["-B", "-m", "packkit"]).args(args).env("PYTHONPATH", path).env("SELFDB_ENV_API_KEY", &c.key).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        eprintln!("{}", text);
        assert!(out.status.success(), "{}", text);
        text
    };
    let (tasks, refs) = (s.dir.join("tasks.json"), s.dir.join("refs.json"));
    let dir = root.join("packs").join(pack);
    run(&["build", dir.to_str().unwrap(), &s.base, &tpl.id, "--per-family", "2", "--out", tasks.to_str().unwrap(), "--references", refs.to_str().unwrap()]);
    let list: serde_json::Value = serde_json::from_slice(&std::fs::read(&tasks).unwrap()).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2 * families);
    let checked = run(&["check", &s.base, tasks.to_str().unwrap(), refs.to_str().unwrap()]);
    assert!(checked.contains(&format!("checked {} families: ok", families)), "{}", checked);
}

#[test]
#[ignore]
fn the_data_pack_builds_checked_tasks() { pack_roundtrip("data", 5); }

#[test]
#[ignore]
fn the_ops_pack_builds_checked_tasks() { pack_roundtrip("ops", 3); }
