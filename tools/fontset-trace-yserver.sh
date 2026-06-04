#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
RUST_LOG=info YSERVER_RENDER_MODEL=v2 YSERVER_MODE=1024x768 target/debug/yserver > /dev/null 2>&1 &
ypid=$!
for _ in $(seq 1 150); do DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
export DISPLAY=:7
x11trace -n -o /tmp/fontset-yserver.trace -- /tmp/fontset-probe
echo "probe rc=$?"
grep -E "Request\((45|49|50)\)|Reply to (ListFonts|OpenFont|QueryFont)|GetAtomName|Error" /tmp/fontset-yserver.trace | cut -c1-220
kill $ypid 2>/dev/null
