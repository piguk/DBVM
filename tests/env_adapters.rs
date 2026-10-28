//! The Harbor and OpenEnv adapters (sdk/python/selfdb_env/{harbor,openenv}.py) against a running service, with
//! stand-ins for the frameworks' interfaces (tests/adapter_stubs). Skipped without python3.
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;

#[test]
fn the_adapters_drive_sessions() {
    if !bwrap_ok() { return; }
    let Some(py) = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("python3")).find(|p| p.is_file())) else {
        eprintln!("skip: python3 not found");
        return;
    };
    let s = start("adapters");
    let c = tenant(&s, "a", Quotas::default());
    let task = calc_task(&s, &c);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let scratch = s.dir.join("py");
    std::fs::create_dir_all(&scratch).unwrap();
    let path = std::env::join_paths([root.join("sdk/python"), root.join("tests/adapter_stubs")]).unwrap();
    let out = std::process::Command::new(py).arg("-B").arg(root.join("tests/adapters/check_adapters.py")).arg(&s.base).arg(&task).arg(&scratch)
        .env("PYTHONPATH", path).env("SELFDB_ENV_API_KEY", &c.key).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("ok"), "{}", text);
}
