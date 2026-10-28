
#[cfg(feature = "fuse")]
pub mod fuse_impl {
    use anyhow::Result;
    use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, KernelConfig};
    use rusqlite::params;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::ffi::OsStr;
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use std::os::raw::c_int;

    use crate::vm::{vm_open, vm_add_bytes, vm_add_dir, vm_add_symlink, normalize_vm_path, now_secs, ChunkCache};

    const TTL: Duration = Duration::from_secs(1);
    const BLOCK_SIZE: u32 = 4096;
    const ROOT: u64 = 1;
    /// Decoded chunks kept by a mount: 1024 × 64 KiB = 64 MiB at most.
    const CHUNK_CACHE_CHUNKS: usize = 1024;
    /// `open` reply flag: keep the kernel's page cache for the file across opens. Every write to the
    /// mount goes through this process and the kernel's cache, so the cache cannot hold stale data.
    const FOPEN_KEEP_CACHE: u32 = 1 << 1;

    /// One entry of the mounted tree.
    struct Node {
        /// The row in vm_fs.
        db_path: String,
        /// The path the kernel reaches for this inode, which differs from `db_path` when an ancestor is
        /// a symlink (/bin/sh is reached as usr/bin/sh). Relative link targets must be computed from
        /// this one, or they point outside the tree.
        view: String,
        attr: FileAttr,
        parent: u64,
        /// Directory entries by name; empty for files and links.
        children: BTreeMap<String, u64>,
    }

    pub struct VmFuse {
        /// One connection per mount; fuser dispatches Filesystem callbacks on a single thread.
        conn: rusqlite::Connection,
        /// Every operation finds its entry through `nodes` and a directory's `children`, so a lookup,
        /// listing or rmdir costs the size of that directory. Scanning every path instead made a
        /// `find` over 30k entries take 39 s (measured).
        nodes: HashMap<u64, Node>,
        next_ino: u64,
        /// The identity every node reports: the mounting process (see owner.rs).
        owner: crate::owner::Owner,
        chunks: ChunkCache,
    }

    fn join(dir: &str, name: &str) -> String {
        if dir == "/" { format!("/{}", name) } else { format!("{}/{}", dir, name) }
    }

    fn file_name(p: &str) -> String {
        Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
    }

    fn parent_of(p: &str) -> String {
        normalize_vm_path(&Path::new(p).parent().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| "/".into()))
    }

    fn system_time(secs: i64) -> SystemTime { UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64) }

    /// errno for a failed database write: a policy refusal is EACCES, anything else is EIO.
    fn write_errno(e: &anyhow::Error) -> c_int {
        if crate::policy::is_refusal(e) { libc::EACCES } else { libc::EIO }
    }

    fn sql_errno(e: &rusqlite::Error) -> c_int { write_errno(&anyhow::anyhow!(e.to_string())) }

    impl VmFuse {
        pub fn new(db_path: String) -> Result<Self> {
            let conn = vm_open(&db_path)?;
            let mut fs = Self {
                owner: crate::owner::Owner::of_current_process(),
                conn,
                nodes: HashMap::new(),
                next_ino: ROOT + 1,
                chunks: ChunkCache::new(CHUNK_CACHE_CHUNKS),
            };
            fs.rebuild()?;
            Ok(fs)
        }

        fn attr(&self, ino: u64, kind: FileType, perm: u16, size: u64, mtime: SystemTime) -> FileAttr {
            FileAttr {
                ino,
                size,
                blocks: if kind == FileType::Directory { 8 } else { size.div_ceil(512) },
                atime: mtime,
                mtime,
                ctime: mtime,
                crtime: mtime,
                kind,
                perm,
                nlink: if kind == FileType::Directory { 2 } else { 1 },
                uid: self.owner.uid,
                gid: self.owner.gid,
                rdev: 0,
                blksize: BLOCK_SIZE,
                flags: 0,
            }
        }

        fn rebuild(&mut self) -> Result<()> {
            self.nodes.clear();
            self.next_ino = ROOT + 1;
            let root_attr = self.attr(ROOT, FileType::Directory, 0o755, 4096, UNIX_EPOCH);
            self.nodes.insert(ROOT, Node { db_path: "/".into(), view: "/".into(), attr: root_attr, parent: ROOT, children: BTreeMap::new() });

            // A path whose parent is a symlink is not reachable at its literal path: /bin is a link to
            // /usr/bin, so a child of /bin lives at usr/bin inside the mounted tree. Registering the
            // literal path made such entries invisible (measured: /bin/sh could not be listed or read at
            // all). Every entry is therefore registered under the path the kernel would reach, and the
            // database path is kept for reads and writes.
            let link_map: HashMap<String, String> = {
                let mut ls = self.conn.prepare("SELECT path, link_target FROM vm_fs WHERE kind='symlink'")?;
                let rows = ls.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                rows.map(|r| r.map(|(p, t)| (normalize_vm_path(&p), t))).collect::<rusqlite::Result<_>>()?
            };
            let view_of = |p: &str| -> String {
                match crate::vm::resolve_parent_in_vm(p, &link_map) {
                    Some(pv) => normalize_vm_path(&format!("{}/{}", pv.trim_end_matches('/'), file_name(p))),
                    None => p.to_string(),
                }
            };
            let real_paths: HashSet<String> = {
                let mut st = self.conn.prepare("SELECT path FROM vm_fs WHERE kind <> 'symlink'")?;
                let rows = st.query_map([], |r| r.get::<_, String>(0))?;
                rows.map(|r| r.map(|p| normalize_vm_path(&p))).collect::<rusqlite::Result<_>>()?
            };
            // uid/gid are not read: Owner reports the mounting process for every node (see owner.rs)
            let rows: Vec<(String, String, i64, i64, i64, Option<String>)> = {
                let mut st = self.conn.prepare("SELECT path, kind, mode, size, mtime, link_target FROM vm_fs ORDER BY length(path), path")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let mut by_view: HashMap<String, u64> = HashMap::from([("/".to_string(), ROOT)]);
            for (path, kind, mode, size, mtime, link_target) in rows {
                let norm = normalize_vm_path(&path);
                if norm == "/" {
                    if let Some(root) = self.nodes.get_mut(&ROOT) {
                        let t = system_time(mtime);
                        root.attr.mtime = t; root.attr.ctime = t; root.attr.atime = t;
                    }
                    continue;
                }
                // an alias link pointing at a real entry has no room in the tree; the real entry serves it
                let view = view_of(&norm);
                if view != norm && real_paths.contains(&view) { continue; }
                if by_view.contains_key(&view) { continue; }
                let ft = match kind.as_str() {
                    "dir" => FileType::Directory,
                    "symlink" => FileType::Symlink,
                    _ => FileType::RegularFile,
                };
                let sz = match ft {
                    FileType::Directory => 4096,
                    FileType::Symlink => link_target.as_ref().map(|s| s.len() as u64).unwrap_or(0),
                    _ => size as u64,
                };
                let ino = self.next_ino;
                self.next_ino += 1;
                let attr = self.attr(ino, ft, (mode & 0o7777) as u16, sz, system_time(mtime));
                by_view.insert(view.clone(), ino);
                self.nodes.insert(ino, Node { db_path: norm, view, attr, parent: ROOT, children: BTreeMap::new() });
            }
            // Parents are linked after every node exists: a view path can be shorter than its parent's
            // (/bin/sh is viewed as /usr/bin/sh), so the length order above does not put parents first.
            // An entry whose parent is not in the tree is unreachable, as it was before.
            let inos: Vec<u64> = self.nodes.keys().copied().filter(|&i| i != ROOT).collect();
            for ino in inos {
                let view = self.nodes[&ino].view.clone();
                match by_view.get(&parent_of(&view)).copied() {
                    Some(parent) if self.nodes[&parent].attr.kind == FileType::Directory => {
                        self.nodes.get_mut(&ino).unwrap().parent = parent;
                        self.nodes.get_mut(&parent).unwrap().children.insert(file_name(&view), ino);
                    }
                    _ => { self.nodes.remove(&ino); }
                }
            }
            Ok(())
        }

        /// The child `name` of directory `parent`.
        fn child(&self, parent: u64, name: &OsStr) -> Option<u64> {
            self.nodes.get(&parent)?.children.get(name.to_str()?).copied()
        }

        /// Database and view paths for a new entry `name` under directory `parent`, or the errno.
        fn new_entry_paths(&self, parent: u64, name: &OsStr) -> std::result::Result<(String, String), c_int> {
            let p = self.nodes.get(&parent).ok_or(libc::ENOENT)?;
            if p.attr.kind != FileType::Directory { return Err(libc::ENOTDIR); }
            let n = name.to_str().ok_or(libc::EINVAL)?;
            if n.is_empty() || n.contains('/') || n == "." || n == ".." { return Err(libc::EINVAL); }
            if p.children.contains_key(n) { return Err(libc::EEXIST); }
            Ok((join(&p.db_path, n), join(&p.view, n)))
        }

        /// Register a created entry under `parent` and return its attributes.
        fn add_node(&mut self, parent: u64, db_path: String, view: String, kind: FileType, perm: u16, size: u64) -> FileAttr {
            let ino = self.next_ino;
            self.next_ino += 1;
            let attr = self.attr(ino, kind, perm, size, SystemTime::now());
            self.nodes.get_mut(&parent).unwrap().children.insert(file_name(&view), ino);
            self.nodes.insert(ino, Node { db_path, view, attr, parent, children: BTreeMap::new() });
            attr
        }

        /// Drop `ino` and everything under it from the tree.
        fn remove_node(&mut self, ino: u64) {
            let Some(node) = self.nodes.remove(&ino) else { return };
            if let Some(p) = self.nodes.get_mut(&node.parent) { p.children.remove(&file_name(&node.view)); }
            let mut stack: Vec<u64> = node.children.into_values().collect();
            while let Some(i) = stack.pop() {
                if let Some(n) = self.nodes.remove(&i) { stack.extend(n.children.into_values()); }
            }
        }

        /// Set a regular file's size through the database and in the cached attributes.
        fn truncate_node(&mut self, ino: u64, size: u64) -> std::result::Result<(), c_int> {
            let path = self.nodes.get(&ino).ok_or(libc::ENOENT)?.db_path.clone();
            if let Err(e) = crate::vm::truncate_range_deferred(&self.conn, &path, size, now_secs(), Some(&self.chunks)) {
                if !crate::policy::is_refusal(&e) { eprintln!("vm-fuse truncate {}: {}", path, e); }
                return Err(write_errno(&e));
            }
            let a = &mut self.nodes.get_mut(&ino).unwrap().attr;
            a.size = size;
            a.blocks = size.div_ceil(512);
            a.mtime = SystemTime::now();
            a.ctime = a.mtime;
            Ok(())
        }

        /// `ino` and its descendants, parents before children.
        fn subtree(&self, ino: u64) -> Vec<u64> {
            let mut out = vec![ino];
            let mut i = 0;
            while i < out.len() {
                if let Some(n) = self.nodes.get(&out[i]) { out.extend(n.children.values().copied()); }
                i += 1;
            }
            out
        }
    }

    impl Filesystem for VmFuse {
        fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::result::Result<(), c_int> {
            // FUSE_ATOMIC_O_TRUNC: O_TRUNC is applied by `open` and `create` (see there) instead of a
            // separate truncate from the kernel, so an overwrite is one request.
            if let Err(e) = config.add_capabilities(1 << 3 /* FUSE_ATOMIC_O_TRUNC */) {
                eprintln!("vm-fuse: FUSE_ATOMIC_O_TRUNC not accepted: {}", e);
            }
            Ok(())
        }
        fn destroy(&mut self) {
            // Every write already reached the database. Writes defer the whole-file hash (see vm_dirty);
            // computing the pending ones now leaves the database complete after an unmount. A failure
            // keeps them in vm_dirty for the next reader of hashes.
            if let Err(e) = crate::vm::finalize_dirty(&self.conn) { eprintln!("vm-fuse: pending hashes not finalized: {}", e); }
        }
        fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
            match self.child(parent, name).and_then(|ino| self.nodes.get(&ino)) {
                Some(n) => reply.entry(&TTL, &n.attr, 0),
                None => reply.error(libc::ENOENT),
            }
        }
        fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
            match self.nodes.get(&ino) {
                Some(n) => reply.attr(&TTL, &n.attr),
                None => reply.error(libc::ENOENT),
            }
        }
        fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
            let Some(n) = self.nodes.get(&ino) else { reply.error(libc::ENOENT); return };
            let target: rusqlite::Result<String> = self.conn.prepare_cached("SELECT link_target FROM vm_fs WHERE path=?1")
                .and_then(|mut st| st.query_row(params![n.db_path], |r| r.get(0)));
            match target {
                // the stored target is unchanged; the mount shows absolute targets as paths that stay
                // inside the mount (see owner::mount_link_text)
                Ok(t) => reply.data(crate::owner::mount_link_text(&n.view, &t).as_bytes()),
                Err(_) => reply.error(libc::ENOENT),
            }
        }
        fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
            match self.nodes.get(&ino).map(|n| n.attr.kind) {
                None => reply.error(libc::ENOENT),
                Some(FileType::Directory) => reply.error(libc::EISDIR),
                Some(kind) => {
                    // With FUSE_ATOMIC_O_TRUNC (announced in init) the kernel leaves O_TRUNC to the open
                    // and sends no truncate of its own. Ignoring it left the old tail behind on every
                    // overwrite: `echo hi > f` over "hello world\n" gave "hi\nlo world\n" (measured).
                    if flags & libc::O_TRUNC != 0 && kind == FileType::RegularFile {
                        if let Err(e) = self.truncate_node(ino, 0) { reply.error(e); return; }
                    }
                    // Without KEEP_CACHE the kernel dropped the file's pages on every open, so a second
                    // read of a tree was as slow as the first (measured).
                    reply.opened(0, FOPEN_KEEP_CACHE)
                }
            }
        }
        fn read(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
            if offset < 0 { reply.error(libc::EINVAL); return; }
            let Some(n) = self.nodes.get(&ino) else { reply.error(libc::ENOENT); return };
            match n.attr.kind {
                FileType::Directory => { reply.error(libc::EISDIR); return; }
                FileType::Symlink => { reply.error(libc::EINVAL); return; }
                _ => {}
            }
            match crate::vm::read_range_cached(&self.conn, &n.db_path, offset as u64, size as usize, Some(&self.chunks)) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => { eprintln!("vm-fuse read {}: {}", n.db_path, e); reply.error(libc::EIO) },
            }
        }
        fn write(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock: Option<u64>, reply: ReplyWrite) {
            if offset < 0 { reply.error(libc::EINVAL); return; }
            let Some(n) = self.nodes.get(&ino) else { reply.error(libc::ENOENT); return };
            if n.attr.kind == FileType::Directory { reply.error(libc::EISDIR); return; }
            let path = n.db_path.clone();
            // Only the 64 KiB chunks this write touches are rewritten, and the whole-file hash is
            // deferred (see vm_dirty). A failed write is reported, never as success: write_range leaves
            // the file unchanged.
            let mtime = now_secs();
            if let Err(e) = crate::vm::write_range_deferred(&self.conn, &path, offset as u64, data, mtime, Some(&self.chunks)) {
                if !crate::policy::is_refusal(&e) { eprintln!("vm-fuse write {}: {}", path, e); }
                reply.error(write_errno(&e));
                return;
            }
            let a = &mut self.nodes.get_mut(&ino).unwrap().attr;
            a.size = a.size.max(offset as u64 + data.len() as u64);
            a.blocks = a.size.div_ceil(512);
            a.mtime = system_time(mtime);
            a.ctime = a.mtime;
            reply.written(data.len() as u32);
        }
        fn flush(&mut self, _req: &Request, _ino: u64, _fh: u64, _lock: u64, reply: ReplyEmpty) {
            reply.ok();
        }
        fn fsync(&mut self, _req: &Request, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
            reply.ok();
        }
        fn release(&mut self, _req: &Request, _ino: u64, _fh: u64, _flags: i32, _lock: Option<u64>, _flush: bool, reply: ReplyEmpty) {
            reply.ok();
        }
        fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
            if self.nodes.get(&ino).map(|n| n.attr.kind) == Some(FileType::Directory) {
                reply.opened(0, 0);
            } else {
                reply.error(libc::ENOTDIR);
            }
        }
        fn readdir(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
            let Some(n) = self.nodes.get(&ino) else { reply.error(libc::ENOENT); return };
            if n.attr.kind != FileType::Directory { reply.error(libc::ENOTDIR); return; }
            // ".", "..", then the children in name order; `offset` is the index of the next entry
            let dots = [(ino, FileType::Directory, "."), (n.parent, FileType::Directory, "..")];
            let children = n.children.iter().map(|(name, c)| (*c, self.nodes[c].attr.kind, name.as_str()));
            for (i, (c_ino, ft, name)) in dots.into_iter().chain(children).enumerate().skip(offset as usize) {
                if reply.add(c_ino, (i + 1) as i64, ft, name) { break; }
            }
            reply.ok();
        }
        fn releasedir(&mut self, _req: &Request, _ino: u64, _fh: u64, _flags: i32, reply: ReplyEmpty) {
            reply.ok();
        }
        fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
            // df reports stored blob bytes (compressed), not host tmpfs
            let c = &self.conn;
            let blob_bytes: i64 = c.prepare_cached("SELECT COALESCE(sum(length(content)),0) FROM vm_blobs").and_then(|mut st| st.query_row([], |r| r.get(0))).unwrap_or(0);
            let files: i64 = c.prepare_cached("SELECT count(*) FROM vm_fs").and_then(|mut st| st.query_row([], |r| r.get(0))).unwrap_or(0);
            reply.statfs((blob_bytes as u64).div_ceil(BLOCK_SIZE as u64), 0, 0, files as u64, 0, BLOCK_SIZE, 255, BLOCK_SIZE);
        }
        fn create(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, flags: i32, reply: ReplyCreate) {
            let (db_path, view) = match self.new_entry_paths(parent, name) {
                Ok(p) => p,
                // O_CREAT without O_EXCL on an existing file must open it, not fail. pi opens its session
                // file repeatedly; answering EEXIST made every run fail (measured). O_TRUNC applies as in
                // `open`.
                Err(libc::EEXIST) => {
                    let existing = self.child(parent, name).filter(|i| self.nodes[i].attr.kind == FileType::RegularFile);
                    match existing {
                        Some(ino) if flags & libc::O_EXCL == 0 => {
                            if flags & libc::O_TRUNC != 0 {
                                if let Err(e) = self.truncate_node(ino, 0) { reply.error(e); return; }
                            }
                            reply.created(&TTL, &self.nodes[&ino].attr, 0, 0, FOPEN_KEEP_CACHE);
                        }
                        _ => reply.error(libc::EEXIST),
                    }
                    return;
                }
                Err(e) => { reply.error(e); return; }
            };
            let perm = mode & 0o7777;
            if let Err(e) = vm_add_bytes(&self.conn, &db_path, &[], perm as i64, now_secs()) {
                eprintln!("vm-fuse create {}: {}", db_path, e);
                reply.error(write_errno(&e));
                return;
            }
            let attr = self.add_node(parent, db_path, view, FileType::RegularFile, perm as u16, 0);
            // The last argument is the reply's FOPEN_* flags, not the open(2) flags. Passing the open flags
            // set FOPEN_PASSTHROUGH whenever O_EXCL was given (both are bit 7), and the kernel failed the
            // create with EIO: every `open(..., O_CREAT|O_EXCL)` (cp, tar, node's 'wx') failed (measured).
            reply.created(&TTL, &attr, 0, 0, FOPEN_KEEP_CACHE);
        }
        fn mkdir(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
            let (db_path, view) = match self.new_entry_paths(parent, name) {
                Ok(p) => p,
                // mkdir on an existing directory is success (EEXIST only if it is not a directory): pi
                // creates its session directory on every run.
                Err(libc::EEXIST) => {
                    match self.child(parent, name).and_then(|i| self.nodes.get(&i)) {
                        Some(n) if n.attr.kind == FileType::Directory => reply.entry(&TTL, &n.attr, 0),
                        _ => reply.error(libc::EEXIST),
                    }
                    return;
                }
                Err(e) => { reply.error(e); return; }
            };
            let perm = mode & 0o7777;
            if let Err(e) = vm_add_dir(&self.conn, &db_path, perm as i64, now_secs()) {
                reply.error(write_errno(&e));
                return;
            }
            let attr = self.add_node(parent, db_path, view, FileType::Directory, perm as u16, 4096);
            reply.entry(&TTL, &attr, 0);
        }
        fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let Some(ino) = self.child(parent, name) else { reply.error(libc::ENOENT); return };
            let n = &self.nodes[&ino];
            if n.attr.kind == FileType::Directory { reply.error(libc::EISDIR); return; }
            // DELETE fires vm_fs_hash_del, which releases the blob reference. A refused delete (a policy
            // rule) must be reported and must not touch the in-memory view, or the mount would show the
            // entry as gone while the database still has it.
            if let Err(e) = self.conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1").and_then(|mut st| st.execute(params![n.db_path])) {
                reply.error(sql_errno(&e));
                return;
            }
            self.remove_node(ino);
            reply.ok();
        }
        fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let Some(ino) = self.child(parent, name) else { reply.error(libc::ENOENT); return };
            let n = &self.nodes[&ino];
            if n.attr.kind != FileType::Directory { reply.error(libc::ENOTDIR); return; }
            if !n.children.is_empty() { reply.error(libc::ENOTEMPTY); return; }
            // as in unlink: report a refused delete and leave the view untouched
            if let Err(e) = self.conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1").and_then(|mut st| st.execute(params![n.db_path])) {
                reply.error(sql_errno(&e));
                return;
            }
            self.remove_node(ino);
            reply.ok();
        }
        fn symlink(&mut self, _req: &Request, parent: u64, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
            let (db_path, view) = match self.new_entry_paths(parent, link_name) {
                Ok(p) => p,
                Err(e) => { reply.error(e); return; }
            };
            let tgt = target.to_string_lossy().to_string();
            if let Err(e) = vm_add_symlink(&self.conn, &db_path, &tgt) {
                reply.error(write_errno(&e));
                return;
            }
            let attr = self.add_node(parent, db_path, view, FileType::Symlink, 0o777, tgt.len() as u64);
            reply.entry(&TTL, &attr, 0);
        }
        fn rename(&mut self, _req: &Request, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty) {
            let Some(ino) = self.child(parent, name) else { reply.error(libc::ENOENT); return };
            let Some(np) = self.nodes.get(&newparent) else { reply.error(libc::ENOENT); return };
            if np.attr.kind != FileType::Directory { reply.error(libc::ENOTDIR); return; }
            let Some(new_n) = newname.to_str().filter(|n| !n.is_empty() && !n.contains('/') && *n != "." && *n != "..") else {
                reply.error(libc::EINVAL); return;
            };
            let replaced = np.children.get(new_n).copied();
            if replaced == Some(ino) { reply.ok(); return; }
            if let Some(r) = replaced {
                if self.nodes[&r].attr.kind == FileType::Directory { reply.error(libc::EEXIST); return; }
            }
            let moved = self.subtree(ino);
            // a directory cannot move into itself
            if moved.contains(&newparent) { reply.error(libc::EINVAL); return; }
            let new_db = join(&np.db_path, new_n);
            let new_view = join(&np.view, new_n);
            // Every moved entry lands under the new path by its position in the tree, so an entry reached
            // through a linked ancestor moves with the directory the user sees it in.
            let old_view = self.nodes[&ino].view.clone();
            let moves: Vec<(u64, String, String, String)> = moved.iter().map(|&i| {
                let n = &self.nodes[&i];
                let rel = &n.view[old_view.len()..];
                (i, n.db_path.clone(), format!("{}{}", new_db, rel), format!("{}{}", new_view, rel))
            }).collect();
            let replaced_db = replaced.map(|r| self.nodes[&r].db_path.clone());
            // All rows move in one savepoint. Only after it commits is the in-memory view updated, so a
            // refused rename leaves the mount showing the old state.
            let res = crate::vm::in_savepoint(&self.conn, "fuse_rename", || {
                // an existing destination is replaced (removed with its chunks by the cascade)
                if let Some(d) = &replaced_db { self.conn.execute("DELETE FROM vm_fs WHERE path=?1", params![d])?; }
                for (_, old_db, new_db, _) in &moves {
                    // vm_fs and vm_file_chunks both key on path, and there is no ON UPDATE CASCADE:
                    // moving one without the other leaves the content unreachable at the new name.
                    // vm_dirty follows vm_fs through its foreign key.
                    self.conn.prepare_cached("UPDATE vm_fs SET path=?1 WHERE path=?2")?.execute(params![new_db, old_db])?;
                    self.conn.prepare_cached("UPDATE vm_file_chunks SET path=?1 WHERE path=?2")?.execute(params![new_db, old_db])?;
                }
                Ok(())
            });
            if let Err(e) = res { reply.error(write_errno(&e)); return; }
            if let Some(r) = replaced { self.remove_node(r); }
            let old_parent = self.nodes[&ino].parent;
            self.nodes.get_mut(&old_parent).unwrap().children.remove(&file_name(&old_view));
            self.nodes.get_mut(&newparent).unwrap().children.insert(new_n.to_string(), ino);
            self.nodes.get_mut(&ino).unwrap().parent = newparent;
            for (i, _, db, view) in moves {
                let n = self.nodes.get_mut(&i).unwrap();
                n.db_path = db;
                n.view = view;
            }
            reply.ok();
        }
        fn setattr(&mut self, _req: &Request, ino: u64, mode: Option<u32>, uid: Option<u32>, gid: Option<u32>, size: Option<u64>, atime: Option<fuser::TimeOrNow>, mtime: Option<fuser::TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<u64>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<u32>, reply: ReplyAttr) {
            let Some(path) = self.nodes.get(&ino).map(|n| n.db_path.clone()) else { reply.error(libc::ENOENT); return };
            // Ownership is fixed to the mounting process. A request to change it is refused with EPERM,
            // not silently ignored: a caller that believes its chown worked would be wrong. A request
            // that names the current owner is a no-op and succeeds.
            if self.owner.is_owner_change(uid, gid) {
                reply.error(libc::EPERM);
                return;
            }
            if let Some(m) = mode {
                if let Err(e) = self.conn.prepare_cached("UPDATE vm_fs SET mode=?1 WHERE path=?2").and_then(|mut st| st.execute(params![m as i64, path])) {
                    reply.error(sql_errno(&e));
                    return;
                }
                self.nodes.get_mut(&ino).unwrap().attr.perm = (m & 0o7777) as u16;
            }
            if let Some(at) = atime {
                let t = match at { fuser::TimeOrNow::SpecificTime(t) => t, fuser::TimeOrNow::Now => SystemTime::now() };
                self.nodes.get_mut(&ino).unwrap().attr.atime = t;
            }
            if let Some(mt) = mtime {
                let t = match mt { fuser::TimeOrNow::SpecificTime(t) => t, fuser::TimeOrNow::Now => SystemTime::now() };
                let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                if let Err(e) = self.conn.prepare_cached("UPDATE vm_fs SET mtime=?1 WHERE path=?2").and_then(|mut st| st.execute(params![secs, path])) {
                    reply.error(sql_errno(&e));
                    return;
                }
                let a = &mut self.nodes.get_mut(&ino).unwrap().attr;
                a.mtime = t;
                a.ctime = t;
            }
            if let Some(sz) = size {
                // Truncate through the database: only the chunks past the new end are touched.
                if let Err(e) = self.truncate_node(ino, sz) { reply.error(e); return; }
            }
            reply.attr(&TTL, &self.nodes[&ino].attr);
        }
        fn access(&mut self, _req: &Request, ino: u64, _mask: i32, reply: ReplyEmpty) {
            if self.nodes.contains_key(&ino) { reply.ok(); } else { reply.error(libc::ENOENT); }
        }
        fn mknod(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
            // only regular files are supported
            let file_type = mode & libc::S_IFMT;
            if file_type != 0 && file_type != libc::S_IFREG { reply.error(libc::ENOSYS); return; }
            let (db_path, view) = match self.new_entry_paths(parent, name) {
                Ok(p) => p,
                Err(e) => { reply.error(e); return; }
            };
            let perm = mode & 0o7777;
            if let Err(e) = vm_add_bytes(&self.conn, &db_path, &[], perm as i64, now_secs()) {
                eprintln!("vm-fuse mknod {}: {}", db_path, e);
                reply.error(write_errno(&e));
                return;
            }
            let attr = self.add_node(parent, db_path, view, FileType::RegularFile, perm as u16, 0);
            reply.entry(&TTL, &attr, 0);
        }
        fn getxattr(&mut self, _req: &Request, _ino: u64, _name: &OsStr, _size: u32, reply: fuser::ReplyXattr) {
            reply.error(libc::ENODATA);
        }
        fn listxattr(&mut self, _req: &Request, _ino: u64, _size: u32, reply: fuser::ReplyXattr) {
            reply.error(libc::ENOSYS);
        }
    }

    fn mount_opts(allow_other: bool) -> Vec<MountOption> {
        // fuser 0.14 encodes AllowRoot as `allow_other` too, and AutoUnmount then forces it.
        // Default = no allow_* option (owner-only, no user_allow_other needed).
        // Explicit --allow-other requires `user_allow_other` in /etc/fuse.conf.
        let mut opts = vec![MountOption::FSName("vmfs".to_string()), MountOption::CUSTOM("default_permissions".to_string())];
        if allow_other { opts.push(MountOption::AllowOther); }
        opts
    }

    pub fn mount_vm(db_path: &str, mountpoint: &str, allow_other: bool) -> Result<()> {
        if !Path::new(mountpoint).is_dir() {
            anyhow::bail!("mountpoint is not a directory: {}", mountpoint);
        }
        let fs = VmFuse::new(db_path.to_string())?;
        println!("mount {} -> {} (blocks compressed, bsize=4096)", db_path, mountpoint);
        fuser::mount2(fs, mountpoint, &mount_opts(allow_other))?;
        Ok(())
    }

    pub fn mount_vm_background(db_path: &str, mountpoint: &str) -> Result<fuser::BackgroundSession> {
        let fs = VmFuse::new(db_path.to_string())?;
        Ok(fuser::spawn_mount2(fs, mountpoint, &mount_opts(false))?)
    }
}
