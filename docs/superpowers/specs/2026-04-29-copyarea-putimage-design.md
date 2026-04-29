# CopyArea + PutImage Design

**Goal:** Implement enough of X11 `CopyArea` (opcode 62) and
`PutImage` (opcode 72) for Phase 1 nested-server clients, especially
`xterm` scrolling and text/cursor image paths, while preserving the
current ynest architecture: host-backed top-level windows, host-backed
pixmaps, little-endian clients, TrueColor-only visuals, and no legacy
server-driver model.

**Status:** Spec only.

**Spec reference:** X11 protocol core requests `CopyArea` and
`PutImage`.

---

## Scope

Phase 1 should implement:

- `PutImage` with format `ZPixmap` into host-backed windows and
  host-backed pixmaps.
- `CopyArea` between any supported pair of host-backed drawables:
  window -> window, window -> pixmap, pixmap -> window, pixmap -> pixmap.
- Host-backed pixmap creation/destruction for `CreatePixmap` /
  `FreePixmap`.
- GC foreground/background state already exists; `PutImage` does not
  need GC colors for `ZPixmap`, but still validates that the GC id
  resolves.
- Phase-1 draw routing rule remains: top-level windows only, or child
  windows whose accumulated target offset is `(0, 0)`. Non-zero child
  offsets are a known follow-up.

Out of scope for this spec:

- `XYBitmap` and `XYPixmap` image formats. Return `BadValue` for now.
- Plane-mask semantics beyond "all planes". Accept all-ones masks and
  either forward the mask to the host GC or reject non-all-ones masks
  with `BadValue`. Prefer forwarding once GC plane-mask tracking
  exists.
- Clip rectangles, subwindow modes, graphics exposures, tile/stipple,
  raster ops other than the current default `GXcopy`.
- MIT-SHM. Shared-memory image paths are Phase 4.
- Full child-window coordinate translation. The current
  `top_level_host_target` API already returns offsets; this spec keeps
  implementation compatible with a later translation pass.

---

## Current State

- `CopyArea` and `PutImage` are stubs in
  `crates/yserver-core/src/nested.rs`.
- `CreatePixmap` records pixmap metadata in `ResourceTable`, but
  pixmaps have no host drawable backing.
- Each nested top-level window already has a host subwindow xid, and
  draw handlers route through `ResourceTable::top_level_host_target`.
- `HostX11` has methods for forwarding primitive drawing requests to a
  host drawable using a shared host GC.
- The server is little-endian only, and the setup advertises
  little-endian image byte order.

---

## Wire Model

### `CopyArea`

Request body:

```text
u32 src_drawable
u32 dst_drawable
u32 gc
i16 src_x
i16 src_y
i16 dst_x
i16 dst_y
u16 width
u16 height
```

Parser:

```rust
pub struct CopyAreaRequest {
    pub src: ResourceId,
    pub dst: ResourceId,
    pub gc: ResourceId,
    pub src_x: i16,
    pub src_y: i16,
    pub dst_x: i16,
    pub dst_y: i16,
    pub width: u16,
    pub height: u16,
}

pub fn copy_area_request(body: &[u8]) -> Option<CopyAreaRequest>;
```

Handler behavior:

- If `width == 0 || height == 0`, treat as a successful no-op.
- Resolve `gc`; if absent, emit `BadGC`.
- Resolve source and destination via the drawable resolver described
  below.
- If either drawable is unsupported by Phase 1 routing, silently drop
  the host call and return success. This matches existing drawing
  handlers that drop unsupported child-window routing.
- Forward to host `CopyArea`.

### `PutImage`

Request body:

```text
u32 drawable
u32 gc
u16 width
u16 height
i16 dst_x
i16 dst_y
u8 left_pad
u8 depth
u16 unused
bytes data
```

The request format is encoded in the request header `data` byte:

- `0`: `XYBitmap`
- `1`: `XYPixmap`
- `2`: `ZPixmap`

Parser:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageFormat {
    XyBitmap,
    XyPixmap,
    ZPixmap,
    Unknown(u8),
}

pub struct PutImageRequest<'a> {
    pub format: ImageFormat,
    pub drawable: ResourceId,
    pub gc: ResourceId,
    pub width: u16,
    pub height: u16,
    pub dst_x: i16,
    pub dst_y: i16,
    pub left_pad: u8,
    pub depth: u8,
    pub data: &'a [u8],
}

