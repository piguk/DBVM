//! Which paths of a directory tree changed: inotify on every directory of the tree.
//!
//! Sessions watch their materialized tree, rollouts their upper directory, and after each command look
//! only at these paths instead of walking the whole tree. Writes from the sandbox reach the tree through the
//! host kernel (bwrap, overlayfs copying up into the upper directory) or through gVisor's gofer, a host
//! process, so inotify sees them all.
//!
//! One inotify instance serves every tree in the process: the kernel allows a user few instances (128 or
//! 256 by default), far fewer than the rollouts a node runs. A thread reads its events as they come, so the
//! shared queue does not overflow, and hands each to the tree whose directory it names.
//!
//! When the watcher cannot be sure of a change set it says so, and the caller walks the whole tree:
//! - the kernel's event queue overflowed;
//! - a directory was moved within the tree, so the paths of everything under it changed;
//! - a watch could not be added (the per-user watch limit): such a tree is walked from then on.
//!
//! The cost of being wrong would be a change never reported (or never synced), so every doubt falls back
//! to the full walk. Writes through a shared memory mapping raise no event at all; callers that must see
//! them walk the tree now and then.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::vm;

const MASK: u32 = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MODIFY | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB
    | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR | libc::IN_DONT_FOLLOW;

/// The change set since the last `take`.
pub struct Changes {
    /// VM paths that may have changed.
    pub paths: BTreeSet<String>,
    /// The set may be incomplete: walk the whole tree.
    pub full: bool,
}

/// One watched tree; dropping it removes its watches.
pub struct TreeWatch { id: u64 }

struct Hub { fd: OwnedFd, state: Mutex<State> }

#[derive(Default)]
struct State {
    next_id: u64,
    /// watch descriptor -> (tree, VM path of the watched directory)
    wds: HashMap<i32, (u64, String)>,
    trees: HashMap<u64, Tree>,
}

struct Tree {
    root: PathBuf,
    wds: HashSet<i32>,
    paths: BTreeSet<String>,
    full: bool,
    /// A watch could not be added: the tree is no longer watched and every change set is a full one.
    unwatched: bool,
}

/// The process's inotify instance and its reader thread; None if inotify is unavailable, in which case
/// every change set is a full one.
fn hub() -> Option<&'static Hub> {
    static HUB: OnceLock<Option<Hub>> = OnceLock::new();
    HUB.get_or_init(|| {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 { return None; }
        // SAFETY: fd was just returned by inotify_init1 and is owned by nothing else
        Some(Hub { fd: unsafe { OwnedFd::from_raw_fd(fd) }, state: Mutex::new(State::default()) })
    }).as_ref().inspect(|h| {
        static READER: OnceLock<()> = OnceLock::new();
        READER.get_or_init(|| {
            let h: &'static Hub = h;
            std::thread::Builder::new().name("inotify".into()).spawn(move || loop {
                let mut p = libc::pollfd { fd: h.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                if unsafe { libc::poll(&mut p, 1, -1) } > 0 { h.state.lock().unwrap().drain(h.fd.as_raw_fd()); }
            }).expect("start the inotify reader");
        });
    })
}

impl TreeWatch {
    /// Watch `root` and every directory below it.
    pub fn new(root: &Path) -> TreeWatch {
        let Some(h) = hub() else { return TreeWatch { id: 0 } };
        let mut st = h.state.lock().unwrap();
        st.next_id += 1;
        let id = st.next_id;
        st.trees.insert(id, Tree { root: root.to_path_buf(), wds: HashSet::new(), paths: BTreeSet::new(), full: false, unwatched: false });
        st.watch_tree(h.fd.as_raw_fd(), id, "/", false);
        TreeWatch { id }
    }

    /// The changes since the last call, and reset. After a full answer the watches are rebuilt, so the next
    /// change set starts from a correct view of the tree.
    pub fn take(&mut self) -> Changes {
        let full = Changes { paths: BTreeSet::new(), full: true };
        let Some(h) = hub() else { return full };
        let fd = h.fd.as_raw_fd();
        let mut st = h.state.lock().unwrap();
        // events of everything that finished before this call are queued; read them now
        st.drain(fd);
        let Some(t) = st.trees.get_mut(&self.id) else { return full };
        if t.unwatched { return full; }
        let changes = Changes { paths: std::mem::take(&mut t.paths), full: t.full };
        if changes.full {
            t.full = false;
            st.unwatch(fd, self.id);
            st.watch_tree(fd, self.id, "/", false);
        }
        changes
    }

    /// Forget the changes so far: the tree was just rewritten from the database (a rollback).
    pub fn discard(&mut self) { let _ = self.take(); }
}

