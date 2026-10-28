//! OCI images as templates: layers applied in order with their whiteouts, compression told by content,
//! the config's environment and working directory, platform choice in an index, docker save archives,
//! archives as tar files, and images that do not match their digests refused.
mod env_common;
use env_common::*;
use selfdb::env::control::Quotas;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

fn digest(data: &[u8]) -> String { format!("sha256:{}", Sha256::digest(data).iter().map(|b| format!("{:02x}", b)).collect::<String>()) }

/// One tar entry of a layer.
enum E<'a> { Dir(&'a str), File(&'a str, &'a str), Link(&'a str, &'a str), Hard(&'a str, &'a str) }

fn layer(entries: &[E]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(1);
        match e {
            E::Dir(p) => { h.set_entry_type(tar::EntryType::Directory); h.set_mode(0o755); h.set_size(0); b.append_data(&mut h, p, &[][..]).unwrap(); }
            E::File(p, c) => { h.set_entry_type(tar::EntryType::Regular); h.set_mode(0o644); h.set_size(c.len() as u64); b.append_data(&mut h, p, c.as_bytes()).unwrap(); }
            E::Link(p, t) => { h.set_entry_type(tar::EntryType::Symlink); h.set_mode(0o777); h.set_size(0); b.append_link(&mut h, p, t).unwrap(); }
            E::Hard(p, t) => { h.set_entry_type(tar::EntryType::Link); h.set_mode(0o644); h.set_size(0); b.append_link(&mut h, p, t).unwrap(); }
        }
    }
    b.into_inner().unwrap()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// The image: the test rootfs (plain), a layer of files (gzip), one that deletes, replaces and makes a
/// directory opaque (zstd), and one with a hard link to a lower file (gzip). Returns (layer tars, their blobs).
fn image_layers(dir: &Path) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let base = std::fs::read(rootfs(dir)).unwrap();
    let add = layer(&[E::Dir("etc"), E::File("etc/keep", "keep\n"), E::File("etc/gone", "gone\n"), E::Dir("srv"), E::File("srv/old", "old\n"),
                      E::Dir("data"), E::File("data/file-then-dir", "f\n"), E::Dir("data/dir-then-file"), E::File("data/dir-then-file/inner", "i\n"),
                      E::File("etc/motd", "hello\n")]);
    let change = layer(&[E::File("etc/.wh.gone", ""), E::Dir("srv"), E::File("srv/.wh..wh..opq", ""), E::File("srv/new", "new\n"),
                         E::Dir("data/file-then-dir"), E::File("data/file-then-dir/x", "x\n"), E::File("data/dir-then-file", "now a file\n"),
                         E::Dir("app"), E::File("app/run.sh", "echo run\n"), E::Link("app/motd", "/etc/motd")]);
    let link = layer(&[E::Dir("app"), E::Hard("app/keep-copy", "etc/keep")]);
    let blobs = vec![base.clone(), gzip(&add), zstd::encode_all(&change[..], 3).unwrap(), gzip(&link)];
    (vec![base, add, change, link], blobs)
}

fn config(tars: &[Vec<u8>], arch: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "architecture": arch, "os": "linux",
        "config": {"Env": ["PATH=/custom/bin:/usr/bin:/bin", "GREETING=hi=there"], "WorkingDir": "/app", "Cmd": ["/bin/sh"]},
        "rootfs": {"type": "layers", "diff_ids": tars.iter().map(|t| digest(t)).collect::<Vec<_>>()},
    })).unwrap()
}

fn put_blob(root: &Path, data: &[u8]) -> Value {
    let d = digest(data);
    std::fs::write(root.join("blobs/sha256").join(d.strip_prefix("sha256:").unwrap()), data).unwrap();
    json!({"digest": d, "size": data.len()})
}

fn host_arch() -> &'static str { match std::env::consts::ARCH { "x86_64" => "amd64", "aarch64" => "arm64", a => a } }

/// An image layout whose index lists this host's image, one for another architecture and an attestation.
fn image_layout(dir: &Path) -> PathBuf {
    let root = dir.join("layout");
    std::fs::create_dir_all(root.join("blobs/sha256")).unwrap();
    std::fs::write(root.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
    let (tars, blobs) = image_layers(dir);
    let manifest = |arch: &str| -> Value {
        let mut cfg = put_blob(&root, &config(&tars, arch));
        cfg["mediaType"] = json!("application/vnd.oci.image.config.v1+json");
        let layers: Vec<Value> = blobs.iter().map(|b| put_blob(&root, b)).collect();
        let m = serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "config": cfg, "layers": layers})).unwrap();
        put_blob(&root, &m)
    };
    let other = if host_arch() == "amd64" { "arm64" } else { "amd64" };
    let mut entries = Vec::new();
    for (arch, os) in [(host_arch(), "linux"), (other, "linux"), ("unknown", "unknown")] {
        let mut d = manifest(arch);
        d["mediaType"] = json!("application/vnd.oci.image.manifest.v1+json");
        d["platform"] = json!({"architecture": arch, "os": os});
        entries.push(d);
    }
    // an index naming an index, as `docker buildx --output type=oci` writes it
    let mut inner = put_blob(&root, &serde_json::to_vec(&json!({"schemaVersion": 2, "manifests": entries})).unwrap());
    inner["mediaType"] = json!("application/vnd.oci.image.index.v1+json");
    std::fs::write(root.join("index.json"), serde_json::to_vec(&json!({"schemaVersion": 2, "manifests": [inner]})).unwrap()).unwrap();
    root
}

