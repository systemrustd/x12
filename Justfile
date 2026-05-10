KERNEL := "/boot/vmlinuz-linux-cachyos"

# Run yserver in virtme-ng with virtio-gpu DRM device and a QEMU window.
yserver:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- target/debug/yserver

# Run yserver in virtme-ng headless; stdout/stderr reach the host terminal.
yserver-headless:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- target/debug/yserver

# Run yserver in virtme-ng headless with sshd inside the guest.
# Pair with `just yserver-ssh-shell` in a second terminal to send signals
# (e.g. `pkill -TERM yserver`) and exercise the clean-shutdown path.
yserver-headless-ssh:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw --ssh \
        --qemu-opts="-device virtio-gpu-pci" \
        -- target/debug/yserver

# Run yserver in virtme-ng with a QEMU window + sshd. Use the QEMU window
# to see the bouncing rect; SSH from a second terminal for clean shutdown.
yserver-ssh:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw --ssh \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- target/debug/yserver

# Connect to the SSH server in a running yserver-*-ssh guest.
yserver-ssh-shell:
    vng --ssh-client

# Run yserver inside the guest for `seconds`, then send SIGTERM
# from inside the guest. Exercises the signalfd shutdown path.
yserver-headless-shutdown seconds="3":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- bash -c 'target/debug/yserver & pid=$!; sleep {{seconds}}; kill -TERM $pid; wait $pid'

# Multi-monitor smoke: virtio-gpu with two scanouts under GTK
# (SDL collapses Virtual-2 — see docs/superpowers/notes/2026-05-07-phase6-10-vng-recipe.md).
# YSERVER_MODE pin keeps both outputs at 1024x768 so seam = x=1024.
yserver-multihead:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci,max_outputs=2 \
                     -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- env YSERVER_MODE=1024x768 bash -c '\
            target/debug/yserver &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 fvwm3 > fvwm3.log 2>&1 &\
            sleep 2;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

# Run the nested ynest binary on the host's X server (no virtme).
ynest display="99":
    cargo run --bin ynest -- {{display}}

# Release-build ynest with a chosen container geometry.
ynest-release display="99" geometry="1920x1080":
    cargo run --release --bin ynest -- {{display}} --geometry {{geometry}}

# Visible smoke: ynest + wmaker + xterm. Ctrl-C tears it all down.
ynest-wmaker-xterm display="99" geometry="1024x768":
    cargo build --release --bin ynest
    RUST_LOG=trace target/release/ynest {{display}} --geometry {{geometry}} > ynest.log 2>&1 & \
        ynest_pid=$!; \
        sleep 1; \
        DISPLAY=:{{display}} wmaker & \
        sleep 2; \
        DISPLAY=:{{display}} xterm & \
        trap 'kill $ynest_pid 2>/dev/null; wait' INT TERM EXIT; \
        wait $ynest_pid

# Visible smoke: ynest + fvwm3 + xterm. Ctrl-C tears it all down.
ynest-fvwm3-xterm display="99" geometry="1024x768":
    cargo build --release --bin ynest
    RUST_LOG=trace target/release/ynest {{display}} --geometry {{geometry}} > ynest.log 2>&1 & \
        ynest_pid=$!; \
        sleep 1; \
        DISPLAY=:{{display}} fvwm3 & \
        sleep 2; \
        DISPLAY=:{{display}} xterm & \
        trap 'kill $ynest_pid 2>/dev/null; wait' INT TERM EXIT; \
        wait $ynest_pid

# Visible smoke: ynest + e16 + xterm. Ctrl-C tears it all down.
ynest-e16-xterm display="99" geometry="1024x768":
    cargo build --release --bin ynest
    RUST_LOG=trace target/release/ynest {{display}} --geometry {{geometry}} > ynest.log 2>&1 & \
        ynest_pid=$!; \
        sleep 1; \
        DISPLAY=:{{display}} e16 & \
        sleep 2; \
        DISPLAY=:{{display}} xterm & \
        trap 'kill $ynest_pid 2>/dev/null; wait' INT TERM EXIT; \
        wait $ynest_pid

# Visible smoke: ynest + wmaker + xterm. Ctrl-C tears it all down.
ynest-xeyes display="99" geometry="1024x768":
    cargo build --release --bin ynest
    RUST_LOG=debug target/release/ynest {{display}} --geometry {{geometry}} > ynest.log 2>&1 & \
        ynest_pid=$!; \
        sleep 1; \
        DISPLAY=:{{display}} xeyes & \
        trap 'kill $ynest_pid 2>/dev/null; wait' INT TERM EXIT; \
        wait $ynest_pid

