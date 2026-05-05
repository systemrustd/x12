# Phase 6.6 — RENDER Completion on KMS

Goal: fully implement `CompositeGlyphs` and `Composite` on `KmsBackend` so
that WM panel text (FvwmPager desktop names, FvwmIconMan titles, FvwmButtons
labels) renders on bare DRM/KMS, close any remaining toolkit double-buffering
gaps, and complete the WM matrix smoke-run (wmaker / e16 on bare KMS).

## 1. Problem Statement

Phase 6.5 landed RENDER advertisement and the three primitives that unblock
fvwm3 clients (Trapezoids, glyph rasterisation via `render_text_string`,
SolidFill). WM clients work; WM chrome does not:

- **Panel text is blank.** fvwm3's FvwmPager / FvwmIconMan / FvwmButtons
  render labels via `CompositeGlyphs8`. `render_composite_glyphs` is a no-op
  stub on `KmsBackend`.
- **GlyphSet lifecycle is unimplemented.** `render_create_glyphset`,
  `render_add_glyphs`, and `render_free_glyphs` are all no-ops, so there are
  no glyph bitmaps to composite even if the call were wired.
- **Generic `Composite` is a no-op.** Toolkits (GTK, Qt) use
  `RENDER::Composite` for double-buffered widget blits from an off-screen
  picture to the window. With the stub, those blits silently drop.
- **wmaker / e16 KMS smoke not done.** Both WMs were validated against the
  host backend in Phase 6.3. They were not re-run on bare KMS after 6.5's
  backend-internal changes.

## 2. Design

### 2.1 GlyphSet state

Add a `glyphsets: HashMap<u32, GlyphSetState>` field to `KmsBackend`
alongside the existing `pictures` map.

```rust
struct StoredGlyph {
    width:  u16,
    height: u16,
    /// RENDER wire field: coordinate of the bitmap's top-left corner relative
    /// to the glyph origin. This is the *negative* of FreeType's bitmap_left
    /// (e.g. bitmap_left=2 → x=-2 on the wire). Draw at pen_x - x.
    x:      i16,
    /// Same sign convention: draw at pen_y - y.
    y:      i16,
    x_off:  i16,   // horizontal advance (move pen right by this after the glyph)
    y_off:  i16,   // vertical advance (almost always 0 for Latin scripts)
    pixels: Vec<u8>,   // row-major A8 data, densely packed (no per-row padding)
}

struct GlyphSetState {
    format: GlyphSetFormat,   // A1 | A8 | ARGB32 etc — derived from ynest format ID at create time
    glyphs: HashMap<u32, StoredGlyph>,
}
```

`GlyphSetFormat` is a small enum (`A1`, `A8`, `ARGB32`). The KMS backend
already has the machinery to decide between A8 and A1 (same distinction used
in `render_trapezoids`). For Phase 6.6 only A8 is needed — fvwm3 uses A8
glyphsets for all Xft rendering.

### 2.2 `render_create_glyphset`

Currently returns `Ok(None)` (no host glyphset allocated). The KMS backend
manages glyphsets internally, so it allocates a synthetic ID via
`self.next_host_xid()` (the same method used by `render_create_picture`
and `render_create_solid_fill`; starts at `0x00400000` so it is never zero),
inserts an empty `GlyphSetState`, and returns
`Ok(GlyphSetHandle::from_raw(id))`. Since the ID comes from `next_host_xid`
it is always non-zero, so `from_raw` never returns `None`. No separate
`next_gs_id` counter is needed — `next_host_xid()` provides a monotone
non-zero ID space shared across all synthetic backend resources.

`nested.rs` stores the returned `GlyphSetHandle` in the `ResourceTable` and
uses it as `host_gs` on all subsequent calls — this path already works for the
host backend.

### 2.3 `render_add_glyphs`

Wire format of `body_tail` (everything after the 4-byte glyphset XID in the
request body, already padded):

```
num_glyphs: u32
glyph_ids:  [u32; num_glyphs]   (always u32 on the wire regardless of GlyphSet8/16/32)
glyph_infos: num_glyphs × 12 bytes each:
    width:  u16
    height: u16
    x:      i16   (negative of FreeType bitmap_left; draw at pen_x - x)
    y:      i16   (negative of FreeType bitmap_top;  draw at pen_y - y)
    x_off:  i16   (horizontal advance)
    y_off:  i16   (vertical advance)
glyph_data: concatenated A8 bitmaps, each row padded to 4-byte boundary
            (only A8 is supported in Phase 6.6; early-return for other formats)
```

Implementation:

1. Parse `num_glyphs` from bytes 0..4.
2. Parse `num_glyphs` glyph IDs from bytes 4..4+4*num_glyphs.
3. Parse `num_glyphs` GlyphInfo structs from the next `12*num_glyphs` bytes.
4. If `GlyphSetState::format != A8`, log at `debug!` level and return
   `Ok(())` — parsing A1 bitmaps with the A8 stride formula would produce
   corrupted pixel data (A1 uses `(width + 31) / 32 * 4` bytes per row).
