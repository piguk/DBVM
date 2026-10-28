//! Moving templates between nodes: a layer directory as a byte stream, and back.
//!
//! The stream is deterministic (entries in path order, only what a layer's content is: kinds, modes, mtimes,
//! `user.*` xattrs such as the overlay's opaque marker, whiteouts, links, hard links), so the sha256 of a
//! layer's stream, taken once when the layer is made, checks every later copy of it.
//!
//! ```text
//! "SELFLYR1\n", then per entry: u32 (big endian) length, a JSON header, and for a regular file `n` bytes;
//! the last header has kind "end".
//! header: {"p": path, "k": "d"|"f"|"l"|"w"|"h"|"p"|"s", "m": mode, "t": mtime_ns, "n": size, "l": target, "x": {xattr: base64}}
//! ```
//! `w` is a whiteout, `h` a hard link to an earlier entry (`l`), `p` a fifo, `s` a socket.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path};

const MAGIC: &[u8] = b"SELFLYR1\n";
/// A header longer than this is not one (a corrupt or hostile stream).
const MAX_HEADER: u32 = 1 << 20;

#[derive(Serialize, Deserialize, Default)]
struct Header {
    p: String,
    k: String,
    #[serde(default)]
    m: u32,
    #[serde(default)]
    t: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    n: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    l: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    x: BTreeMap<String, String>,
}

fn is_zero(n: &u64) -> bool { *n == 0 }

/// `sha256:<hex>` of what was hashed.
pub fn digest_hex(h: Sha256) -> String { format!("sha256:{}", h.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>()) }

/// Writes through to `inner` and hashes what it writes.
struct Hashing<W> { inner: W, hash: Sha256 }

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}

fn cstr(p: &Path) -> Result<std::ffi::CString> { Ok(std::ffi::CString::new(p.as_os_str().as_encoded_bytes())?) }

fn user_xattrs(p: &Path) -> Result<BTreeMap<String, String>> {
    let c = cstr(p)?;
    let size = unsafe { libc::llistxattr(c.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 { return Err(anyhow!("list xattrs of {}: {}", p.display(), std::io::Error::last_os_error())); }
    let mut names = vec![0u8; size as usize];
    let n = if names.is_empty() { 0 } else { unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len()) } };
    if n < 0 { return Err(anyhow!("list xattrs of {}: {}", p.display(), std::io::Error::last_os_error())); }
    let mut out = BTreeMap::new();
    for name in names[..n as usize].split(|b| *b == 0).filter(|n| n.starts_with(b"user.")) {
        let cname = std::ffi::CString::new(name)?;
        let mut v = vec![0u8; 65536];
        let len = unsafe { libc::lgetxattr(c.as_ptr(), cname.as_ptr(), v.as_mut_ptr().cast(), v.len()) };
        if len < 0 { return Err(anyhow!("read xattr of {}: {}", p.display(), std::io::Error::last_os_error())); }
        out.insert(String::from_utf8_lossy(name).into_owned(), B64.encode(&v[..len as usize]));
    }
    Ok(out)
}

fn put_header(out: &mut impl Write, h: &Header) -> Result<()> {
    let j = serde_json::to_vec(h)?;
    out.write_all(&(j.len() as u32).to_be_bytes())?;
    out.write_all(&j)?;
    Ok(())
}

