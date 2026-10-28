//! The environment service end to end over HTTP, as a client uses it: an image becomes a template, a
//! build session customizes it and freezes it, a task with read-only tests and a judge runs as several
//! seeded rollouts, a rollout forks, and tenants cannot see each other.
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;
use serde_json::{json, Value};

#[test]
fn templates_tasks_rollouts_judge_fork_and_tenants() {
    if !bwrap_ok() { return; }
    let s = start("flow");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas::default());
    let b = tenant(&s, "b", Quotas { max_sessions: 1, ..Quotas::default() });

    // authentication
    let anon = Client { base: s.base.clone(), key: "sk-wrong".into() };
    assert_eq!(anon.req("GET", "/v1/templates", None).0, 401);
    let templates = a.ok("GET", "/v1/templates", None);
    assert!(templates.as_array().unwrap().iter().any(|t| t["id"] == image.id.as_str()), "the system image is visible: {}", templates);

    // a build session customizes the image; freezing it makes the tenant's template
    let build = a.ok("POST", "/v1/sessions", Some(json!({"template_id": image.id})))["session_id"].as_str().unwrap().to_string();
    let w = a.act(&build, json!({"tool": "write_file", "path": "/app/calc.sh", "content": "echo $((2 - 3))\n"}));
    assert_eq!(w["exit_code"], 0, "{}", w);
    assert_eq!(w["changes"]["created"], json!(["/app", "/app/calc.sh"]));
    a.act(&build, json!({"tool": "write_file", "path": "/tests/judge.sh", "content": JUDGE, "mode": 0o755}));
    let tpl = a.ok("POST", &format!("/v1/sessions/{}/freeze", build), Some(json!({"name": "calc"})));
    let tpl_id = tpl["id"].as_str().unwrap().to_string();
    a.ok("DELETE", &format!("/v1/sessions/{}", build), None);

    // a task: /tests read-only, judged on /app only
    let task = a.ok("POST", "/v1/tasks", Some(json!({
        "template_id": tpl_id, "readonly": ["/tests"],
        "judge": {"cmd": ["/bin/sh", "/tests/judge.sh"], "sees": ["/app"]}
    })))["task_id"].as_str().unwrap().to_string();
    let ids: Vec<String> = a.ok("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": [{"label": "fix", "seed": 7}, {"label": "cheat", "seed": 8}]})))
        ["session_ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2);
    let (fix, cheat) = (&ids[0], &ids[1]);
    assert_eq!(a.bash(fix, "echo $SEED")["output"], "7\n", "the variant's seed is in the environment");
    assert_eq!(a.bash(cheat, "echo $SEED")["output"], "8\n");

    // the fixing rollout edits the source; the cheating one tries the tests and a poison file
    let e = a.act(fix, json!({"tool": "edit_file", "path": "/app/calc.sh", "old": "2 - 3", "new": "2 + 3"}));
    assert_eq!(e["changes"]["modified"], json!(["/app/calc.sh"]), "{}", e);
    let tamper = a.act(cheat, json!({"tool": "write_file", "path": "/tests/judge.sh", "content": "echo '{\"reward\": 1}'"}));
    assert_ne!(tamper["exit_code"], 0, "the tests are read-only: {}", tamper);
    a.bash(cheat, "mkdir -p /etc && echo x > /etc/poison");
    let rf = a.ok("POST", &format!("/v1/sessions/{}/judge", fix), None);
    assert_eq!(rf["reward"], 1.0, "{}", rf);
    let rc = a.ok("POST", &format!("/v1/sessions/{}/judge", cheat), None);
    assert_eq!(rc["reward"], 0.0, "the judge sees /app only, not the poison: {}", rc);

    // fork the fixing rollout; the children start from its state
    let kids: Vec<String> = a.ok("POST", &format!("/v1/sessions/{}/fork", fix), Some(json!({"variants": [{"label": "k1"}, {"label": "k2"}]})))
        ["session_ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    // one action in each child, in one request
    let batch = a.ok("POST", "/v1/actions", Some(json!([
        {"session_id": kids[0], "action": {"tool": "bash", "command": "/bin/sh /app/calc.sh"}},
        {"session_id": kids[1], "action": {"tool": "read_file", "path": "/app/calc.sh"}},
    ])));
    assert_eq!(batch[0]["output"], "5\n", "{}", batch);
    assert_eq!(batch[1]["output"], "echo $((2 + 3))\n");
    let listing = a.act(&kids[0], json!({"tool": "list_dir", "path": "/app"}));
    assert_eq!(listing["entries"][0]["name"], "calc.sh");

    // the trajectory records every action with the variant's label and seed
    let traj = ureq::get(&format!("{}/v1/sessions/{}/trajectory", s.base, fix)).set("Authorization", &format!("Bearer {}", a.key)).call().unwrap().into_string().unwrap();
    let lines: Vec<Value> = traj.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.len() >= 3, "{}", traj);
    assert!(lines.iter().all(|l| l["label"] == "fix" && l["seed"] == 7));
    assert!(lines.iter().any(|l| l.get("judge").is_some()), "the judge's verdict is part of the trajectory");

    // tenants: b sees nothing of a's, and is held to its own quota
    assert_eq!(b.req("GET", &format!("/v1/tasks/{}", task), None).0, 404);
    assert_eq!(b.req("POST", &format!("/v1/sessions/{}/actions", fix), Some(json!({"tool": "bash", "command": "true"}))).0, 404);
    assert_eq!(b.req("POST", "/v1/sessions", Some(json!({"template_id": tpl_id}))).0, 404, "a's template is not b's");
    let b1 = b.ok("POST", "/v1/sessions", Some(json!({"template_id": image.id})))["session_id"].as_str().unwrap().to_string();
    assert_eq!(b.req("POST", "/v1/sessions", Some(json!({"template_id": image.id}))).0, 429, "b may run one session");
    b.ok("DELETE", &format!("/v1/sessions/{}", b1), None);

    // a template in use cannot be deleted; once its rollouts end it can
    assert_eq!(a.req("DELETE", &format!("/v1/templates/{}", tpl_id), None).0, 400);
    for id in ids.iter().chain(kids.iter()) { a.ok("DELETE", &format!("/v1/sessions/{}", id), None); }
    assert_eq!(a.req("DELETE", &format!("/v1/templates/{}", tpl_id), None).0, 400, "a task still uses it");
    a.ok("DELETE", &format!("/v1/tasks/{}", task), None);
    a.ok("DELETE", &format!("/v1/templates/{}", tpl_id), None);
    let usage = a.ok("GET", "/v1/usage", None);
    let fix_row = usage.as_array().unwrap().iter().find(|u| u["id"] == fix.as_str()).unwrap();
    assert!(fix_row["ended_at"].is_i64() && fix_row["steps"].as_i64().unwrap() >= 2, "{}", fix_row);
    // nothing of the deleted template's own layers is left behind
    let tenant_layers = s.dir.join("data/tenants");
    let left: usize = walkdir::WalkDir::new(&tenant_layers).min_depth(3).max_depth(3).into_iter().flatten()
        .filter(|e| e.path().parent().map(|p| p.ends_with("layers")).unwrap_or(false)).count();
    assert_eq!(left, 0, "unused layers are removed");
}

