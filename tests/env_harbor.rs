//! The Harbor adapter under Harbor itself: `harbor run` with the environment loaded by import path, the oracle
//! agent solving a task and Harbor's verifier scoring it, the nop agent leaving it unsolved. Runs when
//! SELFDB_ADAPTER_PYTHON names a Python whose environment has harbor installed (its `harbor` command beside it).
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;
use std::path::{Path, PathBuf};

/// The task; with `separate`, its verifier runs in an environment of its own, which gets the answer as an
/// artifact (Harbor's default mode).
fn task_dir(root: &Path, separate: bool) -> PathBuf {
    let t = root.join(if separate { "calc-separate" } else { "calc" });
    for d in ["environment", "solution", "tests"] { std::fs::create_dir_all(t.join(d)).unwrap(); }
    // Harbor puts an artifact back at its own path in the verifier's environment
    let (artifacts, mode) = if separate { (r#"["/app/answer"]"#, "separate") } else { ("[]", "shared") };
    let answer = "$(cat /app/answer 2>/dev/null)";
    std::fs::write(t.join("task.toml"), format!(r#"schema_version = "1.4"
artifacts = {}

[task]
name = "selfdb/calc"
version = "1.0.0"
description = "write the answer"

[verifier]
timeout_sec = 120.0
environment_mode = "{}"

[agent]
timeout_sec = 120.0

[environment]
docker_image = "mini"
network_mode = "no-network"
build_timeout_sec = 120.0
os = "linux"
"#, artifacts, mode)).unwrap();
    std::fs::write(t.join("instruction.md"), "Write 42 to /app/answer.\n").unwrap();
    std::fs::write(t.join("environment/Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(t.join("solution/solve.sh"), "#!/bin/bash\nmkdir -p /app && echo 42 > /app/answer\n").unwrap();
    std::fs::write(t.join("tests/test.sh"), format!("#!/bin/bash\nmkdir -p /logs/verifier\nif [ \"{}\" = 42 ]; then echo 1 > /logs/verifier/reward.txt; else echo 0 > /logs/verifier/reward.txt; fi\n", answer)).unwrap();
    t
}

/// The rewards of the trials of the job in `jobs` (Harbor writes a result.json per trial).
fn rewards(jobs: &Path) -> Vec<serde_json::Value> {
    walkdir::WalkDir::new(jobs).into_iter().flatten().filter(|e| e.file_name() == "result.json")
        .filter_map(|e| serde_json::from_slice::<serde_json::Value>(&std::fs::read(e.path()).ok()?).ok())
        .filter_map(|v| v.pointer("/verifier_result/rewards").cloned()).collect()
}

#[test]
fn harbor_runs_a_task_on_the_service() {
    if !bwrap_ok() { return; }
    let Some(py) = std::env::var_os("SELFDB_ADAPTER_PYTHON") else { eprintln!("skip: SELFDB_ADAPTER_PYTHON is not set"); return };
    let harbor = Path::new(&py).parent().unwrap().join("harbor");
    let s = start("harbor");
    s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let c = tenant(&s, "a", Quotas::default());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (separate, agent, want) in [(false, "oracle", 1.0), (false, "nop", 0.0), (true, "oracle", 1.0), (true, "nop", 0.0)] {
        let task = task_dir(&s.dir, separate);
        let jobs = s.dir.join(format!("jobs-{}-{}", agent, separate));
        let out = std::process::Command::new(&harbor).arg("run").arg("-p").arg(&task).args(["-a", agent, "-e", "selfdb_env.harbor:SelfdbEnvironment"])
            .args(["--ek", &format!("base_url={}", s.base), "-y"]).arg("-o").arg(&jobs)
            .env("PYTHONPATH", root.join("sdk/python")).env("SELFDB_ENV_API_KEY", &c.key).current_dir(&s.dir).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let r = rewards(&jobs);
        assert_eq!(r.len(), 1, "{} (separate verifier: {}): one trial with a verifier result\n{}", agent, separate, text);
        assert_eq!(r[0]["reward"].as_f64(), Some(want), "{} (separate verifier: {}): {:?}\n{}", agent, separate, r, text);
    }
    // every trial's session ended
    let t = s.svc.authenticate(&c.key).unwrap();
    assert!(s.svc.usage(&t).unwrap().iter().all(|u| u.ended_at.is_some()));
}
