//! Read paths issue several statements (lookup, then chunks). A read must return one whole
//! version of a file: never a short or mixed result, never an error for a file that exists.
use selfdb::vm;
use std::path::PathBuf;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

const SIZE: usize = 150_000;

fn make(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("rc-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/f", &vec![b'a'; SIZE], 0o644, 1).unwrap();
    drop(b);
    let delta = d.join("delta.db");
    drop(vm::Overlay::open(&delta, &base).unwrap());
    (d, base, delta)
}

/// Whole-version check: the content must be one of the versions the writer produced (uniform
/// fill, length from the writer's set). A short, mixed, or error result is a failure.
fn classify(r: anyhow::Result<Vec<u8>>) -> Option<String> {
    match r {
        Ok(b) if !b.is_empty() && b.iter().all(|&x| x == b[0]) && VERSION_LENS.contains(&b.len()) => None,
        Ok(b) => Some(format!("len={} uniform={}", b.len(), b.iter().all(|&x| x == b.first().copied().unwrap_or(0)))),
        Err(e) => Some(format!("err: {}", e)),
    }
}

/// Lengths the writer cycles through. Changing length changes the chunk count, so a read that
/// mixes chunk lists from two versions would be detectable.
const VERSION_LENS: &[usize] = &[SIZE, 70_000, 200_000, 3_000];

fn version(round: u8) -> (u8, usize) {
    let fill = if round % 2 == 0 { b'b' } else { b'c' };
    (fill, VERSION_LENS[(round as usize) % VERSION_LENS.len()])
}

fn run_reader_against_writer<R, W>(reader: R, writer: W) -> (usize, Vec<String>)
where R: FnOnce(Arc<AtomicBool>) -> (usize, Vec<String>) + Send + 'static, W: FnOnce(), {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let h = std::thread::spawn(move || reader(s2));
    writer();
    stop.store(true, Ordering::SeqCst);
    h.join().unwrap()
}

