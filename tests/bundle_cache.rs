//! Bundle launcher cache: a private library is stored once under its content hash, a missing or
//! truncated entry is rebuilt, and per-run cleanup never removes the shared cache.
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin(name: &str) -> PathBuf {
    let p = PathBuf::from(format!("{}/target/release/{}", env!("CARGO_MANIFEST_DIR"), name));
    assert!(p.exists(), "build release first: {}", p.display());
    p
}

/// Build a bundled executable that depends on a private library (not present on the system).
fn make_bundle(dir: &Path) -> PathBuf {
    let lib_src = dir.join("lib.c");
    std::fs::write(&lib_src, "int big_hello(void){ return 7; }\n").unwrap();
    assert!(Command::new("gcc").args(["-fPIC", "-shared", "-Wl,-soname,libprivate-test.so.1", "-o"])
        .arg(dir.join("libprivate-test.so.1")).arg(&lib_src).status().unwrap().success());
    let app_src = dir.join("app.c");
    std::fs::write(&app_src, "int big_hello(void);\nint main(void){ return big_hello(); }\n").unwrap();
    // link name without a version (-lprivate-test looks for libprivate-test.so)
    std::fs::copy(dir.join("libprivate-test.so.1"), dir.join("libprivate-test.so")).unwrap();
    let st = Command::new("gcc").arg("-o").arg(dir.join("app")).arg(&app_src)
        .arg(format!("-L{}", dir.display())).arg("-lprivate-test")
        .arg("-Wl,-rpath,$ORIGIN").status().unwrap();
    assert!(st.success(), "link failed");
    let out = dir.join("app.self");
    assert!(Command::new(bin("elf2self")).arg(dir.join("app")).arg("-o").arg(&out).arg("--bundle").status().unwrap().success());
    out
}

/// Each test gets its own cache root. It is passed to the launcher explicitly, never through
/// the process-global environment, so parallel tests cannot redirect each other.
fn isolate(dir: &Path) -> PathBuf {
    let root = dir.join("cache");
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn entry_for(bundle: &Path, soname: &str, cache: &Path) -> PathBuf {
    let db = rusqlite::Connection::open(bundle).unwrap();
    let hash: String = db.query_row("SELECT hash FROM bundle_objects WHERE soname=?1", [soname], |r| r.get(0)).unwrap();
    cache.join(hash)
}

fn run(bundle: &Path, cache: &Path) -> std::process::ExitStatus {
    Command::new(bin("self-exec")).arg(bundle).env("LD_LIBRARY_PATH", "").env("SELF_VM_CACHE", cache).status().unwrap()
}

/// Run with TMPDIR pointing at a private directory, so per-run dirs can be counted in isolation.
fn run_in_tmp(bundle: &Path, tmp: &Path, cache: &Path) -> std::process::ExitStatus {
    Command::new(bin("self-exec")).arg(bundle).env("LD_LIBRARY_PATH", "").env("TMPDIR", tmp).env("SELF_VM_CACHE", cache).status().unwrap()
}

#[test]
fn missing_and_truncated_cache_entries_are_rebuilt() {
    let dir = std::env::temp_dir().join(format!("bundle-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = isolate(&dir);
    let cache = root.join("bundle");
    let bundle = make_bundle(&dir);
    let entry = entry_for(&bundle, "libprivate-test.so.1", &cache);

    // first launch populates the cache; program exits with big_hello()'s 7
    assert_eq!(run(&bundle, &root).code(), Some(7));
    let want = std::fs::metadata(&entry).unwrap().len();

    // missing entry: rebuilt
    std::fs::remove_file(&entry).unwrap();
    assert_eq!(run(&bundle, &root).code(), Some(7));
    assert_eq!(std::fs::metadata(&entry).unwrap().len(), want, "rebuilt entry must be complete");

    // truncated entry: size mismatch, so it is rebuilt and the program still runs correctly
    std::fs::write(&entry, b"x").unwrap();
    assert_eq!(run(&bundle, &root).code(), Some(7));
    assert_eq!(std::fs::metadata(&entry).unwrap().len(), want);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_run_cleanup_keeps_shared_cache() {
    let dir = std::env::temp_dir().join(format!("bundle-keep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = isolate(&dir);
    let cache = root.join("bundle");
    let bundle = make_bundle(&dir);
    let entry = entry_for(&bundle, "libprivate-test.so.1", &cache);
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    assert_eq!(run_in_tmp(&bundle, &tmp, &root).code(), Some(7));
    assert_eq!(run_in_tmp(&bundle, &tmp, &root).code(), Some(7));
    assert!(entry.exists(), "shared cache entry must survive launches");
    let leftovers = std::fs::read_dir(&tmp).unwrap()
        .filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with("self-bundle-")).count();
    assert_eq!(leftovers, 0, "per-run self-bundle dirs must be removed");
    let _ = std::fs::remove_dir_all(&dir);
}
