//! The environment service: tenants' templates, tasks and live sessions. The HTTP layer (api.rs) only
//! authenticates and calls these methods; every check of ownership and quota is here.
//!
//! A tenant sees system templates and its own objects. Anything else answers "not found", never
//! "forbidden": the API does not confirm that another tenant's id exists.

use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::control::{Checkpoint, ControlDb, Quotas, SessionUsage, Template, Tenant};
use super::store::{Store, TemplateMeta, MAX_LAYERS};
use super::task::{Action, ActionResponse, DirItem, Encoding, JudgeResult, TaskSpec};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use crate::egress::{EgressPolicy, EgressProxy};
use crate::rollout::{ActionResult, Rollout, RolloutOptions};
use crate::sandbox::{Limits, Outcome};
use crate::vm::WorkDir;

#[derive(Debug)]
pub enum EnvError { Unauthorized, NotFound(String), BadRequest(String), Quota(String), Unavailable(String), Internal(anyhow::Error) }

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

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Variant { pub label: Option<String>, pub seed: Option<u64> }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind { Build, Rollout }

impl Kind {
    fn name(self) -> &'static str { match self { Kind::Build => "build", Kind::Rollout => "rollout" } }
    fn parse(s: &str) -> Kind { if s == "build" { Kind::Build } else { Kind::Rollout } }
}

/// A running session.
struct Live {
    tenant: Tenant,
    kind: Kind,
    template: Template,
    task: Option<TaskRef>,
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

/// A task as its sessions use it: its spec and the layers of its files.
#[derive(Clone)]
struct TaskRef { id: String, spec: TaskSpec, layers: Vec<String> }

/// At most this many bytes of files in a task (they are kept in the control database too).
const MAX_TASK_FILES_BYTES: usize = 32 << 20;

/// How a service runs as a node of a cluster (see `cluster`). The default is a single node on its own.
#[derive(Debug, Clone, Default)]
pub struct NodeConfig {
    /// At most this many sessions on the node, all tenants together.
    pub max_sessions: Option<i64>,
    /// The cluster's shared token: the coordinator and the other nodes present it. None: not in a cluster.
    pub cluster_token: Option<String>,
}

/// What a node reports to the coordinator.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeStatus {
    pub node: String,
    pub max_sessions: Option<i64>,
    /// Running sessions per tenant.
    pub running: HashMap<String, i64>,
    /// Stored bytes per tenant.
    pub storage: HashMap<String, i64>,
    /// The templates on the node, made here or fetched.
    pub templates: Vec<Template>,
}

/// What the sessions a request starts share: everything but their label and seed.
struct Plan<'a> {
    kind: Kind,
    template: &'a Template,
    task: &'a Option<TaskRef>,
    /// Layers above the template's (from forks).
    extra: &'a [String],
    egress: &'a [String],
    parent: Option<&'a str>,
}

/// Places in a tenant's session limit, held for sessions being started (`Service::reserve`). Each session
/// started takes one; the rest are given back on drop.
struct Reservation<'a> {
    svc: &'a Service,
    tenant: String,
    left: std::sync::atomic::AtomicI64,
}

impl Reservation<'_> {
    /// A session took its place: called with the session table locked, after the session was inserted.
    fn admit(&self) {
        self.left.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        *self.svc.reserved.lock().unwrap().entry(self.tenant.clone()).or_insert(0) -= 1;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let left = self.left.load(std::sync::atomic::Ordering::SeqCst);
        *self.svc.reserved.lock().unwrap().entry(self.tenant.clone()).or_insert(0) -= left;
    }
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
    /// Per tenant, sessions being started: counted against the tenant's limit with the running ones, so
    /// concurrent requests cannot start more than it allows. Locked after `sessions`, never before.
    reserved: Mutex<HashMap<String, i64>>,
    /// Held while a layer exists that no session row lists yet (a fork), and while unused layers are
    /// removed, so a fresh fork layer is never collected.
    layers_lock: Mutex<()>,
    node: NodeConfig,
    /// Held while a template is fetched from another node, so it is fetched once.
    fetch_lock: Mutex<()>,
    metrics: super::metrics::Metrics,
}

/// The environment of a session's commands: defaults, then the template's (an image's `Env`), then the
/// task's, then the seed.
fn base_env(template: &Template, task: Option<&TaskSpec>, seed: Option<u64>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()),
        ("HOME".into(), "/root".into()),
        ("TZ".into(), "UTC".into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("LC_ALL".into(), "C.UTF-8".into()),
    ];
    for (k, v) in template.env.iter().map(|(k, v)| (k, v)).chain(task.map(|t| t.env.iter()).into_iter().flatten()) {
        env.retain(|(n, _)| n != k);
        env.push((k.clone(), v.clone()));
    }
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

