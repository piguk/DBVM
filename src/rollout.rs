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
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::sandbox::{self, Isolation, Limits, Outcome, RoOverlay};
use crate::session::supervisor::{DirEntry, Supervisor};
use crate::vm::{self, SnapDiff, WorkDir};
use crate::watch::TreeWatch;

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
    /// After an action only the paths the watcher reports are looked at (see `watch`); every this many
    /// actions the whole upper directory is walked instead, to catch what raises no event (writes through
    /// a shared memory mapping). 1: walk after every action.
    pub walk_every: u32,
}

impl Default for RolloutOptions {
    fn default() -> Self {
        RolloutOptions {
            limits: Limits::default(),
            env: vec![("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()), ("HOME".into(), "/root".into())],
            readonly: Vec::new(),
            egress: None,
            work_root: None,
            walk_every: 64,
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
    /// The size of the regular files in `scan`.
    bytes: u64,
    /// The paths of each regular file's inode in `scan`. inotify reports a change under the name it was made
    /// through; a file's other names (hard links) changed too (measured: a chmod through one name left the
    /// other's change unreported).
    by_ino: HashMap<u64, BTreeSet<String>>,
    watch: TreeWatch,
    walk_every: u32,
    since_walk: u32,
}

/// What the scan keeps per upper entry.
#[derive(Debug, Clone)]
struct Entry {
    kind: Kind,
    mode: u32,
    size: u64,
    mtime_ns: i64,
    link: Option<PathBuf>,
    opaque: bool,
    /// A regular file's inode (0 otherwise): its hard links change with it. Not part of what the entry
    /// shows, so not compared.
    ino: u64,
}

impl PartialEq for Entry {
    fn eq(&self, o: &Entry) -> bool {
        (self.kind, self.mode, self.size, self.mtime_ns, &self.link, self.opaque) == (o.kind, o.mode, o.size, o.mtime_ns, &o.link, o.opaque)
    }
}

impl Eq for Entry {}

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
        ino: if kind == Kind::File { md.ino() } else { 0 },
    }
}

fn is_opaque(dir: &Path) -> bool {
    let (Ok(p), Ok(name)) = (std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()), std::ffi::CString::new("user.overlay.opaque")) else { return false };
    let mut v = [0u8; 4];
    let n = unsafe { libc::lgetxattr(p.as_ptr(), name.as_ptr(), v.as_mut_ptr().cast(), v.len()) };
    n == 1 && v[0] == b'y'
}

/// A walk entry, None if it vanished meanwhile (the rollout's processes may be deleting files while the
/// upper is scanned); any other error is one.
fn vanished_ok(e: walkdir::Result<walkdir::DirEntry>) -> Result<Option<walkdir::DirEntry>> {
    match e {
        Ok(e) => Ok(Some(e)),
        Err(err) if err.io_error().map(|i| i.kind()) == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(err) => Err(anyhow!("scan the upper directory: {}", err)),
    }
}

fn upper_dir(work: &WorkDir) -> PathBuf { work.path().join("upper") }

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
fn lower_tree(layers: &[PathBuf], vm_path: &str) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for dir in layers_of_dir(layers, vm_path) {
        for e in walkdir::WalkDir::new(&dir).min_depth(1) {
            let e = e?;
            let rel = e.path().strip_prefix(&dir).unwrap_or(e.path()).to_string_lossy().to_string();
            let p = vm::normalize_vm_path(&format!("{}/{}", vm_path, rel));
            if lower_lookup(layers, &p).is_some() { out.insert(p); }
        }
    }
    Ok(out)
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
        // watching starts before the sandbox does, so no write of the rollout is missed
        // everything in the upper counts (the disk quota); reserved paths are left out of change lists only
        let mut watch = TreeWatch::new(&upper_dir(&work), |_| false);
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
        // the sandbox's own mount points (/.selfdb, read-only paths) are not the rollout's changes
        let _ = watch.take();
        let mut r = Rollout { work, layers: layers.to_vec(), sup: Some(sup), env, scan: BTreeMap::new(), bytes: 0, by_ino: HashMap::new(), watch,
                              walk_every: opts.walk_every.max(1), since_walk: 0 };
        r.rescan_all()?;
        Ok(r)
    }

    pub fn layers(&self) -> &[PathBuf] { &self.layers }
    pub fn upper(&self) -> PathBuf { upper_dir(&self.work) }

    fn sup(&mut self) -> Result<&mut Supervisor> { self.sup.as_mut().ok_or_else(|| anyhow!("the rollout is closed")) }

    /// CPU time used by the rollout's sandbox so far, in microseconds.
    pub fn cpu_usec(&mut self) -> Option<u64> { self.sup.as_mut().and_then(|s| s.cpu_usec()) }

    /// Take the upper directory as it is now as the starting point: the next action's changes are those
    /// after this (what a task's services wrote while starting is not the agent's).
    pub fn rebase(&mut self) -> Result<()> {
        let _ = self.watch.take();
        self.rescan_all()?;
        Ok(())
    }

    /// Bytes the rollout has written, as of the end of its last action (the regular files of its upper
    /// directory).
    pub fn upper_bytes(&self) -> u64 { self.bytes }

    /// Run a command.
    pub fn exec(&mut self, argv: &[String], cwd: &str, timeout: Duration, max_output: usize) -> Result<ActionResult> {
        let started = Instant::now();
        let env = self.env.clone();
        let outcome = self.sup()?.exec(argv, &env, cwd, timeout, max_output)?;
        self.finish(outcome, started)
    }

    /// Write a file (parent directories created; `mode` as in `Supervisor::write`). A refused write is a failed
    /// action with the message in stderr.
    pub fn write_file(&mut self, path: &str, data: &[u8], mode: Option<u32>) -> Result<ActionResult> {
        let started = Instant::now();
        let outcome = match self.sup()?.write(path, data, mode)? {
            Ok(()) => Outcome::default(),
            Err(msg) => Outcome { exit_code: 1, stderr: msg.into_bytes(), ..Outcome::default() },
        };
        self.finish(outcome, started)
    }

    pub fn read_file(&mut self, path: &str, offset: u64, len: u64) -> Result<std::result::Result<Vec<u8>, String>> {
        self.sup()?.read(path, offset, len)
    }

    pub fn list_dir(&mut self, path: &str) -> Result<std::result::Result<Vec<DirEntry>, String>> {
        self.sup()?.list(path)
    }

    fn finish(&mut self, outcome: Outcome, started: Instant) -> Result<ActionResult> {
        let seen = self.watch.take();
        self.since_walk += 1;
        let changed = if seen.full || self.since_walk >= self.walk_every { self.rescan_all()? } else { self.rescan(&seen.paths)? };
        let changes = self.diff(&changed)?;
        Ok(ActionResult { outcome, changes, duration_ms: started.elapsed().as_millis() })
    }

    /// Replace the scan with a walk of the whole upper directory. Returns the entries that differ, each
    /// with its previous value.
    fn rescan_all(&mut self) -> Result<Vec<(String, Option<Entry>)>> {
        let cur = self.scan_upper()?;
        let mut changed: Vec<(String, Option<Entry>)> = Vec::new();
        for (p, e) in &cur {
            let prev = self.scan.get(p);
            if prev != Some(e) { changed.push((p.clone(), prev.cloned())); }
        }
        for (p, pe) in &self.scan {
            if !cur.contains_key(p) { changed.push((p.clone(), Some(pe.clone()))); }
        }
        self.bytes = cur.values().filter(|e| e.kind == Kind::File).map(|e| e.size).sum();
        self.by_ino.clear();
        for (p, e) in &cur {
            if e.kind == Kind::File { self.by_ino.entry(e.ino).or_default().insert(p.clone()); }
        }
        self.scan = cur;
        self.since_walk = 0;
        Ok(changed)
    }

    /// Update the scan at `paths` only, the other names of the files among them, and what was below those
    /// that are no longer directories. Returns the entries that differ, each with its previous value.
    fn rescan(&mut self, paths: &BTreeSet<String>) -> Result<Vec<(String, Option<Entry>)>> {
        let upper = self.upper();
        let mut changed = Vec::new();
        let mut queue: Vec<String> = paths.iter().rev().cloned().collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(p) = queue.pop() {
            if p == "/" || !seen.insert(p.clone()) { continue; }
            let hp = host(&upper, &p);
            let now = match std::fs::symlink_metadata(&hp) {
                Ok(md) => Some(entry_of(&hp, &md)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(anyhow!("scan {}: {}", hp.display(), err)),
            };
            for e in [self.scan.get(&p), now.as_ref()].into_iter().flatten().filter(|e| e.kind == Kind::File) {
                if let Some(names) = self.by_ino.get(&e.ino) {
                    queue.extend(names.iter().filter(|n| !seen.contains(*n)).cloned());
                }
            }
            if now.as_ref().map(|e| e.kind != Kind::Dir).unwrap_or(true) {
                // not a directory (any more): nothing the scan had below it is there
                let prefix = format!("{}/", p);
                let below: Vec<String> = self.scan.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(k, _)| k.clone()).collect();
                for k in below { self.set(k, None, &mut changed); }
            }
            self.set(p, now, &mut changed);
        }
        Ok(changed)
    }

    fn set(&mut self, p: String, now: Option<Entry>, changed: &mut Vec<(String, Option<Entry>)>) {
        let prev = self.scan.get(&p).cloned();
        if let Some(e) = prev.as_ref().filter(|e| e.kind == Kind::File) {
            if let Some(names) = self.by_ino.get_mut(&e.ino) { names.remove(&p); if names.is_empty() { self.by_ino.remove(&e.ino); } }
        }
        if let Some(e) = now.as_ref().filter(|e| e.kind == Kind::File) { self.by_ino.entry(e.ino).or_default().insert(p.clone()); }
        let size = |e: &Option<Entry>| e.as_ref().filter(|e| e.kind == Kind::File).map(|e| e.size).unwrap_or(0);
        self.bytes = self.bytes + size(&now) - size(&prev);
        // kept even when it shows the same (the inode may differ)
        match &now { Some(e) => { self.scan.insert(p.clone(), e.clone()); } None => { self.scan.remove(&p); } }
        if prev != now { changed.push((p, prev)); }
    }

    fn scan_upper(&self) -> Result<BTreeMap<String, Entry>> {
        let upper = self.upper();
        let mut out = BTreeMap::new();
        for e in walkdir::WalkDir::new(&upper).min_depth(1).follow_links(false) {
            let Some(e) = vanished_ok(e)? else { continue };
            let rel = e.path().strip_prefix(&upper).unwrap_or(e.path()).to_string_lossy().to_string();
            let p = vm::normalize_vm_path(&format!("/{}", rel));
            match std::fs::symlink_metadata(e.path()) {
                Ok(md) => { out.insert(p, entry_of(e.path(), &md)); }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(anyhow!("scan {}: {}", e.path().display(), err)),
            }
        }
        Ok(out)
    }

    /// What `changed` (upper entries with their previous values; the scan holds the current ones) changed
    /// in the rollout's merged view.
    fn diff(&self, changed: &[(String, Option<Entry>)]) -> Result<SnapDiff> {
        let mut created = BTreeSet::new();
        let mut modified = BTreeSet::new();
        let mut deleted = BTreeSet::new();
        let upper = self.upper();
        for (p, prev) in changed {
            let prev = prev.as_ref();
            let Some(e) = self.scan.get(p) else {
                // an entry that only ever existed in the upper, removed again
                if prev.map(|pe| pe.kind != Kind::Whiteout).unwrap_or(false) { deleted.insert(p.clone()); }
                continue;
            };
            let lower = lower_lookup(&self.layers, p);
            let was_visible = match prev { Some(pe) => pe.kind != Kind::Whiteout, None => lower.is_some() };
            if e.kind == Kind::Whiteout {
                if was_visible { deleted.insert(p.clone()); }
                // everything the layers had below a deleted directory is gone with it
                if lower.as_ref().map(|l| l.is_dir()).unwrap_or(false) { deleted.extend(lower_tree(&self.layers, p)?); }
                continue;
            }
            if !was_visible { created.insert(p.clone()); }
            else if self.changed_from(prev, e, &host(&upper, p), lower.as_deref()) { modified.insert(p.clone()); }
            // a directory made opaque hides what the layers had below it
            if e.kind == Kind::Dir && e.opaque && !prev.map(|pe| pe.opaque).unwrap_or(false) {
                for below in lower_tree(&self.layers, p)? {
                    if !self.scan.contains_key(&below) { deleted.insert(below); }
                }
            }
        }
        for p in &deleted { created.remove(p); modified.remove(p); }
        // runtime artefacts (/tmp, __pycache__, the sandbox's mount points) are not the rollout's changes
        for set in [&mut created, &mut modified, &mut deleted] { set.retain(|p| !vm::is_reserved_vm_path(p)); }
        Ok(SnapDiff { created: created.into_iter().collect(), modified: modified.into_iter().collect(), deleted: deleted.into_iter().collect() })
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
    /// layer, for forks and freezes. Every process of the rollout is frozen during the copy, so background
    /// processes that keep writing cannot leave a mix of older and newer files in it; the rollout then keeps
    /// running. Only files are captured: processes and their memory are not.
    pub fn snapshot_upper(&self, dest: &Path) -> Result<()> {
        let _frozen = self.freeze()?;
        copy_layer(&self.upper(), dest)
    }

    /// Stop every process of the rollout until the guard drops, so that its upper directory can be read as
    /// one moment of it.
    pub fn freeze(&self) -> Result<sandbox::Frozen> {
        self.sup.as_ref().ok_or_else(|| anyhow!("the rollout is closed"))?.freeze()
    }

    pub fn close(self) {}
}

