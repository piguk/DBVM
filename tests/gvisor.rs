//! The gVisor backend: the same sandbox contract as bwrap, with a user-space kernel as the boundary.
//!
//! Tests that run sandboxes need `runsc` (`$SELF_RUNSC` or on PATH) and are skipped without it. The
//! compatibility and performance comparisons are `#[ignore]` (they provision a full dev VM): run them with
//! `cargo test --release --test gvisor -- --ignored --nocapture`.
use selfdb::sandbox::{self, gvisor, Isolation, Limits, Spec};
use selfdb::{agent, vm};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn runsc_ok() -> bool {
    if gvisor::runsc().is_none() { eprintln!("skip: runsc not found (set SELF_RUNSC)"); return false; }
    true
}

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn host_spec(script: &str) -> Spec {
    let mut s = Spec::new(Path::new("/"), vec!["/bin/sh".into(), "-c".into(), script.into()]);
    s.root_writable = false;
    s.binds.push(sandbox::Bind { host: std::env::temp_dir(), dest: "/tmp".into(), writable: true });
    s.env.push(("PATH".into(), "/usr/bin:/bin".into()));
    s
}

fn text(o: &sandbox::Outcome) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn the_oci_config_carries_the_spec() {
    let mut s = host_spec("echo hi");
    s.limits = Limits { address_space_mb: 100, cpu_secs: 7, file_size_mb: 3, ..Limits::default() };
    let v: serde_json::Value = serde_json::from_str(&gvisor::oci_config(&s)).expect("valid JSON");
    assert_eq!(v["process"]["args"][2], "echo hi");
    assert_eq!(v["root"]["readonly"], true);
    let env: Vec<&str> = v["process"]["env"].as_array().unwrap().iter().map(|e| e.as_str().unwrap()).collect();
    assert!(env.contains(&"PATH=/usr/bin:/bin"));
    let rl: Vec<(&str, u64)> = v["process"]["rlimits"].as_array().unwrap().iter()
        .map(|r| (r["type"].as_str().unwrap(), r["hard"].as_u64().unwrap())).collect();
    assert!(rl.contains(&("RLIMIT_AS", 100 << 20)) && rl.contains(&("RLIMIT_CPU", 7)) && rl.contains(&("RLIMIT_FSIZE", 3 << 20)), "{:?}", rl);
    let ns: Vec<&str> = v["linux"]["namespaces"].as_array().unwrap().iter().map(|n| n["type"].as_str().unwrap()).collect();
    assert!(ns.contains(&"network") && ns.contains(&"pid"), "{:?}", ns);
    assert_eq!(v["process"]["capabilities"]["bounding"].as_array().unwrap().len(), 0);
    let binds: Vec<&str> = v["mounts"].as_array().unwrap().iter().filter_map(|m| m["destination"].as_str()).collect();
    assert!(binds.contains(&"/tmp"));
}

#[test]
fn gvisor_sandbox_has_no_host_environment_and_no_network() {
    if !runsc_ok() { return; }
    std::env::set_var("GVISOR_TEST_SECRET", "s3cr3t");
    let o = sandbox::run_captured(&host_spec("env; echo IFACES; tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '; dmesg 2>&1 | head -1"),
        Isolation::Gvisor, Duration::from_secs(60), 1 << 20).unwrap();
    let out = text(&o);
    assert_eq!(o.exit_code, 0, "{}", out);
    assert!(!out.contains("s3cr3t"), "host environment leaked: {}", out);
    let ifaces: Vec<&str> = out.lines().skip_while(|l| *l != "IFACES").skip(1).take_while(|l| !l.contains('.')).filter(|l| !l.is_empty()).collect();
    assert!(ifaces.iter().all(|i| *i == "lo"), "only loopback: {}", out);
    // the kernel answering is gVisor's, not the host's
    assert!(out.contains("gVisor"), "runs on the gVisor kernel: {}", out);
}