impl Drop for TreeWatch {
    fn drop(&mut self) {
        let Some(h) = hub() else { return };
        let mut st = h.state.lock().unwrap();
        st.unwatch(h.fd.as_raw_fd(), self.id);
        st.trees.remove(&self.id);
    }
}

impl State {
    fn unwatch(&mut self, fd: i32, id: u64) {
        let Some(t) = self.trees.get_mut(&id) else { return };
        for wd in t.wds.drain() {
            unsafe { libc::inotify_rm_watch(fd, wd); }
            self.wds.remove(&wd);
        }
    }

    /// Watch `vm_dir` of tree `id` and every directory below it. With `mark`, every entry found is also
    /// recorded as changed: a directory that appeared may have been filled before its watch existed.
    fn watch_tree(&mut self, fd: i32, id: u64, vm_dir: &str, mark: bool) {
        let Some(t) = self.trees.get_mut(&id) else { return };
        if t.unwatched { return; }
        let root = t.root.clone();
        let top = root.join(vm_dir.trim_start_matches('/'));
        let mut failed = false;
        for e in walkdir::WalkDir::new(&top).follow_links(false) {
            let e = match e {
                Ok(e) => e,
                // gone before it could be watched: its removal is an event of the parent's watch
                Err(err) if err.io_error().map(|i| i.kind()) == Some(std::io::ErrorKind::NotFound) => continue,
                Err(_) => { t.full = true; continue }
            };
            let rel = e.path().strip_prefix(&root).unwrap_or(e.path()).to_string_lossy().to_string();
            let vm_p = vm::normalize_vm_path(&format!("/{}", rel));
            if vm_p != "/" && vm::is_reserved_vm_path(&vm_p) { continue; }
            if mark { t.paths.insert(vm_p.clone()); }
            if !e.file_type().is_dir() { continue; }
            let Ok(c) = std::ffi::CString::new(e.path().as_os_str().as_encoded_bytes()) else { t.full = true; continue };
            let wd = unsafe { libc::inotify_add_watch(fd, c.as_ptr(), MASK) };
            if wd < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound { continue; }
                failed = true;
                break;
            }
            t.wds.insert(wd);
            self.wds.insert(wd, (id, vm_p));
        }
        if failed {
            // out of watches: rebuilding them after every full answer would fail again, at a walk's cost
            self.unwatch(fd, id);
            if let Some(t) = self.trees.get_mut(&id) { t.unwatched = true; t.paths.clear(); }
        }
    }

    /// Read every queued event into the change sets.
    fn drain(&mut self, fd: i32) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 { break; }
            let mut off = 0usize;
            while off + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                // SAFETY: the kernel writes whole events; the header is followed by `len` name bytes
                let ev = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const libc::inotify_event) };
                let name_start = off + std::mem::size_of::<libc::inotify_event>();
                let name_bytes = &buf[name_start..name_start + ev.len as usize];
                let name = String::from_utf8_lossy(name_bytes.split(|b| *b == 0).next().unwrap_or(&[])).to_string();
                off = name_start + ev.len as usize;
                self.event(fd, ev.wd, ev.mask, &name);
            }
        }
    }

    fn event(&mut self, fd: i32, wd: i32, mask: u32, name: &str) {
        if mask & libc::IN_Q_OVERFLOW != 0 {
            for t in self.trees.values_mut() { t.full = true; }
            return;
        }
        if mask & libc::IN_IGNORED != 0 {
            if let Some((id, _)) = self.wds.remove(&wd) { if let Some(t) = self.trees.get_mut(&id) { t.wds.remove(&wd); } }
            return;
        }
        let Some((id, dir)) = self.wds.get(&wd).cloned() else { return };
        let Some(t) = self.trees.get_mut(&id) else { return };
        if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
            t.paths.insert(dir.clone());
            if mask & libc::IN_MOVE_SELF != 0 { t.full = true; }
            return;
        }
        let path = if name.is_empty() { dir } else { vm::normalize_vm_path(&format!("{}/{}", dir, name)) };
        if vm::is_reserved_vm_path(&path) { return; }
        t.paths.insert(path.clone());
        if mask & libc::IN_ISDIR != 0 {
            // a directory moved away within the tree takes its subtree with it, under watches that still
            // carry the old path; one moved in from outside the tree (overlayfs copies a directory up
            // through its work directory) is new, and watching it marks its contents
            if mask & libc::IN_MOVED_FROM != 0 { t.full = true; }
            if mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 { self.watch_tree(fd, id, &path, true); }
        }
    }
}
