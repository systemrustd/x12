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
# Default per-test timeout is 600s: rendercheck 1.6 has ~3-4× more
# composite cases than 1.5 and the `composite` / `cacomposite` tests
# walk operator × format × source enumerations that take several
# minutes wall-time. Override with the second arg if you want a
# tighter budget.
set -euo pipefail

DISPLAY_ARG=${1:?DISPLAY argument required (e.g. :99)}
TIMEOUT=${2:-600}
TESTS=${3:-fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366}

# rendercheck 1.5 (current AUR / Arch package) has a bug in the
# triangles test that mis-grades Disjoint/Conjoint operator cases —
# both Xwayland and ynest "fail" the same 144 cases under 1.5 even
# though the actual rendering is correct. Upstream commit 3d7add9
# fixes the test. To run against an upstream build, set
# `RENDERCHECK_BIN=/path/to/rendercheck-1.6`.
RENDERCHECK="${RENDERCHECK_BIN:-rendercheck}"

if ! command -v "$RENDERCHECK" >/dev/null 2>&1 && ! [ -x "$RENDERCHECK" ]; then
	echo "error: rendercheck not on PATH (pacman -S rendercheck) and \$RENDERCHECK_BIN not set" >&2
	exit 1
fi

if ! DISPLAY="$DISPLAY_ARG" timeout 5 xdpyinfo >/dev/null 2>&1; then
	echo "error: cannot connect to $DISPLAY_ARG" >&2
	exit 2
fi

DISPLAY=$DISPLAY_ARG xset s off -dpms
declare -i total_pass=0 total_seen=0 incomplete=0

printf "%-14s %8s %8s %s\n" "test" "pass" "total" "status"
printf "%-14s %8s %8s %s\n" "----" "----" "-----" "------"

IFS=',' read -ra test_list <<<"$TESTS"
for t in "${test_list[@]}"; do
	out=$(DISPLAY="$DISPLAY_ARG" timeout "$TIMEOUT" \
		"$RENDERCHECK" -v -t "$t" --minimalrendering 2>&1) || rc=$?
	LOG_DIR="/home/jos/Projects/yserver/target/rc-logs"
	mkdir -p "$LOG_DIR" 2>/dev/null
	echo "$out" >"$LOG_DIR/rc-$t.log" 2>/dev/null || true
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
