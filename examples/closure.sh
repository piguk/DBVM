#!/usr/bin/env bash
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SELF="$ROOT/target/release/self"
if [ ! -x "$SELF" ]; then SELF="$ROOT/target/debug/self"; fi
"$SELF" closure /bin/ls /tmp/coreutils.db
sqlite3 /tmp/coreutils.db "SELECT n.soname, substr(n.resolved_path, 12, 20) FROM needs n JOIN objects o ON o.id=n.object_id WHERE o.is_root=1"
