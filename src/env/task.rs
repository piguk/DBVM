//! Tasks and actions, as the API receives them (JSON).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::sandbox::Limits;

/// What a rollout runs: a template, the paths the agent may not change, how it is judged, its limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub template_id: String,
    /// What the agent is asked to do (the prompt), handed to the agent by adapters (OpenEnv's reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    /// Directories or files the agent cannot change or create (mounted read-only from the template).
    #[serde(default)]
    pub readonly: Vec<String>,
    #[serde(default)]
    pub judge: Option<JudgeSpec>,
    #[serde(default)]
    pub limits: LimitsSpec,
    /// Default timeout of one action.
    #[serde(default = "default_step_timeout")]
    pub step_timeout_secs: u64,
    /// Extra environment for every command of the rollout.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The environment variable that carries a rollout's seed (PYTHONHASHSEED gets it too).
    #[serde(default = "default_seed_env")]
    pub seed_env: String,
    /// Files every rollout of the task starts with, over its template (the task's code, data, tests). They
    /// become one read-only layer the rollouts share; they are not the agent's changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<TaskFile>,
    /// Hosts the task's rollouts may reach (`host`, `host:port`, `.domain`), through the egress proxy: a
    /// license server, an internal API. None: no network.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceSpec>,
}

/// A file of a task: its content as text or (`encoding: base64`) base64, its mode (default 0644).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFile {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub encoding: Encoding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

fn default_step_timeout() -> u64 { 120 }
fn default_seed_env() -> String { "SEED".into() }

/// How a rollout is scored. The command runs in a fresh sandbox whose root is the pristine template plus
/// only the rollout's changes under `sees`: an agent that broke /usr or left processes running cannot
/// influence the judge. It prints a JSON object with a numeric `reward`; a non-zero exit is a judge failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeSpec {
    pub cmd: Vec<String>,
    #[serde(default = "default_judge_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub sees: Vec<String>,
    /// Start the task's services in the judge's sandbox first (to query the state the rollout left).
    #[serde(default)]
    pub services: bool,
    /// Files only the judge sees (hidden tests, expected results): never in a rollout, and above everything
    /// else in the judge's sandbox, so nothing the agent writes replaces them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<TaskFile>,
}

/// A process a task's rollouts run beside the agent (a database, a mock of an internal API): started
/// before the agent's first action, in every rollout and fork, and given `ready_timeout_secs` for `ready`
/// (a shell command) to succeed. Its output goes to /tmp/selfdb-services/<name>.log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    pub name: String,
    /// A shell command line that runs the service (in the foreground: it is started in the background).
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<String>,
    #[serde(default = "default_ready_timeout")]
    pub ready_timeout_secs: u64,
}

fn default_ready_timeout() -> u64 { 30 }

fn default_judge_timeout() -> u64 { 300 }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSpec {
    pub memory_mb: Option<u64>,
    pub pids: Option<u64>,
    pub cpu_percent: Option<u64>,
    pub address_space_mb: Option<u64>,
    pub cpu_secs: Option<u64>,
    pub file_size_mb: Option<u64>,
}

/// The largest limits a task may ask for, unless the node's operator sets others (`NodeConfig`). A tenant's
/// quotas bound sessions and storage; these bound one sandbox, so a task cannot claim a whole node.
pub const DEFAULT_MAX_LIMITS: Limits = Limits { memory_mb: 32 * 1024, pids: 8192, cpu_percent: 1600, address_space_mb: 64 * 1024, cpu_secs: 24 * 3600, file_size_mb: 16 * 1024 };

impl LimitsSpec {
    /// The sandbox defaults with this spec's values, each capped at `max`. 0 (no limit) is not accepted for a
    /// sandbox-wide limit: it would let one rollout exhaust the node.
    pub fn resolve(&self, max: &Limits) -> Limits {
        let d = Limits::default();
        let pick = |v: Option<u64>, def: u64, cap: u64| v.filter(|x| *x > 0).unwrap_or(def).min(cap);
        Limits {
            memory_mb: pick(self.memory_mb, d.memory_mb, max.memory_mb),
            pids: pick(self.pids, d.pids, max.pids),
            cpu_percent: pick(self.cpu_percent, d.cpu_percent, max.cpu_percent),
            address_space_mb: self.address_space_mb.unwrap_or(d.address_space_mb).min(max.address_space_mb),
            cpu_secs: self.cpu_secs.unwrap_or(d.cpu_secs).min(max.cpu_secs),
            file_size_mb: pick(self.file_size_mb, d.file_size_mb, max.file_size_mb),
        }
    }

}

