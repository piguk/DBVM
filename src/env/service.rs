//! The environment service: tenants' templates, tasks and live sessions. The HTTP layer (api.rs) only
//! authenticates and calls these methods; every check of ownership and quota is here.
//!
//! A tenant sees system templates and its own objects. Anything else answers "not found", never
//! "forbidden": the API does not confirm that another tenant's id exists.

use anyhow::anyhow;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::control::{ControlDb, Quotas, SessionUsage, Template, Tenant};
use super::store::Store;
use super::task::{Action, ActionResponse, DirItem, JudgeResult, TaskSpec};
use crate::egress::{EgressPolicy, EgressProxy};
use crate::rollout::{ActionResult, Rollout, RolloutOptions};
use crate::sandbox::{Limits, Outcome};
use crate::vm::WorkDir;

#[derive(Debug)]
pub enum EnvError { Unauthorized, NotFound(String), BadRequest(String), Quota(String), Internal(anyhow::Error) }

impl From<anyhow::Error> for EnvError { fn from(e: anyhow::Error) -> Self { EnvError::Internal(e) } }
impl From<rusqlite::Error> for EnvError { fn from(e: rusqlite::Error) -> Self { EnvError::Internal(e.into()) } }
impl From<std::io::Error> for EnvError { fn from(e: std::io::Error) -> Self { EnvError::Internal(e.into()) } }
impl From<serde_json::Error> for EnvError { fn from(e: serde_json::Error) -> Self { EnvError::Internal(e.into()) } }

pub type EnvResult<T> = std::result::Result<T, EnvError>;

/// The largest output an action returns per stream, and what read_file returns at most.
const MAX_OUTPUT: usize = 1 << 20;
const MAX_READ: u64 = 16 << 20;
/// The longest a single action may run.
const MAX_ACTION_SECS: u64 = 3600;
const MAX_EGRESS_HOSTS: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    /// A build session over a template (it may use `egress`), or
    pub template_id: Option<String>,
    /// a rollout of a task (no network).
    pub task_id: Option<String>,
    #[serde(default)]
    pub egress: Vec<String>,
    pub label: Option<String>,
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant { pub label: Option<String>, pub seed: Option<u64> }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind { Build, Rollout }

impl Kind { fn name(self) -> &'static str { match self { Kind::Build => "build", Kind::Rollout => "rollout" } } }

/// A running session.
struct Live {
    tenant: Tenant,
    kind: Kind,
    template: Template,
    task: Option<(String, TaskSpec)>,
    /// Layers above the template's: what forks froze from the session's ancestors.
    extra_layers: Vec<String>,
    label: Option<String>,
    seed: Option<u64>,
    env: Vec<(String, String)>,
    limits: Limits,
    egress: Vec<String>,
    rollout: Rollout,
    _proxy: Option<(EgressProxy, WorkDir)>,
    steps: u64,
    started: Instant,
    trajectory: std::fs::File,
}

/// A session table entry. What lookups need is kept beside the session, so finding a session never waits
/// for one that is busy running a long command.
struct Entry {
    tenant: String,
    template: String,
    started: Instant,
    max_secs: i64,
    live: Arc<Mutex<Live>>,
}

pub struct Service {
    store: Store,
    control: ControlDb,
    sessions: Mutex<HashMap<String, Entry>>,
    /// Held while a layer exists that no session row lists yet (a fork), and while unused layers are
    /// removed, so a fresh fork layer is never collected.
    layers_lock: Mutex<()>,
}

fn base_env(task: Option<&TaskSpec>, seed: Option<u64>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()),
        ("HOME".into(), "/root".into()),
        ("TZ".into(), "UTC".into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("LC_ALL".into(), "C.UTF-8".into()),
    ];
    if let Some(t) = task { for (k, v) in &t.env { env.retain(|(n, _)| n != k); env.push((k.clone(), v.clone())); } }
    if let Some(s) = seed {
        let var = task.map(|t| t.seed_env.clone()).unwrap_or_else(|| "SEED".into());
        for k in [var, "PYTHONHASHSEED".into()] { env.retain(|(n, _)| *n != k); env.push((k, s.to_string())); }
    }
    env
}

