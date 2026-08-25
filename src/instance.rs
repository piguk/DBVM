//! The default DBVM instance: one `.db` that `dbvm` operates on when no `--db` is given.

use crate::fetch;
use crate::vm;
use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Name of the file snapshot taken right after a rootfs import. `dbvm reset` rolls back to it.
pub const BASE_SNAPSHOT: &str = "base";

/// Where the default instance lives: `$DBVM_DB`, else `$XDG_DATA_HOME/dbvm/default.db`,
/// else `~/.local/share/dbvm/default.db`.
pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("DBVM_DB")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    if let Ok(x) = std::env::var("XDG_DATA_HOME")
        && !x.is_empty()
    {
        return PathBuf::from(x).join("dbvm").join("default.db");
    }
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return PathBuf::from(h)
            .join(".local/share/dbvm")
            .join("default.db");
    }
    std::env::temp_dir().join("dbvm").join("default.db")
}

/// A `.db` counts as an instance only once a rootfs has actually been imported;
/// an empty database would drop the user into a shell that does not exist.
pub fn is_populated(db: &Path) -> bool {
    if !db.exists() {
        return false;
    }
    let Ok(conn) = Connection::open(db) else {
        return false;
    };
    conn.query_row("SELECT count(*) FROM vm_fs WHERE kind='file'", [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Download the current Alpine minirootfs and import it into `db`, replacing whatever
/// was there. Leaves a `base` file snapshot behind for [`reset`].
pub fn provision(db: &Path, arch: Option<&str>) -> Result<fetch::Release> {
    let arch = match arch {
        Some(a) => a.to_string(),
        None => fetch::host_arch()?.to_string(),
    };
    let release = fetch::latest_release(&arch)?;
    eprintln!(
        "-> alpine {} ({}) from latest-stable",
        release.version, release.arch
    );

    let cache = cache_dir();
    let tarball = fetch::download(&release, &cache)
        .with_context(|| format!("downloading {}", release.file))?;

    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db_str = db.to_string_lossy().to_string();
    let conn = vm::init_vm_db(&db_str, true)?;
    let n = vm::vm_import_tar(&conn, &tarball.to_string_lossy(), "")?;
    eprintln!("-> imported {} entries -> {}", n, db.display());

    vm::vm_checkpoint(&conn, BASE_SNAPSHOT, &format!("alpine {}", release.version))?;
    vm::vm_snapshot_file(&conn, &db_str, BASE_SNAPSHOT)?;
    Ok(release)
}

/// Open the default instance, provisioning it on first use.
pub fn open_or_provision(db: &Path, arch: Option<&str>) -> Result<Connection> {
    if !is_populated(db) {
        eprintln!("-> no instance at {}", db.display());
        provision(db, arch)?;
    }
    vm::vm_open(&db.to_string_lossy())
}

/// Roll the instance back to the `base` snapshot taken at import time.
pub fn reset(db: &Path) -> Result<()> {
    let db_str = db.to_string_lossy().to_string();
    if !snapshot_file_path(&db_str, BASE_SNAPSHOT).exists() {
        bail!(
            "no {} snapshot next to {}; run `dbvm reset --hard` to re-provision",
            BASE_SNAPSHOT,
            db.display()
        );
    }
    vm::vm_restore_file(&db_str, BASE_SNAPSHOT)?;
    Ok(())
}

/// Discard the instance entirely and provision a fresh one from latest-stable.
pub fn reset_hard(db: &Path, arch: Option<&str>) -> Result<fetch::Release> {
    let db_str = db.to_string_lossy().to_string();
    for stale in [
        db.to_path_buf(),
        PathBuf::from(format!("{db_str}-wal")),
        PathBuf::from(format!("{db_str}-shm")),
        snapshot_file_path(&db_str, BASE_SNAPSHOT),
    ] {
        if stale.exists() {
            std::fs::remove_file(&stale)
                .with_context(|| format!("removing {}", stale.display()))?;
        }
    }
    provision(db, arch)
}

/// Whether [`reset`] has something to roll back to.
pub fn has_base_snapshot(db: &Path) -> bool {
    snapshot_file_path(&db.to_string_lossy(), BASE_SNAPSHOT).exists()
}

/// Mirrors the naming used by [`vm::vm_snapshot_file`].
fn snapshot_file_path(db: &str, name: &str) -> PathBuf {
    PathBuf::from(format!("{db}.snap.{name}"))
}

/// Downloaded rootfs tarballs are kept out of the instance directory so that
/// `reset --hard` does not have to re-download.
fn cache_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME")
        && !x.is_empty()
    {
        return PathBuf::from(x).join("dbvm");
    }
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return PathBuf::from(h).join(".cache/dbvm");
    }
    std::env::temp_dir().join("dbvm-cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_path_sits_next_to_the_db() {
        assert_eq!(
            snapshot_file_path("/tmp/x.db", BASE_SNAPSHOT),
            PathBuf::from("/tmp/x.db.snap.base")
        );
    }

    #[test]
    fn a_missing_db_is_not_an_instance() {
        assert!(!is_populated(Path::new("/nonexistent/dbvm-test.db")));
    }
}