#[test]
fn deep_fork_chains_and_directories_the_agent_locked() {
    if !bwrap_ok() { return; }
    let s = start("chain");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas { max_session_disk_bytes: 1 << 20, ..Quotas::default() });
    let mut sid = a.ok("POST", "/v1/sessions", Some(json!({"template_id": image.id})))["session_id"].as_str().unwrap().to_string();
    // a directory the host user cannot read: forks, freezes and the disk quota must still see into it
    let r = a.bash(&sid, "mkdir -p /w/locked/in && echo x > /w/locked/in/f && echo 0 > /w/g0 && chmod 000 /w/locked/in /w/locked");
    assert_eq!(r["exit_code"], 0, "{}", r);
    // each generation forks the previous one, which then ends: one more layer per generation unless merged
    for g in 1..=20 {
        let kid = a.ok("POST", &format!("/v1/sessions/{}/fork", sid), Some(json!({"variants": [{}]})))["session_ids"][0].as_str().unwrap().to_string();
        a.ok("DELETE", &format!("/v1/sessions/{}", sid), None);
        sid = kid;
        let r = a.bash(&sid, &format!("echo {} > /w/g{}; rm -f /w/g{}", g, g, g - 1));
        assert_eq!(r["changes"]["created"], json!([format!("/w/g{}", g)]), "generation {}: {}", g, r);
    }
    let r = a.bash(&sid, "ls /w; chmod 700 /w/locked /w/locked/in && cat /w/locked/in/f");
    assert_eq!(r["output"], "g20\nlocked\nx\n", "{}", r);
    let mounts = a.bash(&sid, "cat /proc/self/mountinfo")["output"].as_str().unwrap().to_string();
    let root = mounts.lines().find(|l| l.split(' ').nth(4) == Some("/")).unwrap();
    let lowers = root.split("lowerdir=").nth(1).unwrap().split(',').next().unwrap().split(':').count();
    assert!(lowers <= 1 + selfdb::env::store::MAX_LAYERS, "the template's layer and at most MAX_LAYERS of the forks', not one per generation: {}", root);
    // written, then locked: still counted against the disk quota
    let (code, body) = a.req("POST", &format!("/v1/sessions/{}/actions", sid), Some(json!({"tool": "bash", "command": "mkdir /w/h && head -c 2097152 /dev/zero > /w/h/big && chmod 000 /w/h"})));
    assert_eq!(code, 429, "{}", body);
    // /tmp is left out of change lists, not out of the disk the rollout uses
    let sid = a.ok("POST", "/v1/sessions", Some(json!({"template_id": image.id})))["session_id"].as_str().unwrap().to_string();
    let (code, body) = a.req("POST", &format!("/v1/sessions/{}/actions", sid), Some(json!({"tool": "bash", "command": "head -c 2097152 /dev/zero > /tmp/big"})));
    assert_eq!(code, 429, "{}", body);
}

