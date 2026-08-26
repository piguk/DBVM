#!/usr/bin/env bash
set -e
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ELF2SELF="$ROOT/target/release/elf2self"
SELF="$ROOT/target/release/self"
if [ ! -x "$ELF2SELF" ]; then ELF2SELF="$ROOT/target/debug/elf2self"; fi
if [ ! -x "$SELF" ]; then SELF="$ROOT/target/debug/self"; fi
if [ ! -x "$ELF2SELF" ]; then echo "build first: cargo build --release" >&2; exit 1; fi
ELF=/bin/ls
OUT=/tmp/ls.self
BUNDLE=/tmp/ls.bundle.self
"$ELF2SELF" "$ELF" -o "$OUT"
echo "ELF  $(stat -c%s "$ELF") bytes"
echo "SELF $(stat -c%s "$OUT") bytes"
"$ELF2SELF" "$ELF" -o "$BUNDLE" --bundle
echo "BUNDLE $(stat -c%s "$BUNDLE") bytes  (bundle_objects=$(sqlite3 "$BUNDLE" 'SELECT count(*) FROM bundle_objects'))"
"$SELF" bundle "$BUNDLE" | tail -n 2
"$SELF" bundle-info "$BUNDLE"
sqlite3 "$OUT" "DELETE FROM sections; DELETE FROM notes; DELETE FROM dynamic_entries; VACUUM;"
echo "SELF stripped $(stat -c%s "$OUT") bytes"
echo "--- closure ---"
"$SELF" closure /bin/ls /tmp/c.db >/dev/null
sqlite3 -column /tmp/c.db "SELECT count(*) FROM objects; SELECT count(*) FROM needs;" | paste - - | awk '{print "objects",$1,"needs",$2}'

echo "--- vm ---"
VM=/tmp/vm.bench.db
"$SELF" vm-init "$VM" --force --vm-only >/dev/null
"$SELF" vm-add "$VM" "$ELF" "$ELF" >/dev/null
"$SELF" vm-import "$VM" "$ELF" >/dev/null
echo "VM   $(stat -c%s "$VM") bytes  (vm_fs=$(sqlite3 "$VM" 'SELECT count(*) FROM vm_fs') vm_snapshots=$(sqlite3 "$VM" 'SELECT count(*) FROM vm_snapshots') blobs=$(sqlite3 "$VM" 'SELECT count(*) FROM vm_blobs'))"
"$SELF" vm-checkpoint "$VM" bench --note "size.sh" >/dev/null 2>&1 || true
"$SELF" vm-verify "$VM" | sed 's/^/VM verify: /'

ls -lh /tmp/c.db | awk '{print "size",$5}'

echo "--- vm Alpine benchmark (requires /tmp/alpine*.tar.gz or network) ---"
ALPINE_TAR=/tmp/alpine.tar.gz
[ -f /tmp/alpine-minirootfs.tar.gz ] && ALPINE_TAR=/tmp/alpine-minirootfs.tar.gz
if [ -f "$ALPINE_TAR" ]; then
  VM=/tmp/alpine.vm.bench.db
  "$SELF" vm-init "$VM" --force --vm-only >/dev/null 2>&1 || true
  "$SELF" vm-import-rootfs "$VM" "$ALPINE_TAR" >/dev/null 2>&1 || true
  "$SELF" vm-compress-info "$VM" 2>&1 | sed 's/^/vm Alpine: /'
  "$SELF" vm-verify "$VM" 2>&1 | sed 's/^/vm Alpine verify: /'
  # dict training benefits small files
  "$SELF" vm-train-dict "$VM" >/dev/null 2>&1 || true
  "$SELF" vm-dict-info "$VM" 2>&1 | sed 's/^/vm Alpine dict: /'
  VM2=/tmp/alpine.vm.min.db
  "$SELF" vm-init "$VM2" --force --vm-only >/dev/null 2>&1 || true
  "$SELF" vm-import-rootfs "$VM2" "$ALPINE_TAR" --whitelist /bin --whitelist /etc --whitelist /lib --exclude /usr/share/apk/keys >/dev/null 2>&1 || true
  echo "VM minimal whitelist $(stat -c%s "$VM2" 2>/dev/null || echo "?") bytes  (vm_fs=$(sqlite3 "$VM2" 'SELECT count(*) FROM vm_fs' 2>/dev/null || echo "?"))"

  VMTINY=/tmp/alpine.tiny.bench.db
  "$SELF" vm-init "$VMTINY" --force --vm-only >/dev/null 2>&1 || true
  "$SELF" vm-import-rootfs "$VMTINY" "$ALPINE_TAR" --whitelist /bin/busybox >/dev/null 2>&1 || true
  echo "VM tiny /bin/busybox $(stat -c%s "$VMTINY" 2>/dev/null || echo "?") bytes (official 790K Alpine vs 79K tinyconfig custom)"
else
  echo "(skip Alpine: no tar at /tmp/alpine.tar.gz nor /tmp/alpine-minirootfs.tar.gz)"
fi
