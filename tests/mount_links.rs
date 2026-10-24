//! A mounted VM must not resolve links onto the host. The mount rewrites absolute link targets to
//! paths that stay inside the tree (owner::mount_link_text). These tests check the rewrite against the
//! kernel's own resolution on a real directory tree, not only against the function's arithmetic.
use selfdb::owner::mount_link_text;

fn tree(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("mlinks-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn kernel_resolves_rewritten_links_inside_the_tree() {
    let root = tree("kernel");
    let real = |p: &str| root.join(p.trim_start_matches('/'));
    for (p, body) in [("/usr/bin/dash", "DASH"), ("/a/z", "ZED")] {
        std::fs::create_dir_all(real(p).parent().unwrap()).unwrap();
        std::fs::write(real(p), body).unwrap();
    }
    let links = [("/bin/sh", "/usr/bin/dash", "DASH"), ("/a/b/c/d", "/a/z", "ZED")];
    for (link, target, want) in links {
        let lp = real(link);
        std::fs::create_dir_all(lp.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(mount_link_text(link, target), &lp).unwrap();
        let got = std::fs::read(&lp).map(|b| String::from_utf8_lossy(&b).to_string());
        assert_eq!(got.as_deref().ok(), Some(want), "kernel must resolve {} inside the tree", link);
    }
    // a directory link is followed as a path component too
    std::os::unix::fs::symlink(mount_link_text("/bin-dir", "/usr/bin"), real("/bin-dir")).unwrap();
    assert_eq!(std::fs::read(real("/bin-dir/dash")).map(|b| String::from_utf8_lossy(&b).to_string()).ok().as_deref(),
        Some("DASH"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_link_to_a_host_file_dangles_inside_the_tree_instead_of_reaching_the_host() {
    // The host has /etc/hostname. The rewritten link must not reach it: inside the tree there is no such
    // file, so the link dangles. A dangling link is the correct confinement result.
    let root = tree("host");
    let lp = root.join("etc/probe");
    std::fs::create_dir_all(lp.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(mount_link_text("/etc/probe", "/etc/hostname"), &lp).unwrap();
    assert!(std::fs::read(&lp).is_err(), "must not read the host's /etc/hostname through the link");
    let _ = std::fs::remove_dir_all(&root);
}