impl Drop for Rollout {
    fn drop(&mut self) {
        // the sandbox first: it holds the upper directory mounted
        self.sup = None;
    }
}

/// Merge `layers` (bottom first; each an upper directory over the ones below) into one new layer at `dest`
/// that shows the same tree over whatever lies below the bottom one: a fork of a fork of ... would otherwise
/// add one overlay layer per generation.
pub fn merge_layers(layers: &[PathBuf], dest: &Path) -> Result<()> {
    let (first, rest) = layers.split_first().ok_or_else(|| anyhow!("nothing to merge"))?;
    copy_layer(first, dest)?;
    for layer in rest { copy_into(layer, dest, Onto::Layer)?; }
    Ok(())
}

/// Copy a layer directory (or one entry of it) to `dest`, which must not exist: whiteouts, `user.*` xattrs
/// (overlay markers among them), modes, times, links and hard links kept. In-process, so the copy has the
/// service's access to modes the sandbox set (see `access`); unreadable entries are an error, not skipped.
pub fn copy_layer(src: &Path, dest: &Path) -> Result<()> {
    if std::fs::symlink_metadata(dest).is_ok() { return Err(anyhow!("{} exists", dest.display())); }
    if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent)?; }
    copy_into(src, dest, Onto::Empty)
}

