use clap::{Parser, Subcommand};
use rusqlite::Connection;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};

const APP_ID: u32 = 0x53454C46;

#[derive(Parser)]
#[command(name="self", about="inspect and pack SELF files")]
struct Cli { #[command(subcommand)] cmd: Cmd }

/// How a command that runs VM content is sandboxed. Unset limits keep the command's defaults.
#[derive(clap::Args, Debug, Clone)]
struct SandboxArgs {
    #[arg(long, default_value="bwrap", help="isolation boundary: bwrap (namespaces + seccomp, shared kernel) or gvisor (runsc)")]
    isolation: String,
    #[arg(long, help="memory limit of the whole sandbox in MiB (cgroup memory.max, no swap)")]
    memory_mb: Option<u64>,
    #[arg(long, help="process and thread limit of the sandbox (cgroup pids.max)")]
    pids: Option<u64>,
    #[arg(long, help="CPU quota in percent of one CPU, e.g. 200 = two CPUs (cgroup cpu.max)")]
    cpu_percent: Option<u64>,
    #[arg(long, help="RLIMIT_AS per process in MiB")]
    address_space_mb: Option<u64>,
    #[arg(long, help="RLIMIT_CPU per process in seconds")]
    cpu_secs: Option<u64>,
    #[arg(long, help="RLIMIT_FSIZE per process in MiB")]
    file_mb: Option<u64>,
    #[arg(long, help="run without any resource limit (trusted code only: the sandbox can exhaust the host)")]
    no_limits: bool,
}

impl SandboxArgs {
    fn isolation(&self) -> anyhow::Result<selfdb::sandbox::Isolation> { selfdb::sandbox::Isolation::parse(&self.isolation) }
    fn limits(&self, defaults: selfdb::sandbox::Limits) -> selfdb::sandbox::Limits {
        if self.no_limits { return selfdb::sandbox::Limits::none(); }
        selfdb::sandbox::Limits {
            memory_mb: self.memory_mb.unwrap_or(defaults.memory_mb),
            pids: self.pids.unwrap_or(defaults.pids),
            cpu_percent: self.cpu_percent.unwrap_or(defaults.cpu_percent),
            address_space_mb: self.address_space_mb.unwrap_or(defaults.address_space_mb),
            cpu_secs: self.cpu_secs.unwrap_or(defaults.cpu_secs),
            file_size_mb: self.file_mb.unwrap_or(defaults.file_size_mb),
        }
    }
}

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
    Run{ path: String, #[arg(trailing_var_arg=true, allow_hyphen_values=true)] rest: Vec<String> },
    VmInit{ db: String, #[arg(long)] force: bool, #[arg(long)] vm_only: bool },
    VmAdd{ db: String, host: String, vm_path: String },
    VmPack{ db: String, host_dir: String, #[arg(long, default_value="/")] prefix: String, #[arg(long)] serial: bool },
    VmImport{ db: String, elf: String, #[arg(long, default_value="/")] prefix: String },
    VmImportRootfs{ db: String, tar: String, #[arg(long, default_value="")] strip: String, #[arg(long)] whitelist: Vec<String>, #[arg(long)] exclude: Vec<String> },
    VmMaterialize{ db: String, dest: String },
    VmLs{ db: String, #[arg(default_value="/")] path: String },
    VmCat{ db: String, path: String },
    VmStat{ db: String, path: String },
    /// Run one VM binary in a sandbox whose root holds only that binary and the VM's libraries.
    VmExec{ db: String, vm_path: String, #[command(flatten)] sandbox: SandboxArgs, #[arg(trailing_var_arg=true, allow_hyphen_values=true)] rest: Vec<String> },
    /// Interactive shell (or a command) inside the VM, in a sandbox.
    VmChroot{ db: String, #[arg(default_value="/bin/sh")] cmd: String, #[arg(long, help="write changes back and keep shell history in the VM (the default unless --ephemeral)")] persist: bool, #[arg(long, help="discard every change made in the session")] ephemeral: bool, #[command(flatten)] sandbox: SandboxArgs, #[arg(trailing_var_arg=true, allow_hyphen_values=true)] rest: Vec<String> },
    VmResolve{ db: String, vm_path: String },
    VmCheckpoint{ db: String, name: String, #[arg(long, default_value="")] note: String },
    VmSnapshots{ db: String },
    VmVerify{ db: String, #[arg(long)] deep: bool },
    VmRechunk{ db: String },
    VmExtract{ db: String, vm_path: String, out: String },
    VmMemInsert{ db: String, addr: String, size: String, prot: String, file: String },
    VmMemList{ db: String },
    VmMemClear{ db: String },
    VmSnapshotFile{ db: String, name: String },
    VmRestoreFile{ db: String, name: String },
    VmSync{ db: String, host_root: String, #[arg(long, help="re-read every file and compare content, not just metadata")] full: bool },
    VmGc{ db: String },
    VmCompressInfo{ db: String },
    VmRecompress{ db: String },
    VmStatus{ db: String },
    VmCacheInfo,
    VmCachePrune{ #[arg(long, default_value="1G")] max: String },
    VmTrainDict{ db: String, #[arg(long, default_value="16384")] max_size: usize },
    VmDictInfo{ db: String },
    VmDiff{ db: String, a: String, b: String },
    /// content snapshot of the whole tree (metadata + chunk references, no blob copies)
    VmSnap{ db: String, name: String, #[arg(long, default_value="")] note: String },
    /// replace the live tree with a content snapshot
    VmRollback{ db: String, name: String },
    /// per-path diff between the live tree and a content snapshot
    VmSnapDiff{ db: String, name: String },
    /// delete a content snapshot; blobs no other holder uses are released
    VmSnapDelete{ db: String, name: String },
    /// Run one task with the pi agent inside the VM: no network, model via the host proxy.
    VmAgent{ db: String, #[arg(long, default_value_t=600, help="kill the agent after N seconds")] timeout: u64,
             #[arg(long)] rollback_on_fail: bool,
             #[arg(long, default_value="https://cpa.lolli.tech/v1")] model_base: String,
             #[arg(long, default_value="PIMM_API_KEY")] key_env: String,
             #[arg(long, default_value="gpt-6.1-sol")] model: String,
             #[arg(long, default_value_t=18081)] port: u16,
             #[arg(long, default_value_t=selfdb::agent::DEFAULT_MAX_OUTPUT_BYTES)] max_output: usize,
             #[command(flatten)] sandbox: SandboxArgs,
             #[arg(long, help="read-only Base database; <DB> becomes a Delta on top of it")]
             base: Option<String>,
             #[arg(long, default_value="", help="comma-separated hosts the agent may fetch packages from via /pkg/<host>/<path>")]
             allow_hosts: String,
             #[arg(long, help="EXPERIMENTAL: bind the sandbox to a FUSE mount instead of unpacking to /tmp. The O_EXCL create failure that stopped pi is fixed, but a full agent run over the mount has not been verified yet; the materialised path stays the default.")]
             fuse: bool,
             task: String },
    /// Host-side model proxy: a unix socket the sandboxed agent reaches without network access.
    VmProxy{ #[arg(long)] base: String, #[arg(long)] key_env: String, socket: String,
             #[arg(long, default_value_t=18081)] port: u16, #[arg(long)] forwarder: Option<String>,
             #[arg(long, default_value="", help="comma-separated hosts allowed for package downloads via /pkg/<host>/<path>")]
             allow_hosts: String },
    /// Put node and the pi coding agent into a VM, so `pi` can run inside it.
    VmProvisionAgent{ db: String, #[arg(long)] node: Option<String>, #[arg(long)] pi: Option<String>,
                      #[arg(long, default_value="minimal", help="system tools to install: none, minimal or dev (git, coreutils, python3+stdlib, make, gcc)")]
                      tools: String },
    /// Merge a package VM database (a tool and its libraries) into a target, in one transaction.
    VmInject{ target: String, package: String },
    /// Write policies enforced by the database: list, add, or remove a rule.
    VmPolicy{ db: String, #[command(subcommand)] action: PolicyAction },
    /// List recorded agent steps (oldest first); --json prints one JSON array.
    AgentLog{ db: String, #[arg(long, help="print a JSON array instead of a table")] json: bool, #[arg(long, help="only the most recent N steps")] last: Option<usize> },
    /// Run one command in a sandbox against the VM; record output, changes, and optionally roll back.
    VmStep{ db: String, #[arg(long, default_value_t=60, help="kill the step after N seconds")] timeout: u64, #[arg(long, help="restore the pre-step snapshot if the step fails or times out")] rollback_on_fail: bool, #[arg(long, default_value_t=selfdb::agent::DEFAULT_MAX_OUTPUT_BYTES, help="keep at most N bytes of each of stdout/stderr")] max_output: usize, #[command(flatten)] sandbox: SandboxArgs, #[arg(trailing_var_arg=true, allow_hyphen_values=true, required=true)] argv: Vec<String> },
    /// keep the newest N content snapshots, delete the rest (reports each removed name)
    VmRetain{ db: String, keep: usize },
    VmMount{ db: String, mountpoint: String, #[arg(long)] allow_other: bool },
    VmMemTrace{ db: String, prog: String, #[arg(trailing_var_arg=true, allow_hyphen_values=true)] rest: Vec<String> },
    VmDiskInit{ db: String, #[arg(long, default_value="20G")] size: String },
    VmDiskImport{ db: String, raw: String, #[arg(long, default_value="")] size: String },
    VmDiskExport{ db: String, raw: String },
    VmDiskInfo{ db: String },
    VmRun{ db: String, #[arg(long, default_value="512M")] mem: String, #[arg(long, default_value="")] raw: String, #[arg(long)] kvm: bool, #[arg(long, default_value="")] kernel: String, #[arg(long, default_value="")] initrd: String, #[arg(long, default_value="")] append: String },
}

fn open_db(path: &str) -> Connection {
    let conn = Connection::open(path).unwrap();
    // a SELF file may come from anyone: the same defenses as for VM databases
    if let Err(e) = selfdb::vm::harden(&conn) { eprintln!("{}: {}", path, e); std::process::exit(1); }
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
        Cmd::VmAdd{db, host, vm_path} => { let c=selfdb::vm::vm_open(&db)?; selfdb::vm::vm_add_file(&c, &host, &vm_path)?; println!("add {} -> {}:{}", host, db, vm_path); },
        Cmd::VmPack{db, host_dir, prefix, serial} => { let c=selfdb::vm::vm_open(&db)?; let n=if serial { selfdb::vm::vm_pack_host_dir(&c, &host_dir, &prefix)? } else { selfdb::vm::vm_pack_host_dir_parallel(&c, &host_dir, &prefix)? }; println!("pack {} -> {}:{} ({} entries)", host_dir, db, prefix, n); },
        Cmd::VmImport{db, elf, prefix} => { let c=selfdb::vm::vm_open(&db)?; let n=selfdb::vm::vm_import_closure(&c, &elf, &prefix)?; println!("import {} -> {} ({} objects)", elf, db, n); },
        Cmd::VmImportRootfs{db, tar, strip, whitelist, exclude} => { let c=selfdb::vm::vm_open(&db)?; let wl: Option<&[String]> = if whitelist.is_empty() { None } else { Some(&whitelist) }; let ex: Option<&[String]> = if exclude.is_empty() { None } else { Some(&exclude) }; let n=selfdb::vm::vm_import_tar_filtered(&c, &tar, &strip, wl, ex)?; println!("import-tar {} -> {} ({} entries) whitelist={:?} exclude={:?}", tar, db, n, whitelist, exclude); if n>50 { let _ = selfdb::vm::vm_train_dict(&c, 16384); } },
        Cmd::VmMaterialize{db, dest} => { let c=selfdb::vm::vm_open(&db)?; let n=selfdb::vm::vm_materialize_tree(&c, std::path::Path::new(&dest))?; println!("materialize {} -> {} ({} files)", db, dest, n); },
        Cmd::VmLs{db, path} => { let c=selfdb::vm::vm_open(&db)?; for e in selfdb::vm::vm_ls(&c, &path)? { println!("{:8} {:4} {:>10}  {}", e.kind, format!("{:o}", e.mode), e.size, e.path);} },
        Cmd::VmCat{db, path} => { let c=selfdb::vm::vm_open(&db)?; let data=selfdb::vm::vm_cat(&c, &path)?; use std::io::Write; std::io::stdout().write_all(&data)?; },
        Cmd::VmStat{db, path} => { let c=selfdb::vm::vm_open(&db)?; let (p,k,m,sz,mtime,hash)=selfdb::vm::vm_stat(&c, &path)?; println!("path={} kind={} mode={:o} size={} mtime={} hash={}", p,k,m,sz,mtime,hash); },
        Cmd::VmExec{db, vm_path, sandbox, rest} => {
            let c=selfdb::vm::vm_open(&db)?;
            let tmp = selfdb::vm::make_tmp_dir("self-vm-")?;
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
                        for r in st.query_map([], |r| r.get::<_,String>(0))? { let pp=r?; if pp.ends_with(&soname) { to_mat.push(pp); break; } }
                    }
                }
                // eagerly materialize libs + their symlink aliases
                let mut all_libs: Vec<String> = Vec::new();
                {
                    let mut st=c.prepare("SELECT path, kind, link_target FROM vm_fs WHERE (path LIKE '/lib/%' OR path LIKE '/lib64/%' OR path LIKE '/usr/lib/%' OR path LIKE '/usr/lib64/%')")?;
                    for r in st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,Option<String>>(2)?)))? {
                        let (path,kind,link)=r?;
                        if kind=="symlink" {
                            let host = tmp.join(path.trim_start_matches('/'));
                            if host.exists() || std::fs::symlink_metadata(&host).is_ok() { continue; }
                            if let Some(par)=host.parent(){ std::fs::create_dir_all(par)?; }
                            let target = link.unwrap_or_default();
                            let _ = std::os::unix::fs::symlink(&target, &host);
                        } else {
                            all_libs.push(path);
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
                        if let Ok(Some(t)) = c.query_row("SELECT link_target FROM vm_fs WHERE path=?1", rusqlite::params![lp.clone()], |r| r.get::<_,Option<String>>(0)) {
                            let _ = std::os::unix::fs::symlink(&t, &host); continue;
                        }
                    }
                    let _ = selfdb::vm::vm_materialize(&c, &lp, &host);
                }
            }
            // The binary runs in a sandbox rooted at the partial tree: never on the host, where it would
            // see the host's files, network and environment.
            struct RemoveOnDrop(PathBuf);
            impl Drop for RemoveOnDrop { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
            let _cleanup = RemoveOnDrop(tmp.clone());
            let mut argv = vec![vm_path.clone()];
            argv.extend(rest);
            let mut spec = selfdb::sandbox::Spec::new(&tmp, argv);
            spec.interactive = true;
            spec.limits = sandbox.limits(selfdb::sandbox::Limits::default());
            let code = selfdb::sandbox::run_interactive(&spec, sandbox.isolation()?)?;
            drop(_cleanup);
            std::process::exit(code);
        },
        Cmd::VmChroot{db, cmd, persist, ephemeral, sandbox, rest} => {
            if persist && ephemeral { eprintln!("cannot use both --persist and --ephemeral"); std::process::exit(2); }
            let isolation = sandbox.isolation()?;
            let c = selfdb::vm::vm_open(&db)?;
            let mut argv = vec![if cmd.is_empty() { "/bin/sh".to_string() } else { cmd }];
            argv.extend(rest);
            // Only what an interactive shell needs crosses into the sandbox. History lives in the VM
            // (/.ash_history), so it is kept with the VM's other content and discarded by --ephemeral.
            let mut env: Vec<(String, String)> = vec![("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()), ("HOME".into(), "/".into()),
                                                      ("PS1".into(), r"vm:\w\$ ".into()), ("HISTFILE".into(), "/.ash_history".into())];
            if let Ok(term) = std::env::var("TERM") { env.push(("TERM".into(), term)); }
            let spec_for = |root: &Path| {
                let mut spec = selfdb::sandbox::Spec::new(root, argv.clone());
                spec.env = env.clone();
                spec.interactive = true;
                spec.limits = sandbox.limits(selfdb::sandbox::Limits::default());
                spec
            };
            // A persistent session over a mount writes to the database as it goes. An ephemeral one never
            // uses the mount: its writes would reach the database too.
            #[cfg(feature = "fuse")]
            {
                if !ephemeral && Path::new("/dev/fuse").exists() {
                    let mnt = selfdb::vm::make_tmp_dir("vm-fuse-")?;
                    let session = selfdb::fuse::fuse_impl::mount_vm_background(&db, &mnt.to_string_lossy());
                    if let Ok(session) = session {
                        let code = selfdb::sandbox::run_interactive(&spec_for(&mnt), isolation);
                        drop(session);
                        let _ = std::fs::remove_dir(&mnt);
                        std::process::exit(code?);
                    }
                    let _ = std::fs::remove_dir(&mnt);
                    eprintln!("FUSE mount failed; using a materialized tree");
                }
            }
            // A persistent session reuses a stable scratch directory per database: materialize only rewrites
            // files whose size or mtime differ and prunes entries the database no longer has, so a repeat
            // run costs a metadata walk. An ephemeral one gets its own directory, removed afterwards.
            let tmp = if ephemeral { selfdb::vm::make_tmp_dir("vm-chroot-")? } else {
                let d = selfdb::vm::cache_dir().join("scratch").join(selfdb::vm::path_key(&db));
                std::fs::create_dir_all(&d)?;
                d
            };
            selfdb::vm::vm_materialize_tree(&c, &tmp)?;
            let code = selfdb::sandbox::run_interactive(&spec_for(&tmp), isolation);
            if ephemeral {
                let _ = std::fs::remove_dir_all(&tmp);
            } else {
                let (cr, up, del) = selfdb::vm::vm_sync_from_host(&c, &tmp)?;
                if cr + up + del > 0 { eprintln!("-> sync {} -> {}: +{} ~{} -{}", tmp.display(), db, cr, up, del); }
            }
            std::process::exit(code?);
        },
        Cmd::VmResolve{db, vm_path} => {
            let c=selfdb::vm::vm_open(&db)?;
            match selfdb::vm::vm_resolve(&c, &vm_path) {
                Ok(r) => {
                    println!("resolve {} -> {} (kind={} mode={:o} size={})", vm_path, r.path, r.kind, r.mode, r.content.as_ref().map(|v| v.len()).unwrap_or(0));
                },
                Err(e) => { eprintln!("vm-resolve {}: {}", vm_path, e); std::process::exit(1); }
            }
        },
        Cmd::VmCheckpoint{db, name, note} => { let c=selfdb::vm::vm_open(&db)?; selfdb::vm::vm_checkpoint(&c, &name, &note)?; println!("checkpoint {} @ {}", name, db); },
        Cmd::VmSnapshots{db} => { let c=selfdb::vm::vm_open(&db)?; for s in selfdb::vm::vm_list_snapshots(&c)? { println!("[{}] {} ts={} pc={} bytes={} note={}", s.id, s.name, s.created_at, s.page_count, s.bytes, s.note);} },
        Cmd::VmRechunk{db} => { let c=selfdb::vm::vm_open(&db)?; let (done,skipped,failed)=selfdb::vm::vm_rechunk(&c)?; println!("rechunk {}: rechunked={} already_chunked={} failed={}", db, done, skipped, failed); if failed>0 { anyhow::bail!("{} files failed to rechunk", failed); } },
        Cmd::VmVerify{db, deep} => { let c=selfdb::vm::vm_open(&db)?; println!("{}", selfdb::vm::vm_verify(&c, deep)?); },
        Cmd::VmExtract{db, vm_path, out} => { let c=selfdb::vm::vm_open(&db)?; let data=selfdb::vm::vm_cat(&c, &vm_path)?; std::fs::write(&out, &data)?; println!("extract {} -> {} ({} bytes)", vm_path, out, data.len()); },
        Cmd::VmMemInsert{db, addr, size, prot, file} => { let c=selfdb::vm::vm_open(&db)?; let a=i64::from_str_radix(addr.trim_start_matches("0x"), 16).unwrap_or(addr.parse().unwrap_or(0)); let sz:i64=size.parse().unwrap_or(0); let pr:i64=prot.parse().unwrap_or(7); let data=std::fs::read(&file).unwrap_or_default(); selfdb::vm::vm_mem_insert(&c, a, sz, pr, &data)?; println!("mem insert addr=0x{:x} size={} prot={} bytes={} -> {}", a, sz, pr, data.len(), db); },
        Cmd::VmMemList{db} => { let c=selfdb::vm::vm_open(&db)?; for (id,addr,size,prot) in selfdb::vm::vm_mem_list(&c)? { println!("[{}] addr=0x{:x} size={} prot={}", id, addr, size, prot);} },
        Cmd::VmMemClear{db} => { let c=selfdb::vm::vm_open(&db)?; selfdb::vm::vm_mem_clear(&c)?; println!("mem clear -> {}", db); },
        Cmd::VmSnapshotFile{db, name} => { let c=selfdb::vm::vm_open(&db)?; let p=selfdb::vm::vm_snapshot_file(&c, &db, &name)?; println!("snapshot-file {} -> {}", db, p); },
        Cmd::VmRestoreFile{db, name} => { selfdb::vm::vm_restore_file(&db, &name)?; println!("restore-file {} @ {}", name, db); },
        Cmd::VmSync{db, host_root, full} => { let c=selfdb::vm::vm_open(&db)?; let rep = if full { selfdb::vm::vm_sync_full(&c, std::path::Path::new(&host_root))? } else { selfdb::vm::vm_sync_report(&c, std::path::Path::new(&host_root))? }; let (cr,up,del)=(rep.created, rep.updated, rep.deleted); let _ = selfdb::vm::vm_apply_pragmas(&c); println!("sync {} <- {}: +{} ~{} -{} (WAL+NORMAL+mmap)", db, host_root, cr, up, del); },
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
            let mut st=c.prepare("SELECT id, length(dict), samples, created_at, (SELECT count(*) FROM vm_blobs WHERE dict_id=vm_dicts.id) FROM vm_dicts ORDER BY id")?;
            let mut any=false;
            for r in st.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?, r.get::<_,i64>(4)?)))? {
                let (id,len,samples,ts,blobs)=r?; any=true;
                println!("dict id={} bytes={} samples={} created_at={} blocks_using={}", id, len, samples, ts, blobs);
            }
            if !any { println!("no dictionary trained"); }
        },
        Cmd::VmSnap{db, name, note} => {
            let c=selfdb::vm::vm_open(&db)?;
            let t=std::time::Instant::now();
            let id=selfdb::vm::snapshot_create(&c, &name, &note)?;
            println!("snap {} (id {}) in {} ms", name, id, t.elapsed().as_millis());
        },
        Cmd::VmRollback{db, name} => {
            let c=selfdb::vm::vm_open(&db)?;
            selfdb::vm::snapshot_restore(&c, &name)?;
            println!("rolled back to {}", name);
        },
        Cmd::VmAgent{db, timeout, rollback_on_fail, model_base, key_env, model, port, max_output, sandbox, base, fuse, allow_hosts, task} => {
            let opts = selfdb::agent::AgentOptions {
                rollback_on_fail, timeout_secs: timeout, max_output_bytes: max_output,
                model_base, key_env, model, forward_port: port, task,
                limits: sandbox.limits(selfdb::agent::agent_limits()),
                isolation: sandbox.isolation()?,
                base: base.map(std::path::PathBuf::from),
                fuse,
                allow_hosts: allow_hosts.split(',').map(|h| h.trim().to_string()).filter(|h| !h.is_empty()).collect(),
            };
            let r = if fuse {
                run_fused(std::path::Path::new(&db), &opts)?
            } else if opts.base.is_some() {
                selfdb::agent::run_agent_on_overlay(std::path::Path::new(&db), &opts)?
            } else {
                selfdb::agent::run_agent(std::path::Path::new(&db), &opts)?
            };
            use std::io::Write;
            std::io::stdout().write_all(&r.stdout)?;
            std::io::stderr().write_all(&r.stderr)?;
            if r.stdout_truncated || r.stderr_truncated { eprintln!("output truncated at {} bytes", max_output); }
            eprintln!("agent step {}: exit={} timed_out={} rolled_back={} created={} modified={} deleted={} ({} ms)",
                r.step_id, r.exit_code, r.timed_out, r.rolled_back,
                r.changes.created.len(), r.changes.modified.len(), r.changes.deleted.len(), r.duration_ms);
            std::process::exit(r.exit_code);
        }
        Cmd::VmProxy{base, key_env, socket, port, forwarder, allow_hosts} => {
            let key = std::env::var(&key_env).unwrap_or_default();
            if key.is_empty() { eprintln!("{} is not set (or empty)", key_env); std::process::exit(2); }
            let (up, prefix) = selfdb::proxy::Upstream::from_base_url(&base, &key)?;
            if let Some(f) = forwarder {
                std::fs::write(&f, selfdb::proxy::FORWARDER_JS)?;
                eprintln!("forwarder written to {}", f);
            }
            eprintln!("proxy: socket={} -> {} (key from {})", socket, base, key_env);
            eprintln!("guest: bind {} into the sandbox, run the forwarder on 127.0.0.1:{}, point the agent at http://127.0.0.1:{}",
                socket, port, port);
            // Wait for a signal instead of stdin: with stdin redirected, a read returns at once and the
            // proxy would stop immediately, removing the socket before any guest could use it.
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t); }
            }
            STOP_FLAG.with(|f| *f.borrow_mut() = Some(stop.clone()));
            let allow = selfdb::proxy::AllowList::parse(&allow_hosts);
            if !allow.hosts.is_empty() { eprintln!("package hosts allowed: {:?}", allow.hosts); }
            selfdb::proxy::serve_until_stopped_with(std::path::Path::new(&socket), up, &prefix, allow, stop)?;
            eprintln!("proxy stopped");
        }
        Cmd::VmProvisionAgent{db, node, pi, tools} => {
            let tool_set = selfdb::agent::ToolSet::parse(&tools)?;
            let node = node.map(std::path::PathBuf::from).unwrap_or_else(|| {
                let out = std::process::Command::new("sh").args(["-c", "readlink -f \"$(command -v node)\" && true"]).output().ok();
                let s = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
                std::path::PathBuf::from(s)
            });
            let pi = pi.map(std::path::PathBuf::from).unwrap_or_else(default_pi_dir);
            if node.as_os_str().is_empty() || !node.exists() { eprintln!("node not found; pass --node <path>"); std::process::exit(2); }
            if !pi.exists() { eprintln!("pi not found at {}; pass --pi <dir>", pi.display()); std::process::exit(2); }
            let r = selfdb::agent::provision_agent_with(std::path::Path::new(&db), &node, &pi, tool_set)?;
            println!("provisioned {}: node {} ({} objects), pi at /opt/pi ({} entries), tool data ({} entries)", db, r.node, r.objects, r.files, r.tool_files);
            println!("run it with: vm-agent {} -- \"<task>\"", db);
        }
        Cmd::VmInject{target, package} => {
            let c = selfdb::vm::vm_open(&target)?;
            let r = selfdb::inject::inject(&c, std::path::Path::new(&package))?;
            println!("injected {} into {}: {} paths ({} replaced), {} blobs, {} dictionaries",
                package, target, r.paths, r.replaced, r.blobs_added, r.dicts_added);
        }
        Cmd::VmPolicy{db, action} => {
            use selfdb::policy::{self, Rule};
            let c = selfdb::vm::vm_open(&db)?;
            match action {
                PolicyAction::List => {
                    let rules = policy::list(&c)?;
                    if rules.is_empty() { println!("no policies"); }
                    for p in rules {
                        match p.rule {
                            Rule::ReadonlyPrefix(x) => println!("{:>4}  readonly_prefix  {}", p.id, x),
                            Rule::MaxFileSize(n) => println!("{:>4}  max_file_size    {} bytes", p.id, n),
                            Rule::DiskQuota(n) => println!("{:>4}  disk_quota       {} bytes", p.id, n),
                        }
                    }
                }
                PolicyAction::AddReadonly { prefix } => {
                    let id = policy::add(&c, &Rule::ReadonlyPrefix(prefix.clone()))?;
                    println!("policy {}: readonly {}", id, selfdb::vm::normalize_vm_path(&prefix));
                }
                PolicyAction::AddMaxSize { bytes } => {
                    let id = policy::add(&c, &Rule::MaxFileSize(bytes))?;
                    println!("policy {}: max_file_size {} bytes", id, bytes);
                }
                PolicyAction::AddDiskQuota { bytes } => {
                    let id = policy::add(&c, &Rule::DiskQuota(bytes))?;
                    println!("policy {}: disk_quota {} bytes", id, bytes);
                }
                PolicyAction::Remove { id } => {
                    if policy::remove(&c, id)? { println!("removed policy {}", id); }
                    else { eprintln!("no policy {}", id); std::process::exit(1); }
                }
            }
        }
        Cmd::AgentLog{db, json, last} => {
            let rows = selfdb::agent::list_steps(std::path::Path::new(&db), last)?;
            if json {
                let items: Vec<String> = rows.iter().map(selfdb::agent::step_to_json).collect();
                println!("[{}]", items.join(","));
            } else {
                println!("{:>5}  {:>4}  {:<3}  {:<3}  {:>7}  cmd", "step", "exit", "to", "rb", "ms");
                for r in &rows {
                    println!("{:>5}  {:>4}  {:<3}  {:<3}  {:>7}  {}", r.step_id, r.exit_code,
                        if r.timed_out { "yes" } else { "no" }, if r.rolled_back { "yes" } else { "no" },
                        r.duration_ms, r.cmd);
                }
            }
        }
        Cmd::VmStep{db, timeout, rollback_on_fail, max_output, sandbox, argv} => {
            let opts = selfdb::agent::StepOptions{ rollback_on_fail, timeout_secs: timeout, max_output_bytes: max_output,
                limits: sandbox.limits(selfdb::sandbox::Limits::default()), isolation: sandbox.isolation()? };
            let r = selfdb::agent::run_step(std::path::Path::new(&db), &argv, &opts)?;
            use std::io::Write;
            std::io::stdout().write_all(&r.stdout)?;
            std::io::stderr().write_all(&r.stderr)?;
            if r.stdout_truncated || r.stderr_truncated {
                eprintln!("output truncated at {} bytes (stdout={} stderr={})", max_output, r.stdout_truncated, r.stderr_truncated);
            }
            eprintln!("step {}: exit={} timed_out={} rolled_back={} created={} modified={} deleted={} ({} ms)",
                r.step_id, r.exit_code, r.timed_out, r.rolled_back,
                r.changes.created.len(), r.changes.modified.len(), r.changes.deleted.len(), r.duration_ms);
            std::process::exit(r.exit_code);
        }
        Cmd::VmSnapDelete{db, name} => {
            let c=selfdb::vm::vm_open(&db)?;
            selfdb::vm::snapshot_delete(&c, &name)?;
            println!("deleted snapshot {}", name);
        },
        Cmd::VmRetain{db, keep} => {
            let c=selfdb::vm::vm_open(&db)?;
            let removed=selfdb::vm::snapshot_retain(&c, keep)?;
            for n in &removed { println!("removed {}", n); }
            println!("kept newest {}; removed {}", keep, removed.len());
        },
        Cmd::VmSnapDiff{db, name} => {
            let c=selfdb::vm::vm_open(&db)?;
            let d=selfdb::vm::snapshot_diff_live(&c, &name)?;
            for p in &d.created { println!("A {}", p); }
            for p in &d.modified { println!("M {}", p); }
            for p in &d.deleted { println!("D {}", p); }
            println!("created={} modified={} deleted={}", d.created.len(), d.modified.len(), d.deleted.len());
        },
        Cmd::VmDiff{db, a, b} => {
            let c=selfdb::vm::vm_open(&db)?;
            println!("{}", selfdb::vm::vm_diff(&c, &a, &b)?);
        },
        #[cfg(feature = "fuse")]
        Cmd::VmMount{db, mountpoint, allow_other} => { selfdb::fuse::fuse_impl::mount_vm(&db, &mountpoint, allow_other)?; },
        #[cfg(not(feature = "fuse"))]
        Cmd::VmMount{..} => anyhow::bail!("rebuild with --features fuse"),
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
        Cmd::VmRun{db, mem, raw, kvm, kernel, initrd, append} => {
            let c=selfdb::vm::vm_open(&db)?;
            let info=selfdb::vm::vm_disk_info(&c)?;
            println!("{}", info);
            let disk_size=selfdb::vm::vm_disk_size(&c).unwrap_or(0);
            let raw_path = if raw.is_empty() {
                let p = std::env::temp_dir().join(format!("self-vm-disk-{}.raw", std::process::id())).to_string_lossy().to_string();
                selfdb::vm::vm_disk_export_raw(&c, &p)?;
                println!("exported disk -> {} ({} bytes)", p, disk_size);
                p
            } else {
                raw.to_string()
            };
            let qemu = std::env::var("QEMU").unwrap_or_else(|_| "qemu-system-x86_64".to_string());
            // direct kernel boot path (no bootloader needed) via -kernel/-initrd/-append
            // If kernel empty but disk has no bootable MBR (e.g. /tmp/test_disk.raw ext2 only), auto-suggest direct boot.
            // Also treat direct vm DB disk (no --raw) as .raw-like for probing: check the exported raw_path.
            // direct kernel boot: -kernel/-initrd/-append (rootfs = virtio disk, root=/dev/vda)
            let mut boot_args: Vec<String> = Vec::new();
            if !kernel.is_empty() {
                boot_args.push("-kernel".into()); boot_args.push(kernel.clone());
                let no_initrd = matches!(initrd.as_str(), "" | "none" | "no" | "0");
                if !no_initrd { boot_args.push("-initrd".into()); boot_args.push(initrd.clone()); }
                let app = if append.is_empty() { "console=ttyS0 root=/dev/vda rw panic=1".to_string() } else { append.clone() };
                boot_args.push("-append".into()); boot_args.push(app);
            }
            let mut args: Vec<String> = vec!["-m".to_string(), mem.clone(), "-drive".to_string(), format!("file={},format=raw,if=virtio", raw_path), "-serial".to_string(), "mon:stdio".to_string(), "-nographic".to_string()];
            args.extend(boot_args);
            if kvm && std::path::Path::new("/dev/kvm").exists() { args.push("-enable-kvm".to_string()); args.push("-cpu".to_string()); args.push("host".to_string()); }
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
        if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(rp) {
            let son=selfdb::elf::soname_for_path_cached(slot.key(), &mut meta_cache);
            conn.execute("INSERT INTO objects VALUES (?1,?2,?3,?4,?5)", rusqlite::params![next_id, slot.key().to_string_lossy().to_string(), son, "exe", 1])?;
            slot.insert(next_id); next_id+=1;
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

#[derive(clap::Subcommand, Debug)]
enum PolicyAction {
    /// List the rules in force.
    List,
    /// Refuse writes under a path (content and metadata, including raw SQL).
    AddReadonly { prefix: String },
    /// Refuse writes of regular files larger than N bytes.
    AddMaxSize { bytes: u64 },
    /// Refuse writes that would push the VM's total file bytes past N.
    AddDiskQuota { bytes: u64 },
    /// Remove a rule by id (see list).
    Remove { id: i64 },
}

/// Dispatch to the FUSE path when the binary was built with it, with a clear error otherwise.
fn run_fused(db: &std::path::Path, opts: &selfdb::agent::AgentOptions) -> anyhow::Result<selfdb::agent::StepResult> {
    #[cfg(feature = "fuse")] { selfdb::agent::run_agent_fused(db, opts)}
    #[cfg(not(feature = "fuse"))] {
        let _ = (db, opts);
        anyhow::bail!("--fuse needs a binary built with --features fuse")
    }
}

/// The pi package directory: alongside the node binary when installed globally, else the npm default.
fn default_pi_dir() -> std::path::PathBuf {
    let candidates = [
        "/home/linuxbrew/.linuxbrew/lib/node_modules/@earendil-works/pi-coding-agent",
        "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent",
        "/usr/lib/node_modules/@earendil-works/pi-coding-agent",
    ];
    for c in candidates { let p = std::path::PathBuf::from(c); if p.exists() { return p; } }
    std::path::PathBuf::from(candidates[0])
}

thread_local! {
    static STOP_FLAG: std::cell::RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> =
        const { std::cell::RefCell::new(None) };
}

/// Signal handler: flip the stop flag so the proxy loop exits and removes its socket.
extern "C" fn on_signal(_sig: libc::c_int) {
    STOP_FLAG.with(|f| { if let Some(flag) = f.borrow().as_ref() { flag.store(true, std::sync::atomic::Ordering::SeqCst); } });
}
