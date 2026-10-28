//! A coordinator and two nodes: sessions placed where there is room and the template is, templates fetched
//! (and checked) between nodes, requests routed by id, the session limit held across nodes, deletion checked
//! with every node.
mod env_common;
use env_common::*;
use selfdb::env::cluster::{self, Coordinator, Register};
use selfdb::env::control::Quotas;
use selfdb::env::service::{NodeConfig, Service};
use selfdb::vm;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

struct Cluster { dir: PathBuf, base: String, coord: Arc<Coordinator>, n1: Arc<Service>, n2: Arc<Service>, n1_url: String }
impl Drop for Cluster { fn drop(&mut self) { let _ = vm::remove_tree(&self.dir); } }

/// Serve `router` on a free port; returns its base URL.
fn serve(router: axum::Router) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(l.local_addr().unwrap().port()).unwrap();
            axum::serve(l, router).await.unwrap();
        });
    });
    format!("http://127.0.0.1:{}", rx.recv().unwrap())
}

fn cluster(name: &str) -> Cluster {
    let dir = std::env::temp_dir().join(format!("env-cluster-{}-{}", name, std::process::id()));
    let _ = vm::remove_tree(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let token = selfdb::env::control::random_hex(24).unwrap();
    let node = |id: &str| Service::open_node(&dir.join(id), id, NodeConfig { max_sessions: Some(2), cluster_token: Some(token.clone()), ..Default::default() }).unwrap();
    let (n1, n2) = (node("n1"), node("n2"));
    let (u1, u2) = (serve(selfdb::env::api::router(n1.clone())), serve(selfdb::env::api::router(n2.clone())));
    let coord = Coordinator::open(&dir.join("coord"), "c1", token).unwrap();
    let base = serve(cluster::router(coord.clone()));
    coord.register(Register { id: "n1".into(), url: u1.clone(), max_sessions: Some(2) }).unwrap();
    coord.register(Register { id: "n2".into(), url: u2, max_sessions: Some(2) }).unwrap();
    Cluster { dir, base, coord, n1, n2, n1_url: u1 }
}

fn tenant_on(c: &Cluster, name: &str, q: Quotas) -> Client {
    let id = selfdb::env::control::random_hex(4).unwrap();
    let t = c.coord.control().create_tenant(&format!("c1-ten-{}", id), name, &q).unwrap();
    Client { base: c.base.clone(), key: c.coord.control().create_key(&t.id).unwrap() }
}

fn ids(v: &serde_json::Value) -> Vec<String> { v["session_ids"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect() }

#[test]
fn a_cluster_places_routes_and_moves_templates() {
    if !bwrap_ok() { return; }
    let c = cluster("flow");
    let image = c.n1.store().import_image(c.n1.control(), "mini", &rootfs(&c.dir)).unwrap();
    c.coord.poll_all();
    let a = tenant_on(&c, "a", Quotas { max_sessions: 3, ..Quotas::default() });
    let listed = a.ok("GET", "/v1/templates", None);
    assert!(listed.as_array().unwrap().iter().any(|t| t["id"] == image.id.as_str()), "{}", listed);

    let judge = "#!/bin/sh\nif [ \"$(cat /work/answer)\" = 42 ]; then echo '{\"reward\": 1}'; else echo '{\"reward\": 0}'; fi\n";
    let task = a.ok("POST", "/v1/tasks", Some(json!({
        "template_id": image.id, "readonly": ["/tests"], "judge": {"cmd": ["/tests/judge.sh"], "sees": ["/work"]},
        "files": [{"path": "/work/answer", "content": "41"}, {"path": "/tests/judge.sh", "content": judge, "mode": 0o755}],
    })))["task_id"].as_str().unwrap().to_string();
    assert!(task.starts_with("c1-"), "the coordinator's id: {}", task);

    // three rollouts: n1 holds the template and has room for two, n2 fetches it for the third
    let r = ids(&a.ok("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": [{"seed": 1}, {"seed": 2}, {"seed": 3}]}))));
    let on: Vec<&str> = r.iter().map(|s| s.split('-').next().unwrap()).collect();
    assert_eq!(on, ["n1", "n1", "n2"], "{:?}", r);
    assert!(c.n2.control().template(&image.id).unwrap().is_some(), "n2 has a copy of the template");
    for (i, s) in r.iter().enumerate() { assert_eq!(a.bash(s, "echo $SEED; cat /work/answer")["output"], format!("{}\n41", i + 1)); }
    // the limit counts every node's sessions
    assert_eq!(a.req("POST", "/v1/sessions", Some(json!({"task_id": task}))).0, 429);
    let batch = a.ok("POST", "/v1/actions", Some(json!([
        {"session_id": r[2], "action": {"tool": "write_file", "path": "/work/answer", "content": "42"}},
        {"session_id": r[0], "action": {"tool": "bash", "command": "echo one"}},
    ])));
    assert_eq!(batch[1]["output"], "one\n", "answers in the items' order: {}", batch);
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", r[2]), None)["reward"], 1.0, "judged on n2, with the task's files made there");
    assert_eq!(a.ok("POST", &format!("/v1/sessions/{}/judge", r[0]), None)["reward"], 0.0);
    let traj = a.req("GET", &format!("/v1/sessions/{}/trajectory", r[2]), None);
    assert_eq!(traj.0, 200);

    // a template made on n2 is n2's; a session from it on n1 fetches it from n2
    let tpl = a.ok("POST", &format!("/v1/sessions/{}/freeze", r[2]), Some(json!({"name": "solved"})))["id"].as_str().unwrap().to_string();
    assert!(tpl.starts_with("n2-"), "{}", tpl);
    let ckpt = a.ok("POST", &format!("/v1/sessions/{}/checkpoints", r[2]), None)["id"].as_str().unwrap().to_string();
    for s in &r[..2] { a.ok("DELETE", &format!("/v1/sessions/{}", s), None); }
    let b1 = a.ok("POST", "/v1/sessions", Some(json!({"template_id": tpl})))["session_id"].as_str().unwrap().to_string();
    assert!(b1.starts_with("n2-"), "the node that holds the template first: {}", b1);
    let b = a.ok("POST", "/v1/sessions", Some(json!({"template_id": tpl})))["session_id"].as_str().unwrap().to_string();
    assert!(b.starts_with("n1-"), "n2 is full: {}", b);
    assert!(c.n1.control().template(&tpl).unwrap().is_some(), "n1 fetched it from n2");
    assert_eq!(a.bash(&b, "cat /work/answer")["output"], "42");
    a.ok("DELETE", &format!("/v1/sessions/{}", b1), None);
    // a checkpoint's forks start on its node
    let k = ids(&a.ok("POST", &format!("/v1/checkpoints/{}/fork", ckpt), Some(json!({"variants": [{}]}))));
    assert!(k[0].starts_with("n2-"), "{:?}", k);
    assert_eq!(a.bash(&k[0], "cat /work/answer")["output"], "42");
    let usage = a.ok("GET", "/v1/usage", None);
    assert_eq!(usage.as_array().unwrap().len(), 6, "every node's sessions: {}", usage);

    // deletion asks every node
    assert_eq!(a.req("DELETE", &format!("/v1/tasks/{}", task), None).0, 400, "running sessions use the task");
    for s in [&r[2], &b, &k[0]] { a.ok("DELETE", &format!("/v1/sessions/{}", s), None); }
    assert_eq!(a.req("DELETE", &format!("/v1/tasks/{}", task), None).0, 400, "the checkpoint uses it");
    a.ok("DELETE", &format!("/v1/checkpoints/{}", ckpt), None);
    a.ok("DELETE", &format!("/v1/tasks/{}", task), None);
    a.ok("DELETE", &format!("/v1/templates/{}", tpl), None);
    assert!(c.n1.control().template(&tpl).unwrap().is_none() && c.n2.control().template(&tpl).unwrap().is_none(), "gone from both nodes");

    // the other tenant sees nothing of it; the nodes know no API keys, nor accept a forged tenant header
    let other = tenant_on(&c, "b", Quotas::default());
    assert_eq!(other.req("GET", &format!("/v1/sessions/{}/trajectory", r[2]), None).0, 404);
    let direct = Client { base: c.n1_url.clone(), key: a.key.clone() };
    assert_eq!(direct.req("GET", "/v1/templates", None).0, 401, "an API key at a node");
    let forged = ureq::get(&format!("{}/v1/templates", c.n1_url)).set("Authorization", &format!("Bearer {}", a.key))
        .set("X-Selfdb-Tenant", "c1-ten-x").call();
    assert!(matches!(forged, Err(ureq::Error::Status(401, _))), "a tenant header without the cluster token");
    assert!(matches!(ureq::get(&format!("{}/internal/v1/status", c.n1_url)).set("Authorization", &format!("Bearer {}", a.key)).call(),
                     Err(ureq::Error::Status(401, _))), "the cluster's routes");
}

