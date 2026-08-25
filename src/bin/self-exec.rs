use rusqlite::{Connection, OpenFlags};
use rusqlite::types::ValueRef;
use std::ffi::{CString, CStr};
use std::os::unix::ffi::OsStrExt;
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
fn extract_bundle(db: &Connection) -> Option<PathBuf> {
    let mut st = match db.prepare("SELECT soname, path, content FROM bundle_objects WHERE kind='lib' AND path NOT LIKE '/usr/lib%' AND path NOT LIKE '/lib/%'") { Ok(s)=>s, Err(_)=>return None };
    let rows: Vec<(Option<String>, String, Vec<u8>)> = {
        let mut out=Vec::new();
        let mut q=st.query([]).unwrap();
        while let Some(r)=q.next().unwrap() {
            let sn: Option<String>=r.get(0).unwrap_or(None);
            let p: String=r.get(1).unwrap_or_else(|_| "lib.so".to_string());
            let b: Option<Vec<u8>>=r.get(2).unwrap_or(None);
            if let Some(blob)=b { if !blob.is_empty() { out.push((sn,p,blob)); } }
            else { out.push((sn,p,Vec::new())); }
        }
        out
    };
    if rows.is_empty(){ return None; }
    let mut dir_created: Option<PathBuf>=None;
    let mut get_dir = || -> PathBuf {
        if let Some(ref d)=dir_created { return d.clone(); }
        let mut tmpl=b"/tmp/self-bundle-XXXXXX\0".to_vec();
        let p=unsafe{ libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
        let d=if p.is_null(){ PathBuf::from("/tmp/self-bundle-fallback") } else { let c=unsafe{ CStr::from_ptr(p)}; PathBuf::from(c.to_string_lossy().to_string()) };
        let _=std::fs::create_dir_all(&d);
        dir_created=Some(d.clone()); d
    };
    let mut written=0usize;
    for (sn_opt, path, blob) in rows {
        let use_name: String = sn_opt.as_ref().filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| Path::new(&path).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or("lib.so".to_string()));
        if use_name.is_empty(){ continue; }
        let dir=get_dir();
        let out=dir.join(&use_name);
        if out.exists(){ continue; }
        if let Ok(mut f)=std::fs::File::create(&out) {
            use std::io::Write;
            if !blob.is_empty(){ let _=f.write_all(&blob); }
            let _=f.sync_all();
            unsafe{ libc::chmod(CString::new(out.as_os_str().as_bytes()).unwrap().as_ptr(), 0o755); }
            written+=1;
            let base=Path::new(&path).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
            if base!=use_name && !base.is_empty(){ let alt=dir.join(&base); if !alt.exists(){ let _=std::os::unix::fs::symlink(&use_name, &alt); } }
        }
    }
    if dir_created.is_none(){ return None; }
    let d=dir_created.unwrap();
    if written==0{ let _=std::fs::remove_dir(&d); return None; }
    Some(d)
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
                    if let Ok(s)=cstr.to_str() { if s.starts_with("LD_LIBRARY_PATH=") { ld_val=Some(s["LD_LIBRARY_PATH=".len()..].to_string()); break; } }
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
    let bundle_dir = extract_bundle(&db);
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
    unsafe{
        libc::execve(fdpath_c.as_ptr(), argv_ptrs.as_ptr() as *const *const libc::c_char, env_ptrs.as_ptr() as *const *const libc::c_char);
        eprintln!("execve {}: {}", fdpath, std::io::Error::last_os_error());
        const AT_EMPTY_PATH: libc::c_int = 0x1000;
        libc::syscall(libc::SYS_execveat as libc::c_long, fd as libc::c_long, CString::new("").unwrap().as_ptr(), argv_ptrs.as_ptr() as libc::c_long, env_ptrs.as_ptr() as libc::c_long, AT_EMPTY_PATH as libc::c_long);
        libc::close(fd);
    }
    std::process::exit(127);
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
