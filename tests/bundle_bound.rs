//! Bundle cache bound: LRU eviction, but never an entry a running launch is using.
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn entry(dir: &Path, name: &str, bytes: usize, age: u64) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, vec![1u8; bytes]).unwrap();
    let f = OpenOptions::new().write(true).open(&p).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(age)).unwrap();
    p
}

#[test]
fn evicts_lru_bundle_entries_under_limit() {
    let root = std::env::temp_dir().join(format!("bundle-bound-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let old = entry(&root, "aaaa", 1000, 300);
    let mid = entry(&root, "bbbb", 1000, 200);
    let new = entry(&root, "cccc", 1000, 10);
    let (removed, _) = selfdb::bundle_cache::enforce_bundle_limit(&root, 1500).unwrap();
    assert_eq!(removed, 2);
    assert!(!old.exists() && !mid.exists() && new.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn entry_in_use_by_a_launch_is_never_evicted() {
    let root = std::env::temp_dir().join(format!("bundle-inuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let used = entry(&root, "dddd", 1000, 500); // oldest, but in use
    let spare = entry(&root, "eeee", 1000, 100);
    // a running launch holds a shared lock on the entry it links to
    let hold = selfdb::bundle_cache::hold(&used).unwrap();
    let (removed, _) = selfdb::bundle_cache::enforce_bundle_limit(&root, 1000).unwrap();
    assert!(used.exists(), "entry in use must survive eviction");
    assert_eq!(removed, 1, "the unused entry is evicted instead");
    assert!(!spare.exists());
    drop(hold);
    // once released, it is evictable again
    let (removed2, _) = selfdb::bundle_cache::enforce_bundle_limit(&root, 0).unwrap();
    assert_eq!(removed2, 1);
    let _ = std::fs::remove_dir_all(&root);
}