#[test]
fn the_session_limit_holds_under_concurrent_requests() {
    if !bwrap_ok() { return; }
    let s = start("limit");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas { max_sessions: 3, ..Quotas::default() });
    // every request passes a check of the running sessions before any of them is running
    let codes: Vec<u16> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8).map(|_| sc.spawn(|| a.req("POST", "/v1/sessions", Some(json!({"template_id": image.id}))).0)).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(codes.iter().filter(|c| **c == 200).count(), 3, "{:?}", codes);
    assert_eq!(codes.iter().filter(|c| **c == 429).count(), 5, "{:?}", codes);
    let t = s.svc.authenticate(&a.key).unwrap();
    let running = s.svc.usage(&t).unwrap().into_iter().filter(|u| u.ended_at.is_none()).count();
    assert_eq!(running, 3);
}

#[test]
fn a_batch_starts_in_parallel_all_or_nothing() {
    if !bwrap_ok() { return; }
    let s = start("batch");
    let a = tenant(&s, "a", Quotas { max_sessions: 40, ..Quotas::default() });
    let task = calc_task(&s, &a);
    let variants: Vec<serde_json::Value> = (0..32).map(|i| json!({"label": format!("v{}", i), "seed": i})).collect();
    let ids: Vec<String> = a.ok("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": variants})))["session_ids"]
        .as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 32);
    // in the variants' order, each with its own seed
    for (i, id) in ids.iter().enumerate() { assert_eq!(a.bash(id, "echo $SEED")["output"], format!("{}\n", i)); }
    // more than the limit leaves: refused, and nothing of the batch keeps running
    let (code, _) = a.req("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": (0..9).map(|_| json!({})).collect::<Vec<_>>()})));
    assert_eq!(code, 429);
    let t = s.svc.authenticate(&a.key).unwrap();
    assert_eq!(s.svc.usage(&t).unwrap().into_iter().filter(|u| u.ended_at.is_none()).count(), 32);
}

