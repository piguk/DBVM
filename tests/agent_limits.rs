//! Resource limits for the sandboxed agent.
//!
//! Each limit is measured inside the sandbox the agent runs in (`sandbox::run_captured`), so the test
//! covers the real mechanism rather than a re-implementation. Node is the payload because that is what
//! runs the agent. The sandbox root is the host's `/`, read-only.
use selfdb::sandbox::{self, Isolation, Limits, Spec};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn node() -> Option<PathBuf> {
    let out = std::process::Command::new("sh").args(["-c", "readlink -f \"$(command -v node)\""]).output().ok()?;
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if p.exists() { Some(p) } else { None }
}

/// Run `script` in a sandbox with `limits`. The sandbox's own timeout is the watchdog: without one, a
/// test of the CPU limit would hang whenever the limit is broken (the payload is an infinite loop).
fn run_limited(limits: Limits, script: &str, watchdog: Duration) -> sandbox::Outcome {
    let mut s = Spec::new(Path::new("/"), vec!["/bin/sh".into(), "-c".into(), script.into()]);
    s.root_writable = false;
    s.binds.push(sandbox::Bind { host: std::env::temp_dir(), dest: "/tmp".into(), writable: true });
    s.env.push(("PATH".into(), "/usr/bin:/bin".into()));
    s.limits = limits;
    sandbox::run_captured(&s, Isolation::Bwrap, watchdog, 1 << 20).expect("run sandbox")
}

fn text(o: &sandbox::Outcome) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn a_normal_command_is_not_affected_by_the_agent_limits() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let o = run_limited(selfdb::agent::agent_limits(), "echo NORMAL_OK", Duration::from_secs(30));
    assert_eq!(o.exit_code, 0, "a trivial command must succeed: {}", text(&o));
    assert!(text(&o).contains("NORMAL_OK"));
}

#[test]
fn memory_allocation_beyond_the_address_space_limit_fails_with_a_plain_error() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let Some(node) = node() else { eprintln!("skip: node not available"); return; };
    let script = format!(r#"{} -e 'const a=[]; try{{for(;;){{a.push(Buffer.alloc(256*1024*1024)); if(a.length>20)break;}} console.log("ALLOCATED",a.length*256);}}catch(e){{console.log("CAUGHT",e.constructor.name);}}'"#, node.display());
    let o = run_limited(selfdb::agent::agent_limits(), &script, Duration::from_secs(60));
    let out = text(&o);
    // a catchable allocation failure, not a killed process: memory pressure must be visible to the agent
    assert!(out.contains("CAUGHT"), "a 20x256MB allocation must fail: code={} out={}", o.exit_code, out);
    assert!(!out.contains("ALLOCATED 5120"), "the limit must actually stop it: {}", out);
}

#[test]
fn a_runaway_cpu_loop_is_killed_by_the_cpu_limit() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let Some(node) = node() else { eprintln!("skip: node not available"); return; };
    let limits = Limits { cpu_secs: 2, ..selfdb::agent::agent_limits() };
    let o = run_limited(limits, &format!("{} -e 'for(;;){{}}'", node.display()), Duration::from_secs(20));
    assert!(!o.timed_out, "the CPU limit must stop the loop, not the watchdog");
    assert!(o.exit_code != 0, "a killed loop does not exit 0: {} {}", o.exit_code, text(&o));
}

#[test]
fn a_write_past_the_file_size_limit_is_refused() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let name = format!("fsz-{}.bin", std::process::id());
    let script = format!("dd if=/dev/zero of=/tmp/{0} bs=1M count=8 2>/dev/null; echo dd_exit=$?; stat -c %s /tmp/{0}", name);
    let o = run_limited(Limits { file_size_mb: 1, ..Limits::default() }, &script, Duration::from_secs(30));
    let out = text(&o);
    let size: u64 = out.lines().find_map(|l| l.trim().parse().ok()).unwrap_or(u64::MAX);
    assert!(size <= 1024 * 1024, "the file must not exceed the limit, got {} bytes: {}", size, out);
    assert!(!out.contains("dd_exit=0"), "the oversized write is refused: {}", out);
    let _ = std::fs::remove_file(std::env::temp_dir().join(name));
}
