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
