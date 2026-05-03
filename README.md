# yserver

A modern X11 server written from scratch in Rust.

The goal is not to clone Xorg. It is to provide a practical X11 server that
runs real desktop environments, window managers, and applications on modern
Linux while dropping legacy baggage (multiple screens, non-TrueColor visuals,
indirect GLX, the DDX driver ABI, endian-swapped clients, and so on).

See [`docs/high-level-design.md`](docs/high-level-design.md) for the full
design, scope, and phased plan.

## Status

`ynest` (nested backend) runs real desktop sessions: GTK3 apps, fvwm3,
Window Maker, e16, and partial openbox; many extensions implemented
(BIG-REQUESTS, RANDR, RENDER, XKB, XInput2, XFIXES, SHAPE, DAMAGE,
COMPOSITE, SYNC, Present, MIT-SHM).

`yserver` (standalone DRM/KMS) is in Phase 6.1 bootstrap on the
`phase6-bootstrap` branch: a real DRM/KMS binary that boots in
`virtme-ng`, sets a mode on virtio-gpu, runs a libinput-driven
single-thread `epoll` loop, and paints a moving rectangle into a
dumb-buffer swapchain with clean signalfd shutdown.

See [`docs/status.md`](docs/status.md) for per-phase progress.

## Layout

- `crates/yserver-protocol` — wire-format types, request/reply parsing.
- `crates/yserver-core` — server core: client dispatch, resources, nested
  backend (`nested.rs`), host X11 forwarding (`host_x11.rs`).
- `crates/yserver` — the `ynest` and `yserver` binaries.

## Building

Requires a recent stable Rust toolchain.

```sh
cargo build
```

For a release build:

```sh
cargo build --release
```

## Running the nested server

`ynest` listens on a Unix socket at `/tmp/.X11-unix/X<display>` and renders
into a window on the host X server pointed to by `$DISPLAY`. You need an
existing X11 (or XWayland) session to host it.

Start it on display `:99` (the default):

```sh
cargo run --bin ynest
```

Or pick a different display number:

```sh
cargo run --bin ynest -- 42
```

The host container window is 800×600 by default. Override with
`--geometry WxH`:

```sh
cargo run --bin ynest -- 42 --geometry 1024x768
```

Then point clients at it from another terminal, using the same display
number you started `ynest` on:

```sh
DISPLAY=:42 xeyes
DISPLAY=:42 xclock
DISPLAY=:42 xterm
```

A host window titled by `ynest` appears; nested client output is drawn into
it. Keyboard input over that window is forwarded to the focused nested
client.

## Running the standalone DRM/KMS server

`yserver` opens `/dev/dri/card0`, acquires DRM master, and drives
atomic KMS directly. It needs root, an unused DRM device, and no
other display server holding master — i.e. it cannot share the
host's graphical session.

The dev loop runs the binary inside a [`virtme-ng`](https://github.com/arighi/virtme-ng)
guest: vng boots the host kernel into a QEMU VM that shares the host
filesystem (so `cargo build` on the host is immediately runnable in
the guest with no rebuild step), exposes a virtio-gpu DRM device
inside the guest, and pipes guest stdout back to the host terminal.

This means **all `yserver` development is host-side** (vng inside a
sandbox without `/dev/kvm` can only fall back to slow TCG software
emulation, and bwrap'd vng has additional friction with virtio
plumbing). Open a shell on the bare host, not in a sandboxed editor.

The [`Justfile`](Justfile) wraps the recipes:

```sh
# Run the binary in a vng guest with a QEMU window (visual smoke).
just yserver

# Run headless: stdout/stderr come back to the host terminal.
# Best for log-driven validation.
just yserver-headless

# Run headless and auto-send SIGTERM after N seconds (default 3).
# Exercises the signalfd clean-shutdown path end-to-end.
just yserver-headless-shutdown
just yserver-headless-shutdown 5      # longer

# Variants with sshd in the guest, for sending signals from a
# second terminal via `vng --ssh-client` + `pkill -TERM yserver`.
just yserver-headless-ssh
just yserver-ssh
just yserver-ssh-shell                # connect from a second terminal

# Smoke-test the vng harness itself with Xorg + xterm.
just harness-check
```

vng prerequisites on the host:

- A kernel image (the recipes default to
  `/boot/vmlinuz-linux-cachyos`; edit `KERNEL` in the `Justfile`
  for other distros).
- `qemu-desktop` or `qemu-full` (the minimal `qemu-base` lacks
  virtio-gpu and display backends — symptom: `'virtio-gpu-pci' is
  not a valid device model name`).
- `--disable-microvm` is on by default in the recipes — vng's
  default microvm machine has no PCI bus and therefore no DRM
  device.

## Development

Before committing:

```sh
cargo fmt
cargo clippy
cargo test
```
