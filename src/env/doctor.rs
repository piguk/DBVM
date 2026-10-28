//! `self env-doctor`: whether this host can run the environment service, and what to change if not.

use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level { Ok, Warn, Fail }

pub struct Check { pub name: &'static str, pub level: Level, pub detail: String, pub fix: Option<&'static str> }

fn check(name: &'static str, level: Level, detail: impl Into<String>, fix: Option<&'static str>) -> Check {
    Check { name, level, detail: detail.into(), fix }
}

fn read(p: &str) -> Option<String> { std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()) }

fn version_at_least(text: &str, want: (u32, u32)) -> Option<bool> {
    let v: Vec<u32> = text.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).take(2).map(|s| s.parse().unwrap_or(0)).collect();
    (v.len() == 2).then(|| (v[0], v[1]) >= want)
}

/// Every check, for the data directory `data` if given.
pub fn checks(data: Option<&Path>) -> Vec<Check> {
    let mut out = Vec::new();
    let kernel = read("/proc/sys/kernel/osrelease").unwrap_or_default();
    out.push(match version_at_least(&kernel, (5, 11)) {
        Some(true) => check("kernel", Level::Ok, &kernel, None),
        _ => check("kernel", Level::Fail, format!("{}: overlayfs in a user namespace needs 5.11 or later", kernel), Some("upgrade the kernel")),
    });
    out.push(if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        check("cgroup v2", Level::Ok, read("/sys/fs/cgroup/cgroup.controllers").unwrap_or_default(), None)
    } else {
        check("cgroup v2", Level::Fail, "the unified hierarchy is not mounted at /sys/fs/cgroup", Some("boot with systemd.unified_cgroup_hierarchy=1"))
    });
    let max_ns = read("/proc/sys/user/max_user_namespaces").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let debian = read("/proc/sys/kernel/unprivileged_userns_clone");
    out.push(if max_ns == 0 || debian.as_deref() == Some("0") {
        check("user namespaces", Level::Fail, "unprivileged user namespaces are disabled", Some("sysctl user.max_user_namespaces=65536 (and kernel.unprivileged_userns_clone=1 on Debian)"))
    } else {
        check("user namespaces", Level::Ok, format!("user.max_user_namespaces={}", max_ns), None)
    });
    if read("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref() == Some("1") {
        out.push(check("apparmor", Level::Warn, "AppArmor restricts unprivileged user namespaces (Ubuntu 23.10 and later)",
                       Some("give bwrap an AppArmor profile that allows userns, or sysctl kernel.apparmor_restrict_unprivileged_userns=0")));
    }
    let bwrap = Command::new("bwrap").arg("--version").output().ok().filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    out.push(match &bwrap {
        None => check("bwrap", Level::Fail, "bubblewrap is not installed", Some("install bubblewrap 0.11 or later")),
        Some(v) if version_at_least(v.trim_start_matches("bubblewrap "), (0, 11)) == Some(true) => check("bwrap", Level::Ok, v, None),
        Some(v) => check("bwrap", Level::Fail, format!("{}: overlay roots need 0.11 or later", v), Some("install bubblewrap 0.11 or later")),
    });
    if bwrap.is_some() { out.push(overlay_sandbox()); }
    out.push(match std::env::var("SELF_SANDBOX_CGROUP") {
        Ok(v) => check("cgroups", Level::Ok, format!("SELF_SANDBOX_CGROUP={} (checked when the first sandbox starts)", v), None),
        Err(_) => {
            let user = Command::new("systemd-run").args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
            if user {
                check("cgroups", Level::Warn, "a systemd user scope per sandbox (slower; for development)",
                      Some("run as a systemd service with Delegate=yes and SELF_SANDBOX_CGROUP=auto"))
            } else {
                check("cgroups", Level::Fail, "no delegated cgroup and no systemd user session: sandbox limits cannot be applied",
                      Some("run as a systemd service with Delegate=yes and SELF_SANDBOX_CGROUP=auto"))
            }
        }
    });
    let instances = read("/proc/sys/fs/inotify/max_user_instances").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let watches = read("/proc/sys/fs/inotify/max_user_watches").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    out.push(if watches < 65536 {
        check("inotify", Level::Warn, format!("max_user_watches={}: rollouts with many directories fall back to full scans", watches),
              Some("sysctl fs.inotify.max_user_watches=1048576"))
    } else {
        check("inotify", Level::Ok, format!("max_user_watches={} max_user_instances={}", watches, instances), None)
    });
    if let Some(d) = data { out.extend(data_dir(d)); }
    let runsc = std::env::var("SELF_RUNSC").ok().filter(|p| Path::new(p).exists()).or_else(|| {
        Command::new("sh").args(["-c", "command -v runsc"]).output().ok().filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    });
    out.push(match runsc {
        Some(p) => check("gvisor", Level::Ok, format!("runsc at {}", p), None),
        None => check("gvisor", Level::Ok, "runsc not found (optional: the gVisor backend)", None),
    });
    out
}

