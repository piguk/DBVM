//! Multi-process overlay writers. The test binary re-executes itself as a child writer when the
//! OV_CHILD environment variable is set, so the child uses the same library code and no extra
//! binary is installed.
use selfdb::vm;
use std::path::{Path, PathBuf};
use std::process::Command;

const ENV_ROLE: &str = "OV_CHILD";

/// Child role: write `count` files under `tag` into the shared Delta, reporting successes on stdout.
fn child_main() {
    let base = PathBuf::from(std::env::var("OV_BASE").unwrap());
    let delta = PathBuf::from(std::env::var("OV_DELTA").unwrap());
    let tag = std::env::var("OV_TAG").unwrap();
    let count: usize = std::env::var("OV_COUNT").unwrap().parse().unwrap();
    let report = PathBuf::from(std::env::var("OV_REPORT").unwrap());
    let mut out = std::fs::File::create(&report).unwrap();
    use std::io::Write;
    let ov = vm::Overlay::open(&delta, &base).expect("open overlay");
    for i in 0..count {
        let p = format!("/{}/f{:04}", tag, i);
        match ov.write(&p, format!("{}-{}", tag, i).as_bytes(), 0o644, 2) {
            Ok(()) => writeln!(out, "OK {}", p).unwrap(),
            Err(e) => writeln!(out, "ERR {} {}", p, e.to_string().replace('\n', " ")).unwrap(),
        }
    }
    out.flush().unwrap();
}

fn spawn_child(base: &Path, delta: &Path, tag: &str, count: usize, report: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_entry", "--test-threads=1"])
        .env(ENV_ROLE, "1")
        .env("OV_BASE", base).env("OV_DELTA", delta)
        .env("OV_TAG", tag).env("OV_COUNT", count.to_string())
        .env("OV_REPORT", report)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn().unwrap()
}

/// Re-entry point: a normal test that does nothing unless it is the child.
#[test]
fn child_entry() {
    if std::env::var(ENV_ROLE).is_ok() { child_main(); }
}

fn make(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("ovmp-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    vm::vm_add_bytes(&b, "/seed", b"seed", 0o644, 1).unwrap();
    drop(b);
    let delta = d.join("delta.db");
    drop(vm::Overlay::open(&delta, &base).unwrap());
    (d, base, delta)
}

#[test]
fn separate_processes_lose_no_acknowledged_write() {
    let (d, base, delta) = make("procs");
    let procs = 4usize;
    let per = 30usize;
    let reports: Vec<PathBuf> = (0..procs).map(|t| d.join(format!("report{}.txt", t))).collect();
    let children: Vec<_> = (0..procs)
        .map(|t| spawn_child(&base, &delta, &format!("p{}", t), per, &reports[t]))
        .collect();
    for (t, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "child {} failed: {}", t, String::from_utf8_lossy(&out.stderr));
    }
    let mut acked: Vec<(String, String)> = Vec::new();
    let mut errors = 0usize;
    for (t, rep) in reports.iter().enumerate() {
        let text = std::fs::read_to_string(rep).unwrap();
        for line in text.lines() {
            if let Some(p) = line.strip_prefix("OK ") {
                let idx: usize = p.rsplit('f').next().unwrap().parse().unwrap();
                acked.push((p.to_string(), format!("p{}-{}", t, idx)));
            } else if line.starts_with("ERR ") {
                errors += 1;
            }
        }
    }
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    for (p, want) in &acked {
        assert_eq!(ov.read(p).unwrap(), want.as_bytes(), "acknowledged write lost: {}", p);
    }
    let c = vm::vm_open(delta.to_str().unwrap()).unwrap();
    let rep = vm::vm_verify(&c, true).unwrap();
    assert!(rep.contains("deep=ok"), "delta inconsistent: {}", rep);
    eprintln!("multi-process writers: {} acknowledged, {} reported errors", acked.len(), errors);
    assert_eq!(acked.len() + errors, procs * per, "every attempt is accounted for");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn base_is_read_only_to_other_processes_during_overlay_use() {
    // Another process holding the base file read-write can write it, but the overlay's read-only
    // attachment never blocks it and never sees a torn state of the base's own rows: reads
    // return either the old or the new value of a base file, never a mix of chunks.
    let (d, base, delta) = make("basero");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    assert_eq!(ov.read("/seed").unwrap(), b"seed");
    // a separate writer rewrites the base's seed file through the VM API (read-write connection)
    let writer = vm::vm_open(base.to_str().unwrap()).unwrap();
    vm::vm_add_bytes(&writer, "/seed", b"newer", 0o644, 9).unwrap();
    drop(writer);
    let got = ov.read("/seed").unwrap();
    assert!(got == b"seed" || got == b"newer", "read must be one whole version, got {:?}", got);
    // and the overlay's own writes still never touch the base file
    let before = std::fs::read(&base).unwrap();
    ov.write("/seed", b"delta", 0o644, 10).unwrap();
    assert_eq!(std::fs::read(&base).unwrap(), before, "overlay writes must not modify the base file");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn base_rewrites_never_show_a_torn_file_to_a_reader() {
    // Rewrite a base file repeatedly from another thread while an overlay reads it. Every read
    // must equal one whole version: all 'a' or all 'b', never a mix.
    let (d, base, delta) = make("basetorn");
    let a = vec![b'a'; 150_000];
    let b_bytes = vec![b'b'; 150_000];
    vm::vm_add_bytes(&vm::vm_open(base.to_str().unwrap()).unwrap(), "/t", &a, 0o644, 1).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (base, stop) = (base.clone(), stop.clone());
        let (a, b_bytes) = (a.clone(), b_bytes.clone());
        std::thread::spawn(move || {
            let w = vm::vm_open(base.to_str().unwrap()).unwrap();
            let mut round = 0u32;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) && round < 120 {
                let v = if round % 2 == 0 { &b_bytes } else { &a };
                let _ = vm::vm_add_bytes(&w, "/t", v, 0o644, round as i64);
                round += 1;
            }
        })
    };
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let mut torn = 0usize;
    let mut reads = 0usize;
    for _ in 0..300 {
        match ov.read("/t") {
            Ok(bytes) => {
                reads += 1;
                if bytes.is_empty() { eprintln!("EMPTY_READ"); torn += 1; continue; }
                let first = bytes[0];
                if !bytes.iter().all(|&x| x == first) { torn += 1; }
            }
            Err(e) => eprintln!("READ_ERR {}", e),
        }
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    writer.join().unwrap();
    eprintln!("base rewrites: {} reads, {} torn", reads, torn);
    assert!(reads > 0, "the overlay must read while the base is being rewritten");
    assert_eq!(torn, 0, "a base reader saw a mixed file ({} of {})", torn, reads);
    let _ = std::fs::remove_dir_all(&d);
}