/// What `copy_into` writes over.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Onto {
    /// Nothing: `dest` does not exist.
    Empty,
    /// A layer that `src` is stacked on: the result shows what the two stacked showed.
    Layer,
}

fn copy_into(src: &Path, dest: &Path, onto: Onto) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut links: HashMap<(u64, u64), PathBuf> = HashMap::new();
    // directories get their modes, times and markers after their contents are in
    let mut dirs: Vec<(PathBuf, PathBuf, std::fs::Metadata, bool)> = Vec::new();
    for e in walkdir::WalkDir::new(src).follow_links(false) {
        let e = e.map_err(|err| anyhow!("read {}: {}", src.display(), err))?;
        let rel = e.path().strip_prefix(src).unwrap_or(e.path());
        let d = if rel.as_os_str().is_empty() { dest.to_path_buf() } else { dest.join(rel) };
        let md = std::fs::symlink_metadata(e.path())?;
        let below = std::fs::symlink_metadata(&d).ok();
        let ft = md.file_type();
        if ft.is_dir() {
            let opaque = is_opaque(e.path());
            let mut make_opaque = false;
            match below {
                // an opaque directory hides what was below it; a plain one merges with a directory below
                Some(b) if b.is_dir() && !opaque => {}
                Some(b) => {
                    remove_entry(&d, &b)?;
                    // a directory over a whiteout or a file hid everything further down too
                    make_opaque = onto == Onto::Layer && !b.is_dir() && !opaque;
                    std::fs::create_dir(&d)?;
                }
                None => std::fs::create_dir(&d)?,
            }
            dirs.push((e.path().to_path_buf(), d, md, make_opaque));
            continue;
        }
        if let Some(b) = below { remove_entry(&d, &b)?; }
        if !ft.is_symlink() && md.nlink() > 1 {
            if let Some(first) = links.get(&(md.dev(), md.ino())) {
                std::fs::hard_link(first, &d)?;
                continue;
            }
            links.insert((md.dev(), md.ino()), d.clone());
        }
        if ft.is_file() {
            std::fs::copy(e.path(), &d).map_err(|err| anyhow!("copy {}: {}", e.path().display(), err))?;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(e.path())?, &d)?;
        } else {
            let kind = if ft.is_char_device() && md.rdev() == 0 { libc::S_IFCHR }
                else if ft.is_fifo() { libc::S_IFIFO } else if ft.is_socket() { libc::S_IFSOCK }
                else { return Err(anyhow!("{}: device nodes are not copied", e.path().display())) };
            let c = std::ffi::CString::new(d.as_os_str().as_encoded_bytes())?;
            if unsafe { libc::mknod(c.as_ptr(), kind | (md.mode() & 0o7777), 0) } != 0 {
                return Err(anyhow!("mknod {}: {}", d.display(), std::io::Error::last_os_error()));
            }
        }
        copy_attrs(e.path(), &d, &md)?;
    }
    for (s, d, md, make_opaque) in dirs.iter().rev() {
        copy_attrs(s, d, md)?;
        if *make_opaque { set_opaque(d)?; }
    }
    Ok(())
}

