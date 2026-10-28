//! End-to-end agent tasks under bwrap and gVisor: how much of a task's time the sandbox accounts for.
//!
//! The proxy sums the time spent waiting for the model; the rest of the run is the sandbox, the tools and
//! the agent's own work. Every run starts from its own copy of one Base, and the result is checked from
//! outside the agent by running the project's tests again.
//!
//! Needs a model endpoint, node, pi and runsc:
//! `SELF_RUNSC=... PIMM_BASE_URL=... PIMM_API_KEY=... cargo test --release --test e2e_isolation -- --ignored --nocapture`
use selfdb::sandbox::{gvisor, Isolation};
use selfdb::{agent, vm};
use std::path::{Path, PathBuf};

fn node_and_pi() -> Option<(PathBuf, PathBuf)> {
    let out = std::process::Command::new("sh").args(["-c", "readlink -f \"$(command -v node)\""]).output().ok()?;
    let node = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let pi = ["/home/linuxbrew/.linuxbrew/lib/node_modules/@earendil-works/pi-coding-agent",
              "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent"].iter().map(PathBuf::from).find(|p| p.exists())?;
    node.exists().then_some((node, pi))
}

/// Task A: a node project whose `add` subtracts.
fn node_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/calc.js"), "function add(a, b) {\n  return a - b;\n}\nfunction mul(a, b) { return a * b; }\nmodule.exports = { add, mul };\n").unwrap();
    std::fs::write(dir.join("test.js"), "const assert=require('assert');const {add,mul}=require('./src/calc');\n\
assert.strictEqual(add(2,3),5);assert.strictEqual(mul(3,4),12);console.log('ALL PASS');\n").unwrap();
}

/// Task B: a C project of 20 sources, one of which does not compile.
fn c_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let mut objs = Vec::new();
    for i in 0..20 {
        let body = if i == 13 {
            format!("int f{i}(int x) {{ return x + {i} }}\n")  // missing semicolon
        } else {
            format!("int f{i}(int x) {{ return x + {i}; }}\n")
        };
        std::fs::write(dir.join(format!("src/f{}.c", i)), body).unwrap();
        objs.push(format!("src/f{}.o", i));
    }
    let decls: String = (0..20).map(|i| format!("int f{}(int);", i)).collect();
    let calls: String = (0..20).map(|i| format!("s+=f{}(1);", i)).collect();
    std::fs::write(dir.join("src/main.c"), format!("#include <stdio.h>\n{decls}\nint main(){{int s=0;{calls}printf(\"%d\\n\",s);return s==210?0:1;}}\n")).unwrap();
    std::fs::write(dir.join("Makefile"), format!(
        "OBJS={} src/main.o\nall: app\napp: $(OBJS)\n\tgcc -o app $(OBJS)\n%.o: %.c\n\tgcc -O2 -c $< -o $@\ntest: app\n\t./app && echo ALL PASS\n",
        objs.join(" "))).unwrap();
}

struct Task { name: &'static str, dir: &'static str, prompt: &'static str, check: &'static str }

const TASKS: &[Task] = &[
    Task { name: "node bug fix", dir: "/app",
           prompt: "In /app there is a node project with a failing test. Run 'cd /app && node test.js', fix the bug in src/ without changing test.js, and run the test again until it prints ALL PASS.",
           check: "cd /app && node test.js" },
    Task { name: "C build fix", dir: "/proj",
           prompt: "In /proj there is a C project. Run 'cd /proj && make', fix the compile error in the sources without changing the Makefile, then run 'make test' until it prints ALL PASS.",
           check: "cd /proj && make -s test" },
];

#[test]
#[ignore]
fn agent_tasks_under_bwrap_and_gvisor() {
    let Some((node, pi)) = node_and_pi() else { eprintln!("skip: node or pi missing"); return; };
    let base_url = std::env::var("PIMM_BASE_URL").unwrap_or_default();
    if base_url.is_empty() || std::env::var("PIMM_API_KEY").unwrap_or_default().is_empty() { eprintln!("skip: model endpoint not set"); return; }
    if gvisor::runsc().is_none() { eprintln!("skip: runsc not found"); return; }
    let d = std::env::temp_dir().join(format!("e2e-iso-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    let _cleanup = Cleanup(d.clone());

    node_project(&d.join("app"));
    c_project(&d.join("proj"));
    let base = d.join("base.db");
    drop(vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap());
    agent::provision_agent_with(&base, &node, &pi, agent::ToolSet::Dev).unwrap();
    {
        let c = vm::vm_open(base.to_str().unwrap()).unwrap();
        vm::vm_pack_host_dir_parallel(&c, d.join("app").to_str().unwrap(), "/app").unwrap();
        vm::vm_pack_host_dir_parallel(&c, d.join("proj").to_str().unwrap(), "/proj").unwrap();
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }

    eprintln!("{:14} {:7} {:>4} {:>9} {:>9} {:>9} {:>6} {:>5}", "task", "iso", "run", "total", "model", "rest", "rest%", "ok");
    let mut failures = Vec::new();
    for task in TASKS {
        for iso in [Isolation::Bwrap, Isolation::Gvisor] {
            for run in 0..2 {
                let db = d.join(format!("run-{}-{:?}-{}.db", task.dir.trim_start_matches('/'), iso, run));
                std::fs::copy(&base, &db).unwrap();
                let opts = agent::AgentOptions {
                    rollback_on_fail: false, timeout_secs: 600, max_output_bytes: 1 << 20,
                    model_base: base_url.clone(), key_env: "PIMM_API_KEY".into(), model: "gpt-6.1-sol".into(),
                    forward_port: 18081, task: task.prompt.into(),
                    limits: agent::agent_limits(), isolation: iso,
                    base: None, fuse: false, allow_hosts: Vec::new(),
                };
                let r = agent::run_agent(&db, &opts);
                let (total, model) = match &r { Ok(r) => (r.duration_ms, r.model_ms), Err(_) => (0, 0) };
                // checked from outside the agent: the project's own tests, in a fresh sandbox
                let mut check = agent::StepOptions::new(120);
                check.isolation = Isolation::Bwrap;
                let ok = agent::run_step(&db, &["/bin/sh".into(), "-c".into(), task.check.into()], &check)
                    .map(|s| s.exit_code == 0 && String::from_utf8_lossy(&s.stdout).contains("ALL PASS")).unwrap_or(false);
                let rest = total.saturating_sub(model);
                eprintln!("{:14} {:7} {:>4} {:>7}ms {:>7}ms {:>7}ms {:>5.0}% {:>5}", task.name, format!("{:?}", iso), run,
                          total, model, rest, 100.0 * rest as f64 / total.max(1) as f64, ok);
                if !ok { failures.push(format!("{} {:?} run {}: {:?}", task.name, iso, run, r.as_ref().map(|r| String::from_utf8_lossy(&r.stderr).chars().take(400).collect::<String>()).map_err(|e| e.to_string()))); }
                let _ = std::fs::remove_file(&db);
            }
        }
    }
    assert!(failures.is_empty(), "{:#?}", failures);
}
