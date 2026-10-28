//! The control database (`<data>/control.db`): tenants, API keys, templates, layers, tasks, sessions and
//! usage. API keys are stored as sha256 digests only; the key itself is shown once, when it is created.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tenants(
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    max_sessions INTEGER NOT NULL,
    max_storage_bytes INTEGER NOT NULL,
    max_session_secs INTEGER NOT NULL,
    max_session_disk_bytes INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS api_keys(
    digest BLOB PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id),
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS layers(
    id TEXT PRIMARY KEY,
    tenant_id TEXT,
    path TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    -- the sha256 of the layer's stream (transfer.rs), taken the first time another node asks for it
    digest TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE TABLE IF NOT EXISTS templates(
    id TEXT PRIMARY KEY,
    tenant_id TEXT,
    name TEXT NOT NULL,
    parent_id TEXT,
    layers TEXT NOT NULL,
    db_path TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    env TEXT NOT NULL DEFAULT '[]',
    workdir TEXT NOT NULL DEFAULT '/',
    -- the sha256 of base.db, as for layers
    db_digest TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE TABLE IF NOT EXISTS tasks(
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    spec TEXT NOT NULL,
    layers TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE TABLE IF NOT EXISTS sessions(
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    template_id TEXT NOT NULL,
    task_id TEXT,
    parent_id TEXT,
    label TEXT,
    seed INTEGER,
    extra_layers TEXT NOT NULL DEFAULT '[]',
    started_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    ended_at INTEGER,
    end_reason TEXT,
    wall_ms INTEGER,
    cpu_usec INTEGER,
    steps INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_sessions_tenant ON sessions(tenant_id);
CREATE TABLE IF NOT EXISTS checkpoints(
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    template_id TEXT NOT NULL,
    task_id TEXT,
    egress TEXT NOT NULL DEFAULT '[]',
    layers TEXT NOT NULL,
    step INTEGER NOT NULL,
    label TEXT,
    seed INTEGER,
    bytes INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON checkpoints(session_id);
";

/// A session's files at one step, kept as layers: new sessions start from it after the session has moved
/// on or ended.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Checkpoint {
    pub id: String,
    #[serde(skip)]
    pub tenant_id: String,
    pub session_id: String,
    pub kind: String,
    pub template_id: String,
    pub task_id: Option<String>,
    #[serde(skip)]
    pub egress: Vec<String>,
    /// Layers above the template's, bottom first.
    #[serde(skip)]
    pub layers: Vec<String>,
    /// Actions the session had taken.
    pub step: i64,
    pub label: Option<String>,
    pub seed: Option<u64>,
    /// What the checkpoint's own layer stores.
    pub bytes: i64,
    pub created_at: i64,
}

fn checkpoint_row(r: &rusqlite::Row) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        id: r.get(0)?, tenant_id: r.get(1)?, session_id: r.get(2)?, kind: r.get(3)?, template_id: r.get(4)?, task_id: r.get(5)?,
        egress: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
        layers: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
        step: r.get(8)?, label: r.get(9)?, seed: r.get::<_, Option<i64>>(10)?.map(|s| s as u64), bytes: r.get(11)?, created_at: r.get(12)?,
    })
}
const CHECKPOINT_COLS: &str = "id, tenant_id, session_id, kind, template_id, task_id, egress, layers, step, label, seed, bytes, created_at";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    pub max_sessions: i64,
    pub max_storage_bytes: i64,
    pub max_session_secs: i64,
    pub max_session_disk_bytes: i64,
}

#[derive(Debug, Clone)]
pub struct Quotas { pub max_sessions: i64, pub max_storage_bytes: i64, pub max_session_secs: i64, pub max_session_disk_bytes: i64 }

