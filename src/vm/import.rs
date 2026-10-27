use super::*;

pub fn vm_import_closure(conn: &Connection, host_elf: &str, vm_prefix: &str) -> Result<usize> {
    let host_real = std::fs::canonicalize(host_elf).unwrap_or(PathBuf::from(host_elf));
    // The path the caller names is how a program is invoked (e.g. /bin/sh), and on merged-usr
    // hosts it is a directory symlink (/bin -> usr/bin). Record it, so the VM resolves it, and
    // point it at the canonical file. Canonicalization alone would store only /usr/bin/dash.
    let literal = PathBuf::from(host_elf);
    if literal.is_absolute() && literal != host_real {
        let lit_vm = literal.to_string_lossy().to_string();
        if let Some(parent) = literal.parent() {
            if parent != Path::new("/") {
                let _ = ensure_parent_dirs(conn, &lit_vm);
            }
        }
        // a directory component that is a host symlink (e.g. /bin) must exist as a VM link too
        let mut acc = PathBuf::from("/");
        for comp in literal.parent().unwrap_or(Path::new("/")).components().skip(1) {
            acc.push(comp);
            if let Ok(md) = std::fs::symlink_metadata(&acc) {
                if md.file_type().is_symlink() {
                    if let Ok(t) = std::fs::read_link(&acc) {
                        let target_abs = if t.is_absolute() { t.clone() } else { acc.parent().unwrap_or(Path::new("/")).join(&t) };
                        let target_canon = std::fs::canonicalize(&target_abs).unwrap_or(target_abs);
                        let _ = vm_add_symlink(conn, &acc.to_string_lossy(), &target_canon.to_string_lossy());
                    }
                }
            }
        }
        let _ = vm_add_symlink(conn, &lit_vm, &host_real.to_string_lossy());
    }
    use rustc_hash::FxHashMap;
    use crate::elf::ElfMeta;
    let mut meta_cache: FxHashMap<PathBuf, ElfMeta> = FxHashMap::default();
    let mut search_cache: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    let mut resolve_cache: FxHashMap<u64, Option<PathBuf>> = FxHashMap::default();
    let ld_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    let mut seen: FxHashMap<PathBuf, String> = FxHashMap::default();
    let mut order: Vec<PathBuf> = Vec::new();
    // (host path where the soname was found, soname). The loader looks these names up, so each
    // name that differs from its real file gets a link in the VM at the same location.
    let mut soname_links: Vec<(PathBuf, String)> = Vec::new();
    let mut q: Vec<PathBuf> = vec![host_real.clone()];
    seen.insert(host_real.clone(), "exe".to_string()); order.push(host_real.clone());
    let mut qi=0;
    while qi < q.len() {
        let cur = q[qi].clone(); qi+=1;
        let sdirs = crate::closure::search_dirs_for_cached(&cur, &[], &mut meta_cache, &mut search_cache, &ld_dirs);
        let sh = crate::closure::search_dirs_hash(&sdirs);
        // PT_INTERP (e.g. /lib64/ld-linux-x86-64.so.2) is not a DT_NEEDED entry, but the kernel
        // needs it at exactly that literal path to exec the object inside the VM. The literal
        // path is imported as-is (keeping the host symlink, which points at the real file);
        // the real file is queued like any other dependency so the link resolves.
        if let Ok(data) = std::fs::read(&cur) {
            if let Ok(elf) = goblin::elf::Elf::parse(&data) {
                if let Some(interp) = elf.interpreter {
                    let lit = PathBuf::from(interp);
                    if lit.exists() {
                        if !order.contains(&lit) {
                            seen.insert(lit.clone(), "interp".to_string());
                            order.push(lit.clone());
                        }
                        let real = std::fs::canonicalize(&lit).unwrap_or(lit.clone());
                        if !seen.contains_key(&real) {
                            seen.insert(real.clone(), "interp-target".to_string());
                            order.push(real.clone());
                            q.push(real);
                        }
                    }
                }
            }
        }
        let needed = crate::elf::meta_for_path_cached(&cur, &mut meta_cache).needed.clone();
        for soname in needed {
            if let Some(hit) = crate::closure::resolve_soname_cached(&soname, &sdirs, &mut resolve_cache, sh) {
                // the name the loader asks for (may differ from the real file, e.g. libcap.so.2)
                soname_links.push((hit.clone(), soname.clone()));
                let rp = std::fs::canonicalize(&hit).unwrap_or(hit);
                if !seen.contains_key(&rp) {
                    seen.insert(rp.clone(), "lib".to_string());
                    order.push(rp.clone());
                    q.push(rp);
                }
            }
        }
    }
    let prefix = normalize_vm_path(vm_prefix);
    // DT_NEEDED names: link each soname to the real file it resolved to, at its host location.
    // The name must exist in the VM: whether the host name equals the real file name does not
    // matter (libcap.so.2 is a host link even though its name equals the hit path).
    for (hit, name) in &soname_links {
        let Some(parent) = hit.parent() else { continue };
        let link_path = parent.join(name);
        let link_s = link_path.to_string_lossy().to_string();
        if vm_resolve(conn, &link_s).is_ok() { continue; }
        let real = std::fs::canonicalize(hit).unwrap_or(hit.clone());
        ensure_parent_dirs(conn, &link_s)?;
        vm_add_symlink(conn, &link_s, &real.to_string_lossy())?;
    }
    // Interpreter literal paths are host symlinks, often relative (../lib/...). Inside the VM the
    // target would resolve against a /lib alias that may not exist, so store the link with the
    // absolute path of the real file, which is imported below.
    let interp_literals: Vec<PathBuf> = order.iter().filter(|p| seen.get(*p).map(|k| k == "interp").unwrap_or(false)).cloned().collect();
    for lit in interp_literals {
        if let Ok(real) = std::fs::canonicalize(&lit) {
            let md = std::fs::symlink_metadata(&lit)?;
            if md.file_type().is_symlink() {
                let lit_s = lit.to_string_lossy().to_string();
                vm_add_symlink(conn, &lit_s, &real.to_string_lossy())?;
            }
        }
    }
    for p in &order {
        let vm_path = if prefix == "/" { p.to_string_lossy().to_string() } else { format!("{}/{}", prefix.trim_end_matches('/'), p.file_name().unwrap().to_string_lossy()) };
        let target = p.to_string_lossy().to_string();
        // interpreter literals were stored above as absolute links; do not overwrite them
        if seen.get(p).map(|k| k == "interp").unwrap_or(false) { continue; }
        vm_add_file(conn, &target, &target)?;
        if vm_path != target {
            vm_add_file(conn, &target, &vm_path)?;
        }
    }
    Ok(order.len())
}

