//! The Python SDK against a running service: sdk/python/examples/rollouts.py drives seeded rollouts with
//! asyncio, a batch of actions, a fork and the judge. Skipped without python3.
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;

#[test]
fn the_python_sdk_runs_the_rollouts_example() {
    if !bwrap_ok() { return; }
    let Ok(py) = which("python3") else { eprintln!("skip: python3 not found"); return };
    let s = start("sdk");
    let c = tenant(&s, "sdk", Quotas::default());
    let task = calc_task(&s, &c);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/python");
    let out = std::process::Command::new(py).arg(root.join("examples/rollouts.py")).args([&s.base, &c.key, &task])
        .env("PYTHONPATH", &root).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("ok"), "{}", text);
}

fn which(name: &str) -> Result<std::path::PathBuf, ()> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(name)).find(|p| p.is_file())).ok_or(())
}
