#!/bin/sh
# A root filesystem tarball for a pack's template, assembled from the packages installed on this host (Debian or
# Ubuntu): the given packages and everything they depend on, as dpkg lists their files. For hosts without network
# access; with network access, build the pack's Dockerfile instead and import the image with --oci.
#
#   packs/packkit/host-rootfs.sh OUT.tar PACKAGE...
#   (packs/eda: verilator ngspice g++ make perl python3)
set -eu
out=${1:?usage: host-rootfs.sh OUT.tar PACKAGE...}
shift
[ $# -gt 0 ] || { echo "host-rootfs: name the packages" >&2; exit 2; }
# a shell and the basic commands every task uses; dash provides /bin/sh (Essential packages appear in no Depends)
want="$* libc-bin dash bash coreutils grep sed findutils diffutils"
arch=$(dpkg --print-architecture)

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The installed instance of a package for this architecture (or arch "all"), as name:arch: a name installed for
# several architectures (libc6 for amd64 and i386) is ambiguous to dpkg. Empty if none is installed.
installed() {
    dpkg-query -W -f='${binary:Package} ${Architecture} ${Status}\n' "$1" 2>/dev/null |
        awk -v a="$arch" '$NF == "installed" && ($2 == a || $2 == "all") { sub(/:.*/, "", $1); print $1 ":" $2; exit }'
}

: > "$tmp/closure"
todo=""
for p in $want; do
    q=$(installed "$p")
    [ -n "$q" ] || { echo "host-rootfs: $p is not installed" >&2; exit 1; }
    echo "$q" >> "$tmp/closure"
    todo="$todo $q"
done
while [ -n "$todo" ]; do
    next=""
    for p in $todo; do
        deps=$(dpkg-query -W -f='${Pre-Depends}, ${Depends}' "$p" | tr ',|' '\n\n' |
               sed -e 's/([^)]*)//g' -e 's/:any//g' -e 's/[[:space:]]//g' | grep -v '^$' || true)
        for d in $deps; do
            # of alternatives and virtual packages, what is installed
            q=$(installed "$d")
            [ -n "$q" ] || continue
            grep -qxF "$q" "$tmp/closure" && continue
            echo "$q" >> "$tmp/closure"
            next="$next $q"
        done
    done
    todo=$next
done

# their files (one package at a time: dpkg stops at the first package it cannot list), and the /etc files a
# system needs; directories are recreated by tar
while read -r p; do dpkg -L "$p"; done < "$tmp/closure" | sort -u | while read -r f; do
    [ -f "$f" ] || [ -L "$f" ] || continue
    echo "${f#/}"
done > "$tmp/files"
for f in etc/passwd etc/group etc/nsswitch.conf etc/ld.so.conf etc/ld.so.cache; do [ -e "/$f" ] && echo "$f" >> "$tmp/files"; done
ls -d /etc/ld.so.conf.d/* 2>/dev/null | sed 's|^/||' >> "$tmp/files"
# Commands the alternatives system links in (/usr/bin/python3 -> /etc/alternatives/python3 -> ...): each link is
# made again, to the best candidate that is part of the rootfs. The host's own choice may be a path that is not
# (a python under a home directory), and must not end up in the image.
mkdir -p "$tmp/alt/etc/alternatives"
for a in /etc/alternatives/*; do
    name=${a##*/}
    best=$(update-alternatives --query "$name" 2>/dev/null | awk '
        /^Alternative: / { path = $2 }
        /^Priority: / { print $2, path }' | sort -rn | while read -r prio path; do
            grep -qxF "${path#/}" "$tmp/files" && { echo "$path"; break; }
        done || true)
    [ -n "$best" ] && ln -s "$best" "$tmp/alt/etc/alternatives/$name"
done
# merged /usr: /bin, /lib, /lib64, /sbin are links into /usr
for l in bin lib lib64 sbin; do [ -L "/$l" ] && echo "$l" >> "$tmp/files"; done
# each path once: tar stores a second mention as a hard link to the first
sort -u -o "$tmp/files" "$tmp/files"
tar -cf "$out" --owner=0 --group=0 --numeric-owner -C / --no-recursion -T "$tmp/files" -C "$tmp/alt" --recursion etc/alternatives
echo "$(wc -l < "$tmp/closure") packages, $(wc -l < "$tmp/files") files -> $out" >&2