#[test]
fn a_session_outlives_the_thread_that_started_it() {
    // bwrap --die-with-parent follows the thread that forked it; the server's blocking threads exit after
    // 10 s without work, and every session they had started died with them
    if !bwrap_ok() { return; }
    let s = start("idle");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas::default());
    let sid = a.ok("POST", "/v1/sessions", Some(json!({"template_id": image.id})))["session_id"].as_str().unwrap().to_string();
    std::thread::sleep(std::time::Duration::from_secs(13));
    assert_eq!(a.bash(&sid, "echo alive")["output"], "alive\n");
}

#[test]
fn checkpoints_keep_a_step_to_fork_from_later() {
    if !bwrap_ok() { return; }
    let s = start("ckpt");
    let a = tenant(&s, "a", Quotas::default());
    let b = tenant(&s, "b", Quotas::default());
    let task = calc_task(&s, &a);
    let sid = a.ok("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": [{"label": "main", "seed": 5}]})))["session_ids"][0].as_str().unwrap().to_string();
    // step 1: a wrong edit, kept; step 2: the fix, kept
    a.act(&sid, json!({"tool": "edit_file", "path": "/app/calc.sh", "old": "2 - 3", "new": "2 * 3"}));
    let c1 = a.ok("POST", &format!("/v1/sessions/{}/checkpoints", sid), None);
    assert_eq!(c1["step"], 1, "{}", c1);
    a.act(&sid, json!({"tool": "edit_file", "path": "/app/calc.sh", "old": "2 * 3", "new": "2 + 3"}));
    let c2 = a.ok("POST", &format!("/v1/sessions/{}/checkpoints", sid), None);
    let (c1, c2) = (c1["id"].as_str().unwrap().to_string(), c2["id"].as_str().unwrap().to_string());
    let listed: Vec<i64> = a.ok("GET", &format!("/v1/sessions/{}/checkpoints", sid), None).as_array().unwrap().iter().map(|k| k["step"].as_i64().unwrap()).collect();
    assert_eq!(listed, vec![1, 2]);
    let traj = String::from_utf8(s.svc.trajectory(&s.svc.authenticate(&a.key).unwrap(), &sid).unwrap()).unwrap();
    assert!(traj.lines().any(|l| l.contains(&format!("\"checkpoint\":\"{}\"", c1))), "{}", traj);
    // the session ends; its checkpoints stay
    a.ok("DELETE", &format!("/v1/sessions/{}", sid), None);
    let kids: Vec<String> = a.ok("POST", &format!("/v1/checkpoints/{}/fork", c1), Some(json!({"variants": [{"label": "retry"}, {"label": "retry2", "seed": 9}]})))
        ["session_ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(a.bash(&kids[0], "cat /app/calc.sh; echo $SEED")["output"], "echo $((2 * 3))\n5\n", "step 1's files, the session's seed");
    assert_eq!(a.bash(&kids[1], "echo $SEED")["output"], "9\n");
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", kids[0]), None)["reward"], 0.0);
    a.act(&kids[0], json!({"tool": "edit_file", "path": "/app/calc.sh", "old": "2 * 3", "new": "2 + 3"}));
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", kids[0]), None)["reward"], 1.0);
    let from2 = a.ok("POST", &format!("/v1/checkpoints/{}/fork", c2), Some(json!({"variants": [{}]})))["session_ids"][0].as_str().unwrap().to_string();
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", from2), None)["reward"], 1.0, "step 2 had the fix");
    // another tenant sees none of it
    assert_eq!(b.req("POST", &format!("/v1/checkpoints/{}/fork", c1), Some(json!({"variants": [{}]}))).0, 404);
    assert_eq!(b.req("DELETE", &format!("/v1/checkpoints/{}", c1), None).0, 404);
    assert!(b.ok("GET", &format!("/v1/sessions/{}/checkpoints", sid), None).as_array().unwrap().is_empty());
    // a template under checkpoints stays
    let tpl = s.svc.control().checkpoint(&c1).unwrap().unwrap().template_id;
    for k in kids.iter().chain(std::iter::once(&from2)) { a.ok("DELETE", &format!("/v1/sessions/{}", k), None); }
    assert_eq!(a.req("DELETE", &format!("/v1/templates/{}", tpl), None).0, 400);
    // deleted: no more forks from it; the other checkpoint still works
    a.ok("DELETE", &format!("/v1/checkpoints/{}", c1), None);
    assert_eq!(a.req("POST", &format!("/v1/checkpoints/{}/fork", c1), Some(json!({"variants": [{}]}))).0, 404);
    let again = a.ok("POST", &format!("/v1/checkpoints/{}/fork", c2), Some(json!({"variants": [{}]})))["session_ids"][0].as_str().unwrap().to_string();
    assert_eq!(a.bash(&again, "cat /app/calc.sh")["output"], "echo $((2 + 3))\n");
    a.ok("DELETE", &format!("/v1/sessions/{}", again), None);
    assert_eq!(a.req("DELETE", &format!("/v1/tasks/{}", task), None).0, 400, "a checkpoint still uses the task");
    a.ok("DELETE", &format!("/v1/checkpoints/{}", c2), None);
    a.ok("DELETE", &format!("/v1/tasks/{}", task), None);
    a.ok("DELETE", &format!("/v1/templates/{}", tpl), None);
}

