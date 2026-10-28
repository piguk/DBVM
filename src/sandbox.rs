//! Process isolation for VM content.
//!
//! Every path that runs code from a VM (`vm-step`, `vm-agent`, `vm-chroot`, `vm-exec`) builds its sandbox
//! here, so isolation flags, the syscall filter, resource limits and the environment cannot differ
//! between paths. Before this module they did: `vm-chroot` had no network namespace, passed the host
//! environment (credentials included) into the sandbox and fell back to an unisolated `chroot` without
//! bwrap, and `vm-exec` ran VM binaries directly on the host.
//!
//! Two independent layers:
//! - isolation: bwrap (namespaces and a seccomp filter on the shared host kernel) or gVisor (`runsc`, a
//!   user-space kernel between the sandbox and the host kernel);
//! - resources: one cgroup v2 per sandbox (memory, pids, cpu) and per-process rlimits.
//!
//! A sandbox never has network access, and nothing from the host environment reaches it: the spec's
//! environment is the complete environment.

use anyhow::{anyhow, Result};
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The isolation boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Linux namespaces plus a seccomp filter. The sandbox shares the host kernel: a kernel bug
    /// reachable through the allowed syscalls crosses the boundary.
    Bwrap,
    /// gVisor: the sandbox's syscalls are served by a user-space kernel, and only a small, filtered set
    /// reaches the host kernel.
    Gvisor,
}

impl Isolation {
    pub fn parse(s: &str) -> Result<Isolation> {
        match s {
            "bwrap" => Ok(Isolation::Bwrap),
            "gvisor" | "runsc" => Ok(Isolation::Gvisor),
            other => Err(anyhow!("unknown isolation {}: expected bwrap or gvisor", other)),
        }
    }

    /// `$SELF_SANDBOX_ISOLATION`, or bwrap.
    pub fn from_env() -> Result<Isolation> {
        match std::env::var("SELF_SANDBOX_ISOLATION") {
            Ok(v) if !v.is_empty() => Isolation::parse(&v),
            _ => Ok(Isolation::Bwrap),
        }
    }
}

/// Resource limits for one sandbox. 0 disables a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// memory.max of the sandbox's cgroup, in MiB, for all its processes together. Swap is disabled, so
    /// this is a hard limit: past it the kernel's OOM killer ends a process in the sandbox.
    pub memory_mb: u64,
    /// pids.max: processes and threads in the sandbox. Unlike RLIMIT_NPROC it counts this sandbox only,
    /// not every process of the host user.
    pub pids: u64,
    /// cpu.max quota in percent of one CPU (200 = two CPUs).
    pub cpu_percent: u64,
    /// RLIMIT_AS per process, in MiB. A soft guard where an allocation should fail visibly instead of
    /// being OOM-killed: node turns it into a catchable RangeError. Node needs at least ~1536 to start.
    pub address_space_mb: u64,
    /// RLIMIT_CPU per process, in seconds of CPU time.
    pub cpu_secs: u64,
    /// RLIMIT_FSIZE per process, in MiB: the largest file a process may write.
    pub file_size_mb: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { memory_mb: 2048, pids: 1024, cpu_percent: 200, address_space_mb: 0, cpu_secs: 0, file_size_mb: 1024 }
    }
}

impl Limits {
    /// No limits at all. Only for trusted code: a sandbox without a cgroup can exhaust the host.
    pub fn none() -> Limits {
        Limits { memory_mb: 0, pids: 0, cpu_percent: 0, address_space_mb: 0, cpu_secs: 0, file_size_mb: 0 }
    }

    fn needs_cgroup(&self) -> bool { self.memory_mb > 0 || self.pids > 0 || self.cpu_percent > 0 }
}

/// A host path made visible inside the sandbox.
#[derive(Debug, Clone)]
pub struct Bind {
    pub host: PathBuf,
    /// Absolute path inside the sandbox.
    pub dest: String,
    pub writable: bool,
}

/// A read-only overlay of `layers` (bottom first) mounted at `dest`.
#[derive(Debug, Clone)]
pub struct RoOverlay { pub layers: Vec<PathBuf>, pub dest: String }

