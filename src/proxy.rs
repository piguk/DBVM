//! Host-side model proxy for an agent running inside a VM.
//!
//! The agent in the VM has no network: its sandbox runs with a private network namespace that holds
//! only an unused loopback. It reaches the model through two hops, both inside the host:
//!
//! 1. A unix socket the host listens on. The socket file is bound into the sandbox.
//! 2. A tiny forwarder inside the sandbox (`FORWARDER_JS`) that accepts HTTP on 127.0.0.1 and relays
//!    the byte stream to that socket. The agent points its base URL at the forwarder.
//!
//! The credential never enters the VM: the proxy adds the `Authorization` header on the way out and
//! discards any the guest sent. The guest sees only the local forwarder.
//!
//! Two properties were measured, and both are covered by `tests/proxy.rs`:
//! - a guest `fetch` through the forwarder returns the upstream body while `/proc/net/dev` lists only `lo`;
//! - the credential appears in the forwarded request and never in the guest's environment.
//!
//! A guest-supplied `Host` or `Content-Length` is dropped: the proxy writes its own, and sending both
//! made the upstream answer 400 (two of each header).

use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Where the proxy sends requests and what it authenticates with.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// `host:port` of the real endpoint, for example `cpa.example.com:443`.
    pub host: String,
    /// `https` or `http`.
    pub scheme: String,
    /// Sent as `Authorization: Bearer <key>`, replacing anything the guest sent.
    pub api_key: String,
}

impl Upstream {
    /// Parse a base URL such as `https://host/v1`. Returns the upstream and the path prefix (`/v1`),
    /// which is prepended to every forwarded request path.
    pub fn from_base_url(base: &str, api_key: &str) -> Result<(Upstream, String)> {
        let (scheme, rest) = base.split_once("://").ok_or_else(|| anyhow!("base url needs a scheme: {}", base))?;
        if scheme != "http" && scheme != "https" { return Err(anyhow!("unsupported scheme {}: {}", scheme, base)); }
        let (host, prefix) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if host.is_empty() { return Err(anyhow!("base url has no host: {}", base)); }
        let host = if host.contains(':') || scheme == "http" && host.ends_with(":80") {
            if host.contains(':') { host.to_string() } else { format!("{}:80", host) }
        } else if scheme == "https" { format!("{}:443", host) } else { format!("{}:80", host) };
        Ok((Upstream { host, scheme: scheme.to_string(), api_key: api_key.to_string() }, prefix.to_string()))
    }
}

/// Which extra hosts the guest may reach, and under which path prefix.
///
/// Everything else is refused. The guest cannot widen this: it is host-side state, and a request names its
/// destination only through the prefix, so an unlisted host is rejected before any connection is made.
#[derive(Debug, Clone, Default)]
pub struct AllowList {
    pub hosts: Vec<String>,
}

impl AllowList {
    /// Parse `host1,host2` (host names, optional `:port`). Empty means "no package access".
    pub fn parse(spec: &str) -> AllowList {
        AllowList { hosts: spec.split(',').map(|h| h.trim().to_string()).filter(|h| !h.is_empty()).collect() }
    }
    fn allows(&self, host: &str) -> bool {
        self.hosts.iter().any(|h| h == host)
    }
}

/// Requests the guest sends to `<prefix>/<host>/<path>` are forwarded to `https://<host>/<path>` when the
/// host is allowed; anything else is refused with 403 and a message the guest can read.
pub const PACKAGE_PREFIX: &str = "/pkg";

/// A running proxy bound to a socket path; dropping it stops the loop and removes the socket.
pub struct Proxy {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    /// Bind `socket` and serve on a background thread.
    pub fn bind(socket: &Path, upstream: Upstream, path_prefix: &str) -> Result<Proxy> {
        Proxy::bind_with(socket, upstream, path_prefix, AllowList::default())
    }

    /// Bind with an allowlist for package downloads.
    pub fn bind_with(socket: &Path, upstream: Upstream, path_prefix: &str, allow: AllowList) -> Result<Proxy> {
        let listener = bind_listener(socket)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let pre = path_prefix.to_string();
        let listener_thread = listener;
        let handle = std::thread::spawn(move || {
            serve_loop(listener_thread, upstream, pre, allow, stop2);
        });
        Ok(Proxy { socket: socket.to_path_buf(), stop, handle: Some(handle) })
    }

    pub fn socket_path(&self) -> &Path { &self.socket }