/// Whether a task is well formed (its template aside).
pub fn check_task(spec: &TaskSpec) -> EnvResult<()> {
    for p in &spec.readonly { check_path(p)?; }
    if let Some(j) = &spec.judge {
        if j.cmd.is_empty() { return Err(EnvError::BadRequest("judge.cmd is empty".into())); }
        for p in &j.sees { check_path(p)?; }
    }
    if spec.seed_env.is_empty() || !spec.seed_env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(EnvError::BadRequest("seed_env must be a variable name".into()));
    }
    task_files(spec)?;
    Ok(())
}

/// Whether two secrets are equal, in time that does not depend on where they differ (compares their digests).
pub fn same_secret(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};
    let (x, y) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    x.iter().zip(y.iter()).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

/// A task's files, decoded and checked: (path, bytes, mode).
fn task_files(spec: &TaskSpec) -> EnvResult<Vec<(String, Vec<u8>, u32)>> {
    let mut out: Vec<(String, Vec<u8>, u32)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut size = 0usize;
    for f in &spec.files {
        check_path(&f.path)?;
        let p = crate::vm::normalize_vm_path(&f.path);
        if p == "/" || !seen.insert(p.clone()) { return Err(EnvError::BadRequest(format!("files: {:?} is not a file path, or is given twice", f.path))); }
        let data = match f.encoding {
            Encoding::Utf8 => f.content.as_bytes().to_vec(),
            Encoding::Base64 => B64.decode(&f.content).map_err(|e| EnvError::BadRequest(format!("files: {}: not base64: {}", f.path, e)))?,
        };
        size += data.len();
        if size > MAX_TASK_FILES_BYTES { return Err(EnvError::BadRequest(format!("a task's files may hold at most {} bytes", MAX_TASK_FILES_BYTES))); }
        out.push((p, data, f.mode.unwrap_or(0o644) & 0o7777));
    }
    // a file cannot also be a directory of another
    for (p, _, _) in &out {
        let prefix = format!("{}/", p);
        if out.iter().any(|(q, _, _)| q.starts_with(&prefix)) { return Err(EnvError::BadRequest(format!("files: {} is a file and a directory", p))); }
    }
    Ok(out)
}

/// A layer holding a task's files (directories 0755).
fn write_task_files(dest: &Path, files: &[(String, Vec<u8>, u32)]) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dest)?;
    for (p, data, mode) in files {
        let host = dest.join(p.trim_start_matches('/'));
        if let Some(parent) = host.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(&host, data)?;
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(*mode))?;
    }
    Ok(())
}

/// Copy the entries `sees` of a layer (or upper) directory to `dest`, keeping whiteouts and markers.
fn copy_seen(src: &Path, sees: &[String], dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    for p in sees {
        let rel = p.trim_start_matches('/');
        let s = src.join(rel);
        if std::fs::symlink_metadata(&s).is_err() { continue; }
        crate::rollout::copy_layer(&s, &dest.join(rel))?;
    }
    Ok(())
}

impl Service {
    /// The service reads and copies what rollouts write, whatever modes the agent gave it: the process must
    /// have entered its owner namespace first (`access::enter_owner_namespace`).
    pub fn open(data: &Path, node: &str) -> anyhow::Result<Arc<Service>> { Self::open_node(data, node, NodeConfig::default()) }