#[test]
fn checkpoints_count_against_storage() {
    if !bwrap_ok() { return; }
    let s = start("ckpt-quota");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas { max_storage_bytes: 1 << 20, ..Quotas::default() });
    let sid = a.ok("POST", "/v1/sessions", Some(json!({"template_id": image.id})))["session_id"].as_str().unwrap().to_string();
    a.bash(&sid, "head -c 600000 /dev/zero > /big");
    a.ok("POST", &format!("/v1/sessions/{}/checkpoints", sid), None);
    let (code, body) = a.req("POST", &format!("/v1/sessions/{}/checkpoints", sid), None);
    assert_eq!(code, 429, "two copies exceed 1 MiB: {}", body);
}

#[test]
fn file_tools_carry_any_bytes_and_modes() {
    if !bwrap_ok() { return; }
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let s = start("bytes");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas::default());
    let sid = a.ok("POST", "/v1/sessions", Some(json!({"template_id": image.id})))["session_id"].as_str().unwrap().to_string();
    let data: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let w = a.act(&sid, json!({"tool": "write_file", "path": "/bin.dat", "content": b64.encode(&data), "encoding": "base64", "mode": 0o755}));
    assert_eq!(w["exit_code"], 0, "{}", w);
    let r = a.act(&sid, json!({"tool": "read_file", "path": "/bin.dat", "encoding": "base64"}));
    assert_eq!(b64.decode(r["output"].as_str().unwrap()).unwrap(), data);
    let r = a.act(&sid, json!({"tool": "read_file", "path": "/bin.dat", "offset": 4096, "limit": 100, "encoding": "base64"}));
    assert_eq!(b64.decode(r["output"].as_str().unwrap()).unwrap(), data[4096..4196].to_vec(), "a range");
    let mode = |a: &Client| a.bash(&sid, "ls -l /bin.dat")["output"].as_str().unwrap()[..10].to_string();
    assert_eq!(mode(&a), "-rwxr-xr-x");
    // an existing file: a given mode is applied, none keeps the file's
    a.act(&sid, json!({"tool": "write_file", "path": "/bin.dat", "content": "x", "mode": 0o600}));
    assert_eq!(mode(&a), "-rw-------");
    a.act(&sid, json!({"tool": "write_file", "path": "/bin.dat", "content": "y"}));
    assert_eq!(mode(&a), "-rw-------");
    let bad = a.req("POST", &format!("/v1/sessions/{}/actions", sid), Some(json!({"tool": "write_file", "path": "/x", "content": "not base64!", "encoding": "base64"})));
    assert_eq!(bad.0, 400);
    // what the node reports to Prometheus
    let m = s.svc.metrics_text();
    for want in ["selfdb_actions_total{tool=\"write_file\",outcome=\"ok\"} 3", "selfdb_actions_total{tool=\"write_file\",outcome=\"error\"} 1",
                 "selfdb_action_seconds_bucket{tool=\"read_file\",le=\"+Inf\"} 2", "selfdb_sessions_started_total{kind=\"build\"} 1",
                 "# TYPE selfdb_sessions_running gauge"] {
        assert!(m.contains(want), "{} in:\n{}", want, m);
    }
}

