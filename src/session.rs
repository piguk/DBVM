//! Long-lived sessions: one sandbox per VM that stays up across commands.
//!
//! A step (`agent::run_step`) materializes the tree, starts a sandbox, runs one command and tears it all
//! down, so nothing a command starts survives it, and each step pays for a materialize. A session keeps
//! both: the tree is materialized once, the sandbox runs a supervisor (src/session/init.c) for its whole
//! life, and each command is sent to that supervisor. Background processes (a dev server, a watcher) keep
//! running between commands.
//!
//! Each command is still a recorded step: after it finishes, the tree is synced back to the database,
//! diffed against the snapshot taken after the previous command, and either kept (a new snapshot) or rolled
//! back (database and tree both restored). The root is the materialized tree, not the FUSE mount: over the
//! mount, file-heavy work was 5-12 times slower (measured, docs/vm.md).

use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::{self, Finish, StepLock, StepResult};
use crate::sandbox::{self, Isolation, Limits, Outcome, Sandbox};
use crate::vm;

/// The supervisor binary, built static by build.rs.
const SUPERVISOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/session-init"));

/// Where the supervisor and its control directory appear inside the sandbox. Reserved: never synced into
/// the database (see `vm::is_reserved_vm_path`).
const BIN_DIR: &str = "/.selfdb/bin";
const CTL_DIR: &str = "/.selfdb/ctl";

/// How long a sandbox may take to bring the supervisor up (gVisor starts in well under a second).
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long past a command's own timeout the host waits before declaring the supervisor unresponsive.
const SUPERVISOR_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub isolation: Isolation,
    /// Limits for the whole session: every command and everything it leaves running share them.
    pub limits: Limits,
    /// The environment every command starts with.
    pub env: Vec<(String, String)>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            isolation: Isolation::Bwrap,
            limits: Limits::default(),
            env: vec![("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()), ("HOME".into(), "/root".into())],
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
    /// Restore the state before this command when it fails or times out.
    pub rollback_on_fail: bool,
    pub cwd: String,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions { timeout_secs: 60, max_output_bytes: agent::DEFAULT_MAX_OUTPUT_BYTES, rollback_on_fail: false, cwd: "/".into() }
    }
}

/// A running session. Dropping it stops the sandbox and removes the tree; every finished command is
/// already in the database.
pub struct Session {
    conn: rusqlite::Connection,
    /// Holds the tree, the control directory and the supervisor; reclaimed if this process is killed.
    _work: vm::WorkDir,
    dir: PathBuf,
    sandbox: Option<Sandbox>,
    env: Vec<(String, String)>,
    /// The snapshot the next command is diffed against: the state after the previous command.
    before: String,
    seq: u64,
    _lock: StepLock,
}

impl Session {
    /// Materialize `db`, start its sandbox and wait for the supervisor. The database is locked for the
    /// session's lifetime: steps and other sessions on it wait.
    pub fn start(db: &Path, opts: &SessionOptions) -> Result<Session> {
        let lock = StepLock::acquire(db)?;
        let conn = vm::vm_open(db.to_str().ok_or_else(|| anyhow!("non-utf8 db path"))?)?;
        let work = vm::WorkDir::create("vm-session-")?;
        let dir = work.path().to_path_buf();
        // from here on, a failure must not leave the directory behind
        let mut session = Session { conn, _work: work, dir: dir.clone(), sandbox: None, env: opts.env.clone(), before: String::new(), seq: 0, _lock: lock };
        let (tree, ctl, bin) = (dir.join("tree"), dir.join("ctl"), dir.join("bin"));
        vm::vm_materialize_tree(&session.conn, &tree).map_err(|e| anyhow!("materialize failed: {}", e))?;
        std::fs::create_dir_all(&ctl)?;
        std::fs::create_dir_all(&bin)?;
        let init = bin.join("init");
        std::fs::write(&init, SUPERVISOR)?;
        std::fs::set_permissions(&init, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        let fifo = std::ffi::CString::new(ctl.join("fifo").to_string_lossy().as_bytes())?;
        if unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) } != 0 {
            return Err(anyhow!("mkfifo: {}", std::io::Error::last_os_error()));
        }

        let mut spec = sandbox::Spec::new(&tree, vec![format!("{}/init", BIN_DIR), CTL_DIR.into()]);
        spec.binds.push(sandbox::Bind { host: bin, dest: BIN_DIR.into(), writable: false });
        spec.binds.push(sandbox::Bind { host: ctl.clone(), dest: CTL_DIR.into(), writable: true });
        spec.limits = opts.limits;
        // the host restores the tree after a rolled-back command, while the sandbox runs
        spec.root_shared = true;
        let log = std::fs::File::create(dir.join("supervisor.log"))?;
        let sb = Sandbox::spawn(&spec, opts.isolation, log.try_clone()?.into(), log.into())?;
        session.sandbox = Some(sb);