    pub fn open_node(data: &Path, node: &str, cfg: NodeConfig) -> anyhow::Result<Arc<Service>> {
        if !crate::access::has_owner_access() {
            return Err(anyhow::anyhow!("the environment service needs access to its own files whatever their modes: call access::enter_owner_namespace at startup"));
        }
        let store = Store::open(data, node)?;
        let control = ControlDb::open(&data.join("control.db"))?;
        control.end_orphaned_sessions()?;
        let svc = Arc::new(Service { store, control, sessions: Mutex::new(HashMap::new()), reserved: Mutex::new(HashMap::new()),
                                 layers_lock: Mutex::new(()), node: cfg, fetch_lock: Mutex::new(()),
                                 metrics: Default::default() });
        // the rollouts of a previous run are gone; so are the layers only they used
        crate::vm::remove_tree(&svc.store.run_dir())?;
        std::fs::create_dir_all(svc.store.run_dir())?;
        svc.store.gc_layers(&svc.control)?;
        svc.store.sweep_unrecorded(&svc.control)?;
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

    /// Whether `token` is the cluster's token (compared in constant time, as digests).
    pub fn is_cluster_token(&self, token: &str) -> bool {
        self.node.cluster_token.as_deref().map(|t| same_secret(t, token)).unwrap_or(false)
    }

    /// The tenant a request from the coordinator acts for (it presents the cluster token).
    pub fn cluster_tenant(&self, token: &str, tenant: &str) -> EnvResult<Tenant> {
        if !self.is_cluster_token(token) { return Err(EnvError::Unauthorized); }
        self.control.tenant(tenant)?.ok_or(EnvError::Unauthorized)
    }

    // ---- as a node of a cluster: what the coordinator and the other nodes ask for ----

    pub fn put_tenant(&self, t: &Tenant) -> EnvResult<()> { Ok(self.control.put_tenant(t)?) }

    /// A task the coordinator has (same id): its spec, and the layer of its files, made here.
    pub fn put_task(&self, id: &str, tenant: &str, spec: TaskSpec) -> EnvResult<()> {
        if self.control.task(id)?.is_some() { return Ok(()); }
        let t = self.control.tenant(tenant)?.ok_or_else(|| EnvError::NotFound(format!("tenant {}", tenant)))?;
        check_task(&spec)?;
        self.store_task(id, &t, &spec)
    }

    /// Delete this node's copy of a task, unless a session or a checkpoint here uses it.
    pub fn drop_task(&self, id: &str) -> EnvResult<()> {
        let _g = self.layers_lock.lock().unwrap();
        if self.control.task_in_use(id)? { return Err(EnvError::BadRequest(format!("task {} is in use on node {}", id, self.store.node))); }
        self.control.delete_task(id)?;
        self.store.gc_layers(&self.control)?;
        Ok(())
    }

    /// Delete this node's template (or copy of one), unless a session, a task or a checkpoint here uses it.
    pub fn drop_template(&self, id: &str) -> EnvResult<()> {
        let Some(tp) = self.control.template(id)? else { return Ok(()) };
        if self.sessions.lock().unwrap().values().any(|e| e.template == tp.id) || self.control.template_has_tasks(id)? || self.control.template_has_checkpoints(id)? {
            return Err(EnvError::BadRequest(format!("template {} is in use on node {}", id, self.store.node)));
        }
        let _g = self.layers_lock.lock().unwrap();
        self.store.delete_template(&self.control, &tp)?;
        Ok(())
    }

    pub fn template_meta(&self, id: &str) -> EnvResult<TemplateMeta> {
        let tp = self.control.template(id)?.ok_or_else(|| EnvError::NotFound(format!("template {}", id)))?;
        Ok(self.store.template_meta(&self.control, &tp)?)
    }

    /// The path of template `id`'s database, or of layer `id`, for another node to fetch.
    pub fn template_db_path(&self, id: &str) -> EnvResult<PathBuf> {
        Ok(self.control.template(id)?.ok_or_else(|| EnvError::NotFound(format!("template {}", id)))?.db_path)
    }

    pub fn layer_path(&self, id: &str) -> EnvResult<PathBuf> {
        Ok(self.control.layer(id)?.ok_or_else(|| EnvError::NotFound(format!("layer {}", id)))?.path)
    }

    /// Fetch template `id` from the node at `source` (a base URL) unless this node has it.
    pub fn fetch_template(&self, id: &str, source: &str) -> EnvResult<Template> {
        let r = self.fetch_template_inner(id, source);
        self.metrics.inc("selfdb_template_fetches_total", &[("outcome", if r.is_ok() { "ok" } else { "error" })]);
        r
    }

    fn fetch_template_inner(&self, id: &str, source: &str) -> EnvResult<Template> {
        let _f = self.fetch_lock.lock().unwrap();
        if let Some(t) = self.control.template(id)? { return Ok(t); }
        let token = self.node.cluster_token.clone().ok_or_else(|| EnvError::BadRequest("this node is not in a cluster".into()))?;
        let get = |path: String| -> anyhow::Result<ureq::Response> {
            ureq::get(&format!("{}{}", source.trim_end_matches('/'), path)).set("Authorization", &format!("Bearer {}", token)).call()
                .map_err(|e| anyhow::anyhow!("fetch {} from {}: {}", path, source, e))
        };
        let meta: TemplateMeta = get(format!("/internal/v1/templates/{}", id))?.into_json()?;
        if meta.template.id != id { return Err(EnvError::Internal(anyhow::anyhow!("{} answered with template {}", source, meta.template.id))); }
        let d = self.store.download_template(&self.control, &meta,
            &|| Ok(Box::new(get(format!("/internal/v1/templates/{}/db", id))?.into_reader())),
            &|l| Ok(Box::new(get(format!("/internal/v1/layers/{}", l))?.into_reader())))?;
        let _g = self.layers_lock.lock().unwrap();
        Ok(self.store.register(&self.control, d)?)
    }

    /// The node's metrics (Prometheus text format).
    pub fn metrics_text(&self) -> String {
        let mut running: HashMap<(String, &'static str), i64> = HashMap::new();
        for e in self.sessions.lock().unwrap().values() {
            let kind = e.live.try_lock().map(|l| l.kind.name()).unwrap_or("busy");
            *running.entry((e.tenant.clone(), kind)).or_insert(0) += 1;
        }
        let running: Vec<_> = running.into_iter().map(|((t, k), n)| (vec![("tenant", t), ("kind", k.to_string())], n as f64)).collect();
        let storage: Vec<_> = self.control.tenant_ids().unwrap_or_default().into_iter()
            .filter_map(|t| self.control.storage_bytes(&t).ok().map(|b| (vec![("tenant", t)], b as f64))).collect();
        let layers = self.control.all_layers().unwrap_or_default();
        let gauges = [
            ("selfdb_sessions_running", "Running sessions, by tenant and kind (busy: in the middle of a request).", running),
            ("selfdb_node_max_sessions", "The node's session limit (absent: none).",
             self.node.max_sessions.map(|m| vec![(vec![], m as f64)]).unwrap_or_default()),
            ("selfdb_storage_bytes", "Bytes of templates, layers and tasks, by tenant.", storage),
            ("selfdb_layers", "Layers on the node.", vec![(vec![], layers.len() as f64)]),
            ("selfdb_layer_bytes", "Bytes of the layers on the node.", vec![(vec![], layers.iter().map(|l| l.bytes).sum::<i64>() as f64)]),
        ];
        self.metrics.render(&gauges)
    }

    pub fn status(&self) -> EnvResult<NodeStatus> {
        let mut running: HashMap<String, i64> = HashMap::new();
        for e in self.sessions.lock().unwrap().values() { *running.entry(e.tenant.clone()).or_insert(0) += 1; }
        let templates = self.control.all_templates()?;
        let mut storage = HashMap::new();
        for t in self.control.tenant_ids()? { storage.insert(t.clone(), self.control.storage_bytes(&t)?); }
        Ok(NodeStatus { node: self.store.node.clone(), max_sessions: self.node.max_sessions, running, storage, templates })
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
        if self.control.template_has_tasks(&tp.id)? {
            return Err(EnvError::BadRequest(format!("template {} is used by tasks; delete them first", id)));
        }
        if self.control.template_has_checkpoints(&tp.id)? {
            return Err(EnvError::BadRequest(format!("template {} is used by checkpoints; delete them first", id)));
        }
        let _g = self.layers_lock.lock().unwrap();
        self.store.delete_template(&self.control, &tp)?;
        Ok(())
    }

    pub fn create_task(&self, t: &Tenant, spec: TaskSpec) -> EnvResult<String> {
        self.visible_template(t, &spec.template_id)?;
        check_task(&spec)?;
        let id = self.store.new_id("tsk")?;
        self.store_task(&id, t, &spec)?;
        Ok(id)
    }

    /// Record a checked task as `id`, with the layer of its files.
    fn store_task(&self, id: &str, t: &Tenant, spec: &TaskSpec) -> EnvResult<()> {
        let files = task_files(spec)?;
        let size: usize = files.iter().map(|(_, d, _)| d.len()).sum();
        if self.control.storage_bytes(&t.id)? + size as i64 > t.max_storage_bytes {
            return Err(EnvError::Quota(format!("storing this task would exceed {} bytes", t.max_storage_bytes)));
        }
        let _g = self.layers_lock.lock().unwrap();
        let layers = if files.is_empty() { Vec::new() } else {
            vec![self.store.new_layer(&self.control, Some(&t.id), &|dest| write_task_files(dest, &files))?.id]
        };
        self.control.insert_task(id, &t.id, &serde_json::to_string(spec)?, &layers)?;
        Ok(())
    }

    pub fn task(&self, t: &Tenant, id: &str) -> EnvResult<TaskSpec> { Ok(self.own_task(t, id)?.spec) }

    fn own_task(&self, t: &Tenant, id: &str) -> EnvResult<TaskRef> {
        match self.control.task(id)? {
            Some((owner, spec, layers)) if owner == t.id => Ok(TaskRef { id: id.to_string(), spec: serde_json::from_str(&spec)?, layers }),
            _ => Err(EnvError::NotFound(format!("task {}", id))),
        }
    }

    /// Delete a task no running session and no checkpoint uses; its files' layer goes with it.
    pub fn delete_task(&self, t: &Tenant, id: &str) -> EnvResult<()> {
        self.own_task(t, id)?;
        let _g = self.layers_lock.lock().unwrap();
        if self.control.task_in_use(id)? { return Err(EnvError::BadRequest(format!("task {} is used by a running session or a checkpoint", id))); }
        self.control.delete_task(id)?;
        self.store.gc_layers(&self.control)?;
        Ok(())
    }

    fn live(&self, t: &Tenant, id: &str) -> EnvResult<Arc<Mutex<Live>>> {
        match self.sessions.lock().unwrap().get(id) {
            Some(e) if e.tenant == t.id => Ok(e.live.clone()),
            _ => Err(EnvError::NotFound(format!("session {}", id))),
        }
    }

    /// Hold `n` places in `t`'s session limit, or refuse if the running sessions and those being started
    /// leave fewer.
    fn reserve(&self, t: &Tenant, n: usize) -> EnvResult<Reservation<'_>> {
        let map = self.sessions.lock().unwrap();
        let mut reserved = self.reserved.lock().unwrap();
        let running = map.values().filter(|e| e.tenant == t.id).count() as i64;
        if let Some(cap) = self.node.max_sessions {
            if map.len() as i64 + reserved.values().sum::<i64>() + n as i64 > cap {
                return Err(EnvError::Unavailable(format!("node {} runs at most {} sessions", self.store.node, cap)));
            }
        }
        let r = reserved.entry(t.id.clone()).or_insert(0);
        if running + *r + n as i64 > t.max_sessions {
            return Err(EnvError::Quota(format!("at most {} running sessions", t.max_sessions)));
        }
        *r += n as i64;
        Ok(Reservation { svc: self, tenant: t.id.clone(), left: std::sync::atomic::AtomicI64::new(n as i64) })
    }

    pub fn create_session(&self, t: &Tenant, req: CreateSession) -> EnvResult<String> {
        match (&req.template_id, &req.task_id) {
            (Some(tid), None) => {
                let tp = self.visible_template(t, tid)?;
                if req.egress.len() > MAX_EGRESS_HOSTS { return Err(EnvError::BadRequest(format!("at most {} egress hosts", MAX_EGRESS_HOSTS))); }
                let slots = self.reserve(t, 1)?;
                let plan = Plan { kind: Kind::Build, template: &tp, task: &None, extra: &[], egress: &req.egress, parent: None };
                self.start(t, &slots, &plan, Variant { label: req.label, seed: req.seed })
            }
            (None, Some(task_id)) => {
                if !req.egress.is_empty() { return Err(EnvError::BadRequest("rollouts have no network; egress is for build sessions".into())); }
                let task = self.own_task(t, task_id)?;
                let tp = self.visible_template(t, &task.spec.template_id)?;
                let slots = self.reserve(t, 1)?;
                let task = Some(task);
                let plan = Plan { kind: Kind::Rollout, template: &tp, task: &task, extra: &[], egress: &[], parent: None };
                self.start(t, &slots, &plan, Variant { label: req.label, seed: req.seed })
            }
            _ => Err(EnvError::BadRequest("give exactly one of template_id and task_id".into())),
        }
    }

    /// Start one session of `plan`, in a place `slots` holds.
    fn start(&self, t: &Tenant, slots: &Reservation, plan: &Plan, v: Variant) -> EnvResult<String> {
        let started = Instant::now();
        let r = self.start_inner(t, slots, plan, v);
        if r.is_ok() {
            self.metrics.inc("selfdb_sessions_started_total", &[("kind", plan.kind.name())]);
            self.metrics.observe("selfdb_session_start_seconds", &[], started.elapsed().as_secs_f64());
        }
        r
    }

    fn start_inner(&self, t: &Tenant, slots: &Reservation, plan: &Plan, v: Variant) -> EnvResult<String> {
        let Plan { kind, template, task, extra: extra_layers, egress, parent } = *plan;
        let (label, seed) = (v.label, v.seed);
        let task_layers: &[String] = task.as_ref().map(|k| k.layers.as_slice()).unwrap_or(&[]);
        let ids: Vec<String> = template.layers.iter().chain(task_layers).chain(extra_layers.iter()).cloned().collect();
        let layers = self.store.layer_paths(&self.control, &ids)?;
        let spec = task.as_ref().map(|k| &k.spec);
        let env = base_env(template, spec, seed);
        let limits = spec.map(|s| s.limits.resolve()).unwrap_or_default();
        let proxy = if !egress.is_empty() {
            let dir = WorkDir::create_in(&self.store.run_dir(), "egress-")?;
            Some((EgressProxy::start(dir.path(), EgressPolicy { allow: egress.to_vec() })?, dir))
        } else { None };
        let opts = RolloutOptions {
            limits, env: env.clone(), readonly: spec.map(|s| s.readonly.clone()).unwrap_or_default(),
            egress: proxy.as_ref().map(|(_, d)| d.path().to_path_buf()), work_root: Some(self.store.run_dir()), ..RolloutOptions::default()
        };
        let rollout = Rollout::start(&layers, &opts)?;
        let id = self.store.new_id("ses")?;
        let traj_path = self.store.trajectory_path(&t.id, &id);
        if let Some(p) = traj_path.parent() { std::fs::create_dir_all(p)?; }
        let trajectory = std::fs::File::create(&traj_path)?;
        self.control.insert_session(&id, &t.id, kind.name(), &template.id, task.as_ref().map(|k| k.id.as_str()), parent,
                                    label.as_deref(), seed, extra_layers)?;
        let entry = Entry { tenant: t.id.clone(), template: template.id.clone(), started: Instant::now(), max_secs: t.max_session_secs, live: Arc::new(Mutex::new(Live {
            tenant: t.clone(), kind, template: template.clone(), task: task.clone(), extra_layers: extra_layers.to_vec(), label, seed, env, limits,
            egress: egress.to_vec(), rollout, _proxy: proxy, steps: 0, started: Instant::now(), trajectory })) };
        let mut map = self.sessions.lock().unwrap();
        map.insert(id.clone(), entry);
        slots.admit();
        Ok(id)
    }

    /// Start one session of `plan` per variant, several at once (each start waits mostly on its sandbox
    /// coming up). The results are in the variants' order.
    fn start_many(&self, t: &Tenant, slots: &Reservation, plan: &Plan, variants: Vec<Variant>) -> Vec<EnvResult<String>> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let n = variants.len();
        let workers = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).min(n).max(1);
        let next = AtomicUsize::new(0);
        let variants: Vec<Mutex<Option<Variant>>> = variants.into_iter().map(|v| Mutex::new(Some(v))).collect();
        let results: Vec<Mutex<Option<EnvResult<String>>>> = (0..n).map(|_| Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n { break; }
                    let v = variants[i].lock().unwrap().take().expect("each variant is started once");
                    *results[i].lock().unwrap() = Some(self.start(t, slots, plan, v));
                });
            }
        });
        results.into_iter().map(|r| r.into_inner().unwrap().expect("every variant was started")).collect()
    }

    /// All the sessions `results` started, or (closing them) the first error.
    fn all_started(&self, t: &Tenant, results: Vec<EnvResult<String>>, why: &str) -> EnvResult<Vec<String>> {
        if !results.iter().any(|r| r.is_err()) { return Ok(results.into_iter().map(|r| r.unwrap_or_default()).collect()); }
        let mut first = None;
        for r in results {
            match r {
                Ok(id) => { let _ = self.close(t, &id, why); }
                Err(e) => { if first.is_none() { first = Some(e); } }
            }
        }
        Err(first.expect("an error was seen"))
    }

    pub fn act(&self, t: &Tenant, id: &str, action: Action) -> EnvResult<ActionResponse> {
        let tool = action.tool_name();
        let started = Instant::now();
        let r = self.act_inner(t, id, action);
        let outcome = match &r { Ok(resp) if resp.exit_code == 0 => "ok", Ok(_) => "failed", Err(_) => "error" };
        self.metrics.inc("selfdb_actions_total", &[("tool", tool), ("outcome", outcome)]);
        self.metrics.observe("selfdb_action_seconds", &[("tool", tool)], started.elapsed().as_secs_f64());
        r
    }

    fn act_inner(&self, t: &Tenant, id: &str, action: Action) -> EnvResult<ActionResponse> {
        let live = self.live(t, id)?;
        let mut l = live.lock().unwrap();
        let step_timeout = l.task.as_ref().map(|k| k.spec.step_timeout_secs).unwrap_or(600);
        let mut entries = None;
        let result = match &action {
            Action::Bash { command, timeout_secs, cwd } => {
                let cwd = cwd.clone().unwrap_or_else(|| l.template.workdir.clone());
                check_path(&cwd)?;
                let secs = timeout_secs.unwrap_or(step_timeout).clamp(1, MAX_ACTION_SECS);
                l.rollout.exec(&["/bin/sh".into(), "-c".into(), command.clone()], &cwd, Duration::from_secs(secs), MAX_OUTPUT)?
            }
            Action::ReadFile { path, offset, limit, encoding } => {
                check_path(path)?;
                let started = Instant::now();
                match l.rollout.read_file(path, *offset, limit.unwrap_or(MAX_OUTPUT as u64).min(MAX_READ))? {
                    Ok(bytes) => ActionResult {
                        outcome: Outcome { stdout: match encoding { Encoding::Utf8 => bytes, Encoding::Base64 => B64.encode(bytes).into_bytes() }, ..Outcome::default() },
                        changes: Default::default(), duration_ms: started.elapsed().as_millis() },
                    Err(msg) => failed(msg),
                }
            }
            Action::WriteFile { path, content, mode, encoding } => {
                check_path(path)?;
                let data = match encoding {
                    Encoding::Utf8 => content.as_bytes().to_vec(),
                    Encoding::Base64 => B64.decode(content).map_err(|e| EnvError::BadRequest(format!("write_file.content is not base64: {}", e)))?,
                };
                l.rollout.write_file(path, &data, mode.map(|m| m & 0o7777))?
            }
            Action::EditFile { path, old, new } => {
                check_path(path)?;
                if old.is_empty() { return Err(EnvError::BadRequest("edit_file.old is empty".into())); }
                match l.rollout.read_file(path, 0, MAX_READ)? {
                    Err(msg) => failed(msg),
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Err(_) => failed(format!("{} is not UTF-8 text", path)),
                        Ok(text) => match text.matches(old.as_str()).count() {
                            1 => l.rollout.write_file(path, text.replacen(old.as_str(), new, 1).as_bytes(), None)?,
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

    /// The session's current files as layers above its template: its own layers and a copy of its upper
    /// directory (merged into one past MAX_LAYERS: each generation of forks adds a layer). Returns them and
    /// the bytes the new layers store. Called with the layers lock held: no row lists the new layers yet.
    fn capture(&self, t: &Tenant, l: &Live) -> EnvResult<(Vec<String>, i64)> {
        let layer = self.store.new_layer(&self.control, Some(&t.id), &|dest| l.rollout.snapshot_upper(dest))?;
        let mut bytes = layer.bytes;
        let mut extra = l.extra_layers.clone();
        extra.push(layer.id);
        if extra.len() > MAX_LAYERS {
            let paths = self.store.layer_paths(&self.control, &extra)?;
            let merged = self.store.new_layer(&self.control, Some(&t.id), &|dest| crate::rollout::merge_layers(&paths, dest))?;
            bytes += merged.bytes;
            extra = vec![merged.id];
        }
        Ok((extra, bytes))
    }

    /// New sessions starting from `id`'s current state, one per variant. The parent keeps running.
    pub fn fork(&self, t: &Tenant, id: &str, variants: Vec<Variant>) -> EnvResult<Vec<String>> {
        self.metrics.inc("selfdb_forks_total", &[]);
        if variants.is_empty() { return Err(EnvError::BadRequest("no variants".into())); }
        let slots = self.reserve(t, variants.len())?;
        let g = self.layers_lock.lock().unwrap();
        let live = self.live(t, id)?;
        let (kind, template, task, extra, egress, parent_seed) = {
            let l = live.lock().unwrap();
            let (extra, _) = self.capture(t, &l)?;
            (l.kind, l.template.clone(), l.task.clone(), extra, l.egress.clone(), l.seed)
        };
        let variants = variants.into_iter().map(|v| Variant { label: v.label, seed: v.seed.or(parent_seed) }).collect();
        let plan = Plan { kind, template: &template, task: &task, extra: &extra, egress: &egress, parent: Some(id) };
        let results = self.start_many(t, &slots, &plan, variants);
        // closing a failed batch collects layers, which takes the layers lock
        drop(g);
        self.all_started(t, results, "fork failed")
    }

    /// Keep `id`'s files as of now: sessions can start from the checkpoint later (`fork_checkpoint`), after
    /// this one has moved on or ended. Its layer counts against the tenant's storage until it is deleted.
    pub fn checkpoint(&self, t: &Tenant, id: &str) -> EnvResult<Checkpoint> {
        self.metrics.inc("selfdb_checkpoints_total", &[]);
        let live = self.live(t, id)?;
        let _g = self.layers_lock.lock().unwrap();
        let mut l = live.lock().unwrap();
        if self.control.storage_bytes(&t.id)? + l.rollout.upper_bytes() as i64 > t.max_storage_bytes {
            return Err(EnvError::Quota(format!("storing this checkpoint would exceed {} bytes", t.max_storage_bytes)));
        }
        let (layers, bytes) = self.capture(t, &l)?;
        let k = Checkpoint {
            id: self.store.new_id("ckp")?, tenant_id: t.id.clone(), session_id: id.to_string(), kind: l.kind.name().into(),
            template_id: l.template.id.clone(), task_id: l.task.as_ref().map(|k| k.id.clone()), egress: l.egress.clone(), layers,
            step: l.steps as i64, label: l.label.clone(), seed: l.seed, bytes, created_at: crate::vm::now_secs(),
        };
        self.control.insert_checkpoint(&k)?;
        let line = serde_json::json!({"step": l.steps, "ts": k.created_at, "label": l.label, "seed": l.seed, "checkpoint": k.id});
        writeln!(l.trajectory, "{}", line)?;
        Ok(k)
    }

    fn own_checkpoint(&self, t: &Tenant, id: &str) -> EnvResult<Checkpoint> {
        match self.control.checkpoint(id)? {
            Some(k) if k.tenant_id == t.id => Ok(k),
            _ => Err(EnvError::NotFound(format!("checkpoint {}", id))),
        }
    }

    /// `session`'s checkpoints (also once it has ended).
    pub fn checkpoints(&self, t: &Tenant, session: &str) -> EnvResult<Vec<Checkpoint>> { Ok(self.control.checkpoints_of(&t.id, session)?) }

    /// New sessions starting from checkpoint `id`, one per variant; a variant without a seed keeps the
    /// checkpointed session's.
    pub fn fork_checkpoint(&self, t: &Tenant, id: &str, variants: Vec<Variant>) -> EnvResult<Vec<String>> {
        if variants.is_empty() { return Err(EnvError::BadRequest("no variants".into())); }
        let k = self.own_checkpoint(t, id)?;
        let template = self.control.template(&k.template_id)?.ok_or_else(|| EnvError::NotFound(format!("template {}", k.template_id)))?;
        let task = match &k.task_id { Some(tid) => Some(self.own_task(t, tid)?), None => None };
        let slots = self.reserve(t, variants.len())?;
        let g = self.layers_lock.lock().unwrap();
        // deleted meanwhile: its layers may be gone
        if self.control.checkpoint(id)?.is_none() { return Err(EnvError::NotFound(format!("checkpoint {}", id))); }
        let variants = variants.into_iter().map(|v| Variant { label: v.label, seed: v.seed.or(k.seed) }).collect();
        let plan = Plan { kind: Kind::parse(&k.kind), template: &template, task: &task, extra: &k.layers, egress: &k.egress, parent: Some(&k.session_id) };
        let results = self.start_many(t, &slots, &plan, variants);
        drop(g);
        self.all_started(t, results, "fork failed")
    }

    /// Delete a checkpoint; its layers go when nothing else uses them.
    pub fn delete_checkpoint(&self, t: &Tenant, id: &str) -> EnvResult<()> {
        self.own_checkpoint(t, id)?;
        let _g = self.layers_lock.lock().unwrap();
        self.control.delete_checkpoint(id)?;
        self.store.gc_layers(&self.control)?;
        Ok(())
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
        // the task's files are part of what the session shows
        let below: Vec<String> = l.task.iter().flat_map(|k| k.layers.iter()).chain(&l.extra_layers).cloned().collect();
        Ok(self.store.freeze(&self.control, &t.id, name, &l.template, &below, &|dest| l.rollout.snapshot_upper(dest))?)
    }

    /// Score `id` with its task's judge (see `JudgeSpec`).
    pub fn judge(&self, t: &Tenant, id: &str) -> EnvResult<JudgeResult> {
        let r = self.judge_inner(t, id);
        let outcome = match &r { Ok(j) if j.reward.is_some() => "scored", Ok(_) => "failed", Err(_) => "error" };
        self.metrics.inc("selfdb_judgements_total", &[("outcome", outcome)]);
        r
    }

    fn judge_inner(&self, t: &Tenant, id: &str) -> EnvResult<JudgeResult> {
        let live = self.live(t, id)?;
        let mut l = live.lock().unwrap();
        let Some(task) = l.task.clone() else { return Err(EnvError::BadRequest("this session has no task".into())) };
        let spec = task.spec;
        let Some(judge) = spec.judge.clone() else { return Err(EnvError::BadRequest("the task has no judge".into())) };
        // the pristine template and task files, then only what the rollout changed under `sees`, layer by layer
        let work = WorkDir::create_in(&self.store.run_dir(), "judge-")?;
        let pristine: Vec<String> = l.template.layers.iter().chain(&task.layers).cloned().collect();
        let mut layers = self.store.layer_paths(&self.control, &pristine)?;
        let own = self.store.layer_paths(&self.control, &l.extra_layers)?;
        {
            // the rollout's processes keep running between actions; its upper is copied as one moment
            let _frozen = l.rollout.freeze()?;
            for (i, src) in own.iter().chain(std::iter::once(&l.rollout.upper())).enumerate() {
                let dest = work.path().join(format!("seen{}", i));
                copy_seen(src, &judge.sees, &dest)?;
                layers.push(dest);
            }
        }
        let opts = RolloutOptions { limits: l.limits, env: l.env.clone(), readonly: Vec::new(), egress: None, work_root: Some(self.store.run_dir()), ..RolloutOptions::default() };
        let mut sandbox = Rollout::start(&layers, &opts)?;
        let outcome = sandbox.exec(&judge.cmd, &l.template.workdir, Duration::from_secs(judge.timeout_secs.clamp(1, MAX_ACTION_SECS)), MAX_OUTPUT)?.outcome;
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
        self.metrics.inc("selfdb_sessions_ended_total", &[("reason", reason)]);
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
        let task = self.own_task(t, task_id)?;
        let tp = self.visible_template(t, &task.spec.template_id)?;
        let slots = self.reserve(t, variants.len())?;
        let task = Some(task);
        let plan = Plan { kind: Kind::Rollout, template: &tp, task: &task, extra: &[], egress: &[], parent: None };
        let results = self.start_many(t, &slots, &plan, variants);
        self.all_started(t, results, "rollouts failed")
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
