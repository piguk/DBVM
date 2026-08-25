use crate::elf::{ElfMeta, meta_for_path, meta_for_path_cached};
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Path, PathBuf};

fn expand_runpath(raw: &str, bdir: &Path) -> Vec<PathBuf> {
    let bdir_str = bdir.to_string_lossy();
    let mut out = Vec::new();
    for tok in raw.split(':') {
        if tok.is_empty() {
            continue;
        }
        let expanded = if tok.contains("$ORIGIN") {
            if tok.contains("${ORIGIN}") {
                tok.replace("${ORIGIN}", &bdir_str)
                    .replace("$ORIGIN", &bdir_str)
            } else {
                tok.replace("$ORIGIN", &bdir_str)
            }
        } else {
            tok.to_string()
        };
        if expanded == "." || expanded == "./" || expanded.is_empty() {
            out.push(bdir.to_path_buf());
            continue;
        }
        if let Some(rel) = expanded.strip_prefix("./") {
            out.push(bdir.join(rel));
        } else {
            out.push(PathBuf::from(expanded));
        }
    }
    out
}

pub fn search_dirs_for(obj_path: &Path, extra_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let canon = std::fs::canonicalize(obj_path).unwrap_or_else(|_| obj_path.to_path_buf());
    let bdir_canon = canon
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let m = meta_for_path(&canon.to_string_lossy());
    let runpath = m.runpath;
    let rpath = m.rpath;
    let mut dirs = Vec::with_capacity(20);
    let ld = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let has_runpath = runpath.is_some();
    if has_runpath {
        for d in ld.split(':').filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(d));
        }
        if let Some(rp) = runpath {
            dirs.extend(expand_runpath(&rp, &bdir_canon));
        }
    } else {
        if let Some(rp) = rpath {
            dirs.extend(expand_runpath(&rp, &bdir_canon));
        }
        for d in ld.split(':').filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(d));
        }
    }
    for d in extra_dirs {
        if !dirs.contains(d) {
            dirs.push(d.clone());
        }
    }
    if !dirs.contains(&bdir_canon) {
        dirs.push(bdir_canon);
    }
    for s in [
        "/lib/x86_64-linux-gnu",
        "/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib",
        "/usr/lib64",
    ] {
        let p = PathBuf::from(s);
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    let mut seen = FxHashSet::default();
    let mut uniq = Vec::with_capacity(dirs.len());
    for d in dirs {
        if seen.insert(d.clone()) {
            uniq.push(d);
        }
    }
    uniq
}

pub fn search_dirs_for_cached(
    obj_path: &Path,
    extra_dirs: &[PathBuf],
    meta_cache: &mut FxHashMap<PathBuf, ElfMeta>,
    search_cache: &mut FxHashMap<PathBuf, Vec<PathBuf>>,
    ld_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    if let Some(v) = search_cache.get(obj_path) {
        return v.clone();
    }
    let canon = std::fs::canonicalize(obj_path).unwrap_or_else(|_| obj_path.to_path_buf());
    if let Some(v) = search_cache.get(&canon) {
        let v2 = v.clone();
        search_cache.insert(obj_path.to_path_buf(), v2.clone());
        return v2;
    }
    let bdir_canon = canon
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let meta = meta_for_path_cached(&canon, meta_cache);
    let runpath = meta.runpath.clone();
    let rpath = meta.rpath.clone();
    let mut dirs = Vec::with_capacity(20);
    let has_runpath = runpath.is_some();
    if has_runpath {
        dirs.extend(ld_dirs.iter().cloned());
        if let Some(rp) = runpath {
            dirs.extend(expand_runpath(&rp, &bdir_canon));
        }
    } else {
        if let Some(rp) = rpath {
            dirs.extend(expand_runpath(&rp, &bdir_canon));
        }
        dirs.extend(ld_dirs.iter().cloned());
    }
    for d in extra_dirs {
        if !dirs.contains(d) {
            dirs.push(d.clone());
        }
    }
    if !dirs.contains(&bdir_canon) {
        dirs.push(bdir_canon);
    }
    for s in [
        "/lib/x86_64-linux-gnu",
        "/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib",
        "/usr/lib64",
    ] {
        let p = PathBuf::from(s);
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    let mut seen = FxHashSet::default();
    let mut uniq = Vec::with_capacity(dirs.len());
    for d in dirs {
        if seen.insert(d.clone()) {
            uniq.push(d);
        }
    }
    search_cache.insert(obj_path.to_path_buf(), uniq.clone());
    search_cache.insert(canon, uniq.clone());
    uniq
}

pub fn resolve_soname(soname: &str, search: &[PathBuf]) -> Option<PathBuf> {
    for d in search {
        let cand = d.join(soname);
        if std::fs::metadata(&cand)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return Some(cand);
        }
    }
    None
}

pub fn search_dirs_hash(search: &[PathBuf]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    for p in search {
        p.hash(&mut hasher);
    }
    hasher.finish()
}

pub fn resolve_soname_cached(
    soname: &str,
    search: &[PathBuf],
    cache: &mut FxHashMap<u64, Option<PathBuf>>,
    search_hash: u64,
) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    soname.hash(&mut hasher);
    search_hash.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(v) = cache.get(&key) {
        return v.clone();
    }
    let mut found = None;
    for d in search {
        let cand = d.join(soname);
        if std::fs::metadata(&cand)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            found = Some(cand);
            break;
        }
    }
    cache.insert(key, found.clone());
    found
}

pub fn resolve_soname_cached_strkey(
    soname: &str,
    search: &[PathBuf],
    cache: &mut FxHashMap<String, Option<PathBuf>>,
    search_key: &str,
) -> Option<PathBuf> {
    let key = format!("{}|{}", soname, search_key);
    if let Some(v) = cache.get(&key) {
        return v.clone();
    }
    let mut found = None;
    for d in search {
        let cand = d.join(soname);
        if std::fs::metadata(&cand)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            found = Some(cand);
            break;
        }
    }
    cache.insert(key, found.clone());
    found
}
