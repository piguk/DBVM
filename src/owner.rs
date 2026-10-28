//! Ownership seen through a mounted VM.
//!
//! Model: every node in a mounted VM belongs to the process that mounted it, whatever the node's
//! origin. A file imported from the host, one created through the mount, and the root directory all
//! report the same owner. The owner is the mounting process's real uid and gid.
//!
//! The uid and gid columns in vm_fs are not used for access control. They record where a node came
//! from (sync never recorded the host owner, so most rows hold 0). Using them would make a file's
//! writability depend on how it entered the database, which an agent cannot predict.
//!
//! Permission bits (mode) still come from the database and are enforced by the kernel
//! (`default_permissions`). Only the owner is fixed here.

/// The owner every mounted node reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    /// The owner of the calling process: the identity the mount runs as.
    pub fn of_current_process() -> Self {
        // SAFETY: getuid/getgid take no arguments and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Owner { uid, gid }
    }

    /// Whether a setattr request asks to change ownership to something other than this owner.
    /// Such a request is refused; a request naming the current owner is accepted as a no-op.
    pub fn is_owner_change(&self, uid: Option<u32>, gid: Option<u32>) -> bool {
        uid.is_some_and(|u| u != self.uid) || gid.is_some_and(|g| g != self.gid)
    }

    /// The owner a node reports, given the uid/gid stored for it. Ignores the stored values.
    pub fn for_node(&self, _stored_uid: i64, _stored_gid: i64) -> (u32, u32) {
        (self.uid, self.gid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_owner_is_ignored() {
        let o = Owner { uid: 1000, gid: 1000 };
        // a node imported with uid 0 (what sync records) still reports the mounting owner
        assert_eq!(o.for_node(0, 0), (1000, 1000));
        assert_eq!(o.for_node(4242, 4242), (1000, 1000));
    }

    #[test]
    fn owner_change_is_detected_per_field() {
        let o = Owner { uid: 1000, gid: 1000 };
        assert!(!o.is_owner_change(None, None), "no request");
        assert!(!o.is_owner_change(Some(1000), Some(1000)), "naming the current owner is a no-op");
        assert!(o.is_owner_change(Some(0), None), "uid change refused");
        assert!(o.is_owner_change(None, Some(4242)), "gid change refused");
        assert!(o.is_owner_change(Some(1000), Some(7)), "either field alone is enough");
    }

    #[test]
    fn current_process_owner_matches_libc() {
        let o = Owner::of_current_process();
        let (u, g) = unsafe { (libc::getuid(), libc::getgid()) };
        assert_eq!((o.uid, o.gid), (u, g));
    }
}

/// The link text a mounted VM shows for a stored symlink target.
///
/// Stored targets are kept as written (sync, materialize and resolution all read them), so this
/// changes only what the mount presents. An absolute target would be resolved by the kernel against the
/// host root, escaping the mount. It is shown as a relative path from the link's own directory to the
/// target, so the kernel resolves it inside the mount. Relative targets already stay inside the tree and
/// are returned unchanged. `..` at the top of the tree stops at the root, so the result cannot climb out.
pub fn mount_link_text(link_vm_path: &str, stored_target: &str) -> String {
    if !stored_target.starts_with('/') {
        return stored_target.to_string();
    }
    let from: Vec<&str> = parent_components(link_vm_path);
    let to: Vec<&str> = split_components(stored_target);
    // common prefix of the two directory paths
    let common = from.iter().zip(to.iter()).take_while(|(a, b)| a == b).count();
    let mut out: Vec<&str> = vec![".."; from.len() - common];
    out.extend(to[common..].iter().copied());
    if out.is_empty() { ".".to_string() } else { out.join("/") }
}

/// Components of a VM path with `.` and `..` resolved lexically, not climbing above `/`.
fn split_components(p: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => { out.pop(); }
            c => out.push(c),
        }
    }
    out
}

/// Components of the directory that contains `p`.
fn parent_components(p: &str) -> Vec<&str> {
    let mut v = split_components(p);
    v.pop();
    v
}

#[cfg(test)]
mod link_tests {
    use super::*;

    #[test]
    fn absolute_target_becomes_relative_from_the_link_directory() {
        // /bin/sh -> /usr/bin/dash : from /bin, climb one, then down to usr/bin/dash
        assert_eq!(mount_link_text("/bin/sh", "/usr/bin/dash"), "../usr/bin/dash");
        // /bin -> /usr/bin : from / (the link's own parent), straight down
        assert_eq!(mount_link_text("/bin", "/usr/bin"), "usr/bin");
        // a target in the link's own directory
        assert_eq!(mount_link_text("/a/b/link", "/a/b/c"), "c");
    }

    #[test]
    fn relative_targets_are_unchanged() {
        assert_eq!(mount_link_text("/bin/sh", "dash"), "dash");
        assert_eq!(mount_link_text("/a/b", "../x"), "../x");
    }

    #[test]
    fn the_result_never_climbs_above_the_mount_root() {
        // from a shallow link, nothing above the root is ever requested
        assert_eq!(mount_link_text("/x", "/usr/bin"), "usr/bin");
        // a link in /a/b/c reaches /a/z by climbing from /a/b/c to /a (two levels), then down to z
        assert_eq!(mount_link_text("/a/b/c/d", "/a/z"), "../../z");
    }

    #[test]
    fn a_link_to_the_root_is_handled() {
        // a link in /a reaches the root with one climb
        assert_eq!(mount_link_text("/a/link", "/"), "..");
        assert_eq!(mount_link_text("/link", "/"), ".");
    }

    #[test]
    fn dotdot_inside_an_absolute_target_is_resolved_lexically() {
        // /usr/lib/../bin resolves to /usr/bin before the relative form is computed
        assert_eq!(mount_link_text("/bin/sh", "/usr/lib/../bin/dash"), "../usr/bin/dash");
    }
}
