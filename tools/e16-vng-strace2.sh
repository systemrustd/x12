#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
RUST_LOG=info RUST_BACKTRACE=1 YSERVER_RENDER_MODEL=v2 YSERVER_MODE=1024x768 \
    target/debug/yserver > /dev/null 2>&1 &
pid=$!
for _ in $(seq 1 150); do DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
export DISPLAY=:7
strace -f -e trace=%file,exit_group,write -o /tmp/e16.strace e16 > /tmp/e16-stdout.log 2>&1
echo "e16 rc=$?"
echo "--- execves ---"
grep "execve(" /tmp/e16.strace | head -10
echo "--- .e16 ops ---"
grep -E "\.e16|mkdir" /tmp/e16.strace | head -15
echo "--- writes to stderr/stdout (fd 1/2) ---"
grep -E '^\S+ +write\((1|2),' /tmp/e16.strace | head -15
echo "--- exits ---"
grep -E "exit_group|exited" /tmp/e16.strace | head
kill $pid 2>/dev/null; wait $pid 2>/dev/null
