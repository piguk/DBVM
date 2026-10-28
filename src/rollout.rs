//! Rollouts: long-lived sandboxes whose root is an overlay of shared, read-only layers.
//!
//! A session over a materialized tree pays a materialize per sandbox (1.8 s at 49k entries, measured). A
//! rollout instead mounts the template's layers (read-only, shared by every rollout of that template) under
//! a private upper directory, so starting one costs a few directories: 1000 overlay sandboxes started in
//! 2.0 s and used about 1 MB each when idle (measured). bwrap only: gVisor keeps an overlay's writes in its
//! own file format, which the host cannot read per path.
//!
//! Every write of the rollout lands in its upper directory, so the upper directory is the rollout's change
//! set: each action's changes are a scan of the upper against the previous scan, in proportion to what the
//! rollout changed, not to the template. Nothing is written to a database per action; a rollout's state is
//! persisted by freezing it into a template (see `vm::vm_apply_upper`).
//!
//! overlayfs records what the upper hides: a deleted lower entry is a whiteout (a 0:0 character device), and a
//! directory recreated after its lower contents were removed carries `user.overlay.opaque = y` (unprivileged
//! overlay mounts use the `user.` namespace). Copying an upper with `cp -a` keeps both (measured), which is
//! how a fork turns a running rollout's state into a layer.

use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::sandbox::{self, Isolation, Limits, Outcome, RoOverlay};
use crate::session::supervisor::{DirEntry, Supervisor};
use crate::vm::{self, SnapDiff, WorkDir};

#[derive(Debug, Clone)]
pub struct RolloutOptions {
    pub limits: Limits,
    /// The environment every command starts with.
    pub env: Vec<(String, String)>,
    /// Paths the rollout must not change: mounted read-only from the layers, before the rollout starts.
    pub readonly: Vec<String>,
    /// A host directory holding the egress proxy's `proxy.sock`; the sandbox reaches it through the
    /// supervisor's forwarder at 127.0.0.1:PROXY_PORT. None: no network at all.
    pub egress: Option<PathBuf>,
    /// Where the rollout's upper directory and control files go; $TMPDIR when None. A service puts them on
    /// its data disk: a rollout's upper holds everything it writes.
    pub work_root: Option<PathBuf>,
}