#[test]
fn task_files_are_where_every_rollout_starts() {
    if !bwrap_ok() { return; }
    use base64::Engine;
    let s = start("taskfiles");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas::default());
    let judge = "#!/bin/sh\nif [ \"$(cat /work/answer)\" = 42 ]; then echo '{\"reward\": 1}'; else echo '{\"reward\": 0}'; fi\n";
    let task = a.ok("POST", "/v1/tasks", Some(json!({
        "template_id": image.id, "readonly": ["/tests"],
        "judge": {"cmd": ["/tests/judge.sh"], "sees": ["/work"]},
        "files": [
            {"path": "/work/answer", "content": "41\n"},
            {"path": "/work/data.bin", "content": base64::engine::general_purpose::STANDARD.encode([0u8, 255, 1]), "encoding": "base64"},
            {"path": "/tests/judge.sh", "content": judge, "mode": 0o755},
        ],
    })))["task_id"].as_str().unwrap().to_string();
    let ids: Vec<String> = a.ok("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": [{}, {}]})))["session_ids"]
        .as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    let r = a.bash(&ids[0], "cat /work/answer; ls -l /tests/judge.sh | cut -c1-10; od -An -tu1 /work/data.bin 2>/dev/null || true");
    assert!(r["output"].as_str().unwrap().starts_with("41\n-rwxr-xr-x\n"), "{}", r);
    assert_eq!(r["changes"], json!({"created": [], "modified": [], "deleted": []}), "the task's files are not the agent's changes");
    assert_ne!(a.act(&ids[0], json!({"tool": "write_file", "path": "/tests/judge.sh", "content": "echo '{\"reward\": 1}'"}))["exit_code"], 0, "read-only");
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", ids[0]), None)["reward"], 0.0);
    a.act(&ids[1], json!({"tool": "write_file", "path": "/work/answer", "content": "42"}));
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", ids[1]), None)["reward"], 1.0, "the judge has the task's tests");
    // freezing a rollout keeps the task's files in the template
    let tpl = a.ok("POST", &format!("/v1/sessions/{}/freeze", ids[1]), Some(json!({"name": "solved"})))["id"].as_str().unwrap().to_string();
    let b = a.ok("POST", "/v1/sessions", Some(json!({"template_id": tpl})))["session_id"].as_str().unwrap().to_string();
    assert_eq!(a.bash(&b, "cat /work/answer; test -x /tests/judge.sh && echo X")["output"], "42X\n");
    // a task in use stays; then it goes, and its layer with it
    assert_eq!(a.req("DELETE", &format!("/v1/tasks/{}", task), None).0, 400);
    for id in ids.iter().chain(std::iter::once(&b)) { a.ok("DELETE", &format!("/v1/sessions/{}", id), None); }
    // the frozen template lists the task's layer too: the layer stays until neither uses it
    a.ok("DELETE", &format!("/v1/templates/{}", tpl), None);
    let layers_before = s.svc.control().all_layers().unwrap().len();
    a.ok("DELETE", &format!("/v1/tasks/{}", task), None);
    assert_eq!(s.svc.control().all_layers().unwrap().len(), layers_before - 1, "the task's layer is collected");
    assert_eq!(a.req("GET", &format!("/v1/tasks/{}", task), None).0, 404);
    // bad files are refused
    for bad in [json!([{"path": "rel", "content": ""}]), json!([{"path": "/a", "content": ""}, {"path": "/a/b", "content": ""}]),
                json!([{"path": "/x", "content": "!!", "encoding": "base64"}])] {
        assert_eq!(a.req("POST", "/v1/tasks", Some(json!({"template_id": image.id, "files": bad}))).0, 400, "{}", bad);
    }
}