#[test]
fn a_template_that_does_not_match_its_digest_is_not_fetched() {
    if !bwrap_ok() { return; }
    let c = cluster("digest");
    let image = c.n1.store().import_image(c.n1.control(), "mini", &rootfs(&c.dir)).unwrap();
    // the digest is taken when the template is first asked for; then a file of its layer changes
    let meta = c.n1.template_meta(&image.id).unwrap();
    let layer = c.n1.control().layer(&meta.layers[0].id).unwrap().unwrap();
    std::fs::write(layer.path.join("added-later"), "not in the template\n").unwrap();
    let e = c.n2.fetch_template(&image.id, &c.n1_url).unwrap_err();
    assert!(e.to_string().contains("digest"), "{}", e);
    assert!(c.n2.control().template(&image.id).unwrap().is_none(), "nothing recorded");
    let leftovers: Vec<_> = std::fs::read_dir(c.dir.join("n2/system/layers")).unwrap().flatten().collect();
    assert!(leftovers.is_empty(), "nothing left behind: {:?}", leftovers);
}

#[test]
fn sessions_go_only_where_there_is_room_and_a_node_is_up() {
    if !bwrap_ok() { return; }
    let c = cluster("room");
    let image = c.n1.store().import_image(c.n1.control(), "mini", &rootfs(&c.dir)).unwrap();
    // a node that registered and then went away
    c.coord.register(Register { id: "n9".into(), url: "http://127.0.0.1:1".into(), max_sessions: Some(100) }).unwrap();
    c.coord.poll_all();
    let a = tenant_on(&c, "a", Quotas { max_sessions: 10, ..Quotas::default() });
    let task = a.ok("POST", "/v1/tasks", Some(json!({"template_id": image.id})))["task_id"].as_str().unwrap().to_string();
    let (code, body) = a.req("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": [{}, {}, {}, {}, {}]})));
    assert_eq!(code, 503, "two nodes of two sessions each: {}", body);
    let usage = a.ok("GET", "/v1/usage", None);
    assert!(usage.as_array().unwrap().iter().all(|u| !u["ended_at"].is_null()), "nothing of the batch keeps running: {}", usage);
    let r = ids(&a.ok("POST", "/v1/rollouts", Some(json!({"task_id": task, "variants": [{}, {}, {}, {}]}))));
    assert!(r.iter().all(|s| !s.starts_with("n9-")), "{:?}", r);
    assert_eq!(a.req("GET", "/v1/sessions/n9-ses-0000000000000000/trajectory", None).0, 503, "a node that is down");
    for s in &r { a.ok("DELETE", &format!("/v1/sessions/{}", s), None); }
}
