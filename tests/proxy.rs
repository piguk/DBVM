//! The host-side proxy that lets a network-isolated agent reach the model.
//!
//! These tests run the real proxy and a real forwarder inside a `bwrap` sandbox with `--unshare-net`
//! (only an unused loopback). Two things must hold: the guest can get a response through the proxy, and
//! the credential stays on the host side of the boundary.
use selfdb::proxy::{serve_until_stopped, Upstream, FORWARDER_JS};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

fn node() -> Option<PathBuf> {
    let out = std::process::Command::new("sh").args(["-c", "readlink -f \"$(command -v node)\""]).output().ok()?;
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if p.exists() { Some(p) } else { None }
}

/// A stub upstream that speaks plain HTTP, so the test needs no credentials or real endpoint.
struct Stub { stop: Arc<AtomicBool>, handle: Option<std::thread::JoinHandle<()>>, addr: String }

impl Stub {
    fn start(token: &str) -> Stub {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, tok) = (stop.clone(), token.to_string());
        let handle = std::thread::spawn(move || {
            l.set_nonblocking(true).unwrap();
            while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
                match l.accept() {
                    Ok((mut s, _)) => {
                        let mut buf = [0u8; 8192];
                        let n = s.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        // echo back whether the proxy stamped the expected credential
                        let authed = req.to_ascii_lowercase().contains(&format!("authorization: bearer {}", tok).to_ascii_lowercase());
                        // Count the headers a real upstream rejects when duplicated. The Cloudflare front end
                        // in front of the real endpoint answers 400 for two Host headers (measured), so the
                        // stub refuses them the same way; otherwise a proxy that forwards the guest's Host
                        // would still look correct here.
                        let hosts = req.lines().filter(|l| l.to_ascii_lowercase().starts_with("host:")).count();
                        let lens = req.lines().filter(|l| l.to_ascii_lowercase().starts_with("content-length:")).count();
                        let body;
                        let status;
                        if hosts != 1 || lens != 1 {
                            status = "400 Bad Request";
                            body = format!("{{\"dup\":true,\"host\":{},\"content_length\":{}}}", hosts, lens);
                        } else if authed {
                            status = "200 OK";
                            body = r#"{"via":"stub","authed":true}"#.to_string();
                        } else {
                            status = "200 OK";
                            body = r#"{"authed":false}"#.to_string();
                        }
                        let resp = format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", status, body.len(), body);
                        let _ = s.write_all(resp.as_bytes());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        });
        Stub { stop, handle: Some(handle), addr }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.take() { let _ = h.join(); }
    }
}

#[test]
fn the_guest_reaches_the_upstream_and_never_sees_the_key() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let Some(node) = node() else { eprintln!("skip: node not available"); return; };
    let secret = "test-secret-value-not-for-anyone";
    let stub = Stub::start(secret);
    let d = std::env::temp_dir().join(format!("proxy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let sock = d.join("proxy.sock");
    let fwd = d.join("fwd.js");
    std::fs::write(&fwd, FORWARDER_JS).unwrap();

    // the proxy talks to the stub over plain HTTP
    let (up, prefix) = Upstream::from_base_url(&format!("http://{}", stub.addr), secret).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let (sock2, pre) = (sock.clone(), prefix.clone());
    let h = std::thread::spawn(move || { let _ = serve_until_stopped(&sock2, up, &pre, stop2); });
    for _ in 0..50 { if sock.exists() { break; } std::thread::sleep(std::time::Duration::from_millis(50)); }
    assert!(sock.exists(), "proxy socket must exist");

    // A fixed port here made the suite flaky: another test could hold 18081 at the same moment, since
    // cargo runs test binaries in parallel. Bind a free port on the host to learn a number that is free,
    // then use it inside the sandbox.
    let free_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let script = format!(r#"
fetch('http://127.0.0.1:{p}/v1/chat/completions', {{method:'POST',headers:{{'content-type':'application/json'}},body:'{{}}'}})
  .then(r=>r.text()).then(t=>{{console.log('BODY:'+t);process.exit(0)}})
  .catch(e=>{{console.log('FAIL:'+e.message);process.exit(1)}});
"#, p = free_port);
    let ask = d.join("ask.js");
    std::fs::write(&ask, script).unwrap();
    // the socket path inside the sandbox is unique per test as well
    let guest_sock = format!("/tmp/proxy-{}.sock", std::process::id());
    // env -i: nothing from this process is inherited, so a key could only come from the sandbox itself
    let out = std::process::Command::new("env")
        .args(["-i", "PATH=/usr/bin:/bin", "HOME=/tmp"])
        .arg("bwrap")
        .args(["--bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--unshare-pid", "--unshare-net",
               "--die-with-parent", "--bind", sock.to_str().unwrap(), guest_sock.as_str()])
        .arg("/bin/sh").arg("-c").arg(format!(
            "echo IFACES:$(tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | tr '\\n' ' '); \
             PROXY_SOCK={gsock} PROXY_PORT={port} {node} {fwd} >/dev/null 2>&1 & sleep 2; \
             {node} {ask}; \
             grep -c sk- /proc/self/environ 2>/dev/null > /tmp/keycount-{pid}.txt || true",
            gsock = guest_sock, port = free_port,
            node = node.display(), fwd = fwd.display(), ask = ask.display(), pid = std::process::id()))
        .output().expect("bwrap");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("BODY:"), "guest must get a response: {}", text);
    assert!(text.contains(r#""authed":true"#), "the proxy must stamp the credential: {}", text);
    assert!(!text.contains(secret), "the credential must never reach the guest: {}", text);
    // the sandbox saw only loopback
    assert!(text.contains("IFACES:lo"), "sandbox must have only loopback: {}", text);
    let ifaces = text.split("IFACES:").nth(1).unwrap_or("").lines().next().unwrap_or("").trim().to_string();
    assert_eq!(ifaces, "lo", "the sandbox must have no other interface, got {:?}", ifaces);

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = h.join();
    assert!(!sock.exists(), "the socket is removed when the proxy stops");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn base_url_parsing_keeps_the_scheme_host_and_path_prefix() {
    let (up, prefix) = Upstream::from_base_url("https://example.com/v1", "k").unwrap();
    assert_eq!(up.scheme, "https");
    assert_eq!(up.host, "example.com:443");
    assert_eq!(prefix, "/v1");
    assert_eq!(up.api_key, "k");
    let (up2, prefix2) = Upstream::from_base_url("http://127.0.0.1:8080", "k2").unwrap();
    assert_eq!(up2.host, "127.0.0.1:8080");
    assert_eq!(prefix2, "");
    assert!(Upstream::from_base_url("example.com/v1", "k").is_err(), "a scheme is required");
}

#[test]
fn the_guest_host_and_content_length_do_not_duplicate_the_upstream_ones() {
    // A forwarded request carrying the guest's own Host and Content-Length once made the upstream answer
    // 400 (two of each header). The proxy must send exactly one of each.
    let secret = "k";
    let stub = Stub::start(secret);
    let d = std::env::temp_dir().join(format!("proxyhdr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let sock = d.join("p.sock");
    let (up, prefix) = Upstream::from_base_url(&format!("http://{}", stub.addr), secret).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, pre) = (sock.clone(), prefix.clone());
    let stopper = stop.clone();
    let h = std::thread::spawn(move || { let _ = serve_until_stopped(&s2, up, &pre, stopper); });
    for _ in 0..50 { if sock.exists() { break; } std::thread::sleep(std::time::Duration::from_millis(50)); }

    use std::io::{Read, Write};
    let mut c = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    // a guest that sends its own Host and Content-Length, as curl does
    c.write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\ncontent-length: 2\r\n\r\n{}").unwrap();
    let mut resp = Vec::new();
    let _ = c.read_to_end(&mut resp);
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains("200 OK"), "upstream must accept the forwarded request: {}", text);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = h.join();
    let _ = std::fs::remove_dir_all(&d);
}

/// Package downloads through the proxy: only allowlisted hosts, and the guest sees a refusal it can read.
#[test]
fn only_allowlisted_hosts_are_reachable_through_the_package_prefix() {
    use selfdb::proxy::{AllowList, PACKAGE_PREFIX};
    let secret = "model-key";
    let stub = Stub::start(secret);
    let d = std::env::temp_dir().join(format!("pkgproxy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let sock = d.join("p.sock");
    // the "package host" is the same local stub: http, reached through the model upstream only by prefix
    let (up, pre) = Upstream::from_base_url(&format!("http://{}", stub.addr), secret).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    // allow the stub's host:port by name so the allowlist logic is what decides
    let allow = AllowList { hosts: vec![stub.addr.clone()] };
    let (s2, pre2, stop2) = (sock.clone(), pre.clone(), stop.clone());
    let h = std::thread::spawn(move || { let _ = selfdb::proxy::serve_until_stopped_with(&s2, up, &pre2, allow, stop2); });
    for _ in 0..50 { if sock.exists() { break; } std::thread::sleep(std::time::Duration::from_millis(50)); }

    use std::io::{Read, Write};
    let mut ask = |host_port: &str| -> String {
        let mut c = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        let req = format!("GET {}/{}/some/package HTTP/1.1\r\nHost: local\r\n\r\n", PACKAGE_PREFIX, host_port);
        c.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out);
        String::from_utf8_lossy(&out).to_string()
    };
    // an allowed host goes through (the stub answers 200)
    let ok = ask(&stub.addr);
    assert!(ok.contains("200 OK"), "an allowlisted host is reachable: {}", ok);
    // an unlisted host is refused, with a readable reason
    let denied = ask("not-allowed.example:443");
    assert!(denied.contains("403 Forbidden"), "an unlisted host is refused: {}", denied);
    assert!(denied.contains("not an allowed package host"), "the refusal explains itself: {}", denied);
    // the credential is never attached to package requests
    assert!(!denied.contains(secret) && !ok.contains(secret), "no credential in package traffic");
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = h.join();
    let _ = std::fs::remove_dir_all(&d);
}
