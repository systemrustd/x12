#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
Xvfb :5 -screen 0 1024x768x24 > /dev/null 2>&1 &
xvfb=$!
sleep 2
DISPLAY=:5 Xephyr :6 -screen 1024x768 > /dev/null 2>&1 &
xephyr=$!
for _ in $(seq 1 50); do DISPLAY=:6 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
export DISPLAY=:6
timeout 12 x11trace -n -o e16-xephyr-trace.log -- e16 > /dev/null 2>&1 || true
pkill -f "^e16$" 2>/dev/null
echo "--- first 45 xephyr-trace lines ---"
head -45 e16-xephyr-trace.log
kill $xephyr $xvfb 2>/dev/null
