//! The sandbox every VM command runs in: isolation, syscall filter, resource limits, environment.
//!
//! The sandbox root is the host's `/`, read-only, so the tests can use host tools (sh, python3, unshare)
//! without building a VM first. What is checked is the boundary, not the root's content.
use selfdb::sandbox::{self, Isolation, Limits, Spec};
use std::path::Path;
use std::time::Duration;

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

fn run(spec: &Spec) -> sandbox::Outcome {
    sandbox::run_captured(spec, Isolation::Bwrap, Duration::from_secs(60), 1 << 20).expect("run sandbox")
}

fn text(o: &sandbox::Outcome) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn the_host_environment_does_not_reach_the_sandbox() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    // a credential in the caller's environment must not be visible to sandboxed code
    std::env::set_var("SANDBOX_TEST_SECRET", "s3cr3t");
    let o = run(&host_spec("env; cat /proc/self/environ | tr '\\0' '\\n'"));
    let out = text(&o);
    assert_eq!(o.exit_code, 0, "{}", out);
    assert!(!out.contains("s3cr3t"), "host environment leaked: {}", out);
    assert!(out.contains("PATH=/usr/bin:/bin"), "the spec's environment is set: {}", out);
}

#[test]
fn the_sandbox_has_no_network_and_its_own_namespaces() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let o = run(&host_spec("cat /proc/net/dev | tail -n +3 | cut -d: -f1 | tr -d ' '; echo pid=$$; hostname"));
    let out = text(&o);
    let ifaces: Vec<&str> = out.lines().take_while(|l| !l.starts_with("pid=")).collect();
    assert_eq!(ifaces, vec!["lo"], "only loopback: {}", out);
    assert!(out.contains("pid=1") || out.contains("pid=2"), "own pid namespace: {}", out);
    assert!(out.lines().any(|l| l == "vm"), "own hostname: {}", out);
}

#[test]
fn the_syscall_filter_is_active_and_refuses_namespace_and_mount_calls() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let script = "grep -E '^(Seccomp|NoNewPrivs):' /proc/self/status; \
                  unshare -U true 2>&1; echo unshare_user=$?; \
                  unshare -n true 2>&1; echo unshare_net=$?; \
                  python3 -c 'import ctypes,os; l=ctypes.CDLL(None,use_errno=True); r=l.syscall(165,b\"none\",b\"/tmp\",b\"tmpfs\",0,None); print(\"mount\",r,os.strerror(ctypes.get_errno()))'; \
                  python3 -c 'import ctypes,os; l=ctypes.CDLL(None,use_errno=True); r=l.syscall(425,8,ctypes.create_string_buffer(120)); print(\"io_uring\",r,os.strerror(ctypes.get_errno()))'";
    let o = run(&host_spec(script));
    let out = text(&o);
    assert!(out.contains("Seccomp:\t2"), "a seccomp filter is installed: {}", out);
    assert!(out.contains("unshare_user=1"), "no new user namespace: {}", out);
    assert!(out.contains("unshare_net=1"), "no new network namespace: {}", out);
    assert!(out.contains("mount -1 Operation not permitted"), "mount refused: {}", out);
    assert!(out.contains("io_uring -1 Function not implemented"), "io_uring unavailable: {}", out);
}

#[test]
fn ordinary_work_is_not_affected_by_the_filter() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    // threads (clone without namespace flags), pipes, subprocesses, files
    let script = "python3 -c 'import threading,subprocess; t=[threading.Thread(target=lambda:None) for _ in range(8)]; [x.start() for x in t]; [x.join() for x in t]; print(subprocess.run([\"echo\",\"SUB_OK\"],capture_output=True,text=True).stdout.strip())'; \
                  echo data > /tmp/sandbox-test-file && cat /tmp/sandbox-test-file && rm /tmp/sandbox-test-file";
    let o = run(&host_spec(script));
    let out = text(&o);
    assert_eq!(o.exit_code, 0, "{}", out);
    assert!(out.contains("SUB_OK") && out.contains("data"), "{}", out);
}

#[test]
fn denied_syscall_numbers_match_the_kernel_headers() {
    // the filter names syscalls by number; a wrong number would deny an unrelated syscall
    let header = Path::new("/usr/include/x86_64-linux-gnu/asm/unistd_64.h");
    let Ok(text) = std::fs::read_to_string(header) else { eprintln!("skip: {} missing", header.display()); return; };
    let mut table = std::collections::HashMap::new();
    for l in text.lines() {
        let mut it = l.split_whitespace();
        if let (Some("#define"), Some(name), Some(nr)) = (it.next(), it.next(), it.next()) {
            if let (Some(n), Ok(nr)) = (name.strip_prefix("__NR_"), nr.parse::<u32>()) { table.insert(n.to_string(), nr); }
        }
    }
    for (nr, name, _) in sandbox::seccomp::DENIED {
        assert_eq!(table.get(*name), Some(nr), "{} is not syscall {}", name, nr);
    }
    assert_eq!(table.get("clone"), Some(&56));
}