/// Write layer directory `dir` to `out`; returns the stream's digest.
pub fn write_layer(dir: &Path, out: impl Write) -> Result<String> {
    let mut out = Hashing { inner: out, hash: Sha256::new() };
    out.write_all(MAGIC)?;
    let mut links: HashMap<(u64, u64), String> = HashMap::new();
    for e in walkdir::WalkDir::new(dir).min_depth(1).follow_links(false).sort_by_file_name() {
        let e = e.map_err(|err| anyhow!("read layer {}: {}", dir.display(), err))?;
        let rel = e.path().strip_prefix(dir)?.to_str().ok_or_else(|| anyhow!("non-UTF-8 path in {}", dir.display()))?.to_string();
        let md = std::fs::symlink_metadata(e.path())?;
        let ft = md.file_type();
        let mut h = Header { p: rel.clone(), m: md.mode() & 0o7777, t: md.mtime() * 1_000_000_000 + md.mtime_nsec(), ..Header::default() };
        if !ft.is_symlink() { h.x = user_xattrs(e.path())?; }
        if ft.is_file() && md.nlink() > 1 {
            if let Some(first) = links.get(&(md.dev(), md.ino())) {
                h.k = "h".into();
                h.l = Some(first.clone());
                put_header(&mut out, &h)?;
                continue;
            }
            links.insert((md.dev(), md.ino()), rel.clone());
        }
        if ft.is_dir() { h.k = "d".into(); put_header(&mut out, &h)?; }
        else if ft.is_symlink() {
            h.k = "l".into();
            h.l = Some(std::fs::read_link(e.path())?.to_str().ok_or_else(|| anyhow!("non-UTF-8 link in {}", rel))?.to_string());
            put_header(&mut out, &h)?;
        } else if ft.is_file() {
            h.k = "f".into();
            h.n = md.len();
            put_header(&mut out, &h)?;
            let n = std::io::copy(&mut std::fs::File::open(e.path())?.take(md.len()), &mut out)?;
            if n != md.len() { return Err(anyhow!("{} changed while it was read", e.path().display())); }
        } else if ft.is_char_device() && md.rdev() == 0 { h.k = "w".into(); put_header(&mut out, &h)?; }
        else if ft.is_fifo() { h.k = "p".into(); put_header(&mut out, &h)?; }
        else if ft.is_socket() { h.k = "s".into(); put_header(&mut out, &h)?; }
        else { return Err(anyhow!("{}: device nodes are not part of a layer", e.path().display())); }
    }
    put_header(&mut out, &Header { k: "end".into(), ..Header::default() })?;
    out.flush()?;
    Ok(digest_hex(out.hash))
}

/// The digest of layer directory `dir` (what `write_layer` would send).
pub fn layer_digest(dir: &Path) -> Result<String> { write_layer(dir, std::io::sink()) }

/// Reads from `inner` and hashes what it reads.
struct Reading<R> { inner: R, hash: Sha256 }

impl<R: Read> Read for Reading<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

/// A path from a stream, inside the layer: relative, without `..`.
fn inside(dest: &Path, rel: &str) -> Result<std::path::PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) { return Err(anyhow!("{:?}: not a path inside a layer", rel)); }
    Ok(dest.join(p))
}