/// What to run and where.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The sandbox's `/`, when it is a plain directory (`layers` is empty).
    pub root: PathBuf,
    /// The sandbox's `/` as an overlay instead: these read-only layers (bottom first) under `upper`, which
    /// receives every write (`work` is overlayfs's scratch directory, on the same filesystem). Many
    /// sandboxes share the layers, so starting one costs nothing in proportion to the tree. bwrap only.
    pub layers: Vec<PathBuf>,
    pub upper: Option<(PathBuf, PathBuf)>,
    /// Read-only overlays mounted over the root, for paths the sandbox must not change.
    pub ro_overlays: Vec<RoOverlay>,
    pub root_writable: bool,
    /// The root may be changed from outside while the sandbox runs (a session restores it after a
    /// rollback). gVisor otherwise assumes exclusive access and keeps serving its cached view (measured:
    /// a file removed on the host was still visible inside).
    pub root_shared: bool,
    pub argv: Vec<String>,
    /// The complete environment of the sandboxed process. Nothing from the host is added.
    pub env: Vec<(String, String)>,
    pub binds: Vec<Bind>,
    pub cwd: String,
    pub limits: Limits,
    /// Keep the caller's terminal (an interactive shell). Otherwise stdin is /dev/null and the process
    /// runs in a new session, so it cannot push input into the caller's terminal (TIOCSTI).
    pub interactive: bool,
}

impl Spec {
    pub fn new(root: &Path, argv: Vec<String>) -> Spec {
        Spec {
            root: root.to_path_buf(),
            layers: Vec::new(),
            upper: None,
            ro_overlays: Vec::new(),
            root_writable: true,
            root_shared: false,
            argv,
            env: vec![("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()), ("HOME".into(), "/root".into())],
            binds: Vec::new(),
            cwd: "/".into(),
            limits: Limits::default(),
            interactive: false,
        }
    }

    pub fn env(mut self, key: &str, value: &str) -> Spec {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.to_string(), value.to_string()));
        self
    }
}

