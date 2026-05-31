# High-Level Design

A modern X11 server written from scratch in Rust. Drives DRM/KMS
directly via Vulkan; runs real desktop environments on modern
Linux. Not a clone of Xorg.

For current per-hardware state, see [`status.md`](status.md). For
the user-facing summary of what runs and how to build/run it, see
[`../README.md`](../README.md).

## Goals

- Run real X11 desktop environments and window managers (MATE,
  XFCE, Cinnamon validated; LXQt and similar lightweight desktops
  targeted).
- Support modern Linux graphics hardware through DRM/KMS and
  Vulkan, with no driver ABI for third-party hardware modules.
- Compositor-native architecture with tear-free presentation by
  default.
- Per-output presentation (refresh rate, modes) with multiple
  physical outputs as first-class.
- Provide a nested backend (`ynest`) for development and tests
  under an existing X11 session.
- Implement the modern X11 desktop contract that GTK, Qt, SDL,
  GLFW, Electron, and similar actively used software depend on.
- Keep the implementation auditable: declarative protocol parsing
  in a dedicated crate, no handwritten unsafe wire decoders.
- Single-threaded core (no `Arc<Mutex<ServerState>>`, no
  per-client pump threads) so the protocol invariants stay
  obvious.

## Non-Goals

- Being a drop-in clone of Xorg internals.
- Supporting old server driver modules or a DDX-style hardware
  driver ABI.
- Supporting hardware that lacks Linux DRM/KMS and Mesa support.
- Supporting multiple X11 screens.
- Supporting non-TrueColor legacy visuals as a first-class
  feature.
- Supporting indirect or remote GLX.
- Supporting endian-swapped clients unless a concrete modern use
  case appears.
- Implementing obsolete font functionality beyond what real
  clients still need.
- Preserving behavior that exists only because of Xorg
  implementation accidents, unless real desktop software depends
  on it.

## Crates

- `yserver-protocol` — wire-level X11 encode/decode. Own schema,
  generated parsers/serializers; no XCB XML.
- `yserver-core` — protocol-level core: clients, resources,
  windows, properties, atoms, selections, input dispatch, RANDR,
  XFIXES, DAMAGE, COMPOSITE bookkeeping, XKB, XInput2. Backend-
  agnostic. The single-threaded core loop and mio poller live
  here.
- `yserver` — backends. Contains both the nested `ynest` and the
  standalone DRM/KMS server, plus all GPU/KMS code. Vulkan
  context, libinput input thread, atomic KMS modesetting,
  rendering model v2 (`kms/v2/`).

## Core loop

Single-threaded. The core thread owns `ServerState` and runs an
mio poller. Listed event sources:

- Per-client X11 sockets (read-side; writes go through a separate
  per-client write queue).
- Input thread `eventfd`-style channel.
- Backend-specific FDs (DRM event FD, signalfd).

Signals are blocked at startup and consumed via signalfd in the
poller. SIGTERM/SIGINT route to a `Message::Shutdown` and let the
loop drain cleanly. SIGUSR1 dumps the scanout to PPM; SIGUSR2
dumps drawables.

## Backends

Two backends, selected at binary level.

- `ynest` — nested X11 backend. Runs under an existing X11 (or
  Xwayland) display, treats the parent server as a single output.
  Used for protocol development and regression coverage where
  hardware isn't needed.
- `yserver` — standalone DRM/KMS. Opens `/dev/dri/card*`,
  acquires DRM master, drives atomic modesetting, owns the
  console. This is the production target.

The standalone backend uses **Vulkan directly** for rendering
and dmabuf export. No EGL, no GBM, no Mesa GL. No
`logind`/`seatd`/`libseat` integration — the server expects to be
launched on a free VT with direct access to `/dev/dri/*` and
`/dev/input/*`. Modes are set via DRM atomic; pageflips drive
retirement.

Input on both backends: `libinput` reads on a dedicated thread,
which posts cooked events into the core loop's channel.

## Rendering model (v2)

The standalone backend's rendering core is split into four
components, all under `crates/yserver/src/kms/v2/`. `KmsCore`
(at `kms/core.rs`) sits alongside them and owns the X11
protocol bookkeeping that is independent of GPU state (XID maps,
window/pixmap metadata, COMPOSITE redirects, SHAPE regions,
picture records, font/glyphset records, cursor records).

