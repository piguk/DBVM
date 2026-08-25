#!/usr/bin/env bash
set -e
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ELF2SELF="$ROOT/target/release/elf2self"
DBVM="$ROOT/target/release/dbvm"
if [ ! -x "$ELF2SELF" ]; then ELF2SELF="$ROOT/target/debug/elf2self"; fi
if [ ! -x "$DBVM" ]; then DBVM="$ROOT/target/debug/dbvm"; fi
if [ ! -x "$ELF2SELF" ]; then echo "build first: cargo build --release" >&2; exit 1; fi
ELF=/bin/ls
OUT=/tmp/ls.self
BUNDLE=/tmp/ls.bundle.self
"$ELF2SELF" "$ELF" -o "$OUT"
echo "ELF  $(stat -c%s "$ELF") bytes"
echo "SELF $(stat -c%s "$OUT") bytes"
"$ELF2SELF" "$ELF" -o "$BUNDLE" --bundle
echo "BUNDLE $(stat -c%s "$BUNDLE") bytes  (bundle_objects=$(sqlite3 "$BUNDLE" 'SELECT count(*) FROM bundle_objects'))"
"$DBVM" self bundle "$BUNDLE" | tail -n 2
"$DBVM" self bundle-info "$BUNDLE"
sqlite3 "$OUT" "DELETE FROM sections; DELETE FROM notes; DELETE FROM dynamic_entries; VACUUM;"
echo "SELF stripped $(stat -c%s "$OUT") bytes"
echo "--- closure ---"
"$DBVM" self closure /bin/ls /tmp/c.db >/dev/null
sqlite3 -column /tmp/c.db "SELECT count(*) FROM objects; SELECT count(*) FROM needs;" | paste - - | awk '{print "objects",$1,"needs",$2}'

echo "--- vm ---"
VM=/tmp/vm.bench.db
rm -f "$VM" "$VM-wal" "$VM-shm" # sizes are only comparable from an empty instance
"$DBVM" --db "$VM" cp "$ELF" "$ELF" >/dev/null
"$DBVM" --db "$VM" import "$ELF" >/dev/null
echo "VM   $(stat -c%s "$VM") bytes  (vm_fs=$(sqlite3 "$VM" 'SELECT count(*) FROM vm_fs') vm_snapshots=$(sqlite3 "$VM" 'SELECT count(*) FROM vm_snapshots'))"
"$DBVM" --db "$VM" snapshot bench --note "size.sh" >/dev/null 2>&1 || true
"$DBVM" --db "$VM" verify | sed 's/^/VM verify: /'

echo "closure db $(stat -c%s /tmp/c.db) bytes"
