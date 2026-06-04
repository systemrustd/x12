#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
Xvfb :5 -screen 0 1024x768x24 > /dev/null 2>&1 & xv=$!
sleep 2
DISPLAY=:5 Xephyr :6 -screen 1024x768 > /dev/null 2>&1 & xe=$!
for _ in $(seq 1 50); do DISPLAY=:6 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
export DISPLAY=:6
x11trace -n -o /tmp/fontset-xephyr.trace -- /tmp/fontset-probe
echo "probe rc=$?"
grep -E "Request\((45|49|50)\)|Reply to (ListFonts|OpenFont|QueryFont)|GetAtomName" /tmp/fontset-xephyr.trace | cut -c1-200
kill $xe $xv 2>/dev/null
