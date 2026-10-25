//! Policy-as-trigger: declarative write rules enforced by SQLite itself.
//!
//! A policy is a row in `vm_policies`. Every rule is compiled into `BEFORE` triggers on `vm_fs` and
//! `vm_file_chunks`, so a write that violates it is refused by the database, whatever code issued it.
//! Triggers cannot read a table at run time, so `rebuild_triggers` regenerates them from the table
//! whenever a rule is added or removed.
//!
//! Rules:
//! - `readonly_prefix`: no insert, update, or delete of a path at or under the prefix. The cascade
//!   from a `vm_fs` delete to its chunk rows is covered too, so the delete is refused as a whole.
//! - `max_file_size`: a regular file over the limit (bytes) cannot be written.
//!
//! Internal operations that must rewrite the tree (snapshot restore, a rollback) run under a bypass.
//! The bypass is a flag held by one connection: every connection registers the SQL function
//! `vm_policy_bypassed()`, and the triggers call it. A flag on one connection is invisible to others.
//! A TEMP table would not do: a main-schema trigger cannot reference a TEMP object.

use anyhow::{anyhow, Result};
use rusqlite::functions::FunctionFlags;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::vm::normalize_vm_path;

/// Message a policy trigger aborts with. Callers that map a refusal to an errno (FUSE: EACCES) match
/// on this one constant, so the text lives in exactly one place.
pub const REFUSAL_MESSAGE: &str = "EACCES: protected by vm_policies";

/// True if an error was raised by a policy trigger (as opposed to a corrupt or missing row).
pub fn is_refusal(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.to_string().contains(REFUSAL_MESSAGE))
}

/// SQL function the triggers call. Returns this connection's bypass flag.
pub const BYPASS_FN: &str = "vm_policy_bypassed";

/// Per-connection bypass state, shared between the connection's function and the caller.
/// Atomic because a connection's functions must be `Send`; the count is nested entries.
#[derive(Clone, Default)]
pub struct BypassFlag(Arc<AtomicU32>);

/// Registers `vm_policy_bypassed()` on this connection, with its own flag. Call once per connection.
/// Returns the flag so the connection's owner can enter and leave the bypass. Re-registering replaces
/// the previous flag, which is what a fresh connection needs.
pub fn register(conn: &Connection) -> Result<BypassFlag> {
    let flag = BypassFlag::default();
    let f = flag.clone();
    // not SQLITE_DETERMINISTIC: the value changes while the bypass is entered and left
    conn.create_scalar_function(BYPASS_FN, 0, FunctionFlags::SQLITE_UTF8, move |_| {
        Ok(f.0.load(Ordering::SeqCst) > 0)
    })?;
    Ok(flag)
}

/// Enters the bypass for as long as the guard lives. Dropping it leaves the bypass, even on an error
/// path, so a failed restore cannot leave the connection unprotected.
pub struct BypassGuard { flag: BypassFlag }

impl BypassFlag {
    pub fn enter(&self) -> BypassGuard {
        self.0.fetch_add(1, Ordering::SeqCst);
        BypassGuard { flag: self.clone() }
    }
    pub fn is_active(&self) -> bool { self.0.load(Ordering::SeqCst) > 0 }
}

impl Drop for BypassGuard {
    fn drop(&mut self) {
        self.flag.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Creates the policy table. Idempotent. Triggers are rebuilt from the table.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vm_policies(
            id INTEGER PRIMARY KEY,
            rule_type TEXT NOT NULL CHECK(rule_type IN ('readonly_prefix','max_file_size','disk_quota')),
            pattern TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')));",
    )?;
    rebuild_triggers(conn)
}

/// Rule kinds and the validated pattern for each.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    /// Protect this path and everything under it.
    ReadonlyPrefix(String),
    /// Refuse writes of regular files larger than this many bytes.
    MaxFileSize(u64),
    /// Refuse writes that would push the VM's total logical file bytes past this limit.
    DiskQuota(u64),
}

/// One stored policy.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub id: i64,
    pub rule: Rule,
}

