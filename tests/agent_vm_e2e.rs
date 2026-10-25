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
    let err = agent::run_agent(&db, &opts).expect_err("a refused write must not look like success");
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
