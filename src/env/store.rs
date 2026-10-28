//! The node's data directory: layers, templates, run directories and trajectories.
//!
//! ```text
//! <data>/control.db
//! <data>/system/{layers,templates}/<id>      system images, usable by every tenant
//! <data>/tenants/<tenant>/{layers,templates}/<id>
//! <data>/tenants/<tenant>/trajectories/<session>.jsonl
//! <data>/run/                                 rollouts' upper directories (removed with each rollout)
//! ```
//! A template is immutable once created: a database (`base.db`) and an ordered list of layers. A layer is
//! a read-only directory, shared by every rollout and template that lists it.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

use super::control::{random_hex, ControlDb, Layer, Template};
use super::oci::{self, ImageConfig};
use super::transfer;
use crate::vm;

/// Past this many layers a freeze squashes them into one materialized tree: every layer is an overlay lower
/// directory, and lookups cost a little more per layer.
pub const MAX_LAYERS: usize = 16;

pub struct Store { pub root: PathBuf, pub node: String }

/// A layer as another node fetches it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LayerMeta { pub id: String, pub tenant_id: Option<String>, pub bytes: i64, pub digest: String }

/// A template downloaded from another node, not recorded yet (`Store::register`).
pub struct Downloaded { template: Template, db_digest: String, layers: Vec<(Layer, String)> }

/// A template as another node fetches it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TemplateMeta { pub template: Template, pub db_digest: String, pub layers: Vec<LayerMeta> }

/// Total size of the regular files below `path` (a layer: nothing changes it).
pub fn dir_bytes(path: &Path) -> Result<i64> {
    let mut n = 0;
    for e in walkdir::WalkDir::new(path).follow_links(false) {
        let md = e?.metadata()?;
        if md.is_file() { n += md.len() as i64; }
    }
    Ok(n)
}

impl Store {
    pub fn open(root: &Path, node: &str) -> Result<Store> {
        if node.is_empty() || !node.chars().all(|c| c.is_ascii_alphanumeric()) { return Err(anyhow!("node id must be alphanumeric")); }
        for d in ["system/layers", "system/templates", "tenants", "run"] { std::fs::create_dir_all(root.join(d))?; }
        std::fs::set_permissions(root, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        Ok(Store { root: root.to_path_buf(), node: node.to_string() })
    }

    /// A new id. The node prefix lets a future front end route a request to the node that holds the object.
    pub fn new_id(&self, kind: &str) -> Result<String> { Ok(format!("{}-{}-{}", self.node, kind, random_hex(8)?)) }

    fn owner_dir(&self, tenant: Option<&str>) -> PathBuf {
        match tenant { None => self.root.join("system"), Some(t) => self.root.join("tenants").join(t) }
    }

    pub fn run_dir(&self) -> PathBuf { self.root.join("run") }

    pub fn trajectory_path(&self, tenant: &str, session: &str) -> PathBuf {
        self.owner_dir(Some(tenant)).join("trajectories").join(format!("{}.jsonl", session))
    }

    pub fn layer_paths(&self, control: &ControlDb, ids: &[String]) -> Result<Vec<PathBuf>> {
        ids.iter().map(|id| control.layer(id)?.map(|l| l.path).ok_or_else(|| anyhow!("missing layer {}", id))).collect()
    }

    /// A new layer owned by `tenant`, filled by `fill` (given a path that does not exist yet).
    pub fn new_layer(&self, control: &ControlDb, tenant: Option<&str>, fill: &dyn Fn(&Path) -> Result<()>) -> Result<Layer> {
        let id = self.new_id("lyr")?;
        let dir = self.owner_dir(tenant).join("layers");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&id);
        let r = fill(&path).and_then(|()| {
            let layer = Layer { id, tenant_id: tenant.map(String::from), bytes: dir_bytes(&path)?, path: path.clone() };
            control.insert_layer(&layer)?;
            Ok(layer)
        });
        if r.is_err() { let _ = crate::vm::remove_tree(&path); }
        r
    }

    /// A layer that is `db`'s tree, materialized.
    fn materialized_layer(&self, control: &ControlDb, tenant: Option<&str>, db: &rusqlite::Connection) -> Result<Layer> {
        let id = self.new_id("lyr")?;
        let path = self.owner_dir(tenant).join("layers").join(&id);
        vm::vm_materialize_tree_uncached(db, &path)?;
        let layer = Layer { id, tenant_id: tenant.map(String::from), bytes: dir_bytes(&path)?, path };
        control.insert_layer(&layer)?;
        Ok(layer)
    }

    /// A system template from a root filesystem tarball (a distribution's minimal rootfs, `docker export`).
    pub fn import_image(&self, control: &ControlDb, name: &str, rootfs: &Path) -> Result<Template> {
        self.import_system(control, name, &|conn| {
            vm::vm_import_tar_filtered(conn, rootfs.to_str().ok_or_else(|| anyhow!("non-utf8 path"))?, "", None, None)?;
            Ok(ImageConfig { env: Vec::new(), workdir: "/".into() })
        })
    }