/// One entry of a walked host tree. Directories and symlinks are part of a tree, not incidental:
/// skipping them dropped every entry under a linked directory. `/usr/include/x86_64-linux-gnu/asm`
/// is a link to `/usr/lib/linux/uapi/x86/asm`, so packing /usr/include silently lost all 66
/// `asm/*.h` headers and `#include <errno.h>` failed in the VM (measured).
enum PackEntry {
    Dir { vm_path: String, mode: i64, mtime: i64 },
    Symlink { vm_path: String, target: String },
    File { host: PathBuf, vm_path: String },
}

/// (permission bits, mtime in seconds) as recorded in `vm_fs`.
fn host_mode_mtime(md: &std::fs::Metadata) -> (i64, i64) {
    #[cfg(unix)] let mode = { use std::os::unix::fs::PermissionsExt; (md.permissions().mode() & 0o7777) as i64 };
    #[cfg(not(unix))] let mode = if md.is_dir() { 0o755 } else { 0o644 };
    let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or_else(now_secs);
    (mode, mtime)
}

/// Walks `host_dir` without following links and maps every directory, symlink and regular file
/// below it to its path under `vm_prefix`, in walk order (parents before children). Other kinds
/// (sockets, fifos, devices) are skipped.
fn plan_host_dir(host_dir: &str, vm_prefix: &str) -> Result<Vec<PackEntry>> {
    let mut plan = Vec::new();
    for entry in walkdir::WalkDir::new(host_dir).follow_links(false).min_depth(1) {
        let e = entry?;
        let rel = e.path().strip_prefix(host_dir).unwrap_or(e.path()).to_string_lossy().to_string();
        let vm_path = if vm_prefix == "/" { format!("/{}", rel.trim_start_matches('/')) }
                      else { format!("{}/{}", vm_prefix.trim_end_matches('/'), rel.trim_start_matches('/')) };
        let vm_path = normalize_vm_path(&vm_path);
        let ft = e.file_type();
        if ft.is_dir() {
            let (mode, mtime) = host_mode_mtime(&std::fs::symlink_metadata(e.path())?);
            plan.push(PackEntry::Dir { vm_path, mode, mtime });
        } else if ft.is_symlink() {
            let target = std::fs::read_link(e.path())?.to_string_lossy().to_string();
            plan.push(PackEntry::Symlink { vm_path, target });
        } else if ft.is_file() {
            plan.push(PackEntry::File { host: e.path().to_path_buf(), vm_path });
        }
    }
    Ok(plan)
}

