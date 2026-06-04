#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
RUST_LOG=info RUST_BACKTRACE=1 YSERVER_RENDER_MODEL=v2 YSERVER_MODE=1024x768 \
    target/debug/yserver > yserver-e16probe.log 2>&1 &
pid=$!
for _ in $(seq 1 150); do DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
export DISPLAY=:7
e16 > e16-probe.log 2>&1 &
e16pid=$!
sleep 8
if kill -0 $e16pid 2>/dev/null; then
    echo "E16 ALIVE after 8s"
    xdotool getactivewindow 2>/dev/null || true
    DISPLAY=:7 xlsclients 2>/dev/null | head
else
    wait $e16pid; echo "E16 EXITED rc=$?"
fi
echo "--- e16-probe.log ---"; cat e16-probe.log
kill $pid 2>/dev/null; wait $pid 2>/dev/null
