//! The egress proxy for sandboxes that may reach a few hosts (a build session installing packages).
//!
//! A sandbox has no network namespace of its own beyond loopback. Its only way out is this proxy: the
//! supervisor forwards 127.0.0.1:PROXY_PORT inside the sandbox to the proxy's unix socket, and the sandbox's
//! environment names it as HTTP_PROXY / HTTPS_PROXY, which pip, npm, git and curl follow. The proxy allows
//! `CONNECT host:port` tunnels (HTTPS) and absolute-URI requests (plain HTTP) to listed hosts only, and
//! records every request, allowed or refused.
//!
//! Allow-list entries: `host` (ports 80 and 443), `host:port` (that port), `.domain` (the domain and every
//! name below it, ports 80 and 443).

use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct EgressPolicy { pub allow: Vec<String> }

impl EgressPolicy {
    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = host.to_ascii_lowercase();
        self.allow.iter().any(|e| {
            let e = e.to_ascii_lowercase();
            if let Some(domain) = e.strip_prefix('.') {
                (host == domain || host.ends_with(&format!(".{}", domain))) && (port == 80 || port == 443)
            } else if let Some((h, p)) = e.rsplit_once(':') {
                h == host && p.parse::<u16>().ok() == Some(port)
            } else {
                e == host && (port == 80 || port == 443)
            }
        })
    }
}

/// One request through the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressEvent { pub host: String, pub port: u16, pub allowed: bool }

/// A running proxy on `<dir>/proxy.sock`. Dropping it stops accepting and removes the socket.
pub struct EgressProxy {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    events: Arc<Mutex<Vec<EgressEvent>>>,
}

impl EgressProxy {
    pub fn start(dir: &Path, policy: EgressPolicy) -> Result<EgressProxy> {
        std::fs::create_dir_all(dir)?;
        let socket = dir.join("proxy.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        // the sandbox's identity differs inside its user namespace; the socket must be reachable
        std::fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let events = Arc::new(Mutex::new(Vec::new()));
        let (stop2, events2, policy) = (stop.clone(), events.clone(), Arc::new(policy));
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((c, _)) => {
                        let (p, ev) = (policy.clone(), events2.clone());
                        std::thread::spawn(move || { let _ = serve(c, &p, &ev); });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        });
        Ok(EgressProxy { socket, stop, handle: Some(handle), events })
    }

    /// Every request so far, in order.
    pub fn events(&self) -> Vec<EgressEvent> { self.events.lock().unwrap().clone() }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() { let _ = h.join(); }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn refuse(mut c: UnixStream, why: &str) -> Result<()> {
    let body = format!("refused by the sandbox egress proxy: {}\n", why);
    c.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes())?;
    Ok(())
}

fn serve(mut c: UnixStream, policy: &EgressPolicy, events: &Mutex<Vec<EgressEvent>>) -> Result<()> {
    // the request head; whatever arrives after it (a request body, TLS bytes) is relayed as it is
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = c.read(&mut chunk)?;
        if n == 0 { return Ok(()); }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") { break i + 4; }
        if buf.len() > 64 * 1024 { return refuse(c, "request head too large"); }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split(' ');
    let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("HTTP/1.1"));

    let (host, port, forward_head) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = target.rsplit_once(':').ok_or_else(|| anyhow!("CONNECT without a port"))?;
        (h.trim_matches(|ch| ch == '[' || ch == ']').to_string(), p.parse::<u16>()?, None)
    } else {
        // an absolute URI: http://host[:port]/path
        let Some(rest) = target.strip_prefix("http://") else { return refuse(c, "only http:// requests and CONNECT tunnels are proxied") };
        let (authority, path) = match rest.find('/') { Some(i) => (&rest[..i], &rest[i..]), None => (rest, "/") };
        let (h, p) = match authority.rsplit_once(':') { Some((h, p)) => (h.to_string(), p.parse::<u16>()?), None => (authority.to_string(), 80) };
        // the origin form, without the proxy-only headers
        let mut out = format!("{} {} {}\r\n", method, path, version);
        for l in lines { if l.is_empty() { break; } let lower = l.to_ascii_lowercase(); if lower.starts_with("proxy-") { continue; } out.push_str(l); out.push_str("\r\n"); }
        out.push_str("\r\n");
        (h, p, Some(out))
    };
    let allowed = policy.allows(&host, port);
    events.lock().unwrap().push(EgressEvent { host: host.clone(), port, allowed });
    if !allowed { return refuse(c, &format!("{}:{} is not on the allow list", host, port)); }

    let addr = (host.as_str(), port).to_socket_addrs()?.next().ok_or_else(|| anyhow!("{} does not resolve", host))?;
    let mut up = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    match forward_head {
        None => c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?,
        Some(h) => up.write_all(h.as_bytes())?,
    }
    up.write_all(&buf[head_end..])?;
    // relay both directions until each side closes
    let (mut c2, mut up2) = (c.try_clone()?, up.try_clone()?);
    let t = std::thread::spawn(move || { let _ = std::io::copy(&mut up2, &mut c2); let _ = c2.shutdown(Shutdown::Write); });
    let _ = std::io::copy(&mut c, &mut up);
    let _ = up.shutdown(Shutdown::Write);
    let _ = t.join();
    Ok(())
}
