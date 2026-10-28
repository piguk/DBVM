//! The Harbor and OpenEnv adapters (sdk/python/selfdb_env/{harbor,openenv}.py) against a running service, with
//! stand-ins for the frameworks' interfaces (tests/adapter_stubs), and with the frameworks themselves when
//! SELFDB_ADAPTER_PYTHON names a Python that has them. Skipped without python3.
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
    run(&py, true);
}

/// Against the frameworks themselves: SELFDB_ADAPTER_PYTHON names a Python with harbor and openenv-core
/// installed (no stand-ins on its path).
#[test]
fn the_adapters_drive_sessions_under_the_real_frameworks() {
    if !bwrap_ok() { return; }
    let Some(py) = std::env::var_os("SELFDB_ADAPTER_PYTHON") else { eprintln!("skip: SELFDB_ADAPTER_PYTHON is not set"); return };
    run(std::path::Path::new(&py), false);
    // behind OpenEnv's own server and client
    let s = start("openenv-server");
    let c = tenant(&s, "a", Quotas::default());
    let task = calc_task(&s, &c);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new(&py).arg("-B").arg(root.join("tests/adapters/check_openenv_server.py")).arg(&s.base).arg(&task)
        .env("PYTHONPATH", root.join("sdk/python")).env("SELFDB_ENV_API_KEY", &c.key).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("ok"), "{}", text);
}

fn run(py: &std::path::Path, stubs: bool) {
    let s = start(if stubs { "adapters" } else { "adapters-real" });
    let c = tenant(&s, "a", Quotas::default());
    let task = calc_task(&s, &c);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let scratch = s.dir.join("py");
    std::fs::create_dir_all(&scratch).unwrap();
    let mut path = vec![root.join("sdk/python")];
    if stubs { path.push(root.join("tests/adapter_stubs")); }
    let out = std::process::Command::new(py).arg("-B").arg(root.join("tests/adapters/check_adapters.py")).arg(&s.base).arg(&task).arg(&scratch)
        .env("PYTHONPATH", std::env::join_paths(path).unwrap()).env("SELFDB_ENV_API_KEY", &c.key).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("ok"), "{}", text);
}
