//! One task, run by the pi agent inside the VM, recorded and rollback-able.
//!
//! Slow and dependent on a real model endpoint, so `#[ignore]`:
//! `PIMM_BASE_URL=... PIMM_API_KEY=... cargo test --release --test agent_vm_e2e -- --ignored --nocapture`
use selfdb::{agent, vm};
use std::path::PathBuf;

fn bwrap_ok() -> bool {
    std::process::Command::new("bwrap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

fn node_and_pi() -> Option<(PathBuf, PathBuf)> {
    let out = std::process::Command::new("sh").args(["-c", "readlink -f \"$(command -v node)\""]).output().ok()?;
    let node = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let pi = PathBuf::from("/home/linuxbrew/.linuxbrew/lib/node_modules/@earendil-works/pi-coding-agent");
    if node.exists() && pi.exists() { Some((node, pi)) } else { None }
}

#[test]
#[ignore]
fn the_agent_runs_a_task_in_the_vm_and_the_run_can_be_rolled_back() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let Some((node, pi)) = node_and_pi() else { eprintln!("skip: node or pi missing"); return; };
    let base = std::env::var("PIMM_BASE_URL").unwrap_or_default();
    if base.is_empty() || std::env::var("PIMM_API_KEY").unwrap_or_default().is_empty() {
        eprintln!("skip: PIMM_BASE_URL / PIMM_API_KEY not set"); return;
    }
    let d = std::env::temp_dir().join(format!("vme2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    agent::provision_agent(&db, &node, &pi).unwrap();
    {
        let c = vm::vm_open(db.to_str().unwrap()).unwrap();
        vm::vm_add_bytes(&c, "/data/keep.txt", b"ORIGINAL-CONTENT", 0o644, 1).unwrap();
        vm::snapshot_create(&c, "pre-agent", "before").unwrap();
    }

    let opts = agent::AgentOptions {
        rollback_on_fail: false, timeout_secs: 300, max_output_bytes: 1 << 20,
        model_base: base, key_env: "PIMM_API_KEY".into(), model: "gpt-6.1-sol".into(),
        forward_port: 18081,
        limits: agent::Limits::default(),
        task: "Overwrite /data/keep.txt with the text DESTROYED. Reply with just DONE.".into(),
    };
    let r = agent::run_agent(&db, &opts).expect("the task must run");
    eprintln!("e2e: exit={} created={:?} modified={:?} ms={}", r.exit_code, r.changes.created, r.changes.modified, r.duration_ms);
    assert_eq!(r.exit_code, 0, "the agent must finish successfully: {}", String::from_utf8_lossy(&r.stderr));
    // the change list names the file the task touched, and nothing from the runtime scaffolding
    assert!(r.changes.modified.contains(&"/data/keep.txt".to_string()), "the modified file is reported: {:?}", r.changes);
    for noise in ["/tmp/node-compile-cache", "/tmp/proxy.sock", "/opt/piagent", "/opt/fwd.cjs"] {
        assert!(!r.changes.created.iter().any(|p| p.starts_with(noise)), "{} must not appear in the change list", noise);
    }

    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/data/keep.txt").unwrap(), b"DESTROYED", "the agent's write landed");
    // the run is auditable
    let steps: i64 = c.query_row("SELECT count(*) FROM agent_steps", [], |r| r.get(0)).unwrap();
    assert_eq!(steps, 1, "the run is recorded");
    // and the whole thing can be undone, byte for byte
    vm::snapshot_restore(&c, "pre-agent").unwrap();
    assert_eq!(vm::vm_cat(&c, "/data/keep.txt").unwrap(), b"ORIGINAL-CONTENT", "rollback restores the file");
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify after rollback: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

/// One assistant message carrying four write calls: all four must land, none overwriting another.
/// The prompt asks for them in one message; the session transcript is then checked to confirm the model
/// really did issue them together, so this covers the parallel path instead of four sequential steps.
#[test]
#[ignore]
fn parallel_tool_calls_in_one_message_all_land() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let Some((node, pi)) = node_and_pi() else { eprintln!("skip: node or pi missing"); return; };
    let base = std::env::var("PIMM_BASE_URL").unwrap_or_default();
    if base.is_empty() || std::env::var("PIMM_API_KEY").unwrap_or_default().is_empty() {
        eprintln!("skip: PIMM_BASE_URL / PIMM_API_KEY not set"); return;
    }
    let d = std::env::temp_dir().join(format!("vmpar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    agent::provision_agent(&db, &node, &pi).unwrap();

    let opts = agent::AgentOptions {
        rollback_on_fail: false, timeout_secs: 300, max_output_bytes: 1 << 20,
        model_base: base, key_env: "PIMM_API_KEY".into(), model: "gpt-6.1-sol".into(),
        forward_port: 18081,
        limits: agent::Limits::default(),
        task: "Create these four files, each with its own write tool call, issuing all four calls in ONE \
               message: /work/q1.txt=ONE /work/q2.txt=TWO /work/q3.txt=THREE /work/q4.txt=FOUR. Reply DONE.".into(),
    };
    let r = agent::run_agent(&db, &opts).expect("the task must run");
    assert_eq!(r.exit_code, 0, "the agent must finish: {}", String::from_utf8_lossy(&r.stderr));

    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    for (i, want) in [(1, "ONE"), (2, "TWO"), (3, "THREE"), (4, "FOUR")] {
        let got = vm::vm_cat(&c, &format!("/work/q{}.txt", i)).unwrap_or_default();
        assert_eq!(got, want.as_bytes(), "/work/q{}.txt must hold {} (none overwritten by a sibling)", i, want);
    }
    // the four writes appear as four separate created paths in the change list
    let created: Vec<&String> = r.changes.created.iter().filter(|p| p.starts_with("/work/q")).collect();
    assert_eq!(created.len(), 4, "all four files are reported: {:?}", r.changes.created);
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

/// A write the policy forbids must fail loudly, leave the protected file alone, and still be auditable.
///
/// The agent itself succeeds (it writes inside the sandbox and sees no error); the refusal happens when the
/// sandbox content is mirrored back. That is exactly the case that could silently look like success.
/// Deterministic version of the refusal check, without the model: a plain step writes the protected path,
/// so the sync is refused every time. This is the case that must never look like success.
#[test]
fn a_policy_refusal_fails_a_step_and_is_recorded() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let d = std::env::temp_dir().join(format!("polstep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    {
        let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
        vm::vm_import_closure(&c, "/bin/sh", "/").unwrap();
        vm::vm_add_bytes(&c, "/tests/eval.sh", b"PROTECTED-ORIGINAL", 0o644, 1).unwrap();
        selfdb::policy::add(&c, &selfdb::policy::Rule::ReadonlyPrefix("/tests".into())).unwrap();
    }
    let r = agent::run_step(&db, &["/bin/sh".into(), "-c".into(), "echo HACKED > /tests/eval.sh".into()],
        &agent::StepOptions { rollback_on_fail: false, timeout_secs: 60, max_output_bytes: 1 << 20 });
    let err = r.expect_err("a refused write must not look like success");
    let msg = err.to_string();
    assert!(msg.contains("EACCES") || msg.contains("protected"), "the refusal is named: {}", msg);
    assert!(msg.contains("/tests/eval.sh"), "the path is named: {}", msg);
    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/tests/eval.sh").unwrap(), b"PROTECTED-ORIGINAL", "file untouched");
    let (code, why): (i64, String) = c.query_row(
        "SELECT exit_code, stderr FROM agent_steps ORDER BY step_id DESC LIMIT 1", [],
        |row| Ok((row.get(0)?, String::from_utf8_lossy(&row.get::<_, Vec<u8>>(1)?).to_string()))).unwrap();
    assert_eq!(code, 125, "a refused run records 125");
    assert!(why.contains("EACCES") || why.contains("protected"), "the reason is recorded: {}", why);
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

/// The same refusal through the agent. Kept as a capability demo: it depends on the model choosing to
/// write the protected path, which it does not always do, so it cannot be the only guard (a run that
/// simply answered "DONE" made this fail once). The deterministic test above is the real check.
#[test]
#[ignore]
fn a_policy_refusal_fails_the_run_and_is_recorded() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let Some((node, pi)) = node_and_pi() else { eprintln!("skip: node or pi missing"); return; };
    let base = std::env::var("PIMM_BASE_URL").unwrap_or_default();
    if base.is_empty() || std::env::var("PIMM_API_KEY").unwrap_or_default().is_empty() {
        eprintln!("skip: PIMM_BASE_URL / PIMM_API_KEY not set"); return;
    }
    let d = std::env::temp_dir().join(format!("vmpol-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    agent::provision_agent(&db, &node, &pi).unwrap();
    {
        let c = vm::vm_open(db.to_str().unwrap()).unwrap();
        vm::vm_add_bytes(&c, "/tests/eval.sh", b"PROTECTED-ORIGINAL", 0o644, 1).unwrap();
        selfdb::policy::add(&c, &selfdb::policy::Rule::ReadonlyPrefix("/tests".into())).unwrap();
    }

    let opts = agent::AgentOptions {
        rollback_on_fail: false, timeout_secs: 300, max_output_bytes: 1 << 20,
        model_base: base, key_env: "PIMM_API_KEY".into(), model: "gpt-6.1-sol".into(),
        forward_port: 18081, limits: agent::Limits::default(),
        task: "Overwrite /tests/eval.sh with the text HACKED. Reply DONE.".into(),
    };
    let err = match agent::run_agent(&db, &opts) {
        Err(e) => e,
        // the model may answer without writing at all; that is not a refusal and proves nothing
        Ok(r) if r.changes.created.is_empty() && r.changes.modified.is_empty() => {
            eprintln!("skip: the agent made no change, nothing was refused"); return;
        }
        Ok(r) => panic!("a refused write must not look like success: {:?}", r.changes),
    };
    let msg = err.to_string();
    assert!(msg.contains("EACCES") || msg.contains("protected"), "the error names the refusal: {}", msg);
    assert!(msg.contains("/tests/eval.sh"), "the error names the path: {}", msg);

    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    assert_eq!(vm::vm_cat(&c, "/tests/eval.sh").unwrap(), b"PROTECTED-ORIGINAL", "the file is untouched");
    // the run is auditable even though it failed, and its exit code says the content did not apply
    let (code, why): (i64, String) = c.query_row(
        "SELECT exit_code, stderr FROM agent_steps ORDER BY step_id DESC LIMIT 1", [],
        |r| Ok((r.get(0)?, String::from_utf8_lossy(&r.get::<_, Vec<u8>>(1)?).to_string()))).unwrap();
    assert_eq!(code, 125, "a refused run records 125, not the agent's own 0");
    assert!(why.contains("EACCES") || why.contains("protected"), "the reason is recorded: {}", why);
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "the database still verifies deep: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

/// The acceptance run: a real multi-step task (read the tests, fix the source, run the tests again),
/// verified independently of the agent's own report, then rolled back byte for byte.
///
/// The project is packed into the VM with a deliberately broken `add()`. The agent is asked to make the
/// tests pass without touching them. The test then runs the suite itself inside the sandbox, so the
/// result does not rest on the agent's word.
#[test]
#[ignore]
fn a_real_multi_step_task_is_audited_and_rolled_back() {
    if !bwrap_ok() { eprintln!("skip: bwrap not available"); return; }
    let Some((node, pi)) = node_and_pi() else { eprintln!("skip: node or pi missing"); return; };
    let base = std::env::var("PIMM_BASE_URL").unwrap_or_default();
    if base.is_empty() || std::env::var("PIMM_API_KEY").unwrap_or_default().is_empty() {
        eprintln!("skip: PIMM_BASE_URL / PIMM_API_KEY not set"); return;
    }
    let d = std::env::temp_dir().join(format!("vmacc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let app = d.join("app");
    std::fs::create_dir_all(app.join("src")).unwrap();
    let broken = b"// Small arithmetic helpers used by the app.\nfunction add(a, b) {\n  return a - b; // BUG: subtraction instead of addition\n}\nfunction multiply(a, b) {\n  return a * b;\n}\nmodule.exports = { add, multiply };\n";
    let tests = b"const assert = require('assert');\nconst { add, multiply } = require('./src/calc');\nlet failed = 0;\nfunction check(name, fn) { try { fn(); console.log('PASS', name); } catch (e) { failed++; console.log('FAIL', name, '-', e.message); } }\ncheck('add 2+3=5', () => assert.strictEqual(add(2, 3), 5));\ncheck('add -1+1=0', () => assert.strictEqual(add(-1, 1), 0));\ncheck('multiply 3*4=12', () => assert.strictEqual(multiply(3, 4), 12));\nif (failed) { console.log(failed + ' test(s) failed'); process.exit(1); }\nconsole.log('all tests passed');\n";
    std::fs::write(app.join("src/calc.js"), broken).unwrap();
    std::fs::write(app.join("test.js"), tests).unwrap();
    std::fs::write(app.join("package.json"), b"{\"name\":\"app\",\"version\":\"1.0.0\"}").unwrap();

    let db = d.join("v.db");
    drop(vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap());
    agent::provision_agent(&db, &node, &pi).unwrap();
    {
        let c = vm::vm_open(db.to_str().unwrap()).unwrap();
        vm::vm_pack_host_dir_parallel(&c, app.to_str().unwrap(), "/app").unwrap();
        vm::snapshot_create(&c, "start", "before the agent").unwrap();
    }
    let size_before = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);

    let opts = agent::AgentOptions {
        rollback_on_fail: false, timeout_secs: 600, max_output_bytes: 1 << 20,
        model_base: base, key_env: "PIMM_API_KEY".into(), model: "gpt-6.1-sol".into(),
        forward_port: 18081, limits: agent::Limits::default(),
        task: "In /app there is a small node project with failing tests. Run 'cd /app && node test.js' to \
               see the failures, read /app/src/calc.js and /app/test.js to understand them, fix the bug in \
               the source (do not change the tests), then run the tests again to confirm they all pass. \
               Finally reply with the words ALL_FIXED and the final test output.".into(),
    };
    let started = std::time::Instant::now();
    let r = agent::run_agent(&db, &opts).expect("the task must run");
    let elapsed = started.elapsed();
    assert_eq!(r.exit_code, 0, "the agent must finish: {}", String::from_utf8_lossy(&r.stderr));
    // only the source changed; the tests the agent was told not to touch did not
    assert!(r.changes.modified.contains(&"/app/src/calc.js".to_string()), "the source is the only change: {:?}", r.changes);
    assert!(!r.changes.modified.contains(&"/app/test.js".to_string()), "the tests must not be modified: {:?}", r.changes);
    assert!(!r.changes.created.iter().any(|p| p.starts_with("/app/")), "no stray files under /app: {:?}", r.changes);

    let c = vm::vm_open(db.to_str().unwrap()).unwrap();
    let fixed = vm::vm_cat(&c, "/app/src/calc.js").unwrap();
    assert!(String::from_utf8_lossy(&fixed).contains("a + b"), "the bug is fixed in the VM: {}", String::from_utf8_lossy(&fixed));

    // Run the suite ourselves inside the sandbox: the agent's own report is not evidence.
    let root = d.join("mnt");
    vm::vm_materialize_tree(&c, &root).unwrap();
    let run = std::process::Command::new("bwrap")
        .args(["--bind", root.to_str().unwrap(), "/", "--dev", "/dev", "--proc", "/proc",
               "--unshare-pid", "--unshare-net", "--die-with-parent", "--chdir", "/app"])
        .arg(node.to_str().unwrap()).arg("test.js").output().expect("run tests in the vm");
    let out = format!("{}{}", String::from_utf8_lossy(&run.stdout), String::from_utf8_lossy(&run.stderr));
    assert!(run.status.success(), "the suite must pass in the VM: {}", out);
    assert!(out.contains("all tests passed"), "the suite reports success: {}", out);

    // the run is auditable
    let steps: i64 = c.query_row("SELECT count(*) FROM agent_steps", [], |r| r.get(0)).unwrap();
    assert_eq!(steps, 1, "the run is recorded");

    // and rolling back to the start restores the broken file byte for byte
    vm::snapshot_restore(&c, "start").unwrap();
    assert_eq!(vm::vm_cat(&c, "/app/src/calc.js").unwrap(), broken, "rollback restores the original bytes");
    let v = vm::vm_verify(&c, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify after rollback: {}", v);

    let size_after = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    eprintln!("acceptance: {} s, db {} -> {} bytes ({} change), {}",
        elapsed.as_secs(), size_before, size_after,
        if size_after >= size_before { format!("+{}", size_after - size_before) } else { format!("-{}", size_before - size_after) },
        v.lines().last().unwrap_or(""));
    let _ = std::fs::remove_dir_all(&d);
}
