KERNEL := "/boot/vmlinuz-linux-cachyos"

# Run yserver in virtme-ng with virtio-gpu DRM device and a QEMU window.
yserver:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci" \
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
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci" \
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

# Smoke-test virtme harness: bring up Xorg + xterm in a QEMU window.
harness-check:
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci" \
        -- bash -c "Xorg :1 vt1 -logfile /tmp/xorg-test.log & sleep 5 && DISPLAY=:1 xterm"
