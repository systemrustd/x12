#!/usr/bin/env bash
# Profile a full mate-session run under yserver with system-wide perf,
# so we can see why MATE startup is slow. Captures both on-CPU samples
# (cpu-clock) and scheduler-switch events in one trace — covering
# yserver, mate-session, marco, mate-panel, caja, dbus-daemon and every
# helper. Records from yserver boot until you log out of MATE.
#
# Mirrors the build flags from `just yserver-mate-hw-release`: release
# build with `-C force-frame-pointers=yes` and `perf --call-graph fp`.
# Do NOT swap to `--call-graph dwarf` — optimised Rust gives ~66%
# [unknown] frames there (see comment on yserver-mate-hw-release).
#
# Requires sudo: `perf record -a` needs CAP_PERFMON or
# kernel.perf_event_paranoid <= 1. The perf.data is chowned back to
# you on teardown.
#
# Args (env vars):
#   RUST_LOG   default: warn   (keeps env_logger out of the profile)
#   PERF_FREQ  default: 999
#   STRACE     default: 0      (set STRACE=1 to attach strace to caja
#                               the moment it spawns; dumps poll/recvmsg/
#                               sendmsg/read/futex/connect with fd-paths
#                               to caja.strace, which usually answers
#                               "what is caja sitting in poll() on")
#   DBUS_MONITOR default: 0    (set DBUS_MONITOR=1 to spawn `dbus-monitor`
#                               on the isolated session bus, writing
#                               every method_call and method_return to
#                               dbus-monitor.log — best signal for
#                               "which dbus call is hanging")
#
# Outputs (in project root):
#   yserver-mate.perf.data   perf record
#   yserver-mate.perf.log    perf stderr (sample loss etc.)
#   yserver-hw.log           yserver stdout/stderr
#   mate.log                 mate-session stdout/stderr
#   caja.strace              strace of caja (only with STRACE=1)
#   dbus-monitor.log         session-bus traffic (only with DBUS_MONITOR=1)
#
# Analyse (kernel-symbol resolution needs --vmlinux on custom kernels;
# Arch's debuginfod handles in-tree distro libs but not all of them —
# yserver's own symbols always resolve since it's built unstripped):
#
#   VMLINUX=/path/to/linux/vmlinux  # your kernel source tree
#   export DEBUGINFOD_URLS=https://debuginfod.archlinux.org
#
#   perf report -i yserver-mate.perf.data --vmlinux="$VMLINUX"
#
#   # Off-CPU view — most useful for slow-startup work since startup
#   # is dominated by waits, not CPU. This dumps per-thread stacks
#   # at every sched_switch:
#   perf script -i yserver-mate.perf.data --vmlinux="$VMLINUX" \
#     --no-inline -F comm,event,ip,sym,dso \
#     | awk 'BEGIN{RS=""} /sched_switch/'
#
#   # Flamegraph (needs Brendan Gregg's FlameGraph repo on PATH):
#   perf script -i yserver-mate.perf.data --vmlinux="$VMLINUX" --no-inline \
#     | stackcollapse-perf.pl | flamegraph.pl > on-cpu.svg

set -u

NEST_DISPLAY=":7"
PERF_DATA="yserver-mate.perf.data"
PERF_LOG="yserver-mate.perf.log"
YSERVER_LOG="yserver-hw.log"
MATE_LOG="mate.log"

RUST_LOG="${RUST_LOG:-warn}"
PERF_FREQ="${PERF_FREQ:-999}"
STRACE="${STRACE:-0}"
DBUS_MONITOR="${DBUS_MONITOR:-0}"
CAJA_STRACE="caja.strace"
DBUS_MONITOR_LOG="dbus-monitor.log"

if ! command -v perf >/dev/null; then
    echo "perf not installed" >&2
    exit 1
fi

if [[ "$STRACE" == "1" ]] && ! command -v strace >/dev/null; then
    echo "STRACE=1 set but strace not installed" >&2
    exit 1
