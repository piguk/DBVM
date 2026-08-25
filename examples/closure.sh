#!/usr/bin/env bash
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DBVM="$ROOT/target/release/dbvm"
if [ ! -x "$DBVM" ]; then DBVM="$ROOT/target/debug/dbvm"; fi
"$DBVM" self closure /bin/ls /tmp/coreutils.db
sqlite3 /tmp/coreutils.db "SELECT n.soname, substr(n.resolved_path, 12, 20) FROM needs n JOIN objects o ON o.id=n.object_id WHERE o.is_root=1"
