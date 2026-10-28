//! Which paths of a session's tree changed: inotify on every directory of the tree.
//!
//! A session syncs only these paths after each command instead of walking the whole tree. Writes from
//! the sandbox reach the tree through the host kernel (bwrap) or through gVisor's gofer, a host process,
//! so inotify sees both.
//!
//! When the watcher cannot be sure of the change set it says so, and the caller syncs the whole tree:
//! - the kernel's event queue overflowed;
//! - a directory was moved, so the paths of everything under it changed;
//! - a watch could not be added (the per-user watch limit).
//!
//! The cost of being wrong would be a change never reaching the database, so every doubt falls back to the
//! full walk.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use crate::vm;

const MASK: u32 = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MODIFY | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB
    | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR | libc::IN_DONT_FOLLOW;

/// The change set since the last `take`.
pub struct Changes {
    /// VM paths that may have changed.
    pub paths: BTreeSet<String>,
    /// The set may be incomplete: sync the whole tree.
    pub full: bool,
}

pub struct TreeWatch {
    fd: OwnedFd,
    root: PathBuf,
    /// watch descriptor -> VM path of the watched directory
    dirs: HashMap<i32, String>,
    paths: BTreeSet<String>,
    full: bool,
}

impl TreeWatch {
    pub fn new(root: &Path) -> anyhow::Result<TreeWatch> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 { return Err(anyhow::anyhow!("inotify_init1: {}", std::io::Error::last_os_error())); }
        // SAFETY: fd was just returned by inotify_init1 and is owned by nothing else
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut w = TreeWatch { fd, root: root.to_path_buf(), dirs: HashMap::new(), paths: BTreeSet::new(), full: false };
        w.watch_tree("/", false);
        Ok(w)
    }

    fn host(&self, vm_path: &str) -> PathBuf { self.root.join(vm_path.trim_start_matches('/')) }

    /// Watch `vm_dir` and every directory below it. With `mark`, every entry found is also recorded as
    /// changed: a directory that appeared may have been filled before its watch existed.
    fn watch_tree(&mut self, vm_dir: &str, mark: bool) {
        let top = self.host(vm_dir);
        for e in walkdir::WalkDir::new(&top).follow_links(false) {
            let Ok(e) = e else { self.full = true; continue };
            let rel = e.path().strip_prefix(&self.root).unwrap_or(e.path()).to_string_lossy().to_string();
            let vm_p = vm::normalize_vm_path(&format!("/{}", rel));
            if vm_p != "/" && vm::is_reserved_vm_path(&vm_p) { continue; }
            if mark { self.paths.insert(vm_p.clone()); }
            if !e.file_type().is_dir() { continue; }
            let Ok(c) = std::ffi::CString::new(e.path().as_os_str().as_encoded_bytes()) else { self.full = true; continue };
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), MASK) };
            if wd < 0 { self.full = true; continue; }
            self.dirs.insert(wd, vm_p);
        }
    }

    /// Read every queued event into the change set.
    fn drain(&mut self) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 { break; }
            let mut off = 0usize;
            while off + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                // SAFETY: the kernel writes whole events; the header is followed by `len` name bytes
                let ev = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const libc::inotify_event) };
                let name_start = off + std::mem::size_of::<libc::inotify_event>();
                let name_bytes = &buf[name_start..name_start + ev.len as usize];
                let name = String::from_utf8_lossy(name_bytes.split(|b| *b == 0).next().unwrap_or(&[])).to_string();
                off = name_start + ev.len as usize;
                self.event(ev.wd, ev.mask, &name);
            }
        }
    }

    fn event(&mut self, wd: i32, mask: u32, name: &str) {
        if mask & libc::IN_Q_OVERFLOW != 0 { self.full = true; return; }
        if mask & libc::IN_IGNORED != 0 { self.dirs.remove(&wd); return; }
        let Some(dir) = self.dirs.get(&wd).cloned() else { return };
        if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
            self.paths.insert(dir.clone());
            if mask & libc::IN_MOVE_SELF != 0 { self.full = true; }
            return;
        }
        let path = if name.is_empty() { dir } else { vm::normalize_vm_path(&format!("{}/{}", dir, name)) };
        if vm::is_reserved_vm_path(&path) { return; }
        self.paths.insert(path.clone());
        if mask & libc::IN_ISDIR != 0 {
            // a moved directory takes its subtree with it, under watches that still carry the old path
            if mask & (libc::IN_MOVED_FROM | libc::IN_MOVED_TO) != 0 { self.full = true; }
            if mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 { self.watch_tree(&path, true); }
        }
    }

    /// The changes since the last call, and reset. After a full-sync answer the watches are rebuilt, so the
    /// next change set starts from a correct view of the tree.
    pub fn take(&mut self) -> Changes {
        self.drain();
        let changes = Changes { paths: std::mem::take(&mut self.paths), full: self.full };
        if self.full {
            for wd in self.dirs.keys().copied().collect::<Vec<_>>() {
                unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd); }
            }
            self.dirs.clear();
            self.full = false;
            self.drain();
            self.paths.clear();
            self.watch_tree("/", false);
        }
        changes
    }

    /// Forget the changes so far: the tree was just rewritten from the database (a rollback).
    pub fn discard(&mut self) { let _ = self.take(); self.paths.clear(); }
}
