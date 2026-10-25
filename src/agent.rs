//! agent-step: run one command against a VM, record it, and optionally roll it back.
//!
//! Flow: snapshot the tree (before) -> materialize into a scratch directory -> run the command
//! under bwrap with that directory as its root -> sync the directory back -> diff against the
//! snapshot -> record the step -> restore the snapshot if the step failed and rollback was asked.
//!
//! Isolation is bwrap's: a new PID namespace, a new network namespace (only a down loopback, so no
//! outbound or inbound traffic, no host or tailnet services), /dev and /proc, and the materialized
//! tree as root. A step is refused (not run unisolated) if bwrap is missing.

use anyhow::{anyhow, Result};
use rusqlite::params;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::vm;

#[derive(Debug, Clone)]
pub struct StepOptions {
    /// Restore the pre-step snapshot when the step exits non-zero or times out.
    pub rollback_on_fail: bool,
    pub timeout_secs: u64,
    /// Keep at most this many bytes of each of stdout and stderr; the rest is read and dropped.
    pub max_output_bytes: usize,
}

/// Default output cap per stream (1 MiB).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1 << 20;

#[derive(Debug)]
pub struct StepResult {
    pub step_id: i64,
    pub exit_code: i32,
    pub timed_out: bool,
    pub rolled_back: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub changes: vm::SnapDiff,
    pub duration_ms: u128,
}

fn bwrap_available() -> bool {
    Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

pub fn run_step(db: &Path, argv: &[String], opts: &StepOptions) -> Result<StepResult> {
    if argv.is_empty() { return Err(anyhow!("empty command")); }
    if !bwrap_available() { return Err(anyhow!("bwrap is required for agent-step; refusing to run unisolated")); }
    // one step at a time per database: the tree mirror would otherwise delete a concurrent step's writes
    let _lock = StepLock::acquire(db)?;
    let conn = vm::vm_open(db.to_str().ok_or_else(|| anyhow!("non-utf8 db path"))?)?;
    // the name comes from the snapshot's own row id, so two steps starting at once cannot collide
    let (_, before) = vm::snapshot_create_named(&conn, "step-before", "agent-step")?;

    // same guard as run_agent: no early return may leave the materialised tree behind
    let scratch = ScratchDir::create("agent-step-")?;
    let started = Instant::now();
    vm::vm_materialize_tree(&conn, scratch.path()).map_err(|e| anyhow!("materialize failed: {}", e))?;
    let outcome = run_in(scratch.path(), argv, opts.timeout_secs, opts.max_output_bytes)?;
    let Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated } = outcome;

    // Sync the scratch tree back, then diff against the snapshot taken before the step. A sync that
    // could not write some paths must fail the step: the command ran and its writes were refused (a
    // policy rule, for instance), so reporting success would tell the caller something untrue. The
    // failed files are named in the error.
    let sync = vm::vm_sync_report(&conn, scratch.path())?;
    if let Some(why) = sync.failure_summary() {
        let duration_ms = started.elapsed().as_millis();
        let cmd = argv.join(" ");
        return Err(record_and_fail(&conn, &cmd, &stdout, &stderr, stdout_truncated, exit_code, timed_out,
            &before, &why, duration_ms));
    }
    let changes = vm::snapshot_diff_live(&conn, &before)?;
    // the guard removes the tree when this function returns

    let failed = exit_code != 0 || timed_out;
    let rolled_back = failed && opts.rollback_on_fail;
    let after_name = if rolled_back {
        vm::snapshot_restore(&conn, &before)?;
        None
    } else {
        let (_, a) = vm::snapshot_create_named(&conn, "step-after", "agent-step")?;
        Some(a)
    };
    let duration_ms = started.elapsed().as_millis();

    let diff_json = diff_to_json(&changes);
    conn.execute(
        "INSERT INTO agent_steps(cmd, exit_code, timed_out, rolled_back, duration_ms, stdout, stderr, stdout_truncated, stderr_truncated, snap_before, snap_after, fs_diff_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![argv.join(" "), exit_code, timed_out as i64, rolled_back as i64, duration_ms as i64,
                &stdout, &stderr, stdout_truncated as i64, stderr_truncated as i64, before, after_name, diff_json])?;
    let step_id = conn.last_insert_rowid();
    Ok(StepResult { step_id, exit_code, timed_out, rolled_back, stdout, stderr, stdout_truncated, stderr_truncated, changes, duration_ms })
}