#[test]
fn overlay_read_returns_whole_versions_during_rewrites() {
    let (d, base, delta) = make("ov");
    let (b2, d2) = (base.clone(), delta.clone());
    let (reads, bad) = run_reader_against_writer(
        move |stop| {
            let ov = vm::Overlay::open(&d2, &b2).unwrap();
            let (mut n, mut bad) = (0usize, Vec::new());
            while !stop.load(Ordering::SeqCst) {
                n += 1;
                if let Some(k) = classify(ov.read("/f")) { bad.push(k); }
            }
            (n, bad)
        },
        || {
            // the writer rewrites the file in the delta, alternating content
            let ov = vm::Overlay::open(&delta, &base).unwrap();
            for round in 0..80u8 {
                let (fill, len) = version(round);
                ov.write("/f", &vec![fill; len], 0o644, round as i64).unwrap();
            }
        },
    );
    eprintln!("overlay reads: {} total, {} bad", reads, bad.len());
    assert!(reads > 0);
    assert!(bad.is_empty(), "overlay read returned a non-whole file {} times, e.g. {:?}", bad.len(), bad.iter().take(3).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn vm_cat_returns_whole_versions_during_rewrites() {
    let (d, base, _delta) = make("cat");
    let (b2,) = (base.clone(),);
    let (reads, bad) = run_reader_against_writer(
        move |stop| {
            let r = vm::vm_open(b2.to_str().unwrap()).unwrap();
            let (mut n, mut bad) = (0usize, Vec::new());
            while !stop.load(Ordering::SeqCst) {
                n += 1;
                if let Some(k) = classify(vm::vm_cat(&r, "/f")) { bad.push(k); }
            }
            (n, bad)
        },
        || {
            let w = vm::vm_open(base.to_str().unwrap()).unwrap();
            for round in 0..80u8 {
                let (fill, len) = version(round);
                vm::vm_add_bytes(&w, "/f", &vec![fill; len], 0o644, round as i64).unwrap();
            }
        },
    );
    eprintln!("vm_cat reads: {} total, {} bad", reads, bad.len());
    assert!(reads > 0);
    assert!(bad.is_empty(), "vm_cat returned a non-whole file {} times, e.g. {:?}", bad.len(), bad.iter().take(3).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn many_readers_many_rewrites_no_bad_read() {
    // Several reader threads and a fast writer, for longer: the write-side defects only showed
    // under contention, so reads get the same treatment.
    let (d, base, _delta) = make("many");
    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let (stop, base) = (stop.clone(), base.clone());
        readers.push(std::thread::spawn(move || {
            let r = vm::vm_open(base.to_str().unwrap()).unwrap();
            let (mut n, mut bad) = (0usize, 0usize);
            while !stop.load(Ordering::SeqCst) {
                n += 1;
                if classify(vm::vm_cat(&r, "/f")).is_some() { bad += 1; }
            }
            (n, bad)
        }));
    }
    {
        let w = vm::vm_open(base.to_str().unwrap()).unwrap();
        for round in 0..400u16 {
            let (fill, len) = version((round % 250) as u8);
            vm::vm_add_bytes(&w, "/f", &vec![fill; len], 0o644, round as i64).unwrap();
        }
    }
    stop.store(true, Ordering::SeqCst);
    let (mut total, mut bad) = (0usize, 0usize);
    for h in readers { let (n, b) = h.join().unwrap(); total += n; bad += b; }
    eprintln!("many readers: {} reads, {} bad", total, bad);
    assert!(total > 0);
    assert_eq!(bad, 0, "{} of {} concurrent reads were not whole versions", bad, total);
    let _ = std::fs::remove_dir_all(&d);
}

/// Concurrency smoke test: `list` of a directory being removed and recreated returns only legal
/// results (full Base set, "not a directory", or empty) and never errors otherwise. This does NOT
/// prove the remove/recreate semantics: the bug it was written around (Base children revived by
/// mkdir) is only visible in the short window after mkdir, which the readers rarely hit. The
/// deterministic tests `overlay_list_after_remove_hides_base_children` and
/// `overlay_recreate_then_write_shows_only_new_children` carry that guarantee.
#[test]
fn overlay_list_is_consistent_during_remove_and_recreate() {
    let d = std::env::temp_dir().join(format!("rc-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_dir(&b, "/d", 0o755, 1).unwrap();
    vm::vm_add_bytes(&b, "/d/x", b"x", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/d/y", b"y", 0o644, 1).unwrap();
    drop(b);
    let delta = d.join("delta.db");
    drop(vm::Overlay::open(&delta, &base).unwrap());

    let full = vec!["x".to_string(), "y".to_string()];
    let (b2, d2) = (base.clone(), delta.clone());
    let (reads, bad) = run_reader_against_writer(
        move |stop| {
            let ov = vm::Overlay::open(&d2, &b2).unwrap();
            let (mut n, mut bad) = (0usize, Vec::new());
            while !stop.load(Ordering::SeqCst) {
                n += 1;
                match ov.list("/d") {
                    Ok(names) if names == full => {}
                    Ok(names) if names.is_empty() => {}
                    Ok(names) => bad.push(format!("partial/stale list: {:?}", names)),
                    Err(e) if e.to_string().contains("not a directory") => {}
                    Err(e) => bad.push(format!("err: {}", e)),
                }
            }
            (n, bad)
        },
        || {
            let ov = vm::Overlay::open(&delta, &base).unwrap();
            for _ in 0..60 {
                ov.remove("/d").unwrap();
                ov.mkdir("/d").unwrap();
            }
        },
    );
    eprintln!("overlay list reads: {} total, {} bad", reads, bad.len());
    assert!(reads > 0);
    assert!(bad.is_empty(), "list returned an inconsistent result {} times, e.g. {:?}", bad.len(), bad.iter().take(3).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&d);
}

/// Deterministic: after removing a directory that has Base children, the directory is gone and
/// `list` must not report those children. Also covers the recreate-empty case.
#[test]
fn overlay_list_after_remove_hides_base_children() {
    let d = std::env::temp_dir().join(format!("rc-list-det-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_dir(&b, "/d", 0o755, 1).unwrap();
    vm::vm_add_bytes(&b, "/d/x", b"x", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/d/y", b"y", 0o644, 1).unwrap();
    drop(b);
    let delta = d.join("delta.db");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    assert_eq!(ov.list("/d").unwrap(), vec!["x".to_string(), "y".to_string()], "base listing");
    ov.remove("/d").unwrap();
    assert!(ov.list("/d").is_err(), "removed directory is not a directory: {:?}", ov.list("/d"));
    assert!(!ov.exists("/d/x").unwrap(), "Base child must be hidden once its directory is removed");
    ov.mkdir("/d").unwrap();
    assert_eq!(ov.list("/d").unwrap(), Vec::<String>::new(), "recreated directory is empty");
    let _ = std::fs::remove_dir_all(&d);
}

/// After a remove, recreating the directory and writing one of its former Base names must show
/// exactly that name (the new row), and nothing else from Base.
#[test]
fn overlay_recreate_then_write_shows_only_new_children() {
    let d = std::env::temp_dir().join(format!("rc-recreate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_dir(&b, "/d", 0o755, 1).unwrap();
    vm::vm_add_bytes(&b, "/d/x", b"base-x", 0o644, 1).unwrap();
    vm::vm_add_bytes(&b, "/d/y", b"base-y", 0o644, 1).unwrap();
    drop(b);
    let delta = d.join("delta.db");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.remove("/d").unwrap();
    ov.mkdir("/d").unwrap();
    ov.write("/d/x", b"new-x", 0o644, 2).unwrap();
    assert_eq!(ov.list("/d").unwrap(), vec!["x".to_string()], "only the new x is visible");
    assert_eq!(ov.read("/d/x").unwrap(), b"new-x", "the new row wins over the hidden Base x");
    assert!(!ov.exists("/d/y").unwrap(), "Base y stays hidden");
    let _ = std::fs::remove_dir_all(&d);
}
