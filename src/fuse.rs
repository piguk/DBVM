
#[cfg(feature = "fuse")]
pub mod fuse_impl {
    use anyhow::Result;
    use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, KernelConfig};
    use rusqlite::params;
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use std::os::raw::c_int;
    use std::sync::{Arc, Mutex};

    use crate::vm::{vm_open, vm_add_bytes, vm_add_dir, vm_add_symlink, normalize_vm_path, now_secs, decompress_bytes_raw, decompress_bytes_cached, fx_hash_u64};

    const TTL: Duration = Duration::from_secs(1);
    const BLOCK_SIZE: u32 = 4096;

    pub struct VmFuse {
        db_path: String,
        ino_to_path: HashMap<u64, String>,
        path_to_ino: HashMap<String, u64>,
        ino_to_kind: HashMap<u64, String>,
        ino_to_attr: HashMap<u64, FileAttr>,
        staged: HashMap<u64, Vec<u8>>,
        next_ino: u64,
    }

    impl VmFuse {
        pub fn new(db_path: String) -> Result<Self> {
            let mut fs = Self {
                db_path: db_path.clone(),
                ino_to_path: HashMap::new(),
                path_to_ino: HashMap::new(),
                ino_to_kind: HashMap::new(),
                ino_to_attr: HashMap::new(),
                staged: HashMap::new(),
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

            let conn = vm_open(&self.db_path)?;
            let mut stmt = conn.prepare("SELECT path, kind, mode, size, mtime, uid, gid, link_target, hash, compressed FROM vm_fs ORDER BY length(path), path")?;
            let rows = stmt.query_map([], |r| Ok((
                r.get::<_,String>(0)?,
                r.get::<_,String>(1)?,
                r.get::<_,i64>(2)?,
                r.get::<_,i64>(3)?,
                r.get::<_,i64>(4)?,
                r.get::<_,i64>(5)?,
                r.get::<_,i64>(6)?,
                r.get::<_,Option<String>>(7)?,
                r.get::<_,Option<String>>(8)?,
                r.get::<_,Option<i64>>(9)?,
            )))?;
            for row in rows {
                let (path, kind, mode, size, mtime, uid, gid, link_target, _hash, _comp) = row?;
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

        fn fetch_file_content(&self, path: &str) -> Result<Vec<u8>> {
            let conn = vm_open(&self.db_path)?;
            // use vm_cat logic but inline to avoid borrowing issues
            // resolve symlink? if path is symlink, vm_cat will resolve; for regular file we just fetch blob
            // we should use vm_resolve then content
            match crate::vm::vm_cat(&conn, path) {
                Ok(d) => Ok(d),
                Err(_) => {
                    // maybe is dir or not found
                    Ok(Vec::new())
                }
            }
        }

        fn flush_staged(&mut self, ino: u64) -> Result<()> {
            if let Some(buf) = self.staged.get(&ino).cloned() {
                let path = self.ino_to_path.get(&ino).cloned().ok_or_else(|| anyhow::anyhow!("ino not found"))?;
                let attr = self.ino_to_attr.get(&ino).cloned().unwrap();
                let mode = attr.perm as i64 | if attr.kind == FileType::Directory { 0o40000 } else { 0o100000 };
                // use current time
                let mtime = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                let conn = vm_open(&self.db_path)?;
                // vm_add_bytes handles dedup and blob insertion
                vm_add_bytes(&conn, &path, &buf, mode as i64, mtime)?;
                // update attr size/blocks/mtime
                if let Some(a) = self.ino_to_attr.get_mut(&ino) {
                    a.size = buf.len() as u64;
                    a.blocks = if buf.is_empty() { 0 } else { (buf.len() as u64 + 511)/512 };
                    let t = UNIX_EPOCH + Duration::from_secs(mtime as u64);
                    a.mtime = t;
                    a.ctime = t;
                }
                // keep staged as is? we keep it to serve reads without re-fetch; but on next flush it's same
                // optionally clear after persist but keep for cache
                // we keep it; writes will continue to modify it
                // To reflect persisted state, we keep staged until release? Keep it.
            }
            Ok(())
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
            // flush all staged
            let inos: Vec<u64> = self.staged.keys().copied().collect();
            for ino in inos {
                let _ = self.flush_staged(ino);
            }
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
            let conn = match vm_open(&self.db_path) {
                Ok(c) => c,
                Err(_) => { reply.error(libc::EIO); return; }
            };
            let target: Result<String, _> = conn.query_row("SELECT link_target FROM vm_fs WHERE path=?1", params![path], |r| r.get(0));
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
            // staged overrides
            if let Some(buf) = self.staged.get(&ino) {
                let start = offset as usize;
                if start >= buf.len() {
                    reply.data(&[]);
                    return;
                }
                let end = std::cmp::min(buf.len(), start + size as usize);
                reply.data(&buf[start..end]);
                return;
            }
            let path = match self.ino_to_path.get(&ino) {
                Some(p) => p.clone(),
                None => { reply.error(libc::ENOENT); return; }
            };
            let kind = self.ino_to_kind.get(&ino).map(|s| s.as_str()).unwrap_or("");
            if kind == "dir" { reply.error(libc::EISDIR); return; }
            if kind == "symlink" { reply.error(libc::EINVAL); return; }
            match self.fetch_file_content(&path) {
                Ok(data) => {
                    let start = offset as usize;
                    if start >= data.len() {
                        reply.data(&[]);
                    } else {
                        let end = std::cmp::min(data.len(), start + size as usize);
                        reply.data(&data[start..end]);
                    }
                },
                Err(_) => reply.error(libc::EIO),
            }
        }
        fn write(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock: Option<u64>, reply: ReplyWrite) {
            if offset < 0 { reply.error(libc::EINVAL); return; }
            let path_exists = self.ino_to_path.contains_key(&ino);
            if !path_exists { reply.error(libc::ENOENT); return; }
            let kind = self.ino_to_kind.get(&ino).map(|s| s.as_str()).unwrap_or("file").to_string();
            if kind == "dir" { reply.error(libc::EISDIR); return; }
            // init staged if missing
            if !self.staged.contains_key(&ino) {
                // load existing
                let p = self.ino_to_path.get(&ino).unwrap().clone();
                let existing = self.fetch_file_content(&p).unwrap_or_default();
                self.staged.insert(ino, existing);
            }
            let buf = self.staged.get_mut(&ino).unwrap();
            let off = offset as usize;
            if buf.len() < off + data.len() {
                buf.resize(off + data.len(), 0);
            }
            buf[off..off+data.len()].copy_from_slice(data);
            // update attr
            if let Some(attr) = self.ino_to_attr.get_mut(&ino) {
                attr.size = buf.len() as u64;
                attr.blocks = if buf.is_empty() { 0 } else { (buf.len() as u64 + 511)/512 };
                let now = SystemTime::now();
                attr.mtime = now;
                attr.ctime = now;
            }
            reply.written(data.len() as u32);
        }
        fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock: u64, reply: ReplyEmpty) {
            let _ = self.flush_staged(ino);
            reply.ok();
        }
        fn fsync(&mut self, _req: &Request, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
            let _ = self.flush_staged(ino);
            reply.ok();
        }
        fn release(&mut self, _req: &Request, ino: u64, _fh: u64, _flags: i32, _lock: Option<u64>, _flush: bool, reply: ReplyEmpty) {
            let _ = self.flush_staged(ino);
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
            // compute sum(size) logical, blob storage, db file size
            let conn = vm_open(&self.db_path);
            let (logical, blob_bytes, files, db_blocks) = if let Ok(c) = conn {
                let logical: i64 = c.query_row("SELECT COALESCE(sum(size),0) FROM vm_fs WHERE kind='file'", [], |r| r.get(0)).unwrap_or(0);
                let blob_bytes: i64 = c.query_row("SELECT COALESCE(sum(length(content)),0) FROM vm_blobs", [], |r| r.get(0)).unwrap_or(0);
                let files: i64 = c.query_row("SELECT count(*) FROM vm_fs", [], |r| r.get(0)).unwrap_or(0);
                let page_size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(4096);
                let page_count: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
                let db_bytes = page_size * page_count;
                // use compressed size as truth for df
                let used = if blob_bytes > 0 { blob_bytes } else { logical };
                let blocks = if used > 0 { (used as u64 + BLOCK_SIZE as u64 -1)/BLOCK_SIZE as u64 } else { 0 };
                // ensure at least 1 block for root
                (logical, blob_bytes, files, blocks)
            } else {
                (0,0,0,0)
            };
            let bsize = BLOCK_SIZE;
            let blocks = db_blocks;
            let bfree = 0;
            let bavail = 0;
            let files_u = files as u64;
            let ffree = 0;
            reply.statfs(blocks, bfree, bavail, files_u, ffree, bsize, 255, bsize);
        }
        fn create(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, flags: i32, reply: ReplyCreate) {
            let new_path = match self.ensure_not_exists(parent, name) {
                Ok(p) => p,
                Err(_) => { reply.error(libc::EEXIST); return; }
            };
            let perm = mode & 0o7777;
            let now = now_secs();
            let conn = match vm_open(&self.db_path) {
                Ok(c) => c,
                Err(_) => { reply.error(libc::EIO); return; }
            };
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
            self.staged.insert(ino, Vec::new());
            reply.created(&TTL, &attr, 0, 0, flags as u32);
        }
        fn mkdir(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
            let new_path = match self.ensure_not_exists(parent, name) {
                Ok(p) => p,
                Err(_) => { reply.error(libc::EEXIST); return; }
            };
            let perm = mode & 0o7777;
            let now = now_secs();
            let conn = match vm_open(&self.db_path) {
                Ok(c) => c,
                Err(_) => { reply.error(libc::EIO); return; }
            };
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
            let conn = match vm_open(&self.db_path) { Ok(c)=>c, Err(_)=>{reply.error(libc::EIO); return;} };
            // handle blob refcnt like vm_sync does
            let hash: Option<String> = conn.query_row("SELECT hash FROM vm_fs WHERE path=?1", params![norm.clone()], |r| r.get(0)).ok().flatten();
            let _ = conn.execute("DELETE FROM vm_fs WHERE path=?1", params![norm.clone()]);
            if let Some(h) = hash { let _ = conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![h]); let _ = conn.execute("DELETE FROM vm_blobs WHERE hash=?1 AND refcnt<=0", params![h]); }
            self.path_to_ino.remove(&norm);
            self.ino_to_path.remove(&ino);
            self.ino_to_kind.remove(&ino);
            self.ino_to_attr.remove(&ino);
            self.staged.remove(&ino);
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
            let conn = match vm_open(&self.db_path) { Ok(c)=>c, Err(_)=>{reply.error(libc::EIO); return;} };
            let _ = conn.execute("DELETE FROM vm_fs WHERE path=?1", params![norm.clone()]);
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
            let conn = match vm_open(&self.db_path) { Ok(c)=>c, Err(_)=>{reply.error(libc::EIO); return;} };
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
                    let conn = vm_open(&self.db_path).unwrap();
                    let h: Option<String> = conn.query_row("SELECT hash FROM vm_fs WHERE path=?1", params![new_path.clone()], |r| r.get(0)).ok().flatten();
                    let _ = conn.execute("DELETE FROM vm_fs WHERE path=?1", params![new_path.clone()]);
                    if let Some(hash)=h { let _=conn.execute("UPDATE vm_blobs SET refcnt=refcnt-1 WHERE hash=?1", params![hash]); let _=conn.execute("DELETE FROM vm_blobs WHERE hash=?1 AND refcnt<=0", params![hash]);}
                    let old_ino = self.path_to_ino.remove(&new_path).unwrap();
                    self.ino_to_path.remove(&old_ino);
                    self.ino_to_kind.remove(&old_ino);
                    self.ino_to_attr.remove(&old_ino);
                    self.staged.remove(&old_ino);
                }
            }
            let conn = match vm_open(&self.db_path) { Ok(c)=>c, Err(_)=>{reply.error(libc::EIO); return;} };
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
                let _ = conn.execute("UPDATE vm_fs SET path=?1 WHERE path=?2", params![new_p.clone(), old_p.clone()]);
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
            let conn = vm_open(&self.db_path).ok();
            if let Some(m) = mode {
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) { attr.perm = (m & 0o7777) as u16; }
                if let Some(ref c) = conn { let _ = c.execute("UPDATE vm_fs SET mode=?1 WHERE path=?2", params![m as i64, path.clone()]); }
            }
            if let Some(u) = uid {
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) { attr.uid = u; }
                if let Some(ref c) = conn { let _ = c.execute("UPDATE vm_fs SET uid=?1 WHERE path=?2", params![u as i64, path.clone()]); }
            }
            if let Some(g) = gid {
                if let Some(attr) = self.ino_to_attr.get_mut(&ino) { attr.gid = g; }
                if let Some(ref c) = conn { let _ = c.execute("UPDATE vm_fs SET gid=?1 WHERE path=?2", params![g as i64, path.clone()]); }
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
                    let _ = c.execute("UPDATE vm_fs SET mtime=?1 WHERE path=?2", params![secs, path.clone()]);
                }
            }
            if let Some(sz) = size {
                let cur_len = {
                    let staged_len = self.staged.get(&ino).map(|b| b.len() as u64);
                    let attr_len = self.ino_to_attr.get(&ino).map(|a| a.size).unwrap_or(0);
                    staged_len.unwrap_or(attr_len)
                };
                if sz != cur_len {
                    if !self.staged.contains_key(&ino) {
                        let existing = self.fetch_file_content(&path).unwrap_or_default();
                        self.staged.insert(ino, existing);
                    }
                    let need_flush;
                    {
                        let buf = self.staged.get_mut(&ino).unwrap();
                        if (sz as usize) < buf.len() { buf.truncate(sz as usize); }
                        else if (sz as usize) > buf.len() { buf.resize(sz as usize, 0); }
                        need_flush = true;
                    }
                    if let Some(attr) = self.ino_to_attr.get_mut(&ino) {
                        attr.size = sz;
                        attr.blocks = if sz ==0 {0} else {(sz+511)/512};
                    }
                    if need_flush {
                        let _ = self.flush_staged(ino);
                        // re-acquire after flush
                    }
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
            let conn = match vm_open(&self.db_path) { Ok(c)=>c, Err(_)=>{reply.error(libc::EIO); return;} };
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
            self.staged.insert(ino, Vec::new());
            reply.entry(&TTL, &attr, 0);
        }
        fn getxattr(&mut self, _req: &Request, _ino: u64, _name: &OsStr, _size: u32, reply: fuser::ReplyXattr) {
            reply.error(libc::ENODATA);
        }
        fn listxattr(&mut self, _req: &Request, _ino: u64, _size: u32, reply: fuser::ReplyXattr) {
            reply.error(libc::ENOSYS);
        }
    }

    pub fn mount_vm(db_path: &str, mountpoint: &str, allow_other: bool) -> Result<()> {
        if !Path::new(mountpoint).exists() {
            anyhow::bail!("mountpoint does not exist: {}", mountpoint);
        }
        if !Path::new(mountpoint).is_dir() {
            anyhow::bail!("mountpoint is not a directory: {}", mountpoint);
        }
        let fs = VmFuse::new(db_path.to_string())?;
        let mut opts = vec![MountOption::FSName("vmfs".to_string()), MountOption::AutoUnmount];
        if allow_other {
            opts.push(MountOption::AllowOther);
        } else {
            opts.push(MountOption::AllowRoot);
        }
        // default_permissions lets kernel enforce perm checks
        opts.push(MountOption::CUSTOM("default_permissions".to_string()));
        // ro/rw? make writable so FUSE writes go to DB
        println!("mount {} -> {} (blocks compressed, bsize=4096)", db_path, mountpoint);
        println!("  hint: inside mount, `df -h .` should show ~3.6M, not host tmpfs 13G");
        fuser::mount2(fs, mountpoint, &opts)?;
        Ok(())
    }

    pub fn mount_vm_background(db_path: &str, mountpoint: &str) -> Result<fuser::BackgroundSession> {
        let fs = VmFuse::new(db_path.to_string())?;
        let opts = vec![MountOption::FSName("vmfs".to_string()), MountOption::AutoUnmount, MountOption::AllowOther, MountOption::CUSTOM("default_permissions".to_string())];
        let bg = fuser::spawn_mount2(fs, mountpoint, &opts)?;
        Ok(bg)
    }
}
#[cfg(not(feature="fuse"))]
pub mod fuse_impl {
    use anyhow::Result;
    pub fn mount_vm(_db: &str, _mp: &str, _allow: bool) -> Result<()> { anyhow::bail!("rebuild with --features fuse") }
}
