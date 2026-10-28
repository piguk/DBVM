//! Long-lived sessions: one sandbox per VM that stays up across commands.
//!
//! A step (`agent::run_step`) materializes the tree, starts a sandbox, runs one command and tears it all
//! down, so nothing a command starts survives it, and each step pays for a materialize. A session keeps
//! both: the tree is materialized once, the sandbox runs a supervisor (src/session/init.c) for its whole
//! life, and each command is sent to that supervisor. Background processes (a dev server, a watcher) keep
//! running between commands.
//!
//! Each command is still a recorded step, at a cost in proportion to what it changed, not to the tree:
//! - an inotify watcher (session/watch.rs) collects the paths the command touched, and only those are
//!   synced back (the whole tree when the watcher is unsure);
//! - every row written is first copied into the command's undo record (vm::undo_*), which gives the change
//!   list and, applied, the rollback.
//!
//! With a full sync, a snapshot and a full diff per command, an empty command cost 1.25 s at 49k entries
//! (measured); those three scale with the tree.
//!
//! The root is the materialized tree, not the FUSE mount: over the mount, file-heavy work was 5-12 times
//! slower (measured, docs/vm.md).

mod watch;

use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::{self, RestorePoint, StepLock, StepResult};
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
    /// Paths of the tree changed since the last command was recorded.
    watch: watch::TreeWatch,
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
        let (tree, ctl, bin) = (dir.join("tree"), dir.join("ctl"), dir.join("bin"));
        vm::vm_materialize_tree(&conn, &tree).map_err(|e| anyhow!("materialize failed: {}", e))?;
        // watching starts before the sandbox does, so no write of the session is missed
        let watch = watch::TreeWatch::new(&tree)?;
        let mut session = Session { conn, _work: work, dir: dir.clone(), sandbox: None, env: opts.env.clone(), watch, seq: 0, _lock: lock };
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
        let cmd = format!("session: {}", argv.join(" "));
        let changes = self.watch.take();
        let undo = vm::undo_begin(&self.conn, &cmd)?;
        let sync = if changes.full {
            vm::vm_sync_tree(&self.conn, &self.tree(), Some(undo))?
        } else {
            vm::vm_sync_paths(&self.conn, &self.tree(), &changes.paths, Some(undo))?
        };
        // A sync that could not write some paths fails the command: its writes were refused (a policy rule),
        // so reporting success would be untrue. What was applied is undone, and the tree is brought back to
        // what the database holds.
        if let Some(why) = sync.failure_summary() {
            let paths = undo_paths(&self.conn, &[undo])?;
            vm::undo_apply(&self.conn, undo)?;
            vm::undo_delete(&self.conn, undo)?;
            self.restore_tree(&paths)?;
            return Err(anyhow!("sandbox content not applied: {}", why));
        }
        let diff = vm::undo_diff(&self.conn, undo)?;
        let rolled_back = (outcome.exit_code != 0 || outcome.timed_out) && opts.rollback_on_fail;
        if rolled_back {
            vm::undo_apply(&self.conn, undo)?;
            self.restore_tree(&undo_paths(&self.conn, &[undo])?)?;
        }
        // The undo record is kept either way: it is this command's restore point, and after a rollback it
        // describes the state that is now current.
        let restore = RestorePoint::Undo(undo);
        let duration_ms = started.elapsed().as_millis();
        let step_id = agent::insert_step(&self.conn, &cmd, &outcome, rolled_back, duration_ms, &restore, &diff)?;
        let Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated } = outcome;
        Ok(StepResult { step_id, exit_code, timed_out, rolled_back, stdout, stderr, stdout_truncated, stderr_truncated,
                        changes: diff, duration_ms, model_ms: 0, model_requests: 0, restore })
    }

    /// Return to the state before command `undo` (a `RestorePoint::Undo` of this database): every undo
    /// record from it on is applied, newest first. Processes the commands left running keep running.
    pub fn rewind(&mut self, undo: i64) -> Result<()> {
        let ids: Vec<i64> = self.conn.prepare("SELECT id FROM vm_undo WHERE id >= ?1 ORDER BY id DESC")?
            .query_map([undo], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        if !ids.contains(&undo) { return Err(anyhow!("no undo record {}", undo)); }
        vm::in_savepoint(&self.conn, "rewind", || { for id in &ids { vm::undo_apply(&self.conn, *id)?; } Ok(()) })?;
        let paths = undo_paths(&self.conn, &ids)?;
        self.restore_tree(&paths)
    }

    /// Make the tree match the database again after `paths` were restored there, and forget the writes
    /// that did it. The restored entries are removed from the tree first: the materialize keeps a file
    /// whose size and mtime (to the second) match the row, and a same-size rewrite within the second of the
    /// restored mtime would otherwise stay in the tree while the database holds the old content.
    fn restore_tree(&mut self, paths: &[String]) -> Result<()> {
        let tree = self.tree();
        for p in paths {
            let host = tree.join(p.trim_start_matches('/'));
            if let Ok(md) = std::fs::symlink_metadata(&host) {
                if !md.is_dir() { let _ = std::fs::remove_file(&host); }
            }
        }
        vm::vm_materialize_tree(&self.conn, &tree)?;
        self.watch.discard();
        Ok(())
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

/// The paths the given undo records cover.
fn undo_paths(conn: &rusqlite::Connection, ids: &[i64]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for id in ids {
        let rows: Vec<String> = conn.prepare_cached("SELECT path FROM vm_undo_fs WHERE undo_id=?1")?
            .query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        out.extend(rows);
    }
    Ok(out)
}

/// The first `cap` bytes of a file, and whether it held more.
fn read_capped(path: &Path, cap: usize) -> (Vec<u8>, bool) {
    let Ok(f) = std::fs::File::open(path) else { return (Vec::new(), false) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut out = Vec::new();
    let _ = f.take(cap as u64).read_to_end(&mut out);
    (out, len > cap as u64)
}