# QEMU window + SSH + debug logging. Run `just yserver-ssh-shell` in a second
# terminal to get a shell, then: DISPLAY=:7 xterm
#
# Resolution is forced via YSERVER_MODE — virtio-gpu's xres/yres hint is not
# always honoured by the guest kernel (it often sticks at 640x480 preferred),
# so we override pick_mode directly. Override the default by passing
# `mode="WxH"` to just (e.g. `just yserver-debug-ssh mode=1920x1080`).
yserver-debug-ssh mode="1024x768":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw --ssh \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci,edid=on,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c 'RUST_LOG=trace RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver'

# Boot ynest on `display` and run an xts5 scenario against it.
# `scenario` matches an entry in xts5/tet_scen (Xproto, Xlib3, …, all).
# Tally lands in xts/results/<timestamp>/summary.
xts-ynest scenario="Xproto" display="99" geometry="1024x768" timeout="600":
    cargo build --release --bin ynest
    DISPLAY=:0 RUST_LOG=warn target/release/ynest {{display}} --geometry {{geometry}} > ynest-xts.log 2>&1 & \
        pid=$!; \
        trap "kill $pid 2>/dev/null; wait" INT TERM EXIT; \
        sleep 1; \
        tools/xts-run.sh :{{display}} {{scenario}} {{timeout}}

# Run yserver in virtme-ng with a QEMU window + RUST_LOG=debug + RUST_BACKTRACE=1.
# Shows window content; on crash prints a backtrace if it's a Rust panic.
yserver-debug:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c 'RUST_LOG=debug RUST_BACKTRACE=1 target/debug/yserver'

# Phase 4.1: yserver under virtio-gpu Venus passthrough.
# Exposes a real Vulkan device inside the guest. Requires
# `vulkan-virtio` on the host (Venus ICD).
yserver-venus mode="1024x768" log="info":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c 'RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver'

# Run yserver headless + wait 8 s + start xterm inside the guest.
# Use to smoke-test the xterm path without needing two terminals.
yserver-xterm:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- bash -c 'RUST_LOG=info RUST_BACKTRACE=1 target/debug/yserver &\
            yserver_pid=$!;\
            sleep 8;\
            DISPLAY=:7 xterm -e "echo xterm connected; sleep 10" &\
            wait $yserver_pid'

# Smoke-test virtme harness: bring up Xorg + xterm in a QEMU window.
harness-check:
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c "Xorg :1 vt1 -logfile xorg-test.log & sleep 5 && DISPLAY=:1 xterm"

# Phase 4 spike step 1: Vulkan inside vng with the legacy virtio-gpu-pci
# device. Expected to find no Vulkan device (the 2D device exposes no GPU
# context) — confirms the negative before we go looking for a positive.
vulkan-check-baseline:
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- bash -c 'vulkaninfo --summary 2>&1 | head -60; echo "---ICDs---"; ls /usr/share/vulkan/icd.d/ 2>&1'

# Phase 4 spike step 2: software Vulkan via lavapipe (llvmpipe ICD).
# Requires `vulkan-swrast` installed on the host so the guest sees
# `/usr/share/vulkan/icd.d/lvp_icd.json` (no .x86_64 suffix on Arch).
# Verified 2026-05-07: one llvmpipe device, Vulkan 1.4.335 — proves
# the loader+ICD plumbing works end-to-end inside vng.
vulkan-check-lavapipe:
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- bash -c 'VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json vulkaninfo --summary 2>&1 | head -80'

# Phase 4 spike step 3: real GPU via virtio-gpu Venus passthrough.
# Requires `vulkan-virtio` on the host (provides the Venus ICD inside
# the guest). Verified 2026-05-07: exposes host AMD Radeon 680M as
# "Virtio-GPU Venus (RADV REMBRANDT)" at Vulkan 1.4.307 (conformance
# 1.4.0.0), plus a llvmpipe-backed Venus fallback device.
vulkan-check-venus:
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true" \
        -- bash -c 'vulkaninfo --summary 2>&1 | head -80'

# Bring up yserver + fvwm3 + xterm in one QEMU window. The WM starts
# before xterm so the terminal gets framed. Logs to yserver.log on the
# host side via the shared cwd. Override resolution with `mode=WxH`.
#
# yserver runs in the background; xterm is the foreground process so
# closing it terminates the recipe (yserver dies with the guest).
yserver-fvwm3-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 fvwm3 > fvwm3.log 2>&1 &\
            sleep 2;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

# Phase 4.2 smoke: yserver + vkcube under Venus passthrough.
# Verifies DRI3 / Present extension discovery + handshake.
#
# Pin VK_DRIVER_FILES to virtio_icd.json so the loader doesn't
# probe radeon_icd inside the guest (no PCI passthrough → spurious
# segfault at vkCreateInstance time on some stacks).
#
# Wait for /tmp/.X11-unix/X7 to materialise before launching the
# client (yserver's modeset takes ~20s under the cold-cache vng
# boot). vkcube --c N exits after N frames; we use 5.
yserver-vkcube mode="1024x768" log="info" frames="5":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            for i in $(seq 30); do if [ -e /tmp/.X11-unix/X7 ]; then break; fi; sleep 1; done;\
            DISPLAY=:7 vkcube --c {{frames}} > vkcube.log 2>&1;\
            echo "===VKCUBE rc=$?===";\
            sleep 1;\
            kill $yserver_pid 2>/dev/null;\
            wait $yserver_pid 2>/dev/null;\
            echo "===YSERVER LOG TAIL===";\
            tail -50 yserver.log;\
            echo "===VKCUBE LOG===";\
            cat vkcube.log'