5. Walk the remaining `glyph_data` bytes: for each glyph, the bitmap occupies
   `height × padded_row_stride` bytes where `padded_row_stride = (width + 3) & !3`
   for A8. Copy into a `Vec<u8>` of size `width * height` (un-pad each row so
   pixels are densely packed — same layout used by `render_text_string`'s
   `gdata` writes).
6. Insert a `StoredGlyph` for each `glyph_id` into the `GlyphSetState`.

### 2.4 `render_composite_glyphs`

Item stream format (same as `patch_glyph_command_offsets`):

```
Each item starts at a 4-byte-aligned position.
  byte[0]: count  (number of glyphs in this run; 255 = glyphset-switch sentinel)
  byte[1..3]: padding
  byte[4..5]: dx (i16 LE) — pen X relative to accumulated position
  byte[6..7]: dy (i16 LE) — pen Y relative to accumulated position
  byte[8..8+count*id_size]: glyph IDs (id_size = 1/2/4 for minor 23/24/25)
  (padded to 4-byte boundary)
```

For the glyphset-switch sentinel (`count == 255`), the next 4 bytes are the
new glyphset XID (host-space — this is the synthetic handle returned at create
time). Update the active glyphset and advance `pos += 8`.

Pen starts at `(src_x + x_off, src_y + y_off)` (the accumulated x/y offset
from the trait parameters, applied before the first run).

For each non-sentinel run:
1. Advance pen by `(dx, dy)`.
2. For each glyph ID in the run, look up `StoredGlyph` in the active
   glyphset.
3. Composite the glyph's A8 bitmap onto the destination drawable using pixman,
   exactly as `render_text_string` does today:
   - Create a 1×1 `REPEAT_NORMAL` solid-colour source image from the src
     picture. The src picture in CompositeGlyphs is always a SolidFill in
     practice (fvwm3 uses solid fills for all panel text via Xft). If it's a
     `Drawable` picture, log at `debug!` and use opaque black as fallback.
   - Compute the draw position: `dst_x = pen_x - glyph.x`, `dst_y = pen_y - glyph.y`.
     (The RENDER wire `x` field is the *negative* of FreeType's `bitmap_left`,
     so subtracting it is equivalent to adding the left bearing — matching what
     `render_text_string` does with `cursor_x + glyph.bitmap_left()`.)
   - Build a pixman A8 image from `StoredGlyph::pixels` with the
     un-padded dense layout (same `gdata` unsafe write loop as
     `render_text_string`).
   - Call `dst.composite32(Operation::Over, &color_img, Some(&glyph_img), (0,0), (0,0), (dst_x, dst_y), (glyph.width, glyph.height))`.
   - Advance pen X by `StoredGlyph::x_off` (y advance is for vertical text,
     almost always 0 for Latin scripts).

The composite is clipped to the destination's `clip` rectangles if set (same
pattern as `render_trapezoids`).

### 2.5 `render_composite`

Parameters: `op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y, dst_x, dst_y, width, height`.

Supported combinations for Phase 6.6:

| src type    | mask | dst type    | action |
|-------------|------|-------------|--------|
| SolidFill   | 0    | Drawable    | pixman composite solid onto window/pixmap |
| Drawable    | 0    | Drawable    | pixman composite pixmap/window onto window/pixmap |
| any         | ≠ 0  | any         | log-and-skip (mask compositing deferred) |

Implementation:

The implementation follows the same unsafe `*mut pixman_image_t` raw-pointer
pattern established in `render_trapezoids` — both src and dst raw pointers are
obtained while the pictures map is immutably borrowed, the borrow is released,
then `with_image_mut` drives the composite via the `pixman_image_composite32`
FFI function directly (never via `composite32(&ImageRef)` which would require
reconstructing a `pixman::Image` from a raw pointer — that cast is unsound
because `Image<'_, '_>` is not `#[repr(transparent)]` over `*mut pixman_image_t`).

```rust
// Pseudocode — mirrors the render_trapezoids pattern
let src_ptr: *mut pixman_image_t = match self.pictures.get(&host_src) {
    Some(PictureState::SolidFill { image }) => image.borrow().0.as_ptr(),
    Some(PictureState::Drawable { host_xid, .. }) => {
        self.image_ptr_for_xid(*host_xid).ok_or_else(|| ...)?
    }
    None => return Ok(()),
};

let (dst_xid, dst_clip) = match self.pictures.get(&host_dst) { ... };
// op: translate X RENDER op byte to pixman op u32 (same table as render_trapezoids)
let pixman_op = op as u32;
self.with_image_mut(dst_xid, |dst| {
    let dst_ptr = dst.0.as_ptr();
    // apply clip if set (same pattern as render_trapezoids)
    // SAFETY: src_ptr and dst_ptr both point to pixman images we own that
    // outlive this closure. src != dst because they come from different
    // picture entries. pixman_image_composite32 does not retain the pointers.
    unsafe {
        pixman::ffi::pixman_image_composite32(
            pixman_op, src_ptr, std::ptr::null_mut(), dst_ptr,
            src_x as i32, src_y as i32,
            0, 0,
            dst_x as i32, dst_y as i32,
            width as i32, height as i32,
        );
    }
    // clear clip if set
});
```