/// Result of one sandboxed run.
struct Outcome { exit_code: i32, timed_out: bool, stdout: Vec<u8>, stderr: Vec<u8>, stdout_truncated: bool, stderr_truncated: bool }

/// Read a pipe to EOF, keeping the first `cap` bytes. Bytes past the cap are still read and dropped,
/// so the child never blocks on a full pipe. Returns (kept bytes, truncated).
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

/// Run argv under bwrap with `root` as /.
fn run_in(root: &Path, argv: &[String], timeout_secs: u64, max_output: usize) -> Result<Outcome> {
    let mut cmd = Command::new("bwrap");
    cmd.arg("--bind").arg(root).arg("/")
        .arg("--dev").arg("/dev")
        .arg("--proc").arg("/proc")
        .arg("--unshare-pid")
        .arg("--unshare-net")
        .arg("--die-with-parent")
        .arg("--chdir").arg("/");
    for a in argv { cmd.arg(a); }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| anyhow!("spawn bwrap: {}", e))?;

    // drain both pipes on their own threads so a chatty command cannot block on a full pipe
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || drain_capped(&mut out_pipe, max_output));
    let err_t = std::thread::spawn(move || drain_capped(&mut err_pipe, max_output));

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait()? { break st; }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stdout_truncated) = out_t.join().unwrap_or_default();
    let (stderr, stderr_truncated) = err_t.join().unwrap_or_default();
    let exit_code = if timed_out { 124 } else { status.code().unwrap_or(128) };
    Ok(Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated })
}

/// Compact JSON for the change list, written with stable key order.
fn diff_to_json(d: &vm::SnapDiff) -> String {
    let arr = |v: &Vec<String>| -> String {
        let items: Vec<String> = v.iter().map(|s| json_string(s)).collect();
        format!("[{}]", items.join(","))
    };
    format!("{{\"created\":{},\"modified\":{},\"deleted\":{}}}", arr(&d.created), arr(&d.modified), arr(&d.deleted))
}

/// A JSON string literal. Escapes the quote, backslash, and every control character (U+0000..U+001F),
/// so any path, including one with a newline or tab, produces valid JSON.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One recorded step, as stored in agent_steps.
#[derive(Debug, Clone)]
pub struct StepRecord {
    pub step_id: i64,
    pub cmd: String,
    pub exit_code: i64,
    pub timed_out: bool,
    pub rolled_back: bool,
    pub duration_ms: i64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub fs_diff_json: String,
    pub created_at: i64,
}

/// Recorded steps, oldest first. `limit` keeps the most recent N when given.
pub fn list_steps(db: &Path, limit: Option<usize>) -> Result<Vec<StepRecord>> {
    let conn = vm::vm_open(db.to_str().ok_or_else(|| anyhow!("non-utf8 db path"))?)?;
    let sql = "SELECT step_id, cmd, exit_code, timed_out, rolled_back, duration_ms, stdout_truncated, stderr_truncated, fs_diff_json, created_at
         FROM agent_steps ORDER BY step_id";
    let mut rows: Vec<StepRecord> = Vec::new();
    let map = |r: &rusqlite::Row| -> rusqlite::Result<StepRecord> {
        Ok(StepRecord {
            step_id: r.get(0)?, cmd: r.get(1)?, exit_code: r.get::<_, Option<i64>>(2)?.unwrap_or(-1),
            timed_out: r.get::<_, i64>(3)? != 0, rolled_back: r.get::<_, i64>(4)? != 0,
            duration_ms: r.get(5)?, stdout_truncated: r.get::<_, i64>(6)? != 0,
            stderr_truncated: r.get::<_, i64>(7)? != 0, fs_diff_json: r.get(8)?, created_at: r.get(9)?,
        })
    };
    let mut st = conn.prepare(sql)?;
    for r in st.query_map([], map)? { rows.push(r?); }
    if let Some(n) = limit {
        let skip = rows.len().saturating_sub(n);
        rows.drain(..skip);
    }
    Ok(rows)
}