/// Adds a rule and rebuilds the triggers. Returns the new policy id.
pub fn add(conn: &Connection, rule: &Rule) -> Result<i64> {
    ensure_schema(conn)?;
    let (kind, pattern) = match rule {
        Rule::ReadonlyPrefix(p) => {
            // An empty prefix must not silently become "/" (the whole tree) through normalisation.
            // "/" is still accepted, but only when it is typed explicitly.
            if p.trim().is_empty() { return Err(anyhow!("empty prefix: give a path, or \"/\" for the whole tree")); }
            ("readonly_prefix", normalize_vm_path(p))
        }
        Rule::MaxFileSize(n) => {
            if *n == 0 { return Err(anyhow!("max_file_size must be at least 1 byte")); }
            ("max_file_size", n.to_string())
        }
        Rule::DiskQuota(n) => {
            if *n == 0 { return Err(anyhow!("disk_quota must be at least 1 byte")); }
            ("disk_quota", n.to_string())
        }
    };
    conn.execute("INSERT INTO vm_policies(rule_type, pattern) VALUES (?1, ?2)", params![kind, pattern])?;
    let id = conn.last_insert_rowid();
    rebuild_triggers(conn)?;
    Ok(id)
}

/// Removes a rule by id and rebuilds the triggers. Returns false if no such rule.
pub fn remove(conn: &Connection, id: i64) -> Result<bool> {
    ensure_schema(conn)?;
    let n = conn.execute("DELETE FROM vm_policies WHERE id=?1", params![id])?;
    rebuild_triggers(conn)?;
    Ok(n > 0)
}

/// All stored rules, oldest first.
pub fn list(conn: &Connection) -> Result<Vec<Policy>> {
    ensure_schema(conn)?;
    let mut st = conn.prepare("SELECT id, rule_type, pattern FROM vm_policies ORDER BY id")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    let mut out = Vec::new();
    for r in rows {
        let (id, kind, pattern) = r?;
        let rule = match kind.as_str() {
            "readonly_prefix" => Rule::ReadonlyPrefix(pattern),
            "max_file_size" => Rule::MaxFileSize(pattern.parse()?),
            "disk_quota" => Rule::DiskQuota(pattern.parse()?),
            other => return Err(anyhow!("unknown rule type in vm_policies: {}", other)),
        };
        out.push(Policy { id, rule });
    }
    Ok(out)
}

/// True if `path` is `prefix` or lies under it, by whole path components. `/` covers every path.
/// The same rule the triggers apply, for code that reads rules directly (the overlay).
pub fn covers(prefix: &str, path: &str) -> bool {
    if prefix == "/" { return true; }
    path == prefix || path.starts_with(&format!("{}/", prefix))
}

/// Escapes LIKE wildcards so a prefix matches literally.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// SQL condition: `col` is the protected path or under it. `col` must be a column reference.
fn under_prefix(col: &str, prefix: &str) -> String {
    let p = prefix.replace('\'', "''");
    let like = like_escape(prefix).replace('\'', "''");
    if prefix == "/" {
        "1=1".to_string()
    } else {
        format!("({c} = '{p}' OR {c} LIKE '{like}/%' ESCAPE '\\')", c = col, p = p, like = like)
    }
}