/// Writes a directory or symlink entry. Files are written by the caller (the two packers differ there).
fn put_tree_node(conn: &Connection, entry: &PackEntry) -> Result<()> {
    match entry {
        PackEntry::Dir { vm_path, mode, mtime } => vm_add_dir(conn, vm_path, *mode, *mtime),
        PackEntry::Symlink { vm_path, target } => vm_add_symlink(conn, vm_path, target),
        PackEntry::File { .. } => unreachable!("files are written by the packer"),
    }
}

/// Packs a host tree into `vm_prefix`. Returns the number of `vm_fs` rows written: directories,
/// symlinks and regular files below `host_dir` (not `host_dir` itself).
pub fn vm_pack_host_dir(conn: &Connection, host_dir: &str, vm_prefix: &str) -> Result<usize> {
    let plan = plan_host_dir(host_dir, vm_prefix)?;
    for batch in plan.chunks(2000) {
        write_tx(conn, || batch.iter().try_for_each(|entry| match entry {
            PackEntry::File { host, vm_path } => vm_add_file(conn, &host.to_string_lossy(), vm_path),
            node => put_tree_node(conn, node),
        }))?;
    }
    Ok(plan.len())
}

/// Tables whose secondary indexes are dropped during a bulk load. Single definition, so the
/// helper and anything that checks it cannot disagree.
pub const BULK_TABLES: &[&str] = &["vm_fs", "vm_blobs", "vm_file_chunks"];

const BULK_TABLE_LIST_SQL: &str = "'vm_fs','vm_blobs','vm_file_chunks'";

/// Secondary indexes on the tables a bulk load writes. Dropping them for the load and rebuilding
/// afterwards is much cheaper than maintaining them row by row. Definitions are read from
/// sqlite_master, so the rebuilt indexes are exactly the schema's, not a second copy.
fn bulk_index_defs(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut st = conn.prepare(&format!("SELECT name, sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL AND tbl_name IN ({}) ORDER BY name", BULK_TABLE_LIST_SQL))?;
    let rows = st.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Run `load` with the bulk-path indexes dropped and restored afterwards.
///
/// Restore happens on every exit path. A restore failure is reported as an error, never
/// swallowed: a database left without its indexes must not look like a successful import.
/// Panics unwind through the guard, which still restores the indexes (best effort, since a
/// panic cannot return an error; the panic itself is still propagated).
pub fn with_bulk_indexes_dropped<T>(conn: &Connection, load: impl FnOnce() -> Result<T>) -> Result<T> {
    let defs = bulk_index_defs(conn)?;
    for (name, _) in &defs {
        conn.execute(&format!("DROP INDEX IF EXISTS \"{}\"", name), [])?;
    }
    struct Restore<'a> { conn: &'a Connection, defs: &'a [(String, String)], armed: bool }
    impl Restore<'_> {
        fn run(&mut self) -> Result<()> {
            self.armed = false;
            for (_, sql) in self.defs {
                self.conn.execute_batch(&format!("{};", sql))
                    .map_err(|e| anyhow!("rebuild index failed (database left unindexed): {}", e))?;
            }
            Ok(())
        }
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            // reached only on panic (run() disarms it): best-effort restore
            if self.armed { for (_, sql) in self.defs { let _ = self.conn.execute_batch(&format!("{};", sql)); } }
        }
    }
    let mut guard = Restore { conn, defs: &defs, armed: true };
    let out = load();
    let restored = guard.run();
    match (out, restored) {
        (Ok(v), Ok(())) => Ok(v),
        // keep the import's own error; still surface that the database is unindexed
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(r)) => Err(anyhow!("{}; and {}", e, r)),
        (Ok(_), Err(r)) => Err(r),
    }
}

