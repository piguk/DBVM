#!/usr/bin/env bash
set -e
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SELF="$ROOT/target/release/self"
if [ ! -x "$SELF" ]; then SELF="$ROOT/target/debug/self"; fi
if [ ! -x "$SELF" ]; then echo "build first: cargo build --release" >&2; exit 1; fi
OUT=${1:-/tmp/userland.db}
DIRS=(/usr/bin /bin)
if [ $# -ge 2 ]; then OUT="$1"; shift; DIRS=("$@"); fi
echo "scanning ${DIRS[*]} -> $OUT"
"$SELF" userland "$OUT" "${DIRS[@]}"
echo "--- headline queries ---"
sqlite3 -column "$OUT" "SELECT count(DISTINCT soname), count(*) FROM objects WHERE soname IS NOT NULL"
sqlite3 -column "$OUT" "SELECT soname, count(*) FROM objects WHERE soname IS NOT NULL GROUP BY soname HAVING count(*)>1 ORDER BY 2 DESC LIMIT 4"
sqlite3 -column "$OUT" "SELECT count(*) FROM needs WHERE resolved_path IS NULL AND soname NOT LIKE 'ld-%'"
echo "--- sizes ---"
ls -lh "$OUT"
sqlite3 -column "$OUT" "SELECT count(*) FROM objects; SELECT count(*) FROM needs;" | paste - - | awk '{print "objects",$1,"needs",$2}'
