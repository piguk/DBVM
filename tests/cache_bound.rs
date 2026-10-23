//! Content-cache bound: once the cache exceeds its limit, the least recently used files are
//! evicted. Eviction never affects correctness: every file can be rebuilt from the database.
use std::path::Path;
use std::time::{Duration, SystemTime};

fn write(p: &Path, bytes: usize, age_secs: u64) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![7u8; bytes]).unwrap();
    // older files get an older mtime, so LRU order is deterministic
    let t = SystemTime::now() - Duration::from_secs(age_secs);
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(t).unwrap();
}

#[test]
fn evicts_least_recently_used_until_under_limit() {
    let root = std::env::temp_dir().join(format!("cache-bound-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // five 1000-byte entries, oldest first
    let entries: Vec<_> = (0..5).map(|i| root.join(format!("{:02x}", i)).join(format!("h{}", i))).collect();
    for (i, p) in entries.iter().enumerate() { write(p, 1000, (50 - i as u64) * 10); }
    // limit 3000: must drop the two oldest (h0, h1)
    let (removed, freed) = selfdb::vm::enforce_cache_limit(&root, 3000).unwrap();
    assert_eq!((removed, freed), (2, 2000));
    assert!(!entries[0].exists() && !entries[1].exists(), "oldest two evicted");
    assert!(entries[2].exists() && entries[3].exists() && entries[4].exists(), "newest three kept");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn under_limit_is_a_no_op() {
    let root = std::env::temp_dir().join(format!("cache-noop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let p = root.join("ab").join("x");
    write(&p, 500, 1);
    assert_eq!(selfdb::vm::enforce_cache_limit(&root, 10_000).unwrap(), (0, 0));
    assert!(p.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn eviction_keeps_database_readable() {
    // The cache is an accelerator only: after evicting everything, materialize still produces
    // byte-identical files from the database.
    use selfdb::vm;
    let root = std::env::temp_dir().join(format!("cache-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
    vm::vm_add_bytes(&c, "/f", &data, 0o644, 1).unwrap();
    // evict the whole content cache under this test's own root; never touch the user's cache
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    write(&cache.join("aa").join("stale"), 4096, 1000);
    let (removed, _) = selfdb::vm::enforce_cache_limit(&cache, 0).unwrap();
    assert!(removed >= 1);
    let out = root.join("out");
    vm::vm_materialize_tree_with_cache(&c, &out, &cache).unwrap();
    assert_eq!(std::fs::read(out.join("f")).unwrap(), data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn size_parsing_and_env_limit() {
    use selfdb::vm::parse_size_bytes;
    assert_eq!(parse_size_bytes("1024"), Some(1024));
    assert_eq!(parse_size_bytes("512k"), Some(512 * 1024));
    assert_eq!(parse_size_bytes("512M"), Some(512 * 1024 * 1024));
    assert_eq!(parse_size_bytes("2g"), Some(2 * 1024 * 1024 * 1024));
    assert_eq!(parse_size_bytes("2x"), None);
    assert_eq!(parse_size_bytes(""), None);
    assert_eq!(parse_size_bytes("g"), None);
}
