# yserver

A modern X11 server written from scratch in Rust.

The goal is not to clone Xorg. It is to provide a practical X11 server that
runs real desktop environments, window managers, and applications on modern
Linux while dropping legacy baggage (multiple screens, non-TrueColor visuals,
indirect GLX, the DDX driver ABI, endian-swapped clients, and so on).

See [`docs/high-level-design.md`](docs/high-level-design.md) for the full
design, scope, and phased plan.

## Status

Both backends run real desktop sessions on a single-threaded core
(no `Arc<Mutex<ServerState>>`, no per-client pump threads — the core
thread owns state and a mio poller).

`yserver` (standalone DRM/KMS) can now run full MATE/XFCE/Cinnamon desktops.
We support the following extensions:
- BIG-REQUESTS
- Composite
- DAMAGE
- DPMS
- DRI3
- GLX
- Generic Event Extension
- MIT-SHM
- Present
- RANDR
- RENDER
- SHAPE
- SYNC
- X-Resource
- XFIXES
- XInputExtension
- XKEYBOARD
- XTEST

## Hardware tested

`yserver` (standalone DRM/KMS) has been driven end-to-end against a
MATE / xfce4 desktop on:

- **AMD** — Ryzen 9 6900HX (Rembrandt, RDNA2, RADV); i9 13900k + RX580
  (Polaris/GCN4, RADV).
- **Intel** — i5-7200U (Kaby Lake, ANV).
- **NVIDIA** — i5 6500 with GTX 1050 (proprietary driver).
- **Snapdragon X1** X1E80100 (Adreno X1, Turnip). 
- **Apple** M1 MBA, M2 MBP on Asahi Linux (apple-drm KMS + asahi GPU, Mesa AGX-V).
- **Virtual** — virtio-gpu inside `virtme-ng` (lavapipe and Venus passthrough).

## Building

Requires a recent stable Rust toolchain.

```sh
cargo build
```

For a release build:

```sh
cargo build --release
```

## Running the standalone DRM/KMS server

`yserver` uses libseat for seat management if available.
It can also drive atomic KMS directly, but then you need access to /dev/dri/ and to /dev/input/.

The [`Justfile`](Justfile) wraps the recipes:

```sh
## switch to a free TTY, then run:
just startx
```

which will start yserver and then execute your `~/.xinitrc` (or fall back to `/etc/X11/xinitrc`)

If you are using libseat, you can switch VT, but if you use direct, you CAN NOT switch VT when yserver is running. Zap the server, or log out of your session otherwise.
## Development

Some convenience keybinds are available:

- Ctrl-Alt-Backspace: zap the server, return to console
- Ctrl-Alt-Enter: create a screenshot/scanout of the framebuffer in CWD
- Ctrl-Alt-F12: dump all drawables as PPM files to CWD

### Dependencies

#### Arch

```sh
sudo pacman -S just gcc libseat libxshmfence libxkbcommon libinput glslc systemd-libs fontconfig
```

#### Ubuntu

```sh
sudo apt install just gcc libseat-dev libxshmfence-dev libxkbcommon-dev libinput-dev glslc libudev-dev libfontconfig-dev
```

## Regression coverage with xts5

We run the X.Org X Test Suite (xts5) against `yserver` to gauge protocol completeness.

Latest pass numbers per scenario live in [`docs/test-status.md`](docs/test-status.md).