fi

if [[ "$DBUS_MONITOR" == "1" ]] && ! command -v dbus-monitor >/dev/null; then
    echo "DBUS_MONITOR=1 set but dbus-monitor not installed" >&2
    exit 1
fi

paranoid="$(cat /proc/sys/kernel/perf_event_paranoid)"
if [ "$paranoid" -gt 1 ]; then
    echo "note: kernel.perf_event_paranoid=$paranoid; perf record -a requires sudo"
fi

echo "priming sudo for perf record..."
if ! sudo -v; then
    echo "sudo required for system-wide perf record" >&2
    exit 1
fi

# Build the event list. cpu-clock is a software event, always
# available. sched:sched_switch is a tracepoint, requires CONFIG_FTRACE
# on the running kernel. Without it we can only do on-CPU profiling.
PERF_EVENTS="cpu-clock"
if sudo test -e /sys/kernel/tracing/events/sched/sched_switch ||
   sudo test -e /sys/kernel/debug/tracing/events/sched/sched_switch; then
    PERF_EVENTS="cpu-clock,sched:sched_switch"
else
    echo "warning: sched:sched_switch tracepoint missing on this kernel" \
         "(needs CONFIG_FTRACE+CONFIG_SCHEDSTATS); recording on-CPU only." \
         "Off-CPU / wait-time view will not be available." >&2
fi

# Build release yserver with frame pointers (essential for usable
# Rust stacks under perf).
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver

rm -f "$PERF_DATA" "$PERF_LOG"

cleanup() {
    if [[ -n "${STRACE_WATCHER_PID:-}" ]]; then
        kill -TERM "$STRACE_WATCHER_PID" 2>/dev/null || true
        wait "$STRACE_WATCHER_PID" 2>/dev/null || true
    fi
    if [[ -n "${STRACE_PID:-}" ]]; then
        sudo kill -TERM "$STRACE_PID" 2>/dev/null || true
        wait "$STRACE_PID" 2>/dev/null || true
    fi
    if [[ -n "${PERF_PID:-}" ]]; then
        sudo kill -INT "$PERF_PID" 2>/dev/null || true
        wait "$PERF_PID" 2>/dev/null || true
    fi
    if [[ -n "${YSERVER_PID:-}" ]]; then
        kill -TERM "$YSERVER_PID" 2>/dev/null || true
        wait "$YSERVER_PID" 2>/dev/null || true
    fi
    if [[ -f "$PERF_DATA" ]]; then
        sudo chown "$(id -u):$(id -g)" "$PERF_DATA" 2>/dev/null || true
        echo
        echo "perf.data: $(pwd)/$PERF_DATA ($(du -h "$PERF_DATA" | cut -f1))"
        echo "next: perf report -i $PERF_DATA"
    fi
    if [[ -f "$CAJA_STRACE" ]]; then
        sudo chown "$(id -u):$(id -g)" "$CAJA_STRACE" 2>/dev/null || true
        echo "caja.strace: $(pwd)/$CAJA_STRACE ($(du -h "$CAJA_STRACE" | cut -f1))"
    fi
}
trap cleanup EXIT INT TERM

# Spawn yserver.
RUST_LOG="$RUST_LOG" RUST_BACKTRACE=1 \
    target/release/yserver >"$YSERVER_LOG" 2>&1 &
YSERVER_PID=$!
sleep 2

# Spawn perf. Events selected above based on kernel support.
sudo perf record -a -g --call-graph fp -F "$PERF_FREQ" \
    -e "$PERF_EVENTS" \
    -o "$PERF_DATA" 2>"$PERF_LOG" &
PERF_PID=$!
sleep 1

# Bail out if perf failed to start — otherwise we waste a session
# logging into mate with nothing recorded.
if ! sudo kill -0 "$PERF_PID" 2>/dev/null; then
    echo "perf record died on launch; see $PERF_LOG:" >&2
    cat "$PERF_LOG" >&2
    kill -TERM "$YSERVER_PID" 2>/dev/null || true
    wait "$YSERVER_PID" 2>/dev/null || true
    exit 1
