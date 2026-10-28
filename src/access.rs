//! Reading what sandboxes wrote, whatever modes they gave it.
//!
//! A rollout's upper directory belongs to the host user, but the agent is root inside its sandbox and sets
//! any mode: a directory made 000 there is unreadable to the host user, and copies, scans and the disk
//! quota would stop at it. A process that owns a user namespace holds every capability in it, and
//! CAP_DAC_OVERRIDE / CAP_DAC_READ_SEARCH in a namespace apply to files whose owner is mapped into it. So a
//! service enters a namespace that maps only its own uid and gid: it reads and writes its own files
//! regardless of their modes, and gains nothing over anyone else's.
//!
//! Capabilities do not survive exec for a non-root uid, so programs the service starts (bwrap, runsc)
//! run exactly as before; host-side file work that needs this access is done in-process.

use anyhow::{anyhow, Result};

/// CAP_DAC_OVERRIDE and CAP_DAC_READ_SEARCH: read, search and write regardless of modes.
const ACCESS: u32 = (1 << 1) | (1 << 2);
/// Those and CAP_FOWNER (change the modes and times of one's files regardless of their modes).
const KEEP: u32 = ACCESS | (1 << 3);

/// Enter a user namespace mapping only this process's uid and gid, keeping the capabilities that grant
/// access to its own files and dropping the rest. Must run while the process has a single thread (before
/// any runtime starts): unshare(CLONE_NEWUSER) fails otherwise. Setuid programs do not gain privileges
/// inside such a namespace (root is not mapped).
pub fn enter_owner_namespace() -> Result<()> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    if status.lines().find_map(|l| l.strip_prefix("Threads:")).map(|v| v.trim()) != Some("1") {
        return Err(anyhow!("entering a user namespace needs a single-threaded process"));
    }
    // SAFETY: getuid/getgid cannot fail; unshare takes a flag.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Err(anyhow!("unshare(CLONE_NEWUSER): {}", std::io::Error::last_os_error()));
    }
    std::fs::write("/proc/self/setgroups", "deny")?;
    std::fs::write("/proc/self/uid_map", format!("{} {} 1", uid, uid))?;
    std::fs::write("/proc/self/gid_map", format!("{} {} 1", gid, gid))?;
    // SAFETY: header and data are the v3 layout capset expects (two 32-bit words per set).
    let mut hdr = CapHeader { version: 0x2008_0522, pid: 0 };
    let data = [CapData { effective: KEEP, permitted: KEEP, inheritable: 0 }, CapData::default()];
    if unsafe { libc::syscall(libc::SYS_capset, &mut hdr, data.as_ptr()) } != 0 {
        return Err(anyhow!("capset: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Whether this thread may read and change its own files whatever their modes (see the module docs).
pub fn has_owner_access() -> bool {
    let Ok(status) = std::fs::read_to_string("/proc/thread-self/status") else { return false };
    let eff = status.lines().find_map(|l| l.strip_prefix("CapEff:")).and_then(|v| u64::from_str_radix(v.trim(), 16).ok());
    eff.map(|e| e & u64::from(ACCESS) == u64::from(ACCESS)).unwrap_or(false)
}

#[repr(C)]
struct CapHeader { version: u32, pid: i32 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CapData { effective: u32, permitted: u32, inheritable: u32 }