impl Default for RolloutOptions {
    fn default() -> Self {
        RolloutOptions {
            limits: Limits::default(),
            env: vec![("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()), ("HOME".into(), "/root".into())],
            readonly: Vec::new(),
            egress: None,
            work_root: None,
        }
    }
}

/// The result of one action: what the sandbox reported, and what it changed.
#[derive(Debug)]
pub struct ActionResult {
    pub outcome: Outcome,
    pub changes: SnapDiff,
    pub duration_ms: u128,
}

pub struct Rollout {
    /// upper/, work/, the supervisor's control directory; reclaimed if this process is killed.
    work: WorkDir,
    layers: Vec<PathBuf>,
    sup: Option<Supervisor>,
    env: Vec<(String, String)>,
    /// The upper directory as of the end of the previous action.
    scan: BTreeMap<String, Entry>,
}

/// What the scan keeps per upper entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    kind: Kind,
    mode: u32,
    size: u64,
    mtime_ns: i64,
    link: Option<PathBuf>,
    opaque: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind { File, Dir, Symlink, Whiteout, Other }

fn entry_of(path: &Path, md: &std::fs::Metadata) -> Entry {
    let ft = md.file_type();
    let kind = if is_whiteout(md) { Kind::Whiteout }
        else if ft.is_dir() { Kind::Dir } else if ft.is_symlink() { Kind::Symlink } else if ft.is_file() { Kind::File } else { Kind::Other };
    let opaque = kind == Kind::Dir && is_opaque(path);
    Entry {
        kind, mode: md.mode() & 0o7777, size: md.size(), mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        link: if kind == Kind::Symlink { std::fs::read_link(path).ok() } else { None }, opaque,
    }
}

fn is_opaque(dir: &Path) -> bool {
    let (Ok(p), Ok(name)) = (std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()), std::ffi::CString::new("user.overlay.opaque")) else { return false };
    let mut v = [0u8; 4];
    let n = unsafe { libc::lgetxattr(p.as_ptr(), name.as_ptr(), v.as_mut_ptr().cast(), v.len()) };
    n == 1 && v[0] == b'y'
}

fn host(dir: &Path, vm_path: &str) -> PathBuf { dir.join(vm_path.trim_start_matches('/')) }

/// What the read-only layers show at `vm_path` (top layer first, whiteouts and opaque directories applied):
/// the host path of the entry that wins, or None when the layers have nothing there.
pub fn lower_lookup(layers: &[PathBuf], vm_path: &str) -> Option<PathBuf> {
    let p = vm::normalize_vm_path(vm_path);
    let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
    for layer in layers.iter().rev() {
        // an ancestor in this layer can hide the path (whiteout) or everything below it (opaque)
        let mut cut = false;
        let mut acc = layer.clone();
        for c in &comps[..comps.len().saturating_sub(1)] {
            acc.push(c);
            match std::fs::symlink_metadata(&acc) {
                Ok(md) if is_whiteout(&md) => return None,
                Ok(md) if md.is_dir() && is_opaque(&acc) => cut = true,
                _ => {}
            }
        }
        let hp = host(layer, &p);
        match std::fs::symlink_metadata(&hp) {
            Ok(md) if is_whiteout(&md) => return None,
            Ok(_) => return Some(hp),
            Err(_) => {}
        }
        if cut { return None; }
    }
    None
}

fn is_whiteout(md: &std::fs::Metadata) -> bool { md.file_type().is_char_device() && md.rdev() == 0 }

/// The layers that contribute to the directory `vm_path` (bottom first), for mounting it on its own. Read
/// top down: a whiteout of the path or of an ancestor ends it, an opaque directory (the path's or an
/// ancestor's) still counts but ends it below, and a non-directory hides the lower directories.
fn layers_of_dir(layers: &[PathBuf], vm_path: &str) -> Vec<PathBuf> {
    let p = vm::normalize_vm_path(vm_path);
    let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
    let mut out = Vec::new();
    'layers: for layer in layers.iter().rev() {
        let mut cut = false;
        let mut acc = layer.clone();
        for c in &comps[..comps.len().saturating_sub(1)] {
            acc.push(c);
            match std::fs::symlink_metadata(&acc) {
                Ok(md) if is_whiteout(&md) => break 'layers,
                Ok(md) if md.is_dir() && is_opaque(&acc) => cut = true,
                _ => {}
            }
        }
        let hp = host(layer, &p);
        match std::fs::symlink_metadata(&hp) {
            Ok(md) if md.is_dir() => { out.push(hp.clone()); if is_opaque(&hp) { break; } }
            Ok(_) => break,
            Err(_) => {}
        }
        if cut { break; }
    }
    out.reverse();
    out
}

/// Every path below directory `vm_path` as the layers show it.
fn lower_tree(layers: &[PathBuf], vm_path: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for dir in layers_of_dir(layers, vm_path) {
        for e in walkdir::WalkDir::new(&dir).min_depth(1).into_iter().flatten() {
            let rel = e.path().strip_prefix(&dir).unwrap_or(e.path()).to_string_lossy().to_string();
            let p = vm::normalize_vm_path(&format!("{}/{}", vm_path, rel));
            if lower_lookup(layers, &p).is_some() { out.insert(p); }
        }
    }
    out
}