fn set_times(p: &Path, t: i64) -> Result<()> {
    let ts = libc::timespec { tv_sec: t.div_euclid(1_000_000_000), tv_nsec: t.rem_euclid(1_000_000_000) };
    let c = cstr(p)?;
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [ts, ts].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(anyhow!("set times on {}: {}", p.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

fn set_xattrs(p: &Path, x: &BTreeMap<String, String>) -> Result<()> {
    let c = cstr(p)?;
    for (name, v) in x {
        if !name.starts_with("user.") { return Err(anyhow!("xattr {} is not user.*", name)); }
        let v = B64.decode(v)?;
        let n = std::ffi::CString::new(name.as_bytes())?;
        if unsafe { libc::lsetxattr(c.as_ptr(), n.as_ptr(), v.as_ptr().cast(), v.len(), 0) } != 0 {
            return Err(anyhow!("set xattr on {}: {}", p.display(), std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Read a layer stream into `dest` (which must not exist), checking it against `want` (its digest). On any
/// error, `dest` is removed.
pub fn read_layer(input: impl Read, dest: &Path, want: &str) -> Result<()> {
    if std::fs::symlink_metadata(dest).is_ok() { return Err(anyhow!("{} exists", dest.display())); }
    let r = (|| -> Result<()> {
        std::fs::create_dir_all(dest)?;
        let mut input = Reading { inner: input, hash: Sha256::new() };
        let mut magic = [0u8; 9];
        input.read_exact(&mut magic)?;
        if magic != MAGIC { return Err(anyhow!("not a layer stream")); }
        // directories get their modes and times once their contents are in
        let mut dirs: Vec<(std::path::PathBuf, u32, i64)> = Vec::new();
        // nothing of a layer is below a symlink; an entry that is would be written wherever the link points
        let mut symlinks: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let mut len = [0u8; 4];
            input.read_exact(&mut len)?;
            let len = u32::from_be_bytes(len);
            if len > MAX_HEADER { return Err(anyhow!("a layer stream header of {} bytes", len)); }
            let mut j = vec![0u8; len as usize];
            input.read_exact(&mut j)?;
            let h: Header = serde_json::from_slice(&j)?;
            if h.k == "end" { break; }
            let p = inside(dest, &h.p)?;
            let mut prefix = String::new();
            for c in h.p.split('/') {
                if symlinks.contains(&prefix) { return Err(anyhow!("{} is below a symlink", h.p)); }
                if !prefix.is_empty() { prefix.push('/'); }
                prefix.push_str(c);
            }
            if h.k == "l" { symlinks.insert(h.p.clone()); }
            match h.k.as_str() {
                "d" => { std::fs::create_dir(&p)?; set_xattrs(&p, &h.x)?; dirs.push((p, h.m, h.t)); continue; }
                "f" => {
                    let mut f = std::fs::File::create(&p)?;
                    let n = std::io::copy(&mut (&mut input).take(h.n), &mut f)?;
                    if n != h.n { return Err(anyhow!("the stream ended inside {}", h.p)); }
                }
                "l" => { std::os::unix::fs::symlink(h.l.as_deref().ok_or_else(|| anyhow!("a link without a target"))?, &p)?; set_times(&p, h.t)?; continue; }
                "h" => { std::fs::hard_link(inside(dest, h.l.as_deref().ok_or_else(|| anyhow!("a hard link without a target"))?)?, &p)?; continue; }
                "w" | "p" | "s" => {
                    let kind = match h.k.as_str() { "w" => libc::S_IFCHR, "p" => libc::S_IFIFO, _ => libc::S_IFSOCK };
                    if unsafe { libc::mknod(cstr(&p)?.as_ptr(), kind | (h.m & 0o7777), 0) } != 0 {
                        return Err(anyhow!("mknod {}: {}", p.display(), std::io::Error::last_os_error()));
                    }
                }
                k => return Err(anyhow!("unknown entry kind {:?}", k)),
            }
            set_xattrs(&p, &h.x)?;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(h.m & 0o7777))?;
            set_times(&p, h.t)?;
        }
        for (p, m, t) in dirs.iter().rev() {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m & 0o7777))?;
            set_times(p, *t)?;
        }
        let got = digest_hex(input.hash);
        if got != want { return Err(anyhow!("the layer does not match its digest ({} != {})", got, want)); }
        Ok(())
    })();
    if r.is_err() { let _ = crate::vm::remove_tree(dest); }
    r
}

/// The digest of a file (a template's database).
pub fn file_digest(p: &Path) -> Result<String> {
    let mut h = Sha256::new();
    std::io::copy(&mut std::fs::File::open(p)?, &mut h)?;
    Ok(digest_hex(h))
}

/// Copy a file stream into `dest`, checking it against `want`; on a mismatch `dest` is removed.
pub fn read_file(mut input: impl Read, dest: &Path, want: &str) -> Result<()> {
    let r = (|| -> Result<()> {
        let mut out = Hashing { inner: std::fs::File::create(dest)?, hash: Sha256::new() };
        std::io::copy(&mut input, &mut out)?;
        out.flush()?;
        let got = digest_hex(out.hash);
        if got != want { return Err(anyhow!("the file does not match its digest ({} != {})", got, want)); }
        Ok(())
    })();
    if r.is_err() { let _ = std::fs::remove_file(dest); }
    r
}
