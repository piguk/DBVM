//! The data and ops packs end to end on a python3 template from this host's packages: tasks generated and kept
//! by `packkit build`, judges checked by `packkit check` (solutions score 1, cheats 0, read-only paths hold) and
//! audited (`packkit audit`: no high findings). And the audit and the red team against a pack with known
//! weaknesses (tests/fixtures/weak_pack). And the repository pack on a small repository with a history
//! (tests/fixtures/repo_pack). Needs a Debian or Ubuntu host with python3.
//!
//! `cargo test --release --test packs -- --ignored --nocapture`
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

struct Packs { s: Server, c: Client, tpl: String, root: std::path::PathBuf }

/// A service with a python3 template from this host's packages, and a tenant; None if the host cannot.
fn python_service(name: &str) -> Option<Packs> {
    if !bwrap_ok() { return None; }
    if !Command::new("sh").args(["-c", "command -v dpkg && command -v python3"]).output().map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skip: needs dpkg and python3");
        return None;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    let s = start(name);
    let rootfs = s.dir.join("python-rootfs.tar");
    assert!(Command::new(root.join("packs/packkit/host-rootfs.sh")).arg(&rootfs).arg("python3").env("TMPDIR", &s.dir).status().unwrap().success());
    let tpl = s.svc.store().import_image(s.svc.control(), "python", &rootfs).unwrap().id;
    std::fs::remove_file(&rootfs).unwrap();
    let c = tenant(&s, "a", Quotas::default());
    Some(Packs { s, c, tpl, root })
}

impl Packs {
    /// `python3 -m packkit ARGS`; its output. `ok`: whether it must succeed.
    fn packkit(&self, args: &[&str], ok: bool) -> String { self.packkit_env(args, ok, &[]) }

    fn packkit_env(&self, args: &[&str], ok: bool, env: &[(&str, &str)]) -> String {
        let path = std::env::join_paths([self.root.join("packs"), self.root.join("sdk/python")]).unwrap();
        let out = Command::new("python3").args(["-B", "-m", "packkit"]).args(args).env("PYTHONPATH", path).env("SELFDB_ENV_API_KEY", &self.c.key)
            .envs(env.iter().copied()).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        eprintln!("{}", text);
        if ok { assert!(out.status.success(), "{}", text); }
        text
    }

    fn file(&self, name: &str) -> String { self.s.dir.join(name).to_str().unwrap().to_string() }

    fn build(&self, pack: &Path, per_family: &str) {
        self.packkit(&["build", pack.to_str().unwrap(), &self.s.base, &self.tpl, "--per-family", per_family, "--out", &self.file("tasks.json"),
                       "--references", &self.file("refs.json")], true);
    }
}

fn pack_roundtrip(pack: &str, families: usize) {
    let Some(p) = python_service(&format!("pack-{}", pack)) else { return };
    p.build(&p.root.join("packs").join(pack), "2");
    let list: serde_json::Value = serde_json::from_slice(&std::fs::read(p.file("tasks.json")).unwrap()).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2 * families);
    let checked = p.packkit(&["check", &p.s.base, &p.file("tasks.json"), &p.file("refs.json")], true);
    assert!(checked.contains(&format!("checked {} families: ok", families)), "{}", checked);
    let audited = p.packkit(&["audit", &p.s.base, &p.file("tasks.json"), &p.file("refs.json"), "--out", &p.file("audit.json")], true);
    assert!(audited.contains(&format!("audited {} families: 0 high", families)), "{}", audited);
}

#[test]
#[ignore]
fn the_audit_and_the_red_team_find_weak_judges() {
    let Some(p) = python_service("weak") else { return };
    p.build(&p.root.join("tests/fixtures/weak_pack"), "1");
    let out = p.packkit(&["audit", &p.s.base, &p.file("tasks.json"), &p.file("refs.json"), "--out", &p.file("audit.json")], false);
    let report: serde_json::Value = serde_json::from_slice(&std::fs::read(p.file("audit.json")).unwrap()).unwrap();
    let kinds = |family: &str| -> Vec<String> {
        report[family].as_array().unwrap().iter().filter(|f| f["severity"] == "high").map(|f| f["kind"].as_str().unwrap().to_string()).collect()
    };
    assert!(kinds("weak/judge_in_reach").contains(&"judge in reach".to_string()), "{}", out);
    assert!(kinds("weak/answer_visible").contains(&"answer visible".to_string()), "{}", out);
    assert!(kinds("weak/trivial").contains(&"trivial answer".to_string()), "{}", out);
    assert!(kinds("weak/unittest_shadow").contains(&"test runner hook".to_string()), "{}", out);
    assert!(kinds("sound").is_empty(), "{}", out);
    // a model that drops a unittest.py into the project and submits
    let hack = "import sys\nprint('OK')\nsys.exit(0)\n";
    let model = fake_model(vec![
        serde_json::json!({"name": "write_file", "arguments": serde_json::json!({"path": "/work/proj/unittest.py", "content": hack}).to_string()}),
        serde_json::json!({"name": "submit", "arguments": "{}"}),
    ]);
    let red = p.packkit(&["redteam", &p.s.base, &p.file("tasks.json"), "--model", &format!("http://127.0.0.1:{}/v1", model), "--attempts", "2",
                          "--out", &p.file("redteam.jsonl")], false);
    let flagged: std::collections::BTreeSet<String> = std::fs::read_to_string(p.file("redteam.jsonl")).unwrap().lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()).filter(|r| r["flagged"] == true)
        .map(|r| r["family"].as_str().unwrap().to_string()).collect();
    assert_eq!(flagged.into_iter().collect::<Vec<_>>(), vec!["weak/unittest_shadow"], "{}", red);
}

