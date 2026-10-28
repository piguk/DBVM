//! The host side of the in-sandbox supervisor (init.c): start a sandbox around it and send it requests.
//! Shared by every long-lived sandbox: sessions over a materialized tree and rollouts over an overlay.

use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::sandbox::{self, Isolation, Outcome, Sandbox, Spec};

/// The supervisor binary, built static by build.rs.
const SUPERVISOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/session-init"));

/// Where the supervisor, its control directory and the egress proxy socket appear inside the sandbox.
/// Reserved: never synced into a database (see `vm::is_reserved_vm_path`).
pub const BIN_DIR: &str = "/.selfdb/bin";
pub const CTL_DIR: &str = "/.selfdb/ctl";
pub const NET_DIR: &str = "/.selfdb/net";
/// The loopback port the supervisor's forwarder listens on when egress is enabled.
pub const PROXY_PORT: u16 = 3128;

/// How long a sandbox may take to bring the supervisor up (gVisor starts in well under a second).
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long past a request's own timeout the host waits before declaring the supervisor unresponsive.
const GRACE: Duration = Duration::from_secs(10);
/// File operations are quick; this only bounds a hung one.
const FILE_OP_TIMEOUT: Duration = Duration::from_secs(60);

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry { pub name: String, pub kind: char, pub size: u64 }

pub struct Supervisor {
    ctl: PathBuf,
    log: PathBuf,
    sandbox: Sandbox,
    seq: u64,
}

impl Supervisor {
    /// Start `spec` (its root, limits and binds) with the supervisor as its command. `host_dir` receives the
    /// control directory, the supervisor binary and its log. With `egress`, the host directory holding the
    /// proxy socket `proxy.sock` is bound in and the supervisor forwards 127.0.0.1:PROXY_PORT to it.
    pub fn start(mut spec: Spec, isolation: Isolation, host_dir: &Path, egress: Option<&Path>) -> Result<Supervisor> {
        let (ctl, bin, log) = (host_dir.join("ctl"), host_dir.join("bin"), host_dir.join("supervisor.log"));
        std::fs::create_dir_all(&ctl)?;
        std::fs::create_dir_all(&bin)?;
        let init = bin.join("init");
        std::fs::write(&init, SUPERVISOR)?;
        std::fs::set_permissions(&init, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        let fifo = std::ffi::CString::new(ctl.join("fifo").to_string_lossy().as_bytes())?;
        if unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) } != 0 {
            return Err(anyhow!("mkfifo: {}", std::io::Error::last_os_error()));
        }
        spec.argv = vec![format!("{}/init", BIN_DIR), CTL_DIR.into()];
        spec.binds.push(sandbox::Bind { host: bin, dest: BIN_DIR.into(), writable: false });
        spec.binds.push(sandbox::Bind { host: ctl.clone(), dest: CTL_DIR.into(), writable: true });
        if let Some(net) = egress {
            spec.binds.push(sandbox::Bind { host: net.to_path_buf(), dest: NET_DIR.into(), writable: false });
            spec.argv.extend(["--forward".into(), PROXY_PORT.to_string(), format!("{}/proxy.sock", NET_DIR)]);
        }
        let logf = std::fs::File::create(&log)?;
        let sandbox = Sandbox::spawn(&spec, isolation, logf.try_clone()?.into(), logf.into())?;
        let mut sup = Supervisor { ctl, log, sandbox, seq: 0 };
        let started = Instant::now();
        while !sup.ctl.join("ready").exists() {
            if !sup.running() { return Err(anyhow!("the sandbox exited during start: {}", sup.log_tail())); }
            if started.elapsed() > START_TIMEOUT { return Err(anyhow!("the supervisor did not start: {}", sup.log_tail())); }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(sup)
    }

    pub fn running(&mut self) -> bool { self.sandbox.is_running() }

    /// CPU time of everything in the sandbox so far (see `Sandbox::cpu_usec`).
    pub fn cpu_usec(&mut self) -> Option<u64> { self.sandbox.cpu_usec() }

    /// Stop everything in the sandbox until the guard drops (see `Sandbox::freeze`).
    pub fn freeze(&self) -> Result<sandbox::Frozen> { self.sandbox.freeze() }

