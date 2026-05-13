#!/usr/bin/env bash
# Capture AMD GPU memory placement + PCIe traffic snapshots while
# yserver-mate-hw is running.
#
# Used to discriminate where yserver's scanout BOs actually live
# (VRAM vs GTT) and whether scanout traffic crosses PCIe. The
# scanout-BO-via-Vulkan-linear-export path was originally suspected
# of placing BOs in GTT on discrete Polaris, which would route scanout
# DMA across PCIe and risk bandwidth-marginal display refresh at
# dual 2560x1440. Empirically the suspicion turned out wrong; this
# script exists to capture the data definitively if the question
# comes up again.
#
# Usage:
#   tools/amdgpu-bo-placement.sh [snapshot|live|trace] [args...]
#
#   snapshot               One-shot JSON dump. Filtered to the fields
#                          we care about: GPU activity, VRAM, GTT,
#                          PCIe TX/RX. Capture happens immediately.
#
#   trace [DELAY] [COUNT]  Sleep DELAY seconds, then capture COUNT
#                          one-per-second snapshots into a single
#                          JSON-lines file. Use this to capture data
#                          during an interaction you can't initiate
#                          and tap a terminal at the same time
#                          (e.g. mate-control-center hover lag — by
#                          the time you've moved the mouse to the
#                          terminal, the hover state is gone).
#                          Defaults: DELAY=5, COUNT=10.
#                          Workflow:
#                            1. tools/amdgpu-bo-placement.sh trace 5 10 &
#                            2. switch to mate-control-center and
#                               begin hovering rows immediately
#                            3. five seconds later the script begins
#                               capturing for ten seconds; the file
#                               path is printed when it finishes
#
#   live                   Interactive amdgpu_top — press q to quit.
#
# Expected readings for the "is the scanout BO in VRAM or GTT?"
# question, with one MATE session running on a discrete Polaris:
#
#   VRAM placement: GTT used hovers in the tens of MB; PCIe RX
#   stays near idle (≤100 MB/s).
#
#   GTT placement: GTT used grows by ~14 MB per scanout BO times the
#   buffer-pool depth (3) times the number of outputs; PCIe RX
#   sustains around 1.7 GB/s per output during normal compositing.
#
# A snapshot during three scenarios is most informative:
#   1. MATE idle (no animation, cursor parked).
#   2. wezterm scrolling a `cat /usr/share/dict/words` (continuous paint).
#   3. mate-control-center hover over a row (cascade of small RENDER ops).

set -u

mode=${1:-snapshot}

case "$mode" in
    snapshot)
        if ! command -v amdgpu_top >/dev/null 2>&1; then
            echo "amdgpu_top not installed (Arch: pacman -S amdgpu_top)" >&2
            exit 1
        fi
        # `amdgpu_top -d -J` (--dump --json) emits a one-shot JSON
        # array of devices with VRAM/GTT usage, gpu_activity %, PCIe
        # link state, sensors. The "--single" flag is "--single-gpu"
        # (filter to one device), not "one-shot mode" — easy misread.
        # Live PCIe TX/RX traffic numbers are only in the TUI mode;
        # the JSON dump carries link config + memory usage only.
        if ! command -v jq >/dev/null 2>&1; then
            amdgpu_top -d -J 2>/dev/null
            exit 0
        fi
        amdgpu_top -d -J 2>/dev/null \
            | jq '.[] | {
                gpu: .DeviceName,
                vram_used_mib: .VRAM["Total VRAM Usage"].value,
                vram_total_mib: .VRAM["Total VRAM"].value,
                gtt_used_mib: .VRAM["Total GTT Usage"].value,
                gtt_total_mib: .VRAM["Total GTT"].value,
                gfx_pct: .gpu_activity.GFX.value,
                memory_pct: .gpu_activity.Memory.value,
                pcie_link: .Sensors["PCIe Link Speed"]
              }'
        ;;
    live)
        if ! command -v amdgpu_top >/dev/null 2>&1; then
            echo "amdgpu_top not installed (Arch: pacman -S amdgpu_top)" >&2
            exit 1
        fi
        exec amdgpu_top
        ;;
    trace)
        if ! command -v amdgpu_top >/dev/null 2>&1; then
            echo "amdgpu_top not installed (Arch: pacman -S amdgpu_top)" >&2
            exit 1
        fi
        delay=${2:-5}
        count=${3:-10}
        # mktemp templates need the X's in the FILENAME, not after a
        # suffix; "trace-XXXXXX.jsonl" gets rejected on some mktemp
        # versions. Generate a unique name explicitly so it works.
        out="/tmp/amdgpu-trace-$(date +%s)-$$.jsonl"
        echo "amdgpu-bo-placement: sleeping ${delay}s, then ${count} samples → $out" >&2
        sleep "$delay"
        for ((i = 0; i < count; i++)); do
            amdgpu_top -d -J 2>/dev/null >> "$out"
            sleep 1
        done
        echo "$out"
        ;;
    *)
        echo "Usage: $0 [snapshot|live|trace [delay] [count]]" >&2
        exit 2
        ;;
esac