    /// A system template from an OCI image (see `oci`): an image layout or a `docker save` archive, as a
    /// directory or a tar file.
    pub fn import_oci(&self, control: &ControlDb, name: &str, image: &Path) -> Result<Template> {
        // an archive is unpacked on the data disk, next to the rollouts, and removed afterwards
        let scratch = vm::WorkDir::create_in(&self.run_dir(), "oci-")?;
        self.import_system(control, name, &|conn| oci::import(conn, image, scratch.path()))
    }

    fn import_system(&self, control: &ControlDb, name: &str, fill: &dyn Fn(&rusqlite::Connection) -> Result<ImageConfig>) -> Result<Template> {
        let id = self.new_id("tpl")?;
        let dir = self.owner_dir(None).join("templates").join(&id);
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("base.db");
        let r = (|| -> Result<Template> {
            let conn = vm::init_vm_db_with_opts(db_path.to_str().ok_or_else(|| anyhow!("non-utf8 path"))?, true, true)?;
            let image = fill(&conn)?;
            // mount points and scratch space every sandbox expects, if the image has none
            for d in ["/tmp", "/root", "/dev", "/proc", "/sys"] {
                if vm::vm_stat(&conn, d).is_err() { vm::vm_add_dir(&conn, d, if d == "/tmp" { 0o1777 } else { 0o755 }, 0)?; }
            }
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            let layer = self.materialized_layer(control, None, &conn)?;
            drop(conn);
            let t = Template { id: id.clone(), tenant_id: None, name: name.into(), parent_id: None, layers: vec![layer.id],
                               bytes: std::fs::metadata(&db_path)?.len() as i64, db_path: db_path.clone(), env: image.env, workdir: image.workdir,
                               created_at: vm::now_secs() };
            control.insert_template(&t)?;
            Ok(t)
        })();
        if r.is_err() { let _ = crate::vm::remove_tree(&dir); }
        r
    }

    /// A template from a session: `base` plus the session's own layers (`extra`, from forks) plus its
    /// upper directory, copied by `snapshot`. The database is `base`'s with each of those layers applied; past MAX_LAYERS the
    /// layers are squashed into one materialized tree.
    pub fn freeze(&self, control: &ControlDb, tenant: &str, name: &str, base: &Template, extra: &[String], snapshot: &dyn Fn(&Path) -> Result<()>) -> Result<Template> {
        let id = self.new_id("tpl")?;
        let dir = self.owner_dir(Some(tenant)).join("templates").join(&id);
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("base.db");
        let mut created_layers: Vec<String> = Vec::new();
        let r = (|| -> Result<Template> {
            let top = self.new_layer(control, Some(tenant), snapshot)?;
            created_layers.push(top.id.clone());
            std::fs::copy(&base.db_path, &db_path)?;
            let conn = vm::vm_open(db_path.to_str().ok_or_else(|| anyhow!("non-utf8 path"))?)?;
            let mut layers = base.layers.clone();
            for lid in extra.iter().chain(std::iter::once(&top.id)) {
                let l = control.layer(lid)?.ok_or_else(|| anyhow!("missing layer {}", lid))?;
                vm::vm_apply_upper(&conn, &l.path)?;
                layers.push(lid.clone());
            }
            if layers.len() > MAX_LAYERS {
                let squashed = self.materialized_layer(control, Some(tenant), &conn)?;
                created_layers.push(squashed.id.clone());
                layers = vec![squashed.id];
            }
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            drop(conn);
            let t = Template { id: id.clone(), tenant_id: Some(tenant.into()), name: name.into(), parent_id: Some(base.id.clone()),
                               layers, bytes: std::fs::metadata(&db_path)?.len() as i64, db_path: db_path.clone(),
                               env: base.env.clone(), workdir: base.workdir.clone(), created_at: vm::now_secs() };
            control.insert_template(&t)?;
            Ok(t)
        })();
        if r.is_err() {
            let _ = crate::vm::remove_tree(&dir);
            for lid in created_layers { if let Ok(Some(l)) = control.layer(&lid) { let _ = crate::vm::remove_tree(&l.path); let _ = control.delete_layer(&lid); } }
        }
        r
    }