pub fn put_image_request(format: u8, body: &[u8]) -> Option<PutImageRequest<'_>>;
```

Handler behavior:

- If `width == 0 || height == 0`, treat as a successful no-op.
- Resolve `gc`; if absent, emit `BadGC`.
- Reject `XYBitmap`, `XYPixmap`, and unknown formats with `BadValue`
  on the format byte.
- For `ZPixmap`, require `left_pad == 0`.
- Require `depth == drawable_depth`.
- Require `data.len()` to be at least the expected padded byte length.
  Ignore extra padding bytes already present from the request body.
- Resolve drawable via the drawable resolver.
- Forward to host `PutImage`.

Expected data length for Phase 1:

```rust
let bits_per_pixel = match depth {
    24 => 32,
    32 => 32,
    1 => 1, // only if XYBitmap is later added
    _ => return BadMatch,
};
let stride_bits = usize::from(width) * bits_per_pixel;
let stride_bytes = stride_bits.div_ceil(32) * 4;
let expected = stride_bytes * usize::from(height);
```

Phase 1 should advertise and primarily accept 24-depth drawables with
32 bits per pixel scanlines, matching the current root pixmap format.

---

## Drawable Backing

Add a host-backed drawable resolver that handles windows and pixmaps
uniformly:

```rust
pub enum HostDrawableTarget {
    Window {
        nested: ResourceId,
        top_level: ResourceId,
        host_xid: u32,
        x_offset: i16,
        y_offset: i16,
        depth: u8,
    },
    Pixmap {
        nested: ResourceId,
        host_xid: u32,
        width: u16,
        height: u16,
        depth: u8,
    },
}

pub fn host_drawable_target(&self, id: ResourceId) -> Option<HostDrawableTarget>;
```

Window behavior:

- Use `top_level_host_target(id)` internally.
- Include the requested window's depth.
- For Phase 1, callers only forward when `x_offset == 0 &&
  y_offset == 0`.

Pixmap behavior:

- Return the pixmap's host xid and metadata.
- Pixmap coordinates do not require offset translation.

The resolver belongs in `ResourceTable` because it needs the window
tree and pixmap table. The host backend should not know nested IDs.

**None semantics:** `host_drawable_target` returns `None` in two
distinct cases that callers must not conflate:

1. The ID is not a known window or pixmap → emit `BadDrawable`.
2. The ID is a known window whose top-level has no `host_xid` yet,
   or a known pixmap with `host_xid = None` → **silently drop** the
   host call (same policy as unsupported child-window offsets).

To distinguish these, callers should check drawable existence before
calling this resolver:

```rust
let drawable_exists = s.resources.window(id).is_some()
    || s.resources.pixmap(id).is_some();
if !drawable_exists {
    return emit_x11_error(..., BAD_DRAWABLE, ...);
}
// host_drawable_target returning None here means "no host backing" → drop.
let target = match s.resources.host_drawable_target(id) {
    Some(t) => t,
    None => return log_void(...),
};
```

---

## Host-Backed Pixmaps

Extend `Pixmap`:

```rust
pub struct Pixmap {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub host_xid: Option<u32>,
    pub owner: ClientId,
}
```

`CreatePixmap` handler:

1. Parse the existing request.
2. Resolve the source drawable only far enough to prove it is a known
   drawable on the default screen. Core X11 uses the drawable to pick
   the screen, not to require matching depth.
3. Validate the requested pixmap depth against Phase 1 supported depths:
   `1`, `24`, and `32` are acceptable; start with `24` and `32` if no
   client needs depth-1 pixmaps yet.
4. Allocate a host xid via `HostX11::allocate_xid`.
5. Call `HostX11::create_pixmap(host_xid, depth, width, height)`.
6. Store `host_xid` on the `Pixmap`.

`FreePixmap` handler:

1. Remove the pixmap from `ResourceTable`.
2. If it had a host xid, call `HostX11::free_pixmap(host_xid)`.

If host pixmap creation fails, keep the nested pixmap with
`host_xid = None` and log the host error. That preserves current
client-visible behavior while making the drawing call a no-op later.

---

## HostX11 API

Add methods:

```rust
pub fn create_pixmap(
    &mut self,
    host_xid: u32,
    depth: u8,
    width: u16,
    height: u16,
) -> io::Result<()>;

pub fn free_pixmap(&mut self, host_xid: u32) -> io::Result<()>;

pub fn copy_area(
    &mut self,
    src_host_xid: u32,
    dst_host_xid: u32,
    src_x: i16,
    src_y: i16,
    dst_x: i16,
    dst_y: i16,
    width: u16,
    height: u16,
) -> io::Result<()>;

