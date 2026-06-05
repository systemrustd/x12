#!/usr/bin/env bash
# Run a conformance harness against yserver (KMS) inside a virtme-ng
# guest. Used by `just xts-yserver` / `just rendercheck-yserver`.
#
# vng mounts the host rootfs --rw, so /home/jos/Projects/{xts,yserver}
# and `rendercheck` on PATH are reachable from the guest with no extra
# wiring. Result trees land back on the host filesystem.
#
# Usage (called by the Justfile recipes):
#   tools/yserver-vng-run.sh xts <SCENARIO> [TIMEOUT]
#   tools/yserver-vng-run.sh rendercheck <TIMEOUT> <TESTS>
set -euo pipefail

HARNESS=${1:?harness required: 'xts' or 'rendercheck'}
shift

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

cd "$repo_root"
# vng's guest /tmp is a fresh tmpfs — anything written to /tmp inside
# the guest is invisible to the host post-exit. Stash yserver's
# stderr/stdout in the project tree (host-mounted --rw via vng), so
# post-mortems / crash logs survive. Env-overrideable for ad-hoc runs:
#   YSERVER_VNG_LOG=/path/to/log  YSERVER_VNG_RUST_LOG=trace just rendercheck-yserver ...
YSERVER_LOG="${YSERVER_VNG_LOG:-$repo_root/yserver-vng.log}"
RUST_LOG="${YSERVER_VNG_RUST_LOG:-warn}" RUST_BACKTRACE=1 target/release/yserver > "$YSERVER_LOG" 2>&1 &
pid=$!
trap "kill $pid 2>/dev/null; wait $pid 2>/dev/null || true" EXIT

# yserver listens on /tmp/.X11-unix/X7. KMS modeset can take several
# seconds on first boot — poll for up to 30 s.
for _ in $(seq 1 150); do
    DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break
    sleep 0.2
done
if ! DISPLAY=:7 xdpyinfo >/dev/null 2>&1; then
    echo "error: yserver did not come up on :7" >&2
    tail -30 "$YSERVER_LOG" >&2 || true
    exit 2
fi

case "$HARNESS" in
    xts)         tools/xts-run.sh :7 "$@" ;;
    rendercheck) tools/rendercheck.sh :7 "$@" ;;
    *) echo "unknown harness: $HARNESS" >&2; exit 2 ;;
esac
