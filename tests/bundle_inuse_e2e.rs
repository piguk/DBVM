//! End-to-end: a running launch's cache entry cannot be evicted out from under it.
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin(name: &str) -> PathBuf {
    let p = PathBuf::from(format!("{}/target/release/{}", env!("CARGO_MANIFEST_DIR"), name));
    assert!(p.exists(), "build release first: {}", p.display());
    p
}

#[test]
fn running_launch_keeps_its_cache_entry() {
    let dir = std::env::temp_dir().join(format!("bundle-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache = dir.join("cache");
    // guest: a library whose function sleeps, so the launch is still running during eviction
    std::fs::write(dir.join("lib.c"), "#include <unistd.h>\nint slow(void){ sleep(3); return 0; }\n").unwrap();
    assert!(Command::new("gcc").args(["-fPIC", "-shared", "-Wl,-soname,libslow-e2e.so.1", "-o"])
        .arg(dir.join("libslow-e2e.so.1")).arg(dir.join("lib.c")).status().unwrap().success());
    std::fs::copy(dir.join("libslow-e2e.so.1"), dir.join("libslow-e2e.so")).unwrap();
    std::fs::write(dir.join("app.c"), "int slow(void);\nint main(void){ return slow(); }\n").unwrap();
    assert!(Command::new("gcc").arg("-o").arg(dir.join("app")).arg(dir.join("app.c"))
        .arg(format!("-L{}", dir.display())).arg("-lslow-e2e").arg("-Wl,-rpath,$ORIGIN").status().unwrap().success());
    let bundle = dir.join("app.self");
    assert!(Command::new(bin("elf2self")).arg(dir.join("app")).arg("-o").arg(&bundle).arg("--bundle").status().unwrap().success());
    let entry = {
        let db = rusqlite::Connection::open(&bundle).unwrap();
        let h: String = db.query_row("SELECT hash FROM bundle_objects WHERE soname='libslow-e2e.so.1'", [], |r| r.get(0)).unwrap();
        cache.join("bundle").join(h)
    };

    // start a launch that runs ~3 s
    let mut child = Command::new(bin("self-exec")).arg(&bundle)
        .env("LD_LIBRARY_PATH", "").env("SELF_VM_CACHE", &cache).spawn().unwrap();
    // wait until the entry exists (launch has linked it)
    for _ in 0..200 { if entry.exists() { break; } std::thread::sleep(std::time::Duration::from_millis(10)); }
    std::thread::sleep(std::time::Duration::from_millis(300));

    // eviction to 0 bytes must skip the in-use entry
    let (removed, _) = selfdb::bundle_cache::enforce_bundle_limit(&cache.join("bundle"), 0).unwrap();
    assert_eq!(removed, 0, "in-use entry must not be evicted");
    assert!(entry.exists());

    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "launch must complete successfully");
    // after the launch exits the entry is free and can be evicted
    let (removed2, _) = selfdb::bundle_cache::enforce_bundle_limit(&cache.join("bundle"), 0).unwrap();
    assert_eq!(removed2, 1);
    let _ = std::fs::remove_dir_all(&dir);
}