/// One step as a JSON object. Every string goes through `json_string`.
pub fn step_to_json(s: &StepRecord) -> String {
    format!(
        "{{\"step_id\":{},\"cmd\":{},\"exit_code\":{},\"timed_out\":{},\"rolled_back\":{},\"duration_ms\":{},\"stdout_truncated\":{},\"stderr_truncated\":{},\"created_at\":{},\"changes\":{}}}",
        s.step_id, json_string(&s.cmd), s.exit_code, s.timed_out, s.rolled_back, s.duration_ms,
        s.stdout_truncated, s.stderr_truncated, s.created_at, s.fs_diff_json)
}

/// An exclusive lock over one database, held for a whole step.
///
/// `run_step` materialises the tree, runs the command, then mirrors the tree back. That mirror deletes
/// every database path absent from the tree, so two steps running at once delete each other's writes:
/// measured, two concurrent steps both reported creating a file and only one survived. Serialising the
/// whole step is the only correct behaviour while writes go through a full-tree sync. Real parallelism
/// needs one Delta per step instead (see docs/agentvm-gap-analysis.md).
pub struct StepLock { _file: std::fs::File }

impl StepLock {
    /// Take the lock for `db`. Blocks until any running step on the same database finishes.
    pub fn acquire(db: &Path) -> Result<StepLock> {
        let mut p = db.as_os_str().to_owned();
        p.push(".step-lock");
        let path = std::path::PathBuf::from(p);
        let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path)
            .map_err(|e| anyhow!("open step lock {}: {}", path.display(), e))?;
        // an exclusive advisory lock: released by the kernel if this process dies
        if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&f), libc::LOCK_EX) } != 0 {
            return Err(anyhow!("lock {}: {}", path.display(), std::io::Error::last_os_error()));
        }
        Ok(StepLock { _file: f })
    }
}

/// Provision a VM with node and the pi coding agent, so `pi` runs inside the VM.
///
/// What it does, all in the target database:
/// - imports node's own library closure (the binary plus its shared libraries), so the VM has a
///   runtime that needs nothing from the host;
/// - packs the pi package tree at `/opt/pi`, minus the prebuilt native modules for other platforms
///   (win32, darwin) and source maps, which the VM never loads;
/// - records `/usr/bin/node` and `/usr/bin/pi` as symlinks to those files, so a command line inside
///   the VM can name them without knowing where Homebrew put them.
///
/// The node path is kept as imported (its canonical location) because its shared libraries refer to
/// each other by absolute path. The well-known names are symlinks on top of that.
pub fn provision_agent(db: &Path, node: &Path, pi_dir: &Path) -> Result<ProvisionReport> {
    let conn = vm::vm_open(db.to_str().ok_or_else(|| anyhow!("non-utf8 db path"))?)?;
    let node_s = node.to_str().ok_or_else(|| anyhow!("non-utf8 node path"))?;
    let pi_s = pi_dir.to_str().ok_or_else(|| anyhow!("non-utf8 pi path"))?;
    let objects = vm::vm_import_closure(&conn, node_s, "/")?;
    // an agent runs shell commands, so a shell and its libraries must be there too
    let mut shell_objects = 0usize;
    for prog in ["/bin/sh", "/bin/rm", "/bin/ls", "/bin/cat", "/bin/mv", "/bin/cp", "/bin/mkdir", "/bin/sleep", "/bin/grep"] {
        if std::path::Path::new(prog).exists() {
            shell_objects += vm::vm_import_closure(&conn, prog, "/").unwrap_or(0);
        }
    }
    let files = vm::vm_pack_host_dir_parallel(&conn, pi_s, "/opt/pi")?;
    // the names a command line inside the VM uses
    let node_real = std::fs::canonicalize(node).unwrap_or_else(|_| node.to_path_buf());
    vm::vm_add_symlink(&conn, "/usr/bin/node", &node_real.to_string_lossy())?;
    vm::vm_add_symlink(&conn, "/usr/bin/pi", "/opt/pi/dist/bundle/cli.js")?;
    Ok(ProvisionReport { objects: objects + shell_objects, files, node: node_real.to_string_lossy().to_string() })
}


