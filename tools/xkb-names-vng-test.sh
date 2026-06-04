#!/usr/bin/env bash
# Validate XkbGetNames real-atom fix: pre-fix, libX11 clients
# (xdotool) die with BadAtom on XGetAtomName(0) during XkbGetNames
# processing. Post-fix they run.
set -uo pipefail
cd "$(dirname "$0")/.."
RUST_LOG=info RUST_BACKTRACE=1 YSERVER_RENDER_MODEL=v2 YSERVER_MODE=1024x768 \
    target/debug/yserver > yserver-xkbtest.log 2>&1 &
pid=$!
for _ in $(seq 1 150); do
    DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break
    sleep 0.2
done
DISPLAY=:7 xdpyinfo >/dev/null 2>&1 || { echo "FAIL: yserver did not come up"; exit 2; }
export DISPLAY=:7

echo "=== xdotool getmouselocation (forces XKB init) ==="
if xdotool getmouselocation; then echo "PASS: getmouselocation"; else echo "FAIL: getmouselocation rc=$?"; fi
echo "=== xdotool key a (XTEST + keymap lookup) ==="
if xdotool key a; then echo "PASS: key a"; else echo "FAIL: key a rc=$?"; fi
echo "=== xdotool mousemove + click ==="
if xdotool mousemove 100 100 click 1; then echo "PASS: mousemove+click"; else echo "FAIL: mousemove+click rc=$?"; fi
echo "=== xmodmap -pke head (libX11 keymap consumer) ==="
# capture first, then head — piping xmodmap straight into head makes
# it exit 141 (SIGPIPE) and false-FAILs the check
if xmodmap -pke > /tmp/xmodmap-pke.out 2>/dev/null; then
    head -3 /tmp/xmodmap-pke.out
    echo "PASS: xmodmap"
else
    echo "FAIL: xmodmap rc=$?"
fi

kill $pid 2>/dev/null
wait $pid 2>/dev/null
echo "vng xkb test done"