#[test]
fn memory_beyond_the_cgroup_limit_ends_the_process() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let mut s = host_spec("python3 -c 'b=bytearray(300*1024*1024)\nfor i in range(0,len(b),4096): b[i]=1\nprint(\"ALLOCATED\")'");
    s.limits = Limits { memory_mb: 128, ..Limits::default() };
    let o = run(&s);
    let out = text(&o);
    assert!(!out.contains("ALLOCATED"), "300 MiB must not fit in 128 MiB: {}", out);
    assert_eq!(o.exit_code, 137, "killed by the OOM killer: {}", out);
}

#[test]
fn the_process_count_is_limited_per_sandbox() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let mut s = host_spec("python3 -c '\nimport os,time\nn=0\nfor i in range(200):\n  try:\n    if os.fork()==0:\n      time.sleep(5); os._exit(0)\n    n+=1\n  except OSError: break\nprint(\"forked\",n)'");
    s.limits = Limits { pids: 32, ..Limits::default() };
    let o = run(&s);
    let out = text(&o);
    let n: u32 = out.split_whitespace().skip_while(|w| *w != "forked").nth(1).and_then(|v| v.parse().ok()).unwrap_or(999);
    assert!(n < 32, "fork stopped by pids.max: {}", out);
}

#[test]
fn a_timeout_ends_the_sandbox_including_background_processes() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let marker = std::env::temp_dir().join(format!("sandbox-bg-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    // a background process that would write the marker after the timeout, unless it is killed
    let s = host_spec(&format!("(sleep 3; echo late > /tmp/{}) & sleep 60", marker.file_name().unwrap().to_string_lossy()));
    let started = std::time::Instant::now();
    let o = sandbox::run_captured(&s, Isolation::Bwrap, Duration::from_secs(1), 1 << 20).unwrap();
    assert!(o.timed_out && o.exit_code == sandbox::EXIT_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(10), "stopped promptly: {:?}", started.elapsed());
    std::thread::sleep(Duration::from_secs(4));
    assert!(!marker.exists(), "the background process outlived the sandbox");
}

#[test]
fn limits_without_a_cgroup_source_are_refused_not_ignored() {
    // with neither a delegated cgroup nor a systemd user session, a limited sandbox must not start
    if std::env::var("SANDBOX_NO_CGROUP_CHILD").is_err() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args(["--exact", "limits_without_a_cgroup_source_are_refused_not_ignored", "--nocapture"])
            .env("SANDBOX_NO_CGROUP_CHILD", "1")
            .env("SELF_SANDBOX_CGROUP", "/nonexistent/cgroup")
            .output().unwrap();
        assert!(out.status.success(), "child: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        return;
    }
    let err = sandbox::run_captured(&host_spec("true"), Isolation::Bwrap, Duration::from_secs(10), 1024).expect_err("refused");
    assert!(err.to_string().contains("cannot apply sandbox resource limits"), "{}", err);
}

#[test]
fn a_delegated_cgroup_is_used_when_configured() {
    // A service runs with Delegate=yes and points SELF_SANDBOX_CGROUP at its own cgroup. Reproduced here
    // with a delegated systemd scope around a child copy of this test.
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    if std::env::var("SANDBOX_DELEGATED_CHILD").is_err() {
        let exe = std::env::current_exe().unwrap();
        let script = format!(
            "export SELF_SANDBOX_CGROUP=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup); exec {} --exact a_delegated_cgroup_is_used_when_configured --nocapture",
            exe.display());
        let out = std::process::Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes", "--", "sh", "-c", &script])
            .env("SANDBOX_DELEGATED_CHILD", "1")
            .output();
        let Ok(out) = out else { eprintln!("skip: systemd-run unavailable"); return; };
        assert!(out.status.success(), "child: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        return;
    }
    let mut s = host_spec("cat /proc/self/cgroup; python3 -c 'b=bytearray(200*1024*1024)\nfor i in range(0,len(b),4096): b[i]=1\nprint(\"ALLOCATED\")'");
    s.limits = Limits { memory_mb: 64, ..Limits::default() };
    let o = run(&s);
    let out = text(&o);
    assert_eq!(o.exit_code, 137, "the delegated cgroup's memory.max applies: {}", out);
    // the sandbox cgroup is removed afterwards
    let root = std::env::var("SELF_SANDBOX_CGROUP").unwrap();
    let left: Vec<String> = std::fs::read_dir(&root).unwrap().filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with("sb-")).collect();
    assert!(left.is_empty(), "sandbox cgroups left behind: {:?}", left);
}

#[test]
fn only_standard_descriptors_are_open_in_the_sandbox() {
    // the seccomp program is handed to bwrap as an inherited memfd; the sandboxed process must not see it
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let o = run(&host_spec("ls -l /proc/self/fd/ | tail -n +2 | awk '{print $9, $11}'"));
    let out = text(&o);
    let fds: Vec<&str> = out.lines().filter_map(|l| l.split_whitespace().next()).collect();
    // 0,1,2 plus the descriptor ls itself opens on /proc/self/fd
    assert!(fds.iter().all(|f| matches!(*f, "0" | "1" | "2" | "3")), "unexpected descriptors: {}", out);
    assert!(!out.contains("memfd"), "the seccomp memfd leaked into the sandbox: {}", out);
}
