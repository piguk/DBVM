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

pub mod supervisor;


use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::{self, RestorePoint, StepLock, StepResult};
use crate::sandbox::{self, Isolation, Limits, Outcome};
use crate::vm;
use supervisor::{DirEntry, Supervisor};

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
    sup: Option<Supervisor>,
    env: Vec<(String, String)>,
    /// Paths of the tree changed since the last command was recorded.
    watch: crate::watch::TreeWatch,
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
        let tree = dir.join("tree");
        vm::vm_materialize_tree(&conn, &tree).map_err(|e| anyhow!("materialize failed: {}", e))?;
        // watching starts before the sandbox does, so no write of the session is missed
        let watch = crate::watch::TreeWatch::new(&tree, vm::is_reserved_vm_path);
        let mut session = Session { conn, _work: work, dir: dir.clone(), sup: None, env: opts.env.clone(), watch, _lock: lock };
        let mut spec = sandbox::Spec::new(&tree, Vec::new());
        spec.limits = opts.limits;
        // the host restores the tree after a rolled-back command, while the sandbox runs
        spec.root_shared = true;
        session.sup = Some(Supervisor::start(spec, opts.isolation, &dir, None)?);
        Ok(session)
    }

    /// The session's working directory on the host (tree, control directory, supervisor).
    pub fn work_dir(&self) -> &Path { &self.dir }

    fn tree(&self) -> PathBuf { self.dir.join("tree") }

    fn sup(&mut self) -> Result<&mut Supervisor> { self.sup.as_mut().ok_or_else(|| anyhow!("the session is closed")) }

    /// Run one command in the session and record it as a step.
    pub fn exec(&mut self, argv: &[String], opts: &ExecOptions) -> Result<StepResult> {
        if argv.is_empty() { return Err(anyhow!("empty command")); }
        let started = Instant::now();
        let env = self.env.clone();
        let outcome = self.sup()?.exec(argv, &env, &opts.cwd, Duration::from_secs(opts.timeout_secs), opts.max_output_bytes)?;
        self.record(&format!("session: {}", argv.join(" ")), outcome, opts.rollback_on_fail, started)
    }

    /// Write a file in the session (parent directories are created), recorded as a step like a command. A
    /// refused write (a read-only path) is a failed step with the sandbox's message in stderr.
    pub fn write_file(&mut self, path: &str, data: &[u8], mode: Option<u32>) -> Result<StepResult> {
        let started = Instant::now();
        let outcome = match self.sup()?.write(path, data, mode)? {
            Ok(()) => Outcome::default(),
            Err(msg) => Outcome { exit_code: 1, stderr: msg.into_bytes(), ..Outcome::default() },
        };
        self.record(&format!("session: write {}", path), outcome, false, started)
    }

    /// Read a file in the session. Not a step: it changes nothing.
    pub fn read_file(&mut self, path: &str, offset: u64, len: u64) -> Result<std::result::Result<Vec<u8>, String>> {
        self.sup()?.read(path, offset, len)
    }

    /// List a directory in the session. Not a step: it changes nothing.
    pub fn list_dir(&mut self, path: &str) -> Result<std::result::Result<Vec<DirEntry>, String>> {
        self.sup()?.list(path)
    }

    /// Bring what the action changed into the database and record the step.
    fn record(&mut self, cmd: &str, outcome: Outcome, rollback_on_fail: bool, started: Instant) -> Result<StepResult> {
        let cmd = cmd.to_string();
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
        let rolled_back = (outcome.exit_code != 0 || outcome.timed_out) && rollback_on_fail;
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

    /// Stop the session. Equivalent to dropping it; every finished command is already recorded.
    pub fn close(self) {}
}

impl Drop for Session {
    fn drop(&mut self) {
        // the sandbox first: its processes hold the tree open; the work directory goes after it
        self.sup = None;
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
