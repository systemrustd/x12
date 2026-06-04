#!/usr/bin/env bash
# Scratch script: run a single xts5 test case against yserver (KMS) on
# this machine's GPU with the TCM under gdb, so a client-side SIGSEGV
# is caught before TET's handler swallows it. Generalized from
# debug-xgetimage-hw.sh (which caught the Xlib9/XGetImage libX11 NULL
# deref the same way).
#
# Usage, from a TTY (VT switch, e.g. Ctrl-Alt-F3):
#   cd ~/Projects/yserver && tools/debug-xts-case-hw.sh Xlib11/ButtonPress
#
# The scenario name passed to tcc is the basename (e.g. ButtonPress) —
# it must exist as a named scenario in xts5/tet_scen (per-case names do).
#
# Artifacts:
#   yserver-hw-<case>.log     — server debug log
#   <case>-gdb.txt            — gdb backtrace if the TCM crashes
#   /home/jos/Projects/xts/results/repro-hw-*/ — journal + per-test log
set -uo pipefail

CASE_PATH=${1:?usage: debug-xts-case-hw.sh <Section/Case>, e.g. Xlib11/ButtonPress}
CASE_NAME=$(basename "$CASE_PATH")
XTS=/home/jos/Projects/xts
YSRV=/home/jos/Projects/yserver
WRAPPER="$XTS/xts5/$CASE_PATH"
BIN="$XTS/xts5/$(dirname "$CASE_PATH")/.libs/$CASE_NAME"
GDB_OUT="$YSRV/${CASE_NAME}-gdb.txt"

[ -x "$WRAPPER" ] || { echo "no such test wrapper: $WRAPPER" >&2; exit 1; }
[ -x "$BIN" ] || { echo "no such test binary: $BIN" >&2; exit 1; }
grep -qx "$CASE_NAME" "$XTS/xts5/tet_scen" || {
    echo "warning: '$CASE_NAME' not a named scenario in tet_scen" >&2
}

case "$(tty)" in
    /dev/tty[0-9]*) ;;
    *) echo "must be run from a TTY (got: $(tty))" >&2; exit 1 ;;
esac

cd "$YSRV"
display=0
while [ -e "/tmp/.X11-unix/X$display" ]; do display=$((display+1)); done
echo "using DISPLAY=:$display"

RUST_LOG=debug RUST_BACKTRACE=1 target/release/yserver "$display" \
    > "yserver-hw-${CASE_NAME}.log" 2>&1 &
ypid=$!

for _ in $(seq 30); do [ -S "/tmp/.X11-unix/X$display" ] && break; sleep 1; done
if ! DISPLAY=":$display" timeout 5 xdpyinfo >/dev/null 2>&1; then
    echo "error: yserver did not come up on :$display" >&2
    tail -30 "yserver-hw-${CASE_NAME}.log" >&2 || true
    kill -TERM $ypid 2>/dev/null
    exit 2
fi
DISPLAY=":$display" xset s off -dpms || true

cd "$XTS"
export TET_ROOT=$PWD TET_EXECUTE=$PWD/xts5

# Fresh cfg against the live display (stale cfg = wrong DISPLAY baked in).
rm -f xts5/tetexec.cfg
DISPLAY=":$display" perl -p xts5/bin/xts-config < xts5/tetexec.cfg.in > xts5/tetexec.cfg

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

outdir=$PWD/results/repro-hw-$(date +%H%M%S)
mkdir -p "$outdir"
DISPLAY=":$display" timeout 300 src/tet3/tcc/tcc -e -i "$outdir" -x xts5/tetexec.cfg xts5 "$CASE_NAME"
echo "tcc exit: $?"

echo "==== gdb output (first 50 lines) ===="
head -50 "$GDB_OUT" 2>/dev/null || echo "(no gdb output)"
echo "==== journal tail ===="
tail -15 "$outdir/journal" 2>/dev/null
