#!/usr/bin/env bash
# Run a sweep of xts5 scenarios with a per-scenario overall timeout,
# emitting one row per scenario. Useful for capturing a coarse Xlib4-17
# baseline without per-test timeouts (which kill normally-quick tests
# mid-flight and skew the result columns).
#
# Usage:
#   tools/xts-xlib-sweep.sh <DISPLAY> [SCENARIO_TIMEOUT] [SCENARIOS...]
#
# Example (default = all of Xlib4-17 minus Xlib3, which is its own row):
#   tools/xts-xlib-sweep.sh :99
#
# Cases that hang ynest will steal the remaining wall-clock budget for
# their scenario; partial results land in the result_dir column.
set -uo pipefail

DISPLAY_ARG=${1:?DISPLAY argument required (e.g. :99)}
SCENARIO_TIMEOUT=${2:-240}
shift 2 2>/dev/null || shift $#
SCENARIOS=${*:-Xlib4 Xlib5 Xlib6 Xlib7 Xlib8 Xlib9 Xlib10 Xlib11 Xlib12 Xlib13 Xlib14 Xlib15 Xlib16 Xlib17}
export TET_ROOT=${TET_ROOT:-/home/jos/Projects/xts}
export TET_EXECUTE=$TET_ROOT/xts5
TCC=$TET_ROOT/src/tet3/tcc/tcc
REPORT=$TET_ROOT/xts5/src/bin/reports/xts-report
config=$TET_ROOT/xts5/tetexec.cfg

cd "$TET_ROOT"

if ! DISPLAY="$DISPLAY_ARG" timeout 3 xdpyinfo >/dev/null 2>&1; then
    echo "error: cannot connect to $DISPLAY_ARG" >&2
    exit 2
fi

printf "%-8s %5s %5s %5s %5s %5s %5s %5s %s\n" \
    scenario CASES TESTS PASS FAIL UNRES UNTST UNSUP result_dir
for s in $SCENARIOS; do
    if ! DISPLAY="$DISPLAY_ARG" timeout 3 xdpyinfo >/dev/null 2>&1; then
        printf "%-8s ynest_dead — skipping rest\n" "$s"
        break
    fi
    outdir=$TET_ROOT/results/$(date +%F-%T)
    mkdir -p "$outdir"
    DISPLAY="$DISPLAY_ARG" timeout "$SCENARIO_TIMEOUT" \
        "$TCC" -e -i "$outdir" -x "$config" xts5 "$s" >/dev/null 2>&1 || true
    pkill -9 -f "/xts5/$s/" 2>/dev/null || true
    sleep 1
    read -r cases tests pass unsup untst notiu warn fip fail unres unin abort < <(
        "$REPORT" -d2 -f "$outdir/journal" 2>/dev/null \
            | awk -v s="$s" '$1==s {print $2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13}'
    )
    printf "%-8s %5s %5s %5s %5s %5s %5s %5s %s\n" \
        "$s" "${cases:-?}" "${tests:-?}" "${pass:-?}" "${fail:-?}" \
        "${unres:-?}" "${untst:-?}" "${unsup:-?}" "${outdir##*/}"
done
