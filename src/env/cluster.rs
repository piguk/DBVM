//! The coordinator of a cluster: one public `/v1` API over several nodes.
//!
//! The coordinator keeps the tenants, API keys and tasks (`control.db`) and the list of nodes; the nodes keep
//! everything else and run the sessions. Clients talk to the coordinator only:
//! - a request about a session or a checkpoint goes to the node its id names (`<node>-ses-...`);
//! - a request that starts sessions is placed on nodes with room, those holding the template first; a node
//!   lacking the template fetches it from one that has it (`transfer`), the tenant and the task are pushed
//!   to it first;
//! - the template list is what the nodes report; usage is gathered from every node.
//!
//! Nodes register themselves (`env-serve --coordinator`) and again every few seconds, so a restarted
//! coordinator learns its nodes back without a list of its own; it polls each node's status every two
//! seconds. Both directions authenticate with the cluster token; the coordinator forwards a client's
//! request with the token and the tenant (`X-Selfdb-Tenant`): API keys never reach the nodes.
//!
//! A tenant's running sessions are counted as the nodes last reported them plus the sessions started since
//! and those being started, which can only overcount: the session limit holds across nodes. Storage limits
//! are applied by each node to what it stores, so a tenant's total over several nodes can exceed its limit.

use anyhow::anyhow;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::api::bearer;
use super::control::{ControlDb, Template, Tenant};
use super::service::{check_task, same_secret, EnvError, NodeStatus, Variant};
use super::task::TaskSpec;

type EnvResult<T> = Result<T, EnvError>;

/// After this many failed polls in a row a node gets no new sessions.
const MAX_FAILED_POLLS: u32 = 2;

struct Node {
    url: String,
    max_sessions: Option<i64>,
    status: Option<NodeStatus>,
    /// When the request for `status` was sent: sessions started after it are not in it.
    polled_at: Instant,
    /// Sessions started on the node since `polled_at`: (when, tenant, how many).
    started: Vec<(Instant, String, i64)>,
    /// Places held for sessions being started on the node.
    pending: i64,
    failed_polls: u32,
}

impl Node {
    fn healthy(&self) -> bool { self.status.is_some() && self.failed_polls < MAX_FAILED_POLLS }

    fn running(&self, tenant: Option<&str>) -> i64 {
        let reported: i64 = match (&self.status, tenant) {
            (Some(s), Some(t)) => s.running.get(t).copied().unwrap_or(0),
            (Some(s), None) => s.running.values().sum(),
            (None, _) => 0,
        };
        reported + self.started.iter().filter(|(_, t, _)| tenant.map(|x| x == t).unwrap_or(true)).map(|(_, _, n)| n).sum::<i64>()
    }

    fn free(&self) -> i64 { self.max_sessions.unwrap_or(i64::MAX / 4) - self.running(None) - self.pending }

    fn has_template(&self, id: &str) -> bool { self.status.as_ref().map(|s| s.templates.iter().any(|t| t.id == id)).unwrap_or(false) }
}

pub struct Coordinator {
    /// The prefix of the ids the coordinator makes (tasks).
    id: String,
    control: ControlDb,
    token: String,
    nodes: Mutex<HashMap<String, Node>>,
    /// Per tenant, places held by requests starting sessions. Locked after `nodes`.
    reserved: Mutex<HashMap<String, i64>>,
    /// (node, tenant) pairs the node has been told about (`put_tenant`).
    told: Mutex<std::collections::HashSet<(String, String)>>,
    metrics: super::metrics::Metrics,
    http: ureq::Agent,
}

/// Places in a tenant's session limit and on nodes, held while sessions start.
struct Hold<'a> {
    c: &'a Coordinator,
    tenant: String,
    /// node -> places held there
    nodes: Mutex<HashMap<String, i64>>,
}

impl Hold<'_> {
    /// `n` sessions started on `node`: they count as running there now.
    fn started(&self, node: &str, n: i64) {
        let mut nodes = self.c.nodes.lock().unwrap();
        let mut reserved = self.c.reserved.lock().unwrap();
        let mut held = self.nodes.lock().unwrap();
        let h = held.entry(node.to_string()).or_insert(0);
        let n = n.min(*h);
        *h -= n;
        *reserved.entry(self.tenant.clone()).or_insert(0) -= n;
        if let Some(nd) = nodes.get_mut(node) {
            nd.pending -= n;
            nd.started.push((Instant::now(), self.tenant.clone(), n));
        }
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let mut nodes = self.c.nodes.lock().unwrap();
        let mut reserved = self.c.reserved.lock().unwrap();
        for (node, n) in self.nodes.lock().unwrap().drain() {
            *reserved.entry(self.tenant.clone()).or_insert(0) -= n;
            if let Some(nd) = nodes.get_mut(&node) { nd.pending -= n; }
        }
    }
}