fn docker_archive(dir: &Path) -> PathBuf {
    let root = dir.join("docker");
    std::fs::create_dir_all(&root).unwrap();
    let (tars, _) = image_layers(dir);
    let mut names = Vec::new();
    for (i, t) in tars.iter().enumerate() {
        std::fs::create_dir_all(root.join(format!("l{}", i))).unwrap();
        std::fs::write(root.join(format!("l{}/layer.tar", i)), t).unwrap();
        names.push(format!("l{}/layer.tar", i));
    }
    std::fs::write(root.join("config.json"), config(&tars, host_arch())).unwrap();
    std::fs::write(root.join("manifest.json"), serde_json::to_vec(&json!([{"Config": "config.json", "RepoTags": ["t:1"], "Layers": names}])).unwrap()).unwrap();
    root
}

fn tar_of(dir: &Path, out: &Path) -> PathBuf {
    let mut b = tar::Builder::new(std::fs::File::create(out).unwrap());
    b.append_dir_all(".", dir).unwrap();
    b.finish().unwrap();
    out.to_path_buf()
}

/// What a session over the template sees.
fn check(s: &Server, c: &Client, template_id: &str) {
    let sid = c.ok("POST", "/v1/sessions", Some(json!({"template_id": template_id})))["session_id"].as_str().unwrap().to_string();
    let r = c.bash(&sid, "pwd; echo \"$PATH $GREETING\"; cat /etc/keep /srv/new /data/file-then-dir/x /data/dir-then-file /app/keep-copy /app/motd; \
                          ls /srv; for p in /etc/gone /srv/old /data/dir-then-file/inner; do test -e $p && echo LEFT $p; done; /bin/sh /app/run.sh");
    assert_eq!(r["exit_code"], 0, "{}", r);
    assert_eq!(r["output"], "/app\n/custom/bin:/usr/bin:/bin hi=there\nkeep\nnew\nx\nnow a file\nkeep\nhello\nnew\nrun\n", "{}", r);
    c.ok("DELETE", &format!("/v1/sessions/{}", sid), None);
    let _ = s;
}

#[test]
fn images_become_templates() {
    if !bwrap_ok() { return; }
    let s = start("oci");
    let c = tenant(&s, "a", Quotas::default());
    let layout = image_layout(&s.dir);
    for (name, path) in [("layout", layout.clone()), ("layout-tar", tar_of(&layout, &s.dir.join("layout.tar"))),
                         ("docker", docker_archive(&s.dir)), ("docker-tar", tar_of(&s.dir.join("docker"), &s.dir.join("docker.tar")))] {
        let t = s.svc.store().import_oci(s.svc.control(), name, &path).unwrap_or_else(|e| panic!("{}: {:#}", name, e));
        assert_eq!(t.workdir, "/app");
        check(&s, &c, &t.id);
    }
    // nothing of the unpacked archives is left in the run directory
    let left: Vec<_> = std::fs::read_dir(s.dir.join("data/run")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "{:?}", left);
}

#[test]
fn images_that_do_not_match_their_digests_are_refused() {
    let s = start("oci-bad");
    let layout = image_layout(&s.dir);
    // flip a byte of the gzip layer in the image layout: the blob no longer matches its descriptor
    let index: Value = serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let blob = |d: &Value| layout.join("blobs/sha256").join(d["digest"].as_str().unwrap().strip_prefix("sha256:").unwrap());
    let inner: Value = serde_json::from_slice(&std::fs::read(blob(&index["manifests"][0])).unwrap()).unwrap();
    let manifest: Value = serde_json::from_slice(&std::fs::read(blob(&inner["manifests"][0])).unwrap()).unwrap();
    let target = blob(&manifest["layers"][3]);
    let mut data = std::fs::read(&target).unwrap();
    let n = data.len();
    data[n / 2] ^= 0xff;
    std::fs::write(&target, &data).unwrap();
    let e = s.svc.store().import_oci(s.svc.control(), "bad", &layout).unwrap_err();
    assert!(format!("{:#}", e).contains("does not match its digest"), "{:#}", e);
    // a docker archive whose layer is not the one its config names
    let docker = docker_archive(&s.dir);
    std::fs::write(docker.join("l1/layer.tar"), layer(&[E::File("evil", "x")])).unwrap();
    let e = s.svc.store().import_oci(s.svc.control(), "bad2", &docker).unwrap_err();
    assert!(format!("{:#}", e).contains("diff_id"), "{:#}", e);
    assert!(s.svc.control().templates_for("nobody").unwrap().is_empty(), "nothing was imported");
}