/// Drops and recreates every policy trigger from the current rule set.
///
/// Each condition is built with the column reference passed in (`NEW.path` or `OLD.path`), never by
/// rewriting text afterwards, so a pattern that happens to contain "path" is left alone.
pub fn rebuild_triggers(conn: &Connection) -> Result<()> {
    let policies = {
        let mut st = conn.prepare("SELECT rule_type, pattern FROM vm_policies ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut sql = String::new();
    for name in ["vm_pol_fs_ins", "vm_pol_fs_upd", "vm_pol_fs_del", "vm_pol_ch_ins", "vm_pol_ch_upd", "vm_pol_ch_del"] {
        sql.push_str(&format!("DROP TRIGGER IF EXISTS {};\n", name));
    }
    let mut prefixes: Vec<String> = Vec::new();
    let mut max_size: Option<u64> = None;
    let mut quota: Option<u64> = None;
    for (kind, pattern) in &policies {
        match kind.as_str() {
            "readonly_prefix" => prefixes.push(pattern.clone()),
            "max_file_size" => {
                let n: u64 = pattern.parse()?;
                // several limits: the strictest one applies
                max_size = Some(max_size.map_or(n, |m| m.min(n)));
            }
            "disk_quota" => {
                let n: u64 = pattern.parse()?;
                quota = Some(quota.map_or(n, |m| m.min(n)));
            }
            other => return Err(anyhow!("unknown rule type in vm_policies: {}", other)),
        }
    }
    // Internal operations bypass the policy on this connection only (see register / BypassFlag).
    let bypass = format!("{}()", BYPASS_FN);
    let refuse = format!("SELECT RAISE(ABORT, '{}')", REFUSAL_MESSAGE);
    // Disk quota: the total the counters would show once this row exists. Only inserts and updates can
    // grow it, so the condition uses NEW; a delete only frees space. The row's own previous size is
    // subtracted, because vm_meta still counts it (an update replaces a row).
    let quota_cond = || -> Option<String> {
        let q = quota?;
        Some(format!(
            "(NEW.kind = 'file' AND (CAST((SELECT value FROM vm_meta WHERE key='logical_bytes') AS INTEGER)
               - COALESCE((SELECT CASE WHEN f2.kind='file' THEN f2.size ELSE 0 END FROM vm_fs f2 WHERE f2.path = NEW.path), 0)
               + NEW.size) > {q})", q = q))
    };
    let fs_row = |r: &str| -> Option<String> {
        if prefixes.is_empty() && max_size.is_none() && quota.is_none() { return None; }
        let mut conds: Vec<String> = prefixes.iter().map(|p| under_prefix(&format!("{}.path", r), p)).collect();
        // size and quota apply to the new row only: an old row's size is not what is being written
        if r == "NEW" {
            if let Some(m) = max_size {
                conds.push(format!("({r}.kind = 'file' AND {r}.size > {m})", r = r, m = m));
            }
            if let Some(q) = quota_cond() { conds.push(q); }
        }
        if conds.is_empty() { None } else { Some(conds.join(" OR ")) }
    };
    // Chunk rows carry content, so the prefix rule applies to them as well (with no size rule: the
    // size is on vm_fs, and the chunk write that goes with a too-large file is refused by its vm_fs row).
    let ch_row = |r: &str| -> Option<String> {
        if prefixes.is_empty() { return None; }
        Some(prefixes.iter().map(|p| under_prefix(&format!("{}.path", r), p)).collect::<Vec<_>>().join(" OR "))
    };
    // An UPDATE is refused when either side is protected. Checking only NEW would let a file be moved
    // out of a protected directory: the old path is protected, the new one is not, and the row would
    // change. Checking only OLD would let one be moved in.
    for (name, event, from, to) in [
        ("vm_pol_fs_upd", "UPDATE", Some("OLD"), Some("NEW")),
        ("vm_pol_ch_upd", "UPDATE", Some("OLD"), Some("NEW")),
    ] {
        let conds: Vec<String> = [from, to].into_iter().flatten()
            .filter_map(|r| if name.starts_with("vm_pol_fs") { fs_row(r) } else { ch_row(r) })
            .collect();
        if !conds.is_empty() {
            let (table, _) = if name.starts_with("vm_pol_fs") { ("vm_fs", ()) } else { ("vm_file_chunks", ()) };
            sql.push_str(&format!(
                "CREATE TRIGGER {name} BEFORE {event} ON {table} WHEN NOT {bypass} AND ({cond}) BEGIN {refuse}; END;\n",
                name = name, event = event, table = table, bypass = bypass, cond = conds.join(" OR "), refuse = refuse));
        }
    }
    for (name, event, row) in [
        ("vm_pol_fs_ins", "INSERT", "NEW"), ("vm_pol_fs_del", "DELETE", "OLD"),
    ] {
        if let Some(cond) = fs_row(row) {
            sql.push_str(&format!(
                "CREATE TRIGGER {name} BEFORE {event} ON vm_fs WHEN NOT {bypass} AND ({cond}) BEGIN {refuse}; END;\n",
                name = name, event = event, bypass = bypass, cond = cond, refuse = refuse));
        }
    }
    for (name, event, row) in [
        ("vm_pol_ch_ins", "INSERT", "NEW"), ("vm_pol_ch_del", "DELETE", "OLD"),
    ] {
        if let Some(cond) = ch_row(row) {
            sql.push_str(&format!(
                "CREATE TRIGGER {name} BEFORE {event} ON vm_file_chunks WHEN NOT {bypass} AND ({cond}) BEGIN {refuse}; END;\n",
                name = name, event = event, bypass = bypass, cond = cond, refuse = refuse));
        }
    }
    conn.execute_batch(&sql)?;
    Ok(())
}

/// Largest size limit in force, if any. Enforcement is by trigger; this is for reporting.
pub fn size_limit(conn: &Connection) -> Result<Option<u64>> {
    let v: Option<String> = conn.query_row(
        "SELECT pattern FROM vm_policies WHERE rule_type='max_file_size' ORDER BY id LIMIT 1", [], |r| r.get(0)).optional()?;
    Ok(match v { Some(s) => Some(s.parse()?), None => None })
}
