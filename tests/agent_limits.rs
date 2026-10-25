//! Resource limits for the sandboxed agent.
//!
//! Each limit is measured inside a `bwrap` sandbox with the same prelude `vm-agent` uses, so the test
//! covers the real mechanism rather than a re-implementation. Node is the payload because that is what
//! runs the agent.
use std::path::PathBuf;
use selfdb::agent::Limits;

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

fn node() -> Option<PathBuf> {
    let out = std::process::Command::new("sh").args(["-c", "readlink -f \"$(command -v node)\""]).output().ok()?;
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if p.exists() { Some(p) } else { None }
}

/// Run `script` in a sandbox with the given limits applied the way vm-agent applies them.
///
/// The child is killed by a watchdog after `watchdog` seconds. Without it a test of the CPU limit would
/// hang forever whenever the limit is broken: the payload is an infinite loop, and the very mechanism
/// under test is what would stop it. A test must not depend on the code it is checking.
fn run_limited_watchdog(limits: &Limits, script: &str, watchdog: std::time::Duration) -> (i32, String, bool) {
    let prelude = format!("{}. /dev/stdin", selfdb::agent::limits_prelude(limits));
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{} <<'EOS'\n{}\nEOS", prelude, script))
        .env_clear().env("PATH", "/usr/bin:/bin")
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .spawn().expect("spawn");
    let deadline = std::time::Instant::now() + watchdog;
    let mut killed = false;
    loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            let out = child.wait_with_output().expect("wait");
            return (st.code().unwrap_or(-1),
                    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
                    killed);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            killed = true;
            return (-1, String::from("<watchdog fired>"), killed);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn run_limited(limits: &Limits, script: &str) -> (i32, String) {
    let (code, out, _) = run_limited_watchdog(limits, script, std::time::Duration::from_secs(30));
    (code, out)
}

#[test]
fn a_normal_command_is_not_affected_by_the_default_limits() {
    let (code, out) = run_limited(&Limits::default(), "echo NORMAL_OK");
    assert_eq!(code, 0, "a trivial command must succeed: {}", out);
    assert!(out.contains("NORMAL_OK"), "{}", out);
}

#[test]
fn memory_allocation_beyond_the_limit_fails_with_a_plain_error() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let Some(node) = node() else { eprintln!("skip: node not available"); return; };
    let script = format!(r#"{node_exec} -e 'const a=[]; try{{for(;;){{a.push(Buffer.alloc(256*1024*1024)); if(a.length>20)break;}} console.log("ALLOCATED",a.length*256);}}catch(e){{console.log("CAUGHT",e.constructor.name);}}'"#, node_exec = node.display());
    let limits = Limits { address_space_mb: 2048, cpu_secs: 0, file_size_mb: 1024, processes: 0 };
    let (code, out) = run_limited(&limits, &script);
    // a catchable allocation failure, not a killed process: memory pressure must be visible to the agent
    assert!(out.contains("CAUGHT"), "a 20x256MB allocation must fail: code={} out={}", code, out);
    assert!(!out.contains("ALLOCATED 5120"), "the limit must actually stop it: {}", out);
}

#[test]
fn a_runaway_cpu_loop_is_killed_by_the_cpu_limit() {
    let Some(node) = node() else { eprintln!("skip: node not available"); return; };
    let limits = Limits { address_space_mb: 2048, cpu_secs: 2, file_size_mb: 0, processes: 0 };
    let start = std::time::Instant::now();
    // a 20 s watchdog: the CPU limit must fire long before it, and the test cannot hang if it does not
    let (code, out, killed_by_watchdog) = run_limited_watchdog(&limits, &format!("{} -e 'for(;;){{}}'", node.display()),
        std::time::Duration::from_secs(20));
    let elapsed = start.elapsed();
    assert!(!killed_by_watchdog, "the CPU limit must stop the loop, not the watchdog ({} s)", elapsed.as_secs());
    assert!(elapsed.as_secs() < 20, "the loop must be stopped quickly, took {:?}", elapsed);
    assert!(code != 0, "a killed loop does not exit 0: {} {}", code, out);
}

#[test]
fn a_write_past_the_file_size_limit_is_refused() {
    let script = "dd if=/dev/zero of=/tmp/fsz.bin bs=1M count=8 2>/dev/null; echo dd_exit=$?; stat -c %s /tmp/fsz.bin 2>/dev/null";
    let limits = Limits { address_space_mb: 0, cpu_secs: 0, file_size_mb: 1, processes: 0 };
    let (_, out) = run_limited(&limits, script);
    // the file may not grow past the limit; dd reports failure
    let size_line = out.lines().find(|l| l.trim().parse::<u64>().is_ok()).unwrap_or("0");
    let size: u64 = size_line.trim().parse().unwrap_or(0);
    assert!(size <= 1024 * 1024 + 1, "the file must not exceed the limit, got {} bytes: {}", size, out);
    assert!(out.contains("dd_exit=153") || size < 8 * 1024 * 1024, "the oversized write is refused: {}", out);
    let _ = std::fs::remove_file("/tmp/fsz.bin");
}