/// Record a run whose synced content could not be applied, then report the failure.
///
/// A refused write (a policy rule, for example) aborts the sync. The run still happened, so it must be
/// auditable: `agent_steps` gets the row with the reason attached to stderr, and the caller still sees an
/// error. Silently dropping the row leaves no trace of why the sandbox content vanished.
/// Exit code recorded for a run whose content was refused. The agent process may itself have exited 0
/// (it wrote inside the sandbox and saw no error), but the run did not take effect, so reporting 0 would
/// tell the reader the task succeeded. 125 is the shell's "command could not be executed" code, distinct
/// from 124 (timeout) and from an agent's own failure.
const EXIT_NOT_APPLIED: i32 = 125;

#[allow(clippy::too_many_arguments)]
fn record_and_fail(conn: &rusqlite::Connection, cmd: &str, stdout: &[u8], stderr: &[u8],
                   stdout_truncated: bool, _exit_code: i32, _timed_out: bool,
                   before: &str, why: &str, duration_ms: u128) -> anyhow::Error {
    let note = format!("sandbox content not applied: {}", why);
    let mut err_bytes = stderr.to_vec();
    err_bytes.extend_from_slice(b"\n");
    err_bytes.extend_from_slice(note.as_bytes());
    let _ = conn.execute(
        "INSERT INTO agent_steps(cmd, exit_code, timed_out, rolled_back, duration_ms, stdout, stderr, stdout_truncated, stderr_truncated, snap_before, snap_after, fs_diff_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![cmd, EXIT_NOT_APPLIED, 0i64, 0i64, duration_ms as i64,
                stdout, &err_bytes, stdout_truncated as i64, false, before, None::<String>,
                format!("{{\"created\":[],\"modified\":[],\"deleted\":[]}}")]);
    anyhow!("{}", note)
}


/// Removes a scratch directory when it goes out of scope.
///
/// Every early return in a run must clean up. Writing `remove_dir_all` on each error path is easy to
/// forget: measured, three failed agent runs left 539 MB of materialised trees under /tmp because the
/// paths after the proxy was bound returned with `?` and never cleaned up.
struct ScratchDir { path: std::path::PathBuf }

impl ScratchDir {
    fn create(prefix: &str) -> Result<ScratchDir> {
        Ok(ScratchDir { path: vm::make_tmp_dir(prefix)? })
    }
    fn path(&self) -> &Path { &self.path }
    /// Keep a file that lives inside the tree: used before syncing, so the guard still removes it.
    fn remove(&self, rel: &str) { let _ = std::fs::remove_file(self.path.join(rel)); }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() { let _ = std::fs::remove_dir_all(&self.path); }
    }
}

/// Options for one agent task.
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// Restore the pre-task snapshot when the agent exits non-zero or times out.
    pub rollback_on_fail: bool,
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
    /// Model base URL the host proxy forwards to, e.g. `https://host/v1`.
    pub model_base: String,
    /// Name of the environment variable holding the model credential. It is read on the host side and
    /// never placed in the sandbox environment.
    pub key_env: String,
    /// Model id the agent asks for.
    pub model: String,
    /// Port the in-sandbox forwarder listens on (loopback).
    pub forward_port: u16,
    /// The task text handed to the agent.
    pub task: String,
    /// Resource limits for the sandbox. 0 disables that limit.
    pub limits: Limits,
}

