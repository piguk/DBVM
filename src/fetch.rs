//! Resolving and downloading the current Alpine minirootfs.
//!
//! Mirrors `scripts/fetch-alpine-rootfs.sh`, which CI uses; keep the two in sync.

use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MIRROR: &str = "https://dl-cdn.alpinelinux.org/alpine";

#[derive(Debug, Clone)]
pub struct Release {
    /// Release branch, e.g. `v3.24`.
    pub branch: String,
    /// Full version, e.g. `3.24.1`.
    pub version: String,
    /// Alpine's name for the CPU, e.g. `aarch64`.
    pub arch: String,
    pub file: String,
    pub sha256: String,
}

impl Release {
    pub fn url(&self) -> String {
        format!(
            "{}/latest-stable/releases/{}/{}",
            MIRROR, self.arch, self.file
        )
    }
}

/// Map the host CPU onto Alpine's arch naming. `uname -m` spellings differ per OS
/// (macOS reports `arm64`, Linux `aarch64`).
pub fn host_arch() -> Result<&'static str> {
    Ok(match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "arm" => "armv7",
        "x86" => "x86",
        "powerpc64" => "ppc64le",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        "loongarch64" => "loongarch64",
        other => bail!("no Alpine arch for CPU {other}; pass --arch explicitly"),
    })
}

/// Ask the mirror which minirootfs `latest-stable` currently points at.
pub fn latest_release(arch: &str) -> Result<Release> {
    let url = format!(
        "{}/latest-stable/releases/{}/latest-releases.yaml",
        MIRROR, arch
    );
    let yaml = String::from_utf8_lossy(&http_get(&url)?).into_owned();
    parse_latest_releases(&yaml, arch)
        .ok_or_else(|| anyhow!("no alpine-minirootfs entry for {arch} in {url}"))
}

/// `latest-releases.yaml` lists one block per flavor, with `sha256` last, so a block
/// is complete by the time its checksum is seen.
fn parse_latest_releases(yaml: &str, arch: &str) -> Option<Release> {
    let (mut branch, mut version, mut flavor, mut file) =
        (String::new(), String::new(), String::new(), String::new());
    for line in yaml.lines() {
        let t = line.trim();
        if let Some(rest) = t
            .strip_prefix("- ")
            .or(if t == "-" { Some("") } else { None })
        {
            branch.clear();
            version.clear();
            flavor.clear();
            file.clear();
            if rest.is_empty() {
                continue;
            }
        }
        let Some((key, value)) = t.split_once(": ") else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "branch" => branch = value,
            "version" => version = value,
            "flavor" => flavor = value,
            "file" => file = value,
            "sha256" if flavor == "alpine-minirootfs" && !file.is_empty() => {
                return Some(Release {
                    branch: std::mem::take(&mut branch),
                    version: std::mem::take(&mut version),
                    arch: arch.to_string(),
                    file: std::mem::take(&mut file),
                    sha256: value,
                });
            }
            _ => {}
        }
    }
    None
}

/// Download the rootfs into `dest_dir` and verify its checksum. A cached file that
/// already matches is reused.
pub fn download(release: &Release, dest_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir)?;
    let out = dest_dir.join(&release.file);
    if out.exists()
        && let Ok(data) = std::fs::read(&out)
        && sha256_hex(&data) == release.sha256
    {
        return Ok(out);
    }

    let data = http_get(&release.url())?;
    let got = sha256_hex(&data);
    if got != release.sha256 {
        bail!(
            "sha256 mismatch for {}\n  want {}\n  got  {}",
            release.file,
            release.sha256,
            got
        );
    }
    // Write to a sibling temp file first so an interrupted download is never
    // mistaken for a verified cache entry.
    let tmp = out.with_extension("part");
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Fetch over HTTPS by shelling out, so the binary carries no TLS stack. `curl` is
/// tried first, then `wget`.
fn http_get(url: &str) -> Result<Vec<u8>> {
    let curl = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "3", "--retry-delay", "2", url])
        .output();
    match curl {
        Ok(o) if o.status.success() => return Ok(o.stdout),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if !is_missing_program(&o.status) {
                bail!("curl failed for {url}: {err}");
            }
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            bail!("curl failed for {url}: {e}")
        }
        Err(_) => {}
    }

    match std::process::Command::new("wget")
        .args(["-q", "-O", "-", url])
        .output()
    {
        Ok(o) if o.status.success() => Ok(o.stdout),
        Ok(o) => bail!(
            "wget failed for {url}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("need curl or wget on PATH to download {url}")
        }
        Err(e) => bail!("wget failed for {url}: {e}"),
    }
}

fn is_missing_program(status: &std::process::ExitStatus) -> bool {
    status.code() == Some(127)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"---
-
  title: "Netboot"
  branch: v3.24
  arch: x86_64
  version: 3.24.1
  flavor: alpine-netboot
  file: alpine-netboot-3.24.1-x86_64.tar.gz
  sha256: 9a7769ea8fa1737b1b49d82f1bdd53d0a17338d6d3b7cfc6f2c3ec5158596d8b
-
  title: "Mini root filesystem"
  desc: |
    Minimal root filesystem.
  branch: v3.24
  arch: x86_64
  version: 3.24.1
  flavor: alpine-minirootfs
  file: alpine-minirootfs-3.24.1-x86_64.tar.gz
  sha256: 41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081
"#;

    #[test]
    fn picks_the_minirootfs_block_not_its_neighbours() {
        let r = parse_latest_releases(SAMPLE, "x86_64").expect("minirootfs entry");
        assert_eq!(r.version, "3.24.1");
        assert_eq!(r.branch, "v3.24");
        assert_eq!(r.file, "alpine-minirootfs-3.24.1-x86_64.tar.gz");
        assert_eq!(
            r.sha256,
            "41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081"
        );
        assert!(
            r.url()
                .ends_with("/latest-stable/releases/x86_64/alpine-minirootfs-3.24.1-x86_64.tar.gz")
        );
    }

    #[test]
    fn returns_none_when_the_flavor_is_absent() {
        let only_netboot = SAMPLE.split("-\n  title: \"Mini").next().unwrap();
        assert!(parse_latest_releases(only_netboot, "x86_64").is_none());
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