/// Result of one captured run.
#[derive(Debug, Default)]
pub struct Outcome {
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Exit code recorded for a run stopped by its time limit (the `timeout` command's convention).
pub const EXIT_TIMEOUT: i32 = 124;

/// Run `spec` to completion with stdout and stderr captured, each capped at `max_output` bytes (the
/// rest is read and dropped, so the process never blocks on a full pipe). The sandbox is killed after
/// `timeout`.
pub fn run_captured(spec: &Spec, isolation: Isolation, timeout: Duration, max_output: usize) -> Result<Outcome> {
    let mut sb = Sandbox::spawn(spec, isolation, Stdio::piped(), Stdio::piped())?;
    let mut out_pipe = sb.child.stdout.take().ok_or_else(|| anyhow!("no stdout pipe"))?;
    let mut err_pipe = sb.child.stderr.take().ok_or_else(|| anyhow!("no stderr pipe"))?;
    let out_t = std::thread::spawn(move || drain_capped(&mut out_pipe, max_output));
    let err_t = std::thread::spawn(move || drain_capped(&mut err_pipe, max_output));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = sb.child.try_wait()? { break st; }
        if Instant::now() >= deadline {
            timed_out = true;
            sb.kill_all();
            break sb.child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Anything the sandbox left running is stopped before its output is collected: a background process
    // holding the pipes open would otherwise keep the readers waiting.
    sb.kill_all();
    let (stdout, stdout_truncated) = out_t.join().unwrap_or_default();
    let (stderr, stderr_truncated) = err_t.join().unwrap_or_default();
    let exit_code = if timed_out { EXIT_TIMEOUT } else { exit_code(status) };
    Ok(Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated })
}

/// Run `spec` attached to the caller's terminal and return its exit code.
pub fn run_interactive(spec: &Spec, isolation: Isolation) -> Result<i32> {
    let mut sb = Sandbox::spawn(spec, isolation, Stdio::inherit(), Stdio::inherit())?;
    let status = sb.child.wait()?;
    sb.kill_all();
    Ok(exit_code(status))
}

fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Read a pipe to EOF, keeping the first `cap` bytes. Returns (kept bytes, truncated).
fn drain_capped<R: Read>(mut r: R, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                if room > 0 { kept.extend_from_slice(&buf[..n.min(room)]); }
                if n > room { truncated = true; }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (kept, truncated)
}

/// A running sandbox. Dropping it kills everything left in it and removes its cgroup.
pub struct Sandbox {
    child: Child,
    cgroup: Option<SandboxCgroup>,
    /// gVisor keeps per-container state in this directory; it is removed with the sandbox.
    _runsc_state: Option<crate::vm::WorkDir>,
    _keep: Vec<OwnedFd>,
}

/// Where a sandbox's cgroup lives and how to stop everything in it.
enum SandboxCgroup {
    /// A directory created under a delegated cgroup; removed after the sandbox ends.
    Dir(PathBuf),
    /// A transient systemd scope; systemd removes it once it is empty (`--collect`).
    Unit(String),
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_name(prefix: &str) -> String {
    format!("{}-{}-{}", prefix, std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

impl Sandbox {
    /// Start `spec`. A non-interactive sandbox gets /dev/null as stdin and the given stdout and stderr.
    pub fn spawn(spec: &Spec, isolation: Isolation, stdout: Stdio, stderr: Stdio) -> Result<Sandbox> {
        if spec.argv.is_empty() { return Err(anyhow!("empty command")); }
        let mut keep: Vec<OwnedFd> = Vec::new();
        let mut runsc_state = None;
        let mut cmd = match isolation {
            Isolation::Bwrap => bwrap_command(spec, &mut keep)?,
            Isolation::Gvisor => {
                let (cmd, state) = gvisor::command(spec)?;
                runsc_state = Some(state);
                cmd
            }
        };
        let rl = match isolation {
            // inside gVisor the limits are applied by its kernel (OCI rlimits); on the host process they
            // would constrain gVisor itself
            Isolation::Gvisor => Limits::none(),
            Isolation::Bwrap => spec.limits,
        };

        // resources: a cgroup per sandbox
        let mut cgroup = None;
        let mut join_fd: Option<OwnedFd> = None;
        if spec.limits.needs_cgroup() {
            match cgroups()? {
                Cgroups::Delegated(root) => {
                    let dir = root.join(unique_name("sb"));
                    std::fs::create_dir(&dir).map_err(|e| anyhow!("create cgroup {}: {}", dir.display(), e))?;
                    write_cgroup_limits(&dir, &spec.limits)?;
                    let procs = std::fs::OpenOptions::new().write(true).open(dir.join("cgroup.procs"))
                        .map_err(|e| anyhow!("open {}/cgroup.procs: {}", dir.display(), e))?;
                    join_fd = Some(procs.into());
                    cgroup = Some(SandboxCgroup::Dir(dir));
                }
                Cgroups::SystemdUser => {
                    let unit = format!("{}.scope", unique_name("selfdb-sb"));
                    let mut wrapped = Command::new("systemd-run");
                    wrapped.args(["--user", "--scope", "--quiet", "--collect", "--unit", &unit]);
                    for p in systemd_properties(&spec.limits) { wrapped.arg("-p").arg(p); }
                    wrapped.arg("--").arg(cmd.get_program()).args(cmd.get_args());
                    cmd = wrapped;
                    cgroup = Some(SandboxCgroup::Unit(unit));
                }
            }
        }

        let join = join_fd.as_ref().map(|f| f.as_raw_fd());
        // descriptors this child alone must inherit (see seccomp::memfd_with)
        let inherit: Vec<i32> = keep.iter().map(|f| f.as_raw_fd()).collect();
        let rlimits = rlimit_list(&rl);
        // SAFETY: the closure runs in the child between fork and exec. It only calls write(2), fcntl(2) and
        // setrlimit(2), which are async-signal-safe, on values prepared before the fork.
        unsafe {
            cmd.pre_exec(move || {
                if let Some(fd) = join {
                    // joining the sandbox's cgroup before exec: nothing of the sandbox runs outside it
                    if libc::write(fd, b"0\n".as_ptr().cast(), 2) != 2 { return Err(std::io::Error::last_os_error()); }
                }
                for fd in &inherit {
                    if libc::fcntl(*fd, libc::F_SETFD, 0) != 0 { return Err(std::io::Error::last_os_error()); }
                }
                for (res, value) in rlimits.iter().flatten() {
                    let lim = libc::rlimit { rlim_cur: *value, rlim_max: *value };
                    if libc::setrlimit(*res, &lim) != 0 { return Err(std::io::Error::last_os_error()); }
                }
                Ok(())
            });
        }
        if spec.interactive {
            cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
        }
        let child = cmd.spawn().map_err(|e| anyhow!("start sandbox ({:?}): {}", cmd.get_program(), e))?;
        if let Some(f) = join_fd { keep.push(f); }
        Ok(Sandbox { child, cgroup, _runsc_state: runsc_state, _keep: keep })
    }

    /// CPU time used by everything in the sandbox so far, in microseconds, from its cgroup. None without
    /// a cgroup, or when it cannot be read.
    pub fn cpu_usec(&mut self) -> Option<u64> {
        let path = match &self.cgroup {
            Some(SandboxCgroup::Dir(dir)) => dir.clone(),
            Some(SandboxCgroup::Unit(_)) => {
                // the scope's cgroup is the one the sandbox's top process runs in
                let line = std::fs::read_to_string(format!("/proc/{}/cgroup", self.child.id())).ok()?;
                PathBuf::from(format!("/sys/fs/cgroup{}", line.trim().strip_prefix("0::")?))
            }
            None => return None,
        };
        let stat = std::fs::read_to_string(path.join("cpu.stat")).ok()?;
        stat.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok())
    }

    /// Whether the sandbox's top process is still running.
    pub fn is_running(&mut self) -> bool { matches!(self.child.try_wait(), Ok(None)) }

    /// Stop every process of the sandbox, including ones that left the process group.
    pub fn kill_all(&mut self) {
        let _ = self.child.kill();
        match &self.cgroup {
            Some(SandboxCgroup::Dir(dir)) => { let _ = std::fs::write(dir.join("cgroup.kill"), "1"); }
            Some(SandboxCgroup::Unit(unit)) => {
                let _ = Command::new("systemctl").args(["--user", "kill", "--signal=SIGKILL", unit])
                    .stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
            None => {}
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.kill_all();
        let _ = self.child.wait();
        if let Some(SandboxCgroup::Dir(dir)) = &self.cgroup {
            // the directory can only be removed once it is empty; the processes may take a moment to exit
            for _ in 0..100 {
                if std::fs::remove_dir(dir).is_ok() { break; }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // _runsc_state is removed when the field drops, after the processes above have ended
    }
}

fn rlimit_list(l: &Limits) -> [Option<(libc::__rlimit_resource_t, libc::rlim_t)>; 3] {
    let mib = |v: u64| v.saturating_mul(1024 * 1024) as libc::rlim_t;
    [
        (l.address_space_mb > 0).then_some((libc::RLIMIT_AS, mib(l.address_space_mb))),
        (l.cpu_secs > 0).then_some((libc::RLIMIT_CPU, l.cpu_secs as libc::rlim_t)),
        (l.file_size_mb > 0).then_some((libc::RLIMIT_FSIZE, mib(l.file_size_mb))),
    ]
}

fn systemd_properties(l: &Limits) -> Vec<String> {
    let mut p = Vec::new();
    if l.memory_mb > 0 {
        p.push(format!("MemoryMax={}M", l.memory_mb));
        p.push("MemorySwapMax=0".into());
        // the kernel ends the process that ran out, as in a delegated cgroup; systemd's default for a
        // scope stops every process in it with SIGTERM
        p.push("OOMPolicy=continue".into());
    }
    if l.pids > 0 { p.push(format!("TasksMax={}", l.pids)); }
    if l.cpu_percent > 0 { p.push(format!("CPUQuota={}%", l.cpu_percent)); }
    p
}

fn write_cgroup_limits(dir: &Path, l: &Limits) -> Result<()> {
    let set = |file: &str, value: String| -> Result<()> {
        std::fs::write(dir.join(file), &value).map_err(|e| anyhow!("set {}/{} = {}: {}", dir.display(), file, value, e))
    };
    if l.memory_mb > 0 {
        set("memory.max", (l.memory_mb * 1024 * 1024).to_string())?;
        set("memory.swap.max", "0".into())?;
    }
    if l.pids > 0 { set("pids.max", l.pids.to_string())?; }
    if l.cpu_percent > 0 { set("cpu.max", format!("{} 100000", l.cpu_percent * 1000))?; }
    Ok(())
}

/// How sandboxes get their cgroups on this host.
enum Cgroups {
    /// `$SELF_SANDBOX_CGROUP`: a cgroup this process owns (a service with `Delegate=yes`). Each sandbox is
    /// a child directory of it.
    Delegated(PathBuf),
    /// A transient scope per sandbox from the user's systemd instance (an interactive login).
    SystemdUser,
}

/// Detected once per process.
fn cgroups() -> Result<&'static Cgroups> {
    static DETECTED: OnceLock<std::result::Result<Cgroups, String>> = OnceLock::new();
    DETECTED.get_or_init(detect_cgroups).as_ref().map_err(|why| anyhow!(
        "cannot apply sandbox resource limits: {}. Set SELF_SANDBOX_CGROUP to a delegated cgroup, run under a systemd user session, or disable the limits explicitly (trusted code only)", why))
}

fn detect_cgroups() -> std::result::Result<Cgroups, String> {
    if let Ok(root) = std::env::var("SELF_SANDBOX_CGROUP") {
        return prepare_delegated(Path::new(&root)).map(Cgroups::Delegated);
    }
    let ok = Command::new("systemd-run").args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false);
    if ok { Ok(Cgroups::SystemdUser) } else { Err("no delegated cgroup and no systemd user session".into()) }
}

/// Make `root` usable as the parent of sandbox cgroups. cgroup v2 allows controllers to be enabled for
/// children only when the parent itself holds no processes, so this process moves into a leaf
/// (`root/supervisor`) first.
fn prepare_delegated(root: &Path) -> std::result::Result<PathBuf, String> {
    if !root.starts_with("/sys/fs/cgroup") { return Err(format!("{} is not under /sys/fs/cgroup", root.display())); }
    let supervisor = root.join("supervisor");
    let _ = std::fs::create_dir(&supervisor);
    std::fs::write(supervisor.join("cgroup.procs"), "0")
        .map_err(|e| format!("move this process into {}: {}", supervisor.display(), e))?;
    let available = std::fs::read_to_string(root.join("cgroup.controllers")).map_err(|e| e.to_string())?;
    for c in ["memory", "pids", "cpu"] {
        if !available.split_whitespace().any(|x| x == c) { return Err(format!("controller {} is not delegated to {}", c, root.display())); }
    }
    std::fs::write(root.join("cgroup.subtree_control"), "+memory +pids +cpu")
        .map_err(|e| format!("enable controllers in {}: {}", root.display(), e))?;
    Ok(root.to_path_buf())
}

/// The bwrap command for `spec`. The seccomp program is passed as an inherited memfd, kept open in `keep`
/// until the sandbox has started.
fn bwrap_command(spec: &Spec, keep: &mut Vec<OwnedFd>) -> Result<Command> {
    if !Command::new("bwrap").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false) {
        return Err(anyhow!("bwrap is required to run VM content; refusing to run unisolated"));
    }
    let filter = seccomp::program()?;
    let fd = seccomp::memfd_with(&filter)?;
    let mut cmd = Command::new("bwrap");
    // new user, ipc, pid, net, uts and (if possible) cgroup namespaces; no nested user namespaces, so the
    // sandbox cannot regain capabilities in a namespace of its own
    cmd.args(["--unshare-all", "--unshare-user", "--disable-userns", "--die-with-parent", "--hostname", "vm"]);
    if !spec.interactive { cmd.arg("--new-session"); }
    if spec.layers.is_empty() {
        cmd.arg(if spec.root_writable { "--bind" } else { "--ro-bind" }).arg(&spec.root).arg("/");
    } else {
        match &spec.upper {
            Some((upper, work)) => {
                for l in &spec.layers { cmd.arg("--overlay-src").arg(l); }
                cmd.arg("--overlay").arg(upper).arg(work).arg("/");
            }
            // one layer read-only is a read-only bind (bwrap's read-only overlay needs two)
            None if spec.layers.len() == 1 => { cmd.arg("--ro-bind").arg(&spec.layers[0]).arg("/"); }
            None => {
                for l in &spec.layers { cmd.arg("--overlay-src").arg(l); }
                cmd.args(["--ro-overlay", "/"]);
            }
        }
    }
    cmd.args(["--dev", "/dev", "--proc", "/proc"]);
    for b in &spec.binds {
        cmd.arg(if b.writable { "--bind" } else { "--ro-bind" }).arg(&b.host).arg(&b.dest);
    }
    for o in &spec.ro_overlays {
        // bwrap needs two layers for a read-only overlay; one layer read-only is the same as a read-only bind
        if let [one] = o.layers.as_slice() {
            cmd.arg("--ro-bind").arg(one).arg(&o.dest);
            continue;
        }
        for l in &o.layers { cmd.arg("--overlay-src").arg(l); }
        cmd.arg("--ro-overlay").arg(&o.dest);
    }
    cmd.arg("--clearenv");
    for (k, v) in &spec.env { cmd.arg("--setenv").arg(k).arg(v); }
    cmd.arg("--chdir").arg(&spec.cwd);
    cmd.arg("--seccomp").arg(fd.as_raw_fd().to_string());
    cmd.arg("--");
    cmd.args(&spec.argv);
    keep.push(fd);
    Ok(cmd)
}

/// The syscall filter for bwrap sandboxes.
///
/// A deny list in the spirit of Docker's default profile: syscalls that a build or an agent does not need
/// and that widen the reachable kernel surface (module loading, mount API, keyrings, BPF, perf,
/// userfaultfd, io_uring, namespace creation, clock and system state). Each is refused with an errno,
/// so programs that probe for a feature see it as unavailable instead of being killed.
pub mod seccomp {
    use super::*;

    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const JSET_K: u16 = 0x45;
    const RET_K: u16 = 0x06;
    /// Offsets in `struct seccomp_data`.
    const OFF_NR: u32 = 0;
    const OFF_ARCH: u32 = 4;
    const OFF_ARG0_LO: u32 = 16;
    /// CLONE_NEWNS | NEWCGROUP | NEWUTS | NEWIPC | NEWUSER | NEWPID | NEWNET | NEWTIME
    const CLONE_NS_FLAGS: u32 = 0x0002_0000 | 0x0200_0000 | 0x0400_0000 | 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000 | 0x0000_0080;
    const SYS_CLONE: u32 = 56;

    /// (x86_64 syscall number, name, errno). Names are checked against the kernel headers by a test.
    pub const DENIED: &[(u32, &str, i32)] = &[
        (103, "syslog", libc::EPERM),
        (134, "uselib", libc::EPERM),
        (153, "vhangup", libc::EPERM),
        (155, "pivot_root", libc::EPERM),
        (159, "adjtimex", libc::EPERM),
        (163, "acct", libc::EPERM),
        (164, "settimeofday", libc::EPERM),
        (165, "mount", libc::EPERM),
        (166, "umount2", libc::EPERM),
        (167, "swapon", libc::EPERM),
        (168, "swapoff", libc::EPERM),
        (169, "reboot", libc::EPERM),
        (172, "iopl", libc::EPERM),
        (173, "ioperm", libc::EPERM),
        (175, "init_module", libc::EPERM),
        (176, "delete_module", libc::EPERM),
        (179, "quotactl", libc::EPERM),
        (212, "lookup_dcookie", libc::EPERM),
        (227, "clock_settime", libc::EPERM),
        (246, "kexec_load", libc::EPERM),
        (248, "add_key", libc::EPERM),
        (249, "request_key", libc::EPERM),
        (250, "keyctl", libc::EPERM),
        (272, "unshare", libc::EPERM),
        (298, "perf_event_open", libc::EPERM),
        (300, "fanotify_init", libc::EPERM),
        (303, "name_to_handle_at", libc::EPERM),
        (304, "open_by_handle_at", libc::EPERM),
        (305, "clock_adjtime", libc::EPERM),
        (308, "setns", libc::EPERM),
        (313, "finit_module", libc::EPERM),
        (320, "kexec_file_load", libc::EPERM),
        (321, "bpf", libc::EPERM),
        (323, "userfaultfd", libc::EPERM),
        (425, "io_uring_setup", libc::ENOSYS),
        (426, "io_uring_enter", libc::ENOSYS),
        (427, "io_uring_register", libc::ENOSYS),
        (428, "open_tree", libc::EPERM),
        (429, "move_mount", libc::EPERM),
        (430, "fsopen", libc::EPERM),
        (431, "fsconfig", libc::EPERM),
        (432, "fsmount", libc::EPERM),
        (433, "fspick", libc::EPERM),
        // ENOSYS, not EPERM: glibc then falls back to clone(2), whose flags the filter can inspect
        (435, "clone3", libc::ENOSYS),
        (442, "mount_setattr", libc::EPERM),
        (443, "quotactl_fd", libc::EPERM),
    ];

    fn ins(code: u16, jt: u8, jf: u8, k: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&code.to_ne_bytes());
        b[2] = jt;
        b[3] = jf;
        b[4..8].copy_from_slice(&k.to_ne_bytes());
        b
    }

    /// The classic-BPF program, as the bytes of a `struct sock_filter` array.
    pub fn program() -> Result<Vec<u8>> {
        if !cfg!(target_arch = "x86_64") {
            return Err(anyhow!("the sandbox syscall filter is defined for x86_64 only; refusing to run without it"));
        }
        let mut p: Vec<[u8; 8]> = vec![
            // another architecture's syscall table (32-bit compat) is not covered: kill
            ins(LD_W_ABS, 0, 0, OFF_ARCH),
            ins(JEQ_K, 1, 0, AUDIT_ARCH_X86_64),
            ins(RET_K, 0, 0, RET_KILL_PROCESS),
            ins(LD_W_ABS, 0, 0, OFF_NR),
            // the x32 ABI reuses x86_64 numbers with a high bit set: refuse all of it
            ins(JGE_K, 0, 1, X32_SYSCALL_BIT),
            ins(RET_K, 0, 0, RET_ERRNO | libc::ENOSYS as u32),
        ];
        for (nr, _, errno) in DENIED {
            p.push(ins(JEQ_K, 0, 1, *nr));
            p.push(ins(RET_K, 0, 0, RET_ERRNO | *errno as u32));
        }
        // clone(2) with a namespace flag: refused
        p.push(ins(JEQ_K, 0, 3, SYS_CLONE));
        p.push(ins(LD_W_ABS, 0, 0, OFF_ARG0_LO));
        p.push(ins(JSET_K, 0, 1, CLONE_NS_FLAGS));
        p.push(ins(RET_K, 0, 0, RET_ERRNO | libc::EPERM as u32));
        p.push(ins(RET_K, 0, 0, RET_ALLOW));
        Ok(p.concat())
    }

    /// A memfd holding `bytes`, positioned at the start, for `bwrap --seccomp FD`.
    ///
    /// Created close-on-exec: an inheritable descriptor would also reach every process another thread
    /// starts meanwhile, including other sandboxes (measured: concurrent sandboxes saw each other's
    /// filter descriptors). The sandbox's own child clears the flag on this one descriptor just before exec.
    pub(super) fn memfd_with(bytes: &[u8]) -> Result<OwnedFd> {
        let fd = unsafe { libc::memfd_create(c"selfdb-seccomp".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 { return Err(anyhow!("memfd_create: {}", std::io::Error::last_os_error())); }
        // SAFETY: fd was just returned by memfd_create and is owned by nothing else
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut f = std::fs::File::from(owned);
        use std::io::{Seek, SeekFrom, Write};
        f.write_all(bytes)?;
        f.seek(SeekFrom::Start(0))?;
        Ok(f.into())
    }
}

/// gVisor backend: the spec becomes an OCI bundle that `runsc` runs rootless, with no network.
pub mod gvisor {
    use super::*;

    /// The runsc binary: `$SELF_RUNSC`, else `runsc` on PATH.
    pub fn runsc() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("SELF_RUNSC") { let p = PathBuf::from(p); return p.is_file().then_some(p); }
        std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join("runsc")).find(|p| p.is_file()))
    }

    /// The command that runs `spec` in gVisor, and the state directory to remove afterwards.
    pub(super) fn command(spec: &Spec) -> Result<(Command, crate::vm::WorkDir)> {
        if !spec.layers.is_empty() || !spec.ro_overlays.is_empty() {
            return Err(anyhow!("overlay roots are supported with bwrap only; use a materialized root under gVisor"));
        }
        let runsc = runsc().ok_or_else(|| anyhow!("gVisor isolation needs runsc (set SELF_RUNSC or put it on PATH)"))?;
        let work = crate::vm::WorkDir::create("selfdb-runsc-")?;
        let state = work.path();
        let bundle = state.join("bundle");
        std::fs::create_dir_all(&bundle)?;
        std::fs::write(bundle.join("config.json"), oci_config(spec))?;
        let mut cmd = Command::new(runsc);
        if spec.root_shared { cmd.arg("--file-access=shared"); }
        cmd.arg("--rootless")
            .arg("--network=none")
            // cgroups are applied around runsc by the caller (one cgroup per sandbox, as for bwrap)
            .arg("--ignore-cgroups")
            // writes must reach the root directory: the tree is synced back to the database afterwards,
            // and gVisor's default keeps root writes in a memory overlay
            .arg("--overlay2=none")
            // the agent reaches the host proxy through a unix socket bound into the sandbox
            .arg("--host-uds=open")
            // a session's supervisor takes requests through a FIFO in its bound control directory
            .arg("--host-fifo=open")
            .arg("--root").arg(state.join("runsc-root"))
            .arg("run")
            .arg("--bundle").arg(&bundle)
            .arg(unique_name("selfdb"));
        Ok((cmd, work))
    }

    fn json_list(items: &[String]) -> String {
        let v: Vec<String> = items.iter().map(|s| crate::agent::json_string(s)).collect();
        format!("[{}]", v.join(","))
    }

    /// OCI runtime config for `spec`.
    pub fn oci_config(spec: &Spec) -> String {
        let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        let mut mounts = vec![
            r#"{"destination":"/proc","type":"proc","source":"proc"}"#.to_string(),
            r#"{"destination":"/dev","type":"tmpfs","source":"tmpfs","options":["nosuid","mode=755"]}"#.to_string(),
            r#"{"destination":"/dev/pts","type":"devpts","source":"devpts","options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620"]}"#.to_string(),
            r#"{"destination":"/dev/shm","type":"tmpfs","source":"shm","options":["nosuid","noexec","nodev","mode=1777"]}"#.to_string(),
            r#"{"destination":"/sys","type":"sysfs","source":"sysfs","options":["nosuid","noexec","nodev","ro"]}"#.to_string(),
        ];
        for b in &spec.binds {
            mounts.push(format!(r#"{{"destination":{},"type":"bind","source":{},"options":["rbind","{}"]}}"#,
                crate::agent::json_string(&b.dest), crate::agent::json_string(&b.host.to_string_lossy()),
                if b.writable { "rw" } else { "ro" }));
        }
        let l = &spec.limits;
        let mut rlimits = Vec::new();
        let mib = |v: u64| v.saturating_mul(1024 * 1024);
        if l.address_space_mb > 0 { rlimits.push(format!(r#"{{"type":"RLIMIT_AS","hard":{0},"soft":{0}}}"#, mib(l.address_space_mb))); }
        if l.cpu_secs > 0 { rlimits.push(format!(r#"{{"type":"RLIMIT_CPU","hard":{0},"soft":{0}}}"#, l.cpu_secs)); }
        if l.file_size_mb > 0 { rlimits.push(format!(r#"{{"type":"RLIMIT_FSIZE","hard":{0},"soft":{0}}}"#, mib(l.file_size_mb))); }
        format!(r#"{{
  "ociVersion": "1.0.2",
  "process": {{
    "terminal": false,
    "user": {{"uid": 0, "gid": 0}},
    "args": {args},
    "env": {env},
    "cwd": {cwd},
    "rlimits": [{rlimits}],
    "noNewPrivileges": true,
    "capabilities": {{"bounding": [], "effective": [], "inheritable": [], "permitted": [], "ambient": []}}
  }},
  "root": {{"path": {root}, "readonly": {ro}}},
  "hostname": "vm",
  "mounts": [{mounts}],
  "linux": {{
    "namespaces": [{{"type":"pid"}},{{"type":"network"}},{{"type":"ipc"}},{{"type":"uts"}},{{"type":"mount"}}]
  }}
}}
"#,
            args = json_list(&spec.argv),
            env = json_list(&env),
            cwd = crate::agent::json_string(&spec.cwd),
            rlimits = rlimits.join(","),
            root = crate::agent::json_string(&spec.root.to_string_lossy()),
            ro = !spec.root_writable,
            // no user namespace here: `runsc --rootless` sets up its own, and declaring one in the spec
            // made gVisor fail to start its gofer ("fork/exec /proc/self/exe: operation not permitted",
            // measured with release-20261019)
            mounts = mounts.join(","))
    }
}
