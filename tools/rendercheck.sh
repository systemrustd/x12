#!/usr/bin/env bash
# Run rendercheck against an X display and emit a per-test tally.
#
# rendercheck (https://gitlab.freedesktop.org/xorg/test/rendercheck) is a
# small smoke suite for the X RENDER extension. Each test category prints
# "<n> tests passed of <m> total" on completion; we grep that line so the
# summary stays one row per test.
#
# Usage:
#   tools/rendercheck.sh <DISPLAY> [TIMEOUT_SECONDS] [TESTS]
#
# Examples:
#   tools/rendercheck.sh :99
#   tools/rendercheck.sh :99 60 fill,dcoords,blend
#
# The default test list excludes `repeat` (which runs every operator
# against every format and routinely takes >120s even on the host) and
# `cacomposite` (component-alpha composite — same shape, slow). Pass an
# explicit TESTS list to include them.
set -euo pipefail

DISPLAY_ARG=${1:?DISPLAY argument required (e.g. :99)}
TIMEOUT=${2:-90}
TESTS=${3:-fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,gradients,triangles,bug7366}

if ! command -v rendercheck >/dev/null 2>&1; then
    echo "error: rendercheck not on PATH (pacman -S rendercheck)" >&2
    exit 1
fi

if ! DISPLAY="$DISPLAY_ARG" timeout 5 xdpyinfo >/dev/null 2>&1; then
    echo "error: cannot connect to $DISPLAY_ARG" >&2
    exit 2
fi

declare -i total_pass=0 total_seen=0 incomplete=0

printf "%-14s %8s %8s %s\n" "test" "pass" "total" "status"
printf "%-14s %8s %8s %s\n" "----" "----" "-----" "------"

IFS=',' read -ra test_list <<< "$TESTS"
for t in "${test_list[@]}"; do
    out=$(DISPLAY="$DISPLAY_ARG" timeout "$TIMEOUT" \
        rendercheck -t "$t" --minimalrendering 2>&1) || rc=$?
    rc=${rc:-0}
    summary=$(echo "$out" | grep -E "tests passed of [0-9]+" | tail -1 || true)
    if [[ -z "$summary" ]]; then
        printf "%-14s %8s %8s %s\n" "$t" "?" "?" "INCOMPLETE (rc=$rc)"
        incomplete+=1
        continue
    fi
    pass=$(echo "$summary" | grep -oE "^[0-9]+")
    tot=$(echo "$summary" | grep -oE "of [0-9]+" | grep -oE "[0-9]+")
    total_pass+=$pass
    total_seen+=$tot
    status="OK"
    if [[ $rc -eq 124 ]]; then
        status="TIMEOUT-after-${TIMEOUT}s"
    elif [[ $rc -ne 0 ]]; then
        status="FAIL (rc=$rc)"
    elif [[ "$pass" != "$tot" ]]; then
        status="MISMATCH"
    fi
    printf "%-14s %8d %8d %s\n" "$t" "$pass" "$tot" "$status"
    rc=0
done

printf "%-14s %8s %8s %s\n" "----" "----" "-----" "------"
printf "%-14s %8d %8d (%d test(s) incomplete)\n" "TOTAL" "$total_pass" "$total_seen" "$incomplete"
