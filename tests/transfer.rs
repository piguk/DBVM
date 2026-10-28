//! Layers as streams between nodes: a round trip keeps what a layer is (kinds, modes, times, xattrs, whiteouts,
//! links, hard links) and its digest; a stream that does not match its digest, or that would write outside the
//! layer, is refused and leaves nothing behind.
use selfdb::env::transfer::{layer_digest, read_layer, write_layer};
use selfdb::vm;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

struct Dir(PathBuf);
impl Drop for Dir { fn drop(&mut self) { let _ = vm::remove_tree(&self.0); } }

fn dir(name: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("transfer-{}-{}", name, std::process::id()));
    let _ = vm::remove_tree(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

fn c(p: &Path) -> std::ffi::CString { std::ffi::CString::new(p.to_str().unwrap()).unwrap() }

fn sample(root: &Path) {
    std::fs::create_dir_all(root.join("etc/sub")).unwrap();
    std::fs::write(root.join("etc/a"), b"alpha").unwrap();
    std::fs::set_permissions(root.join("etc/a"), std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::hard_link(root.join("etc/a"), root.join("etc/sub/a-link")).unwrap();
    std::os::unix::fs::symlink("../a", root.join("etc/sub/rel")).unwrap();
    std::fs::write(root.join("big"), vec![7u8; 300_000]).unwrap();
    assert_eq!(unsafe { libc::mknod(c(&root.join("etc/gone")).as_ptr(), libc::S_IFCHR, 0) }, 0, "a whiteout");
    let opaque = std::ffi::CString::new("user.overlay.opaque").unwrap();
    assert_eq!(unsafe { libc::lsetxattr(c(&root.join("etc/sub")).as_ptr(), opaque.as_ptr(), b"y".as_ptr().cast(), 1, 0) }, 0);
    std::fs::set_permissions(root.join("etc/sub"), std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn a_layer_survives_the_trip() {
    let d = dir("trip");
    let src = d.0.join("src");
    sample(&src);
    let mut stream = Vec::new();
    let digest = write_layer(&src, &mut stream).unwrap();
    assert_eq!(layer_digest(&src).unwrap(), digest, "deterministic");
    let dest = d.0.join("dest");
    read_layer(&stream[..], &dest, &digest).unwrap();
    assert_eq!(layer_digest(&dest).unwrap(), digest, "the copy is the same layer");
    let md = std::fs::metadata(dest.join("etc/a")).unwrap();
    assert_eq!((md.mode() & 0o7777, md.nlink()), (0o750, 2), "mode and the hard link");
    assert_eq!(std::fs::read_link(dest.join("etc/sub/rel")).unwrap(), PathBuf::from("../a"));
    assert_eq!(std::fs::symlink_metadata(dest.join("etc/gone")).unwrap().rdev(), 0);
    assert_eq!(std::fs::metadata(dest.join("etc/sub")).unwrap().mode() & 0o7777, 0o700);
    assert_eq!(std::fs::metadata(dest.join("etc/a")).unwrap().mtime(), std::fs::metadata(src.join("etc/a")).unwrap().mtime());
}

#[test]
fn a_stream_that_is_not_the_layer_is_refused() {
    let d = dir("bad");
    let src = d.0.join("src");
    sample(&src);
    let mut stream = Vec::new();
    let digest = write_layer(&src, &mut stream).unwrap();
    let mut changed = stream.clone();
    let i = changed.windows(5).position(|w| w == b"alpha").unwrap();
    changed[i] = b'A';
    let dest = d.0.join("dest");
    let e = read_layer(&changed[..], &dest, &digest).unwrap_err();
    assert!(e.to_string().contains("digest"), "{}", e);
    assert!(!dest.exists(), "nothing is left behind");
    assert!(read_layer(&stream[..stream.len() - 10], &dest, &digest).is_err(), "a cut stream");
    assert!(!dest.exists());
}

fn crafted(entries: &[serde_json::Value]) -> Vec<u8> {
    let mut s = b"SELFLYR1\n".to_vec();
    for e in entries.iter().chain(std::iter::once(&serde_json::json!({"p": "", "k": "end"}))) {
        let j = serde_json::to_vec(e).unwrap();
        s.extend((j.len() as u32).to_be_bytes());
        s.extend(j);
    }
    s
}

#[test]
fn a_stream_cannot_write_outside_the_layer() {
    let d = dir("escape");
    let outside = d.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    for (what, s) in [
        ("a parent component", crafted(&[serde_json::json!({"p": "../escaped", "k": "d", "m": 493})])),
        ("an absolute path", crafted(&[serde_json::json!({"p": "/etc/escaped", "k": "d", "m": 493})])),
        ("an entry below a symlink", crafted(&[serde_json::json!({"p": "a", "k": "l", "l": outside.to_str().unwrap()}),
                                               serde_json::json!({"p": "a/escaped", "k": "d", "m": 493})])),
        ("a hard link to outside", crafted(&[serde_json::json!({"p": "h", "k": "h", "l": "../../x"})])),
    ] {
        let dest = d.0.join("dest");
        assert!(read_layer(&s[..], &dest, "sha256:any").is_err(), "{}", what);
        assert!(!dest.exists() && std::fs::read_dir(&outside).unwrap().next().is_none(), "{}: nothing written", what);
    }
}
