use clap::Parser;
use selfdb::{elf::{parse_elf, ElfMeta}, db::create_self_db};
use anyhow::Result;
use rusqlite::{Connection, params};
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
struct Args {
    input: String,
    #[arg(short, long, default_value="a.self")]
    output: String,
    #[arg(long, default_value_t=false)]
    no_sections: bool,
    #[arg(long, default_value_t=false)]
    no_notes: bool,
    #[arg(long, default_value_t=false)]
    bundle: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let info = parse_elf(&args.input, args.no_sections, args.no_notes)?;
    create_self_db(&args.output, &info, args.no_sections, args.no_notes)?;
    let mut extra = String::new();
    if args.bundle {
        let bundle_objects = bundle_closure(&args.input, &args.output, &info)?;
        extra = format!(" bundle_objects={} bundle_needs={}", bundle_objects.0, bundle_objects.1);
    }
    println!("wrote {}", args.output);
    println!("  segments: {}, symbols: {}, sections: {}, notes: {}, dynamic: {}, needed: {}, relocs: {}{}", info.e_phnum, info.symbols.len(), info.sections.len(), info.notes.len(), info.dynamic_entries.len(), info.needed.len(), info.relocs.len(), extra);
    Ok(())
}

fn bundle_closure(input: &str, output: &str, info: &selfdb::elf::ElfInfo) -> Result<(usize, usize)> {
    let inp_real = std::fs::canonicalize(input).unwrap_or(PathBuf::from(input));
    let interp = info.interp.clone();
    let conn = Connection::open(output)?;
    let mut meta_cache: FxHashMap<PathBuf, ElfMeta> = FxHashMap::default();
    let mut search_cache: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    let mut resolve_cache: FxHashMap<u64, Option<PathBuf>> = FxHashMap::default();
    let ld_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    let mut seen: FxHashMap<PathBuf, String> = FxHashMap::default();
    let mut order: Vec<(PathBuf, Option<String>, String)> = Vec::new();
    let soname_of_cached = |path: &Path, cache: &mut FxHashMap<PathBuf, ElfMeta>| selfdb::elf::soname_for_path_cached(path, cache);
    let add = |path: PathBuf, kind: &str, order: &mut Vec<(PathBuf, Option<String>, String)>, seen: &mut FxHashMap<PathBuf, String>, meta_cache: &mut FxHashMap<PathBuf, ElfMeta>| {
        let rp = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if seen.contains_key(&rp) { return; }
        let soname = if kind=="lib" { soname_of_cached(&rp, meta_cache) } else { None };
        seen.insert(rp.clone(), kind.to_string());
        order.push((rp, soname, kind.to_string()));
    };
    add(inp_real.clone(), "exe", &mut order, &mut seen, &mut meta_cache);
    if let Some(p) = interp { let pb=PathBuf::from(p); if pb.is_file() { add(std::fs::canonicalize(&pb).unwrap_or(pb.clone()), "lib", &mut order, &mut seen, &mut meta_cache); } }
    let mut qh: usize = 0;
    let mut qvec: Vec<PathBuf> = order.iter().map(|(p,_,_)| p.clone()).collect();
    while qh < qvec.len() {
        let cur = qvec[qh].clone(); qh+=1;
        let sdirs = selfdb::closure::search_dirs_for_cached(&cur, &[], &mut meta_cache, &mut search_cache, &ld_dirs);
        let search_hash = selfdb::closure::search_dirs_hash(&sdirs);
        let needed = selfdb::elf::meta_for_path_cached(&cur, &mut meta_cache).needed.clone();
        for soname in needed {
            if let Some(rp) = selfdb::closure::resolve_soname_cached(&soname, &sdirs, &mut resolve_cache, search_hash) {
                let rp = std::fs::canonicalize(&rp).unwrap_or(rp);
                if !seen.contains_key(&rp) {
                    add(rp.clone(), "lib", &mut order, &mut seen, &mut meta_cache);
                    qvec.push(rp);
                }
            }
        }
    }
    conn.execute_batch(r#"
    CREATE TABLE bundle_objects(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, soname TEXT, kind TEXT NOT NULL, is_root INTEGER NOT NULL, size INTEGER NOT NULL, content BLOB NOT NULL);
    CREATE TABLE bundle_needs(object_id INTEGER NOT NULL REFERENCES bundle_objects(id), ord INTEGER NOT NULL, soname TEXT NOT NULL, resolved_path TEXT REFERENCES bundle_objects(path));
    CREATE INDEX IF NOT EXISTS idx_bundle_needs_soname ON bundle_needs(soname);
    "#)?;
    let mut path_to_id: FxHashMap<PathBuf,i64> = FxHashMap::default();
    for (idx,(rp,soname,kind)) in order.iter().enumerate() {
        let is_root = if idx==0 {1} else {0};
        let data = std::fs::read(rp)?;
        let id = (idx+1) as i64;
        conn.execute("INSERT INTO bundle_objects VALUES (?1,?2,?3,?4,?5,?6,?7)", params![id, rp.to_string_lossy().to_string(), soname, kind, is_root, data.len() as i64, data])?;
        path_to_id.insert(rp.clone(), id);
    }
    let mut needs = 0;
    for (rp,_,_) in &order {
        if let Some(oid) = path_to_id.get(rp) {
            let sdirs = selfdb::closure::search_dirs_for_cached(rp, &[], &mut meta_cache, &mut search_cache, &ld_dirs);
            let search_hash = selfdb::closure::search_dirs_hash(&sdirs);
            for (n, soname) in selfdb::elf::meta_for_path_cached(rp, &mut meta_cache).needed.clone().iter().enumerate() {
                let resolved = selfdb::closure::resolve_soname_cached(soname, &sdirs, &mut resolve_cache, search_hash).map(|p| std::fs::canonicalize(&p).unwrap_or(p).to_string_lossy().to_string());
                conn.execute("INSERT INTO bundle_needs VALUES (?1,?2,?3,?4)", params![oid, n as i64, soname, resolved])?;
                needs+=1;
            }
        }
    }
    Ok((order.len(), needs))
}
