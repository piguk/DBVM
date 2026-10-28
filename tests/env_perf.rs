//! Service timings: rollout start, fork, an empty action (in-process and over HTTP), many idle rollouts,
//! and action throughput at 64 concurrent sessions. Runs under a delegated cgroup, as a deployed service
//! does (a systemd scope per sandbox would measure systemd, not the service).
//!
//! `cargo test --release --test env_perf -- --ignored --nocapture`
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;
use selfdb::env::service::{CreateSession, Variant};
use selfdb::env::task::Action;
use std::time::{Duration, Instant};

fn pct(mut v: Vec<Duration>, p: f64) -> Duration {
    v.sort();
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn report(name: &str, v: Vec<Duration>) {
    let mean = v.iter().sum::<Duration>() / v.len() as u32;
    eprintln!("{:34} n={:<4} mean {:>7.2} ms  p50 {:>7.2} ms  p95 {:>7.2} ms", name, v.len(),
              mean.as_secs_f64() * 1e3, pct(v.clone(), 0.5).as_secs_f64() * 1e3, pct(v, 0.95).as_secs_f64() * 1e3);
}

fn mem_used_mb() -> u64 {
    let m = std::fs::read_to_string("/proc/meminfo").unwrap();
    let get = |k: &str| m.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    (get("MemTotal:") - get("MemAvailable:")) / 1024
}

fn bash(cmd: &str) -> Action { Action::Bash { command: cmd.into(), timeout_secs: None, cwd: None } }

#[test]
#[ignore]
fn service_timings() {
    if !bwrap_ok() { return; }
    if std::env::var("SELF_SANDBOX_CGROUP").is_err() {
        let exe = std::env::current_exe().unwrap();
        let script = format!("export SELF_SANDBOX_CGROUP=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup); exec {} --exact service_timings --ignored --nocapture", exe.display());
        let st = std::process::Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes", "--", "sh", "-c", &script]).status().unwrap();
        assert!(st.success());
        return;
    }
    let s = start("perf");
    let c = tenant(&s, "perf", Quotas { max_sessions: 2000, ..Quotas::default() });
    let task = calc_task(&s, &c);
    let t = s.svc.authenticate(&c.key).unwrap();
    let create = || s.svc.create_session(&t, CreateSession { template_id: None, task_id: Some(task.clone()), egress: vec![], label: None, seed: Some(1) }).unwrap();

    // rollout start
    let mut starts = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..50 { let t0 = Instant::now(); ids.push(create()); starts.push(t0.elapsed()); }
    report("start a rollout", starts);

    // an empty action, in-process and over HTTP
    let sid = ids[0].clone();
    let mut steps = Vec::new();
    for _ in 0..200 { let t0 = Instant::now(); s.svc.act(&t, &sid, bash("true")).unwrap(); steps.push(t0.elapsed()); }
    report("empty action (service)", steps);
    let mut http = Vec::new();
    for _ in 0..200 { let t0 = Instant::now(); c.bash(&sid, "true"); http.push(t0.elapsed()); }
    report("empty action (HTTP)", http);

    // fork after a change
    s.svc.act(&t, &sid, bash("echo state > /app/state")).unwrap();
    let mut forks = Vec::new();
    for _ in 0..20 { let t0 = Instant::now(); ids.extend(s.svc.fork(&t, &sid, vec![Variant::default()]).unwrap()); forks.push(t0.elapsed()); }
    report("fork one child", forks);
    for id in ids.drain(..) { s.svc.close(&t, &id, "perf").unwrap(); }

    // 500 idle rollouts at once
    let before = mem_used_mb();
    let t0 = Instant::now();
    let many: Vec<String> = (0..500).map(|_| create()).collect();
    let took = t0.elapsed();
    std::thread::sleep(Duration::from_secs(2));
    let after = mem_used_mb();
    eprintln!("{:34} {:?} total, {:.2} ms each, memory +{} MB ({:.2} MB each)", "500 idle rollouts", took,
              took.as_secs_f64() * 1e3 / 500.0, after.saturating_sub(before), after.saturating_sub(before) as f64 / 500.0);

    // throughput: 64 sessions, 20 actions each, concurrently
    let svc = s.svc.clone();
    let t0 = Instant::now();
    let handles: Vec<_> = many.iter().take(64).cloned().map(|id| {
        let (svc, t) = (svc.clone(), t.clone());
        std::thread::spawn(move || { for _ in 0..20 { svc.act(&t, &id, bash("echo x > /app/out; cat /app/out >/dev/null")).unwrap(); } })
    }).collect();
    for h in handles { h.join().unwrap(); }
    let el = t0.elapsed();
    eprintln!("{:34} {} actions in {:?}: {:.0} actions/s", "64 concurrent sessions", 64 * 20, el, (64 * 20) as f64 / el.as_secs_f64());
    for id in many { s.svc.close(&t, &id, "perf").unwrap(); }
}
