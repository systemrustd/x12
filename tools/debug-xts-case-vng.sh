#!/usr/bin/env bash
# Run a single xts5 test case against yserver (KMS) inside a vng guest
# with the TCM under gdb, so a client-side SIGSEGV is caught before
# TET's handler swallows it. Guest-side counterpart of
# debug-xts-case-hw.sh — use that one when running natively on a TTY.
#
# Run from the host (adjust kernel/GPU device per machine — see the
# xts-yserver recipe in the Justfile; on aarch64 hosts substitute
# virtio-gpu-gl-pci for virtio-vga-gl):
#   vng -r <kernel> --disable-microvm --rw \
#     --qemu-opts="-display egl-headless,gl=on -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
#     -- tools/debug-xts-case-vng.sh Xlib11/ButtonPress
#
# Artifacts (host-visible via the --rw rootfs mount):
#   yserver-vng-<case>.log    — server debug log
#   <case>-gdb.txt            — gdb backtrace if the TCM crashes
#   /home/jos/Projects/xts/results/repro-vng-*/ — journal + per-test log
set -uo pipefail

CASE_PATH=${1:?usage: debug-xts-case-vng.sh <Section/Case>, e.g. Xlib11/ButtonPress}
CASE_NAME=$(basename "$CASE_PATH")
XTS=/home/jos/Projects/xts
YSRV=/home/jos/Projects/yserver
WRAPPER="$XTS/xts5/$CASE_PATH"
BIN="$XTS/xts5/$(dirname "$CASE_PATH")/.libs/$CASE_NAME"
GDB_OUT="$YSRV/${CASE_NAME}-gdb.txt"

[ -x "$WRAPPER" ] || { echo "no such test wrapper: $WRAPPER" >&2; exit 1; }
[ -x "$BIN" ] || { echo "no such test binary: $BIN" >&2; exit 1; }

cd "$YSRV"
RUST_LOG=debug RUST_BACKTRACE=1 target/release/yserver > "yserver-vng-${CASE_NAME}.log" 2>&1 &
ypid=$!

# yserver listens on /tmp/.X11-unix/X7 in the guest; KMS modeset can
# take several seconds on first boot.
for _ in $(seq 1 150); do
    DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break
    sleep 0.2
done
if ! DISPLAY=:7 xdpyinfo >/dev/null 2>&1; then
    echo "error: yserver did not come up on :7" >&2
    tail -30 "yserver-vng-${CASE_NAME}.log" >&2 || true
    kill -TERM $ypid 2>/dev/null
    exit 2
fi

cd "$XTS"
export TET_ROOT=$PWD TET_EXECUTE=$PWD/xts5

# Fresh cfg against the live display (stale cfg = wrong DISPLAY baked in).
rm -f xts5/tetexec.cfg
DISPLAY=:7 perl -p xts5/bin/xts-config < xts5/tetexec.cfg.in > xts5/tetexec.cfg

# Swap the libtool wrapper for a gdb shim; restore on exit.
mv "$WRAPPER" "$WRAPPER.orig"
cat > "$WRAPPER" <<SHIM
#!/bin/sh
export LD_LIBRARY_PATH=$XTS/xts5/src/.libs:$XTS/src/tet3/apilib/.libs:\$LD_LIBRARY_PATH
export DEBUGINFOD_URLS="https://debuginfod.archlinux.org"
exec gdb -batch \\
  -ex 'set debuginfod enabled on' \\
  -ex 'handle SIGSEGV stop nopass' \\
  -ex run \\
  -ex 'bt full' \\
  -ex 'info registers' \\
  -ex 'x/4i \$pc' \\
  --args $BIN "\$@" \\
  > $GDB_OUT 2>&1
SHIM
chmod +x "$WRAPPER"
trap "kill -TERM $ypid 2>/dev/null; mv -f '$WRAPPER.orig' '$WRAPPER'" EXIT

outdir=$PWD/results/repro-vng-$(date +%H%M%S)
mkdir -p "$outdir"
DISPLAY=:7 timeout 300 src/tet3/tcc/tcc -e -i "$outdir" -x xts5/tetexec.cfg xts5 "$CASE_NAME"
echo "tcc exit: $?"

echo "==== gdb output (first 50 lines) ===="
head -50 "$GDB_OUT" 2>/dev/null || echo "(no gdb output)"
echo "==== journal tail ===="
tail -15 "$outdir/journal" 2>/dev/null