impl Default for Quotas {
    fn default() -> Self {
        Quotas { max_sessions: 256, max_storage_bytes: 50 << 30, max_session_secs: 4 * 3600, max_session_disk_bytes: 4 << 30 }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Template {
    pub id: String,
    /// None: a system template every tenant can use.
    pub tenant_id: Option<String>,
    pub name: String,
    pub parent_id: Option<String>,
    /// Layer ids, bottom first.
    pub layers: Vec<String>,
    #[serde(skip)]
    pub db_path: PathBuf,
    pub bytes: i64,
    /// The environment commands start with (an image's `Env`), under the task's own.
    pub env: Vec<(String, String)>,
    /// Where commands start (an image's `WorkingDir`).
    pub workdir: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Layer { pub id: String, pub tenant_id: Option<String>, pub path: PathBuf, pub bytes: i64 }

/// One session's row, as recorded for usage.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionUsage {
    pub id: String,
    pub kind: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub wall_ms: Option<i64>,
    pub cpu_usec: Option<i64>,
    pub steps: i64,
}

pub struct ControlDb { conn: Mutex<Connection> }

pub fn key_digest(key: &str) -> Vec<u8> { Sha256::digest(key.as_bytes()).to_vec() }

/// `n` random bytes from the kernel, as hex.
pub fn random_hex(n: usize) -> Result<String> {
    use std::io::Read;
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{:02x}", x)).collect())
}

fn template_row(r: &rusqlite::Row) -> rusqlite::Result<Template> {
    let layers: String = r.get(4)?;
    Ok(Template {
        id: r.get(0)?, tenant_id: r.get(1)?, name: r.get(2)?, parent_id: r.get(3)?,
        layers: serde_json::from_str(&layers).unwrap_or_default(),
        db_path: PathBuf::from(r.get::<_, String>(5)?), bytes: r.get(6)?, created_at: r.get(7)?,
        env: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(), workdir: r.get(9)?,
    })
}
const TEMPLATE_COLS: &str = "id, tenant_id, name, parent_id, layers, db_path, bytes, created_at, env, workdir";

fn tenant_row(r: &rusqlite::Row) -> rusqlite::Result<Tenant> {
    Ok(Tenant { id: r.get(0)?, name: r.get(1)?, max_sessions: r.get(2)?, max_storage_bytes: r.get(3)?, max_session_secs: r.get(4)?, max_session_disk_bytes: r.get(5)? })
}
const TENANT_COLS: &str = "id, name, max_sessions, max_storage_bytes, max_session_secs, max_session_disk_bytes";

impl ControlDb {
    pub fn open(path: &Path) -> Result<ControlDb> {
        let conn = Connection::open(path)?;
        crate::vm::harden(&conn)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(ControlDb { conn: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> { f(&self.conn.lock().unwrap()) }

    pub fn create_tenant(&self, id: &str, name: &str, q: &Quotas) -> Result<Tenant> {
        self.with(|c| {
            c.execute("INSERT INTO tenants(id, name, max_sessions, max_storage_bytes, max_session_secs, max_session_disk_bytes) VALUES (?1,?2,?3,?4,?5,?6)",
                params![id, name, q.max_sessions, q.max_storage_bytes, q.max_session_secs, q.max_session_disk_bytes])?;
            Ok(())
        })?;
        self.tenant(id)?.ok_or_else(|| anyhow!("tenant {} vanished", id))
    }

    /// A tenant as the coordinator of a cluster has it (its id, name and quotas), on a node.
    pub fn put_tenant(&self, t: &Tenant) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO tenants(id, name, max_sessions, max_storage_bytes, max_session_secs, max_session_disk_bytes) VALUES (?1,?2,?3,?4,?5,?6)
                       ON CONFLICT(id) DO UPDATE SET name=excluded.name, max_sessions=excluded.max_sessions, max_storage_bytes=excluded.max_storage_bytes,
                       max_session_secs=excluded.max_session_secs, max_session_disk_bytes=excluded.max_session_disk_bytes",
                params![t.id, t.name, t.max_sessions, t.max_storage_bytes, t.max_session_secs, t.max_session_disk_bytes])?;
            Ok(())
        })
    }

    pub fn tenant(&self, id: &str) -> Result<Option<Tenant>> {
        self.with(|c| Ok(c.query_row(&format!("SELECT {} FROM tenants WHERE id=?1", TENANT_COLS), params![id], tenant_row).optional()?))
    }

    /// A new API key for `tenant`. The key is returned once; only its digest is kept.
    pub fn create_key(&self, tenant: &str) -> Result<String> {
        if self.tenant(tenant)?.is_none() { return Err(anyhow!("no tenant {}", tenant)); }
        let key = format!("sk-{}", random_hex(32)?);
        self.with(|c| { c.execute("INSERT INTO api_keys(digest, tenant_id) VALUES (?1, ?2)", params![key_digest(&key), tenant])?; Ok(()) })?;
        Ok(key)
    }

    pub fn tenant_for_key(&self, key: &str) -> Result<Option<Tenant>> {
        self.with(|c| Ok(c.query_row(
            &format!("SELECT {} FROM tenants WHERE id = (SELECT tenant_id FROM api_keys WHERE digest=?1 AND revoked=0)", TENANT_COLS),
            params![key_digest(key)], tenant_row).optional()?))
    }

    pub fn insert_layer(&self, l: &Layer) -> Result<()> {
        self.with(|c| { c.execute("INSERT INTO layers(id, tenant_id, path, bytes) VALUES (?1,?2,?3,?4)", params![l.id, l.tenant_id, l.path.to_string_lossy(), l.bytes])?; Ok(()) })
    }

    pub fn layer(&self, id: &str) -> Result<Option<Layer>> {
        self.with(|c| Ok(c.query_row("SELECT id, tenant_id, path, bytes FROM layers WHERE id=?1", params![id],
            |r| Ok(Layer { id: r.get(0)?, tenant_id: r.get(1)?, path: PathBuf::from(r.get::<_, String>(2)?), bytes: r.get(3)? })).optional()?))
    }

    pub fn layer_digest(&self, id: &str) -> Result<Option<String>> {
        self.with(|c| Ok(c.query_row("SELECT digest FROM layers WHERE id=?1", params![id], |r| r.get(0)).optional()?.flatten()))
    }

    pub fn set_layer_digest(&self, id: &str, digest: &str) -> Result<()> {
        self.with(|c| { c.execute("UPDATE layers SET digest=?2 WHERE id=?1", params![id, digest])?; Ok(()) })
    }

    pub fn template_db_digest(&self, id: &str) -> Result<Option<String>> {
        self.with(|c| Ok(c.query_row("SELECT db_digest FROM templates WHERE id=?1", params![id], |r| r.get(0)).optional()?.flatten()))
    }

    pub fn set_template_db_digest(&self, id: &str, digest: &str) -> Result<()> {
        self.with(|c| { c.execute("UPDATE templates SET db_digest=?2 WHERE id=?1", params![id, digest])?; Ok(()) })
    }

    pub fn delete_layer(&self, id: &str) -> Result<()> {
        self.with(|c| { c.execute("DELETE FROM layers WHERE id=?1", params![id])?; Ok(()) })
    }

    /// Layer ids some template or live session still uses.
    pub fn layers_in_use(&self) -> Result<std::collections::HashSet<String>> {
        self.with(|c| {
            let mut out = std::collections::HashSet::new();
            let mut st = c.prepare("SELECT layers FROM templates UNION ALL SELECT extra_layers FROM sessions WHERE ended_at IS NULL
                                    UNION ALL SELECT layers FROM checkpoints UNION ALL SELECT layers FROM tasks")?;
            for r in st.query_map([], |r| r.get::<_, String>(0))? {
                for id in serde_json::from_str::<Vec<String>>(&r?).unwrap_or_default() { out.insert(id); }
            }
            Ok(out)
        })
    }

    pub fn all_layers(&self) -> Result<Vec<Layer>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, tenant_id, path, bytes FROM layers")?;
            let rows = st.query_map([], |r| Ok(Layer { id: r.get(0)?, tenant_id: r.get(1)?, path: PathBuf::from(r.get::<_, String>(2)?), bytes: r.get(3)? }))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn insert_template(&self, t: &Template) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO templates(id, tenant_id, name, parent_id, layers, db_path, bytes, env, workdir, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![t.id, t.tenant_id, t.name, t.parent_id, serde_json::to_string(&t.layers)?, t.db_path.to_string_lossy(), t.bytes,
                        serde_json::to_string(&t.env)?, t.workdir, t.created_at])?;
            Ok(())
        })
    }

    pub fn template(&self, id: &str) -> Result<Option<Template>> {
        self.with(|c| Ok(c.query_row(&format!("SELECT {} FROM templates WHERE id=?1", TEMPLATE_COLS), params![id], template_row).optional()?))
    }

    pub fn all_templates(&self) -> Result<Vec<Template>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {} FROM templates ORDER BY created_at, id", TEMPLATE_COLS))?;
            let rows = st.query_map([], template_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn tenant_ids(&self) -> Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id FROM tenants ORDER BY id")?;
            let rows = st.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The system templates and `tenant`'s own.
    pub fn templates_for(&self, tenant: &str) -> Result<Vec<Template>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {} FROM templates WHERE tenant_id IS NULL OR tenant_id=?1 ORDER BY created_at, id", TEMPLATE_COLS))?;
            let rows = st.query_map(params![tenant], template_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn delete_template(&self, id: &str) -> Result<()> {
        self.with(|c| { c.execute("DELETE FROM templates WHERE id=?1", params![id])?; Ok(()) })
    }

    /// Bytes `tenant` stores: its templates' databases and its layers.
    pub fn storage_bytes(&self, tenant: &str) -> Result<i64> {
        self.with(|c| Ok(c.query_row(
            "SELECT (SELECT COALESCE(sum(bytes),0) FROM templates WHERE tenant_id=?1) + (SELECT COALESCE(sum(bytes),0) FROM layers WHERE tenant_id=?1)",
            params![tenant], |r| r.get(0))?))
    }

    /// A task: its spec (JSON) and the layers of its files, above its template's.
    pub fn insert_task(&self, id: &str, tenant: &str, spec: &str, layers: &[String]) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO tasks(id, tenant_id, spec, layers) VALUES (?1,?2,?3,?4)", params![id, tenant, spec, serde_json::to_string(layers)?])?;
            Ok(())
        })
    }

    /// (tenant, spec JSON, layers)
    pub fn task(&self, id: &str) -> Result<Option<(String, String, Vec<String>)>> {
        self.with(|c| Ok(c.query_row("SELECT tenant_id, spec, layers FROM tasks WHERE id=?1", params![id],
            |r| Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default()))).optional()?))
    }

    pub fn delete_task(&self, id: &str) -> Result<()> {
        self.with(|c| { c.execute("DELETE FROM tasks WHERE id=?1", params![id])?; Ok(()) })
    }

    /// Whether a running session or a checkpoint uses `task`.
    pub fn task_in_use(&self, task: &str) -> Result<bool> {
        self.with(|c| Ok(c.query_row("SELECT 1 FROM sessions WHERE task_id=?1 AND ended_at IS NULL UNION ALL SELECT 1 FROM checkpoints WHERE task_id=?1 LIMIT 1",
            params![task], |_| Ok(())).optional()?.is_some()))
    }

    /// Whether a task is built on `template`.
    pub fn template_has_tasks(&self, template: &str) -> Result<bool> {
        self.with(|c| Ok(c.query_row("SELECT 1 FROM tasks WHERE json_extract(spec, '$.template_id')=?1 LIMIT 1", params![template], |_| Ok(())).optional()?.is_some()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_session(&self, id: &str, tenant: &str, kind: &str, template: &str, task: Option<&str>, parent: Option<&str>,
                          label: Option<&str>, seed: Option<u64>, extra_layers: &[String]) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO sessions(id, tenant_id, kind, template_id, task_id, parent_id, label, seed, extra_layers) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id, tenant, kind, template, task, parent, label, seed.map(|s| s as i64), serde_json::to_string(extra_layers)?])?;
            Ok(())
        })
    }

    pub fn end_session(&self, id: &str, reason: &str, wall_ms: u128, cpu_usec: Option<u64>, steps: u64) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE sessions SET ended_at=strftime('%s','now'), end_reason=?2, wall_ms=?3, cpu_usec=?4, steps=?5 WHERE id=?1 AND ended_at IS NULL",
                params![id, reason, wall_ms as i64, cpu_usec.map(|v| v as i64), steps as i64])?;
            Ok(())
        })
    }

    pub fn insert_checkpoint(&self, k: &Checkpoint) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO checkpoints(id, tenant_id, session_id, kind, template_id, task_id, egress, layers, step, label, seed, bytes)
                       VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![k.id, k.tenant_id, k.session_id, k.kind, k.template_id, k.task_id, serde_json::to_string(&k.egress)?,
                        serde_json::to_string(&k.layers)?, k.step, k.label, k.seed.map(|s| s as i64), k.bytes])?;
            Ok(())
        })
    }

    pub fn checkpoint(&self, id: &str) -> Result<Option<Checkpoint>> {
        self.with(|c| Ok(c.query_row(&format!("SELECT {} FROM checkpoints WHERE id=?1", CHECKPOINT_COLS), params![id], checkpoint_row).optional()?))
    }

    /// `tenant`'s checkpoints of `session`, oldest first.
    pub fn checkpoints_of(&self, tenant: &str, session: &str) -> Result<Vec<Checkpoint>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {} FROM checkpoints WHERE tenant_id=?1 AND session_id=?2 ORDER BY step, created_at, id", CHECKPOINT_COLS))?;
            let rows = st.query_map(params![tenant, session], checkpoint_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn delete_checkpoint(&self, id: &str) -> Result<()> {
        self.with(|c| { c.execute("DELETE FROM checkpoints WHERE id=?1", params![id])?; Ok(()) })
    }

    /// Whether a checkpoint starts from `template` (it needs the template's layers and database).
    pub fn template_has_checkpoints(&self, template: &str) -> Result<bool> {
        self.with(|c| Ok(c.query_row("SELECT 1 FROM checkpoints WHERE template_id=?1 LIMIT 1", params![template], |_| Ok(())).optional()?.is_some()))
    }

    /// For a support bundle: counts, and the recent sessions' ids, kinds, times, end reasons and steps. No
    /// key digests, names, labels, task specs or file contents.
    pub fn support_summary(&self) -> Result<serde_json::Value> {
        self.with(|c| {
            let count = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, [], |r| r.get(0)) };
            let mut st = c.prepare("SELECT id, tenant_id, kind, started_at, ended_at, end_reason, steps, wall_ms FROM sessions ORDER BY started_at DESC LIMIT 500")?;
            let sessions: Vec<serde_json::Value> = st.query_map([], |r| Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?, "tenant": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?, "started_at": r.get::<_, i64>(3)?,
                "ended_at": r.get::<_, Option<i64>>(4)?, "end_reason": r.get::<_, Option<String>>(5)?, "steps": r.get::<_, i64>(6)?, "wall_ms": r.get::<_, Option<i64>>(7)?,
            })))?.collect::<rusqlite::Result<_>>()?;
            let mut st = c.prepare("SELECT end_reason, count(*) FROM sessions WHERE ended_at IS NOT NULL GROUP BY end_reason")?;
            let reasons: serde_json::Map<String, serde_json::Value> = st.query_map([], |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|(k, v)| (k, v.into())).collect();
            let mut st = c.prepare("SELECT id, tenant_id IS NULL, bytes, layers, created_at FROM templates ORDER BY created_at")?;
            let templates: Vec<serde_json::Value> = st.query_map([], |r| Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?, "system": r.get::<_, bool>(1)?, "bytes": r.get::<_, i64>(2)?,
                "layers": serde_json::from_str::<Vec<String>>(&r.get::<_, String>(3)?).map(|l| l.len()).unwrap_or(0), "created_at": r.get::<_, i64>(4)?,
            })))?.collect::<rusqlite::Result<_>>()?;
            Ok(serde_json::json!({
                "tenants": count("SELECT count(*) FROM tenants")?, "api_keys": count("SELECT count(*) FROM api_keys WHERE revoked=0")?,
                "tasks": count("SELECT count(*) FROM tasks")?, "checkpoints": count("SELECT count(*) FROM checkpoints")?,
                "layers": count("SELECT count(*) FROM layers")?, "layer_bytes": count("SELECT COALESCE(sum(bytes),0) FROM layers")?,
                "sessions_running": count("SELECT count(*) FROM sessions WHERE ended_at IS NULL")?, "end_reasons": reasons,
                "templates": templates, "recent_sessions": sessions,
            }))
        })
    }

    /// Sessions a previous run of the service left open: they ended with it.
    pub fn end_orphaned_sessions(&self) -> Result<usize> {
        self.with(|c| Ok(c.execute("UPDATE sessions SET ended_at=strftime('%s','now'), end_reason='service restarted' WHERE ended_at IS NULL", [])?))
    }

    pub fn usage(&self, tenant: &str) -> Result<Vec<SessionUsage>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, kind, started_at, ended_at, wall_ms, cpu_usec, steps FROM sessions WHERE tenant_id=?1 ORDER BY started_at, id")?;
            let rows = st.query_map(params![tenant], |r| Ok(SessionUsage { id: r.get(0)?, kind: r.get(1)?, started_at: r.get(2)?, ended_at: r.get(3)?, wall_ms: r.get(4)?, cpu_usec: r.get(5)?, steps: r.get(6)? }))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
}
