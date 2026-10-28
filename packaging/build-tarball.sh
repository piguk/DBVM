#!/bin/sh
# A release tarball: the binary, the systemd units, install.sh and the docs.
#
#   packaging/build-tarball.sh                                     # for this host's libc (glibc)
#   TARGET=x86_64-unknown-linux-musl packaging/build-tarball.sh   # a static binary, for any distribution
#
# A glibc build runs where the glibc is at least as new as the newest symbol version it uses (recorded in
# REQUIREMENTS in the tarball); the static build has no such requirement.
set -eu
cd "$(dirname "$0")/.."
cargo build --release ${TARGET:+--target "$TARGET"}
bin=target/${TARGET:+$TARGET/}release/self
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
name=selfdb-$version-$(uname -m)${TARGET:+-$TARGET}
out=${OUT:-target/dist}
stage=$out/$name
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/systemd" "$stage/docs"
install -m 0755 "$bin" "$stage/bin/self"
install -m 0644 packaging/systemd/*.service "$stage/systemd/"
install -m 0755 packaging/install.sh "$stage/"
install -m 0644 packaging/README.md "$stage/"
install -m 0644 docs/env.md "$stage/docs/"
if ldd "$bin" >/dev/null 2>&1; then
    glibc=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sort -t. -k1,1 -k2,2n -k3,3n -u | tail -1)
    echo "glibc >= ${glibc#GLIBC_} (a dynamic build); bubblewrap >= 0.11; Linux >= 5.11 with cgroup v2" > "$stage/REQUIREMENTS"
else
    echo "a static build; bubblewrap >= 0.11; Linux >= 5.11 with cgroup v2" > "$stage/REQUIREMENTS"
fi
tar -C "$out" --owner=0 --group=0 --numeric-owner -czf "$out/$name.tar.gz" "$name"
rm -rf "$stage"
echo "$out/$name.tar.gz"
