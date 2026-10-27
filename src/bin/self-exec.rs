use rusqlite::{Connection, OpenFlags};
use rusqlite::types::ValueRef;
use std::ffi::{CString, CStr};
use std::path::{Path, PathBuf};

const MFD_CLOEXEC: u32 = 0x0001;
extern "C" { static mut environ: *const *const libc::c_char; }

fn dump_to_fd(db: &Connection, fd: i32) -> anyhow::Result<()> {
    {
        let mut st = db.prepare_cached("SELECT content FROM self_blob LIMIT 1")?;
        let mut rows = st.query([])?;
        if let Some(r) = rows.next()? {
            let vr = r.get_ref(0)?;
            if let ValueRef::Blob(b) = vr {
                if !b.is_empty() {
                    if b.len() > 200_000_000 { anyhow::bail!("blob too large"); }
                    if unsafe { libc::ftruncate(fd, b.len() as libc::off_t) } != 0 { anyhow::bail!("ftruncate"); }
                    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), b.len(), libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
                    if ptr != libc::MAP_FAILED {
                        let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, b.len()) };
                        slice.copy_from_slice(b);
                        unsafe { libc::munmap(ptr, b.len()); }
                        return Ok(());
                    }
                    let mut off = 0usize;
                    while off < b.len() {
                        let n = unsafe { libc::pwrite(fd, b[off..].as_ptr() as *const libc::c_void, b.len()-off, off as libc::off_t) };
                        if n <= 0 { anyhow::bail!("pwrite blob"); }
                        off += n as usize;
                    }
                    return Ok(());
                }
            }
        }
    }
    let max_end: i64 = db.query_row("SELECT max(offset+filesz) FROM segments WHERE content IS NOT NULL", [], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0);
    if max_end == 0 { anyhow::bail!("no segments"); }
    if max_end > 200_000_000 { anyhow::bail!("segments too large"); }
    if unsafe { libc::ftruncate(fd, max_end as libc::off_t) } != 0 { anyhow::bail!("ftruncate"); }
    let size = max_end as usize;
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    if ptr == libc::MAP_FAILED {
        let mut st = db.prepare_cached("SELECT offset, content FROM segments WHERE content IS NOT NULL ORDER BY offset")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let off: i64 = r.get(0)?;
            let vr = r.get_ref(1)?;
            if let ValueRef::Blob(b) = vr { if !b.is_empty() {
                unsafe { libc::pwrite(fd, b.as_ptr() as *const libc::c_void, b.len(), off as libc::off_t); }
            }}
        }
        return Ok(());
    }
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size) };
    let mut st = db.prepare_cached("SELECT offset, content FROM segments WHERE content IS NOT NULL ORDER BY offset")?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let off: i64 = r.get(0)?;
        let vr = r.get_ref(1)?;
        if let ValueRef::Blob(b) = vr {
            if b.is_empty() { continue; }
            let off_us = off as usize;
            debug_assert!(off_us + b.len() <= slice.len());
            if off_us + b.len() > slice.len() { unsafe { libc::munmap(ptr, size); } anyhow::bail!("segment out of range"); }
            slice[off_us..off_us+b.len()].copy_from_slice(b);
        }
    }
    unsafe { libc::munmap(ptr, size); }
    Ok(())
}
/// Materialize the bundle's private libraries for one run.
///
/// Library bytes live in a shared, content-addressed cache (`cache_dir()/bundle/<sha256>`), so
/// a repeat launch writes nothing. The per-run directory holds only links to those cache files
/// plus soname aliases, and is the only thing removed afterwards.
fn extract_bundle(db: &Connection) -> Option<(PathBuf, Vec<selfdb::bundle_cache::Hold>)> {
    use sha2::{Digest, Sha256};
    let cache = selfdb::vm::cache_dir().join("bundle");
    let _ = std::fs::create_dir_all(&cache);
    // Bundles from elf2self >= this change carry a sha256 per object: list metadata first, so a
    // cache hit reads no library bytes at all. Older bundles lack the column and read content.
    let has_hash: bool = db.query_row("SELECT count(*) FROM pragma_table_info('bundle_objects') WHERE name='hash'", [], |r| r.get::<_,i64>(0)).unwrap_or(0) > 0;
    let sql = if has_hash {
        "SELECT soname, path, hash FROM bundle_objects WHERE kind='lib' AND path NOT LIKE '/usr/lib%' AND path NOT LIKE '/lib/%'"
    } else {
        "SELECT soname, path, NULL FROM bundle_objects WHERE kind='lib' AND path NOT LIKE '/usr/lib%' AND path NOT LIKE '/lib/%'"
    };
    let list: Vec<(Option<String>, String, Option<String>)> = {
        let mut st = match db.prepare(sql) { Ok(s)=>s, Err(_)=>return None };
        let rows = st.query_map([], |r| Ok((r.get::<_,Option<String>>(0)?, r.get::<_,String>(1)?, r.get::<_,Option<String>>(2)?)));
        match rows { Ok(it) => it.filter_map(|x| x.ok()).collect(), Err(_) => return None }
    };
    if list.is_empty(){ return None; }
    let mut dir_created: Option<PathBuf> = None;
    let mut written = 0usize;
    let mut holds: Vec<selfdb::bundle_cache::Hold> = Vec::new();
    for (sn_opt, path, hash_opt) in list {
        let use_name: String = sn_opt.as_ref().filter(|s| !s.is_empty()).cloned()
            .unwrap_or_else(|| Path::new(&path).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or("lib.so".to_string()));
        if use_name.is_empty(){ continue; }
        // cache entry name: stored hash, or (old bundles) the hash of the content read now
        let key: String = match hash_opt {
            Some(h) if !h.is_empty() => h,
            _ => {
                let blob: Vec<u8> = db.query_row("SELECT content FROM bundle_objects WHERE path=?1", [&path], |r| r.get(0)).unwrap_or_default();
                Sha256::digest(&blob).iter().map(|b| format!("{:02x}", b)).collect()
            }
        };
        let entry = cache.join(&key);
        // A present entry is trusted only if its size matches the stored size (cheap, no read of
        // the bytes). Mismatch means a truncated or foreign file: rebuild it.
        let want_size: Option<i64> = db.query_row("SELECT size FROM bundle_objects WHERE path=?1", [&path], |r| r.get(0)).ok();
        let have_ok = match (std::fs::metadata(&entry), want_size) {
            (Ok(m), Some(sz)) => m.len() as i64 == sz,
            (Ok(_), None) => true,   // old bundles: no stored size to compare; content hash names the file
            (Err(_), _) => false,
        };
        if !have_ok {
            // only now is the content needed: fetch it, write atomically (tmp + rename)
            let blob: Vec<u8> = match db.query_row("SELECT content FROM bundle_objects WHERE path=?1", [&path], |r| r.get(0)) { Ok(b)=>b, Err(_)=>continue };
            let tmp = cache.join(format!(".{}.{}", key, std::process::id()));
            let ok = std::fs::write(&tmp, &blob).and_then(|_| std::fs::rename(&tmp, &entry)).is_ok();
            let _ = std::fs::remove_file(&tmp);
            if !ok { continue; }
        }
        let dir = match dir_created.clone() {
            Some(d) => d,
            None => {
                let d = selfdb::vm::make_tmp_dir("self-bundle-").unwrap_or_else(|e| { eprintln!("self-exec: {e}"); std::process::exit(126) });
                dir_created = Some(d.clone());
                d
            }
        };
        let out = dir.join(&use_name);
        if std::fs::symlink_metadata(&out).is_ok() { continue; }
        if std::os::unix::fs::symlink(&entry, &out).is_ok() {
            written += 1;
            // keep the entry alive (not evictable) until the guest exits
            if let Ok(h) = selfdb::bundle_cache::hold(&entry) { holds.push(h); }
        }
        let base = Path::new(&path).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        if base != use_name && !base.is_empty() {
            let alt = dir.join(&base);
            if std::fs::symlink_metadata(&alt).is_err() { let _ = std::os::unix::fs::symlink(&use_name, &alt); }
        }
    }
    let d = dir_created?;
    if written == 0 { let _ = std::fs::remove_dir_all(&d); return None; }
    // bound the shared cache. The entries this launch uses are already held, so they survive.
    let _ = selfdb::bundle_cache::enforce_bundle_limit(&cache, selfdb::vm::cache_limit_from_env());
    Some((d, holds))
}
fn build_argv(guest: &[String]) -> (Vec<CString>, Vec<*const libc::c_char>) {
    let cstrs: Vec<CString>=guest.iter().map(|s| CString::new(s.as_bytes()).unwrap()).collect();
    let mut ptrs: Vec<*const libc::c_char>=cstrs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    (cstrs, ptrs)
}
fn build_env(bundle_dir: Option<&PathBuf>) -> (Vec<CString>, Vec<*const libc::c_char>) {
    if let Some(dir)=bundle_dir {
        let mut ld_val: Option<String>=None;
        unsafe {
            if !environ.is_null() {
                let mut i=0;
                while !(*environ.offset(i)).is_null() {
                    let cstr=CStr::from_ptr(*environ.offset(i));
                    if let Some(v)=cstr.to_str().ok().and_then(|s| s.strip_prefix("LD_LIBRARY_PATH=")) { ld_val=Some(v.to_string()); break; }
                    i+=1;
                }
            }
        }
        let merged=if let Some(orig)=ld_val { if orig.is_empty(){ dir.to_string_lossy().to_string() } else { format!("{}:{}", dir.to_string_lossy(), orig) } } else { dir.to_string_lossy().to_string() };
        let ld_cstring=CString::new(format!("LD_LIBRARY_PATH={}", merged)).unwrap();
        let ld_ptr=ld_cstring.as_ptr();
        let env_cstrs=vec![ld_cstring];
        let mut env_ptrs: Vec<*const libc::c_char>=Vec::new();
        env_ptrs.push(ld_ptr);
        unsafe {
            if !environ.is_null() {
                let mut i=0;
                while !(*environ.offset(i)).is_null() {
                    let p=*environ.offset(i);
                    let cstr=CStr::from_ptr(p);
                    if let Ok(s)=cstr.to_str() { if s.starts_with("LD_LIBRARY_PATH=") { i+=1; continue; } }
                    env_ptrs.push(p);
                    i+=1;
                }
            }
        }
        env_ptrs.push(std::ptr::null());
        (env_cstrs, env_ptrs)
    } else {
        let mut env_ptrs: Vec<*const libc::c_char>=Vec::new();
        unsafe {
            if !environ.is_null() {
                let mut i=0;
                while !(*environ.offset(i)).is_null() { env_ptrs.push(*environ.offset(i)); i+=1; }
            }
        }
        env_ptrs.push(std::ptr::null());
        (Vec::new(), env_ptrs)
    }
}
fn run_via_memfd(db: Connection, guest: Vec<String>) -> ! {
    let extracted = extract_bundle(&db);
    // the holds must live until the guest exits, so keep them in scope through waitpid
    let (bundle_dir, _holds) = match extracted { Some((d, h)) => (Some(d), h), None => (None, Vec::new()) };
    let mut fd: i32;
    unsafe { fd = libc::syscall(libc::SYS_memfd_create as libc::c_long, CString::new("self-elf").unwrap().as_ptr(), MFD_CLOEXEC as libc::c_long) as i32; }
    if fd<0 {
        let mut tmpl=b"/tmp/self-XXXXXX\0".to_vec();
        fd=unsafe{ libc::mkstemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
        if fd>=0 { unsafe{ let c=CStr::from_ptr(tmpl.as_ptr() as *const libc::c_char); let _=std::fs::remove_file(c.to_string_lossy().to_string()); } }
    }
    if fd<0 { eprintln!("memfd failed"); std::process::exit(2); }
    if let Err(e)=dump_to_fd(&db, fd){ eprintln!("dump: {}", e); unsafe{ libc::close(fd); } std::process::exit(2); }
    drop(db);
    unsafe{ libc::fchmod(fd, 0o755); }
    let (_argv_cstrs, argv_ptrs)=build_argv(&guest);
    let (_env_cstrs, env_ptrs)=build_env(bundle_dir.as_ref());
    let fdpath=format!("/proc/self/fd/{}", fd);
    let fdpath_c=CString::new(fdpath.clone()).unwrap();
    // Fork so the parent survives to remove the bundle dir after the guest exits.
    // Terminal signals reach the whole foreground group, so the guest gets them directly.
    let pid = unsafe { libc::fork() };
    if pid < 0 { eprintln!("fork: {}", std::io::Error::last_os_error()); std::process::exit(2); }
    if pid == 0 {
        unsafe {
            libc::execve(fdpath_c.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
            eprintln!("execve {}: {}", fdpath, std::io::Error::last_os_error());
            libc::_exit(127);
        }
    }
    unsafe { libc::close(fd); }
    let mut status: libc::c_int = 0;
    unsafe { while libc::waitpid(pid, &mut status, 0) < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {} }
    if let Some(d) = bundle_dir { let _ = std::fs::remove_dir_all(&d); }
    let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
    std::process::exit(code);
}
fn main() -> anyhow::Result<()> {
    let args: Vec<String>=std::env::args().collect();
    if args.len()<2 { eprintln!("usage: self-exec <program.self> [args...]"); std::process::exit(2); }
    let path=&args[1];
    let uri = format!("file:{}?immutable=1", path);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match Connection::open_with_flags(&uri, flags) {
        Ok(c) => c,
        Err(_) => Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?,
    };
    let _ = conn.execute_batch("PRAGMA query_only=ON; PRAGMA mmap_size=268435456; PRAGMA cache_size=-262144; PRAGMA temp_store=MEMORY;");
    run_via_memfd(conn, args[1..].to_vec());
}