#[test]
fn task_services_and_egress() {
    if !bwrap_ok() { return; }
    let s = start("services");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas::default());
    // a service that writes its state and keeps running
    let judge = "#!/bin/sh\nif [ -f /srv/state ]; then echo '{\"reward\": 1}'; else echo '{\"reward\": 0}'; fi\n";
    let spec = |judge_services: bool| json!({
        "template_id": image.id, "readonly": ["/tests"],
        "files": [{"path": "/tests/judge.sh", "content": judge, "mode": 0o755}],
        "services": [{"name": "db", "run": "mkdir -p /srv && echo up > /srv/state && exec sleep 3600", "ready": "test -f /srv/state"}],
        "judge": {"cmd": ["/tests/judge.sh"], "sees": ["/work"], "services": judge_services},
    });
    let task = a.ok("POST", "/v1/tasks", Some(spec(true)))["task_id"].as_str().unwrap().to_string();
    let sid = a.ok("POST", "/v1/sessions", Some(json!({"task_id": task})))["session_id"].as_str().unwrap().to_string();
    let r = a.bash(&sid, "cat /srv/state; ls /tmp/selfdb-services");
    assert_eq!(r["output"], "up\ndb.log\n", "{}", r);
    assert_eq!(r["changes"], json!({"created": [], "modified": [], "deleted": []}), "the service's start is not the agent's change");
    // a fork starts the service again over the parent's files
    a.bash(&sid, "rm /srv/state");
    let kid = a.ok("POST", &format!("/v1/sessions/{}/fork", sid), Some(json!({"variants": [{}]})))["session_ids"][0].as_str().unwrap().to_string();
    assert_eq!(a.bash(&kid, "cat /srv/state")["output"], "up\n");
    // the judge starts the services when asked to, else sees only the pristine template and /work
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", sid), None)["reward"], 1.0);
    let plain = a.ok("POST", "/v1/tasks", Some(spec(false)))["task_id"].as_str().unwrap().to_string();
    let p = a.ok("POST", "/v1/sessions", Some(json!({"task_id": plain})))["session_id"].as_str().unwrap().to_string();
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", p), None)["reward"], 0.0);
    // a service that never becomes ready: the rollout does not start, and says why
    let broken = a.ok("POST", "/v1/tasks", Some(json!({"template_id": image.id,
        "services": [{"name": "slow", "run": "echo starting; exec sleep 3600", "ready": "false", "ready_timeout_secs": 1}]})))["task_id"].as_str().unwrap().to_string();
    let (code, body) = a.req("POST", "/v1/sessions", Some(json!({"task_id": broken})));
    assert_eq!(code, 400);
    assert!(body["error"].as_str().unwrap().contains("not ready") && body["error"].as_str().unwrap().contains("starting"), "{}", body);
    assert_eq!(a.req("POST", "/v1/tasks", Some(json!({"template_id": image.id, "services": [{"name": "a b", "run": "x"}]}))).0, 400);

    // a task's rollouts reach the hosts it lists, through the egress proxy (bash's /dev/tcp to the proxy, an
    // absolute-URI request), and nothing else
    let serve = |body: &'static str| {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || for c in l.incoming().flatten() {
            use std::io::{Read, Write};
            let mut c = c;
            let _ = c.read(&mut [0u8; 4096]);
            let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes());
        });
        port
    };
    let (license, other) = (serve("LICENSE OK"), serve("SECRET"));
    let get = |port: u16| format!("exec 3<>/dev/tcp/127.0.0.1/3128 && printf 'GET http://127.0.0.1:{}/ HTTP/1.0\\r\\n\\r\\n' >&3 && cat <&3", port);
    let net = a.ok("POST", "/v1/tasks", Some(json!({"template_id": image.id, "egress": [format!("127.0.0.1:{}", license)]})))["task_id"].as_str().unwrap().to_string();
    let r = a.ok("POST", "/v1/sessions", Some(json!({"task_id": net})))["session_id"].as_str().unwrap().to_string();
    let bash = |sid: &str, cmd: String| a.act(sid, json!({"tool": "bash", "command": format!("bash -c {:?}", cmd)}));
    assert!(bash(&r, get(license))["output"].as_str().unwrap().ends_with("LICENSE OK"), "{}", bash(&r, get(license)));
    let refused = bash(&r, get(other));
    assert!(!refused["output"].as_str().unwrap().contains("SECRET") && refused["output"].as_str().unwrap().contains("403"), "{}", refused);
    assert_ne!(bash(&p, get(license))["exit_code"], 0, "a task without egress has no proxy to reach");
}