/// Resource limits applied inside the sandbox.
///
/// Measured on node 26 before choosing them: `RLIMIT_DATA` and small `RLIMIT_AS` make node fail to
/// start at all (a shared library cannot be mapped), so the address-space limit must be at least
/// ~1.5 GB and is a guard against runaway allocation, not a tight quota. With 2 GB a 2.5 GB
/// allocation fails as a catchable `RangeError` instead of an OOM kill.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// `RLIMIT_AS` in mebibytes; 0 = unlimited. Node needs >= ~1536.
    pub address_space_mb: u32,
    /// `RLIMIT_CPU` in seconds; 0 = unlimited.
    pub cpu_secs: u32,
    /// `RLIMIT_FSIZE` in mebibytes; 0 = unlimited.
    pub file_size_mb: u32,
    /// `RLIMIT_NPROC`; 0 = unlimited. Counts processes for the whole user, so it is off by default.
    pub processes: u32,
}

impl Default for Limits {
    fn default() -> Self { Limits { address_space_mb: 2048, cpu_secs: 0, file_size_mb: 1024, processes: 0 } }
}

impl Limits {
    /// The `ulimit` prelude for the sandbox shell. Kept in one place so the CLI and the library agree.
    pub fn ulimit_prelude(&self) -> String {
        let mut out = String::new();
        if self.address_space_mb > 0 { out.push_str(&format!("ulimit -v {} 2>/dev/null; ", self.address_space_mb as u64 * 1024)); }
        if self.cpu_secs > 0 { out.push_str(&format!("ulimit -t {} 2>/dev/null; ", self.cpu_secs)); }
        if self.file_size_mb > 0 { out.push_str(&format!("ulimit -f {} 2>/dev/null; ", self.file_size_mb as u64 * 1024)); }
        if self.processes > 0 { out.push_str(&format!("ulimit -u {} 2>/dev/null; ", self.processes)); }
        out
    }
}

/// What `provision_agent` put in the database.
#[derive(Debug)]
pub struct ProvisionReport { pub objects: usize, pub files: usize, pub node: String }