        let started = Instant::now();
        while !ctl.join("ready").exists() {
            if !session.sandbox_running() { return Err(anyhow!("session sandbox exited during start: {}", session.log_tail())); }
            if started.elapsed() > START_TIMEOUT { return Err(anyhow!("session supervisor did not start: {}", session.log_tail())); }
            std::thread::sleep(Duration::from_millis(5));
        }
        session.before = vm::snapshot_create_named(&session.conn, "session-start", "vm-session")?.1;
        Ok(session)
    }

    /// The session's working directory on the host (tree, control directory, supervisor).
    pub fn work_dir(&self) -> &Path { &self.dir }

    fn tree(&self) -> PathBuf { self.dir.join("tree") }
    fn ctl(&self) -> PathBuf { self.dir.join("ctl") }

    fn sandbox_running(&mut self) -> bool { self.sandbox.as_mut().map(|s| s.is_running()).unwrap_or(false) }

    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(self.dir.join("supervisor.log")).unwrap_or_default();
        log.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ")
    }

    /// Run one command in the session and record it as a step.
    pub fn exec(&mut self, argv: &[String], opts: &ExecOptions) -> Result<StepResult> {
        if argv.is_empty() { return Err(anyhow!("empty command")); }
        let started = Instant::now();
        let outcome = self.run(argv, opts)?;
        // Sync the tree back. A sync that could not write some paths fails the step: the command's writes
        // were refused (a policy rule), so reporting success would be untrue.
        let sync = vm::vm_sync_report(&self.conn, &self.tree())?;
        if let Some(why) = sync.failure_summary() {
            // the database refused the change: bring the tree back to what the database holds
            let _ = vm::vm_materialize_tree(&self.conn, &self.tree());
            return Err(anyhow!("sandbox content not applied: {}", why));
        }
        let before = self.before.clone();
        let r = agent::finish_run(&self.conn, outcome, Finish {
            cmd: format!("session: {}", argv.join(" ")), before: &before, rollback_on_fail: opts.rollback_on_fail,
            after_prefix: "session-step", note: "vm-session", started, model: (0, 0),
        })?;
        if r.rolled_back {
            // the database is back at `before`; so must the tree be, or the next sync would undo the rollback
            vm::vm_materialize_tree(&self.conn, &self.tree())?;
        }
        if let Some(after) = &r.snap_after { self.before = after.clone(); }
        Ok(r)
    }

    /// Send one request to the supervisor and wait for its status.
    fn run(&mut self, argv: &[String], opts: &ExecOptions) -> Result<Outcome> {
        if !self.sandbox_running() { return Err(anyhow!("the session sandbox is not running: {}", self.log_tail())); }
        self.seq += 1;
        let id = self.seq.to_string();
        let req = self.ctl().join(&id);
        std::fs::create_dir(&req)?;
        let nul_list = |items: &mut dyn Iterator<Item = String>| -> Vec<u8> {
            let mut v = Vec::new();
            for s in items { v.extend_from_slice(s.as_bytes()); v.push(0); }
            v
        };
        std::fs::write(req.join("argv"), nul_list(&mut argv.iter().cloned()))?;
        std::fs::write(req.join("env"), nul_list(&mut self.env.iter().map(|(k, v)| format!("{}={}", k, v))))?;
        std::fs::write(req.join("cwd"), &opts.cwd)?;
        let timeout = Duration::from_secs(opts.timeout_secs);
        std::fs::write(req.join("timeout_ms"), timeout.as_millis().to_string())?;
        {
            // A FIFO opened for writing blocks until it has a reader. The supervisor always holds it open,
            // so a non-blocking open fails at once instead of hanging when the supervisor is gone.
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(self.ctl().join("fifo"))
                .map_err(|e| anyhow!("signal the session supervisor: {}", e))?;
            f.write_all(format!("{}\n", id).as_bytes())?;
        }

        let started = Instant::now();
        let status = loop {
            if let Ok(s) = std::fs::read_to_string(req.join("status")) { break s; }
            if !self.sandbox_running() { return Err(anyhow!("the session sandbox exited while running a command: {}", self.log_tail())); }
            if started.elapsed() > timeout + SUPERVISOR_GRACE { return Err(anyhow!("the session supervisor did not answer")); }
            std::thread::sleep(Duration::from_millis(2));
        };
        let (exit_code, timed_out) = match status.trim() {
            "timeout" => (sandbox::EXIT_TIMEOUT, true),
            s if s.starts_with("exit ") => (s[5..].parse().map_err(|_| anyhow!("bad status {:?}", s))?, false),
            s => return Err(anyhow!("session command failed: {}", s.strip_prefix("error ").unwrap_or(s))),
        };
        let (stdout, stdout_truncated) = read_capped(&req.join("out"), opts.max_output_bytes);
        let (stderr, stderr_truncated) = read_capped(&req.join("err"), opts.max_output_bytes);
        let _ = std::fs::remove_dir_all(&req);
        Ok(Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated })
    }

    /// Stop the session. Equivalent to dropping it; every finished command is already recorded.
    pub fn close(self) {}
}

impl Drop for Session {
    fn drop(&mut self) {
        // the sandbox first: its processes hold the tree open; the work directory goes after it
        self.sandbox = None;
    }
}

/// The first `cap` bytes of a file, and whether it held more.
fn read_capped(path: &Path, cap: usize) -> (Vec<u8>, bool) {
    let Ok(f) = std::fs::File::open(path) else { return (Vec::new(), false) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut out = Vec::new();
    let _ = f.take(cap as u64).read_to_end(&mut out);
    (out, len > cap as u64)
}
