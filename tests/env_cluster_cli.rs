//! A coordinator and a node as the `self` binary runs them: token files, a node registering itself, sessions
//! through the coordinator, metrics endpoints.
mod env_common;
use env_common::*;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Procs(Vec<Child>, std::path::PathBuf);
impl Drop for Procs {
    fn drop(&mut self) {
        for c in &mut self.0 { let _ = c.kill(); let _ = c.wait(); }
        let _ = selfdb::vm::remove_tree(&self.1);
    }
}

fn free_port() -> u16 { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() }

fn admin(data: &std::path::Path, node: &str, args: &[&str]) -> String {
    let o = Command::new(env!("CARGO_BIN_EXE_self")).arg("env-admin").arg("--data").arg(data).arg("--node").arg(node).args(args).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim().to_string()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_binary_runs_a_cluster() {
    if !bwrap_ok() { return; }
    let dir = std::env::temp_dir().join(format!("env-cluster-cli-{}", std::process::id()));
    let _ = selfdb::vm::remove_tree(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut procs = Procs(Vec::new(), dir.clone());
    let token = dir.join("cluster-token");
    std::fs::write(&token, selfdb::env::control::random_hex(24).unwrap()).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
    let exe = env!("CARGO_BIN_EXE_self");
    let (cp, np, mp) = (free_port(), free_port(), free_port());
    let coordinator = |procs: &mut Procs| {
        procs.0.push(Command::new(exe).args(["env-coordinator", "--listen", &format!("127.0.0.1:{}", cp), "--metrics-listen", &format!("127.0.0.1:{}", mp)])
            .arg("--data").arg(dir.join("coord")).arg("--token-file").arg(&token).stderr(Stdio::piped()).spawn().unwrap());
    };
    // a token others can read is refused
    coordinator(&mut procs);
    let out = procs.0.pop().unwrap().wait_with_output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("readable by others"), "{}", String::from_utf8_lossy(&out.stderr));
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    coordinator(&mut procs);

    let node_data = dir.join("n1");
    admin(&node_data, "n1", &["image", "mini", rootfs(&dir).to_str().unwrap()]);
    let tenant = admin(&dir.join("coord"), "c1", &["tenant", "a"]);
    let key = admin(&dir.join("coord"), "c1", &["key", &tenant]);
    procs.0.push(Command::new(exe).args(["env-serve", "--node", "n1", "--listen", &format!("127.0.0.1:{}", np), "--max-sessions", "4"])
        .args(["--coordinator", &format!("http://127.0.0.1:{}", cp), "--advertise", &format!("http://127.0.0.1:{}", np)])
        .arg("--data").arg(&node_data).arg("--token-file").arg(&token).stderr(Stdio::null()).spawn().unwrap());

    let c = Client { base: format!("http://127.0.0.1:{}", cp), key };
    wait_for("the node to register", || c.req("GET", "/v1/templates", None).1.as_array().map(|a| !a.is_empty()).unwrap_or(false));
    let tpl = c.ok("GET", "/v1/templates", None)[0]["id"].as_str().unwrap().to_string();
    let sid = c.ok("POST", "/v1/sessions", Some(serde_json::json!({"template_id": tpl})))["session_id"].as_str().unwrap().to_string();
    assert!(sid.starts_with("n1-"));
    assert_eq!(c.bash(&sid, "echo through the coordinator")["output"], "through the coordinator\n");
    let metrics = ureq::get(&format!("http://127.0.0.1:{}/metrics", mp)).call().unwrap().into_string().unwrap();
    assert!(metrics.contains("selfdb_node_up{node=\"n1\"} 1"), "{}", metrics);
    c.ok("DELETE", &format!("/v1/sessions/{}", sid), None);
}