/// Start a sandbox over an overlay root, as rollouts do.
fn overlay_sandbox() -> Check {
    let dir = std::env::temp_dir().join(format!("selfdb-doctor-{}", std::process::id()));
    let _ = crate::vm::remove_tree(&dir);
    let r = (|| -> anyhow::Result<String> {
        for d in ["lower", "upper", "work"] { std::fs::create_dir_all(dir.join(d))?; }
        let o = Command::new("bwrap").args(["--unshare-all", "--unshare-user", "--die-with-parent", "--overlay-src"]).arg(dir.join("lower"))
            .arg("--overlay").arg(dir.join("upper")).arg(dir.join("work")).arg("/mnt")
            .args(["--ro-bind", "/usr", "/usr", "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib64", "/lib64",
                   "--proc", "/proc", "--dev", "/dev", "--", "/bin/sh", "-c", "echo ok > /mnt/probe"]).output()?;
        if !o.status.success() { anyhow::bail!("{}", String::from_utf8_lossy(&o.stderr).trim()); }
        if !dir.join("upper/probe").exists() { anyhow::bail!("the write did not reach the upper directory"); }
        Ok("an overlay sandbox started and wrote to its upper directory".into())
    })();
    let _ = crate::vm::remove_tree(&dir);
    match r {
        Ok(d) => check("overlay sandbox", Level::Ok, d, None),
        Err(e) => check("overlay sandbox", Level::Fail, format!("{:#}", e), Some("see the user namespaces, apparmor and bwrap checks")),
    }
}

/// The data directory's filesystem: room, `user.*` xattrs (overlay markers), whiteouts.
fn data_dir(d: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    if let Err(e) = std::fs::create_dir_all(d) {
        out.push(check("data dir", Level::Fail, format!("{}: {}", d.display(), e), None));
        return out;
    }
    let c = std::ffi::CString::new(d.as_os_str().as_encoded_bytes()).unwrap_or_default();
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 {
        let free = st.f_bavail as u64 * st.f_frsize as u64;
        let gib = free as f64 / (1u64 << 30) as f64;
        out.push(if gib < 10.0 {
            check("data dir space", Level::Warn, format!("{:.1} GiB free at {}", gib, d.display()), Some("templates and rollouts need room: 10 GiB or more"))
        } else {
            check("data dir space", Level::Ok, format!("{:.1} GiB free at {}", gib, d.display()), None)
        });
    }
    let probe = d.join(format!(".doctor-{}", std::process::id()));
    let r = (|| -> anyhow::Result<()> {
        std::fs::write(&probe, b"")?;
        let p = std::ffi::CString::new(probe.as_os_str().as_encoded_bytes())?;
        let n = std::ffi::CString::new("user.selfdb.probe")?;
        if unsafe { libc::lsetxattr(p.as_ptr(), n.as_ptr(), b"y".as_ptr().cast(), 1, 0) } != 0 {
            anyhow::bail!("user.* xattrs: {}", std::io::Error::last_os_error());
        }
        std::fs::remove_file(&probe)?;
        if unsafe { libc::mknod(p.as_ptr(), libc::S_IFCHR, 0) } != 0 {
            anyhow::bail!("whiteouts (a 0:0 character device): {}", std::io::Error::last_os_error());
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&probe);
    out.push(match r {
        Ok(()) => check("data dir filesystem", Level::Ok, "user xattrs and whiteouts work", None),
        Err(e) => check("data dir filesystem", Level::Fail, format!("{:#}", e), Some("put the data directory on ext4, xfs or btrfs")),
    });
    out
}

/// The checks as text; whether none failed.
pub fn report(checks: &[Check]) -> (String, bool) {
    let mut s = String::new();
    for c in checks {
        let tag = match c.level { Level::Ok => "OK  ", Level::Warn => "WARN", Level::Fail => "FAIL" };
        s.push_str(&format!("{} {:20} {}\n", tag, c.name, c.detail));
        if let (Some(f), true) = (c.fix, c.level != Level::Ok) { s.push_str(&format!("     {:20} fix: {}\n", "", f)); }
    }
    (s, !checks.iter().any(|c| c.level == Level::Fail))
}

/// Write a support bundle (a .tar.gz) to `out`: the checks, versions of the host and its tools, the data
/// directory's sizes and the control database's summary (`ControlDb::support_summary`). Nothing secret: no
/// keys, tokens, names, labels, task specs or file contents.
pub fn support_bundle(data: &Path, out: &Path) -> anyhow::Result<()> {
    let run = |cmd: &str, args: &[&str]| Command::new(cmd).args(args).output().map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
        .unwrap_or_else(|e| format!("{}: {}\n", cmd, e));
    let (doctor, _) = report(&checks(Some(data)));
    let versions = format!("self {}\n{}{}{}\n", env!("CARGO_PKG_VERSION"), run("uname", &["-srvm"]), run("bwrap", &["--version"]),
                           read("/etc/os-release").unwrap_or_default());
    let mut sizes = String::new();
    for sub in ["system", "tenants", "run", "control.db"] {
        let p = data.join(sub);
        let bytes: u64 = walkdir::WalkDir::new(&p).into_iter().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum();
        sizes.push_str(&format!("{:12} {}\n", bytes, sub));
    }
    let summary = super::control::ControlDb::open(&data.join("control.db"))?.support_summary()?;
    let f = std::fs::File::create(out)?;
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(f, flate2::Compression::default()));
    for (name, body) in [("doctor.txt", doctor), ("versions.txt", versions), ("data-sizes.txt", sizes),
                         ("control-summary.json", serde_json::to_string_pretty(&summary)?)] {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(crate::vm::now_secs() as u64);
        tar.append_data(&mut h, format!("selfdb-support/{}", name), body.as_bytes())?;
    }
    tar.into_inner()?.finish()?;
    Ok(())
}
