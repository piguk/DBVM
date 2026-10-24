
#[cfg(feature = "fuse")]
pub mod fuse_impl {
    use anyhow::Result;
    use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, KernelConfig};
    use rusqlite::params;
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use std::os::raw::c_int;

    use crate::vm::{vm_open, vm_add_bytes, vm_add_dir, vm_add_symlink, normalize_vm_path, now_secs};

    const TTL: Duration = Duration::from_secs(1);
    const BLOCK_SIZE: u32 = 4096;

    pub struct VmFuse {
        /// One connection per mount; fuser dispatches Filesystem callbacks on a single thread.
        conn: rusqlite::Connection,
        ino_to_path: HashMap<u64, String>,
        path_to_ino: HashMap<String, u64>,
        ino_to_kind: HashMap<u64, String>,
        ino_to_attr: HashMap<u64, FileAttr>,
        next_ino: u64,
    }

    impl VmFuse {
        pub fn new(db_path: String) -> Result<Self> {
            let conn = vm_open(&db_path)?;
            let mut fs = Self {
                conn,
                ino_to_path: HashMap::new(),
                path_to_ino: HashMap::new(),
                ino_to_kind: HashMap::new(),
                ino_to_attr: HashMap::new(),
                next_ino: 2,
            };
            fs.rebuild()?;
            Ok(fs)
        }

        fn rebuild(&mut self) -> Result<()> {
            self.ino_to_path.clear();
            self.path_to_ino.clear();
            self.ino_to_kind.clear();
            self.ino_to_attr.clear();
            // root
            let root_attr = FileAttr {
                ino: 1,
                size: 4096,
                blocks: 8,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: 0,
                blksize: BLOCK_SIZE,
                flags: 0,
            };
            self.ino_to_path.insert(1, "/".to_string());
            self.path_to_ino.insert("/".to_string(), 1);
            self.ino_to_kind.insert(1, "dir".to_string());
            self.ino_to_attr.insert(1, root_attr);

            let conn = &self.conn;
            let mut stmt = conn.prepare("SELECT path, kind, mode, size, mtime, uid, gid, link_target, compressed FROM vm_fs ORDER BY length(path), path")?;
            let rows = stmt.query_map([], |r| Ok((
                r.get::<_,String>(0)?,
                r.get::<_,String>(1)?,
                r.get::<_,i64>(2)?,
                r.get::<_,i64>(3)?,
                r.get::<_,i64>(4)?,
                r.get::<_,i64>(5)?,
                r.get::<_,i64>(6)?,
                r.get::<_,Option<String>>(7)?,
                r.get::<_,Option<i64>>(8)?,
            )))?;
            for row in rows {
                let (path, kind, mode, size, mtime, uid, gid, link_target, _comp) = row?;
                let norm = normalize_vm_path(&path);
                if norm == "/" { 
                    // update root mtime if needed but keep ino 1
                    if let Some(attr) = self.ino_to_attr.get_mut(&1) {
                        let t = UNIX_EPOCH + Duration::from_secs(mtime as u64);
                        attr.mtime = t;
                        attr.ctime = t;
                        attr.atime = t;
                    }
                    continue;
                }
                if self.path_to_ino.contains_key(&norm) { continue; }
                let ino = self.next_ino;
                self.next_ino += 1;
                self.path_to_ino.insert(norm.clone(), ino);
                self.ino_to_path.insert(ino, norm.clone());
                self.ino_to_kind.insert(ino, kind.clone());
                let ft = match kind.as_str() {
                    "dir" => FileType::Directory,
                    "symlink" => FileType::Symlink,
                    _ => FileType::RegularFile,
                };
                let perm = (mode & 0o7777) as u16;
                let sz = if kind == "dir" { 4096 } else if kind == "symlink" { link_target.as_ref().map(|s| s.len() as u64).unwrap_or(0) } else { size as u64 };
                let blocks = if sz == 0 { 0 } else { (sz + 511)/512 };
                let t = UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64);
                let nlink = if ft == FileType::Directory { 2 } else { 1 };
                // directories need parent check but default nlink 2
                let attr = FileAttr {
                    ino,
                    size: sz,
                    blocks,
                    atime: t,
                    mtime: t,
                    ctime: t,
                    crtime: t,
                    kind: ft,
                    perm,
                    nlink,
                    uid: uid as u32,
                    gid: gid as u32,
                    rdev: 0,
                    blksize: BLOCK_SIZE,
                    flags: 0,
                };
                self.ino_to_attr.insert(ino, attr);
                // ensure parent dir attrs nlink maybe update but keep simple
            }
            // ensure parent dirs missing? vm_fs ensure_parent_dirs should have created them
            Ok(())
        }

        fn get_parent_ino(&self, ino: u64) -> Option<u64> {
            let path = self.ino_to_path.get(&ino)?;
            if path == "/" { return Some(1); }
            let parent = Path::new(path).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or("/".to_string());
            let parent_norm = normalize_vm_path(&if parent.is_empty() { "/".to_string() } else { parent });
            self.path_to_ino.get(&parent_norm).copied()
        }

        fn ensure_not_exists(&self, parent: u64, name: &OsStr) -> Result<String> {
            let parent_path = self.ino_to_path.get(&parent).ok_or_else(|| anyhow::anyhow!("parent not found"))?.clone();
            let n = name.to_string_lossy().to_string();
            if n.contains('/') || n.is_empty() || n == "." || n == ".." {
                anyhow::bail!("invalid name");
            }
            let new_path = if parent_path == "/" { format!("/{}", n) } else { format!("{}/{}", parent_path, n) };
            let norm = normalize_vm_path(&new_path);
            if self.path_to_ino.contains_key(&norm) {
                anyhow::bail!("exists");
            }
            Ok(norm)
        }
    }

    impl Filesystem for VmFuse {
        fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::result::Result<(), c_int> {
            // rebuild already done
            Ok(())
        }
        fn destroy(&mut self) {
            // nothing is buffered: every write already reached the database
        }
        fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let parent_path = match self.ino_to_path.get(&parent) {
                Some(p) => p.clone(),
                None => { reply.error(libc::ENOENT); return; }
            };
            let n = name.to_string_lossy().to_string();
            let child_path = if parent_path == "/" { format!("/{}", n) } else { format!("{}/{}", parent_path, n) };
            let norm = normalize_vm_path(&child_path);
            if let Some(&ino) = self.path_to_ino.get(&norm) {
                if let Some(attr) = self.ino_to_attr.get(&ino) {
                    reply.entry(&TTL, attr, 0);
                    return;
                }
            }
            reply.error(libc::ENOENT);
        }
        fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
            if let Some(attr) = self.ino_to_attr.get(&ino) {
                reply.attr(&TTL, attr);
            } else {
                reply.error(libc::ENOENT);
            }
        }
        fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
            let path = match self.ino_to_path.get(&ino) {
                Some(p) => p.clone(),
                None => { reply.error(libc::ENOENT); return; }
            };
            let conn = &self.conn;
            let target: Result<String, _> = conn.prepare_cached("SELECT link_target FROM vm_fs WHERE path=?1").and_then(|mut st| st.query_row(params![path], |r| r.get(0)));
            match target {
                Ok(t) => reply.data(t.as_bytes()),
                Err(_) => reply.error(libc::ENOENT),
            }
        }
        fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
            if self.ino_to_attr.contains_key(&ino) {
                // check kind is file?
                let kind = self.ino_to_kind.get(&ino).map(|s| s.as_str()).unwrap_or("file");
                if kind == "dir" {
                    reply.error(libc::EISDIR);
                } else {
                    reply.opened(0, 0);
                }
            } else {
                reply.error(libc::ENOENT);
            }
        }
        fn read(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
            if offset < 0 { reply.error(libc::EINVAL); return; }
            let path = match self.ino_to_path.get(&ino) {
                Some(p) => p.clone(),
                None => { reply.error(libc::ENOENT); return; }
            };
            let kind = self.ino_to_kind.get(&ino).map(|s| s.as_str()).unwrap_or("");
            if kind == "dir" { reply.error(libc::EISDIR); return; }
            if kind == "symlink" { reply.error(libc::EINVAL); return; }
            match crate::vm::read_range(&self.conn, &path, offset as u64, size as usize) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => { eprintln!("vm-fuse read {}: {}", path, e); reply.error(libc::EIO) },
            }
        }
        fn write(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock: Option<u64>, reply: ReplyWrite) {
            if offset < 0 { reply.error(libc::EINVAL); return; }
            let path = match self.ino_to_path.get(&ino) { Some(p) => p.clone(), None => { reply.error(libc::ENOENT); return; } };
            let kind = self.ino_to_kind.get(&ino).map(|s| s.as_str()).unwrap_or("file").to_string();
            if kind == "dir" { reply.error(libc::EISDIR); return; }
            // Only the 64 KiB chunks this write touches are rewritten. A failed write is reported as
            // EIO, never as success: the file is left unchanged by write_range.
            let mtime = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
            if let Err(e) = crate::vm::write_range(&self.conn, &path, offset as u64, data, mtime) {
                eprintln!("vm-fuse write {}: {}", path, e);
                reply.error(libc::EIO);
                return;
            }
            // refresh the cached attributes from the stored row
            if let Ok((size, mt)) = self.conn.query_row("SELECT size, mtime FROM vm_fs WHERE path=?1", params![path], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))) {
                if let Some(a) = self.ino_to_attr.get_mut(&ino) {
                    a.size = size as u64;
                    a.blocks = if size == 0 { 0 } else { (size as u64 + 511) / 512 };
                    let t = UNIX_EPOCH + Duration::from_secs(mt as u64);
                    a.mtime = t;
                    a.ctime = t;
                }
            }
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
            if self.ino_to_attr.get(&ino).map(|a| a.kind) == Some(FileType::Directory) {
                reply.opened(0, 0);
            } else {
                reply.error(libc::ENOTDIR);
            }
        }
        fn readdir(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
            let path = match self.ino_to_path.get(&ino) {
                Some(p) => p.clone(),
                None => { reply.error(libc::ENOENT); return; }
            };
            let kind = self.ino_to_kind.get(&ino).map(|s| s.as_str()).unwrap_or("");
            if kind != "dir" && ino != 1 {
                reply.error(libc::ENOTDIR);
                return;
            }
            let mut entries: Vec<(u64, FileType, String)> = Vec::new();
            // . and ..
            entries.push((ino, FileType::Directory, ".".to_string()));
            let parent_ino = self.get_parent_ino(ino).unwrap_or(1);
            entries.push((parent_ino, FileType::Directory, "..".to_string()));
            // children
            for (p, c_ino) in self.path_to_ino.iter() {
                if p == "/" { continue; }
                let parent_p = Path::new(p).parent().map(|pp| pp.to_string_lossy().to_string()).unwrap_or("/".to_string());
                let parent_norm = normalize_vm_path(&if parent_p.is_empty() { "/".to_string() } else { parent_p });
                if parent_norm == path {
                    let k = self.ino_to_kind.get(c_ino).map(|s| s.as_str()).unwrap_or("file");
                    let ft = match k {
                        "dir" => FileType::Directory,
                        "symlink" => FileType::Symlink,
                        _ => FileType::RegularFile,
                    };
                    let name = Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    entries.push((*c_ino, ft, name));
                }
            }
            // sort by name but keep . and .. first
            let mut tail = entries.split_off(2);
            tail.sort_by(|a,b| a.2.cmp(&b.2));
            entries.extend(tail);
            for (i, (c_ino, ft, name)) in entries.into_iter().enumerate().skip(offset as usize) {
                if reply.add(c_ino, (i+1) as i64, ft, name) { break; }
            }
            reply.ok();
        }
        fn releasedir(&mut self, _req: &Request, _ino: u64, _fh: u64, _flags: i32, reply: ReplyEmpty) {
            reply.ok();
        }
        fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
            // df reports stored blob bytes (compressed), not host tmpfs
            let (blocks, files) = match Some(&self.conn) {
                Some(c) => {
                    let blob_bytes: i64 = c.prepare_cached("SELECT COALESCE(sum(length(content)),0) FROM vm_blobs").and_then(|mut st| st.query_row([], |r| r.get(0))).unwrap_or(0);
                    let files: i64 = c.prepare_cached("SELECT count(*) FROM vm_fs").and_then(|mut st| st.query_row([], |r| r.get(0))).unwrap_or(0);
                    ((blob_bytes as u64).div_ceil(BLOCK_SIZE as u64), files as u64)
                }
                None => (0, 0),
            };
            let bsize = BLOCK_SIZE;
            reply.statfs(blocks, 0, 0, files, 0, bsize, 255, bsize);
        }
        fn create(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, flags: i32, reply: ReplyCreate) {
            let new_path = match self.ensure_not_exists(parent, name) {
                Ok(p) => p,
                Err(_) => { reply.error(libc::EEXIST); return; }
            };
            let perm = mode & 0o7777;
            let now = now_secs();
            let conn = &self.conn;
            if let Err(_) = vm_add_bytes(&conn, &new_path, &[], perm as i64, now) {
                reply.error(libc::EIO);
                return;
            }
            let ino = self.next_ino;
            self.next_ino += 1;
            self.path_to_ino.insert(new_path.clone(), ino);
            self.ino_to_path.insert(ino, new_path);
            self.ino_to_kind.insert(ino, "file".to_string());
            let now_sys = UNIX_EPOCH + Duration::from_secs(now as u64);
            let attr = FileAttr {
                ino,
                size: 0,
                blocks: 0,
                atime: now_sys,
                mtime: now_sys,
                ctime: now_sys,
                crtime: now_sys,
                kind: FileType::RegularFile,
                perm: perm as u16,
                nlink: 1,
                uid: _req.uid(),
                gid: _req.gid(),
                rdev: 0,
                blksize: BLOCK_SIZE,
                flags: 0,
            };
            self.ino_to_attr.insert(ino, attr);
            reply.created(&TTL, &attr, 0, 0, flags as u32);
        }
        fn mkdir(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
            let new_path = match self.ensure_not_exists(parent, name) {
                Ok(p) => p,
                Err(_) => { reply.error(libc::EEXIST); return; }
            };
            let perm = mode & 0o7777;
            let now = now_secs();
            let conn = &self.conn;
            if let Err(_) = vm_add_dir(&conn, &new_path, perm as i64, now) {
                reply.error(libc::EIO);
                return;
            }
            let ino = self.next_ino;
            self.next_ino += 1;
            self.path_to_ino.insert(new_path.clone(), ino);
            self.ino_to_path.insert(ino, new_path);
            self.ino_to_kind.insert(ino, "dir".to_string());
            let now_sys = UNIX_EPOCH + Duration::from_secs(now as u64);
            let attr = FileAttr {
                ino,
                size: 4096,
                blocks: 8,
                atime: now_sys,
                mtime: now_sys,
                ctime: now_sys,
                crtime: now_sys,
                kind: FileType::Directory,
                perm: perm as u16,
                nlink: 2,
                uid: _req.uid(),
                gid: _req.gid(),
                rdev: 0,
                blksize: BLOCK_SIZE,
                flags: 0,
            };
            self.ino_to_attr.insert(ino, attr);
            reply.entry(&TTL, &attr, 0);
        }
        fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let parent_path = match self.ino_to_path.get(&parent) { Some(p)=>p.clone(), None=> {reply.error(libc::ENOENT); return;} };
            let n = name.to_string_lossy().to_string();
            let target = if parent_path == "/" { format!("/{}", n) } else { format!("{}/{}", parent_path, n) };
            let norm = normalize_vm_path(&target);
            let ino = match self.path_to_ino.get(&norm).copied() { Some(i)=> i, None=> {reply.error(libc::ENOENT); return;} };
            let kind = self.ino_to_kind.get(&ino).cloned().unwrap_or_default();
            if kind == "dir" { reply.error(libc::EISDIR); return; }
            let conn = &self.conn;
            // DELETE fires vm_fs_hash_del, which releases the blob reference
            let _ = conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1").and_then(|mut st| st.execute(params![norm.clone()]));
            self.path_to_ino.remove(&norm);
            self.ino_to_path.remove(&ino);
            self.ino_to_kind.remove(&ino);
            self.ino_to_attr.remove(&ino);
            reply.ok();
        }
        fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let parent_path = match self.ino_to_path.get(&parent) { Some(p)=>p.clone(), None=> {reply.error(libc::ENOENT); return;} };
            let n = name.to_string_lossy().to_string();
            let target = if parent_path == "/" { format!("/{}", n) } else { format!("{}/{}", parent_path, n) };
            let norm = normalize_vm_path(&target);
            let ino = match self.path_to_ino.get(&norm).copied() { Some(i)=> i, None=> {reply.error(libc::ENOENT); return;} };
            let kind = self.ino_to_kind.get(&ino).cloned().unwrap_or_default();
            if kind != "dir" { reply.error(libc::ENOTDIR); return; }
            // check empty
            for p in self.path_to_ino.keys() {
                if p != &norm {
                    let pp = Path::new(p).parent().map(|pp| pp.to_string_lossy().to_string()).unwrap_or("/".to_string());
                    let pp_norm = normalize_vm_path(&if pp.is_empty() { "/".to_string() } else { pp });
                    if pp_norm == norm { reply.error(libc::ENOTEMPTY); return; }
                }
            }
            let conn = &self.conn;
            let _ = conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1").and_then(|mut st| st.execute(params![norm.clone()]));
            self.path_to_ino.remove(&norm);
            self.ino_to_path.remove(&ino);
            self.ino_to_kind.remove(&ino);
            self.ino_to_attr.remove(&ino);
            reply.ok();
        }
        fn symlink(&mut self, _req: &Request, parent: u64, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
            let new_path = match self.ensure_not_exists(parent, link_name) {
                Ok(p) => p,
                Err(_) => { reply.error(libc::EEXIST); return; }
            };
            let tgt = target.to_string_lossy().to_string();
            let conn = &self.conn;
            if let Err(_) = vm_add_symlink(&conn, &new_path, &tgt) {
                reply.error(libc::EIO); return;
            }
            let ino = self.next_ino;
            self.next_ino += 1;
            self.path_to_ino.insert(new_path.clone(), ino);
            self.ino_to_path.insert(ino, new_path);
            self.ino_to_kind.insert(ino, "symlink".to_string());
            let now_sys = SystemTime::now();
            let attr = FileAttr {
                ino,
                size: tgt.len() as u64,
                blocks: 0,
                atime: now_sys,
                mtime: now_sys,
                ctime: now_sys,
                crtime: now_sys,
                kind: FileType::Symlink,
                perm: 0o777,
                nlink: 1,
                uid: _req.uid(),
                gid: _req.gid(),
                rdev: 0,
                blksize: BLOCK_SIZE,
                flags: 0,
            };
            self.ino_to_attr.insert(ino, attr);
            reply.entry(&TTL, &attr, 0);
        }
        fn rename(&mut self, _req: &Request, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty) {
            let old_parent_path = match self.ino_to_path.get(&parent) { Some(p)=>p.clone(), None=>{reply.error(libc::ENOENT); return;}};
            let new_parent_path = match self.ino_to_path.get(&newparent) { Some(p)=>p.clone(), None=>{reply.error(libc::ENOENT); return;}};
            let old_n = name.to_string_lossy().to_string();
            let new_n = newname.to_string_lossy().to_string();
            let old_path = normalize_vm_path(& if old_parent_path=="/" { format!("/{}", old_n) } else { format!("{}/{}", old_parent_path, old_n) });
            let new_path = normalize_vm_path(& if new_parent_path=="/" { format!("/{}", new_n) } else { format!("{}/{}", new_parent_path, new_n) });
            if !self.path_to_ino.contains_key(&old_path) { reply.error(libc::ENOENT); return; }
            if self.path_to_ino.contains_key(&new_path) {
                // if target exists, remove it first (like overwrite)
                if let Some(ino) = self.path_to_ino.get(&new_path).copied() {
                    // simple unlink if file
                    let kind = self.ino_to_kind.get(&ino).cloned().unwrap_or_default();
                    if kind == "dir" { reply.error(libc::EEXIST); return; }
                    let conn = &self.conn;
                    let _ = conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1").and_then(|mut st| st.execute(params![new_path.clone()]));
                    let old_ino = self.path_to_ino.remove(&new_path).unwrap();
                    self.ino_to_path.remove(&old_ino);
                    self.ino_to_kind.remove(&old_ino);
                    self.ino_to_attr.remove(&old_ino);
                }
            }
            let conn = &self.conn;
            // update vm_fs for old_path and descendants
            let old_prefix = if old_path=="/" { "/".to_string() } else { format!("{}/", old_path) };
            let new_prefix = if new_path=="/" { "/".to_string() } else { format!("{}/", new_path) };
            // collect affected paths
            let mut affected: Vec<String> = Vec::new();
            for p in self.path_to_ino.keys() {
                if p == &old_path || p.starts_with(&old_prefix) {
                    affected.push(p.clone());
                }
            }
            affected.sort_by(|a,b| b.len().cmp(&a.len())); // longer first to avoid collision
            for old_p in affected {
                let new_p = if old_p == old_path { new_path.clone() } else { format!("{}{}", new_prefix, &old_p[old_prefix.len()..]) };
                let _ = conn.prepare_cached("UPDATE vm_fs SET path=?1 WHERE path=?2").and_then(|mut st| st.execute(params![new_p.clone(), old_p.clone()]));
                if let Some(ino) = self.path_to_ino.remove(&old_p) {
                    self.path_to_ino.insert(new_p.clone(), ino);
                    self.ino_to_path.insert(ino, new_p);
                }
            }
            reply.ok();
        }
        fn setattr(&mut self, _req: &Request, ino: u64, mode: Option<u32>, uid: Option<u32>, gid: Option<u32>, size: Option<u64>, atime: Option<fuser::TimeOrNow>, mtime: Option<fuser::TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<u64>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<u32>, reply: ReplyAttr) {
            if !self.ino_to_attr.contains_key(&ino) { reply.error(libc::ENOENT); return; }
            let path = self.ino_to_path.get(&ino).cloned().unwrap_or_default();
            let conn = Some(&self.conn);
            if let Some(m) = mode {
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) { attr.perm = (m & 0o7777) as u16; }
                if let Some(ref c) = conn { let _ = c.prepare_cached("UPDATE vm_fs SET mode=?1 WHERE path=?2").and_then(|mut st| st.execute(params![m as i64, path.clone()])); }
            }
            if let Some(u) = uid {
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) { attr.uid = u; }
                if let Some(ref c) = conn { let _ = c.prepare_cached("UPDATE vm_fs SET uid=?1 WHERE path=?2").and_then(|mut st| st.execute(params![u as i64, path.clone()])); }
            }
            if let Some(g) = gid {
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) { attr.gid = g; }
                if let Some(ref c) = conn { let _ = c.prepare_cached("UPDATE vm_fs SET gid=?1 WHERE path=?2").and_then(|mut st| st.execute(params![g as i64, path.clone()])); }
            }
            if let Some(at) = atime {
                let t = match at { fuser::TimeOrNow::SpecificTime(t)=> t, fuser::TimeOrNow::Now => SystemTime::now() };
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) { attr.atime = t; }
            }
            if let Some(mt) = mtime {
                let t = match mt { fuser::TimeOrNow::SpecificTime(t)=> t, fuser::TimeOrNow::Now => SystemTime::now() };
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) { attr.mtime = t; attr.ctime = t; }
                if let Some(ref c) = conn {
                    let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                    let _ = c.prepare_cached("UPDATE vm_fs SET mtime=?1 WHERE path=?2").and_then(|mut st| st.execute(params![secs, path.clone()]));
                }
            }
            if let Some(sz) = size {
                // Truncate through the database: only the chunks past the new end are touched.
                let mtime = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                if let Err(e) = crate::vm::truncate_range(&self.conn, &path, sz, mtime) {
                    eprintln!("vm-fuse truncate {}: {}", path, e);
                    reply.error(libc::EIO);
                    return;
                }
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) {
                    attr.size = sz;
                    attr.blocks = if sz == 0 { 0 } else { (sz + 511) / 512 };
                }
            }
            let attr_copy = *self.ino_to_attr.get(&ino).unwrap();
            reply.attr(&TTL, &attr_copy);
        }
        fn access(&mut self, _req: &Request, ino: u64, _mask: i32, reply: ReplyEmpty) {
            if self.ino_to_attr.contains_key(&ino) { reply.ok(); } else { reply.error(libc::ENOENT); }
        }
        fn mknod(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
            let new_path = match self.ensure_not_exists(parent, name) {
                Ok(p) => p,
                Err(_) => { reply.error(libc::EEXIST); return; }
            };
            let perm = mode & 0o7777;
            let file_type = mode & libc::S_IFMT as u32;
            // only regular file supported; otherwise error
            if file_type != 0 && file_type != libc::S_IFREG as u32 {
                reply.error(libc::ENOSYS);
                return;
            }
            let now = now_secs();
            let conn = &self.conn;
            if let Err(_) = vm_add_bytes(&conn, &new_path, &[], perm as i64, now) {
                reply.error(libc::EIO); return;
            }
            let ino = self.next_ino;
            self.next_ino += 1;
            self.path_to_ino.insert(new_path.clone(), ino);
            self.ino_to_path.insert(ino, new_path);
            self.ino_to_kind.insert(ino, "file".to_string());
            let now_sys = UNIX_EPOCH + Duration::from_secs(now as u64);
            let attr = FileAttr {
                ino, size: 0, blocks: 0, atime: now_sys, mtime: now_sys, ctime: now_sys, crtime: now_sys,
                kind: FileType::RegularFile, perm: perm as u16, nlink: 1, uid: _req.uid(), gid: _req.gid(), rdev: 0, blksize: BLOCK_SIZE, flags: 0,
            };
            self.ino_to_attr.insert(ino, attr);
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
