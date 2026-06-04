#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
# Control: e16 against Xephyr (needs an X display for Xephyr itself —
# use Xvfb as the outer server, Xephyr inside it, e16 on Xephyr).
Xvfb :5 -screen 0 1024x768x24 > /dev/null 2>&1 &
xvfb=$!
sleep 2
DISPLAY=:5 Xephyr :6 -screen 1024x768 > /dev/null 2>&1 &
xephyr=$!
for _ in $(seq 1 50); do DISPLAY=:6 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
if ! DISPLAY=:6 xdpyinfo >/dev/null 2>&1; then echo "Xephyr did not come up"; exit 2; fi
DISPLAY=:6 e16 > /tmp/e16-xephyr.log 2>&1 &
e16pid=$!
sleep 8
if kill -0 $e16pid 2>/dev/null; then
    echo "E16-on-Xephyr ALIVE after 8s"
else
    wait $e16pid; echo "E16-on-Xephyr EXITED rc=$?"
fi
cat /tmp/e16-xephyr.log
kill $e16pid $xephyr $xvfb 2>/dev/null