fn response(step: u64, r: ActionResult) -> ActionResponse {
    let out = String::from_utf8(r.outcome.stdout.clone());
    let err = String::from_utf8(r.outcome.stderr.clone());
    let lossy = out.is_err() || err.is_err();
    ActionResponse {
        step, exit_code: r.outcome.exit_code, timed_out: r.outcome.timed_out,
        output: out.unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string()),
        stderr: err.unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string()),
        output_truncated: r.outcome.stdout_truncated, stderr_truncated: r.outcome.stderr_truncated,
        lossy, changes: r.changes.into(), duration_ms: r.duration_ms, entries: None,
    }
}

fn failed(stderr: String) -> ActionResult {
    ActionResult { outcome: Outcome { exit_code: 1, stderr: stderr.into_bytes(), ..Outcome::default() }, changes: Default::default(), duration_ms: 0 }
}

fn check_path(p: &str) -> EnvResult<()> {
    if !p.starts_with('/') || p.split('/').any(|c| c == "..") || p.contains('\0') { return Err(EnvError::BadRequest(format!("path must be absolute, without '..': {:?}", p))); }
    Ok(())
}

/// Copy the entries `sees` of a layer (or upper) directory to `dest`, keeping whiteouts and markers.
fn copy_seen(src: &Path, sees: &[String], dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    for p in sees {
        let rel = p.trim_start_matches('/');
        let s = src.join(rel);
        if std::fs::symlink_metadata(&s).is_err() { continue; }
        let d = dest.join(rel);
        if let Some(parent) = d.parent() { std::fs::create_dir_all(parent)?; }
        let out = std::process::Command::new("cp").arg("-a").arg("--no-target-directory").arg(&s).arg(&d).output()?;
        if !out.status.success() { return Err(anyhow!("copy {}: {}", s.display(), String::from_utf8_lossy(&out.stderr).trim())); }
    }
    Ok(())
}

impl Service {
    pub fn open(data: &Path, node: &str) -> anyhow::Result<Arc<Service>> {
        let store = Store::open(data, node)?;
        let control = ControlDb::open(&data.join("control.db"))?;
        control.end_orphaned_sessions()?;
        let svc = Arc::new(Service { store, control, sessions: Mutex::new(HashMap::new()), layers_lock: Mutex::new(()) });
        // the rollouts of a previous run are gone; so are the layers only they used
        crate::vm::remove_tree(&svc.store.run_dir())?;
        std::fs::create_dir_all(svc.store.run_dir())?;
        svc.store.gc_layers(&svc.control)?;
        Ok(svc)
    }

    pub fn store(&self) -> &Store { &self.store }
    pub fn control(&self) -> &ControlDb { &self.control }

    /// Create a tenant (operator use, not part of the API).
    pub fn create_tenant(&self, name: &str, q: &Quotas) -> anyhow::Result<Tenant> {
        let id = self.store.new_id("ten")?;
        self.control.create_tenant(&id, name, q)
    }

    pub fn authenticate(&self, key: &str) -> EnvResult<Tenant> {
        self.control.tenant_for_key(key)?.ok_or(EnvError::Unauthorized)
    }

    fn visible_template(&self, t: &Tenant, id: &str) -> EnvResult<Template> {
        match self.control.template(id)? {
            Some(tp) if tp.tenant_id.is_none() || tp.tenant_id.as_deref() == Some(&t.id) => Ok(tp),
            _ => Err(EnvError::NotFound(format!("template {}", id))),
        }
    }

    pub fn list_templates(&self, t: &Tenant) -> EnvResult<Vec<Template>> { Ok(self.control.templates_for(&t.id)?) }