# Phase 4.2 GLX smoke. glxgears exercises GLX framing + DRI3 +
# Present.
yserver-glxgears mode="1024x768" log="info":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            for i in $(seq 30); do if [ -e /tmp/.X11-unix/X7 ]; then break; fi; sleep 1; done;\
            DISPLAY=:7 timeout 10 glxgears > glxgears.log 2>&1;\
            echo "===GLXGEARS rc=$?===";\
            sleep 1;\
            kill $yserver_pid 2>/dev/null;\
            wait $yserver_pid 2>/dev/null;\
            echo "===YSERVER LOG TAIL===";\
            tail -50 yserver.log;\
            echo "===GLXGEARS LOG===";\
            cat glxgears.log'

yserver-e16-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 e16 > e16.log 2>&1 &\
            sleep 2;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-wmaker-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 wmaker > wmaker.log 2>&1 &\
            sleep 2;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

# Run yserver directly on bare-metal hardware (no vng), capture its log,
# and bring up fvwm3 + xterm against it. Intended for TTY2 use while
# another graphical session (GNOME/Xorg) holds the user's main display
# on a different VT — yserver acquires DRM master on whichever
# /dev/dri/cardN matches its discovery.
#
# Default log level is debug; lower it with `log=info`.
#
# Closing xterm terminates the recipe; yserver is then SIGTERMed and
# the DRM master is released cleanly.
yserver-fvwm3-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 fvwm3 > fvwm3-hw.log 2>&1 &\
        sleep 8;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# No-WM hw smoke: just xterm against yserver. Lets us tell whether
# fvwm3 specifically is the blocker or whether the compositor / input
# pipeline itself is broken on hw. Without a WM xterm won't get a
# frame, but it should still render its own content + the cursor
# should track the mouse.
yserver-xterm-only-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 xterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "yserver log: yserver-hw.log"'

yserver-e16-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 e16 > e16-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "yserver log: yserver-hw.log";\
        echo "e16 log:   e16-hw.log"'

yserver-wmaker-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 wmaker > wmaker-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "yserver log: yserver-hw.log";\
        echo "wmaker log:   wmaker-hw.log"'

yserver-xfce-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 dbus-run-session xfce4-session --display :7 > xfce.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "yserver log: yserver-hw.log";\
        echo "xfce log:    xfce.log"'

# Bare-metal GLX/DRI3 smoke: yserver + glxgears with verbose Mesa logs.
# Mesa's loader_dri3 prints every probe step + driver load failure so
# we can pinpoint why "failed to load driver: radeonsi" fires. Pair
# with the yserver log to correlate Mesa's expectations against the
# DRI3 / GLX requests we actually see.
yserver-glxgears-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 LIBGL_DEBUG=verbose MESA_DEBUG=1 glxgears > glxgears.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "yserver log:  yserver-hw.log";\
        echo "glxgears log: glxgears.log"'

# Run rendercheck (X RENDER smoke suite) against ynest on `display`.
# `tests` is a comma-separated list. Default budget is 600s/test —
# `composite` / `cacomposite` / `repeat` are intrinsically slow
# (massive operator × format × source enumeration). Set timeout=N to
# override.
rendercheck-ynest display="99" geometry="1024x768" timeout="600" tests="fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366":
    cargo build --release --bin ynest
    DISPLAY=:0 RUST_LOG=warn target/release/ynest {{display}} --geometry {{geometry}} > ynest-rc.log 2>&1 & \
        pid=$!; \
        trap "kill $pid 2>/dev/null; wait" INT TERM EXIT; \
        sleep 1; \
        tools/rendercheck.sh :{{display}} {{timeout}} {{tests}}

# Run an xts5 scenario against yserver (KMS) inside virtme-ng.
# Boots vng once with yserver in the background (headless QEMU,
# virtio-gpu KMS), polls for the X socket on :7, then runs the same
# xts harness ynest uses. Result tree lands in xts/results/ on the
# host because vng mounts the host rootfs --rw.
xts-yserver scenario="Xproto" timeout="600":
    cargo build --release --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci -display none" \
        -- tools/yserver-vng-run.sh xts {{scenario}} {{timeout}}

# Run rendercheck against yserver (KMS) inside virtme-ng.
rendercheck-yserver timeout="600" tests="fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366":
    cargo build --release --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci -display none" \
        -- tools/yserver-vng-run.sh rendercheck {{timeout}} {{tests}}