/// Run one task with the pi agent inside the VM, recorded like a step.
///
/// The chain, all of it measured by `tests/agent_proxy_e2e.rs`:
/// - snapshot the tree, materialise it, and start a host proxy on a unix socket;
/// - run the sandbox with no network, with that socket bound in, and a loopback forwarder started
///   before the agent;
/// - point the agent at the forwarder through a generated `models.json`, so the agent sees a local
///   endpoint and never a credential;
/// - mirror the tree back, diff it, and record the run in `agent_steps`.
pub fn run_agent(db: &Path, opts: &AgentOptions) -> Result<StepResult> {
    if opts.task.trim().is_empty() { return Err(anyhow!("empty task")); }
    if !bwrap_available() { return Err(anyhow!("bwrap is required for vm-agent; refusing to run unisolated")); }
    let api_key = std::env::var(&opts.key_env)
        .map_err(|_| anyhow!("{} is not set; the proxy needs the model credential", opts.key_env))?;
    if api_key.is_empty() { return Err(anyhow!("{} is empty", opts.key_env)); }
    let _lock = StepLock::acquire(db)?;
    let conn = vm::vm_open(db.to_str().ok_or_else(|| anyhow!("non-utf8 db path"))?)?;

    // The agent must be in the VM, or the task cannot run at all.
    let has_agent: i64 = conn.query_row(
        "SELECT count(*) FROM vm_fs WHERE path='/opt/pi/dist/bundle/cli.js'", [], |r| r.get(0))?;
    if has_agent == 0 {
        return Err(anyhow!("this VM has no agent; run `vm-provision-agent {}` first", db.display()));
    }

    let (_, before) = vm::snapshot_create_named(&conn, "agent-before", "vm-agent")?;
    // A guard, not ad-hoc cleanups: every early return below must remove the materialised tree.
    let scratch = ScratchDir::create("vm-agent-")?;
    let started = Instant::now();
    vm::vm_materialize_tree(&conn, scratch.path()).map_err(|e| anyhow!("materialize failed: {}", e))?;

    // The agent's settings live in the VM tree, so they are part of the diff and can be rolled back.
    let agent_dir = scratch.path().join("opt/piagent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("models.json"), model_config(&opts.model, opts.forward_port))?;
    std::fs::write(agent_dir.join("settings.json"), agent_settings(&opts.model))?;
    // .cjs: inside a directory with "type": "module" a .js file would load as ESM and lose `require`
    std::fs::create_dir_all(scratch.path().join("opt"))?;
    std::fs::write(scratch.path().join("opt/fwd.cjs"), crate::proxy::FORWARDER_JS)?;

    // Host side of the tunnel. The socket lives inside the tree under tmp/, which the sync reserves, so it
    // is never mistaken for VM content and is removed with the tree.
    std::fs::create_dir_all(scratch.path().join("tmp"))?;
    let sock = scratch.path().join("tmp/proxy.sock");
    let (upstream, prefix) = crate::proxy::Upstream::from_base_url(&opts.model_base, &api_key)?;
    let proxy = crate::proxy::Proxy::bind(&sock, upstream, &prefix)?;

    let outcome = run_agent_in(scratch.path(), &sock, opts);
    drop(proxy);
    let outcome = outcome?;
    let Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated } = outcome;

    // The agent rewrote settings the run itself created; do not report those as task changes.
    for rel in ["opt/piagent/models.json", "opt/piagent/settings.json", "opt/fwd.cjs", "opt/run-agent.sh", "tmp/proxy.sock", "tmp/fwd.log"] {
        scratch.remove(rel);
    }
    let sync = vm::vm_sync_report(&conn, scratch.path())?;
    if let Some(why) = sync.failure_summary() {
        let duration_ms = started.elapsed().as_millis();
        let cmd = format!("vm-agent: {}", opts.task);
        return Err(record_and_fail(&conn, &cmd, &stdout, &stderr, stdout_truncated, exit_code, timed_out,
            &before, &why, duration_ms));
    }
    let changes = vm::snapshot_diff_live(&conn, &before)?;
    // the guard removes the tree when this function returns

    let failed = exit_code != 0 || timed_out;
    let rolled_back = failed && opts.rollback_on_fail;
    let after_name = if rolled_back {
        vm::snapshot_restore(&conn, &before)?;
        None
    } else {
        let (_, a) = vm::snapshot_create_named(&conn, "agent-after", "vm-agent")?;
        Some(a)
    };
    let duration_ms = started.elapsed().as_millis();
    let diff_json = diff_to_json(&changes);
    conn.execute(
        "INSERT INTO agent_steps(cmd, exit_code, timed_out, rolled_back, duration_ms, stdout, stderr, stdout_truncated, stderr_truncated, snap_before, snap_after, fs_diff_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![format!("vm-agent: {}", opts.task), exit_code, timed_out as i64, rolled_back as i64, duration_ms as i64,
                &stdout, &stderr, stdout_truncated as i64, stderr_truncated as i64, before, after_name, diff_json])?;
    let step_id = conn.last_insert_rowid();
    Ok(StepResult { step_id, exit_code, timed_out, rolled_back, stdout, stderr, stdout_truncated, stderr_truncated, changes, duration_ms })
}

