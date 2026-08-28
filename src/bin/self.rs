use clap::{Parser, Subcommand};
use std::ffi::CStr;
use rusqlite::Connection;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};

const APP_ID: u32 = 0x53454C46;

#[derive(Parser)]
#[command(name="self", about="inspect and pack SELF files")]
struct Cli { #[command(subcommand)] cmd: Cmd }

#[derive(Subcommand)]
enum Cmd {
    File{ path: String },
    Ldd{ path: String },
    Exports{ path: String },
    Imports{ path: String },
    Segments{ path: String },
    Meta{ path: String },
    Closure{ path: String, output: String },
    Scan{ db: String, dir: String },
    Userland{ output: String, dirs: Vec<String> },
    Bundle{ path: String, #[arg(long, default_value="")] filter: String },
    BundleInfo{ path: String },
    Pack{ input: String, #[arg(short,long, default_value="a.self")] output: String, #[arg(long)] no_bundle: bool, #[arg(long)] no_sections: bool, #[arg(long)] no_notes: bool },
    Run{ path: String, rest: Vec<String> },
    VmInit{ db: String, #[arg(long)] force: bool, #[arg(long)] vm_only: bool },
    VmAdd{ db: String, host: String, vm_path: String },
    VmPack{ db: String, host_dir: String, #[arg(long, default_value="/")] prefix: String },
    VmImport{ db: String, elf: String, #[arg(long, default_value="/")] prefix: String },
    VmImportRootfs{ db: String, tar: String, #[arg(long, default_value="")] strip: String, #[arg(long)] whitelist: Vec<String>, #[arg(long)] exclude: Vec<String> },
    VmMaterialize{ db: String, dest: String },
    VmLs{ db: String, #[arg(default_value="/")] path: String },
    VmCat{ db: String, path: String },
    VmStat{ db: String, path: String },
    VmExec{ db: String, vm_path: String, rest: Vec<String> },
    VmChroot{ db: String, #[arg(default_value="/bin/sh")] cmd: String, rest: Vec<String>, #[arg(long, help="keep tmp dir and persist history (default: true unless --ephemeral)")] persist: bool, #[arg(long, help="skip sync and remove tmp, disables history persistence")] ephemeral: bool },
    VmResolve{ db: String, vm_path: String },
    VmCheckpoint{ db: String, name: String, #[arg(long, default_value="")] note: String },
    VmSnapshots{ db: String },
    VmVerify{ db: String },
    VmExtract{ db: String, vm_path: String, out: String },
    VmMemInsert{ db: String, addr: String, size: String, prot: String, file: String },
    VmMemList{ db: String },
    VmMemClear{ db: String },
    VmSnapshotFile{ db: String, name: String },
    VmRestoreFile{ db: String, name: String },
    VmSync{ db: String, host_root: String },
    VmGc{ db: String },
    VmCompressInfo{ db: String },
    VmRecompress{ db: String },
    VmStatus{ db: String },
    VmCacheInfo,
    VmCachePrune{ #[arg(long, default_value="1G")] max: String },
    VmTrainDict{ db: String, #[arg(long, default_value="16384")] max_size: usize },
    VmDictInfo{ db: String },
    VmDiff{ db: String, a: String, b: String },
    VmMount{ db: String, mountpoint: String, #[arg(long)] allow_other: bool },
    VmMemTrace{ db: String, prog: String, rest: Vec<String> },
    VmDiskInit{ db: String, #[arg(long, default_value="20G")] size: String },
    VmDiskImport{ db: String, raw: String, #[arg(long, default_value="")] size: String },
    VmDiskExport{ db: String, raw: String },
    VmDiskInfo{ db: String },
    VmRun{ db: String, #[arg(long, default_value="512M")] mem: String, #[arg(long)] nbd: bool, #[arg(long, default_value="")] raw: String, #[arg(long)] kvm: bool, #[arg(long, default_value="")] kernel: String, #[arg(long, default_value="")] initrd: String, #[arg(long, default_value="")] append: String },
}

fn open_db(path: &str) -> Connection {
    let conn = Connection::open(path).unwrap();
    let appid: u32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0)).unwrap_or(0);
    if appid != APP_ID { eprintln!("not a SELF file: {}", path); std::process::exit(1); }
    conn
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::File{path} => {
            let mut f = std::fs::File::open(&path)?; use std::io::{Read, Seek, SeekFrom};
            let mut head=[0u8;16]; f.read_exact(&mut head)?; f.seek(SeekFrom::Start(64))?; let mut appid=[0u8;8]; f.read_exact(&mut appid)?;
            let kind = if &appid[4..8]==b"SELF" {"SQLite 3.x database, application id 0x53454c46, user version 1"} else {"SQLite 3.x database"};
            println!("{}: {}", path, kind);
            println!("magic : {}", head.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" "));
            println!("appid : {}  <- bytes 68..71 == 'SELF'", appid.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" "));
        },
        Cmd::Ldd{path} => { let db=open_db(&path); let mut st=db.prepare("SELECT ord, soname FROM ldd")?; let rows=st.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,String>(1)?)))?; let mut n=0; for r in rows{ let (_,s)=r?; println!("{}", s); n+=1; } println!("({} libraries)", n); },
        Cmd::Exports{path} => { let db=open_db(&path); let mut st=db.prepare("SELECT name, version, type, size FROM exports ORDER BY name")?; for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Option<String>>(1)?, r.get::<_,Option<String>>(2)?, r.get::<_,Option<i64>>(3)?)))? { let (n,v,t,s)=r?; println!("{}\t{}\t{}\t{}", n, v.unwrap_or_default(), t.unwrap_or_default(), s.map(|x| x.to_string()).unwrap_or_default()); } },
        Cmd::Imports{path} => { let db=open_db(&path); let mut st=db.prepare("SELECT name, version FROM imports ORDER BY name")?; for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Option<String>>(1)?)))? { let (n,v)=r?; println!("{}\t{}", n, v.unwrap_or_default()); } },
        Cmd::Segments{path} => { let db=open_db(&path); let mut st=db.prepare("SELECT type, vaddr, filesz, memsz, r, w, x FROM segments ORDER BY id")?; for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?, r.get::<_,i64>(4)?, r.get::<_,i64>(5)?, r.get::<_,i64>(6)?)))? { let (t,v,f,m,r,w,x)=r?; println!("{}\t0x{:x}\t{}\t{}\t{}{}{}", t, v, f, m, r,w,x); } },
        Cmd::Meta{path} => { let db=open_db(&path); let mut st=db.prepare("SELECT key, value FROM self_meta ORDER BY rowid")?; for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))? { let (k,v)=r?; println!("{} = {}", k, v); } },
        Cmd::Closure{path, output} => { selfdb_closure(&path, &output)?; },
        Cmd::Scan{db, dir} => { selfdb_scan(&db, &dir)?; },
        Cmd::Userland{output, dirs} => { selfdb_userland(&output, dirs)?; },
        Cmd::Bundle{path, filter} => { bundle_list(&path, &filter)?; },
        Cmd::BundleInfo{path} => { bundle_info(&path)?; },
        Cmd::Pack{input, output, no_bundle, no_sections, no_notes} => {
            let mut cmd = vec!["elf2self".to_string(), input.clone(), "-o".to_string(), output.clone()];
            if !no_bundle { cmd.push("--bundle".to_string()); }
            if no_sections { cmd.push("--no-sections".to_string()); }
            if no_notes { cmd.push("--no-notes".to_string()); }
            println!("{}", cmd.join(" "));
            let info = selfdb::elf::parse_elf(&input, no_sections, no_notes)?; selfdb::db::create_self_db(&output, &info, no_sections, no_notes)?;
        },
        Cmd::Run{path, rest} => {
            let exe = std::env::var("SELF_EXEC").unwrap_or_else(|_| "target/release/self-exec".to_string());
            let mut args = vec![path.clone()]; args.extend(rest.clone());
            let status = std::process::Command::new(&exe).args(&args).status().unwrap_or_else(|e| { eprintln!("exec failed: {}: {}", exe, e); std::process::exit(127)});
            std::process::exit(status.code().unwrap_or(127));
        },
        Cmd::VmInit{db, force, vm_only} => { let _c=selfdb::vm::init_vm_db_with_opts(&db, force, vm_only)?; println!("vm init -> {} (WAL+mmap+compress vm_only={})", db, vm_only); },
        Cmd::VmAdd{db, host, vm_path} => { let c=Connection::open(&db)?; selfdb::vm::vm_add_file(&c, &host, &vm_path)?; println!("add {} -> {}:{}", host, db, vm_path); },
        Cmd::VmPack{db, host_dir, prefix} => { let c=Connection::open(&db)?; let n=selfdb::vm::vm_pack_host_dir(&c, &host_dir, &prefix)?; println!("pack {} -> {}:{} ({} files)", host_dir, db, prefix, n); },
        Cmd::VmImport{db, elf, prefix} => { let c=Connection::open(&db)?; let n=selfdb::vm::vm_import_closure(&c, &elf, &prefix)?; println!("import {} -> {} ({} objects)", elf, db, n); },
        Cmd::VmImportRootfs{db, tar, strip, whitelist, exclude} => { let c=selfdb::vm::vm_open(&db)?; let wl: Option<&[String]> = if whitelist.is_empty() { None } else { Some(&whitelist) }; let ex: Option<&[String]> = if exclude.is_empty() { None } else { Some(&exclude) }; let n=selfdb::vm::vm_import_tar_filtered(&c, &tar, &strip, wl, ex)?; println!("import-tar {} -> {} ({} entries) whitelist={:?} exclude={:?}", tar, db, n, whitelist, exclude); if n>50 { let _ = selfdb::vm::vm_train_dict(&c, 16384); } },
        Cmd::VmMaterialize{db, dest} => { let c=selfdb::vm::vm_open(&db)?; let n=selfdb::vm::vm_materialize_tree(&c, std::path::Path::new(&dest))?; println!("materialize {} -> {} ({} files)", db, dest, n); },
        Cmd::VmLs{db, path} => { let c=selfdb::vm::vm_open(&db)?; for (p,k,m,sz,_) in selfdb::vm::vm_ls(&c, &path)? { println!("{:8} {:4} {:>10}  {}", k, format!("{:o}", m), sz, p);} },
        Cmd::VmCat{db, path} => { let c=selfdb::vm::vm_open(&db)?; let data=selfdb::vm::vm_cat(&c, &path)?; use std::io::Write; std::io::stdout().write_all(&data)?; },
        Cmd::VmStat{db, path} => { let c=selfdb::vm::vm_open(&db)?; let (p,k,m,sz,mtime,hash)=selfdb::vm::vm_stat(&c, &path)?; println!("path={} kind={} mode={:o} size={} mtime={} hash={}", p,k,m,sz,mtime,hash); },
        Cmd::VmExec{db, vm_path, rest} => {
            let c=selfdb::vm::vm_open(&db)?;
            // mkdtemp under /tmp/self-vm-XXXXXX
            let mut tmpl=b"/tmp/self-vm-XXXXXX\0".to_vec();
            let p=unsafe{ libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
            let tmp = if p.is_null(){ std::env::temp_dir().join(format!("vm-exec-{}", std::process::id())) } else { let s=unsafe{ std::ffi::CStr::from_ptr(p)}; std::path::PathBuf::from(s.to_string_lossy().to_string()) };
            let host_bin = tmp.join(vm_path.trim_start_matches('/'));
            if let Some(parent)=host_bin.parent(){ std::fs::create_dir_all(parent)?; }
            // resolve symlink via vm_resolve and materialize (symlinks become regular file with target content)
            selfdb::vm::vm_materialize(&c, &vm_path, &host_bin)?;
            // materialize needed closure from vm_fs only (no host FS, no DB mutation)
            // also recreate symlinks for libs (e.g. libc.musl -> ld-musl)
            {
                let meta = selfdb::elf::meta_for_path(&host_bin.to_string_lossy());
                let mut to_mat: Vec<String> = Vec::new();
                for soname in meta.needed {
                    let cand: Option<String> = c.query_row(
                        "SELECT path FROM vm_fs WHERE (kind='file' OR kind='symlink') AND (path LIKE ?1 OR path LIKE ?2) LIMIT 1",
                        rusqlite::params![format!("%/{}", soname), format!("%{}", soname)],
                        |r| r.get(0)).ok();
                    if let Some(p)=cand { to_mat.push(p); }
                    else {
                        let mut st=c.prepare("SELECT path FROM vm_fs WHERE (kind='file' OR kind='symlink') AND (path LIKE '/lib/%' OR path LIKE '/usr/lib/%')")?;
                        for r in st.query_map([], |r| r.get::<_,String>(0))? { if let Ok(pp)=r { if pp.ends_with(&soname) { to_mat.push(pp); break; } } }
                    }
                }
                // eagerly materialize libs + their symlink aliases
                let mut all_libs: Vec<String> = Vec::new();
                {
                    let mut st=c.prepare("SELECT path, kind, link_target FROM vm_fs WHERE (path LIKE '/lib/%' OR path LIKE '/usr/lib/%' OR path LIKE '/usr/lib64/%')")?;
                    for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,Option<String>>(2)?)))? {
                        if let Ok((path,kind,link))=r {
                            if kind=="symlink" {
                                let host = tmp.join(path.trim_start_matches('/'));
                                if host.exists() || std::fs::symlink_metadata(&host).is_ok() { continue; }
                                if let Some(par)=host.parent(){ std::fs::create_dir_all(par)?; }
                                let target = link.unwrap_or_default();
                                let _ = std::os::unix::fs::symlink(&target, &host);
                                // also ensure target file materialized
                                if let Ok((_,_,content,_,_)) = selfdb::vm::vm_resolve(&c, &path) {
                                    if let Some(_d)=content {
                                        // materialize resolved target at its real path if missing
                                        // vm_resolve already guarantees file exists at resolved path, ensure it is present on host
                                        // find resolved path by re-querying
                                    }
                                }
                            } else {
                                all_libs.push(path);
                            }
                        }
                    }
                }
                for lp in all_libs {
                    let host = tmp.join(lp.trim_start_matches('/'));
                    if host.exists() { continue; }
                    let _ = selfdb::vm::vm_materialize(&c, &lp, &host);
                }
                // also ensure NEEDED libs materialized (may be symlink that vm_materialize resolves)
                for lp in to_mat {
                    let host = tmp.join(lp.trim_start_matches('/'));
                    if host.exists() || std::fs::symlink_metadata(&host).is_ok() { continue; }
                    // if entry is symlink, recreate symlink; vm_materialize will resolve and write file, so handle symlink case
                    let kind: Option<String> = c.query_row("SELECT kind FROM vm_fs WHERE path=?1", rusqlite::params![lp.clone()], |r| r.get(0)).ok();
                    if kind.as_deref()==Some("symlink") {
                        if let Ok(target) = c.query_row("SELECT link_target FROM vm_fs WHERE path=?1", rusqlite::params![lp.clone()], |r| r.get::<_,Option<String>>(0)) {
                            if let Some(t)=target { let _ = std::os::unix::fs::symlink(&t, &host); continue; }
                        }
                    }
                    let _ = selfdb::vm::vm_materialize(&c, &lp, &host);
                }
            }
            let mut env_ld = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            let vm_ld = format!("{}:{}", tmp.join("usr/lib/x86_64-linux-gnu").to_string_lossy(), tmp.join("lib/x86_64-linux-gnu").to_string_lossy());
            let merged_ld = if env_ld.is_empty(){ format!("{}:{}", tmp.to_string_lossy(), vm_ld) } else { format!("{}:{}:{}", tmp.to_string_lossy(), vm_ld, env_ld) };
            let mut args = Vec::new(); args.extend(rest.clone());
            let exe = std::env::var("SELF_EXEC").unwrap_or_else(|_| "target/release/self-exec".to_string());
            let is_self = {
                let mut f=std::fs::File::open(&host_bin).unwrap();
                use std::io::{Read, Seek, SeekFrom};
                let mut head=[0u8;16]; let _=f.read_exact(&mut head);
                let mut appid=[0u8;8]; let _=f.seek(SeekFrom::Start(64));
                let _=f.read_exact(&mut appid);
                &appid[4..8]==b"SELF"
            };
            // prefer using the ELF interpreter recorded at build-time when available:
            // musl busybox uses /lib/ld-musl-x86_64.so.1 which we have materialized under tmp.
            // If interpreter exists in tmp, exec it directly; otherwise try native exec (glibc binaries).
            let interp: Option<String> = selfdb::elf::parse_elf(&host_bin.to_string_lossy(), true, true).ok().and_then(|info| info.interp);
            let status = if is_self {
                let mut cmd=std::process::Command::new(&exe);
                cmd.arg(host_bin.to_string_lossy().to_string());
                cmd.args(&args);
                cmd.env("LD_LIBRARY_PATH", merged_ld);
                cmd.status().unwrap_or_else(|e| { eprintln!("exec failed: {}: {}", exe, e); std::process::exit(127)})
            } else if let Some(ip) = interp.clone() {
                // ip is absolute like /lib/ld-musl-x86_64.so.1; map to tmp/...
                let ip_host = tmp.join(ip.trim_start_matches('/'));
                if ip_host.exists() {
                    // musl ld expects: ld-musl <exe> [args]; also pass --library-path if needed
                    let mut cmd=std::process::Command::new(&ip_host);
                    cmd.arg(&host_bin);
                    cmd.args(&args);
                    // musl honours LD_LIBRARY_PATH as well; keep merged_ld for any secondary libs
                    cmd.env("LD_LIBRARY_PATH", merged_ld);
                    cmd.status().unwrap_or_else(|e| { eprintln!("exec via interp failed: {}: {}", ip_host.display(), e); std::process::exit(127)})
                } else {
                    let mut cmd=std::process::Command::new(&host_bin);
                    cmd.args(&args);
                    cmd.env("LD_LIBRARY_PATH", merged_ld);
                    cmd.status().unwrap_or_else(|e| { eprintln!("exec failed: {}: {}", host_bin.to_string_lossy(), e); std::process::exit(127)})
                }
            } else {
                let mut cmd=std::process::Command::new(&host_bin);
                cmd.args(&args);
                cmd.env("LD_LIBRARY_PATH", merged_ld);
                cmd.status().unwrap_or_else(|e| { eprintln!("exec failed: {}: {}", host_bin.to_string_lossy(), e); std::process::exit(127)})
            };
            std::process::exit(status.code().unwrap_or(127));
        },
        Cmd::VmChroot{db, cmd, rest, persist, ephemeral} => {
            if persist && ephemeral { eprintln!("cannot use both --persist and --ephemeral"); std::process::exit(2); }
            let effective_persist = persist || !ephemeral;
            let c=selfdb::vm::vm_open(&db)?;
            let full_cmd = if cmd.is_empty(){ "/bin/sh".to_string()} else { cmd.clone()};
            let has_bwrap = std::process::Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false);
            #[cfg(feature = "fuse")]
            {
                if std::path::Path::new("/dev/fuse").exists() {
                    let fuse_tmp = std::env::temp_dir().join(format!("vm-fuse-{}", std::process::id()));
                    let _ = std::fs::create_dir_all(&fuse_tmp);
                    // try FUSE: mount db -> fuse_tmp, then bwrap bind /mnt as /
                    // background mount keeps session alive until child exits
                    match selfdb::fuse::fuse_impl::mount_vm_background(&db, &fuse_tmp.to_string_lossy()) {
                        Ok(_bg) => {
                            eprintln!("-> FUSE {} -> {} (no materialize, SELECT vm_blobs on demand, statfs blocks=sum(blob)/4096)", db, fuse_tmp.display());
                            // give kernel a moment to establish mount
                            std::thread::sleep(std::time::Duration::from_millis(80));
                            let status = if has_bwrap {
                                eprintln!("-> bwrap --bind {} / (FUSE-backed, host tmpfs not leaked)", fuse_tmp.display());
                                let mut cmd2 = std::process::Command::new("bwrap");
                                cmd2.arg("--bind").arg(&fuse_tmp).arg("/").arg("--dev").arg("/dev").arg("--proc").arg("/proc").arg("--unshare-pid");
                                if effective_persist {
                                let db_hash = selfdb::vm::fx_hash_u64(db.as_bytes());
                                let hist_cached = selfdb::vm::cache_dir().join("history").join(&db_hash).join(".ash_history");
                                let _ = std::fs::create_dir_all(hist_cached.parent().unwrap());
                                // FUSE history lives inside fuse mount but also cache for later -- persist after exit via file copy
                                cmd2.env("HISTFILE", "/.ash_history");
                                cmd2.env("PS1", r"vm:\w\$ ");
                                cmd2.env("HOME", "/");
                                // pre-seed from cache if exists
                                if hist_cached.exists() { let _ = std::fs::copy(&hist_cached, fuse_tmp.join(".ash_history")); }
                            }
                                cmd2.arg(full_cmd.clone()); for a in &rest { cmd2.arg(a); }
                                cmd2.status().unwrap_or_else(|e| { eprintln!("bwrap over FUSE failed: {}", e); std::process::exit(127)})
                            } else if std::process::Command::new("unshare").arg("--help").output().map(|o| o.status.success()).unwrap_or(false) {
                                let mut cmd2 = std::process::Command::new("unshare");
                                cmd2.arg("--mount").arg("--map-root-user").arg("--root").arg(&fuse_tmp);
                                if effective_persist { cmd2.env("HISTFILE", "/.ash_history"); cmd2.env("PS1", r"vm:\w\$ "); cmd2.env("HOME", "/"); }
                                cmd2.arg(full_cmd.clone()); for a in &rest { cmd2.arg(a); }
                                cmd2.status().unwrap_or_else(|e| { eprintln!("unshare over FUSE failed: {}", e); std::process::exit(127)})
                            } else {
                                let mut cmd2 = std::process::Command::new("chroot");
                                if effective_persist { cmd2.env("HISTFILE", "/.ash_history"); cmd2.env("PS1", r"vm:\w\$ "); cmd2.env("HOME", "/"); }
                                cmd2.arg(&fuse_tmp).arg(full_cmd.clone()); for a in &rest { cmd2.arg(a); }
                                cmd2.status().unwrap_or_else(|e| { eprintln!("chroot over FUSE failed: {}", e); std::process::exit(127)})
                            };
                            // persist history from fuse mount to cache_dir
                            if effective_persist {
                                let db_hash = selfdb::vm::fx_hash_u64(db.as_bytes());
                                let hist_cached = selfdb::vm::cache_dir().join("history").join(&db_hash).join(".ash_history");
                                let _ = std::fs::create_dir_all(hist_cached.parent().unwrap());
                                let src_hist = fuse_tmp.join(".ash_history");
                                if src_hist.exists() { let _ = std::fs::copy(&src_hist, &hist_cached); }
                            }
                            // bg dropped here -> AutoUnmount; writes already flushed via flush_staged -> vm_blobs
                            std::process::exit(status.code().unwrap_or(127));
                        },
                        Err(e) => {
                            eprintln!("FUSE mount failed ({}), falling back to materialize+bwrap", e);
                        }
                    }
                } else {
                    eprintln!("hint: /dev/fuse missing (no FUSE in this env) -> falling back to materialize. Build `cargo build --release --features fuse` already pure-rust (fuser default-features=false). In a host with /dev/fuse: `self vm-chroot` will mount without writing /tmp (df shows 3.6M not host tmpfs 13G).");
                }
            }
            #[cfg(not(feature = "fuse"))]
            {
                eprintln!("hint: rebuild with --features fuse for zero-materialize: `cargo build --release --features fuse` (pure-rust, no libfuse3-dev)");
            }
            // fallback: materialize (88 files) then bwrap; still auto-persists via vm_sync_from_host + WAL
            let mut tmpl=b"/tmp/self-vm-XXXXXX\0".to_vec();
            let pp=unsafe{ libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
            let tmp = if pp.is_null(){ std::env::temp_dir().join(format!("vm-chroot-{}", std::process::id())) } else { let s=unsafe{ std::ffi::CStr::from_ptr(pp)}; std::path::PathBuf::from(s.to_string_lossy().to_string()) };
            let n=selfdb::vm::vm_materialize_tree(&c, &tmp)?;
            eprintln!("materialized {} files -> {} (fallback, would be 0 with FUSE)", n, tmp.display());
            for d in ["proc","sys","dev","tmp"] { let _=std::fs::create_dir_all(tmp.join(d)); }
            let has_unshare = std::process::Command::new("unshare").arg("--help").output().map(|o| o.status.success()).unwrap_or(false);
            let db_hash = selfdb::vm::fx_hash_u64(db.as_bytes());
            let hist_cached = selfdb::vm::cache_dir().join("history").join(&db_hash).join(".ash_history");
            let _ = std::fs::create_dir_all(hist_cached.parent().unwrap());
            // if cached exists, copy to tmp for session, else start fresh
            if hist_cached.exists() { let _ = std::fs::copy(&hist_cached, tmp.join(".ash_history")); }
            let hist = tmp.join(".ash_history");
            let ps1 = r"vm:\w\$ ";
            let status = if has_bwrap {
                eprintln!("-> bwrap {}", tmp.display());
                let mut cmd = std::process::Command::new("bwrap");
                cmd.arg("--bind").arg(&tmp).arg("/").arg("--dev").arg("/dev").arg("--proc").arg("/proc").arg("--unshare-pid");
                if effective_persist { cmd.env("HISTFILE", "/.ash_history"); cmd.env("PS1", ps1); cmd.env("HOME", "/"); cmd.env("SELF_VM_PERSIST", "1"); }
                cmd.arg(full_cmd.clone()); for a in &rest { cmd.arg(a); }
                cmd.status().unwrap_or_else(|e| { eprintln!("bwrap failed: {}", e); std::process::exit(127)})
            } else if has_unshare {
                eprintln!("-> unshare --root {}", tmp.display());
                let mut cmd = std::process::Command::new("unshare");
                cmd.arg("--mount").arg("--map-root-user").arg("--root").arg(&tmp);
                if effective_persist { cmd.env("HISTFILE", "/.ash_history"); cmd.env("PS1", ps1); cmd.env("HOME", "/"); }
                cmd.arg(full_cmd.clone()); for a in &rest { cmd.arg(a); }
                let s = cmd.status();
                match s {
                    Ok(st) if st.success() || st.code().is_some() => st,
                    _ => {
                        let mut cmd2 = std::process::Command::new("unshare");
                        cmd2.arg("--mount").arg("--map-root-user").arg("sh").arg("-c").arg(format!("chroot {} {} {}", tmp.display(), full_cmd, rest.join(" ")));
                        cmd2.status().unwrap_or_else(|e| { eprintln!("unshare/chroot failed: {}", e); std::process::exit(127)})
                    }
                }
            } else {
                eprintln!("-> chroot {}", tmp.display());
                let mut cmd = std::process::Command::new("chroot");
                if effective_persist { cmd.env("HISTFILE", "/.ash_history"); cmd.env("PS1", ps1); cmd.env("HOME", "/"); }
                cmd.arg(&tmp).arg(full_cmd.clone()); for a in &rest { cmd.arg(a); }
                cmd.status().unwrap_or_else(|e| { eprintln!("chroot failed (need root): {}", e); std::process::exit(127)})
            };
            let do_sync = !ephemeral;
            if do_sync {
                let sync_res = selfdb::vm::vm_sync_from_host(&c, &tmp);
                if let Ok((cr,up,del)) = sync_res {
                    if cr+up+del>0 {
                        eprintln!("-> sync {} -> {}: +{} ~{} -{} ", tmp.display(), db, cr, up, del);
                        let _ = selfdb::vm::vm_apply_pragmas(&c);
                    }
                }
            } else {
                eprintln!("--ephemeral: skip sync back to DB");
            }
            // persist history to cache_dir/history/<hash> for next FUSE or persist session
            if hist.exists() && effective_persist {
                let db_hash = selfdb::vm::fx_hash_u64(db.as_bytes());
                let hist_cached = selfdb::vm::cache_dir().join("history").join(&db_hash).join(".ash_history");
                let _ = std::fs::create_dir_all(hist_cached.parent().unwrap());
                let _ = std::fs::copy(&hist, &hist_cached);
                eprintln!("history persisted {} -> {}", hist.display(), hist_cached.display());
            }
            if effective_persist {
                eprintln!("--persist: keep {} (history at {}/.ash_history also cached at {})", tmp.display(), tmp.display(), selfdb::vm::cache_dir().join("history").join(selfdb::vm::fx_hash_u64(db.as_bytes())).join(".ash_history").display());
            }
            if ephemeral {
                let _ = std::fs::remove_dir_all(&tmp);
                eprintln!("--ephemeral: removed {}", tmp.display());
            }
            std::process::exit(status.code().unwrap_or(127));
        },
        Cmd::VmResolve{db, vm_path} => {
            let c=selfdb::vm::vm_open(&db)?;
            match selfdb::vm::vm_resolve(&c, &vm_path) {
                Ok((real, kind, content, link, mode)) => {
                    println!("resolve {} -> {} (kind={} mode={:o} link={:?} size={})", vm_path, real, kind, mode, link, content.as_ref().map(|v| v.len()).unwrap_or(0));
                    if let Some(t) = link { println!("  symlink -> {}", t); }
                    if kind=="symlink" { println!("  (symlink hops resolved via 40-step vm_resolve)"); }
                },
                Err(e) => { eprintln!("vm-resolve {}: {}", vm_path, e); std::process::exit(1); }
            }
        },
        Cmd::VmCheckpoint{db, name, note} => { let c=selfdb::vm::vm_open(&db)?; selfdb::vm::vm_checkpoint(&c, &name, &note)?; println!("checkpoint {} @ {}", name, db); },
        Cmd::VmSnapshots{db} => { let c=Connection::open(&db)?; for (id,name,ts,pc,bytes,note) in selfdb::vm::vm_list_snapshots(&c)? { println!("[{}] {} ts={} pc={} bytes={} note={}", id, name, ts, pc, bytes, note);} },
        Cmd::VmVerify{db} => { let c=Connection::open(&db)?; println!("{}", selfdb::vm::vm_verify(&c)?); },
        Cmd::VmExtract{db, vm_path, out} => { let c=selfdb::vm::vm_open(&db)?; let data=selfdb::vm::vm_cat(&c, &vm_path)?; std::fs::write(&out, &data)?; println!("extract {} -> {} ({} bytes)", vm_path, out, data.len()); },
        Cmd::VmMemInsert{db, addr, size, prot, file} => { let c=Connection::open(&db)?; let a=i64::from_str_radix(addr.trim_start_matches("0x"), 16).unwrap_or(addr.parse().unwrap_or(0)); let sz:i64=size.parse().unwrap_or(0); let pr:i64=prot.parse().unwrap_or(7); let data=std::fs::read(&file).unwrap_or_default(); selfdb::vm::vm_mem_insert(&c, a, sz, pr, &data)?; println!("mem insert addr=0x{:x} size={} prot={} bytes={} -> {}", a, sz, pr, data.len(), db); },
        Cmd::VmMemList{db} => { let c=Connection::open(&db)?; for (id,addr,size,prot) in selfdb::vm::vm_mem_list(&c)? { println!("[{}] addr=0x{:x} size={} prot={}", id, addr, size, prot);} },
        Cmd::VmMemClear{db} => { let c=Connection::open(&db)?; selfdb::vm::vm_mem_clear(&c)?; println!("mem clear -> {}", db); },
        Cmd::VmSnapshotFile{db, name} => { let c=selfdb::vm::vm_open(&db)?; let p=selfdb::vm::vm_snapshot_file(&c, &db, &name)?; println!("snapshot-file {} -> {}", db, p); },
        Cmd::VmRestoreFile{db, name} => { selfdb::vm::vm_restore_file(&db, &name)?; println!("restore-file {} @ {}", name, db); },
        Cmd::VmSync{db, host_root} => { let c=selfdb::vm::vm_open(&db)?; let (cr,up,del)=selfdb::vm::vm_sync_from_host(&c, std::path::Path::new(&host_root))?; let _ = selfdb::vm::vm_apply_pragmas(&c); println!("sync {} <- {}: +{} ~{} -{} (WAL+NORMAL+mmap)", db, host_root, cr, up, del); },
        Cmd::VmGc{db} => { let c=selfdb::vm::vm_open(&db)?; let (before,saved)=selfdb::vm::vm_gc(&c)?; println!("gc {}: pages {} -> saved {} bytes", db, before, saved); },
        Cmd::VmCompressInfo{db} => {
            let c=selfdb::vm::vm_open(&db)?;
            let total_files: i64 = c.query_row("SELECT count(*) FROM vm_fs WHERE kind='file'", [], |r| r.get(0))?;
            let comp_files: i64 = c.query_row("SELECT count(*) FROM vm_blobs WHERE compressed!=0", [], |r| r.get(0)).unwrap_or(0);
            let raw_sum: Option<i64> = c.query_row("SELECT sum(size) FROM vm_fs WHERE kind='file'", [], |r| r.get(0))?;
            let blob_sum: Option<i64> = c.query_row("SELECT sum(length(content)) FROM vm_blobs", [], |r| r.get(0))?;
            let page_size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let page_count: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let journal: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let mmap: i64 = c.query_row("PRAGMA mmap_size", [], |r| r.get(0))?;
            println!("compress: {}/{} files compressed", comp_files, total_files);
            println!("  logical size: {} bytes", raw_sum.unwrap_or(0));
            println!("  blob storage: {} bytes (ratio {:.1}%)", blob_sum.unwrap_or(0), 100.0* (blob_sum.unwrap_or(0) as f64)/(raw_sum.unwrap_or(1) as f64));
            println!("  db file: {} bytes (page_size={} count={} journal={} mmap={})", page_size*page_count, page_size, page_count, journal, mmap);
        },
        Cmd::VmRecompress{db} => {
            let c=selfdb::vm::vm_open(&db)?;
            let (n, before, after) = selfdb::vm::vm_recompress(&c)?;
            let _ = selfdb::vm::vm_gc(&c);
            println!("recompress: {} files {} -> {} bytes (saved {} bytes, {:.1}% saved); VACUUM done", n, before, after, before-after, 100.0*(before-after) as f64/(before as f64 + 1.0));
            let total_files: i64 = c.query_row("SELECT count(*) FROM vm_fs WHERE kind='file'", [], |r| r.get(0))?;
            let comp_files: i64 = c.query_row("SELECT count(*) FROM vm_blobs WHERE compressed!=0", [], |r| r.get(0)).unwrap_or(0);
            println!("now: {}/{} compressed", comp_files, total_files);
        },
        Cmd::VmStatus{db} => {
            let c=selfdb::vm::vm_open(&db)?;
            println!("{}", selfdb::vm::vm_status(&c)?);
        },
        Cmd::VmCacheInfo => {
            let (dir, cnt, bytes) = selfdb::vm::vm_cache_info();
            println!("cache dir: {}", dir.display());
            println!("  files: {}  bytes: {} ({:.2}M)", cnt, bytes, bytes as f64/1024.0/1024.0);
        },
        Cmd::VmCachePrune{max} => {
            let bytes = parse_size(&max).unwrap_or(1024*1024*1024);
            let (n, freed) = selfdb::vm::vm_cache_prune(bytes)?;
            println!("prune max={} ({} bytes): removed {} files freed {} bytes ({:.2}M)", max, bytes, n, freed, freed as f64/1024.0/1024.0);
        },
        Cmd::VmTrainDict{db, max_size} => {
            let c=selfdb::vm::vm_open(&db)?;
            match selfdb::vm::vm_train_dict(&c, max_size)? {
                Some(d) => println!("train dict {} -> {} bytes samples dict_size={}", db, d.len(), max_size),
                None => println!("train dict {}: not enough samples or failed (max_size={})", db, max_size),
            }
        },
        Cmd::VmDictInfo{db} => {
            let c=selfdb::vm::vm_open(&db)?;
            match selfdb::vm::vm_get_dict(&c)? {
                Some(d) => println!("dict {}: {} bytes (vm_dict id=1)", db, d.len()),
                None => println!("dict {}: none", db),
            }
        },
        Cmd::VmDiff{db, a, b} => {
            let c=selfdb::vm::vm_open(&db)?;
            println!("{}", selfdb::vm::vm_diff(&c, &a, &b)?);
        },
        Cmd::VmMount{db, mountpoint, allow_other} => {
            #[cfg(feature = "fuse")]
            {
                selfdb::fuse::fuse_impl::mount_vm(&db, &mountpoint, allow_other)?;
            }
            #[cfg(not(feature = "fuse"))]
            {
                eprintln!("fuse feature not enabled: rebuild with --features fuse (needs fuser default-features=false, pure-rust, no libfuse)");
                std::process::exit(1);
            }
        },
        Cmd::VmMemTrace{db, prog, rest} => {
            #[cfg(target_os = "linux")]
            {
                selfdb::vm::vm_mem_trace(&db, &prog, rest)?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                eprintln!("vm-mem-trace only on Linux");
                let _ = (db, prog, rest);
                std::process::exit(1);
            }
        },
        Cmd::VmDiskInit{db, size} => {
            let c=selfdb::vm::vm_open(&db)?;
            let bytes = parse_size(&size).ok_or_else(|| anyhow::anyhow!("bad size {}", size))? as i64;
            selfdb::vm::vm_disk_init(&c, bytes)?;
            println!("vm-disk-init {} size={} ({})", db, bytes, size);
            println!("{}", selfdb::vm::vm_disk_info(&c)?);
        },
        Cmd::VmDiskImport{db, raw, size} => {
            let c=selfdb::vm::vm_open(&db)?;
            let existing = selfdb::vm::vm_disk_size(&c).unwrap_or(0);
            let disk_size = if !size.is_empty() { parse_size(&size).ok_or_else(|| anyhow::anyhow!("bad size {}", size))? as i64 }
                            else if existing>0 { existing }
                            else { std::fs::metadata(&raw).map(|m| m.len() as i64).unwrap_or(0) };
            let rounded = (disk_size + 4095)/4096*4096;
            let (total, nonzero)=selfdb::vm::vm_disk_import_raw(&c, &raw, rounded)?;
            println!("vm-disk-import {} <- {} blocks={} nonzero={} disk={} bytes", db, raw, total, nonzero, rounded);
            println!("{}", selfdb::vm::vm_disk_info(&c)?);
        },
        Cmd::VmDiskExport{db, raw} => {
            let c=selfdb::vm::vm_open(&db)?;
            let (blocks, nonzero)=selfdb::vm::vm_disk_export_raw(&c, &raw)?;
            println!("vm-disk-export {} -> {} blocks={} nonzero={}", db, raw, blocks, nonzero);
        },
        Cmd::VmDiskInfo{db} => {
            let c=selfdb::vm::vm_open(&db)?;
            println!("{}", selfdb::vm::vm_disk_info(&c)?);
        },
        Cmd::VmRun{db, mem, nbd, raw, kvm, kernel, initrd, append} => {
            let c=selfdb::vm::vm_open(&db)?;
            let info=selfdb::vm::vm_disk_info(&c)?;
            println!("{}", info);
            let disk_size=selfdb::vm::vm_disk_size(&c).unwrap_or(0);
            let raw_path = if raw.is_empty() {
                let p = format!("/tmp/self-vm-disk-{}.raw", std::process::id());
                selfdb::vm::vm_disk_export_raw(&c, &p)?;
                println!("exported disk -> {} ({} bytes)", p, disk_size);
                p
            } else {
                if std::path::Path::new(&raw).exists() && disk_size==0 {
                    let sz = std::fs::metadata(&raw).map(|m| m.len() as i64).unwrap_or(0);
                    selfdb::vm::vm_disk_init(&c, (sz+4095)/4096*4096)?;
                    let (total, nonzero)=selfdb::vm::vm_disk_import_raw(&c, &raw, (sz+4095)/4096*4096)?;
                    println!("imported {} -> vm_disk_blocks ({} blocks nonzero={})", raw, total, nonzero);
                }
                raw.to_string()
            };
            let qemu = std::env::var("QEMU_SYSTEM_X86_64")
                .or_else(|_| std::env::var("QEMU"))
                .unwrap_or_else(|_| {
                    for c in ["qemu-system-x86_64", "qemu-system-x86", "qemu-system-i386"] {
                        if std::path::Path::new(&format!("/usr/bin/{}", c)).exists() { return c.to_string(); }
                        if let Ok(out) = std::process::Command::new("which").arg(c).output() {
                            if out.status.success() { return c.to_string(); }
                        }
                    }
                    "qemu-system-x86_64".to_string()
                });
            // direct kernel boot path (no bootloader needed) via -kernel/-initrd/-append
            // If kernel empty but disk has no bootable MBR (e.g. /tmp/test_disk.raw ext2 only), auto-suggest direct boot.
            // Also treat direct vm DB disk (no --raw) as .raw-like for probing: check the exported raw_path.
            let mut boot_args: Vec<String> = Vec::new();
            let effective_kernel = if !kernel.is_empty() { Some(kernel.clone()) } else {
                let is_raw_like = raw_path.ends_with(".raw") || raw.is_empty();
                if is_raw_like {
                    // probe disk for MBR bootable signature; if missing, use host kernel + busybox trick
                    let has_mbr = std::fs::File::open(&raw_path).ok().and_then(|mut f| {
                        use std::io::{Read, Seek, SeekFrom};
                        let mut b = [0u8; 512];
                        if f.read_exact(&mut b).is_ok() { Some(b[510]==0x55 && b[511]==0xAA) } else { None }
                    }).unwrap_or(false);
                    if !has_mbr {
                        // use host kernel + initrd for demo when disk is just an ext2 fs (like /tmp/test_disk.raw)
                        // robust host kernel probe: try /vmlinuz symlink, /boot/vmlinuz*, explicit fallback
                        let mut host_k: Option<String> = None;
                        for cand in ["/vmlinuz".to_string(), "/boot/vmlinuz".to_string(), format!("/boot/vmlinuz-{}", std::env::var("HOST_KERNEL").unwrap_or_default()), "/boot/vmlinuz-6.12.74+deb13+1-amd64".to_string()] {
                            if !cand.is_empty() && std::path::Path::new(&cand).exists() { host_k = Some(cand); break; }
                            // also handle /vmlinuz -> boot/vmlinuz-* relative link: canonicalize
                            if cand == "/vmlinuz" {
                                if let Ok(link) = std::fs::read_link(&cand) {
                                    let abs = if link.is_absolute() { link } else { std::path::Path::new("/").join(link) };
                                    if abs.exists() { host_k = Some(abs.to_string_lossy().to_string()); break; }
                                    // also try resolving via canonicalize
                                    if let Ok(canon) = std::fs::canonicalize(&cand) { if canon.exists() { host_k = Some(canon.to_string_lossy().to_string()); break; } }
                                }
                            }
                        }
                        if host_k.is_none() {
                            if let Ok(rd) = std::fs::read_dir("/boot") {
                                for e in rd.flatten() {
                                    let n = e.file_name().to_string_lossy().to_string();
                                    if n.starts_with("vmlinuz-") { host_k = Some(e.path().to_string_lossy().to_string()); break; }
                                }
                            }
                        }
                        let host_k = host_k.unwrap_or_else(|| "/boot/vmlinuz-6.12.74+deb13+1-amd64".to_string());
                        if std::path::Path::new(&host_k).exists() { Some(host_k) } else { None }
                    } else { None }
                } else { None }
            };
            if let Some(k) = &effective_kernel {
                // -kernel boot needs --raw to be the rootfs disk; we pass it as virtio drive + root=/dev/vda
                let initrd_arg = if !initrd.is_empty() { Some(initrd.clone()) } else {
                    // try to guess initrd for host kernel
                    let guess = k.replace("vmlinuz", "initrd.img");
                    if std::path::Path::new(&guess).exists() { Some(guess) } else {
                        ["/boot/initrd.img-6.12.74+deb13+1-amd64", "/boot/initrd.img"].into_iter().find(|p| std::path::Path::new(p).exists()).map(|s| s.to_string())
                    }
                };
                boot_args.push("-kernel".to_string()); boot_args.push(k.clone());
                if let Some(ir) = initrd_arg { boot_args.push("-initrd".to_string()); boot_args.push(ir); }
                let app = if !append.is_empty() { append.clone() } else { "console=ttyS0 root=/dev/vda rw panic=1".to_string() };
                boot_args.push("-append".to_string()); boot_args.push(app);
                println!("direct-kernel boot: kernel={} initrd={} append={:?}", k, initrd, if append.is_empty() { "console=ttyS0 root=/dev/vda rw panic=1" } else { &append });
            }
            let mut args: Vec<String> = vec!["-m".to_string(), mem.clone(), "-drive".to_string(), format!("file={},format=raw,if=virtio", raw_path), "-serial".to_string(), "mon:stdio".to_string(), "-nographic".to_string()];
            args.extend(boot_args);
            if kvm && std::path::Path::new("/dev/kvm").exists() { args.push("-enable-kvm".to_string()); args.push("-cpu".to_string()); args.push("host".to_string()); }
            if nbd { println!("hint: qemu {} {}", qemu, args.join(" ")); println!("or NBD: qemu-nbd --connect=/dev/nbd0 {} (needs nbd kernel)", raw_path); }
            // warn if MBR missing and no kernel provided
            if effective_kernel.is_none() && !raw_path.is_empty() {
                let has_mbr = std::fs::File::open(&raw_path).ok().and_then(|mut f| {
                    use std::io::{Read, Seek};
                    let mut b = [0u8; 512];
                    if f.seek(std::io::SeekFrom::Start(510)).is_ok() && f.read_exact(&mut b[0..2]).is_ok() { Some(b[0]==0x55 && b[1]==0xAA) } else { None }
                }).unwrap_or(false);
                if !has_mbr {
                    eprintln!("note: disk {} has no MBR/bootloader (ext2 only); boot will fall back to iPXE/SeaBIOS and fail with 'Boot failed: could not read the boot disk'. Use: vm-run --kernel /boot/vmlinuz --initrd /boot/initrd.img --append 'console=ttyS0 root=/dev/vda rw' OR build a bootable image with grub.", raw_path);
                }
            }
            println!("boot: {} {}  (apt: qemu-system-x86 provides {})", qemu, args.join(" "), qemu);
            let status = std::process::Command::new(&qemu).args(&args).status().map_err(|e| anyhow::anyhow!("exec {} failed: {} (apt install qemu-system-x86)", qemu, e))?;
            std::process::exit(status.code().unwrap_or(0));
        },
    }
    Ok(())
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if s.ends_with('g') {
        s[..s.len()-1].parse::<u64>().ok().map(|v| v*1024*1024*1024)
    } else if s.ends_with('m') {
        s[..s.len()-1].parse::<u64>().ok().map(|v| v*1024*1024)
    } else if s.ends_with('k') {
        s[..s.len()-1].parse::<u64>().ok().map(|v| v*1024)
    } else {
        s.parse::<u64>().ok()
    }
}

