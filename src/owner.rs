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
        uid.map_or(false, |u| u != self.uid) || gid.map_or(false, |g| g != self.gid)
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
