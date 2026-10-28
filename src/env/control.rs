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
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE TABLE IF NOT EXISTS tasks(
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    spec TEXT NOT NULL,
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
";

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, serde::Serialize)]
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

    pub fn delete_layer(&self, id: &str) -> Result<()> {
        self.with(|c| { c.execute("DELETE FROM layers WHERE id=?1", params![id])?; Ok(()) })
    }

    /// Layer ids some template or live session still uses.
    pub fn layers_in_use(&self) -> Result<std::collections::HashSet<String>> {
        self.with(|c| {
            let mut out = std::collections::HashSet::new();
            let mut st = c.prepare("SELECT layers FROM templates UNION ALL SELECT extra_layers FROM sessions WHERE ended_at IS NULL")?;
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
            c.execute("INSERT INTO templates(id, tenant_id, name, parent_id, layers, db_path, bytes, env, workdir) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![t.id, t.tenant_id, t.name, t.parent_id, serde_json::to_string(&t.layers)?, t.db_path.to_string_lossy(), t.bytes,
                        serde_json::to_string(&t.env)?, t.workdir])?;
            Ok(())
        })
    }

    pub fn template(&self, id: &str) -> Result<Option<Template>> {
        self.with(|c| Ok(c.query_row(&format!("SELECT {} FROM templates WHERE id=?1", TEMPLATE_COLS), params![id], template_row).optional()?))
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

    pub fn insert_task(&self, id: &str, tenant: &str, spec: &str) -> Result<()> {
        self.with(|c| { c.execute("INSERT INTO tasks(id, tenant_id, spec) VALUES (?1,?2,?3)", params![id, tenant, spec])?; Ok(()) })
    }

    /// (tenant, spec JSON)
    pub fn task(&self, id: &str) -> Result<Option<(String, String)>> {
        self.with(|c| Ok(c.query_row("SELECT tenant_id, spec FROM tasks WHERE id=?1", params![id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
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