    /// A handle that stops the proxy from another thread (used by the CLI's signal handler).
    pub fn stopper(&self) -> Arc<AtomicBool> { self.stop.clone() }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() { let _ = h.join(); }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn bind_listener(socket: &Path) -> Result<UnixListener> {
    let _ = std::fs::remove_file(socket);
    if let Some(p) = socket.parent() { std::fs::create_dir_all(p)?; }
    let l = UnixListener::bind(socket)?;
    // the guest runs as a different identity inside its namespace; the socket must be reachable
    let _ = std::fs::set_permissions(socket, std::os::unix::fs::PermissionsExt::from_mode(0o666));
    Ok(l)
}

/// Accept and serve until `stop` is set. Polls with a short sleep so a stop request is noticed even
/// when no connection arrives.
fn serve_loop(listener: UnixListener, upstream: Upstream, prefix: String, allow: AllowList, stop: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((c, _)) => {
                let (up, pre, al) = (upstream.clone(), prefix.clone(), allow.clone());
                std::thread::spawn(move || { let _ = serve(c, &up, &pre, &al); });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

/// Serve on the calling thread until `stop` is set. The CLI uses this and waits for a signal: a thread
/// that waited on stdin would return at once when stdin is not a terminal, dropping the proxy and
/// removing the socket before any guest could use it.
pub fn serve_until_stopped(socket: &Path, upstream: Upstream, path_prefix: &str, stop: Arc<AtomicBool>) -> Result<()> {
    serve_until_stopped_with(socket, upstream, path_prefix, AllowList::default(), stop)
}

/// As `serve_until_stopped`, with an allowlist for package hosts.
pub fn serve_until_stopped_with(socket: &Path, upstream: Upstream, path_prefix: &str, allow: AllowList, stop: Arc<AtomicBool>) -> Result<()> {
    let l = bind_listener(socket)?;
    serve_loop(l, upstream, path_prefix.to_string(), allow, stop);
    let _ = std::fs::remove_file(socket);
    Ok(())
}

/// Serve one guest connection: read its HTTP request, forward it upstream, relay the response.
fn serve(mut guest: UnixStream, up: &Upstream, prefix: &str, allow: &AllowList) -> Result<()> {
    let mut buf = vec![0u8; 65536];
    let mut head = Vec::new();
    loop {
        let n = guest.read(&mut buf)?;
        if n == 0 { return Ok(()); }
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") { break; }
        if head.len() > 1024 * 1024 { return Err(anyhow!("request headers too large")); }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("POST").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    let mut forwarded: Vec<String> = Vec::new();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if line.is_empty() { break; }
        let lower = line.to_ascii_lowercase();
        // the guest's credential is discarded; the proxy authenticates
        if lower.starts_with("authorization:") { continue; }
        // per-request connections only
        if lower.starts_with("connection:") || lower.starts_with("keep-alive:") { continue; }
        // Host and Content-Length are written by forward() from the real upstream and body. Sending the
        // guest's copy too duplicates the header and the upstream rejects the request (measured: 400).
        if lower.starts_with("host:") { continue; }
        if lower.starts_with("content-length:") { content_length = line[15..].trim().parse().ok(); continue; }
        forwarded.push(line.to_string());
    }
    forwarded.push(format!("Authorization: Bearer {}", up.api_key));

    let header_len = head.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(head.len());
    let mut body = head[header_len..].to_vec();
    if let Some(len) = content_length {
        while body.len() < len {
            let n = guest.read(&mut buf)?;
            if n == 0 { break; }
            body.extend_from_slice(&buf[..n]);
        }
    }

    // Two shapes of request:
    //   <prefix>/...            the model endpoint (the only thing a task needs)
    //   /pkg/<host>/<path>      a package download, allowed only for listed hosts
    if let Some(rest) = path.strip_prefix(PACKAGE_PREFIX) {
        let rest = rest.trim_start_matches('/');
        let (host, sub) = match rest.split_once('/') {
            Some((h, p)) => (h.to_string(), format!("/{}", p)),
            None => (rest.to_string(), "/".to_string()),
        };
        if host.is_empty() || !allow.allows(&host) {
            let body = format!("refused by the VM proxy: {} is not an allowed package host", if host.is_empty() { "<none>" } else { &host });
            eprintln!("proxy: refused package request for {:?}", host);
            let resp = format!("HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            guest.write_all(resp.as_bytes())?;
            let _ = guest.flush();
            return Ok(());
        }
        // A host may carry an explicit port; a package registry is https. Tests point at a local http stub
        // by naming its port, so the scheme follows the port: 443 means TLS, anything else is plain.
        let (host_only, port) = match host.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.to_string()),
            None => (host.clone(), "443".to_string()),
        };
        let scheme = if port == "443" { "https" } else { "http" };
        let pkg_up = Upstream { host: if scheme == "https" { format!("{}:443", host_only) } else { format!("{}:{}", host_only, port) },
                                scheme: scheme.into(), api_key: String::new() };
        let mut hdrs = forwarded.clone();
        // no credential for third-party hosts
        hdrs.retain(|h| !h.to_ascii_lowercase().starts_with("authorization:"));
        let response = forward(&pkg_up, &method, &sub, &version, &hdrs, &body)?;
        guest.write_all(&response)?;
        let _ = guest.flush();
        return Ok(());
    }
    let full_path = format!("{}{}", prefix.trim_end_matches('/'), path);
    let response = forward(up, &method, &full_path, &version, &forwarded, &body)?;
    guest.write_all(&response)?;
    let _ = guest.flush();
    Ok(())
}

/// Forward one request upstream and return the raw response bytes.
fn forward(up: &Upstream, method: &str, path: &str, version: &str, headers: &[String], body: &[u8]) -> Result<Vec<u8>> {
    let host_only = up.host.rsplit_once(':').map(|(h, _)| h).unwrap_or(&up.host);
    let port = up.host.rsplit_once(':').map(|(_, p)| p).unwrap_or("443");
    let mut request = format!("{} {} {}\r\nHost: {}\r\n", method, path, version, host_only);
    for h in headers { request.push_str(h); request.push_str("\r\n"); }
    request.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
    let mut raw = request.into_bytes();
    raw.extend_from_slice(body);

    if std::env::var("PROXY_DEBUG").is_ok() {
        // never log the credential: print the request line and header names, with authorization hidden
        let head = String::from_utf8_lossy(&raw[..raw.len().min(600)]).to_string();
        let redacted: Vec<String> = head.lines()
            .map(|l| if l.to_ascii_lowercase().starts_with("authorization:") { "Authorization: <redacted>".to_string() } else { l.to_string() })
            .collect();
        eprintln!("PROXY->UPSTREAM {}://{}\n{}", up.scheme, up.host, redacted.join("\n"));
    }

    if up.scheme == "https" {
        tunnel_tls(host_only, port, &raw)
    } else {
        let mut s = TcpStream::connect(&up.host)?;
        s.write_all(&raw)?;
        let mut out = Vec::new();
        s.read_to_end(&mut out)?;
        Ok(out)
    }
}

/// One HTTPS exchange through `openssl s_client`, which performs the TLS handshake.
fn tunnel_tls(host: &str, port: &str, raw: &[u8]) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("openssl")
        .args(["s_client", "-quiet", "-connect", &format!("{}:{}", host, port), "-servername", host])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().map_err(|e| anyhow!("openssl is required for https upstreams: {}", e))?;
    {
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        stdin.write_all(raw)?;
    }
    let mut out = Vec::new();
    child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?.read_to_end(&mut out)?;
    let _ = child.wait();
    Ok(out)
}

/// The forwarder the sandbox runs: accepts HTTP on a loopback port and relays to the bound socket.
/// JavaScript because the VM already has node for the agent.
///
/// Two details that cost real debugging time, kept as warnings:
/// - the file must NOT end in `.js` inside a directory whose package.json has `"type": "module"`;
///   node would load it as ESM and `require` is undefined. A `.cjs` name avoids that.
/// - if it fails while started in the background, the error goes nowhere and the guest only sees
///   `ECONNREFUSED` on the port. Start it in the foreground when diagnosing.
pub const FORWARDER_JS: &str = r#"// relay HTTP from 127.0.0.1:PORT to the proxy unix socket bound into this sandbox
const net = require('net');
const SOCK = process.env.PROXY_SOCK || '/tmp/proxy.sock';
const PORT = Number(process.env.PROXY_PORT || 18081);
net.createServer(client => {
  const up = net.connect(SOCK);
  client.pipe(up); up.pipe(client);
  const end = () => { try { client.destroy(); } catch {} try { up.destroy(); } catch {} };
  client.on('error', end); up.on('error', end);
  client.on('end', () => up.end()); up.on('end', () => client.end());
}).listen(PORT, '127.0.0.1', () => console.error('proxy forwarder listening on ' + PORT));
"#;