/// Parallel directory import. Compression (CPU) runs on rayon workers in bounded batches;
/// SQLite writes stay serial in walk order inside short transactions. The dictionary is read
/// once per run, so every file is encoded exactly as the serial path would encode it.
/// Output matches `vm_pack_host_dir` (verified by a test); only wall time differs.
pub fn vm_pack_host_dir_parallel(conn: &Connection, host_dir: &str, vm_prefix: &str) -> Result<usize> {
    let n = vm_pack_host_dir_parallel_inner(conn, host_dir, vm_prefix)?;
    // bound the content cache once per import (a full scan, so not per file)
    let _ = enforce_cache_limit(&cache_dir(), cache_limit_from_env());
    // a bulk load changes the data distribution the planner relies on; refresh statistics
    // (cheap: a few ms on a 30k-file tree). Failure here is harmless, so it is not an error.
    let _ = conn.execute_batch("PRAGMA optimize;");
    Ok(n)
}

/// (chunk hash, chunk length, encoded chunk as returned by `compress_with_dict`)
type EncodedChunk = (BlobKey, usize, (Vec<u8>, i64, i64));

/// A file read and compressed off the database thread, ready to be written.
struct EncodedFile {
    vm_path: String,
    mode: i64,
    mtime: i64,
    pieces: Vec<EncodedChunk>,
    data: Vec<u8>,
}

