#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
RUST_LOG=info YSERVER_RENDER_MODEL=v2 YSERVER_MODE=1024x768 target/debug/yserver > /dev/null 2>&1 &
ypid=$!
for _ in $(seq 1 150); do DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
echo "=== fontset on yserver ==="
DISPLAY=:7 /tmp/fontset-probe; echo "rc=$?"
Xvfb :5 -screen 0 1024x768x24 > /dev/null 2>&1 & xv=$!
sleep 2
DISPLAY=:5 Xephyr :6 -screen 1024x768 > /dev/null 2>&1 & xe=$!
for _ in $(seq 1 50); do DISPLAY=:6 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
echo "=== fontset on Xephyr ==="
DISPLAY=:6 /tmp/fontset-probe; echo "rc=$?"
kill $ypid $xe $xv 2>/dev/null
