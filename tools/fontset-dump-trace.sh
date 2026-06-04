#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
RUST_LOG=info YSERVER_RENDER_MODEL=v2 YSERVER_MODE=1024x768 target/debug/yserver > /dev/null 2>&1 &
ypid=$!
for _ in $(seq 1 150); do DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
DISPLAY=:7 x11trace -n -o /tmp/t.trace -- /tmp/fontset-probe >/dev/null 2>&1 || true
grep -A0 "Reply to ListFontsWithInfo: min-bounds" /tmp/t.trace
kill $ypid 2>/dev/null
