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

# Run yserver in virtme-ng with a QEMU window + RUST_LOG=debug + RUST_BACKTRACE=1.
# Shows window content; on crash prints a backtrace if it's a Rust panic.
yserver-debug:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c 'RUST_LOG=debug RUST_BACKTRACE=1 target/debug/yserver'

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
        -- bash -c "Xorg :1 vt1 -logfile /tmp/xorg-test.log & sleep 5 && DISPLAY=:1 xterm"

# Bring up yserver + fvwm3 + xterm in one QEMU window. The WM starts
# before xterm so the terminal gets framed. Logs to yserver.log on the
# host side via the shared cwd. Override resolution with `mode=WxH`.
#
# yserver runs in the background; xterm is the foreground process so
# closing it terminates the recipe (yserver dies with the guest).
yserver-fvwm3-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci,edid=on,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 1;\
            DISPLAY=:7 fvwm3 > fvwm3.log 2>&1 &\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-e16-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci,edid=on,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 1;\
            DISPLAY=:7 e16 > e16.log 2>&1 &\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-wmaker-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci,edid=on,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 wmaker > wmaker.log 2>&1 &\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'