#[test]
fn gvisor_writes_reach_the_root_directory() {
    // the tree is synced back to the database after a step, so writes must land in the host directory
    if !runsc_ok() { return; }
    let root = std::env::temp_dir().join(format!("gv-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut s = host_spec("echo written > /tmp/gv-out-check");
    let marker = std::env::temp_dir().join("gv-out-check");
    let _ = std::fs::remove_file(&marker);
    s.limits = Limits::default();
    let o = sandbox::run_captured(&s, Isolation::Gvisor, Duration::from_secs(60), 1 << 20).unwrap();
    assert_eq!(o.exit_code, 0, "{}", text(&o));
    assert_eq!(std::fs::read_to_string(&marker).unwrap_or_default(), "written\n", "the write reached the host directory");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn gvisor_memory_limit_and_timeout_apply() {
    if !runsc_ok() { return; }
    let mut s = host_spec("python3 -c 'b=bytearray(400*1024*1024)\nfor i in range(0,len(b),4096): b[i]=1\nprint(\"ALLOCATED\")'");
    s.limits = Limits { memory_mb: 256, ..Limits::default() };
    let o = sandbox::run_captured(&s, Isolation::Gvisor, Duration::from_secs(60), 1 << 20).unwrap();
    assert!(!text(&o).contains("ALLOCATED"), "400 MiB must not fit in a 256 MiB sandbox: {}", text(&o));
    let started = Instant::now();
    let o = sandbox::run_captured(&host_spec("sleep 60"), Isolation::Gvisor, Duration::from_secs(2), 1 << 20).unwrap();
    assert!(o.timed_out && started.elapsed() < Duration::from_secs(15), "timeout stops gVisor: {:?}", started.elapsed());
}

// ---- compatibility and performance on a provisioned dev VM -----------------------------------------

fn node_binary() -> Option<PathBuf> {
    let out = std::process::Command::new("sh").args(["-c", "readlink -f \"$(command -v node)\""]).output().ok()?;
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    p.exists().then_some(p)
}

fn pi_dir() -> Option<PathBuf> {
    ["/home/linuxbrew/.linuxbrew/lib/node_modules/@earendil-works/pi-coding-agent",
     "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent"].iter().map(PathBuf::from).find(|p| p.exists())
}

/// A materialized dev VM (git, python3, gcc, make, node, pi), removed on drop.
struct DevTree { dir: PathBuf }
impl DevTree {
    fn root(&self) -> PathBuf { self.dir.join("root") }
}
impl Drop for DevTree { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.dir); } }

fn dev_tree() -> Option<DevTree> {
    let (node, pi) = (node_binary()?, pi_dir()?);
    let dir = std::env::temp_dir().join(format!("gv-dev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    agent::provision_agent_with(&db, &node, &pi, agent::ToolSet::Dev).unwrap();
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let t = DevTree { dir };
    vm::vm_materialize_tree(&c, &t.root()).unwrap();
    Some(t)
}

fn in_vm(root: &Path, iso: Isolation, script: &str) -> (sandbox::Outcome, Duration) {
    let mut s = Spec::new(root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
    s.env = vec![("PATH".into(), "/usr/bin:/bin".into()), ("HOME".into(), "/tmp".into())];
    s.limits = agent::agent_limits();
    let started = Instant::now();
    let o = sandbox::run_captured(&s, iso, Duration::from_secs(600), 1 << 20).unwrap();
    (o, started.elapsed())
}

const COMPAT: &[(&str, &str, &str)] = &[
    ("git", "cd /tmp && rm -rf r && git init -q r && cd r && echo x > f && git -c user.email=a@b -c user.name=t add f && git -c user.email=a@b -c user.name=t commit -qm m && git log --oneline | wc -l", "1"),
    ("python3", "python3 -c 'import json,sqlite3,ssl,http.client;print(\"PY_OK\")'", "PY_OK"),
    ("gcc", "cd /tmp && printf '#include <stdio.h>\\nint main(){puts(\"C_OK\");return 0;}' > c.c && gcc -O2 c.c -o c && ./c", "C_OK"),
    ("make", "cd /tmp && printf 'all:\\n\\t@echo MAKE_OK\\n' > Makefile && make", "MAKE_OK"),
    ("node", "node -e 'console.log(\"NODE_\"+\"OK\")'", "NODE_OK"),
    ("pi", "node /opt/pi/dist/bundle/cli.js --version", "."),
];

#[test]
#[ignore]
fn dev_tools_work_under_both_isolations() {
    // bwrap always: its syscall filter and limits must not break a real toolchain; gVisor when available
    if !bwrap_ok() { return; }
    let Some(t) = dev_tree() else { eprintln!("skip: node or pi missing"); return; };
    let mut isolations = vec![Isolation::Bwrap];
    if runsc_ok() { isolations.push(Isolation::Gvisor); }
    let mut failures = Vec::new();
    for iso in isolations {
        for (name, script, want) in COMPAT {
            let (o, took) = in_vm(&t.root(), iso, script);
            let out = text(&o);
            let ok = o.exit_code == 0 && out.contains(want);
            eprintln!("{:?} {:8} {} ({:?})", iso, name, if ok { "ok" } else { "FAILED" }, took);
            if !ok { failures.push(format!("{:?} {}: exit={} {}", iso, name, o.exit_code, out.lines().take(5).collect::<Vec<_>>().join(" | "))); }
        }
    }
    assert!(failures.is_empty(), "{:#?}", failures);
}

#[test]
#[ignore]
fn workload_timings_bwrap_vs_gvisor() {
    if !runsc_ok() || !bwrap_ok() { return; }
    let Some(t) = dev_tree() else { eprintln!("skip: node or pi missing"); return; };
    // Each workload is self-contained (it prepares and removes its own data), so runs do not see each
    // other's leftovers. Loops use shell arithmetic: the VM has no `seq`. /usr/include/linux (~2k files)
    // is the file tree to work on.
    const PREP: &str = "rm -rf /tmp/w /tmp/x /tmp/t.tar; cp -r /usr/include/linux /tmp/w";
    let loop20 = |cmd: &str| format!("i=0; while [ $i -lt 20 ]; do {}; i=$((i+1)); done", cmd);
    let workloads: Vec<(&str, String)> = vec![
        ("sandbox start + true", "true".into()),
        ("python3 startup x20", loop20("python3 -c pass")),
        ("node startup x20", loop20("node -e 0")),
        ("gcc compile x20", format!("cd /tmp && printf 'int main(){{return 0;}}' > m.c && {}", loop20("gcc -O2 m.c -o m"))),
        ("copy 2k-file tree", format!("{}; find /tmp/w -type f | wc -l", PREP)),
        ("git add+commit tree", format!("{}; cd /tmp/w && git init -q . && git -c user.email=a@b -c user.name=t add -A && git -c user.email=a@b -c user.name=t commit -qm m", PREP)),
        ("tar create+extract", format!("{}; cd /tmp && tar -cf t.tar w && mkdir x && tar -xf t.tar -C x", PREP)),
        ("stat 2k files x20", format!("{}; {}", PREP, loop20("find /tmp/w -type f > /dev/null"))),
    ];
    // the best of three runs, so a one-off stall on the host does not decide the comparison
    let best = |iso: Isolation, script: &str| -> Duration {
        (0..3).map(|_| {
            let (o, took) = in_vm(&t.root(), iso, script);
            assert_eq!(o.exit_code, 0, "{:?} {}: {}", iso, script, text(&o));
            took
        }).min().unwrap()
    };
    eprintln!("{:24} {:>10} {:>10} {:>7}", "workload", "bwrap", "gvisor", "ratio");
    for (name, script) in &workloads {
        let tb = best(Isolation::Bwrap, script);
        let tg = best(Isolation::Gvisor, script);
        eprintln!("{:24} {:>8}ms {:>8}ms {:>6.1}x", name, tb.as_millis(), tg.as_millis(), tg.as_secs_f64() / tb.as_secs_f64().max(1e-6));
    }
}