/// pi's provider config: a local endpoint that the forwarder serves, with a placeholder key. The real
/// credential is added by the host proxy, so this file carries nothing secret and can live in the VM.
fn model_config(model: &str, port: u16) -> String {
    format!(r#"{{
  "$schema": "https://pi.dev/schemas/models.schema.json",
  "providers": {{
    "vmproxy": {{
      "baseUrl": "http://127.0.0.1:{port}",
      "api": "openai-completions",
      "apiKey": "the-host-proxy-adds-the-real-key",
      "models": [ {{ "id": "{model}" }} ]
    }}
  }}
}}
"#)
}

/// pi's defaults for a one-shot run: no startup noise, the proxied provider selected.
fn agent_settings(model: &str) -> String {
    format!(r#"{{"defaultProvider":"vmproxy","defaultModel":"{model}","quietStartup":true,"hideThinkingBlock":true}}
"#)
}

/// The shell script the sandbox runs: start the forwarder, wait for its port, then run the agent.
///
/// Kept as its own small file inside the VM (`/opt/run-agent.sh`) rather than a long `-c` string, so
/// quoting cannot be mangled by a nested format!. The task itself travels in the environment.
const RUN_AGENT_SH: &str = r#"#!/bin/sh
# start the loopback forwarder, wait until it accepts, then run the agent once
PROXY_SOCK=/tmp/proxy.sock PROXY_PORT=$VM_AGENT_PORT /usr/bin/node /opt/fwd.cjs >/tmp/fwd.log 2>&1 &
i=0
while [ $i -lt 400 ]; do
  /usr/bin/node -e 'require("net").connect(Number(process.env.VM_AGENT_PORT),"127.0.0.1").on("connect",()=>process.exit(0)).on("error",()=>process.exit(1))' && break
  i=$((i+1))
  sleep 0.05
done
HOME=/opt/home PI_CODING_AGENT_DIR=/opt/piagent /usr/bin/node /opt/pi/dist/bundle/cli.js --print "$VM_AGENT_TASK"
"#;

/// Run the agent in the sandbox: bind the proxy socket in, then run the script above.
fn run_agent_in(root: &Path, proxy_sock: &Path, opts: &AgentOptions) -> Result<Outcome> {
    // the script lives in the tree that is bound as the sandbox root
    std::fs::write(root.join("opt/run-agent.sh"), RUN_AGENT_SH)?;
    let mut cmd = Command::new("bwrap");
    cmd.arg("--bind").arg(root).arg("/")
        .arg("--dev").arg("/dev")
        .arg("--proc").arg("/proc")
        .arg("--unshare-pid")
        .arg("--unshare-net")
        .arg("--die-with-parent")
        .arg("--bind").arg(proxy_sock).arg("/tmp/proxy.sock")
        .arg("--chdir").arg("/")
        .arg("/bin/sh").arg("-c").arg(format!("{}. /opt/run-agent.sh", opts.limits.ulimit_prelude()));
    // a clean environment: nothing from the host leaks in, and the credential is not here at all
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("VM_AGENT_TASK", &opts.task);
    cmd.env("VM_AGENT_PORT", opts.forward_port.to_string());
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| anyhow!("spawn bwrap: {}", e))?;
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let max = opts.max_output_bytes;
    let out_t = std::thread::spawn(move || drain_capped(&mut out_pipe, max));
    let err_t = std::thread::spawn(move || drain_capped(&mut err_pipe, max));
    let deadline = Instant::now() + Duration::from_secs(opts.timeout_secs);
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait()? { break st; }
        if Instant::now() >= deadline { timed_out = true; let _ = child.kill(); break child.wait()?; }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stdout_truncated) = out_t.join().unwrap_or_default();
    let (stderr, stderr_truncated) = err_t.join().unwrap_or_default();
    let exit_code = if timed_out { 124 } else { status.code().unwrap_or(128) };
    Ok(Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated })
}

/// The shell prelude that applies `limits`. Public so tests exercise the same code the sandbox runs.
pub fn limits_prelude(limits: &Limits) -> String { limits.ulimit_prelude() }
