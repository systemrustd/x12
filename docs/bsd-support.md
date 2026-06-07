# BSD support — exploratory notes

Quick survey of what it would take to run yserver on FreeBSD /
GhostBSD. Not a commitment, not a plan — just enough to make the
next decision cheaply. macOS is explicitly out of scope (no DRM,
no TTY).

## Architecture is already on our side

`yserver-protocol` is pure protocol code: zero OS deps.

`yserver-core` is the protocol + state + fanout core. It touches
`libc`/`nix` only for unix sockets and `Fd` plumbing. Nothing
Linux-specific in the core loop.

`yserver/` (the binary) is where every Linux-ism lives. Today the
tree has exactly **2** `#[cfg(target_os = "linux")]` guards: a
panic in `lib.rs::run` and a top-level `mod kms;` gate. Everything
else compiles unconditionally on Linux because Linux is the only
target we build on.

## What ports for free (we hope)

| Crate / API | Why portable |
|---|---|
| `mio` (event loop) | already picks `kqueue` on BSD automatically |
| `drm` crate | uses ioctls; FreeBSD's drm-kmod exposes the same uAPI |
| `input` crate (libinput) | libinput works on FreeBSD over evdev |
| `libseat` 0.2.4 | seatd has a FreeBSD port; libseat-rs should pick it up |
| Mesa / GBM / Vulkan | FreeBSD ports track Linux closely; RADV builds |
| `crossbeam-channel`, `log`, etc. | std-only |

## What needs explicit work

| Linux-ism | BSD equivalent | Estimate |
|---|---|---|
| `signalfd` (via `nix`) | kqueue `EVFILT_SIGNAL` | half a day |
| VT switching (`/dev/console` KDSKBMODE / VT_ACTIVATE) | FreeBSD `vt(4)` ioctls, slightly different surface | 1–2 days |
| udev hotplug (if/when we wire it) | `devd` socket | optional, defer |
| Tiny hardcoded paths (`/dev/dri/card*`, `/sys/class/drm/...`) | most still apply, sysfs differs | small wrapper |

## Suggested shape — when we actually do this

```
crates/yserver/src/platform/
├── mod.rs        # Platform trait + cfg dispatch
├── linux.rs      # signalfd / KDSKBMODE / udev today
└── freebsd.rs    # kqueue / vt(4) / devd
```

The trait is small (`signal_handle`, `read_pending_signals`,
`vt_switch_to`, `device_hotplug`). Everything else stays shared.

## Real risks (in order)

1. **libseat backend availability** on FreeBSD — seatd ports exist,
   but verify the libseat-rs crate's `seatd` backend builds without
   patches.
2. **DRM ioctl drift** — FreeBSD's drm-kmod lags Linux kernel by
   months. Modesetting is fine; newer features like syncobj
   timelines or DRI3 implicit-sync ABIs may not be there. yserver
   uses DMA-BUF + syncobj heavily — this is the biggest unknown.
3. **Vulkan driver maturity** — RADV builds on FreeBSD but is less
   exercised than on Linux. RX580 (silence) is well-trodden on
   Linux; less so on FreeBSD.
4. **Bit-rot** — without CI, the cfg arms drift. Mitigation: a
   `cargo check --target x86_64-unknown-freebsd` step on Linux CI
   (no execution, just compile), plus periodic manual rebuilds.

## First-build recipe (silence/GhostBSD)

```sh
# install build deps
pkg install rust pkgconf libdrm libinput mesa-libs libseat \
    libxcb libxkbcommon vulkan-loader vulkan-validation-layers

# clone yserver where convenient
git clone <repo> && cd yserver

# dumb build — compile errors are the most honest survey
cargo build --release --bin yserver 2>&1 | tee bsd-build.log
```

Expected failure order (eyeball; verify):

1. `nix::sys::signalfd` import — feature gated to Linux. Either
   re-feature with `signal` on BSD or platform-fence the call site.
2. Possible `unsafe { libc::CONST }` references using Linux-only
   constants (e.g., `O_PATH`, some `eventfd` flags). grep and
   replace.
3. Possible `libseat-rs` build-script failure if it tries to detect
   a non-existent backend.

Once it links, scanout bring-up (DRM master + first KMS pageflip)
is the next milestone. Past that, the libinput path is where most
of the input-side validation needs to re-run because libinput's
default config differs between Linux and FreeBSD (keyboard layout
discovery, evdev permissions).

## Recommendation

Don't pre-refactor. The `Platform` trait shape above is the right
target, but spending a week extracting it before a single BSD
build is speculative. **Try the dumb build first** on silence/
GhostBSD; the compile errors will tell us the actual surface
faster than my eyeballed table. After that, the refactor falls
out naturally as you patch each leaf.

The architecture isn't fighting us. The hidden cost is *testing*
on BSD continuously — without a CI runner or a regular dogfooding
loop, the cfg arms will rot. That's the real cost question, not
the refactor itself.
