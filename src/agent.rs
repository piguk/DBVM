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

/// Next snapshot name for this database, unique within it.
fn next_snapshot_name(conn: &rusqlite::Connection, prefix: &str) -> Result<String> {
    let n: i64 = conn.query_row("SELECT COALESCE(MAX(id),0)+1 FROM vm_snapshots", [], |r| r.get(0))?;
    Ok(format!("{}-{}", prefix, n))
}

pub fn run_step(db: &Path, argv: &[String], opts: &StepOptions) -> Result<StepResult> {
    if argv.is_empty() { return Err(anyhow!("empty command")); }
    if !bwrap_available() { return Err(anyhow!("bwrap is required for agent-step; refusing to run unisolated")); }
    let conn = vm::vm_open(db.to_str().ok_or_else(|| anyhow!("non-utf8 db path"))?)?;
    let before = next_snapshot_name(&conn, "step-before")?;
    vm::snapshot_create(&conn, &before, "agent-step")?;

    let scratch = vm::make_tmp_dir("agent-step-")?;
    let started = Instant::now();
    let mat = vm::vm_materialize_tree(&conn, &scratch);
    let outcome = match mat {
        Err(e) => { let _ = std::fs::remove_dir_all(&scratch); return Err(anyhow!("materialize failed: {}", e)); }
        Ok(_) => run_in(&scratch, argv, opts.timeout_secs, opts.max_output_bytes)?,
    };
    let Outcome { exit_code, timed_out, stdout, stderr, stdout_truncated, stderr_truncated } = outcome;

    // sync the scratch tree back, then diff against the snapshot taken before the step
    vm::vm_sync_from_host(&conn, &scratch)?;
    let changes = vm::snapshot_diff_live(&conn, &before)?;
    let _ = std::fs::remove_dir_all(&scratch);

    let failed = exit_code != 0 || timed_out;
    let rolled_back = failed && opts.rollback_on_fail;
    let after_name = if rolled_back {
        vm::snapshot_restore(&conn, &before)?;
        None
    } else {
        let a = next_snapshot_name(&conn, "step-after")?;
        vm::snapshot_create(&conn, &a, "agent-step")?;
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
        let items: Vec<String> = v.iter().map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))).collect();
        format!("[{}]", items.join(","))
    };
    format!("{{\"created\":{},\"modified\":{},\"deleted\":{}}}", arr(&d.created), arr(&d.modified), arr(&d.deleted))
}
