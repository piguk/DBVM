//! One person's setup: `self env-local` (a tenant and a key made on first use, the key readable by its owner only),
//! and sdk/python/examples/local_agent.py driving rollouts with a model behind an OpenAI-compatible endpoint (a
//! stand-in that answers with tool calls), writing rollouts with rewards and advantages.
mod env_common;
use env_common::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Killed(Child);
impl Drop for Killed { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

/// A chat-completions endpoint that writes the answer with its first tool call and submits with its second.
fn fake_model() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || for c in l.incoming().flatten() {
        let mut r = BufReader::new(c.try_clone().unwrap());
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" { break; }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") { len = v.trim().parse().unwrap_or(0); }
        }
        let mut body = vec![0u8; len];
        let _ = r.read_exact(&mut body);
        let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let tools_done = req["messages"].as_array().map(|m| m.iter().filter(|x| x["role"] == "tool").count()).unwrap_or(0);
        let call = if tools_done == 0 {
            serde_json::json!({"id": "c1", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\": \"/work/answer\", \"content\": \"42\\n\"}"}})
        } else {
            serde_json::json!({"id": "c2", "type": "function", "function": {"name": "submit", "arguments": "{}"}})
        };
        let resp = serde_json::json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [call]}}]}).to_string();
        let mut c = c;
        let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", resp.len(), resp).as_bytes());
    });
    port
}

#[test]
fn one_machine_one_command() {
    if !bwrap_ok() { return; }
    let Ok(_) = Command::new("python3").arg("--version").output() else { eprintln!("skip: python3 not found"); return };
    let dir = std::env::temp_dir().join(format!("env-local-{}", std::process::id()));
    let _ = selfdb::vm::remove_tree(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("xdg/selfdb/local");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let image = rootfs(&dir);
    let _svc = Killed(Command::new(env!("CARGO_BIN_EXE_self")).arg("env-local").arg("--data").arg(&data)
        .args(["--listen", &format!("127.0.0.1:{}", port), "--image-name", "mini"]).arg("--image").arg(&image)
        .stderr(Stdio::null()).spawn().unwrap());
    let key_file = data.join("local-key");
    let t0 = Instant::now();
    let base = format!("http://127.0.0.1:{}", port);
    while ureq::get(&format!("{}/v1/templates", base)).call().map(|_| false).unwrap_or_else(|e| !matches!(e, ureq::Error::Status(401, _))) {
        assert!(t0.elapsed() < Duration::from_secs(60), "env-local did not come up");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777, 0o600);
    let c = Client { base: base.clone(), key: std::fs::read_to_string(&key_file).unwrap().trim().to_string() };
    let tpl = c.ok("GET", "/v1/templates", None)[0]["id"].as_str().unwrap().to_string();
    let check = "#!/bin/sh\nif [ \"$(cat /work/answer)\" = 42 ]; then echo '{\"reward\": 1}'; else echo '{\"reward\": 0}'; fi\n";
    let task = c.ok("POST", "/v1/tasks", Some(serde_json::json!({"template_id": tpl,
        "judge": {"cmd": ["/judge/check.sh"], "sees": ["/work"], "files": [{"path": "/judge/check.sh", "content": check, "mode": 0o755}]}})))
        ["task_id"].as_str().unwrap().to_string();
    std::fs::write(dir.join("tasks.json"), serde_json::json!([{"task_id": task, "instruction": "Write 42 to /work/answer."}]).to_string()).unwrap();

    let model = fake_model();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("python3").arg("-B").arg(root.join("sdk/python/examples/local_agent.py")).arg(dir.join("tasks.json"))
        .args(["--model", &format!("http://127.0.0.1:{}/v1", model), "--group", "3", "--out"]).arg(dir.join("rollouts.jsonl"))
        .env("PYTHONPATH", root.join("sdk/python")).env("SELFDB_ENV_URL", &base).env("XDG_DATA_HOME", dir.join("xdg"))
        .env_remove("SELFDB_ENV_API_KEY").output().unwrap();
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(dir.join("rollouts.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 3, "a group of 3");
    for r in &rows {
        assert_eq!((r["reward"].as_f64(), r["advantage"].as_f64()), (Some(1.0), Some(0.0)), "{}", r);
        assert!(r["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool"));
    }
    let _ = selfdb::vm::remove_tree(&dir);
}