### PlatformBackend (`v2/platform.rs`)

Hardware and OS surface. Owns the DRM device, the Vulkan
instance/device/queue, the scanout BO pool, the page-flip
retirement state, libinput context, and the output layout.
Provides FenceTicket allocation (recyclable VkFence wrappers with
CPU-side lifetime semantics).

### DrawableStore (`v2/store.rs`)

Storage and lifetime for every X11 drawable (windows, pixmaps,
the cursor sprite). Each entry is a Vulkan image plus a layout
state machine, with refcount, damage region, presentation
damage, render fence ticket, and scene-participation flag. The
store is the single source of truth for what GPU memory exists
and who holds references.

### RenderEngine (`v2/engine.rs`)

Paint operations. All X11 drawing requests — fill, put_image,
get_image, copy_area, copy_plane, image_text, poly_*,
render_composite, render_fill_rectangles, render_composite_glyphs,
render_trapezoids/triangles — record one or more Vulkan command
buffers against the destination drawable's storage. The engine
also owns the pipeline cache, the glyph atlas, the descriptor
pool ring, and the deferred frame builder.

### SceneCompositor (`v2/scene.rs`)

The composed output pass. Walks the window tree once per
present, builds a draw list (root → mapped descendants →
cursor), and emits one composite command buffer per output.
Buffer-age tracking with per-output history rings clips the
repaint to actually-dirty regions; full redraw is the fallback
when history is too short.

### FrameBuilder (`v2/frame_builder.rs`)

Per-frame coalescer (introduced in Stage 5 Phase B). Every paint
op records into a deferred `RecordedOp` list on the open frame
instead of submitting its own command buffer. The frame closes
on PRESENT completion, get_image sync wait, scene tick, timeout,
or pin-set saturation; close emits one CB and one
`vkQueueSubmit2` per frame. Collapses thousand-op MATE drag
bursts into one submission per vblank.

## Synchronization

- **FenceTicket**: per-submission Rc-shared handle to a VkFence.
  Holds the CPU-side lifetime of staging buffers, descriptor
  arenas, scratch images, and any resource that must outlive its
  CB. Pool-recycled.
- **Per-ScanoutBo export semaphore**: VkSemaphore exported as a
  SYNC_FD, passed to KMS as `IN_FENCE_FD` so the atomic commit
  blocks until the composite CB has finished writing.
- **Image-layout state machine**: every drawable tracks its
  current Vulkan layout; transitions are explicit barriers
  emitted by the engine.
- **Single graphics queue**: same-queue submission ordering is
  the GPU dependency for compose-after-paint and glyph-upload-
  before-draw. No `vkQueueWaitIdle` on the hot path (only inside
  `get_image`, which is synchronous by protocol).

## Protocol scope

Connection setup, native-endian only. Resource IDs, windows,
pixmaps, cursors, graphics contexts, drawables, atoms,
properties, selections, hierarchy, mapping, focus, grabs,
pointer/keyboard events, crossings, event masks.

Extensions implemented: BIG-REQUESTS, COMPOSITE, DAMAGE, DPMS,
DRI3, Generic Event Extension, GLX (modern direct-rendering only
via DRI3/Present), MIT-SCREEN-SAVER, MIT-SHM, PRESENT, RANDR,
RENDER, SHAPE, SYNC, X-Resource, XFIXES, XInput2, XKB (XKEYBOARD),
XTEST. Coverage
is what real clients actually drive — extension versions and
capability sets are conservative where the implementation isn't
mature.

## Compositing model

The built-in compositor is primary. Every paintable drawable has
its own storage that can be damaged, composed, and presented
independently. The root window does not imply a global
framebuffer. COMPOSITE redirect (both Automatic and Manual mode)
is supported; the Composite Overlay Window (COW) is a
first-class scene participant so external compositors (xfwm4
composited, picom, marco) draw to it and see their output on
screen.

Per-output presentation scheduling is in place. Tear-free is the
default. VRR, fullscreen direct-scanout bypass, and color
management / HDR are valid future directions but not yet
implemented.

## What's not in this document

- Security model. yserver does not yet enforce per-client
  capability restrictions; a client can read other clients'
  windows, observe global input, and inspect the clipboard. 