#[test]
#[ignore]
fn the_data_pack_builds_checked_tasks() { pack_roundtrip("data", 5); }

#[test]
#[ignore]
fn the_ops_pack_builds_checked_tasks() { pack_roundtrip("ops", 3); }

#[test]
#[ignore]
fn the_repo_pack_makes_tasks_from_a_repository() {
    let Some(p) = python_service("repo") else { return };
    let fixture = p.root.join("tests/fixtures/repo_pack");
    let checkout = p.s.dir.join("calc");
    assert!(Command::new("python3").arg(fixture.join("make_repo.py")).arg(&checkout).status().unwrap().success());
    let config = p.file("repo.json");
    std::fs::copy(fixture.join("repo.json"), &config).unwrap();
    // the template: the repository at HEAD under /work/repo, without the hidden test
    let build = p.c.ok("POST", "/v1/sessions", Some(json!({"template_id": p.tpl})))["session_id"].as_str().unwrap().to_string();
    let files = Command::new("git").arg("-C").arg(&checkout).args(["ls-files"]).output().unwrap().stdout;
    for f in String::from_utf8(files).unwrap().lines().filter(|f| *f != "tests/test_hidden.py") {
        let content = std::fs::read_to_string(checkout.join(f)).unwrap();
        let w = p.c.act(&build, json!({"tool": "write_file", "path": format!("/work/repo/{}", f), "content": content}));
        assert_eq!(w["exit_code"], 0, "{}", w);
    }
    let tpl = p.c.ok("POST", &format!("/v1/sessions/{}/freeze", build), Some(json!({"name": "calc"})))["id"].as_str().unwrap().to_string();
    p.c.ok("DELETE", &format!("/v1/sessions/{}", build), None);

    let env = [("REPO_PACK_CONFIG", config.as_str())];
    p.packkit_env(&["build", p.root.join("packs/repo").to_str().unwrap(), &p.s.base, &tpl, "--per-family", "2",
                    "--out", &p.file("tasks.json"), "--references", &p.file("refs.json")], true, &env);
    let tasks: Value = serde_json::from_slice(&std::fs::read(p.file("tasks.json")).unwrap()).unwrap();
    let tasks = tasks.as_array().unwrap();
    let modes: Vec<&str> = tasks.iter().map(|t| t["mode"].as_str().unwrap()).collect();
    assert_eq!(modes.iter().filter(|m| **m == "mutation").count(), 2, "{:?}", tasks);
    // both past commits that changed the source: the discount fix (undone next to a later change) and the tax
    assert_eq!(modes.iter().filter(|m| **m == "history").count(), 2, "{:?}", tasks);
    let checked = p.packkit(&["check", &p.s.base, &p.file("tasks.json"), &p.file("refs.json")], true);
    assert!(checked.contains("checked 2 families: ok"), "{}", checked);
    let audited = p.packkit(&["audit", &p.s.base, &p.file("tasks.json"), &p.file("refs.json"), "--out", &p.file("audit.json")], true);
    assert!(audited.contains("audited 2 families: 0 high"), "{}", audited);

    // the hidden test is not in a rollout; code that exits before the tests run scores nothing
    let task = tasks.iter().find(|t| t["mode"] == "history").unwrap()["task_id"].as_str().unwrap().to_string();
    let r = p.c.ok("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": [{"label": "hack"}]})))["session_ids"][0].as_str().unwrap().to_string();
    assert_ne!(p.c.bash(&r, "test -e /work/repo/tests/test_hidden.py")["exit_code"], 0);
    p.c.act(&r, json!({"tool": "write_file", "path": "/work/repo/calc/pricing.py", "content": "import os\nos._exit(0)\n"}));
    let v = p.c.ok("POST", &format!("/v1/sessions/{}/judge", r), None);
    assert_eq!(v["reward"], 0.0, "{}", v);
}