    pub fn delete_template(&self, t: &Tenant, id: &str) -> EnvResult<()> {
        let tp = self.visible_template(t, id)?;
        if tp.tenant_id.is_none() { return Err(EnvError::BadRequest("system templates cannot be deleted through the API".into())); }
        let in_use = self.sessions.lock().unwrap().values().any(|e| e.template == tp.id);
        if in_use { return Err(EnvError::BadRequest(format!("template {} is used by a running session", id))); }
        let _g = self.layers_lock.lock().unwrap();
        self.store.delete_template(&self.control, &tp)?;
        Ok(())
    }

    pub fn create_task(&self, t: &Tenant, spec: TaskSpec) -> EnvResult<String> {
        self.visible_template(t, &spec.template_id)?;
        for p in &spec.readonly { check_path(p)?; }
        if let Some(j) = &spec.judge {
            if j.cmd.is_empty() { return Err(EnvError::BadRequest("judge.cmd is empty".into())); }
            for p in &j.sees { check_path(p)?; }
        }
        if spec.seed_env.is_empty() || !spec.seed_env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(EnvError::BadRequest("seed_env must be a variable name".into()));
        }
        let id = self.store.new_id("tsk")?;
        self.control.insert_task(&id, &t.id, &serde_json::to_string(&spec)?)?;
        Ok(id)
    }

    pub fn task(&self, t: &Tenant, id: &str) -> EnvResult<TaskSpec> {
        match self.control.task(id)? {
            Some((owner, spec)) if owner == t.id => Ok(serde_json::from_str(&spec)?),
            _ => Err(EnvError::NotFound(format!("task {}", id))),
        }
    }

    fn live(&self, t: &Tenant, id: &str) -> EnvResult<Arc<Mutex<Live>>> {
        match self.sessions.lock().unwrap().get(id) {
            Some(e) if e.tenant == t.id => Ok(e.live.clone()),
            _ => Err(EnvError::NotFound(format!("session {}", id))),
        }
    }

    fn live_count(&self, tenant: &str) -> usize {
        self.sessions.lock().unwrap().values().filter(|e| e.tenant == tenant).count()
    }

    pub fn create_session(&self, t: &Tenant, req: CreateSession) -> EnvResult<String> {
        match (&req.template_id, &req.task_id) {
            (Some(tid), None) => {
                let tp = self.visible_template(t, tid)?;
                if req.egress.len() > MAX_EGRESS_HOSTS { return Err(EnvError::BadRequest(format!("at most {} egress hosts", MAX_EGRESS_HOSTS))); }
                self.start(t, Kind::Build, tp, None, Vec::new(), req.label, req.seed, req.egress, None)
            }
            (None, Some(task_id)) => {
                if !req.egress.is_empty() { return Err(EnvError::BadRequest("rollouts have no network; egress is for build sessions".into())); }
                let spec = self.task(t, task_id)?;
                let tp = self.visible_template(t, &spec.template_id)?;
                self.start(t, Kind::Rollout, tp, Some((task_id.clone(), spec)), Vec::new(), req.label, req.seed, Vec::new(), None)
            }
            _ => Err(EnvError::BadRequest("give exactly one of template_id and task_id".into())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start(&self, t: &Tenant, kind: Kind, template: Template, task: Option<(String, TaskSpec)>, extra_layers: Vec<String>,
             label: Option<String>, seed: Option<u64>, egress: Vec<String>, parent: Option<&str>) -> EnvResult<String> {
        if self.live_count(&t.id) as i64 >= t.max_sessions {
            return Err(EnvError::Quota(format!("at most {} running sessions", t.max_sessions)));
        }
        let ids: Vec<String> = template.layers.iter().chain(extra_layers.iter()).cloned().collect();
        let layers = self.store.layer_paths(&self.control, &ids)?;
        let spec = task.as_ref().map(|(_, s)| s);
        let env = base_env(spec, seed);
        let limits = spec.map(|s| s.limits.resolve()).unwrap_or_default();
        let proxy = if !egress.is_empty() {
            let dir = WorkDir::create_in(&self.store.run_dir(), "egress-")?;
            Some((EgressProxy::start(dir.path(), EgressPolicy { allow: egress.clone() })?, dir))
        } else { None };
        let opts = RolloutOptions {
            limits, env: env.clone(), readonly: spec.map(|s| s.readonly.clone()).unwrap_or_default(),
            egress: proxy.as_ref().map(|(_, d)| d.path().to_path_buf()), work_root: Some(self.store.run_dir()),
        };
        let rollout = Rollout::start(&layers, &opts)?;
        let id = self.store.new_id("ses")?;
        let traj_path = self.store.trajectory_path(&t.id, &id);
        if let Some(p) = traj_path.parent() { std::fs::create_dir_all(p)?; }
        let trajectory = std::fs::File::create(&traj_path)?;
        self.control.insert_session(&id, &t.id, kind.name(), &template.id, task.as_ref().map(|(i, _)| i.as_str()), parent,
                                    label.as_deref(), seed, &extra_layers)?;
        let entry = Entry { tenant: t.id.clone(), template: template.id.clone(), started: Instant::now(), max_secs: t.max_session_secs, live: Arc::new(Mutex::new(Live {
            tenant: t.clone(), kind, template, task, extra_layers, label, seed, env, limits, egress, rollout,
            _proxy: proxy, steps: 0, started: Instant::now(), trajectory })) };
        self.sessions.lock().unwrap().insert(id.clone(), entry);
        Ok(id)
    }

    pub fn act(&self, t: &Tenant, id: &str, action: Action) -> EnvResult<ActionResponse> {
        let live = self.live(t, id)?;
        let mut l = live.lock().unwrap();
        let step_timeout = l.task.as_ref().map(|(_, s)| s.step_timeout_secs).unwrap_or(600);
        let mut entries = None;
        let result = match &action {
            Action::Bash { command, timeout_secs, cwd } => {
                let cwd = cwd.clone().unwrap_or_else(|| "/".into());
                check_path(&cwd)?;
                let secs = timeout_secs.unwrap_or(step_timeout).clamp(1, MAX_ACTION_SECS);
                l.rollout.exec(&["/bin/sh".into(), "-c".into(), command.clone()], &cwd, Duration::from_secs(secs), MAX_OUTPUT)?
            }
            Action::ReadFile { path, offset, limit } => {
                check_path(path)?;
                let started = Instant::now();
                match l.rollout.read_file(path, *offset, limit.unwrap_or(MAX_OUTPUT as u64).min(MAX_READ))? {
                    Ok(bytes) => ActionResult { outcome: Outcome { stdout: bytes, ..Outcome::default() }, changes: Default::default(), duration_ms: started.elapsed().as_millis() },
                    Err(msg) => failed(msg),
                }
            }
            Action::WriteFile { path, content, mode } => {
                check_path(path)?;
                l.rollout.write_file(path, content.as_bytes(), mode.unwrap_or(0o644) & 0o7777)?
            }
            Action::EditFile { path, old, new } => {
                check_path(path)?;
                if old.is_empty() { return Err(EnvError::BadRequest("edit_file.old is empty".into())); }
                match l.rollout.read_file(path, 0, MAX_READ)? {
                    Err(msg) => failed(msg),
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Err(_) => failed(format!("{} is not UTF-8 text", path)),
                        Ok(text) => match text.matches(old.as_str()).count() {
                            1 => l.rollout.write_file(path, text.replacen(old.as_str(), new, 1).as_bytes(), 0o644)?,
                            n => failed(format!("the text to replace occurs {} times in {}; it must occur exactly once", n, path)),
                        },
                    },
                }
            }
            Action::ListDir { path } => {
                check_path(path)?;
                let started = Instant::now();
                match l.rollout.list_dir(path)? {
                    Ok(items) => {
                        entries = Some(items.into_iter().map(|e| DirItem {
                            name: e.name, size: e.size,
                            kind: match e.kind { 'f' => "file", 'd' => "dir", 'l' => "symlink", _ => "other" }.into(),
                        }).collect());
                        ActionResult { outcome: Outcome::default(), changes: Default::default(), duration_ms: started.elapsed().as_millis() }
                    }
                    Err(msg) => failed(msg),
                }
            }
        };
        l.steps += 1;
        let mut resp = response(l.steps, result);
        resp.entries = entries;
        let line = serde_json::json!({"step": l.steps, "ts": crate::vm::now_secs(), "label": l.label, "seed": l.seed, "action": action, "response": resp});
        writeln!(l.trajectory, "{}", line)?;
        let over = l.rollout.upper_bytes() as i64 > l.tenant.max_session_disk_bytes;
        drop(l);
        if over {
            self.close(t, id, "disk quota")?;
            return Err(EnvError::Quota(format!("the session wrote more than {} bytes and was closed", t.max_session_disk_bytes)));
        }
        Ok(resp)
    }

    /// New sessions starting from `id`'s current state, one per variant. The parent keeps running.
    pub fn fork(&self, t: &Tenant, id: &str, variants: Vec<Variant>) -> EnvResult<Vec<String>> {
        if variants.is_empty() { return Err(EnvError::BadRequest("no variants".into())); }
        if self.live_count(&t.id) as i64 + variants.len() as i64 > t.max_sessions {
            return Err(EnvError::Quota(format!("at most {} running sessions", t.max_sessions)));
        }
        let _g = self.layers_lock.lock().unwrap();
        let live = self.live(t, id)?;
        let (kind, template, task, mut extra, egress, parent_seed) = {
            let l = live.lock().unwrap();
            let layer = self.store.new_layer(&self.control, Some(&t.id), &l.rollout.upper())?;
            let mut extra = l.extra_layers.clone();
            extra.push(layer.id);
            (l.kind, l.template.clone(), l.task.clone(), extra, l.egress.clone(), l.seed)
        };
        extra.shrink_to_fit();
        let mut ids = Vec::new();
        for v in variants {
            match self.start(t, kind, template.clone(), task.clone(), extra.clone(), v.label, v.seed.or(parent_seed), egress.clone(), Some(id)) {
                Ok(child) => ids.push(child),
                Err(e) => {
                    drop(_g);
                    for c in &ids { let _ = self.close(t, c, "fork failed"); }
                    return Err(e);
                }
            }
        }
        Ok(ids)
    }

    /// A template of `id`'s current state, owned by the tenant.
    pub fn freeze(&self, t: &Tenant, id: &str, name: &str) -> EnvResult<Template> {
        let live = self.live(t, id)?;
        let l = live.lock().unwrap();
        let need = l.rollout.upper_bytes() as i64 + l.template.bytes;
        if self.control.storage_bytes(&t.id)? + need > t.max_storage_bytes {
            return Err(EnvError::Quota(format!("storing this template would exceed {} bytes", t.max_storage_bytes)));
        }
        let _g = self.layers_lock.lock().unwrap();
        Ok(self.store.freeze(&self.control, &t.id, name, &l.template, &l.extra_layers, &l.rollout.upper())?)
    }

    /// Score `id` with its task's judge (see `JudgeSpec`).
    pub fn judge(&self, t: &Tenant, id: &str) -> EnvResult<JudgeResult> {
        let live = self.live(t, id)?;
        let mut l = live.lock().unwrap();
        let Some((_, spec)) = l.task.clone() else { return Err(EnvError::BadRequest("this session has no task".into())) };
        let Some(judge) = spec.judge.clone() else { return Err(EnvError::BadRequest("the task has no judge".into())) };
        // the pristine template, then only what the rollout changed under `sees`, layer by layer
        let work = WorkDir::create_in(&self.store.run_dir(), "judge-")?;
        let mut layers = self.store.layer_paths(&self.control, &l.template.layers)?;
        let own = self.store.layer_paths(&self.control, &l.extra_layers)?;
        for (i, src) in own.iter().chain(std::iter::once(&l.rollout.upper())).enumerate() {
            let dest = work.path().join(format!("seen{}", i));
            copy_seen(src, &judge.sees, &dest)?;
            layers.push(dest);
        }
        let opts = RolloutOptions { limits: l.limits, env: l.env.clone(), readonly: Vec::new(), egress: None, work_root: Some(self.store.run_dir()) };
        let mut sandbox = Rollout::start(&layers, &opts)?;
        let outcome = sandbox.exec(&judge.cmd, "/", Duration::from_secs(judge.timeout_secs.clamp(1, MAX_ACTION_SECS)), MAX_OUTPUT)?.outcome;
        drop(sandbox);
        let verdict = JudgeResult::parse(&outcome);
        let line = serde_json::json!({"step": l.steps, "ts": crate::vm::now_secs(), "label": l.label, "seed": l.seed, "judge": verdict});
        writeln!(l.trajectory, "{}", line)?;
        Ok(verdict)
    }

    /// The session's trajectory (JSON lines); available after the session has ended too.
    pub fn trajectory(&self, t: &Tenant, id: &str) -> EnvResult<Vec<u8>> {
        let path = self.store.trajectory_path(&t.id, id);
        if !path.is_file() { return Err(EnvError::NotFound(format!("session {}", id))); }
        Ok(std::fs::read(path)?)
    }

    pub fn close(&self, t: &Tenant, id: &str, reason: &str) -> EnvResult<()> {
        let entry = {
            let mut map = self.sessions.lock().unwrap();
            match map.get(id) { Some(e) if e.tenant == t.id => map.remove(id), _ => None }
        };
        let Some(Entry { live, .. }) = entry else { return Err(EnvError::NotFound(format!("session {}", id))) };
        let mut l = live.lock().unwrap();
        let cpu = l.rollout.cpu_usec();
        self.control.end_session(id, reason, l.started.elapsed().as_millis(), cpu, l.steps)?;
        drop(l);
        drop(live);
        let _g = self.layers_lock.lock().unwrap();
        self.store.gc_layers(&self.control)?;
        Ok(())
    }

    /// One rollout of `task_id` per variant.
    pub fn rollouts(&self, t: &Tenant, task_id: &str, variants: Vec<Variant>) -> EnvResult<Vec<String>> {
        if variants.is_empty() { return Err(EnvError::BadRequest("no variants".into())); }
        if self.live_count(&t.id) as i64 + variants.len() as i64 > t.max_sessions {
            return Err(EnvError::Quota(format!("at most {} running sessions", t.max_sessions)));
        }
        let mut ids = Vec::new();
        for v in variants {
            match self.create_session(t, CreateSession { template_id: None, task_id: Some(task_id.into()), egress: Vec::new(), label: v.label, seed: v.seed }) {
                Ok(id) => ids.push(id),
                Err(e) => { for c in &ids { let _ = self.close(t, c, "rollouts failed"); } return Err(e); }
            }
        }
        Ok(ids)
    }

    pub fn usage(&self, t: &Tenant) -> EnvResult<Vec<SessionUsage>> { Ok(self.control.usage(&t.id)?) }

    /// Close every session that has run longer than its tenant allows.
    pub fn reap(&self) {
        let expired: Vec<(String, String)> = self.sessions.lock().unwrap().iter()
            .filter(|(_, e)| e.started.elapsed().as_secs() as i64 > e.max_secs)
            .map(|(id, e)| (e.tenant.clone(), id.clone())).collect();
        for (tenant, id) in expired {
            if let Ok(Some(t)) = self.control.tenant(&tenant) { let _ = self.close(&t, &id, "max session time"); }
        }
    }

    /// Paths of the layers a session runs over (for tests and operators).
    pub fn session_layers(&self, t: &Tenant, id: &str) -> EnvResult<Vec<PathBuf>> {
        let live = self.live(t, id)?;
        let l = live.lock().unwrap();
        Ok(l.rollout.layers().to_vec())
    }
}