A helper `image_ptr_for_xid(xid: u32) -> Option<*mut pixman_image_t>` is
added that looks up `self.windows` and `self.pixmaps` and returns the raw
pixman pointer from the backing `RefCell<PixmanImage>`. The pointer is safe
to use after the `Ref` guard drops because the `PixmanImage` allocation is
owned by the `WindowState`/`PixmapState` and is not freed until the resource
itself is freed — the same reasoning that makes the pattern safe in
`render_trapezoids` (documented at lines 2666–2674 of `kms/backend.rs`).

### 2.6 Validation — wmaker / e16 on bare KMS

No code changes. Run the same `vng`-based test harness used for fvwm3 in 6.5:
launch yserver under virtme-ng with the KMS backend, start wmaker then e16,
confirm:
- WM starts and manages a client window (xterm or xclock).
- No crash or busy-loop in the yserver log.
- Window titlebars and decorations render (wmaker and e16 use core X11 fonts
  for their own chrome, not Xft/CompositeGlyphs, so this does not directly
  test Step 1 — it confirms the 6.5 backend changes did not regress these WMs
  on bare KMS). Separately confirm fvwm3 panel text now renders to validate
  CompositeGlyphs end-to-end.

## 3. Implementation Plan

### Step 0 — GlyphSet lifecycle

- Add `GlyphSetFormat`, `StoredGlyph`, `GlyphSetState` structs to `kms/backend.rs`.
- Add `glyphsets: HashMap<u32, GlyphSetState>` to `KmsBackend` (no separate
  ID counter — reuse `self.next_host_xid()` as in `render_create_picture`).
- Implement `render_create_glyphset`: call `next_host_xid()`, insert empty
  state, return `Ok(GlyphSetHandle::from_raw(id))` (always `Some` since
  `next_host_xid` is non-zero).
- Implement `render_free_glyphset`: remove from map.
- Implement `render_add_glyphs`: parse body_tail, populate `StoredGlyph`
  entries. Add a unit test: add 2 glyphs to a glyphset, verify width/height/
  pixel bytes are stored correctly.
- Implement `render_free_glyphs`: remove listed glyph IDs from the glyphset.

### Step 1 — `render_composite_glyphs`

- Implement item-stream decoder (analogous to `patch_glyph_command_offsets`
  but consuming rather than patching).
- Extract src colour from `host_src` picture (SolidFill → ARGB colour; else
  log and use opaque black as fallback).
- Composite each glyph using pixman (mirrors `render_text_string` phase 2).
- Add unit tests:
  - Single-run CompositeGlyphs8 with a known 4×4 A8 glyph produces non-zero
    pixels at the expected location in the destination.
  - Multi-run item stream (two consecutive glyph commands) advances pen
    correctly.
  - Glyphset-switch sentinel is parsed without panicking.

### Step 2 — `render_composite`

- Add `image_ptr_for_xid` helper.
- Implement `render_composite` for SolidFill-src and Drawable-src cases.
- Mask ≠ 0 path: log once at `warn!` level and return `Ok(())`.
- Add unit test: composite a 1×1 red SolidFill picture onto a 4×4 destination
  drawable, verify destination pixel is non-zero red.

### Step 3 — wmaker / e16 KMS smoke

- Launch yserver + wmaker under `vng`, confirm startup and client management.
- Launch yserver + e16 under `vng`, confirm startup and client management.
- Document results in `status.md` Phase 6.6 section.

## 4. Out of scope (deferred to 6.7+)

- Real font enumeration on KMS (`list_fonts_proxy` returns empty terminator).
- Host/guest cursor drift and lock.
- pixman `fill_rectangles` partly-out-of-bounds segfault root cause.
- Full VT_SETMODE / logind / suspend-resume / hotplug polish.
- `SetDashes` no-op + reply (log noise, cosmetic).
- `InstallColormap` no-op (TrueColor backend, safe to ignore).
- `line_width` thick lines in `poly_line`.
- Partial-angle clipping for `poly_arc` / `poly_fill_arc`.
- RENDER mask compositing (`host_mask ≠ 0` in `render_composite`).
- ChangePicture CPClipMask / CPAlphaMap XID translation.
- `Window.background_pixmap_host_xid` leak on window destroy.