fn vm_pack_host_dir_parallel_inner(conn: &Connection, host_dir: &str, vm_prefix: &str) -> Result<usize> {
    use rayon::prelude::*;
    const BATCH: usize = 256;
    // 1) plan in walk order; nothing is written yet
    let mut nodes: Vec<PackEntry> = Vec::new();
    let mut plan: Vec<(PathBuf, String)> = Vec::new();
    for entry in plan_host_dir(host_dir, vm_prefix)? {
        match entry {
            PackEntry::File { host, vm_path } => plan.push((host, vm_path)),
            node => nodes.push(node),
        }
    }
    // 2) one dictionary snapshot for the whole run
    let dict = latest_dict(conn);
    with_bulk_indexes_dropped(conn, || {
    // 3) directories and symlinks first, in one transaction: walk order puts parents before
    //    children, so every file's parent directory already carries its host mode and mtime
    write_tx(conn, || nodes.iter().try_for_each(|node| put_tree_node(conn, node)))?;
    let mut n = nodes.len();
    for chunk in plan.chunks(BATCH) {
        // parallel: read + encode each file's chunks (pure CPU, no DB)
        let encoded: Vec<Result<EncodedFile>> = chunk
            .par_iter()
            .map(|(host, vm_path)| {
                let (mode, mtime) = host_mode_mtime(&std::fs::metadata(host)?);
                let data = std::fs::read(host)?;
                let pieces = data.chunks(CHUNK_SIZE).map(|piece| {
                    let enc = compress_with_dict(piece, dict.as_ref()).unwrap_or_else(|| (piece.to_vec(), 0, 0));
                    (blob_key(piece), piece.len(), enc)
                }).collect();
                Ok(EncodedFile { vm_path: vm_path.clone(), mode, mtime, pieces, data })
            })
            .collect();
        // serial writes, walk order
        write_tx(conn, || {
            for item in encoded {
                let EncodedFile { vm_path, mode, mtime, pieces, data } = item?;
                ensure_parent_dirs(conn, &vm_path)?;
                conn.prepare_cached("DELETE FROM vm_file_chunks WHERE path=?1")?.execute(params![vm_path])?;
                for (i, (key, plen, enc)) in pieces.into_iter().enumerate() {
                    let piece_start = i * CHUNK_SIZE;
                    let piece = &data[piece_start..piece_start + plen];
                    if conn.prepare_cached("SELECT 1 FROM vm_blobs WHERE hash=?1")?.query_row(params![key.as_slice()], |_| Ok(())).optional()?.is_none() {
                        blob_insert_encoded(conn, &key, piece, enc)?;
                    }
                    conn.prepare_cached("INSERT INTO vm_file_chunks(path, idx, hash) VALUES (?1,?2,?3)")?
                        .execute(params![vm_path, i as i64, key.as_slice()])?;
                }
                let file_hash = blob_key(&data);
                conn.prepare_cached(
                    "INSERT INTO vm_fs(path,kind,mode,mtime,size,hash,content,compressed,link_target) VALUES (?1,'file',?2,?3,?4,?5,NULL,0,NULL)
                     ON CONFLICT(path) DO UPDATE SET kind='file', mode=excluded.mode, mtime=excluded.mtime, size=excluded.size,
                        hash=excluded.hash, content=NULL, compressed=0, link_target=NULL")?
                    .execute(params![vm_path, mode, mtime, data.len() as i64, file_hash.as_slice()])?;
                n += 1;
            }
            Ok(())
        })?;
    }
    Ok(n)
    })
}

/// The VM path of a tar member: `./` and `strip_prefix` removed, then normalized. An empty result or "/"
/// is the archive root.
fn tar_vm_path(archive_path: &str, strip_prefix: &str) -> String {
    let mut p = archive_path.strip_prefix("./").unwrap_or(archive_path);
    if !strip_prefix.is_empty() {
        if let Some(rest) = p.strip_prefix(strip_prefix) { p = rest; }
        else if p == strip_prefix.trim_end_matches('/') { p = ""; }
    }
    normalize_vm_path(p)
}

pub fn vm_import_tar_filtered(conn: &Connection, tar_path: &str, strip_prefix: &str, whitelist: Option<&[String]>, exclude: Option<&[String]>) -> Result<usize> {
    use std::io::Read;
    let file = std::fs::File::open(tar_path).map_err(|e| anyhow!("open tar {}: {}", tar_path, e))?;
    let is_gz = tar_path.ends_with(".gz") || tar_path.ends_with(".tgz");
    let reader: Box<dyn Read> = if is_gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    let mut n=0usize;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let vm_path = tar_vm_path(&entry.path()?.to_string_lossy(), strip_prefix);
        if vm_path.is_empty() || vm_path=="/" { continue; }
        if let Some(wl) = whitelist {
            if !wl.is_empty() && !wl.iter().any(|p| vm_path.starts_with(p) || p.starts_with(&vm_path)) { continue; }
        }
        if let Some(ex) = exclude {
            if ex.iter().any(|p| vm_path == *p || vm_path.starts_with(&format!("{}/", p))) { continue; }
        }
        let header = entry.header();
        let kind = header.entry_type();
        let mode = header.mode().unwrap_or(0o644) as i64;
        let mtime = header.mtime().unwrap_or(now_secs() as u64) as i64;
        if kind.is_dir() { vm_add_dir(conn, &vm_path, mode, mtime)?; }
        else if kind.is_symlink() { if let Some(t) = entry.link_name()? { vm_add_symlink(conn, &vm_path, &t.to_string_lossy())?; } }
        else if kind.is_hard_link() {
            // A hard link names another archive member by its archive path, so it is a second regular file
            // with the same content (the blobs are shared). Storing it as a symlink to that archive path
            // pointed at the wrong file: the path is relative to the archive root, not to the link.
            let Some(t) = entry.link_name()? else { continue };
            let target = tar_vm_path(&t.to_string_lossy(), strip_prefix);
            let data = vm_cat(conn, &target).map_err(|e| anyhow!("hard link {} -> {}: {}", vm_path, target, e))?;
            vm_add_bytes(conn, &vm_path, &data, mode, mtime)?;
        }
        else if kind.is_file() { let mut data = Vec::new(); entry.read_to_end(&mut data)?; vm_add_bytes(conn, &vm_path, &data, mode, mtime)?; }
        else { continue; }
        n+=1;
        if n.is_multiple_of(2000) { conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?; }
    }
    conn.execute_batch("COMMIT;")?;
    conn.prepare_cached("INSERT INTO vm_log(op, detail) VALUES ('import_tar', ?1)")?.execute( params![tar_path])?;
    Ok(n)
}