/// One action of an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// A shell command line, run with /bin/sh -c.
    Bash { command: String, #[serde(default)] timeout_secs: Option<u64>, #[serde(default)] cwd: Option<String> },
    /// `output` is the content, as text or (`encoding: base64`) base64.
    ReadFile { path: String, #[serde(default)] offset: u64, #[serde(default)] limit: Option<u64>, #[serde(default)] encoding: Encoding },
    /// With `mode` the file gets exactly that mode; without, a new file is 0644 and an existing one keeps
    /// its mode. `content` is text or (`encoding: base64`) base64.
    WriteFile { path: String, content: String, #[serde(default)] mode: Option<u32>, #[serde(default)] encoding: Encoding },
    /// Replace exactly one occurrence of `old` with `new`.
    EditFile { path: String, old: String, new: String },
    ListDir { path: String },
}

/// How file contents travel in JSON: text, or base64 for any bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding { #[default] Utf8, Base64 }

impl Action {
    pub fn tool_name(&self) -> &'static str {
        match self {
            Action::Bash { .. } => "bash", Action::ReadFile { .. } => "read_file", Action::WriteFile { .. } => "write_file",
            Action::EditFile { .. } => "edit_file", Action::ListDir { .. } => "list_dir",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Changes { pub created: Vec<String>, pub modified: Vec<String>, pub deleted: Vec<String> }

impl From<crate::vm::SnapDiff> for Changes {
    fn from(d: crate::vm::SnapDiff) -> Changes { Changes { created: d.created, modified: d.modified, deleted: d.deleted } }
}

#[derive(Debug, Clone, Serialize)]
pub struct DirItem { pub name: String, pub kind: String, pub size: u64 }

/// What an action returned.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ActionResponse {
    pub step: u64,
    pub exit_code: i32,
    pub timed_out: bool,
    /// stdout of a command; the content read by read_file. UTF-8; invalid bytes replaced (see `lossy`).
    pub output: String,
    pub stderr: String,
    pub output_truncated: bool,
    pub stderr_truncated: bool,
    /// Some bytes of output or stderr were not UTF-8 and were replaced.
    pub lossy: bool,
    pub changes: Changes,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<DirItem>>,
}

/// The judge's verdict.
#[derive(Debug, Clone, Serialize)]
pub struct JudgeResult {
    /// None when the judge failed (non-zero exit, no JSON, no numeric `reward`).
    pub reward: Option<f64>,
    /// The judge's whole JSON object.
    pub details: serde_json::Value,
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl JudgeResult {
    /// Read the verdict from the judge's output: the whole stdout as a JSON object, or else its last
    /// non-empty line (judges often log before printing the result).
    pub fn parse(o: &crate::sandbox::Outcome) -> JudgeResult {
        let stdout = String::from_utf8_lossy(&o.stdout).to_string();
        let stderr = String::from_utf8_lossy(&o.stderr).to_string();
        let parsed: Option<serde_json::Value> = serde_json::from_str::<serde_json::Value>(stdout.trim()).ok()
            .filter(|v| v.is_object())
            .or_else(|| stdout.lines().rev().find(|l| !l.trim().is_empty()).and_then(|l| serde_json::from_str(l.trim()).ok()).filter(|v: &serde_json::Value| v.is_object()));
        let mut r = JudgeResult { reward: None, details: serde_json::Value::Null, exit_code: o.exit_code, timed_out: o.timed_out, stdout, stderr, error: None };
        if o.timed_out { r.error = Some("the judge timed out".into()); return r; }
        if o.exit_code != 0 { r.error = Some(format!("the judge exited with {}", o.exit_code)); return r; }
        match parsed {
            None => r.error = Some("the judge printed no JSON object".into()),
            Some(v) => {
                r.reward = v.get("reward").and_then(|x| x.as_f64());
                if r.reward.is_none() { r.error = Some("the judge's JSON has no numeric reward".into()); }
                r.details = v;
            }
        }
        r
    }
}