fi

echo "perf:    $PERF_DATA (freq ${PERF_FREQ}Hz, $PERF_EVENTS)"
echo "yserver: $YSERVER_LOG (RUST_LOG=$RUST_LOG)"
echo "mate:    $MATE_LOG"

# Optional: watch for caja and strace it the moment it spawns.
# Run as sudo because Arch's default ptrace_scope=1 only lets parents
# attach; sudo bypasses that. Background loop polls pgrep ~10x/s.
if [[ "$STRACE" == "1" ]]; then
    rm -f "$CAJA_STRACE"
    (
        for _ in $(seq 1 300); do
            pid="$(pgrep -n -x caja 2>/dev/null || true)"
            if [[ -n "$pid" ]]; then
                # exec into strace so $STRACE_WATCHER_PID == strace PID
                exec sudo strace -p "$pid" -f -ttt -yy -s 0 \
                    -e trace=poll,ppoll,recvmsg,sendmsg,read,write,futex,connect,openat \
                    -o "$CAJA_STRACE" 2>/dev/null
            fi
            sleep 0.1
        done
        echo "strace watcher: caja never appeared (gave up after 30s)" >&2
    ) &
    STRACE_WATCHER_PID=$!
    echo "strace:  watching for caja, will write $CAJA_STRACE"
fi

if [[ "$DBUS_MONITOR" == "1" ]]; then
    rm -f "$DBUS_MONITOR_LOG"
    echo "dbus:    monitoring isolated session bus, will write $DBUS_MONITOR_LOG"
fi

echo
echo "Quit MATE (logout, or Ctrl+Alt+Backspace) to stop the profile."

# Run mate-session in the foreground; the trap fires on its exit.
# If DBUS_MONITOR=1, wrap in a bash so we can spawn `gdbus monitor`
# inside the dbus-run-session (sees the isolated bus, not the host bus).
# Fresh XDG_RUNTIME_DIR so the nested mate session doesn't fight the
# host's GNOME-Wayland session over /run/user/1000/{doc,gvfs,...}.
# Without this, xdg-desktop-portal's startup stalls ~25s because its
# Documents FUSE mount can't claim /run/user/1000/doc (already held
# by the host portal), and caja's StartServiceByName waits the full
# default GDBus 25s timeout for the portal to register on the bus.
NESTED_RUNTIME_DIR="$(mktemp -d -t yserver-run.XXXXXX)"
chmod 700 "$NESTED_RUNTIME_DIR"

cleanup_runtime_dir() {
    rm -rf "$NESTED_RUNTIME_DIR" 2>/dev/null || true
}
trap 'cleanup; cleanup_runtime_dir' EXIT INT TERM

echo "runtime: $NESTED_RUNTIME_DIR (isolates from host /run/user/1000)"

if [[ "$DBUS_MONITOR" == "1" ]]; then
    env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
        DISPLAY="$NEST_DISPLAY" GDK_BACKEND=x11 XDG_SESSION_TYPE=x11 \
        XDG_RUNTIME_DIR="$NESTED_RUNTIME_DIR" \
        DBUS_MONITOR_LOG_FILE="$DBUS_MONITOR_LOG" \
        dbus-run-session bash -c '
            dbus-monitor --session > "$DBUS_MONITOR_LOG_FILE" 2>&1 &
            exec mate-session --display "'"$NEST_DISPLAY"'"
        ' >"$MATE_LOG" 2>&1 || true
else
    env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
        DISPLAY="$NEST_DISPLAY" GDK_BACKEND=x11 XDG_SESSION_TYPE=x11 \
        XDG_RUNTIME_DIR="$NESTED_RUNTIME_DIR" \
        dbus-run-session mate-session --display "$NEST_DISPLAY" \
        >"$MATE_LOG" 2>&1 || true
fi
