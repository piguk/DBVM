//! Helpers shared by the environment service tests: a service on a free port, a small rootfs image, tenants.
#![allow(dead_code)]
use selfdb::env::{api, control::Quotas, service::Service};
use selfdb::vm;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Before the test harness starts its threads: the access a service has (see `selfdb::access`).
#[used]
#[link_section = ".init_array"]
static ENTER_OWNER_NAMESPACE: extern "C" fn() = {
    extern "C" fn enter() { let _ = selfdb::access::enter_owner_namespace(); }
    enter
};

pub fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

pub struct Server { pub base: String, pub svc: Arc<Service>, pub dir: PathBuf }
impl Drop for Server { fn drop(&mut self) { let _ = vm::remove_tree(&self.dir); } }

/// A small root filesystem (a shell and a few tools) as a tarball.
pub fn rootfs(dir: &Path) -> PathBuf { rootfs_with(dir, "rootfs", &[]) }

/// A small root filesystem (a shell and a few tools, and `extra` programs) as a tarball named `name`.tar.
pub fn rootfs_with(dir: &Path, name: &str, extra: &[&str]) -> PathBuf {
    let c = vm::init_vm_db_with_opts(dir.join(format!("{}.db", name)).to_str().unwrap(), true, true).unwrap();
    let base = ["/bin/sh", "/bin/cat", "/bin/mkdir", "/bin/rm", "/bin/ls", "/bin/chmod", "/usr/bin/head", "/bin/ln", "/usr/bin/readlink",
                "/usr/bin/cut", "/bin/sleep", "/bin/bash", "/usr/bin/tee", "/usr/bin/tail"];
    for prog in base.iter().chain(extra) {
        if Path::new(prog).exists() { vm::vm_import_closure(&c, prog, "/").unwrap(); }
    }
    let tree = dir.join(format!("{}-tree", name));
    vm::vm_materialize_tree(&c, &tree).unwrap();
    let tar_path = dir.join(format!("{}.tar", name));
    let mut b = tar::Builder::new(std::fs::File::create(&tar_path).unwrap());
    b.follow_symlinks(false);
    b.append_dir_all(".", &tree).unwrap();
    b.finish().unwrap();
    tar_path
}

pub fn start(name: &str) -> Server {
    let dir = std::env::temp_dir().join(format!("env-api-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let svc = Service::open(&dir.join("data"), "n1").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let s2 = svc.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(l.local_addr().unwrap().port()).unwrap();
            axum::serve(l, api::router(s2)).await.unwrap();
        });
    });
    let port = rx.recv().unwrap();
    Server { base: format!("http://127.0.0.1:{}", port), svc, dir }
}

pub struct Client { pub base: String, pub key: String }

impl Client {
    pub fn req(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let r = ureq::request(method, &format!("{}{}", self.base, path)).set("Authorization", &format!("Bearer {}", self.key));
        let res = match body { Some(b) => r.send_json(b), None => r.call() };
        match res {
            Ok(resp) => (resp.status(), resp.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(code, resp)) => (code, resp.into_json().unwrap_or(Value::Null)),
            Err(e) => panic!("{} {}: {}", method, path, e),
        }
    }
    pub fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (code, v) = self.req(method, path, body);
        assert_eq!(code, 200, "{} {}: {}", method, path, v);
        v
    }
    pub fn act(&self, sid: &str, action: Value) -> Value { self.ok("POST", &format!("/v1/sessions/{}/actions", sid), Some(action)) }
    pub fn bash(&self, sid: &str, cmd: &str) -> Value { self.act(sid, json!({"tool": "bash", "command": cmd})) }
}

pub fn tenant(s: &Server, name: &str, q: Quotas) -> Client {
    let t = s.svc.create_tenant(name, &q).unwrap();
    Client { base: s.base.clone(), key: s.svc.control().create_key(&t.id).unwrap() }
}

pub const JUDGE: &str = r#"#!/bin/sh
# reads only /app (the task's `sees`); a poison file elsewhere must not reach it
if [ -e /etc/poison ]; then echo '{"reward": 99}'; exit 0; fi
out=$(/bin/sh /app/calc.sh)
if [ "$out" = "5" ]; then echo '{"reward": 1, "out": "'$out'"}'; else echo '{"reward": 0, "out": "'$out'"}'; fi
"#;

/// The calc example: an image, a template with a broken /app/calc.sh and a judge at /tests/judge.sh, and a
/// task over it with /tests read-only and the judge seeing /app only. Returns the task id.
pub fn calc_task(s: &Server, c: &Client) -> String {
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let build = c.ok("POST", "/v1/sessions", Some(json!({"template_id": image.id})))["session_id"].as_str().unwrap().to_string();
    c.act(&build, json!({"tool": "write_file", "path": "/app/calc.sh", "content": "echo $((2 - 3))\n"}));
    c.act(&build, json!({"tool": "write_file", "path": "/tests/judge.sh", "content": JUDGE, "mode": 0o755}));
    let tpl = c.ok("POST", &format!("/v1/sessions/{}/freeze", build), Some(json!({"name": "calc"})))["id"].as_str().unwrap().to_string();
    c.ok("DELETE", &format!("/v1/sessions/{}", build), None);
    c.ok("POST", "/v1/tasks", Some(json!({
        "template_id": tpl, "readonly": ["/tests"],
        "judge": {"cmd": ["/bin/sh", "/tests/judge.sh"], "sees": ["/app"]}
    })))["task_id"].as_str().unwrap().to_string()
}

/// A chat-completions endpoint (OpenAI's format) that answers the n-th request of a conversation with the n-th of
/// `calls` (each {"name", "arguments"}) as a tool call: n is the number of tool results in the request.
pub fn fake_model(calls: Vec<Value>) -> u16 {
    use std::io::{BufRead, BufReader, Read, Write};
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
        let req: Value = serde_json::from_slice(&body).unwrap_or_default();
        let n = req["messages"].as_array().map(|m| m.iter().filter(|x| x["role"] == "tool").count()).unwrap_or(0);
        let call = calls.get(n).or(calls.last()).cloned().unwrap_or(json!({"name": "submit", "arguments": "{}"}));
        let resp = json!({"choices": [{"message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": format!("c{}", n), "type": "function", "function": call}]}}]}).to_string();
        let mut c = c;
        let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", resp.len(), resp).as_bytes());
    });
    port
}