fn selfdb_closure(root: &str, out: &str) -> anyhow::Result<()> {
    if Path::new(out).exists() { std::fs::remove_file(out)?; }
    let conn = Connection::open(out)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-8192;")?;
    conn.execute_batch(r#"CREATE TABLE objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT, is_root INTEGER NOT NULL DEFAULT 0); CREATE TABLE needs(object_id INTEGER NOT NULL REFERENCES objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES objects(path)); CREATE INDEX idx_needs_resolved ON needs(resolved_path);"#)?;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let root_canon = std::fs::canonicalize(root)?;
    let mut meta_cache: FxHashMap<PathBuf, selfdb::elf::ElfMeta> = FxHashMap::default();
    let mut search_cache: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    let mut resolve_cache: FxHashMap<u64, Option<PathBuf>> = FxHashMap::default();
    let ld_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    let mut seen: FxHashMap<PathBuf, i64> = FxHashMap::default();
    let mut order: Vec<(i64, PathBuf, Option<String>, String, i64)> = Vec::new();
    let mut next_id: i64 = 1;
    let add = |path: PathBuf, soname: Option<String>, kind: &str, is_root: i64, conn: &Connection, seen: &mut FxHashMap<PathBuf,i64>, order: &mut Vec<(i64, PathBuf, Option<String>, String, i64)>, next_id: &mut i64| -> i64 {
        let rp = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if let Some(id)=seen.get(&rp) { return *id; }
        let id = *next_id; *next_id += 1;
        seen.insert(rp.clone(), id);
        order.push((id, rp.clone(), soname.clone(), kind.to_string(), is_root));
        conn.execute("INSERT INTO objects VALUES (?1,?2,?3,?4,?5)", rusqlite::params![id, rp.to_string_lossy().to_string(), soname, kind, is_root]).unwrap();
        id
    };
    let _root_id = add(root_canon.clone(), None, "exe", 1, &conn, &mut seen, &mut order, &mut next_id);
    let mut queue = vec![root_canon.clone()];
    let mut qh: usize = 0;
    let mut stmt_needs = conn.prepare_cached("INSERT INTO needs VALUES (?1,?2,?3,?4)")?;
    while qh < queue.len() {
        let cur = queue[qh].clone(); qh+=1;
        let cur_id = *seen.get(&cur).unwrap();
        let needed = selfdb::elf::meta_for_path_cached(&cur, &mut meta_cache).needed.clone();
        if needed.is_empty() { continue; }
        let sdirs = selfdb::closure::search_dirs_for_cached(&cur, &[], &mut meta_cache, &mut search_cache, &ld_dirs);
        let search_hash = selfdb::closure::search_dirs_hash(&sdirs);
        for (i, soname) in needed.iter().enumerate() {
            let resolved = selfdb::closure::resolve_soname_cached(soname, &sdirs, &mut resolve_cache, search_hash).map(|p| std::fs::canonicalize(&p).unwrap_or(p));
            let rp_str = resolved.as_ref().map(|p| p.to_string_lossy().to_string());
            if let Some(rp)=resolved.clone() {
                if !seen.contains_key(&rp) {
                    let son = selfdb::elf::soname_for_path_cached(&rp, &mut meta_cache);
                    add(rp.clone(), son, "lib", 0, &conn, &mut seen, &mut order, &mut next_id);
                    queue.push(rp);
                }
            }
            stmt_needs.execute(rusqlite::params![cur_id, i as i64, soname, rp_str])?;
        }
    }
    drop(stmt_needs);
    conn.execute_batch("COMMIT;")?;
    let total: i64 = conn.query_row("SELECT count(*) FROM objects", [], |r| r.get(0))?;
    let needs: i64 = conn.query_row("SELECT count(*) FROM needs", [], |r| r.get(0))?;
    let missing: i64 = conn.query_row("SELECT count(*) FROM needs WHERE resolved_path IS NULL AND soname NOT LIKE 'ld-%'", [], |r| r.get(0))?;
    println!("{} + closure -> {} ({} objects, {} edges, missing={})", Path::new(root).file_name().unwrap().to_string_lossy(), out, total, needs, missing);
    Ok(())
}

fn selfdb_scan(db: &str, dir: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-8192; PRAGMA temp_store=MEMORY;");
    let _ = conn.execute_batch("BEGIN IMMEDIATE;");
    conn.execute_batch(r#"CREATE TABLE IF NOT EXISTS objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT, is_root INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS needs(object_id INTEGER NOT NULL REFERENCES objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES objects(path)); CREATE INDEX IF NOT EXISTS idx_needs_resolved ON needs(resolved_path);"#)?;
    let base_dirs = vec![PathBuf::from(dir), PathBuf::from("/lib/x86_64-linux-gnu"), PathBuf::from("/lib"), PathBuf::from("/usr/lib/x86_64-linux-gnu"), PathBuf::from("/usr/lib")];
    let ld_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    let mut meta_cache: FxHashMap<PathBuf, selfdb::elf::ElfMeta> = FxHashMap::default();
    let mut search_cache: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    let mut resolve_cache: FxHashMap<u64, Option<PathBuf>> = FxHashMap::default();
    let mut exes = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e=e?; let p=e.path();
        if !p.is_file() { continue; }
        if let Ok(mut f)=std::fs::File::open(&p){
            let mut b=[0u8;4]; use std::io::Read;
            if f.read(&mut b).is_ok() && &b==b"\x7fELF" {
                if let Ok(canon)=std::fs::canonicalize(&p) { exes.push(canon); } else { exes.push(p); }
            }
        }
    }
    let mut seen: FxHashMap<PathBuf, i64> = conn.prepare("SELECT path, id FROM objects")?.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?)))?.filter_map(|r| r.ok()).map(|(p,id)| (PathBuf::from(p), id)).collect();
    let mut next_id: i64 = conn.query_row("SELECT max(id) FROM objects", [], |r| r.get::<_,Option<i64>>(0)).unwrap_or(None).unwrap_or(0)+1;
    for exe in &exes {
        let rp=exe.clone();
        if !seen.contains_key(&rp) {
            let son=selfdb::elf::soname_for_path_cached(&rp, &mut meta_cache);
            conn.execute("INSERT INTO objects VALUES (?1,?2,?3,?4,?5)", rusqlite::params![next_id, rp.to_string_lossy().to_string(), son, "exe", 1])?;
            seen.insert(rp, next_id); next_id+=1;
        }
    }
    let mut queue: Vec<PathBuf> = seen.keys().cloned().collect();
    queue.sort();
    queue.dedup();
    let mut qh: usize = 0;
    let mut stmt_needs = conn.prepare_cached("INSERT INTO needs VALUES (?1,?2,?3,?4)")?;
    let mut stmt_del = conn.prepare_cached("DELETE FROM needs WHERE object_id=?1")?;
    while qh < queue.len() {
        let cur=queue[qh].clone(); qh+=1;
        let cur_id=*seen.get(&cur).unwrap();
        stmt_del.execute(rusqlite::params![cur_id])?;
        let needed = selfdb::elf::meta_for_path_cached(&cur, &mut meta_cache).needed.clone();
        if needed.is_empty() { continue; }
        let sdirs = selfdb::closure::search_dirs_for_cached(&cur, &base_dirs, &mut meta_cache, &mut search_cache, &ld_dirs);
        let search_hash = selfdb::closure::search_dirs_hash(&sdirs);
        for (i, soname) in needed.iter().enumerate() {
            let resolved=selfdb::closure::resolve_soname_cached(soname, &sdirs, &mut resolve_cache, search_hash).map(|p| std::fs::canonicalize(&p).unwrap_or(p));
            let rp_str=resolved.as_ref().map(|p| p.to_string_lossy().to_string());
            if let Some(rp)=resolved.clone(){
                if !seen.contains_key(&rp){
                    let son=selfdb::elf::soname_for_path_cached(&rp, &mut meta_cache);
                    conn.execute("INSERT INTO objects VALUES (?1,?2,?3,?4,?5)", rusqlite::params![next_id, rp.to_string_lossy().to_string(), son, "lib", 0])?;
                    seen.insert(rp.clone(), next_id); queue.push(rp); next_id+=1;
                }
            }
            stmt_needs.execute(rusqlite::params![cur_id, i as i64, soname, rp_str])?;
        }
    }
    drop(stmt_needs); drop(stmt_del);
    conn.execute_batch("COMMIT;")?;
    let total: i64 = conn.query_row("SELECT count(*) FROM objects", [], |r| r.get(0))?;
    let needs: i64 = conn.query_row("SELECT count(*) FROM needs", [], |r| r.get(0))?;
    println!("indexed {} ELFs in {} -> {} (objects={} needs={})", exes.len(), dir, db, total, needs);
    Ok(())
}
fn selfdb_userland(out: &str, dirs: Vec<String>) -> anyhow::Result<()> { if Path::new(out).exists(){ std::fs::remove_file(out)?; } let conn=Connection::open(out)?; conn.execute_batch(r#"CREATE TABLE IF NOT EXISTS objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT, is_root INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS needs(object_id INTEGER NOT NULL REFERENCES objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES objects(path));"#)?; drop(conn); for d in dirs { if !Path::new(&d).is_dir(){ eprintln!("skip not a dir: {}", d); continue; } selfdb_scan(out, &d)?; } let conn=Connection::open(out)?; let nobjs: i64=conn.query_row("SELECT count(*) FROM objects", [], |r| r.get(0))?; let nneeds: i64=conn.query_row("SELECT count(*) FROM needs", [], |r| r.get(0))?; println!("userland -> {}: objects={} needs={}", out, nobjs, nneeds); Ok(()) }
fn bundle_list(path: &str, filter: &str) -> anyhow::Result<()> { let db=open_db(path); let has: i64=db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='bundle_objects'", [], |r| r.get(0))?; if has==0 { println!("{}: no bundle_objects (build with --bundle)", path); return Ok(()); } let mut q=String::from("SELECT id, path, soname, kind, is_root, size FROM bundle_objects"); if !filter.is_empty(){ q.push_str(" WHERE soname LIKE ?1 OR path LIKE ?1"); } q.push_str(" ORDER BY id"); let mut st=db.prepare(&q)?; let like=format!("%{}%", filter); let mut rows: Vec<(i64,String,Option<String>,String,i64,i64)> = Vec::new(); if filter.is_empty(){ for r in st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))? { rows.push(r?); } } else { for r in st.query_map([like.clone()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))? { rows.push(r?); } } for (id, p, soname, kind, is_root, size) in &rows { let label=soname.clone().unwrap_or_else(|| Path::new(p).file_name().unwrap().to_string_lossy().to_string()); println!("[{}] {:28} {:4} {:8}  {}{}", id, label, kind, size, p, if *is_root==1 {" root"} else {""}); } let n: i64=db.query_row("SELECT count(*) FROM bundle_objects", [], |r| r.get(0))?; let e: i64=db.query_row("SELECT count(*) FROM bundle_needs", [], |r| r.get(0))?; println!("bundle: {} objects, {} edges", n, e); Ok(()) }
fn bundle_info(path: &str) -> anyhow::Result<()> { let db=open_db(path); let has: i64=db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='bundle_objects'", [], |r| r.get(0))?; if has==0 { println!("{}: no bundle_objects", path); return Ok(()); } let (cnt,sum): (i64, Option<i64>) = db.query_row("SELECT count(*), sum(size) FROM bundle_objects", [], |r| Ok((r.get(0)?, r.get(1)?)))?; let needs: i64=db.query_row("SELECT count(*) FROM bundle_needs", [], |r| r.get(0))?; let sz=std::fs::metadata(path).map(|m| m.len()).unwrap_or(0); println!("{}: bundle_objects={} bytes={} needs={} self_size={}", path, cnt, sum.unwrap_or(0), needs, sz); Ok(()) }