/// `user.*` xattrs, mode and times of `src` onto `dest` (a symlink: xattrs and times only).
fn copy_attrs(src: &Path, dest: &Path, md: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let (cs, cd) = (std::ffi::CString::new(src.as_os_str().as_encoded_bytes())?, std::ffi::CString::new(dest.as_os_str().as_encoded_bytes())?);
    let size = unsafe { libc::llistxattr(cs.as_ptr(), std::ptr::null_mut(), 0) };
    let mut names = vec![0u8; size.max(0) as usize];
    let n = if names.is_empty() { 0 } else { unsafe { libc::llistxattr(cs.as_ptr(), names.as_mut_ptr().cast(), names.len()) } };
    if size < 0 || n < 0 { return Err(anyhow!("list xattrs of {}: {}", src.display(), std::io::Error::last_os_error())); }
    for name in names[..n as usize].split(|b| *b == 0).filter(|n| n.starts_with(b"user.")) {
        let cname = std::ffi::CString::new(name)?;
        // XATTR_SIZE_MAX
        let mut v = vec![0u8; 65536];
        let len = unsafe { libc::lgetxattr(cs.as_ptr(), cname.as_ptr(), v.as_mut_ptr().cast(), v.len()) };
        if len < 0 { return Err(anyhow!("read xattr of {}: {}", src.display(), std::io::Error::last_os_error())); }
        if unsafe { libc::lsetxattr(cd.as_ptr(), cname.as_ptr(), v.as_ptr().cast(), len as usize, 0) } != 0 {
            return Err(anyhow!("set xattr on {}: {}", dest.display(), std::io::Error::last_os_error()));
        }
    }
    if !md.file_type().is_symlink() {
        std::fs::set_permissions(dest, std::os::unix::fs::PermissionsExt::from_mode(md.mode() & 0o7777))?;
    }
    let times = [
        libc::timespec { tv_sec: md.atime(), tv_nsec: md.atime_nsec() },
        libc::timespec { tv_sec: md.mtime(), tv_nsec: md.mtime_nsec() },
    ];
    if unsafe { libc::utimensat(libc::AT_FDCWD, cd.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(anyhow!("set times on {}: {}", dest.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

fn remove_entry(path: &Path, md: &std::fs::Metadata) -> Result<()> {
    if md.is_dir() { vm::remove_tree(path)?; } else { std::fs::remove_file(path)?; }
    Ok(())
}

fn set_opaque(dir: &Path) -> Result<()> {
    let p = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())?;
    let name = std::ffi::CString::new("user.overlay.opaque")?;
    if unsafe { libc::lsetxattr(p.as_ptr(), name.as_ptr(), b"y".as_ptr().cast(), 1, 0) } != 0 {
        return Err(anyhow!("mark {} opaque: {}", dir.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}