    pub fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        log.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ")
    }

    /// Write a request's files, signal it, and wait up to `wait` for its status. Returns the status line and
    /// the request directory (the caller reads the results and removes it).
    fn request(&mut self, op: &str, files: &[(&str, &[u8])], wait: Duration) -> Result<(String, PathBuf)> {
        if !self.running() { return Err(anyhow!("the sandbox is not running: {}", self.log_tail())); }
        self.seq += 1;
        let id = self.seq.to_string();
        let req = self.ctl.join(&id);
        std::fs::create_dir(&req)?;
        std::fs::write(req.join("op"), op)?;
        for (name, data) in files { std::fs::write(req.join(name), data)?; }
        {
            // A FIFO opened for writing blocks until it has a reader. The supervisor always holds it open,
            // so a non-blocking open fails at once instead of hanging when the supervisor is gone.
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(self.ctl.join("fifo"))
                .map_err(|e| anyhow!("signal the supervisor: {}", e))?;
            f.write_all(format!("{}\n", id).as_bytes())?;
        }
        let started = Instant::now();
        loop {
            if let Ok(s) = std::fs::read_to_string(req.join("status")) { return Ok((s.trim().to_string(), req)); }
            if !self.running() { return Err(anyhow!("the sandbox exited while handling a request: {}", self.log_tail())); }
            if started.elapsed() > wait + GRACE { return Err(anyhow!("the supervisor did not answer")); }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Run a command. `env` is its whole environment.
    pub fn exec(&mut self, argv: &[String], env: &[(String, String)], cwd: &str, timeout: Duration, max_output: usize) -> Result<Outcome> {
        if argv.is_empty() { return Err(anyhow!("empty command")); }
        let argv_b = nul_list(argv.iter().cloned());
        let env_b = nul_list(env.iter().map(|(k, v)| format!("{}={}", k, v)));
        let t = timeout.as_millis().to_string();
        let (status, req) = self.request("exec", &[("argv", &argv_b), ("env", &env_b), ("cwd", cwd.as_bytes()), ("timeout_ms", t.as_bytes())], timeout)?;
        let (exit_code, timed_out) = match status.as_str() {
            "timeout" => (sandbox::EXIT_TIMEOUT, true),
            s if s.starts_with("exit ") => (s[5..].parse().map_err(|_| anyhow!("bad status {:?}", s))?, false),
            s => { let _ = std::fs::remove_dir_all(&req); return Err(anyhow!("command failed: {}", s.strip_prefix("error ").unwrap_or(s))); }
        };
        let (stdout, stdout_truncated) = read_capped(&req.join("out"), max_output);
        let (stderr, stderr_truncated) = read_capped(&req.join("err"), max_output);
        let _ = std::fs::remove_dir_all(&req);
        Ok(Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated })
    }

    /// The outcome of a file operation: its output on success, the sandbox's error message otherwise.
    fn file_op(&mut self, op: &str, files: &[(&str, &[u8])]) -> Result<std::result::Result<Vec<u8>, String>> {
        let (status, req) = self.request(op, files, FILE_OP_TIMEOUT)?;
        let r = if status == "exit 0" {
            Ok(std::fs::read(req.join("out")).unwrap_or_default())
        } else {
            Err(std::fs::read_to_string(req.join("err")).unwrap_or_else(|_| status.clone()).trim().to_string())
        };
        let _ = std::fs::remove_dir_all(&req);
        Ok(r)
    }

    /// Up to `len` bytes of `path` from `offset`, read inside the sandbox.
    pub fn read(&mut self, path: &str, offset: u64, len: u64) -> Result<std::result::Result<Vec<u8>, String>> {
        self.file_op("read", &[("path", path.as_bytes()), ("offset", offset.to_string().as_bytes()), ("len", len.to_string().as_bytes())])
    }

    /// Write `data` to `path` inside the sandbox, creating parent directories.
    /// Write `path` (parent directories created). With `mode` the file gets exactly that mode; without, a new
    /// file is 0644 and an existing one keeps its mode.
    pub fn write(&mut self, path: &str, data: &[u8], mode: Option<u32>) -> Result<std::result::Result<(), String>> {
        let mode = match mode { Some(m) => format!("={:o}", m & 0o7777), None => "644".into() };
        Ok(self.file_op("write", &[("path", path.as_bytes()), ("mode", mode.as_bytes()), ("in", data)])?.map(|_| ()))
    }

    /// The entries of directory `path` inside the sandbox, sorted by name.
    pub fn list(&mut self, path: &str) -> Result<std::result::Result<Vec<DirEntry>, String>> {
        Ok(self.file_op("list", &[("path", path.as_bytes())])?.map(|out| {
            let mut v: Vec<DirEntry> = String::from_utf8_lossy(&out).lines().filter_map(|l| {
                let mut it = l.splitn(3, '\t');
                let kind = it.next()?.chars().next()?;
                let size = it.next()?.parse().ok()?;
                Some(DirEntry { name: it.next()?.to_string(), kind, size })
            }).collect();
            v.sort_by(|a, b| a.name.cmp(&b.name));
            v
        }))
    }
}

fn nul_list(items: impl Iterator<Item = String>) -> Vec<u8> {
    let mut v = Vec::new();
    for s in items { v.extend_from_slice(s.as_bytes()); v.push(0); }
    v
}

/// The first `cap` bytes of a file, and whether it held more.
fn read_capped(path: &Path, cap: usize) -> (Vec<u8>, bool) {
    let Ok(f) = std::fs::File::open(path) else { return (Vec::new(), false) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut out = Vec::new();
    let _ = f.take(cap as u64).read_to_end(&mut out);
    (out, len > cap as u64)
}
