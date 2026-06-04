#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
RUST_LOG=info RUST_BACKTRACE=1 YSERVER_RENDER_MODEL=v2 YSERVER_MODE=1024x768 \
    target/debug/yserver > yserver-e16trace.log 2>&1 &
pid=$!
for _ in $(seq 1 150); do DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
export DISPLAY=:7
x11trace -n -o e16-x11trace.log -- e16 > e16-trace-stdout.log 2>&1
echo "e16 exited rc=$?"
kill $pid 2>/dev/null; wait $pid 2>/dev/null
echo "--- last 60 trace lines ---"
tail -60 e16-x11trace.log
echo "--- e16 stdout ---"
cat e16-trace-stdout.log
