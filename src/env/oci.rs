//! OCI images as templates: an image layout (`skopeo copy docker://IMAGE oci:DIR`, `docker buildx ...
//! --output type=oci`) or a `docker save` archive, as a directory or a tar file. The image's layers are
//! applied in order to a VM database, whiteouts included; its config gives the environment and the working
//! directory commands start in.
//!
//! Every layer is checked against the config's `rootfs.diff_ids` (the digest of its uncompressed tar), and
//! in an image layout every blob against the digest its descriptor names, so an image that is not what its
//! manifest says is refused.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::vm;

/// What a template takes from an image's config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageConfig {
    pub env: Vec<(String, String)>,
    pub workdir: String,
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform { os: String, architecture: String }

#[derive(Deserialize)]
struct Index { manifests: Vec<Descriptor> }

#[derive(Deserialize)]
struct Manifest { config: Descriptor, layers: Vec<Descriptor> }

#[derive(Deserialize)]
struct DockerEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    config: RunConfig,
    rootfs: RootFs,
}

#[derive(Deserialize, Default)]
struct RunConfig {
    #[serde(rename = "Env", default)]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir", default)]
    working_dir: Option<String>,
}

#[derive(Deserialize)]
struct RootFs { diff_ids: Vec<String> }

/// A layer to apply: its file and, in an image layout, the digest of that file.
struct LayerBlob { path: PathBuf, digest: Option<String> }

/// Apply the image at `src` to `conn` (an empty VM database). A tar file is unpacked into `scratch` first.
pub fn import(conn: &Connection, src: &Path, scratch: &Path) -> Result<ImageConfig> {
    let root = if src.is_dir() { src.to_path_buf() } else { unpack(src, scratch)?; scratch.to_path_buf() };
    let (config_bytes, layers) = if root.join("index.json").is_file() {
        image_layout(&root)?
    } else if root.join("manifest.json").is_file() {
        docker_archive(&root)?
    } else {
        return Err(anyhow!("{}: neither an OCI image layout (index.json) nor a docker save archive (manifest.json)", src.display()));
    };
    let config: Config = serde_json::from_slice(&config_bytes).map_err(|e| anyhow!("image config: {}", e))?;
    if config.rootfs.diff_ids.len() != layers.len() {
        return Err(anyhow!("the manifest lists {} layers, the config {}", layers.len(), config.rootfs.diff_ids.len()));
    }
    for (layer, diff_id) in layers.iter().zip(&config.rootfs.diff_ids) { apply_layer(conn, layer, diff_id)?; }
    let env = config.config.env.unwrap_or_default().into_iter()
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let workdir = config.config.working_dir.filter(|w| !w.is_empty()).map(|w| vm::normalize_vm_path(&w)).unwrap_or_else(|| "/".into());
    Ok(ImageConfig { env, workdir })
}

/// Unpack an archive's regular files and directories (an image is nothing else) under `dest`.
fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut a = tar::Archive::new(std::fs::File::open(archive).map_err(|e| anyhow!("open {}: {}", archive.display(), e))?);
    for e in a.entries()? {
        let mut e = e?;
        let t = e.header().entry_type();
        if !(t.is_file() || t.is_dir()) { continue; }
        // unpack_in refuses paths that leave `dest`
        if !e.unpack_in(dest)? { return Err(anyhow!("{}: an entry outside the archive", archive.display())); }
    }
    Ok(())
}

/// A path inside the image directory, from a manifest: relative and without `..`.
fn inside(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if p.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(anyhow!("{:?}: not a path inside the image", rel));
    }
    Ok(root.join(p))
}

fn blob_path(root: &Path, digest: &str) -> Result<PathBuf> {
    let hex = digest.strip_prefix("sha256:").ok_or_else(|| anyhow!("{}: only sha256 digests are supported", digest))?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(anyhow!("{}: not a sha256 digest", digest)); }
    Ok(root.join("blobs/sha256").join(hex))
}

/// `sha256:<hex>`, as descriptors write digests.
fn digest_of(hash: &[u8]) -> String { format!("sha256:{}", hash.iter().map(|b| format!("{:02x}", b)).collect::<String>()) }

/// A small blob (an index, a manifest, a config), checked against its digest.
fn read_blob(root: &Path, digest: &str) -> Result<Vec<u8>> {
    let data = std::fs::read(blob_path(root, digest)?).map_err(|e| anyhow!("blob {}: {}", digest, e))?;
    if digest_of(&Sha256::digest(&data)) != digest { return Err(anyhow!("blob {} does not match its digest", digest)); }
    Ok(data)
}

