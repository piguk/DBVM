#!/bin/sh
# Resolve the current Alpine minirootfs from latest-stable, download it and verify its sha256.
#
# Usage: scripts/fetch-alpine-rootfs.sh [dest-dir] [arch]
#
# arch defaults to the host CPU; pass it explicitly to fetch a foreign rootfs.
#
# Prints `KEY=value` lines on stdout, so it can be piped straight into $GITHUB_ENV
# or consumed with `eval`:
#   ALPINE_BRANCH=v3.24
#   ALPINE_VERSION=3.24.1
#   ALPINE_ARCH=aarch64
#   ALPINE_TARBALL=/tmp/alpine-minirootfs-3.24.1-aarch64.tar.gz
set -eu

# uname -m spellings differ per OS (macOS reports arm64, Linux aarch64); Alpine
# publishes under its own names.
host_arch() {
	case "$(uname -m)" in
	x86_64 | amd64) echo x86_64 ;;
	aarch64 | arm64) echo aarch64 ;;
	armv7l | armv7) echo armv7 ;;
	armv6l | armhf) echo armhf ;;
	i386 | i486 | i586 | i686) echo x86 ;;
	ppc64le) echo ppc64le ;;
	s390x) echo s390x ;;
	riscv64) echo riscv64 ;;
	loongarch64) echo loongarch64 ;;
	*)
		echo "fetch-alpine-rootfs: unmapped CPU $(uname -m), pass the Alpine arch explicitly" >&2
		return 1
		;;
	esac
}

dest="${1:-${TMPDIR:-/tmp}}"
arch="${2:-$(host_arch)}"
base="https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/${arch}"

meta=$(curl -fsSL --retry 3 --retry-delay 2 "${base}/latest-releases.yaml")

# latest-releases.yaml lists one block per flavor; sha256 is the last field of a block,
# so by the time it is seen the other fields of that block are already captured.
entry=$(printf '%s\n' "$meta" | awk '
  /^-/                    { branch=""; ver=""; flavor=""; file="" ; next }
  /^[[:space:]]+branch:/  { branch=$2 }
  /^[[:space:]]+version:/ { ver=$2 }
  /^[[:space:]]+flavor:/  { flavor=$2 }
  /^[[:space:]]+file:/    { file=$2 }
  /^[[:space:]]+sha256:/  { if (flavor == "alpine-minirootfs") { print branch, ver, file, $2; exit } }
')
if [ -z "$entry" ]; then
	echo "fetch-alpine-rootfs: no alpine-minirootfs entry for ${arch}" >&2
	exit 1
fi

# shellcheck disable=SC2086 # deliberate word splitting of the four awk fields
set -- $entry
branch="$1"
version="$2"
file="$3"
want_sha="$4"

mkdir -p "$dest"
out="${dest}/${file}"
curl -fsSL --retry 3 --retry-delay 2 -o "$out" "${base}/${file}"

if command -v sha256sum >/dev/null 2>&1; then
	got_sha=$(sha256sum "$out" | cut -d' ' -f1)
else
	got_sha=$(shasum -a 256 "$out" | cut -d' ' -f1) # macOS has no sha256sum
fi
if [ "$got_sha" != "$want_sha" ]; then
	echo "fetch-alpine-rootfs: sha256 mismatch for ${file}" >&2
	echo "  want ${want_sha}" >&2
	echo "  got  ${got_sha}" >&2
	rm -f "$out"
	exit 1
fi

echo "ALPINE_BRANCH=${branch}"
echo "ALPINE_VERSION=${version}"
echo "ALPINE_ARCH=${arch}"
echo "ALPINE_TARBALL=${out}"
