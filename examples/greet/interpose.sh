#!/usr/bin/env bash
set -e
cd "$(dirname "$0")"
gcc -fPIC -shared -o libmul.so.1 libmul.c
echo "== without preload =="
LD_LIBRARY_PATH=. ./app
echo "== LD_PRELOAD =="
LD_LIBRARY_PATH=. LD_PRELOAD=./libmul.so.1 ./app
echo "== preload as table =="
sqlite3 -column /tmp/greet_closure.db "SELECT path FROM objects WHERE soname='libgreet.so.1'"
echo "If closure.db had preload table, self-exec would map preload libs last"