#[test]
fn the_operator_bounds_what_a_task_asks_for() {
    use selfdb::env::task::LimitsSpec;
    let max = selfdb::sandbox::Limits { memory_mb: 256, pids: 64, ..selfdb::env::task::DEFAULT_MAX_LIMITS };
    let l = LimitsSpec { memory_mb: Some(4096), pids: Some(10), ..LimitsSpec::default() }.resolve(&max);
    assert_eq!((l.memory_mb, l.pids), (256, 10));
}

#[test]
fn judge_files_are_the_judges_alone() {
    if !bwrap_ok() { return; }
    let s = start("judgefiles");
    let image = s.svc.store().import_image(s.svc.control(), "mini", &rootfs(&s.dir)).unwrap();
    let a = tenant(&s, "a", Quotas::default());
    let check = "#!/bin/sh\nif [ \"$(cat /work/answer)\" = \"$(cat /judge/expected)\" ]; then echo '{\"reward\": 1}'; else echo '{\"reward\": 0}'; fi\n";
    // sees includes /judge on purpose: what the agent writes there must not replace the judge's files
    let task = a.ok("POST", "/v1/tasks", Some(json!({
        "template_id": image.id,
        "judge": {"cmd": ["/judge/check.sh"], "sees": ["/work", "/judge"],
                  "files": [{"path": "/judge/check.sh", "content": check, "mode": 0o755}, {"path": "/judge/expected", "content": "42\n"}]},
    })))["task_id"].as_str().unwrap().to_string();
    let ids: Vec<String> = a.ok("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": [{}, {}]})))["session_ids"]
        .as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert!(a.bash(&ids[0], "ls /judge 2>&1 || echo HIDDEN")["output"].as_str().unwrap().contains("HIDDEN"), "the rollout does not see them");
    a.bash(&ids[0], "mkdir -p /work /judge && echo 41 > /work/answer && echo 41 > /judge/expected");
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", ids[0]), None)["reward"], 0.0, "the agent's /judge/expected does not count");
    a.bash(&ids[1], "mkdir -p /work && echo 42 > /work/answer");
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", ids[1]), None)["reward"], 1.0);
}
