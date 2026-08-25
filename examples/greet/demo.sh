#!/usr/bin/env bash
set -e
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"
DBVM="$ROOT/target/release/dbvm"
ELF2SELF="$ROOT/target/release/elf2self"
SELF_EXEC="$ROOT/target/release/self-exec"
if [ ! -x "$DBVM" ]; then DBVM="$ROOT/target/debug/dbvm"; fi
if [ ! -x "$ELF2SELF" ]; then ELF2SELF="$ROOT/target/debug/elf2self"; fi
if [ ! -x "$SELF_EXEC" ]; then SELF_EXEC="$ROOT/target/debug/self-exec"; fi
make -s
echo "== 1. normal run =="
LD_LIBRARY_PATH=. ./app
echo "== 2. closure db =="
"$DBVM" self closure ./app /tmp/greet_closure.db
sqlite3 -column /tmp/greet_closure.db "SELECT n.soname, n.resolved_path FROM needs n JOIN objects o ON o.id=n.object_id WHERE o.is_root=1"
echo "== 3. remove .so -> fails =="
mv libgreet.so.1 libgreet.so.1.bak
if LD_LIBRARY_PATH=. ./app 2>&1; then echo "unexpected success"; else echo "failed as expected"; fi
mv libgreet.so.1.bak libgreet.so.1
echo "== 4. audit =="
echo "   SELF_SYSTEM_DB=/tmp/greet_closure.db LD_AUDIT=./libself-audit.so ./app"
echo "== 5. bundle self-contained =="
"$ELF2SELF" ./app -o /tmp/greet.bundle.self --bundle
echo "   bundle_objects: $(sqlite3 /tmp/greet.bundle.self 'SELECT count(*) FROM bundle_objects')  bundle_needs: $(sqlite3 /tmp/greet.bundle.self 'SELECT count(*) FROM bundle_needs')"
mv libgreet.so.1 libgreet.so.1.bak
echo "   without original .so:"
LD_LIBRARY_PATH="" "$SELF_EXEC" /tmp/greet.bundle.self
echo "   bundle still runs"
mv libgreet.so.1.bak libgreet.so.1
