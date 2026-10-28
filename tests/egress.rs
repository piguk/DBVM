//! A build sandbox reaches listed hosts through the egress proxy and nothing else. The "internet" here is a
//! plain HTTP server on the host, so the test needs no network.
use selfdb::egress::{EgressPolicy, EgressProxy};
use selfdb::rollout::{Rollout, RolloutOptions};
use selfdb::vm;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// A one-page HTTP server on 127.0.0.1; returns its port.
fn http_server(body: &'static str) -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut c = c;
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf);
            let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes());
        }
    });
    port
}

struct Fixture { dir: PathBuf, layer: PathBuf }
impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.dir); } }

fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("egress-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let c = vm::init_vm_db_with_opts(dir.join("v.db").to_str().unwrap(), true, true).unwrap();
    for prog in ["/bin/sh", "/usr/bin/curl"] {
        if Path::new(prog).exists() { vm::vm_import_closure(&c, prog, "/").unwrap(); }
    }
    let layer = dir.join("layer");
    vm::vm_materialize_tree(&c, &layer).unwrap();
    Fixture { dir, layer }
}

fn curl(r: &mut Rollout, args: &str) -> (i32, String) {
    let o = r.exec(&["/bin/sh".into(), "-c".into(), format!("/usr/bin/curl -s -S --max-time 10 {}", args)], "/", Duration::from_secs(30), 1 << 16).unwrap().outcome;
    (o.exit_code, format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

#[test]
fn a_build_sandbox_reaches_listed_hosts_only() {
    if !bwrap_ok() || !Path::new("/usr/bin/curl").exists() { return; }
    let f = fixture("allow");
    let (allowed_port, other_port) = (http_server("HELLO"), http_server("SECRET"));
    let net = f.dir.join("net");
    let proxy = EgressProxy::start(&net, EgressPolicy { allow: vec![format!("127.0.0.1:{}", allowed_port)] }).unwrap();
    let mut r = Rollout::start(std::slice::from_ref(&f.layer), &RolloutOptions { egress: Some(net.clone()), ..RolloutOptions::default() }).unwrap();
    // plain HTTP through the proxy (absolute URI) and through a CONNECT tunnel (-p)
    let (code, out) = curl(&mut r, &format!("http://127.0.0.1:{}/", allowed_port));
    assert_eq!((code, out.as_str()), (0, "HELLO"), "allowed host over HTTP");
    let (code, out) = curl(&mut r, &format!("-p http://127.0.0.1:{}/", allowed_port));
    assert_eq!((code, out.as_str()), (0, "HELLO"), "allowed host over a CONNECT tunnel");
    let (_, out) = curl(&mut r, &format!("http://127.0.0.1:{}/", other_port));
    assert!(out.contains("refused by the sandbox egress proxy") && !out.contains("SECRET"), "an unlisted port is refused: {}", out);
    let (code, out) = curl(&mut r, &format!("-p http://127.0.0.1:{}/", other_port));
    assert!(code != 0 && !out.contains("SECRET"), "an unlisted tunnel is refused: {}", out);
    let ev = proxy.events();
    assert!(ev.iter().any(|e| e.port == other_port && !e.allowed), "refusals are recorded: {:?}", ev);
    assert!(ev.iter().filter(|e| e.port == allowed_port).all(|e| e.allowed));
}

#[test]
fn a_sandbox_without_egress_has_no_network() {
    if !bwrap_ok() || !Path::new("/usr/bin/curl").exists() { return; }
    let f = fixture("none");
    let port = http_server("HELLO");
    let mut r = Rollout::start(std::slice::from_ref(&f.layer), &RolloutOptions::default()).unwrap();
    let (code, out) = curl(&mut r, &format!("http://127.0.0.1:{}/", port));
    assert!(code != 0 && !out.contains("HELLO"), "no route out: {}", out);
}

#[test]
fn allow_list_entries_match_hosts_ports_and_domains() {
    let p = EgressPolicy { allow: vec!["pypi.org".into(), ".pythonhosted.org".into(), "10.0.0.5:8080".into()] };
    assert!(p.allows("pypi.org", 443) && p.allows("PyPI.org", 80));
    assert!(!p.allows("pypi.org", 22), "a bare host allows 80 and 443 only");
    assert!(p.allows("files.pythonhosted.org", 443) && p.allows("pythonhosted.org", 443));
    assert!(!p.allows("evilpythonhosted.org", 443), "a domain entry matches whole labels");
    assert!(p.allows("10.0.0.5", 8080) && !p.allows("10.0.0.5", 443));
    assert!(!p.allows("example.com", 443));
}