fn host_arch() -> &'static str {
    match std::env::consts::ARCH { "x86_64" => "amd64", "aarch64" => "arm64", other => other }
}

fn is_index(media_type: &str) -> bool {
    media_type == "application/vnd.oci.image.index.v1+json" || media_type == "application/vnd.docker.distribution.manifest.list.v2+json"
}

/// The manifest an index means on this host: the one for linux on this architecture, or its only one.
/// Entries for platform unknown/unknown (build attestations) are not images.
fn choose(index: Index) -> Result<Descriptor> {
    let mut images: Vec<Descriptor> = index.manifests.into_iter().filter(|d| d.platform.as_ref().map(|p| p.os != "unknown").unwrap_or(true)).collect();
    if images.iter().any(|d| d.platform.is_some()) {
        images.retain(|d| d.platform.as_ref().map(|p| p.os == "linux" && p.architecture == host_arch()).unwrap_or(false));
    }
    match images.len() {
        1 => Ok(images.remove(0)),
        0 => Err(anyhow!("the image has no manifest for linux/{}", host_arch())),
        n => Err(anyhow!("the image index lists {} images; import one", n)),
    }
}

fn image_layout(root: &Path) -> Result<(Vec<u8>, Vec<LayerBlob>)> {
    let index: Index = serde_json::from_slice(&std::fs::read(root.join("index.json"))?).map_err(|e| anyhow!("index.json: {}", e))?;
    let mut desc = choose(index)?;
    // an index may name further indexes (one per tag, or per platform)
    for _ in 0..8 {
        if !is_index(&desc.media_type) { break; }
        let nested: Index = serde_json::from_slice(&read_blob(root, &desc.digest)?).map_err(|e| anyhow!("index {}: {}", desc.digest, e))?;
        desc = choose(nested)?;
    }
    if is_index(&desc.media_type) { return Err(anyhow!("image indexes nested too deep")); }
    let manifest: Manifest = serde_json::from_slice(&read_blob(root, &desc.digest)?).map_err(|e| anyhow!("manifest {}: {}", desc.digest, e))?;
    let config = read_blob(root, &manifest.config.digest)?;
    let layers = manifest.layers.into_iter().map(|l| Ok(LayerBlob { path: blob_path(root, &l.digest)?, digest: Some(l.digest) })).collect::<Result<_>>()?;
    Ok((config, layers))
}

fn docker_archive(root: &Path) -> Result<(Vec<u8>, Vec<LayerBlob>)> {
    let mut entries: Vec<DockerEntry> = serde_json::from_slice(&std::fs::read(root.join("manifest.json"))?).map_err(|e| anyhow!("manifest.json: {}", e))?;
    if entries.len() != 1 { return Err(anyhow!("the archive holds {} images; save one", entries.len())); }
    let e = entries.remove(0);
    let config = std::fs::read(inside(root, &e.config)?)?;
    // the layers are checked against the config's diff_ids as they are read
    let layers = e.layers.iter().map(|l| Ok(LayerBlob { path: inside(root, l)?, digest: None })).collect::<Result<_>>()?;
    Ok((config, layers))
}

/// Hashes what passes through.
struct Hashing<R> { inner: R, hash: Sha256 }

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

/// A layer's tar stream: gzip, zstd or plain, told by its first bytes (blobs have no file names to go by).
fn layer_stream(path: &Path) -> Result<Box<dyn Read>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).map_err(|e| anyhow!("layer {}: {}", path.display(), e))?;
    let mut r = std::io::BufReader::new(file);
    let head = r.fill_buf()?;
    Ok(if head.starts_with(&[0x1f, 0x8b]) { Box::new(flate2::read::MultiGzDecoder::new(r)) }
       else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) { Box::new(zstd::stream::read::Decoder::with_buffer(r)?) }
       else { Box::new(r) })
}