/// A node's answer, passed on to the client as it is.
struct Answer { status: u16, body: Vec<u8>, content_type: Option<String> }

impl Answer {
    fn ok(&self) -> bool { (200..300).contains(&self.status) }

    fn json<T: serde::de::DeserializeOwned>(&self) -> EnvResult<T> {
        serde_json::from_slice(&self.body).map_err(|e| EnvError::Internal(anyhow!("a node answered {}: {}", self.status, e)))
    }

    /// The node's error, as the coordinator's.
    fn error(&self) -> EnvError {
        let msg = serde_json::from_slice::<serde_json::Value>(&self.body).ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).into_owned());
        match self.status {
            401 => EnvError::Internal(anyhow!("a node refused the cluster token")),
            404 => EnvError::BadRequest(msg),
            400 => EnvError::BadRequest(msg),
            429 => EnvError::Quota(msg),
            503 => EnvError::Unavailable(msg),
            _ => EnvError::Internal(anyhow!("{}", msg)),
        }
    }

    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let ct = self.content_type.unwrap_or_else(|| "application/json".into());
        (code, [("content-type", ct)], self.body).into_response()
    }
}

#[derive(Deserialize)]
pub struct Register { pub id: String, pub url: String, #[serde(default)] pub max_sessions: Option<i64> }

#[derive(Serialize)]
struct NodeView { id: String, url: String, healthy: bool, max_sessions: Option<i64>, running: i64, templates: usize }

/// The node an object id names (`<node>-<kind>-<hex>`).
fn node_of(id: &str) -> &str { id.split('-').next().unwrap_or("") }

impl Coordinator {
    pub fn open(data: &std::path::Path, id: &str, token: String) -> anyhow::Result<Arc<Coordinator>> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) { return Err(anyhow!("the coordinator id must be alphanumeric")); }
        std::fs::create_dir_all(data)?;
        std::fs::set_permissions(data, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        let control = ControlDb::open(&data.join("control.db"))?;
        let http = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).build();
        Ok(Arc::new(Coordinator { id: id.into(), control, token, nodes: Mutex::new(HashMap::new()), reserved: Mutex::new(HashMap::new()), told: Mutex::new(Default::default()), metrics: Default::default(), http }))
    }

    pub fn control(&self) -> &ControlDb { &self.control }

    fn authenticate(&self, key: Option<&str>) -> EnvResult<Tenant> {
        self.control.tenant_for_key(key.ok_or(EnvError::Unauthorized)?)?.ok_or(EnvError::Unauthorized)
    }

    fn is_cluster_token(&self, key: Option<&str>) -> bool { key.map(|k| same_secret(&self.token, k)).unwrap_or(false) }

    // ---- nodes ----

    pub fn register(&self, r: Register) -> EnvResult<()> {
        if r.id.is_empty() || !r.id.chars().all(|c| c.is_ascii_alphanumeric()) || r.id == self.id {
            return Err(EnvError::BadRequest(format!("bad node id {:?}", r.id)));
        }
        let fresh = {
            let mut nodes = self.nodes.lock().unwrap();
            let fresh = !nodes.contains_key(&r.id);
            let n = nodes.entry(r.id.clone()).or_insert_with(|| Node {
                url: r.url.clone(), max_sessions: r.max_sessions, status: None, polled_at: Instant::now(), started: Vec::new(), pending: 0, failed_polls: 0,
            });
            n.url = r.url.trim_end_matches('/').to_string();
            n.max_sessions = r.max_sessions;
            fresh
        };
        if fresh { self.poll(&r.id); }
        Ok(())
    }

    /// Ask a node for its status.
    fn poll(&self, id: &str) {
        let Some(url) = self.nodes.lock().unwrap().get(id).map(|n| n.url.clone()) else { return };
        let sent = Instant::now();
        let r = self.http.get(&format!("{}/internal/v1/status", url)).set("Authorization", &format!("Bearer {}", self.token))
            .timeout(Duration::from_secs(5)).call().map_err(|e| e.to_string()).and_then(|r| r.into_json::<NodeStatus>().map_err(|e| e.to_string()));
        let mut nodes = self.nodes.lock().unwrap();
        let Some(n) = nodes.get_mut(id) else { return };
        match r {
            Ok(s) if s.node == id => {
                n.status = Some(s);
                n.polled_at = sent;
                n.started.retain(|(t, _, _)| *t >= sent);
                n.failed_polls = 0;
            }
            _ => n.failed_polls += 1,
        }
    }

    pub fn poll_all(&self) {
        let ids: Vec<String> = self.nodes.lock().unwrap().keys().cloned().collect();
        for id in ids { self.poll(&id); }
    }

    fn node_url(&self, id: &str) -> EnvResult<String> {
        match self.nodes.lock().unwrap().get(id) {
            Some(n) if n.healthy() => Ok(n.url.clone()),
            Some(_) => Err(EnvError::Unavailable(format!("node {} is unavailable", id))),
            None => Err(EnvError::NotFound(format!("node {}", id))),
        }
    }

    fn nodes_view(&self) -> Vec<NodeView> {
        let nodes = self.nodes.lock().unwrap();
        let mut v: Vec<NodeView> = nodes.iter().map(|(id, n)| NodeView {
            id: id.clone(), url: n.url.clone(), healthy: n.healthy(), max_sessions: n.max_sessions, running: n.running(None),
            templates: n.status.as_ref().map(|s| s.templates.len()).unwrap_or(0),
        }).collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Send a request to a node, as `tenant` (or as the cluster, for its own routes).
    fn send(&self, node: &str, method: &str, path: &str, tenant: Option<&str>, body: Option<&serde_json::Value>) -> EnvResult<Answer> {
        let url = self.node_url(node)?;
        let mut req = self.http.request(method, &format!("{}{}", url, path)).set("Authorization", &format!("Bearer {}", self.token));
        if let Some(t) = tenant { req = req.set("X-Selfdb-Tenant", t); }
        let res = match body { Some(b) => req.send_json(b), None => req.call() };
        let resp = match res {
            Ok(r) => { self.metrics.inc("selfdb_forwarded_total", &[("outcome", "ok")]); r }
            Err(ureq::Error::Status(_, r)) => { self.metrics.inc("selfdb_forwarded_total", &[("outcome", "error")]); r }
            Err(e) => {
                self.metrics.inc("selfdb_forwarded_total", &[("outcome", "unreachable")]);
                return Err(EnvError::Unavailable(format!("node {}: {}", node, e)));
            }
        };
        let status = resp.status();
        let content_type = resp.header("content-type").map(str::to_string);
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut body).map_err(|e| EnvError::Unavailable(format!("node {}: {}", node, e)))?;
        Ok(Answer { status, body, content_type })
    }

    /// `send`, with a node's error as the coordinator's.
    fn send_ok<T: serde::de::DeserializeOwned>(&self, node: &str, method: &str, path: &str, tenant: Option<&str>, body: Option<&serde_json::Value>) -> EnvResult<T> {
        let a = self.send(node, method, path, tenant, body)?;
        if !a.ok() { return Err(a.error()); }
        a.json()
    }

    /// The coordinator's metrics (Prometheus text format).
    pub fn metrics_text(&self) -> String {
        let nodes = self.nodes.lock().unwrap();
        let per = |f: &dyn Fn(&Node) -> f64| -> Vec<(Vec<(&'static str, String)>, f64)> { nodes.iter().map(|(k, n)| (vec![("node", k.clone())], f(n))).collect() };
        let gauges = [
            ("selfdb_node_up", "Whether the node answered its last polls.", per(&|n| if n.healthy() { 1.0 } else { 0.0 })),
            ("selfdb_node_sessions", "Sessions on the node, as last reported and started since.", per(&|n| n.running(None) as f64)),
            ("selfdb_node_free", "Sessions the node has room for.", per(&|n| n.free().min(1 << 40) as f64)),
        ];
        drop(nodes);
        self.metrics.render(&gauges)
    }

    // ---- templates ----

    /// Every template of the cluster, from where it was made (its id names that node), else any copy.
    fn templates(&self) -> HashMap<String, Template> {
        let nodes = self.nodes.lock().unwrap();
        let mut out: HashMap<String, Template> = HashMap::new();
        for (id, n) in nodes.iter() {
            for t in n.status.iter().flat_map(|s| s.templates.iter()) {
                if node_of(&t.id) == id || !out.contains_key(&t.id) { out.insert(t.id.clone(), t.clone()); }
            }
        }
        out
    }

    fn visible_template(&self, t: &Tenant, id: &str) -> EnvResult<Template> {
        match self.templates().remove(id) {
            Some(tp) if tp.tenant_id.is_none() || tp.tenant_id.as_deref() == Some(&t.id) => Ok(tp),
            _ => Err(EnvError::NotFound(format!("template {}", id))),
        }
    }

    fn list_templates(&self, t: &Tenant) -> Vec<Template> {
        let mut v: Vec<Template> = self.templates().into_values().filter(|tp| tp.tenant_id.is_none() || tp.tenant_id.as_deref() == Some(&t.id)).collect();
        v.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        v
    }

    fn delete_template(&self, t: &Tenant, id: &str) -> EnvResult<()> {
        let tp = self.visible_template(t, id)?;
        if tp.tenant_id.is_none() { return Err(EnvError::BadRequest("system templates cannot be deleted through the API".into())); }
        if self.control.template_has_tasks(id)? { return Err(EnvError::BadRequest(format!("template {} is used by tasks; delete them first", id))); }
        let mut holders: Vec<String> = self.nodes.lock().unwrap().iter().filter(|(_, n)| n.has_template(id)).map(|(k, _)| k.clone()).collect();
        // the node that made it last: copies go first, so a refusal leaves the original
        holders.sort_by_key(|n| n == node_of(id));
        for n in holders {
            let _: serde_json::Value = self.send_ok(&n, "DELETE", &format!("/internal/v1/templates/{}", id), None, None)?;
            if let Some(s) = self.nodes.lock().unwrap().get_mut(&n).and_then(|x| x.status.as_mut()) { s.templates.retain(|x| x.id != id); }
        }
        Ok(())
    }

    // ---- tasks ----

    fn create_task(&self, t: &Tenant, spec: TaskSpec) -> EnvResult<String> {
        self.visible_template(t, &spec.template_id)?;
        check_task(&spec)?;
        let id = format!("{}-tsk-{}", self.id, super::control::random_hex(8)?);
        self.control.insert_task(&id, &t.id, &serde_json::to_string(&spec)?, &[], &[])?;
        Ok(id)
    }

    fn task(&self, t: &Tenant, id: &str) -> EnvResult<TaskSpec> {
        match self.control.task(id)? {
            Some(row) if row.tenant == t.id => Ok(serde_json::from_str(&row.spec)?),
            _ => Err(EnvError::NotFound(format!("task {}", id))),
        }
    }

    fn delete_task(&self, t: &Tenant, id: &str) -> EnvResult<()> {
        self.task(t, id)?;
        let nodes: Vec<(String, bool)> = self.nodes.lock().unwrap().iter().map(|(k, n)| (k.clone(), n.healthy())).collect();
        if let Some((down, _)) = nodes.iter().find(|(_, h)| !h) {
            return Err(EnvError::Unavailable(format!("node {} is unavailable: it may use the task", down)));
        }
        for (n, _) in &nodes {
            let _: serde_json::Value = self.send_ok(n, "DELETE", &format!("/internal/v1/tasks/{}", id), None, None)?;
        }
        self.control.delete_task(id)?;
        Ok(())
    }

    // ---- placing sessions ----

    /// Hold places for `n` sessions of `t` from `template`: in the tenant's limit, and on nodes with room
    /// (those holding the template first, then the emptiest). Returns the hold and the nodes with counts.
    fn place(&self, t: &Tenant, template: Option<&str>, n: usize, only: Option<&str>) -> EnvResult<(Hold<'_>, Vec<(String, usize)>)> {
        let mut nodes = self.nodes.lock().unwrap();
        let mut reserved = self.reserved.lock().unwrap();
        let running: i64 = nodes.values().map(|x| x.running(Some(&t.id))).sum();
        let r = reserved.entry(t.id.clone()).or_insert(0);
        if running + *r + n as i64 > t.max_sessions {
            self.metrics.inc("selfdb_placement_refusals_total", &[("reason", "quota")]);
            return Err(EnvError::Quota(format!("at most {} running sessions", t.max_sessions)));
        }
        let mut cands: Vec<(&String, &Node)> = nodes.iter().filter(|(k, x)| x.healthy() && only.map(|o| o == k.as_str()).unwrap_or(true)).collect();
        cands.sort_by_key(|(k, x)| (!template.map(|tp| x.has_template(tp)).unwrap_or(false), -x.free(), (*k).clone()));
        let mut plan = Vec::new();
        let mut left = n as i64;
        for (k, x) in cands {
            if left == 0 { break; }
            let take = x.free().min(left);
            if take > 0 { plan.push((k.clone(), take as usize)); left -= take; }
        }
        if left > 0 {
            self.metrics.inc("selfdb_placement_refusals_total", &[("reason", "capacity")]);
            return Err(EnvError::Unavailable(format!("the cluster has room for {} more sessions, not {}", n as i64 - left, n)));
        }
        *r += n as i64;
        let mut held = HashMap::new();
        for (k, c) in &plan {
            nodes.get_mut(k).unwrap().pending += *c as i64;
            held.insert(k.clone(), *c as i64);
        }
        Ok((Hold { c: self, tenant: t.id.clone(), nodes: Mutex::new(held) }, plan))
    }

    /// Tell `node` about tenant `t` (its quotas as they are now).
    fn put_tenant(&self, node: &str, t: &Tenant) -> EnvResult<()> {
        let _: serde_json::Value = self.send_ok(node, "PUT", &format!("/internal/v1/tenants/{}", t.id), None, Some(&serde_json::to_value(t).map_err(anyhow::Error::from)?))?;
        self.told.lock().unwrap().insert((node.to_string(), t.id.clone()));
        Ok(())
    }

    /// `put_tenant` unless the node knows the tenant already: before passing a request on, so that the
    /// node answers "not found" for another tenant's object rather than refusing an unknown tenant.
    fn ensure_tenant(&self, node: &str, t: &Tenant) -> EnvResult<()> {
        if self.told.lock().unwrap().contains(&(node.to_string(), t.id.clone())) { return Ok(()); }
        self.put_tenant(node, t)
    }

    /// Get `node` ready for sessions of `t`: the tenant, the template (fetched from a node that has it) and
    /// the task.
    fn prepare(&self, node: &str, t: &Tenant, template: &str, task: Option<(&str, &TaskSpec)>) -> EnvResult<()> {
        self.put_tenant(node, t)?;
        let source = {
            let nodes = self.nodes.lock().unwrap();
            if nodes.get(node).map(|n| n.has_template(template)).unwrap_or(false) { None } else {
                let mut holders: Vec<(&String, &Node)> = nodes.iter().filter(|(_, n)| n.healthy() && n.has_template(template)).collect();
                // the node that made it, if it is up
                holders.sort_by_key(|(k, _)| k.as_str() != node_of(template));
                Some(holders.first().map(|(_, n)| n.url.clone()).ok_or_else(|| EnvError::Unavailable(format!("no node that holds template {} is up", template)))?)
            }
        };
        if let Some(from) = source {
            let tp: Template = self.send_ok(node, "POST", &format!("/internal/v1/templates/{}/fetch", template), None, Some(&serde_json::json!({ "from": from })))?;
            if let Some(s) = self.nodes.lock().unwrap().get_mut(node).and_then(|n| n.status.as_mut()) { s.templates.push(tp); }
        }
        if let Some((id, spec)) = task {
            let _: serde_json::Value = self.send_ok(node, "PUT", &format!("/internal/v1/tasks/{}", id), None, Some(&serde_json::json!({ "tenant_id": t.id, "spec": spec })))?;
        }
        Ok(())
    }

    /// Start sessions as `plan` says, one request per node (concurrently); all of them, or none.
    fn start_on(&self, t: &Tenant, hold: &Hold, plan: Vec<(String, usize)>, template: &str, task: Option<(&str, &TaskSpec)>,
                request: &(dyn Fn(usize, usize) -> (String, serde_json::Value) + Sync)) -> EnvResult<Vec<String>> {
        let mut offset = 0;
        let parts: Vec<(String, usize, usize)> = plan.into_iter().map(|(n, c)| { let p = (n, offset, c); offset += c; p }).collect();
        let results: Vec<EnvResult<Vec<String>>> = std::thread::scope(|s| {
            let hs: Vec<_> = parts.iter().map(|(node, from, count)| s.spawn(move || {
                self.prepare(node, t, template, task)?;
                let (path, body) = request(*from, *count);
                let v: serde_json::Value = self.send_ok(node, "POST", &path, Some(&t.id), Some(&body))?;
                let ids: Vec<String> = match v.get("session_ids") {
                    Some(ids) => serde_json::from_value(ids.clone()).map_err(anyhow::Error::from)?,
                    None => vec![v.get("session_id").and_then(|i| i.as_str()).ok_or_else(|| EnvError::Internal(anyhow!("a node answered {}", v)))?.to_string()],
                };
                hold.started(node, ids.len() as i64);
                Ok(ids)
            })).collect();
            hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(EnvError::Internal(anyhow!("a worker failed"))))).collect()
        });
        if let Some(e) = results.iter().position(|r| r.is_err()) {
            for ids in results.iter().flatten() {
                for id in ids { let _ = self.send(node_of(id), "DELETE", &format!("/v1/sessions/{}", id), Some(&t.id), None); }
            }
            return Err(results.into_iter().nth(e).unwrap().unwrap_err());
        }
        Ok(results.into_iter().flat_map(|r| r.unwrap()).collect())
    }

    fn create_session(&self, t: &Tenant, req: serde_json::Value) -> EnvResult<serde_json::Value> {
        let (template, task) = match (req.get("template_id").and_then(|v| v.as_str()), req.get("task_id").and_then(|v| v.as_str())) {
            (Some(tp), None) => (self.visible_template(t, tp)?.id, None),
            (None, Some(task)) => { let spec = self.task(t, task)?; (self.visible_template(t, &spec.template_id)?.id, Some((task.to_string(), spec))) }
            _ => return Err(EnvError::BadRequest("give exactly one of template_id and task_id".into())),
        };
        let (hold, plan) = self.place(t, Some(&template), 1, None)?;
        let ids = self.start_on(t, &hold, plan, &template, task.as_ref().map(|(i, s)| (i.as_str(), s)), &|_, _| ("/v1/sessions".into(), req.clone()))?;
        Ok(serde_json::json!({ "session_id": ids[0] }))
    }

    fn rollouts(&self, t: &Tenant, task_id: &str, variants: Vec<Variant>) -> EnvResult<serde_json::Value> {
        if variants.is_empty() { return Err(EnvError::BadRequest("no variants".into())); }
        let spec = self.task(t, task_id)?;
        let template = self.visible_template(t, &spec.template_id)?.id;
        let (hold, plan) = self.place(t, Some(&template), variants.len(), None)?;
        let ids = self.start_on(t, &hold, plan, &template, Some((task_id, &spec)), &|from, count| {
            ("/v1/rollouts".into(), serde_json::json!({ "task_id": task_id, "variants": &variants[from..from + count] }))
        })?;
        Ok(serde_json::json!({ "session_ids": ids }))
    }

    /// Forks of a session or of a checkpoint start on its node: its layers are there.
    fn fork(&self, t: &Tenant, path: &str, of: &str, variants: Vec<Variant>) -> EnvResult<serde_json::Value> {
        if variants.is_empty() { return Err(EnvError::BadRequest("no variants".into())); }
        let node = node_of(of).to_string();
        let (hold, _) = self.place(t, None, variants.len(), Some(&node))?;
        self.put_tenant(&node, t)?;
        let v: serde_json::Value = self.send_ok(&node, "POST", path, Some(&t.id), Some(&serde_json::json!({ "variants": variants })))?;
        hold.started(&node, v.get("session_ids").and_then(|i| i.as_array()).map(|a| a.len() as i64).unwrap_or(0));
        Ok(v)
    }

    /// One action in each of several sessions: each node gets its sessions' items, all nodes at once.
    fn actions(&self, t: &Tenant, items: Vec<serde_json::Value>) -> EnvResult<Vec<serde_json::Value>> {
        let mut by_node: HashMap<String, Vec<(usize, serde_json::Value)>> = HashMap::new();
        for (i, it) in items.into_iter().enumerate() {
            let node = it.get("session_id").and_then(|v| v.as_str()).map(|s| node_of(s).to_string()).unwrap_or_default();
            by_node.entry(node).or_default().push((i, it));
        }
        let n = by_node.values().map(|v| v.len()).sum();
        let mut out = vec![serde_json::Value::Null; n];
        let answers: Vec<(Vec<usize>, EnvResult<Vec<serde_json::Value>>)> = std::thread::scope(|s| {
            let hs: Vec<_> = by_node.into_iter().map(|(node, its)| s.spawn(move || {
                let idx: Vec<usize> = its.iter().map(|(i, _)| *i).collect();
                let body = serde_json::Value::Array(its.into_iter().map(|(_, v)| v).collect());
                (idx, self.ensure_tenant(&node, t).and_then(|_| self.send_ok(&node, "POST", "/v1/actions", Some(&t.id), Some(&body))))
            })).collect();
            hs.into_iter().map(|h| h.join().unwrap_or_else(|_| (Vec::new(), Err(EnvError::Internal(anyhow!("a worker failed")))))).collect()
        });
        for (idx, r) in answers {
            match r {
                Ok(vals) => for (i, v) in idx.into_iter().zip(vals) { out[i] = v; },
                Err(e) => for i in idx { out[i] = serde_json::json!({ "error": e.to_string() }); },
            }
        }
        Ok(out)
    }

    fn usage(&self, t: &Tenant) -> EnvResult<Vec<serde_json::Value>> {
        let nodes: Vec<String> = self.nodes.lock().unwrap().iter().filter(|(_, n)| n.healthy()).map(|(k, _)| k.clone()).collect();
        let mut out = Vec::new();
        for n in nodes {
            self.ensure_tenant(&n, t)?;
            let v: Vec<serde_json::Value> = self.send_ok(&n, "GET", "/v1/usage", Some(&t.id), None)?;
            out.extend(v);
        }
        Ok(out)
    }

    /// Pass a session's or a checkpoint's request on to its node; a template it made joins the list.
    fn pass(&self, t: &Tenant, method: &str, path: &str, of: &str, body: Option<serde_json::Value>) -> EnvResult<Answer> {
        let node = node_of(of).to_string();
        self.ensure_tenant(&node, t)?;
        let a = self.send(&node, method, path, Some(&t.id), body.as_ref())?;
        // a closed session no longer counts: a status asked for now is exact (the sessions the node closes
        // by itself, at a time limit, count until the next poll)
        if a.ok() && method == "DELETE" && path.starts_with("/v1/sessions/") { self.poll(&node); }
        if a.ok() && path.ends_with("/freeze") {
            if let Ok(tp) = a.json::<Template>() {
                if let Some(s) = self.nodes.lock().unwrap().get_mut(&node).and_then(|n| n.status.as_mut()) { s.templates.push(tp); }
            }
        }
        Ok(a)
    }
}