    /// What another node needs to fetch template `t`: the template, its database's digest, and its layers
    /// with their digests (each taken now if it never was; layers do not change, so it stays right).
    pub fn template_meta(&self, control: &ControlDb, t: &Template) -> Result<TemplateMeta> {
        let db_digest = match control.template_db_digest(&t.id)? {
            Some(d) => d,
            None => { let d = transfer::file_digest(&t.db_path)?; control.set_template_db_digest(&t.id, &d)?; d }
        };
        let mut layers = Vec::new();
        for id in &t.layers {
            let l = control.layer(id)?.ok_or_else(|| anyhow!("missing layer {}", id))?;
            let digest = match control.layer_digest(id)? {
                Some(d) => d,
                None => { let d = transfer::layer_digest(&l.path)?; control.set_layer_digest(id, &d)?; d }
            };
            layers.push(LayerMeta { id: l.id, tenant_id: l.tenant_id, bytes: l.bytes, digest });
        }
        Ok(TemplateMeta { template: t.clone(), db_digest, layers })
    }

    /// Download another node's template here (same ids): its database and the layers this node lacks, each
    /// checked against its digest. `db` and `layer` open the streams. Nothing is recorded yet (`register`):
    /// the download takes long, and needs no lock; unrecorded directories are swept at the next start.
    pub fn download_template(&self, control: &ControlDb, meta: &TemplateMeta, db: &dyn Fn() -> Result<Box<dyn std::io::Read>>,
                             layer: &dyn Fn(&str) -> Result<Box<dyn std::io::Read>>) -> Result<Downloaded> {
        let mut layers = Vec::new();
        let mut made: Vec<PathBuf> = Vec::new();
        let r = (|| -> Result<Template> {
            for l in &meta.layers {
                if control.layer(&l.id)?.is_some() { continue; }
                let dir = self.owner_dir(l.tenant_id.as_deref()).join("layers");
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(&l.id);
                transfer::read_layer(layer(&l.id)?, &path, &l.digest)?;
                made.push(path.clone());
                layers.push((Layer { id: l.id.clone(), tenant_id: l.tenant_id.clone(), path, bytes: l.bytes }, l.digest.clone()));
            }
            let t = &meta.template;
            let dir = self.owner_dir(t.tenant_id.as_deref()).join("templates").join(&t.id);
            std::fs::create_dir_all(&dir)?;
            made.push(dir.clone());
            let db_path = dir.join("base.db");
            transfer::read_file(db()?, &db_path, &meta.db_digest)?;
            Ok(Template { db_path, ..t.clone() })
        })();
        match r {
            Ok(template) => Ok(Downloaded { template, db_digest: meta.db_digest.clone(), layers }),
            Err(e) => { for p in made { let _ = crate::vm::remove_tree(&p); } Err(e) }
        }
    }

    /// Record a downloaded template (with the layers lock held: its layers must not be collected before the
    /// template lists them).
    pub fn register(&self, control: &ControlDb, d: Downloaded) -> Result<Template> {
        for (l, digest) in &d.layers {
            if control.layer(&l.id)?.is_some() { let _ = crate::vm::remove_tree(&l.path); continue; }
            control.insert_layer(l)?;
            control.set_layer_digest(&l.id, digest)?;
        }
        if let Some(t) = control.template(&d.template.id)? { return Ok(t); }
        control.insert_template(&d.template)?;
        control.set_template_db_digest(&d.template.id, &d.db_digest)?;
        Ok(d.template)
    }

    /// Remove layer and template directories the control database does not list: what an interrupted
    /// download or import left behind.
    pub fn sweep_unrecorded(&self, control: &ControlDb) -> Result<()> {
        let layers: std::collections::HashSet<PathBuf> = control.all_layers()?.into_iter().map(|l| l.path).collect();
        let templates: std::collections::HashSet<PathBuf> = control.all_templates()?.into_iter()
            .filter_map(|t| t.db_path.parent().map(Path::to_path_buf)).collect();
        let mut owners = vec![self.root.join("system")];
        if let Ok(rd) = std::fs::read_dir(self.root.join("tenants")) { owners.extend(rd.flatten().map(|e| e.path())); }
        for o in owners {
            for (sub, known) in [("layers", &layers), ("templates", &templates)] {
                let Ok(rd) = std::fs::read_dir(o.join(sub)) else { continue };
                for e in rd.flatten() {
                    if !known.contains(&e.path()) { crate::vm::remove_tree(&e.path())?; }
                }
            }
        }
        Ok(())
    }

    /// Remove a template's database; its layers go when nothing uses them (`gc_layers`).
    pub fn delete_template(&self, control: &ControlDb, t: &Template) -> Result<()> {
        control.delete_template(&t.id)?;
        if let Some(dir) = t.db_path.parent() { let _ = crate::vm::remove_tree(dir); }
        self.gc_layers(control)
    }

    /// Remove every layer no template and no live session lists.
    pub fn gc_layers(&self, control: &ControlDb) -> Result<()> {
        let used = control.layers_in_use()?;
        for l in control.all_layers()? {
            if used.contains(&l.id) { continue; }
            let _ = crate::vm::remove_tree(&l.path);
            control.delete_layer(&l.id)?;
        }
        Ok(())
    }
}