pub fn put_image(
    &mut self,
    host_xid: u32,
    depth: u8,
    width: u16,
    height: u16,
    dst_x: i16,
    dst_y: i16,
    data: &[u8],
) -> io::Result<()>;
```

Implementation details:

- `create_pixmap` writes host opcode 53 (`CreatePixmap`) with
  drawable `self.window_id` as the screen-compatible drawable.
- `free_pixmap` writes host opcode 54 (`FreePixmap`).
- `copy_area` writes host opcode 62 using the shared host GC.
- `put_image` writes host opcode 72 with format `ZPixmap` and the
  shared host GC.
- `put_image` should send exactly the expected byte length for the
  image plus 4-byte request padding.
- If a future caller needs GC function/plane-mask fidelity,
  `HostX11` can mirror those GC fields before `copy_area`/`put_image`
  the same way it already mirrors foreground/background/font.

---

## Coordinate Rules

For top-level windows:

- `CopyArea` coordinates are forwarded unchanged.
- `PutImage` destination coordinates are forwarded unchanged.

For child windows with `(x_offset, y_offset) == (0, 0)`:

- Treat them like top-levels. This keeps current Phase 1 behavior.

For child windows with non-zero offsets:

- Phase 1: drop the host call and log a debug line.
- Later: translate destination coordinates by adding
  `(x_offset, y_offset)` for destination windows, and translate source
  coordinates by adding source offsets for source windows.

For pixmaps:

- Coordinates are forwarded unchanged.

For mixed window/pixmap `CopyArea`:

- Apply the above rule independently to source and destination.
- If either side is an unsupported non-zero-offset child window, drop
  the copy.

---

## Error Policy

Phase 1 currently prefers compatibility-oriented no-ops for unsupported
drawing cases. Keep that policy for unsupported routing, but emit real
errors for malformed core requests:

- `BadGC` when the GC id does not exist.
- `BadDrawable` when the drawable id is neither a known window nor a
  known pixmap.
- `BadMatch` for incompatible depths between `PutImage.depth` and
  destination drawable depth, or incompatible drawable depths in
  `CopyArea`.
- `BadValue` for unsupported `PutImage` formats or invalid `left_pad`.

Depth compatibility for `CopyArea`:

- Window->window and pixmap->pixmap require equal depth.
- Window->pixmap and pixmap->window require equal depth.
- This is stricter than some future extension paths, but correct for
  core `CopyArea` without format conversion.

---

## Tests

Unit tests in `yserver-protocol`:

- `copy_area_request` parses every field.
- `put_image_request` parses `ZPixmap` and preserves the data slice.
- `put_image_request` maps unsupported format bytes to
  `ImageFormat::Unknown`.

Unit tests in `yserver-core`:

- `ResourceTable::host_drawable_target` resolves a top-level window.
- `ResourceTable::host_drawable_target` resolves a host-backed pixmap.
- A pixmap without `host_xid` returns no host drawable target.
- Non-zero child offsets are reported in the window target.

Manual smoke tests:

```sh
RUSTC_WRAPPER= cargo run --bin ynest -- 99
DISPLAY=:99 xterm
```

Expected:

- `xterm` scrolls by copying old terminal contents instead of leaving
  stale or black rectangles.
- Typing enough lines to scroll does not corrupt the window.
- Fish/tide prompt redraw still works.

Useful direct checks:

```sh
DISPLAY=:99 xev
DISPLAY=:99 xclock
DISPLAY=:99 xeyes
```

Expected:

- No regression in existing Phase 1 clients.
- `xev` continues to receive expose, key, pointer, and focus events.

---

## Rollout Plan

1. Add protocol parsers and parser tests.
2. Add host pixmap lifecycle to `HostX11`.
3. Extend `Pixmap` with `host_xid` and wire `CreatePixmap` /
   `FreePixmap`.
4. Add `host_drawable_target` to `ResourceTable`.
5. Add `HostX11::copy_area` and wire opcode 62.
6. Add `HostX11::put_image` and wire opcode 72.
7. Run `cargo fmt --all`, `cargo check --workspace`, and manual
   `xterm` scroll/input smoke tests.

---

## Open Questions

- Whether depth-1 pixmaps are needed in Phase 1. `xterm` has used
  pixmaps for cursor/icon paths before; if any depth-1 path appears,
  implement host depth-1 pixmap creation but keep `PutImage XYBitmap`
  out of scope until observed.
- Whether to emit `GraphicsExpose` / `NoExpose` for `CopyArea`.
  Current clients in the Phase 1 set should not depend on it. If a
  client selects graphics exposures in the GC and waits on them, add
  `NoExpose` first.
- Whether to track GC function and plane-mask now. The default
  `GXcopy` with all planes is enough for the xterm scrolling target.