/// Whether two files hold the same bytes.
fn same_content(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let (Ok(mut fa), Ok(mut fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else { return false };
    let (mut ba, mut bb) = (vec![0u8; 65536], vec![0u8; 65536]);
    loop {
        let (Ok(na), Ok(nb)) = (fa.read(&mut ba), fb.read(&mut bb)) else { return false };
        if na != nb || ba[..na] != bb[..nb] { return false; }
        if na == 0 { return true; }
    }
}

impl Rollout {
    /// Start a rollout over `layers` (bottom first). The layers are only read.
    pub fn start(layers: &[PathBuf], opts: &RolloutOptions) -> Result<Rollout> {
        if layers.is_empty() { return Err(anyhow!("a rollout needs at least one layer")); }
        let work = match &opts.work_root { Some(r) => WorkDir::create_in(r, "rollout-")?, None => WorkDir::create("rollout-")? };
        let (upper, ovl_work) = (work.path().join("upper"), work.path().join("work"));
        std::fs::create_dir_all(&upper)?;
        std::fs::create_dir_all(&ovl_work)?;
        let mut spec = sandbox::Spec::new(&upper, Vec::new());
        spec.layers = layers.to_vec();
        spec.upper = Some((upper, ovl_work));
        spec.limits = opts.limits;
        for p in &opts.readonly {
            let p = vm::normalize_vm_path(p);
            let srcs = layers_of_dir(layers, &p);
            if !srcs.is_empty() {
                spec.ro_overlays.push(RoOverlay { layers: srcs, dest: p });
            } else if let Some(file) = lower_lookup(layers, &p) {
                spec.binds.push(sandbox::Bind { host: file, dest: p, writable: false });
            } else {
                // absent from the template: an empty read-only directory, so the rollout cannot create it
                let empty = work.path().join("empty");
                std::fs::create_dir_all(&empty)?;
                spec.ro_overlays.push(RoOverlay { layers: vec![empty], dest: p });
            }
        }
        let sup = Supervisor::start(spec, Isolation::Bwrap, work.path(), opts.egress.as_deref())?;
        let mut env = opts.env.clone();
        if opts.egress.is_some() {
            // the forwarder inside the sandbox; pip, npm, git and curl read these
            let proxy = format!("http://127.0.0.1:{}", crate::session::supervisor::PROXY_PORT);
            for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                env.retain(|(name, _)| name != k);
                env.push((k.into(), proxy.clone()));
            }
        }
        let mut r = Rollout { work, layers: layers.to_vec(), sup: Some(sup), env, scan: BTreeMap::new() };
        // the sandbox's own mount points (/.selfdb, read-only paths) are not the rollout's changes
        r.scan = r.scan_upper();
        Ok(r)
    }

    pub fn layers(&self) -> &[PathBuf] { &self.layers }
    pub fn upper(&self) -> PathBuf { self.work.path().join("upper") }

    fn sup(&mut self) -> Result<&mut Supervisor> { self.sup.as_mut().ok_or_else(|| anyhow!("the rollout is closed")) }

    /// CPU time used by the rollout's sandbox so far, in microseconds.
    pub fn cpu_usec(&mut self) -> Option<u64> { self.sup.as_mut().and_then(|s| s.cpu_usec()) }

    /// Bytes the rollout has written so far (the size of its upper directory).
    pub fn upper_bytes(&self) -> u64 {
        walkdir::WalkDir::new(self.upper()).into_iter().flatten()
            .filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
    }

    /// Run a command.
    pub fn exec(&mut self, argv: &[String], cwd: &str, timeout: Duration, max_output: usize) -> Result<ActionResult> {
        let started = Instant::now();
        let env = self.env.clone();
        let outcome = self.sup()?.exec(argv, &env, cwd, timeout, max_output)?;
        Ok(self.finish(outcome, started))
    }

    /// Write a file (parent directories created). A refused write is a failed action with the message in stderr.
    pub fn write_file(&mut self, path: &str, data: &[u8], mode: u32) -> Result<ActionResult> {
        let started = Instant::now();
        let outcome = match self.sup()?.write(path, data, mode)? {
            Ok(()) => Outcome::default(),
            Err(msg) => Outcome { exit_code: 1, stderr: msg.into_bytes(), ..Outcome::default() },
        };
        Ok(self.finish(outcome, started))
    }

    pub fn read_file(&mut self, path: &str, offset: u64, len: u64) -> Result<std::result::Result<Vec<u8>, String>> {
        self.sup()?.read(path, offset, len)
    }

    pub fn list_dir(&mut self, path: &str) -> Result<std::result::Result<Vec<DirEntry>, String>> {
        self.sup()?.list(path)
    }

    fn finish(&mut self, outcome: Outcome, started: Instant) -> ActionResult {
        let cur = self.scan_upper();
        let changes = self.diff(&cur);
        self.scan = cur;
        ActionResult { outcome, changes, duration_ms: started.elapsed().as_millis() }
    }

    fn scan_upper(&self) -> BTreeMap<String, Entry> {
        let upper = self.upper();
        let mut out = BTreeMap::new();
        for e in walkdir::WalkDir::new(&upper).min_depth(1).follow_links(false).into_iter().flatten() {
            let rel = e.path().strip_prefix(&upper).unwrap_or(e.path()).to_string_lossy().to_string();
            let p = vm::normalize_vm_path(&format!("/{}", rel));
            if vm::is_reserved_vm_path(&p) { continue; }
            if let Ok(md) = std::fs::symlink_metadata(e.path()) { out.insert(p, entry_of(e.path(), &md)); }
        }
        out
    }

    /// The changes between the previous scan and `cur`, as the rollout's merged view sees them.
    fn diff(&self, cur: &BTreeMap<String, Entry>) -> SnapDiff {
        let mut created = BTreeSet::new();
        let mut modified = BTreeSet::new();
        let mut deleted = BTreeSet::new();
        let upper = self.upper();
        for (p, e) in cur {
            let prev = self.scan.get(p);
            if prev == Some(e) { continue; }
            let lower = lower_lookup(&self.layers, p);
            let was_visible = match prev { Some(pe) => pe.kind != Kind::Whiteout, None => lower.is_some() };
            if e.kind == Kind::Whiteout {
                if was_visible { deleted.insert(p.clone()); }
                // everything the layers had below a deleted directory is gone with it
                if lower.as_ref().map(|l| l.is_dir()).unwrap_or(false) { deleted.extend(lower_tree(&self.layers, p)); }
                continue;
            }
            if !was_visible { created.insert(p.clone()); }
            else if self.changed_from(prev, e, &host(&upper, p), lower.as_deref()) { modified.insert(p.clone()); }
            // a directory made opaque hides what the layers had below it
            if e.kind == Kind::Dir && e.opaque && !prev.map(|pe| pe.opaque).unwrap_or(false) {
                for below in lower_tree(&self.layers, p) {
                    if !cur.contains_key(&below) { deleted.insert(below); }
                }
            }
        }
        for (p, pe) in &self.scan {
            if cur.contains_key(p) || pe.kind == Kind::Whiteout { continue; }
            // an entry that only ever existed in the upper, removed again
            deleted.insert(p.clone());
        }
        for p in &deleted { created.remove(p); modified.remove(p); }
        SnapDiff { created: created.into_iter().collect(), modified: modified.into_iter().collect(), deleted: deleted.into_iter().collect() }
    }

    /// Whether an upper entry changed what the path shows. A directory copied up only because something below
    /// it changed is not a change; neither is a file copied up by an open for writing that wrote nothing.
    fn changed_from(&self, prev: Option<&Entry>, e: &Entry, upper_path: &Path, lower: Option<&Path>) -> bool {
        let base = match prev {
            Some(pe) => (pe.kind, pe.mode, pe.link.clone()),
            None => {
                let Some(l) = lower else { return true };
                let Ok(md) = std::fs::symlink_metadata(l) else { return true };
                let le = entry_of(l, &md);
                if e.kind == Kind::File && le.kind == Kind::File && e.mode == le.mode {
                    return !(e.size == le.size && same_content(upper_path, l));
                }
                (le.kind, le.mode, le.link)
            }
        };
        match e.kind {
            Kind::Dir => base.0 != Kind::Dir || base.1 != e.mode,
            Kind::Symlink => base.0 != Kind::Symlink || base.2 != e.link,
            _ => match prev { Some(pe) => pe != e, None => true },
        }
    }

    /// Copy the upper directory to `dest`, keeping whiteouts and opaque markers: the rollout's state as a
    /// layer, for forks and freezes. The rollout keeps running.
    pub fn snapshot_upper(&self, dest: &Path) -> Result<()> {
        copy_layer(&self.upper(), dest)
    }

    pub fn close(self) {}
}

impl Drop for Rollout {
    fn drop(&mut self) {
        // the sandbox first: it holds the upper directory mounted
        self.sup = None;
    }
}

/// Copy a layer directory to `dest` (which must not exist), keeping whiteouts, xattrs, modes and links.
pub fn copy_layer(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() { return Err(anyhow!("{} exists", dest.display())); }
    std::fs::create_dir_all(dest)?;
    let st = std::process::Command::new("cp").arg("-a").arg("--no-target-directory")
        .arg(src).arg(dest).stderr(std::process::Stdio::piped()).output()?;
    if !st.status.success() {
        return Err(anyhow!("copy layer {}: {}", src.display(), String::from_utf8_lossy(&st.stderr).trim()));
    }
    Ok(())
}