type ApiResult = Result<Response, EnvError>;

/// Run `f` for the client's tenant on a blocking thread.
async fn call<T: Serialize + Send + 'static>(c: Arc<Coordinator>, h: HeaderMap, f: impl FnOnce(&Coordinator, &Tenant) -> EnvResult<T> + Send + 'static) -> ApiResult {
    let key = bearer(&h);
    let out = tokio::task::spawn_blocking(move || { let t = c.authenticate(key.as_deref())?; f(&c, &t) }).await
        .map_err(|e| EnvError::Internal(anyhow!("worker failed: {}", e)))??;
    Ok(Json(out).into_response())
}

/// Run `f` for the client's tenant and pass the node's answer on.
async fn pass(c: Arc<Coordinator>, h: HeaderMap, f: impl FnOnce(&Coordinator, &Tenant) -> EnvResult<Answer> + Send + 'static) -> ApiResult {
    let key = bearer(&h);
    let a = tokio::task::spawn_blocking(move || { let t = c.authenticate(key.as_deref())?; f(&c, &t) }).await
        .map_err(|e| EnvError::Internal(anyhow!("worker failed: {}", e)))??;
    Ok(a.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Variants { variants: Vec<Variant> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RolloutsReq { task_id: String, variants: Vec<Variant> }

pub fn router(c: Arc<Coordinator>) -> Router {
    type S = State<Arc<Coordinator>>;
    Router::new()
        .route("/v1/templates", get(|State(c): S, h: HeaderMap| async move { call(c, h, |c, t| Ok(c.list_templates(t))).await }))
        .route("/v1/templates/{id}", delete(|State(c): S, h: HeaderMap, Path(id): Path<String>| async move {
            call(c, h, move |c, t| c.delete_template(t, &id).map(|_| serde_json::json!({}))).await }))
        .route("/v1/tasks", post(|State(c): S, h: HeaderMap, Json(spec): Json<TaskSpec>| async move {
            call(c, h, move |c, t| c.create_task(t, spec).map(|id| serde_json::json!({ "task_id": id }))).await }))
        .route("/v1/tasks/{id}", get(|State(c): S, h: HeaderMap, Path(id): Path<String>| async move { call(c, h, move |c, t| c.task(t, &id)).await })
            .delete(|State(c): S, h: HeaderMap, Path(id): Path<String>| async move {
            call(c, h, move |c, t| c.delete_task(t, &id).map(|_| serde_json::json!({}))).await }))
        .route("/v1/sessions", post(|State(c): S, h: HeaderMap, Json(req): Json<serde_json::Value>| async move {
            call(c, h, move |c, t| c.create_session(t, req)).await }))
        .route("/v1/rollouts", post(|State(c): S, h: HeaderMap, Json(r): Json<RolloutsReq>| async move {
            call(c, h, move |c, t| c.rollouts(t, &r.task_id, r.variants)).await }))
        .route("/v1/sessions/{id}/fork", post(|State(c): S, h: HeaderMap, Path(id): Path<String>, Json(v): Json<Variants>| async move {
            call(c, h, move |c, t| c.fork(t, &format!("/v1/sessions/{}/fork", id), &id, v.variants)).await }))
        .route("/v1/checkpoints/{id}/fork", post(|State(c): S, h: HeaderMap, Path(id): Path<String>, Json(v): Json<Variants>| async move {
            call(c, h, move |c, t| c.fork(t, &format!("/v1/checkpoints/{}/fork", id), &id, v.variants)).await }))
        .route("/v1/sessions/{id}", delete(|State(c): S, h: HeaderMap, Path(id): Path<String>| async move {
            pass(c, h, move |c, t| c.pass(t, "DELETE", &format!("/v1/sessions/{}", id), &id, None)).await }))
        .route("/v1/sessions/{id}/{op}", post(|State(c): S, h: HeaderMap, Path((id, op)): Path<(String, String)>, body: Option<Json<serde_json::Value>>| async move {
            pass(c, h, move |c, t| c.pass(t, "POST", &format!("/v1/sessions/{}/{}", id, op), &id, body.map(|b| b.0))).await })
            .get(|State(c): S, h: HeaderMap, Path((id, op)): Path<(String, String)>| async move {
            pass(c, h, move |c, t| c.pass(t, "GET", &format!("/v1/sessions/{}/{}", id, op), &id, None)).await }))
        .route("/v1/checkpoints/{id}", delete(|State(c): S, h: HeaderMap, Path(id): Path<String>| async move {
            pass(c, h, move |c, t| c.pass(t, "DELETE", &format!("/v1/checkpoints/{}", id), &id, None)).await }))
        .route("/v1/actions", post(|State(c): S, h: HeaderMap, Json(items): Json<Vec<serde_json::Value>>| async move {
            call(c, h, move |c, t| c.actions(t, items)).await }))
        .route("/v1/usage", get(|State(c): S, h: HeaderMap| async move { call(c, h, |c, t| c.usage(t)).await }))
        // the nodes register here, with the cluster token
        .route("/internal/v1/nodes", post(|State(c): S, h: HeaderMap, Json(r): Json<Register>| async move {
            let key = bearer(&h);
            tokio::task::spawn_blocking(move || {
                if !c.is_cluster_token(key.as_deref()) { return Err(EnvError::Unauthorized); }
                c.register(r).map(|_| Json(serde_json::json!({})).into_response())
            }).await.map_err(|e| EnvError::Internal(anyhow!("worker failed: {}", e)))? })
            .get(|State(c): S, h: HeaderMap| async move {
            if !c.is_cluster_token(bearer(&h).as_deref()) { return Err(EnvError::Unauthorized); }
            Ok::<Response, EnvError>(Json(c.nodes_view()).into_response()) }))
        .layer(DefaultBodyLimit::max(64 << 20))
        .with_state(c)
}

/// Serve until the process is stopped, polling the nodes every two seconds.
pub async fn serve(c: Arc<Coordinator>, listen: &str) -> anyhow::Result<()> {
    let poller = c.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let p = poller.clone();
            let _ = tokio::task::spawn_blocking(move || p.poll_all()).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, router(c)).await?;
    Ok(())
}

/// Register node `id` (reachable at `url`) with the coordinator at `coordinator`, now and every ten seconds,
/// so a restarted coordinator learns it back.
pub fn register_forever(coordinator: String, token: String, id: String, url: String, max_sessions: Option<i64>) {
    std::thread::Builder::new().name("register".into()).spawn(move || loop {
        let r = ureq::post(&format!("{}/internal/v1/nodes", coordinator.trim_end_matches('/')))
            .set("Authorization", &format!("Bearer {}", token)).timeout(Duration::from_secs(5))
            .send_json(serde_json::json!({ "id": id, "url": url, "max_sessions": max_sessions }));
        if let Err(e) = r { eprintln!("register with the coordinator at {}: {}", coordinator, e); }
        std::thread::sleep(Duration::from_secs(10));
    }).expect("start the registration thread");
}
