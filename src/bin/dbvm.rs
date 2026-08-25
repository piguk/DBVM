use clap::{Args, Parser, Subcommand};
use rusqlite::Connection;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};

const APP_ID: u32 = 0x53454C46;

#[derive(Parser)]
#[command(
    name = "dbvm",
    about = "a SQLite database is the whole system",
    long_about = "dbvm runs a Linux userland kept entirely inside one SQLite file.\n\
                  With no subcommand it opens a shell in the default instance,\n\
                  provisioning it from Alpine latest-stable on first use.",
    version
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Args, Clone)]
struct Global {
    /// Instance to operate on [env: DBVM_DB] [default: ~/.local/share/dbvm/default.db]
    #[arg(long, short = 'd', global = true, value_name = "PATH")]
    db: Option<PathBuf>,
    /// Alpine arch for provisioning [default: host CPU]
    #[arg(long, global = true, value_name = "ARCH")]
    arch: Option<String>,
    /// Report the scratch dir, sandbox backend and sync counts
    #[arg(long, short = 'v', global = true)]
    verbose: bool,
}

impl Global {
    fn db_path(&self) -> PathBuf {
        self.db
            .clone()
            .unwrap_or_else(dbvm::instance::default_db_path)
    }
    fn arch(&self) -> Option<&str> {
        self.arch.as_deref()
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command inside the instance (default: an interactive shell)
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Run one binary without a namespace: no privileges, no rootfs visibility
    Exec {
        vm_path: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// Provision the instance from Alpine latest-stable
    Init {
        /// Re-provision even if the instance already exists
        #[arg(long)]
        force: bool,
    },
    /// Roll back to the state right after the rootfs import
    Reset {
        /// Discard the instance and re-provision from latest-stable
        #[arg(long)]
        hard: bool,
    },
    /// Show instance path, size and Alpine version
    Status,

    /// List a directory
    Ls {
        #[arg(default_value = "/")]
        path: String,
    },
    /// Write a file to stdout
    Cat { path: String },
    /// Show kind, mode, size and hash of a path
    Stat { path: String },
    /// Copy a host file into the instance
    Cp { host: String, vm_path: String },
    /// Write a file from the instance to the host
    Extract { vm_path: String, out: String },
    /// Unpack the whole filesystem onto the host
    Materialize { dest: String },
    /// Import a rootfs tarball
    ImportRootfs {
        tar: String,
        #[arg(long, default_value = "")]
        strip: String,
    },
    /// Import an ELF binary together with its shared-library closure
    Import {
        elf: String,
        #[arg(long, default_value = "/")]
        prefix: String,
    },
    /// Import a host directory tree
    Pack {
        host_dir: String,
        #[arg(long, default_value = "/")]
        prefix: String,
    },
    /// Sync host changes back into the instance
    Sync { host_root: String },

    /// Take a named snapshot
    Snapshot {
        name: String,
        #[arg(long, default_value = "")]
        note: String,
        /// Also copy the whole database to <db>.snap.<name>
        #[arg(long)]
        file: bool,
    },
    /// List snapshots
    Snapshots,
    /// Restore a file snapshot taken with `snapshot --file`
    Restore { name: String },

    /// Check integrity and report size
    Verify,
    /// Reclaim space
    Gc,
    /// Report compression ratio and page layout
    Compress,
    /// Inspect the memory image
    #[command(subcommand)]
    Mem(MemCmd),

    /// Inspect and pack SELF files
    #[command(subcommand)]
    Self_(SelfCmd),
}

#[derive(Subcommand)]
enum MemCmd {
    /// Insert a page
    Insert {
        addr: String,
        size: String,
        prot: String,
        file: String,
    },
    /// List pages
    List,
    /// Drop all pages
    Clear,
}

#[derive(Subcommand)]
enum SelfCmd {
    /// Identify a .self file
    File { path: String },
    /// List needed libraries
    Ldd { path: String },
    /// List exported symbols
    Exports { path: String },
    /// List imported symbols
    Imports { path: String },
    /// List segments
    Segments { path: String },
    /// Show metadata
    Meta { path: String },
    /// Build a shared-library closure database
    Closure { path: String, output: String },
    /// Scan a directory into a closure database
    Scan { db: String, dir: String },
    /// Scan several directories into one database
    Userland { output: String, dirs: Vec<String> },
    /// List bundled objects
    Bundle {
        path: String,
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Summarise a bundle
    BundleInfo { path: String },
    /// Convert an ELF binary into a .self database
    Pack {
        input: String,
        #[arg(short, long, default_value = "a.self")]
        output: String,
        #[arg(long)]
        no_bundle: bool,
        #[arg(long)]
        no_sections: bool,
        #[arg(long)]
        no_notes: bool,
    },
    /// Execute a .self file
    Run {
        path: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
}

fn open_db(path: &str) -> Connection {
    let conn = Connection::open(path).unwrap();
    let appid: u32 = conn
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .unwrap_or(0);
    if appid != APP_ID {
        eprintln!("not a SELF file: {}", path);
        std::process::exit(1);
    }
    conn
}

/// Open the instance for writing, laying down the schema when the file is new. Lets
/// `dbvm --db fresh.db cp ./hello /hello` build an instance without an Alpine rootfs.
fn open_for_write(db: &str) -> anyhow::Result<Connection> {
    let has_schema = Connection::open(db)
        .and_then(|c| {
            c.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='vm_fs'",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .map(|n| n > 0)
        .unwrap_or(false);
    if has_schema {
        return dbvm::vm::vm_open(db);
    }
    if let Some(parent) = Path::new(db).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    dbvm::vm::init_vm_db(db, true)
}

/// Run `argv` with output discarded and report whether it succeeded.
fn probe(argv: &[&str]) -> bool {
    let Some((prog, args)) = argv.split_first() else {
        return false;
    };
    std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Locate the self-exec loader: $SELF_EXEC, then next to this binary, then $PATH.
fn self_exec_path() -> String {
    if let Ok(p) = std::env::var("SELF_EXEC")
        && !p.is_empty()
    {
        return p;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let cand = dir.join("self-exec");
        if cand.exists() {
            return cand.to_string_lossy().to_string();
        }
    }
    "self-exec".to_string()
}

fn status(db: &Path) -> anyhow::Result<()> {
    println!("instance : {}", db.display());
    if !dbvm::instance::is_populated(db) {
        println!("state    : not provisioned");
        println!("           run `dbvm` or `dbvm init` to fetch Alpine latest-stable");
        return Ok(());
    }
    let c = dbvm::vm::vm_open(&db.to_string_lossy())?;
    let entries: i64 = c.query_row("SELECT count(*) FROM vm_fs", [], |r| r.get(0))?;
    let bytes = std::fs::metadata(db).map(|m| m.len()).unwrap_or(0);
    println!("size     : {} bytes", bytes);
    println!("entries  : {}", entries);
    if let Ok(v) = dbvm::vm::vm_cat(&c, "/etc/alpine-release") {
        println!("alpine   : {}", String::from_utf8_lossy(&v).trim());
    }
    println!(
        "base     : {}",
        if dbvm::instance::has_base_snapshot(db) {
            "present (dbvm reset)"
        } else {
            "missing (dbvm reset --hard)"
        }
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let g = cli.global.clone();
    let db_path = g.db_path();
    let db = db_path.to_string_lossy().to_string();

    // Bare `dbvm` opens a shell in the default instance.
    let Some(cmd) = cli.cmd else {
        let c = dbvm::instance::open_or_provision(&db_path, g.arch())?;
        std::process::exit(run_in_instance(&c, &db, &[], g.verbose)?);
    };

    match cmd {
        Cmd::Run { argv } => {
            let c = dbvm::instance::open_or_provision(&db_path, g.arch())?;
            std::process::exit(run_in_instance(&c, &db, &argv, g.verbose)?);
        }
        Cmd::Exec { vm_path, rest } => {
            let c = dbvm::instance::open_or_provision(&db_path, g.arch())?;
            std::process::exit(exec_binary(&c, &vm_path, &rest)?);
        }
        Cmd::Init { force } => {
            if !force && dbvm::instance::is_populated(&db_path) {
                println!(
                    "instance already at {} (--force to re-provision)",
                    db_path.display()
                );
                return Ok(());
            }
            let r = dbvm::instance::provision(&db_path, g.arch())?;
            println!("alpine {} ({}) -> {}", r.version, r.arch, db_path.display());
        }
        Cmd::Reset { hard } => {
            if hard {
                let r = dbvm::instance::reset_hard(&db_path, g.arch())?;
                println!(
                    "re-provisioned alpine {} -> {}",
                    r.version,
                    db_path.display()
                );
            } else {
                dbvm::instance::reset(&db_path)?;
                println!("rolled back to base -> {}", db_path.display());
            }
        }
        Cmd::Status => status(&db_path)?,

        Cmd::Ls { path } => {
            let c = dbvm::instance::open_or_provision(&db_path, g.arch())?;
            for (p, k, m, sz, _) in dbvm::vm::vm_ls(&c, &path)? {
                println!("{:8} {:4} {:>10}  {}", k, format!("{:o}", m), sz, p);
            }
        }
        Cmd::Cat { path } => {
            let c = dbvm::instance::open_or_provision(&db_path, g.arch())?;
            let data = dbvm::vm::vm_cat(&c, &path)?;
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }
        Cmd::Stat { path } => {
            let c = dbvm::instance::open_or_provision(&db_path, g.arch())?;
            let (p, k, mode, size, mtime, hash) = dbvm::vm::vm_stat(&c, &path)?;
            println!(
                "{}\nkind={} mode={:o} size={} mtime={} hash={}",
                p, k, mode, size, mtime, hash
            );
        }
        Cmd::Cp { host, vm_path } => {
            let c = open_for_write(&db)?;
            dbvm::vm::vm_add_file(&c, &host, &vm_path)?;
            println!("{} -> {}:{}", host, db, vm_path);
        }
        Cmd::Extract { vm_path, out } => {
            let c = dbvm::instance::open_or_provision(&db_path, g.arch())?;
            let data = dbvm::vm::vm_cat(&c, &vm_path)?;
            std::fs::write(&out, &data)?;
            println!("{} -> {} ({} bytes)", vm_path, out, data.len());
        }
        Cmd::Materialize { dest } => {
            let c = dbvm::instance::open_or_provision(&db_path, g.arch())?;
            let n = dbvm::vm::vm_materialize_tree(&c, Path::new(&dest))?;
            println!("{} -> {} ({} files)", db, dest, n);
        }
        Cmd::ImportRootfs { tar, strip } => {
            let c = open_for_write(&db)?;
            let n = dbvm::vm::vm_import_tar(&c, &tar, &strip)?;
            println!("{} -> {} ({} entries)", tar, db, n);
        }
        Cmd::Import { elf, prefix } => {
            let c = open_for_write(&db)?;
            let n = dbvm::vm::vm_import_closure(&c, &elf, &prefix)?;
            println!("{} -> {} ({} objects)", elf, db, n);
        }
        Cmd::Pack { host_dir, prefix } => {
            let c = open_for_write(&db)?;
            let n = dbvm::vm::vm_pack_host_dir(&c, &host_dir, &prefix)?;
            println!("{} -> {}:{} ({} files)", host_dir, db, prefix, n);
        }
        Cmd::Sync { host_root } => {
            let c = dbvm::vm::vm_open(&db)?;
            let (cr, up, del) = dbvm::vm::vm_sync_from_host(&c, Path::new(&host_root))?;
            let _ = dbvm::vm::vm_apply_pragmas(&c);
            println!("{} <- {}: +{} ~{} -{}", db, host_root, cr, up, del);
        }

        Cmd::Snapshot { name, note, file } => {
            let c = dbvm::vm::vm_open(&db)?;
            dbvm::vm::vm_checkpoint(&c, &name, &note)?;
            if file {
                let p = dbvm::vm::vm_snapshot_file(&c, &db, &name)?;
                println!("snapshot {} -> {}", name, p);
            } else {
                println!("snapshot {} @ {}", name, db);
            }
        }
        Cmd::Snapshots => {
            let c = Connection::open(&db)?;
            for (id, name, ts, pc, bytes, note) in dbvm::vm::vm_list_snapshots(&c)? {
                println!(
                    "[{}] {} ts={} pc={} bytes={} note={}",
                    id, name, ts, pc, bytes, note
                );
            }
        }
        Cmd::Restore { name } => {
            dbvm::vm::vm_restore_file(&db, &name)?;
            println!("restored {} @ {}", name, db);
        }

        Cmd::Verify => {
            let c = Connection::open(&db)?;
            println!("{}", dbvm::vm::vm_verify(&c)?);
        }
        Cmd::Gc => {
            let c = dbvm::vm::vm_open(&db)?;
            let (before, saved) = dbvm::vm::vm_gc(&c)?;
            println!("gc {}: pages {} -> saved {} bytes", db, before, saved);
        }
        Cmd::Compress => {
            let c = dbvm::vm::vm_open(&db)?;
            let total_files: i64 =
                c.query_row("SELECT count(*) FROM vm_fs WHERE kind='file'", [], |r| {
                    r.get(0)
                })?;
            let comp_files: i64 =
                c.query_row("SELECT count(*) FROM vm_fs WHERE compressed=1", [], |r| {
                    r.get(0)
                })?;
            let raw_sum: Option<i64> =
                c.query_row("SELECT sum(size) FROM vm_fs WHERE kind='file'", [], |r| {
                    r.get(0)
                })?;
            let blob_sum: Option<i64> = c.query_row(
                "SELECT sum(length(content)) FROM vm_fs WHERE kind='file'",
                [],
                |r| r.get(0),
            )?;
            let page_size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let page_count: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let journal: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let mmap: i64 = c.query_row("PRAGMA mmap_size", [], |r| r.get(0))?;
            println!("compress: {}/{} files compressed", comp_files, total_files);
            println!("  logical size: {} bytes", raw_sum.unwrap_or(0));
            println!(
                "  blob storage: {} bytes (ratio {:.1}%)",
                blob_sum.unwrap_or(0),
                100.0 * (blob_sum.unwrap_or(0) as f64) / (raw_sum.unwrap_or(1) as f64)
            );
            println!(
                "  db file: {} bytes (page_size={} count={} journal={} mmap={})",
                page_size * page_count,
                page_size,
                page_count,
                journal,
                mmap
            );
        }
        Cmd::Mem(m) => match m {
            MemCmd::Insert {
                addr,
                size,
                prot,
                file,
            } => {
                let c = Connection::open(&db)?;
                let a = i64::from_str_radix(addr.trim_start_matches("0x"), 16)
                    .unwrap_or(addr.parse().unwrap_or(0));
                let sz: i64 = size.parse().unwrap_or(0);
                let pr: i64 = prot.parse().unwrap_or(7);
                let data = std::fs::read(&file).unwrap_or_default();
                dbvm::vm::vm_mem_insert(&c, a, sz, pr, &data)?;
                println!(
                    "mem insert addr=0x{:x} size={} prot={} bytes={} -> {}",
                    a,
                    sz,
                    pr,
                    data.len(),
                    db
                );
            }
            MemCmd::List => {
                let c = Connection::open(&db)?;
                for (id, addr, size, prot) in dbvm::vm::vm_mem_list(&c)? {
                    println!("[{}] addr=0x{:x} size={} prot={}", id, addr, size, prot);
                }
            }
            MemCmd::Clear => {
                let c = Connection::open(&db)?;
                dbvm::vm::vm_mem_clear(&c)?;
                println!("mem clear -> {}", db);
            }
        },

        Cmd::Self_(s) => run_self(s)?,
    }
    Ok(())
}

fn run_self(cmd: SelfCmd) -> anyhow::Result<()> {
    match cmd {
        SelfCmd::File { path } => {
            let mut f = std::fs::File::open(&path)?;
            use std::io::{Read, Seek, SeekFrom};
            let mut head = [0u8; 16];
            f.read_exact(&mut head)?;
            f.seek(SeekFrom::Start(64))?;
            let mut appid = [0u8; 8];
            f.read_exact(&mut appid)?;
            let kind = if &appid[4..8] == b"SELF" {
                "SQLite 3.x database, application id 0x53454c46, user version 1"
            } else {
                "SQLite 3.x database"
            };
            println!("{}: {}", path, kind);
            println!(
                "magic : {}",
                head.iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            println!(
                "appid : {}  <- bytes 68..71 == 'SELF'",
                appid
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        SelfCmd::Ldd { path } => {
            let db = open_db(&path);
            let mut st = db.prepare("SELECT ord, soname FROM ldd")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            let mut n = 0;
            for r in rows {
                let (_, s) = r?;
                println!("{}", s);
                n += 1;
            }
            println!("({} libraries)", n);
        }
        SelfCmd::Exports { path } => {
            let db = open_db(&path);
            let mut st =
                db.prepare("SELECT name, version, type, size FROM exports ORDER BY name")?;
            for r in st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })? {
                let (n, v, t, s) = r?;
                println!(
                    "{}\t{}\t{}\t{}",
                    n,
                    v.unwrap_or_default(),
                    t.unwrap_or_default(),
                    s.map(|x| x.to_string()).unwrap_or_default()
                );
            }
        }
        SelfCmd::Imports { path } => {
            let db = open_db(&path);
            let mut st = db.prepare("SELECT name, version FROM imports ORDER BY name")?;
            for r in st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })? {
                let (n, v) = r?;
                println!("{}\t{}", n, v.unwrap_or_default());
            }
        }
        SelfCmd::Segments { path } => {
            let db = open_db(&path);
            let mut st =
                db.prepare("SELECT type, vaddr, filesz, memsz, r, w, x FROM segments ORDER BY id")?;
            for r in st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })? {
                let (t, v, f, m, r, w, x) = r?;
                println!("{}\t0x{:x}\t{}\t{}\t{}{}{}", t, v, f, m, r, w, x);
            }
        }
        SelfCmd::Meta { path } => {
            let db = open_db(&path);
            let mut st = db.prepare("SELECT key, value FROM self_meta ORDER BY rowid")?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (k, v) = r?;
                println!("{} = {}", k, v);
            }
        }
        SelfCmd::Closure { path, output } => build_closure(&path, &output)?,
        SelfCmd::Scan { db, dir } => scan_dir(&db, &dir)?,
        SelfCmd::Userland { output, dirs } => scan_userland(&output, dirs)?,
        SelfCmd::Bundle { path, filter } => bundle_list(&path, &filter)?,
        SelfCmd::BundleInfo { path } => bundle_info(&path)?,
        SelfCmd::Pack {
            input,
            output,
            no_bundle,
            no_sections,
            no_notes,
        } => {
            let info = dbvm::elf::parse_elf(&input, no_sections, no_notes)?;
            dbvm::db::create_self_db(&output, &info, no_sections, no_notes)?;
            let bundled = if no_bundle { "" } else { " +bundle" };
            println!("{} -> {}{}", input, output, bundled);
        }
        SelfCmd::Run { path, rest } => {
            let exe = self_exec_path();
            let mut args = vec![path.clone()];
            args.extend(rest.clone());
            let status = std::process::Command::new(&exe)
                .args(&args)
                .status()
                .unwrap_or_else(|e| {
                    eprintln!("exec failed: {}: {}", exe, e);
                    std::process::exit(127)
                });
            std::process::exit(status.code().unwrap_or(127));
        }
    }
    Ok(())
}

/// Materialize just `vm_path` and its shared-library closure into a scratch dir and
/// run it there. Needs no privileges, but the guest rootfs is not visible.
fn exec_binary(conn: &Connection, vm_path: &str, argv: &[String]) -> anyhow::Result<i32> {
    let c = conn;
    let vm_path = vm_path.to_string();
    let rest = argv.to_vec();
    // mkdtemp under /tmp/dbvm-XXXXXX
    let mut tmpl = b"/tmp/dbvm-XXXXXX\0".to_vec();
    let p = unsafe { libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
    let tmp = if p.is_null() {
        std::env::temp_dir().join(format!("dbvm-exec-{}", std::process::id()))
    } else {
        let s = unsafe { std::ffi::CStr::from_ptr(p) };
        std::path::PathBuf::from(s.to_string_lossy().to_string())
    };
    let host_bin = tmp.join(vm_path.trim_start_matches('/'));
    if let Some(parent) = host_bin.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // resolve symlink via vm_resolve and materialize (symlinks become regular file with target content)
    dbvm::vm::vm_materialize(c, &vm_path, &host_bin)?;
    // materialize needed closure from vm_fs only (no host FS, no DB mutation)
    // also recreate symlinks for libs (e.g. libc.musl -> ld-musl)
    {
        let meta = dbvm::elf::meta_for_path(&host_bin.to_string_lossy());
        let mut to_mat: Vec<String> = Vec::new();
        for soname in meta.needed {
            let cand: Option<String> = c.query_row(
                        "SELECT path FROM vm_fs WHERE (kind='file' OR kind='symlink') AND (path LIKE ?1 OR path LIKE ?2) LIMIT 1",
                        rusqlite::params![format!("%/{}", soname), format!("%{}", soname)],
                        |r| r.get(0)).ok();
            if let Some(p) = cand {
                to_mat.push(p);
            } else {
                let mut st=c.prepare("SELECT path FROM vm_fs WHERE (kind='file' OR kind='symlink') AND (path LIKE '/lib/%' OR path LIKE '/usr/lib/%')")?;
                for pp in st.query_map([], |r| r.get::<_, String>(0))?.flatten() {
                    if pp.ends_with(&soname) {
                        to_mat.push(pp);
                        break;
                    }
                }
            }
        }
        // eagerly materialize libs + their symlink aliases
        let mut all_libs: Vec<String> = Vec::new();
        {
            let mut st=c.prepare("SELECT path, kind, link_target FROM vm_fs WHERE (path LIKE '/lib/%' OR path LIKE '/usr/lib/%' OR path LIKE '/usr/lib64/%')")?;
            for r in st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .flatten()
            {
                let (path, kind, link) = r;
                if kind == "symlink" {
                    let host = tmp.join(path.trim_start_matches('/'));
                    if host.exists() || std::fs::symlink_metadata(&host).is_ok() {
                        continue;
                    }
                    if let Some(par) = host.parent() {
                        std::fs::create_dir_all(par)?;
                    }
                    let target = link.unwrap_or_default();
                    let _ = std::os::unix::fs::symlink(&target, &host);
                } else {
                    all_libs.push(path);
                }
            }
        }
        for lp in all_libs {
            let host = tmp.join(lp.trim_start_matches('/'));
            if host.exists() {
                continue;
            }
            let _ = dbvm::vm::vm_materialize(c, &lp, &host);
        }
        // also ensure NEEDED libs materialized (may be symlink that vm_materialize resolves)
        for lp in to_mat {
            let host = tmp.join(lp.trim_start_matches('/'));
            if host.exists() || std::fs::symlink_metadata(&host).is_ok() {
                continue;
            }
            // if entry is symlink, recreate symlink; vm_materialize will resolve and write file, so handle symlink case
            let kind: Option<String> = c
                .query_row(
                    "SELECT kind FROM vm_fs WHERE path=?1",
                    rusqlite::params![lp.clone()],
                    |r| r.get(0),
                )
                .ok();
            if kind.as_deref() == Some("symlink")
                && let Ok(Some(t)) = c.query_row(
                    "SELECT link_target FROM vm_fs WHERE path=?1",
                    rusqlite::params![lp.clone()],
                    |r| r.get::<_, Option<String>>(0),
                )
            {
                let _ = std::os::unix::fs::symlink(&t, &host);
                continue;
            }
            let _ = dbvm::vm::vm_materialize(c, &lp, &host);
        }
    }
    let env_ld = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    // musl keeps libraries directly in /lib and /usr/lib; glibc distros add a
    // multiarch subdir named after the host arch.
    let multiarch = format!("{}-linux-gnu", std::env::consts::ARCH);
    let vm_ld = [
        tmp.join("lib"),
        tmp.join("usr/lib"),
        tmp.join("usr/lib").join(&multiarch),
        tmp.join("lib").join(&multiarch),
    ]
    .iter()
    .map(|p| p.to_string_lossy().to_string())
    .collect::<Vec<_>>()
    .join(":");
    let merged_ld = if env_ld.is_empty() {
        format!("{}:{}", tmp.to_string_lossy(), vm_ld)
    } else {
        format!("{}:{}:{}", tmp.to_string_lossy(), vm_ld, env_ld)
    };
    let mut args = Vec::new();
    args.extend(rest.clone());
    let exe = std::env::var("SELF_EXEC").unwrap_or_else(|_| "target/release/self-exec".to_string());
    let is_self = {
        let mut f = std::fs::File::open(&host_bin).unwrap();
        use std::io::{Read, Seek, SeekFrom};
        let mut head = [0u8; 16];
        let _ = f.read_exact(&mut head);
        let mut appid = [0u8; 8];
        let _ = f.seek(SeekFrom::Start(64));
        let _ = f.read_exact(&mut appid);
        &appid[4..8] == b"SELF"
    };
    // prefer using the ELF interpreter recorded at build-time when available:
    // musl busybox uses /lib/ld-musl-x86_64.so.1 which we have materialized under tmp.
    // If interpreter exists in tmp, exec it directly; otherwise try native exec (glibc binaries).
    let interp: Option<String> = dbvm::elf::parse_elf(&host_bin.to_string_lossy(), true, true)
        .ok()
        .and_then(|info| info.interp);
    let status = if is_self {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(host_bin.to_string_lossy().to_string());
        cmd.args(&args);
        cmd.env("LD_LIBRARY_PATH", merged_ld);
        cmd.status().unwrap_or_else(|e| {
            eprintln!("exec failed: {}: {}", exe, e);
            std::process::exit(127)
        })
    } else if let Some(ip) = interp.clone() {
        // ip is absolute like /lib/ld-musl-x86_64.so.1; map to tmp/...
        let ip_host = tmp.join(ip.trim_start_matches('/'));
        if ip_host.exists() {
            // musl ld expects: ld-musl <exe> [args]; also pass --library-path if needed
            let mut cmd = std::process::Command::new(&ip_host);
            cmd.arg(&host_bin);
            cmd.args(&args);
            // musl honours LD_LIBRARY_PATH as well; keep merged_ld for any secondary libs
            cmd.env("LD_LIBRARY_PATH", merged_ld);
            cmd.status().unwrap_or_else(|e| {
                eprintln!("exec via interp failed: {}: {}", ip_host.display(), e);
                std::process::exit(127)
            })
        } else {
            let mut cmd = std::process::Command::new(&host_bin);
            cmd.args(&args);
            cmd.env("LD_LIBRARY_PATH", merged_ld);
            cmd.status().unwrap_or_else(|e| {
                eprintln!("exec failed: {}: {}", host_bin.to_string_lossy(), e);
                std::process::exit(127)
            })
        }
    } else {
        let mut cmd = std::process::Command::new(&host_bin);
        cmd.args(&args);
        cmd.env("LD_LIBRARY_PATH", merged_ld);
        cmd.status().unwrap_or_else(|e| {
            eprintln!("exec failed: {}: {}", host_bin.to_string_lossy(), e);
            std::process::exit(127)
        })
    };
    Ok(status.code().unwrap_or(127))
}

/// Materialize the whole filesystem, run `argv` inside it with a real root, then sync
/// changes back into the database. Requires bwrap, unshare or root.
fn run_in_instance(
    conn: &Connection,
    db: &str,
    argv: &[String],
    verbose: bool,
) -> anyhow::Result<i32> {
    let c = conn;
    let (cmd, rest): (String, Vec<String>) = match argv.split_first() {
        Some((first, tail)) => (first.clone(), tail.to_vec()),
        None => ("/bin/sh".to_string(), Vec::new()),
    };
    let mut tmpl = b"/tmp/dbvm-XXXXXX\0".to_vec();
    let pp = unsafe { libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
    let tmp = if pp.is_null() {
        std::env::temp_dir().join(format!("dbvm-run-{}", std::process::id()))
    } else {
        let s = unsafe { std::ffi::CStr::from_ptr(pp) };
        std::path::PathBuf::from(s.to_string_lossy().to_string())
    };
    let n = dbvm::vm::vm_materialize_tree(c, &tmp)?;
    if verbose {
        eprintln!("-> materialized {} files -> {}", n, tmp.display());
    }
    for d in ["proc", "sys", "dev", "tmp"] {
        let _ = std::fs::create_dir_all(tmp.join(d));
    }
    let full_cmd = if cmd.is_empty() {
        "/bin/sh".to_string()
    } else {
        cmd.clone()
    };
    let mut args = vec![full_cmd.clone()];
    args.extend(rest.clone());
    // Probe by actually entering a namespace: both tools exist in containers where the
    // kernel then refuses the unshare, and `--help` cannot tell the two apart.
    let has_bwrap = probe(&["bwrap", "--ro-bind", "/", "/", "/bin/true"]);
    let has_unshare = probe(&["unshare", "--mount", "--map-root-user", "/bin/true"]);
    let is_root = unsafe { libc::geteuid() } == 0;
    if !has_bwrap && !has_unshare && !is_root {
        let _ = std::fs::remove_dir_all(&tmp);
        anyhow::bail!(
            "cannot enter the instance: needs bubblewrap (bwrap), util-linux (unshare) or root.\n\
             install one of them, or run a single binary without a namespace:\n    \
             dbvm exec /bin/busybox -- echo hi"
        );
    }
    let status = if has_bwrap {
        if verbose {
            eprintln!("-> bwrap {}", tmp.display());
        }
        let mut cmd = std::process::Command::new("bwrap");
        cmd.arg("--bind")
            .arg(&tmp)
            .arg("/")
            .arg("--dev")
            .arg("/dev")
            .arg("--proc")
            .arg("/proc")
            .arg("--unshare-pid");
        cmd.arg(full_cmd.clone());
        for a in &rest {
            cmd.arg(a);
        }
        cmd.status().unwrap_or_else(|e| {
            eprintln!("bwrap failed: {}", e);
            std::process::exit(127)
        })
    } else if has_unshare {
        if verbose {
            eprintln!("-> unshare --root {}", tmp.display());
        }
        let mut cmd = std::process::Command::new("unshare");
        cmd.arg("--mount")
            .arg("--map-root-user")
            .arg("--root")
            .arg(&tmp);
        cmd.arg(full_cmd.clone());
        for a in &rest {
            cmd.arg(a);
        }
        cmd.status().unwrap_or_else(|e| {
            eprintln!("unshare failed: {}", e);
            std::process::exit(127)
        })
    } else {
        if verbose {
            eprintln!("-> chroot {}", tmp.display());
        }
        let mut cmd = std::process::Command::new("chroot");
        cmd.arg(&tmp).arg(full_cmd.clone());
        for a in &rest {
            cmd.arg(a);
        }
        cmd.status().unwrap_or_else(|e| {
            eprintln!("chroot failed: {}", e);
            std::process::exit(127)
        })
    };
    // Persist whatever the session changed back into the same .db.
    if let Ok((cr, up, del)) = dbvm::vm::vm_sync_from_host(c, &tmp)
        && cr + up + del > 0
    {
        if verbose {
            eprintln!(
                "-> sync {} -> {}: +{} ~{} -{}",
                tmp.display(),
                db,
                cr,
                up,
                del
            );
        }
        let _ = dbvm::vm::vm_apply_pragmas(c);
    }
    // The scratch tree is a full copy of the rootfs; leaving it behind leaks megabytes
    // per run. `dbvm materialize` is the way to keep one.
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(status.code().unwrap_or(127))
}

fn build_closure(root: &str, out: &str) -> anyhow::Result<()> {
    if Path::new(out).exists() {
        std::fs::remove_file(out)?;
    }
    let conn = Connection::open(out)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-8192;")?;
    conn.execute_batch(r#"CREATE TABLE objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT, is_root INTEGER NOT NULL DEFAULT 0); CREATE TABLE needs(object_id INTEGER NOT NULL REFERENCES objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES objects(path)); CREATE INDEX idx_needs_resolved ON needs(resolved_path);"#)?;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let root_canon = std::fs::canonicalize(root)?;
    let mut meta_cache: FxHashMap<PathBuf, dbvm::elf::ElfMeta> = FxHashMap::default();
    let mut search_cache: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    let mut resolve_cache: FxHashMap<u64, Option<PathBuf>> = FxHashMap::default();
    let ld_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let mut seen: FxHashMap<PathBuf, i64> = FxHashMap::default();
    let mut order: Vec<(i64, PathBuf, Option<String>, String, i64)> = Vec::new();
    let mut next_id: i64 = 1;
    let add = |path: PathBuf,
               soname: Option<String>,
               kind: &str,
               is_root: i64,
               conn: &Connection,
               seen: &mut FxHashMap<PathBuf, i64>,
               order: &mut Vec<(i64, PathBuf, Option<String>, String, i64)>,
               next_id: &mut i64|
     -> i64 {
        let rp = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if let Some(id) = seen.get(&rp) {
            return *id;
        }
        let id = *next_id;
        *next_id += 1;
        seen.insert(rp.clone(), id);
        order.push((id, rp.clone(), soname.clone(), kind.to_string(), is_root));
        conn.execute(
            "INSERT INTO objects VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![id, rp.to_string_lossy().to_string(), soname, kind, is_root],
        )
        .unwrap();
        id
    };
    let _root_id = add(
        root_canon.clone(),
        None,
        "exe",
        1,
        &conn,
        &mut seen,
        &mut order,
        &mut next_id,
    );
    let mut queue = vec![root_canon.clone()];
    let mut qh: usize = 0;
    let mut stmt_needs = conn.prepare_cached("INSERT INTO needs VALUES (?1,?2,?3,?4)")?;
    while qh < queue.len() {
        let cur = queue[qh].clone();
        qh += 1;
        let cur_id = *seen.get(&cur).unwrap();
        let needed = dbvm::elf::meta_for_path_cached(&cur, &mut meta_cache)
            .needed
            .clone();
        if needed.is_empty() {
            continue;
        }
        let sdirs = dbvm::closure::search_dirs_for_cached(
            &cur,
            &[],
            &mut meta_cache,
            &mut search_cache,
            &ld_dirs,
        );
        let search_hash = dbvm::closure::search_dirs_hash(&sdirs);
        for (i, soname) in needed.iter().enumerate() {
            let resolved = dbvm::closure::resolve_soname_cached(
                soname,
                &sdirs,
                &mut resolve_cache,
                search_hash,
            )
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
            let rp_str = resolved.as_ref().map(|p| p.to_string_lossy().to_string());
            if let Some(rp) = resolved.clone()
                && !seen.contains_key(&rp)
            {
                let son = dbvm::elf::soname_for_path_cached(&rp, &mut meta_cache);
                add(
                    rp.clone(),
                    son,
                    "lib",
                    0,
                    &conn,
                    &mut seen,
                    &mut order,
                    &mut next_id,
                );
                queue.push(rp);
            }
            stmt_needs.execute(rusqlite::params![cur_id, i as i64, soname, rp_str])?;
        }
    }
    drop(stmt_needs);
    conn.execute_batch("COMMIT;")?;
    let total: i64 = conn.query_row("SELECT count(*) FROM objects", [], |r| r.get(0))?;
    let needs: i64 = conn.query_row("SELECT count(*) FROM needs", [], |r| r.get(0))?;
    let missing: i64 = conn.query_row(
        "SELECT count(*) FROM needs WHERE resolved_path IS NULL AND soname NOT LIKE 'ld-%'",
        [],
        |r| r.get(0),
    )?;
    println!(
        "{} + closure -> {} ({} objects, {} edges, missing={})",
        Path::new(root).file_name().unwrap().to_string_lossy(),
        out,
        total,
        needs,
        missing
    );
    Ok(())
}

fn scan_dir(db: &str, dir: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-8192; PRAGMA temp_store=MEMORY;");
    let _ = conn.execute_batch("BEGIN IMMEDIATE;");
    conn.execute_batch(r#"CREATE TABLE IF NOT EXISTS objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT, is_root INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS needs(object_id INTEGER NOT NULL REFERENCES objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES objects(path)); CREATE INDEX IF NOT EXISTS idx_needs_resolved ON needs(resolved_path);"#)?;
    let base_dirs = vec![
        PathBuf::from(dir),
        PathBuf::from("/lib/x86_64-linux-gnu"),
        PathBuf::from("/lib"),
        PathBuf::from("/usr/lib/x86_64-linux-gnu"),
        PathBuf::from("/usr/lib"),
    ];
    let ld_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let mut meta_cache: FxHashMap<PathBuf, dbvm::elf::ElfMeta> = FxHashMap::default();
    let mut search_cache: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    let mut resolve_cache: FxHashMap<u64, Option<PathBuf>> = FxHashMap::default();
    let mut exes = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        if let Ok(mut f) = std::fs::File::open(&p) {
            let mut b = [0u8; 4];
            use std::io::Read;
            if f.read(&mut b).is_ok() && &b == b"\x7fELF" {
                if let Ok(canon) = std::fs::canonicalize(&p) {
                    exes.push(canon);
                } else {
                    exes.push(p);
                }
            }
        }
    }
    let mut seen: FxHashMap<PathBuf, i64> = conn
        .prepare("SELECT path, id FROM objects")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .filter_map(|r| r.ok())
        .map(|(p, id)| (PathBuf::from(p), id))
        .collect();
    let mut next_id: i64 = conn
        .query_row("SELECT max(id) FROM objects", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .unwrap_or(None)
        .unwrap_or(0)
        + 1;
    for exe in &exes {
        if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(exe.clone()) {
            let son = dbvm::elf::soname_for_path_cached(slot.key(), &mut meta_cache);
            let path = slot.key().to_string_lossy().to_string();
            conn.execute(
                "INSERT INTO objects VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![next_id, path, son, "exe", 1],
            )?;
            slot.insert(next_id);
            next_id += 1;
        }
    }
    let mut queue: Vec<PathBuf> = seen.keys().cloned().collect();
    queue.sort();
    queue.dedup();
    let mut qh: usize = 0;
    let mut stmt_needs = conn.prepare_cached("INSERT INTO needs VALUES (?1,?2,?3,?4)")?;
    let mut stmt_del = conn.prepare_cached("DELETE FROM needs WHERE object_id=?1")?;
    while qh < queue.len() {
        let cur = queue[qh].clone();
        qh += 1;
        let cur_id = *seen.get(&cur).unwrap();
        stmt_del.execute(rusqlite::params![cur_id])?;
        let needed = dbvm::elf::meta_for_path_cached(&cur, &mut meta_cache)
            .needed
            .clone();
        if needed.is_empty() {
            continue;
        }
        let sdirs = dbvm::closure::search_dirs_for_cached(
            &cur,
            &base_dirs,
            &mut meta_cache,
            &mut search_cache,
            &ld_dirs,
        );
        let search_hash = dbvm::closure::search_dirs_hash(&sdirs);
        for (i, soname) in needed.iter().enumerate() {
            let resolved = dbvm::closure::resolve_soname_cached(
                soname,
                &sdirs,
                &mut resolve_cache,
                search_hash,
            )
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
            let rp_str = resolved.as_ref().map(|p| p.to_string_lossy().to_string());
            if let Some(rp) = resolved.clone()
                && !seen.contains_key(&rp)
            {
                let son = dbvm::elf::soname_for_path_cached(&rp, &mut meta_cache);
                conn.execute(
                    "INSERT INTO objects VALUES (?1,?2,?3,?4,?5)",
                    rusqlite::params![next_id, rp.to_string_lossy().to_string(), son, "lib", 0],
                )?;
                seen.insert(rp.clone(), next_id);
                queue.push(rp);
                next_id += 1;
            }
            stmt_needs.execute(rusqlite::params![cur_id, i as i64, soname, rp_str])?;
        }
    }
    drop(stmt_needs);
    drop(stmt_del);
    conn.execute_batch("COMMIT;")?;
    let total: i64 = conn.query_row("SELECT count(*) FROM objects", [], |r| r.get(0))?;
    let needs: i64 = conn.query_row("SELECT count(*) FROM needs", [], |r| r.get(0))?;
    println!(
        "indexed {} ELFs in {} -> {} (objects={} needs={})",
        exes.len(),
        dir,
        db,
        total,
        needs
    );
    Ok(())
}
fn scan_userland(out: &str, dirs: Vec<String>) -> anyhow::Result<()> {
    if Path::new(out).exists() {
        std::fs::remove_file(out)?;
    }
    let conn = Connection::open(out)?;
    conn.execute_batch(r#"CREATE TABLE IF NOT EXISTS objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT, is_root INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS needs(object_id INTEGER NOT NULL REFERENCES objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES objects(path));"#)?;
    drop(conn);
    for d in dirs {
        if !Path::new(&d).is_dir() {
            eprintln!("skip not a dir: {}", d);
            continue;
        }
        scan_dir(out, &d)?;
    }
    let conn = Connection::open(out)?;
    let nobjs: i64 = conn.query_row("SELECT count(*) FROM objects", [], |r| r.get(0))?;
    let nneeds: i64 = conn.query_row("SELECT count(*) FROM needs", [], |r| r.get(0))?;
    println!("userland -> {}: objects={} needs={}", out, nobjs, nneeds);
    Ok(())
}
fn bundle_list(path: &str, filter: &str) -> anyhow::Result<()> {
    let db = open_db(path);
    let has: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='bundle_objects'",
        [],
        |r| r.get(0),
    )?;
    if has == 0 {
        println!("{}: no bundle_objects (build with --bundle)", path);
        return Ok(());
    }
    let mut q = String::from("SELECT id, path, soname, kind, is_root, size FROM bundle_objects");
    if !filter.is_empty() {
        q.push_str(" WHERE soname LIKE ?1 OR path LIKE ?1");
    }
    q.push_str(" ORDER BY id");
    let mut st = db.prepare(&q)?;
    let like = format!("%{}%", filter);
    let mut rows: Vec<(i64, String, Option<String>, String, i64, i64)> = Vec::new();
    if filter.is_empty() {
        for r in st.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })? {
            rows.push(r?);
        }
    } else {
        for r in st.query_map([like.clone()], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })? {
            rows.push(r?);
        }
    }
    for (id, p, soname, kind, is_root, size) in &rows {
        let label = soname.clone().unwrap_or_else(|| {
            Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        });
        println!(
            "[{}] {:28} {:4} {:8}  {}{}",
            id,
            label,
            kind,
            size,
            p,
            if *is_root == 1 { " root" } else { "" }
        );
    }
    let n: i64 = db.query_row("SELECT count(*) FROM bundle_objects", [], |r| r.get(0))?;
    let e: i64 = db.query_row("SELECT count(*) FROM bundle_needs", [], |r| r.get(0))?;
    println!("bundle: {} objects, {} edges", n, e);
    Ok(())
}
fn bundle_info(path: &str) -> anyhow::Result<()> {
    let db = open_db(path);
    let has: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='bundle_objects'",
        [],
        |r| r.get(0),
    )?;
    if has == 0 {
        println!("{}: no bundle_objects", path);
        return Ok(());
    }
    let (cnt, sum): (i64, Option<i64>) =
        db.query_row("SELECT count(*), sum(size) FROM bundle_objects", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let needs: i64 = db.query_row("SELECT count(*) FROM bundle_needs", [], |r| r.get(0))?;
    let sz = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    println!(
        "{}: bundle_objects={} bytes={} needs={} self_size={}",
        path,
        cnt,
        sum.unwrap_or(0),
        needs,
        sz
    );
    Ok(())
}