/// Check a layer's digests, reading it to its end, and return its whiteouts: (directories whose lower
/// contents it hides, paths it deletes). Nothing of a blob is parsed before its digest is checked.
fn whiteouts(layer: &LayerBlob, diff_id: &str) -> Result<(Vec<String>, Vec<String>)> {
    if let Some(want) = &layer.digest {
        let mut h = Hashing { inner: std::fs::File::open(&layer.path)?, hash: Sha256::new() };
        std::io::copy(&mut h, &mut std::io::sink())?;
        if digest_of(&h.hash.finalize()) != *want { return Err(anyhow!("layer blob {} does not match its digest", want)); }
    }
    let mut archive = tar::Archive::new(Hashing { inner: layer_stream(&layer.path)?, hash: Sha256::new() });
    let (mut opaque, mut deleted) = (Vec::new(), Vec::new());
    for e in archive.entries()? {
        let e = e?;
        let p = vm::normalize_vm_path(&e.path()?.to_string_lossy());
        let (dir, name) = split(&p);
        if name == ".wh..wh..opq" { opaque.push(dir.to_string()); }
        else if let Some(target) = name.strip_prefix(".wh.") { deleted.push(join(dir, target)); }
    }
    // the digest covers the whole stream, past the end-of-archive blocks the reader stops at
    let mut rest = archive.into_inner();
    std::io::copy(&mut rest, &mut std::io::sink())?;
    if digest_of(&rest.hash.finalize()) != diff_id {
        return Err(anyhow!("layer {} does not match its diff_id {}", layer.path.display(), diff_id));
    }
    Ok((opaque, deleted))
}

fn split(p: &str) -> (&str, &str) {
    match p.rfind('/') { Some(0) => ("/", &p[1..]), Some(i) => (&p[..i], &p[i + 1..]), None => ("/", p) }
}

fn join(dir: &str, name: &str) -> String { if dir == "/" { format!("/{}", name) } else { format!("{}/{}", dir, name) } }

fn delete_below(conn: &Connection, p: &str, with_self: bool) -> Result<()> {
    let (lo, hi) = vm::below_bounds(p);
    conn.prepare_cached("DELETE FROM vm_fs WHERE path > ?1 AND path < ?2")?.execute(params![lo, hi])?;
    if with_self { conn.prepare_cached("DELETE FROM vm_fs WHERE path=?1")?.execute(params![p])?; }
    Ok(())
}

/// Apply one layer: first what it deletes from the layers below, then its entries.
fn apply_layer(conn: &Connection, layer: &LayerBlob, diff_id: &str) -> Result<()> {
    let (opaque, deleted) = whiteouts(layer, diff_id)?;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let r = (|| -> Result<()> {
        for d in &opaque { delete_below(conn, d, false)?; }
        for p in &deleted { delete_below(conn, p, true)?; }
        let mut archive = tar::Archive::new(layer_stream(&layer.path)?);
        for e in archive.entries()? {
            let mut e = e?;
            let p = vm::normalize_vm_path(&e.path()?.to_string_lossy());
            if p == "/" || split(&p).1.starts_with(".wh.") { continue; }
            let h = e.header();
            let (kind, mode, mtime) = (h.entry_type(), h.mode().unwrap_or(0o644) as i64 & 0o7777, h.mtime().unwrap_or(0) as i64);
            let existing: Option<String> = conn.prepare_cached("SELECT kind FROM vm_fs WHERE path=?1")?.query_row(params![p], |r| r.get(0)).optional()?;
            // an entry replaces what the layers below had there; a directory keeps what was below it
            if existing.as_deref() == Some("dir") && !kind.is_dir() { delete_below(conn, &p, false)?; }
            if kind.is_dir() {
                vm::vm_add_dir(conn, &p, mode, mtime)?;
            } else if kind.is_symlink() {
                let Some(t) = e.link_name()? else { continue };
                vm::vm_add_symlink(conn, &p, &t.to_string_lossy())?;
            } else if kind.is_hard_link() {
                // the target is named by its archive path, and is in this layer or one below
                let Some(t) = e.link_name()? else { continue };
                vm::hard_link(conn, &p, &vm::normalize_vm_path(&t.to_string_lossy()), mode, mtime)?;
            } else if kind.is_file() {
                vm::ensure_parent_dirs(conn, &p)?;
                vm::fs_put_file_stream(conn, &p, mode, mtime, &mut e)?;
            }
            // device nodes and fifos: the sandbox provides /dev
        }
        Ok(())
    })();
    match r {
        Ok(()) => { conn.execute_batch("COMMIT;")?; Ok(()) }
        Err(e) => { let _ = conn.execute_batch("ROLLBACK;"); Err(e) }
    }
}
