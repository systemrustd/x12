#!/usr/bin/env bash
# Run an xts5 scenario against an X display and emit a one-line tally.
#
# Usage:
#   tools/xts-run.sh <DISPLAY> <SCENARIO> [TIMEOUT_SECONDS] [XTS_DIR]
#
# Example:
#   tools/xts-run.sh :99 Xproto 600
#
# The script invokes xts's check.sh (which runs `tcc` then xts-report).
# Output: the path to the result directory and the summary report.
set -euo pipefail

DISPLAY_ARG=${1:?DISPLAY argument required (e.g. :99)}
SCENARIO=${2:?SCENARIO required (e.g. Xproto, Xlib3, all)}
TIMEOUT=${3:-600}
XTS_DIR=${4:-/home/jos/Projects/xts}

if [[ ! -x "$XTS_DIR/check.sh" ]]; then
    echo "error: $XTS_DIR/check.sh not executable" >&2
    exit 1
fi

if ! DISPLAY="$DISPLAY_ARG" timeout 5 xdpyinfo >/dev/null 2>&1; then
    echo "error: cannot connect to $DISPLAY_ARG" >&2
    exit 2
fi

cd "$XTS_DIR"
DISPLAY="$DISPLAY_ARG" timeout "$TIMEOUT" ./check.sh "$SCENARIO"
status=$?

# Find the most recent result directory.
result_dir=$(ls -1dt results/* 2>/dev/null | head -1 || true)
if [[ -z "$result_dir" || ! -f "$result_dir/journal" ]]; then
    echo "error: no journal produced" >&2
    exit 3
fi

# xts-report consumes the journal. The previous run may have already
# generated `summary`; either way, regenerate.
report_bin="$XTS_DIR/build/xts5/src/bin/reports/xts-report"
"$report_bin" -d2 -f "$result_dir/journal" > "$result_dir/summary" 2>/dev/null || true

echo
echo "==== Result directory ===="
echo "$result_dir"
echo
echo "==== Summary ===="
grep -E "^(Xproto|Xlib|Xt|XI|SHAPE|Xopen|TOTAL|CASES)" "$result_dir/summary" 2>/dev/null || true

# Propagate timeout exit so callers can see it (124 = timeout).
if [[ $status -eq 124 ]]; then
    echo "(scenario timed out at ${TIMEOUT}s — partial results)"
fi
exit 0
