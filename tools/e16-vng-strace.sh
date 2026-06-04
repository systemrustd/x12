#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
RUST_LOG=info RUST_BACKTRACE=1 YSERVER_RENDER_MODEL=v2 YSERVER_MODE=1024x768 \
    target/debug/yserver > /dev/null 2>&1 &
pid=$!
for _ in $(seq 1 150); do DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
export DISPLAY=:7
echo "HOME=$HOME USER=$(id -un 2>/dev/null) writable_home=$(test -w "$HOME" && echo yes || echo no)"
strace -f -e trace=%file,exit_group -o /tmp/e16.strace e16 > /tmp/e16-stdout.log 2>&1
echo "e16 rc=$?"
echo "--- last 40 strace lines ---"
tail -40 /tmp/e16.strace
echo "--- failed file ops (EACCES/EROFS/ENOENT on write-ish) ---"
grep -E "EACCES|EROFS|EPERM" /tmp/e16.strace | head -10
echo "--- e16 stdout/stderr ---"
cat /tmp/e16-stdout.log
kill $pid 2>/dev/null; wait $pid 2>/dev/null
