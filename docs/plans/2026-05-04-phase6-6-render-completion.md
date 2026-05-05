# Phase 6.6 — RENDER Completion on KMS Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement `CompositeGlyphs` and `Composite` on `KmsBackend` so fvwm3 panel text renders on bare DRM/KMS, then smoke-run wmaker/e16 on bare KMS to close the WM matrix.

**Architecture:** All new state lives in `crates/yserver/src/kms/backend.rs` alongside the existing `pictures: HashMap<u32, PictureState>`. GlyphSets are stored in a new `glyphsets: HashMap<u32, GlyphSetState>` map; glyph bitmaps are decoded once at `AddGlyphs` time and composited with pixman at `CompositeGlyphs` time. `Composite` follows the same unsafe `*mut pixman_image_t` raw-pointer pattern established in `render_trapezoids`.

**Tech Stack:** Rust, pixman crate (`pixman::ffi::pixman_image_composite32`), `pixman::Image`/`PixmanImage` wrappers already in the file, `GlyphSetHandle::from_raw` from `yserver-core`.

**Spec:** `docs/superpowers/specs/2026-05-04-phase6-6-render-completion-design.md`

---

## Task 0: GlyphSet state structs + `render_create_glyphset` / `render_free_glyphset`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

**Context:**
- `PictureState` enum is at line ~356; add `GlyphSetState` etc. near it.
- `pictures: HashMap<u32, PictureState>` is at line ~352; add `glyphsets` alongside.
- `KmsBackend` initialiser is around line ~628; add `glyphsets: HashMap::new()` there.
- `render_create_glyphset` stub is at line ~2499; `render_free_glyphset` at ~2507.
- `next_host_xid()` is at line ~652; reuse it (same as `render_create_picture` at ~2473).
- `GlyphSetHandle::from_raw` is in `yserver_core::backend::handles` — already imported via the `use yserver_core::{...}` block at the top of the file.

**Step 1: Add structs**

Insert after the closing brace of `PictureState` (~line 370):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlyphSetFormat {
    A8,
    Other, // A1, ARGB32 etc — not supported in Phase 6.6
}

struct StoredGlyph {
    width:  u16,
    height: u16,
    /// RENDER wire field: top-left of bitmap relative to glyph origin.
    /// This is the *negative* of FreeType's bitmap_left.
    /// Draw at pen_x - x, pen_y - y.
    x:      i16,
    y:      i16,
    x_off:  i16,
    y_off:  i16,
    /// Row-major A8 bytes, densely packed (no per-row padding).
    pixels: Vec<u8>,
}

struct GlyphSetState {
    format: GlyphSetFormat,
    glyphs: HashMap<u32, StoredGlyph>,
}
```

**Step 2: Add `glyphsets` field to `KmsBackend`**

In the `KmsBackend` struct (near line ~311), add after `pictures`:
```rust
glyphsets: HashMap<u32, GlyphSetState>,
```

In the initialiser (near line ~648), add after `pictures: HashMap::new()`:
```rust
glyphsets: HashMap::new(),
```

**Step 3: Implement `render_create_glyphset`**

Replace the current stub (line ~2499):
```rust
fn render_create_glyphset(
    &mut self,
    _origin: Option<OriginContext>,
    ynest_format: u32,
) -> io::Result<Option<GlyphSetHandle>> {
    // ynest_format is the ynest-local PICTFORMAT id (1–4).
    // Format 2 = A8 (8-bit alpha); everything else treated as unsupported.
    let format = if ynest_format == 2 { GlyphSetFormat::A8 } else { GlyphSetFormat::Other };
    let id = self.next_host_xid();
    self.glyphsets.insert(id, GlyphSetState { format, glyphs: HashMap::new() });
    Ok(GlyphSetHandle::from_raw(id))
}
```

**Step 4: Implement `render_free_glyphset`**

Replace the stub (~line 2507):
```rust
fn render_free_glyphset(
    &mut self,
    _origin: Option<OriginContext>,
    host_gs: u32,
) -> io::Result<()> {
    self.glyphsets.remove(&host_gs);
    Ok(())
}
```

**Step 5: Implement `render_free_glyphs`**

Replace the stub (~line 2524):
```rust
fn render_free_glyphs(
    &mut self,
    _origin: Option<OriginContext>,
    host_gs: u32,
    glyph_ids: &[u8],
) -> io::Result<()> {
    let Some(gs) = self.glyphsets.get_mut(&host_gs) else { return Ok(()); };
    for chunk in glyph_ids.chunks_exact(4) {
        let id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        gs.glyphs.remove(&id);
    }
    Ok(())
}
```

**Step 6: Compile check**

```bash
cargo build -p yserver 2>&1 | grep -E "^error"
```
Expected: no errors (warnings about unused fields are fine).

**Step 7: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): add GlyphSetState + create/free_glyphset lifecycle"
```

---

## Task 1: `render_add_glyphs`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

**Context:**
- Stub is at line ~2515.
- `body_tail` is everything after the 4-byte glyphset XID in the request body.
- Wire layout: `num_glyphs:u32` | `glyph_ids:[u32;N]` | `glyph_infos:[12 bytes each; N]` | `glyph_data:[A8 bitmaps]`
- GlyphInfo (12 bytes): `width:u16` `height:u16` `x:i16` `y:i16` `x_off:i16` `y_off:i16`
- For A8: each bitmap row is padded to `(width + 3) & !3` bytes on the wire; store densely.

**Step 1: Write the failing test**

Add inside `mod tests` at the bottom of the file (after the last `}`):

```rust
#[test]
fn add_glyphs_stores_pixel_data_correctly() {
    // Build a minimal AddGlyphs body_tail for 2 glyphs:
    //   Glyph 0 (id=1): 2×2 A8 bitmap, x=-1, y=-2, x_off=4, y_off=0
    //   Glyph 1 (id=2): 4×1 A8 bitmap, x=0,  y=-1, x_off=5, y_off=0
    //
    // Wire layout:
    //   num_glyphs(4) = 2
    //   ids: [1u32 LE, 2u32 LE]
    //   infos:
    //     glyph0: width=2 height=2 x=-1 y=-2 x_off=4 y_off=0  (12 bytes)
    //     glyph1: width=4 height=1 x=0  y=-1 x_off=5 y_off=0  (12 bytes)
    //   pixel data:
    //     glyph0: 2×2 A8, row-stride=4 (padded): [0x11,0x22, 0,0, 0x33,0x44, 0,0]
    //     glyph1: 4×1 A8, row-stride=4 (no pad):  [0x55,0x66,0x77,0x88]

    let mut body = Vec::new();
    body.extend_from_slice(&2u32.to_le_bytes());   // num_glyphs
    body.extend_from_slice(&1u32.to_le_bytes());   // id[0]
    body.extend_from_slice(&2u32.to_le_bytes());   // id[1]
    // glyph0 info
    body.extend_from_slice(&2u16.to_le_bytes());   // width
    body.extend_from_slice(&2u16.to_le_bytes());   // height
    body.extend_from_slice(&(-1i16).to_le_bytes()); // x
    body.extend_from_slice(&(-2i16).to_le_bytes()); // y
    body.extend_from_slice(&4i16.to_le_bytes());   // x_off
    body.extend_from_slice(&0i16.to_le_bytes());   // y_off
    // glyph1 info
    body.extend_from_slice(&4u16.to_le_bytes());   // width
    body.extend_from_slice(&1u16.to_le_bytes());   // height
    body.extend_from_slice(&0i16.to_le_bytes());   // x
    body.extend_from_slice(&(-1i16).to_le_bytes()); // y
    body.extend_from_slice(&5i16.to_le_bytes());   // x_off
    body.extend_from_slice(&0i16.to_le_bytes());   // y_off
    // glyph0 pixels: 2×2, padded row stride 4
    body.extend_from_slice(&[0x11, 0x22, 0x00, 0x00]); // row 0
    body.extend_from_slice(&[0x33, 0x44, 0x00, 0x00]); // row 1
    // glyph1 pixels: 4×1, padded row stride 4
    body.extend_from_slice(&[0x55, 0x66, 0x77, 0x88]);

    let mut gs = super::GlyphSetState {
        format: super::GlyphSetFormat::A8,
        glyphs: std::collections::HashMap::new(),
    };
    super::parse_add_glyphs(&mut gs, &body);

    let g0 = gs.glyphs.get(&1).expect("glyph id=1 missing");
    assert_eq!(g0.width, 2);
    assert_eq!(g0.height, 2);
    assert_eq!(g0.x, -1);
    assert_eq!(g0.y, -2);
    assert_eq!(g0.x_off, 4);
    assert_eq!(g0.pixels, vec![0x11, 0x22, 0x33, 0x44]); // densely packed

    let g1 = gs.glyphs.get(&2).expect("glyph id=2 missing");
    assert_eq!(g1.width, 4);
    assert_eq!(g1.pixels, vec![0x55, 0x66, 0x77, 0x88]);
}
```

**Step 2: Run — expect compile error (function not yet defined)**

```bash
cargo test -p yserver --lib -- add_glyphs_stores_pixel_data_correctly 2>&1 | grep -E "error|FAILED|ok"
```

**Step 3: Add `parse_add_glyphs` free function and implement the stub**

Add a `pub(super)` free function just before `mod tests` (so the test can call it as `super::parse_add_glyphs`):

```rust
/// Parse an AddGlyphs `body_tail` and insert glyphs into `gs`.
/// `body_tail` is everything after the 4-byte glyphset XID.
/// Only A8 glyphsets are handled; Other formats are silently skipped.
pub(super) fn parse_add_glyphs(gs: &mut GlyphSetState, body_tail: &[u8]) {
    if gs.format != GlyphSetFormat::A8 {
        return;
    }
    if body_tail.len() < 4 {
        return;
    }
    let n = u32::from_le_bytes([body_tail[0], body_tail[1], body_tail[2], body_tail[3]]) as usize;
    let ids_start = 4;
    let ids_end = ids_start + n * 4;
    let infos_end = ids_end + n * 12;
    if body_tail.len() < infos_end {
        return;
    }

    // Decode glyph IDs.
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let off = ids_start + i * 4;
        ids.push(u32::from_le_bytes([
            body_tail[off], body_tail[off+1], body_tail[off+2], body_tail[off+3],
        ]));
    }

    // Decode glyph infos (12 bytes each).
    struct Info { width: u16, height: u16, x: i16, y: i16, x_off: i16, y_off: i16 }
    let mut infos = Vec::with_capacity(n);
    for i in 0..n {
        let off = ids_end + i * 12;
        let b = &body_tail[off..off+12];
        infos.push(Info {
            width:  u16::from_le_bytes([b[0], b[1]]),
            height: u16::from_le_bytes([b[2], b[3]]),
            x:      i16::from_le_bytes([b[4], b[5]]),
            y:      i16::from_le_bytes([b[6], b[7]]),
            x_off:  i16::from_le_bytes([b[8], b[9]]),
            y_off:  i16::from_le_bytes([b[10], b[11]]),
        });
    }

    // Decode pixel data: each glyph's A8 bitmap has row stride = (width+3)&!3.
    let mut data_off = infos_end;
    for (id, info) in ids.into_iter().zip(infos) {
        let w = info.width as usize;
        let h = info.height as usize;
        let stride = (w + 3) & !3;
        let nbytes = stride * h;
        if data_off + nbytes > body_tail.len() {
            break;
        }
        // Un-pad: copy only the live `w` bytes from each row.
        let mut pixels = vec![0u8; w * h];
        let slice = &body_tail[data_off..data_off + nbytes];
        for row in 0..h {
            pixels[row*w..row*w+w].copy_from_slice(&slice[row*stride..row*stride+w]);
        }
        data_off += nbytes;
        gs.glyphs.insert(id, StoredGlyph {
            width: info.width, height: info.height,
            x: info.x, y: info.y,
            x_off: info.x_off, y_off: info.y_off,
            pixels,
        });
    }
}
```

Then implement the stub method to call it:

```rust
fn render_add_glyphs(
    &mut self,
    _origin: Option<OriginContext>,
    host_gs: u32,
    body_tail: &[u8],
) -> io::Result<()> {
    if let Some(gs) = self.glyphsets.get_mut(&host_gs) {
        parse_add_glyphs(gs, body_tail);
    }
    Ok(())
}
```

**Step 4: Run test — expect PASS**

```bash
cargo test -p yserver --lib -- add_glyphs_stores_pixel_data_correctly 2>&1 | grep -E "ok|FAILED"
```

**Step 5: Compile check**

```bash
cargo build -p yserver 2>&1 | grep -E "^error"
```

**Step 6: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): implement render_add_glyphs with A8 bitmap parsing"
```

---

## Task 2: `render_composite_glyphs`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

**Context:**
- Stub is at line ~2552.
- Item stream format is identical to `patch_glyph_command_offsets` in `host_x11/request.rs` — consume rather than patch.
- Each run: `count(1) pad(3) dx(i16) dy(i16) glyph_ids(count * id_size bytes, padded to 4)`.
- Sentinel: `count == 255` → skip 8 bytes, the last 4 bytes are the new glyphset XID.
- `id_size` = 1/2/4 for minor 23/24/25.
- Pen starts at `(src_x as i32 + x_off as i32, src_y as i32 + y_off as i32)`.
- Draw position: `dst_x = pen_x - glyph.x`, `dst_y = pen_y - glyph.y`.
- After each glyph: advance pen X by `glyph.x_off`.
- Src picture: extract ARGB from `PictureState::SolidFill`; use opaque black fallback for Drawable.
- Composite each glyph onto dst drawable: same `color_img` + A8 `glyph_img` + `composite32` pattern as `render_text_string` (lines ~1107–1150).
- The `PictureState::SolidFill { image }` stores a `RefCell<PixmanImage>`.
  Extract colour with: `image.borrow().0.data()` reads raw ARGB pixel, or just re-use the borrow:
  `let color_img_borrow = image.borrow();` and pass `&color_img_borrow.0` to composite... but that conflicts with the mutable borrow of `self` in `with_image_mut`. **Safer approach:** read the solid-fill colour *before* the closure — extract it as a `pixman::Color` value, then create a fresh 1×1 solid-fill image inside the closure (same as `render_text_string` does).
- To read the ARGB pixel from the solid-fill image: `unsafe { *image.borrow().0.data() }` gives the packed A8R8G8B8 u32; then split into channels for `Color::new`.

**Step 1: Write failing tests**

Add to `mod tests`:

```rust
#[test]
fn composite_glyphs_single_run_places_glyph_on_dst() {
    // Set up: 1×1 opaque-red solid colour source, 8×8 white destination.
    // Glyph: 2×2 fully-opaque A8 (all 0xFF), stored at id=1 in a GlyphSetState.
    // CompositeGlyphs8 item stream: one run of 1 glyph at dx=2, dy=3.
    // Expected: after composite, pixel at (2 - glyph.x, 3 - glyph.y) is red.

    // Build glyphset with a 2×2 all-opaque A8 glyph (id=1, x=-1, y=-1).
    let mut gs = super::GlyphSetState {
        format: super::GlyphSetFormat::A8,
        glyphs: std::collections::HashMap::new(),
    };
    gs.glyphs.insert(1, super::StoredGlyph {
        width: 2, height: 2, x: -1, y: -1, x_off: 3, y_off: 0,
        pixels: vec![0xFF; 4],
    });

    // Red solid-fill source (A8R8G8B8 = 0xFFFF0000).
    let mut src_img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
    let _ = src_img.0.fill_rectangles(
        Operation::Src,
        Color::new(0xFFFF, 0, 0, 0xFFFF),
        &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
    );
    src_img.0.set_repeat(Repeat::Normal);

    // White destination.
    let mut dst_img = PixmanImage::new(FormatCode::X8R8G8B8, 8, 8, true).unwrap();
    fill_image(&mut dst_img, 0x00FF_FFFF);

    // Build a CompositeGlyphs8 item stream: one run with count=1, dx=2, dy=3, id=1.
    let mut items = Vec::new();
    items.push(1u8);        // count
    items.extend_from_slice(&[0, 0, 0]); // pad
    items.extend_from_slice(&2i16.to_le_bytes()); // dx
    items.extend_from_slice(&3i16.to_le_bytes()); // dy
    items.push(1u8);        // glyph id (8-bit for minor=23)
    items.extend_from_slice(&[0, 0, 0]); // pad to 4-byte boundary

    super::composite_glyphs_onto(
        &gs, &src_img, &mut dst_img,
        /*minor=*/23, /*pen_x=*/0, /*pen_y=*/0,
        &items,
    );

    // Pen after dx/dy = (0+2, 0+3) = (2, 3).
    // Draw at (pen_x - glyph.x, pen_y - glyph.y) = (2-(-1), 3-(-1)) = (3, 4).
    let p = read_pixel(&dst_img, 3, 4);
    assert_ne!(p & 0x00FF_0000, 0, "pixel (3,4) should have red channel; got 0x{:08x}", p);
    assert_eq!(p & 0x0000_FFFF, 0, "pixel (3,4) should have no blue/green; got 0x{:08x}", p);
}

#[test]
fn composite_glyphs_multi_run_advances_pen() {
    // Two runs: first places glyph id=1 (x_off=5), second places glyph id=2.
    // After first run pen advances by x_off=5.
    let mut gs = super::GlyphSetState {
        format: super::GlyphSetFormat::A8,
        glyphs: std::collections::HashMap::new(),
    };
    gs.glyphs.insert(1, super::StoredGlyph {
        width: 1, height: 1, x: 0, y: 0, x_off: 5, y_off: 0,
        pixels: vec![0xFF],
    });
    gs.glyphs.insert(2, super::StoredGlyph {
        width: 1, height: 1, x: 0, y: 0, x_off: 3, y_off: 0,
        pixels: vec![0xFF],
    });

    let mut src_img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
    let _ = src_img.0.fill_rectangles(Operation::Src, Color::new(0, 0, 0xFFFF, 0xFFFF),
        &[Rectangle16 { x:0, y:0, width:1, height:1 }]);
    src_img.0.set_repeat(Repeat::Normal);

    let mut dst_img = PixmanImage::new(FormatCode::X8R8G8B8, 20, 4, true).unwrap();
    fill_image(&mut dst_img, 0x00FF_FFFF);

    // Run 1: count=1, dx=2, dy=1, id=1
    // Run 2: count=1, dx=3, dy=0, id=2
    let mut items = Vec::new();
    items.push(1u8); items.extend_from_slice(&[0,0,0]);
    items.extend_from_slice(&2i16.to_le_bytes()); items.extend_from_slice(&1i16.to_le_bytes());
    items.push(1u8); items.extend_from_slice(&[0,0,0]);
    items.push(1u8); items.extend_from_slice(&[0,0,0]);
    items.extend_from_slice(&3i16.to_le_bytes()); items.extend_from_slice(&0i16.to_le_bytes());
    items.push(2u8); items.extend_from_slice(&[0,0,0]);

    super::composite_glyphs_onto(&gs, &src_img, &mut dst_img, 23, 0, 0, &items);

    // Glyph 1 at pen (2,1): draw at (2,1). After glyph, pen_x += x_off=5 → pen_x=7.
    // Glyph 2 at pen (7+3=10, 1+0=1): draw at (10,1).
    let p1 = read_pixel(&dst_img, 2, 1);
    let p2 = read_pixel(&dst_img, 10, 1);
    assert_ne!(p1 & 0x0000_FFFF, 0, "glyph1 pixel (2,1) should have blue; got 0x{:08x}", p1);
    assert_ne!(p2 & 0x0000_FFFF, 0, "glyph2 pixel (10,1) should have blue; got 0x{:08x}", p2);
}

#[test]
fn composite_glyphs_sentinel_does_not_panic() {
    // A sentinel-only item stream (count=255 + 4-byte gs XID) should be a no-op.
    let gs = super::GlyphSetState {
        format: super::GlyphSetFormat::A8,
        glyphs: std::collections::HashMap::new(),
    };
    let src_img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
    let mut dst_img = PixmanImage::new(FormatCode::X8R8G8B8, 4, 4, true).unwrap();
    fill_image(&mut dst_img, 0x00FF_FFFF);
    let items = vec![255u8, 0, 0, 0,  0x99, 0, 0, 0]; // sentinel + fake gs xid
    // Should not panic:
    super::composite_glyphs_onto(&gs, &src_img, &mut dst_img, 23, 0, 0, &items);
    assert_eq!(read_pixel(&dst_img, 0, 0), 0x00FF_FFFF, "sentinel must not modify dst");
}
```

**Step 2: Run — expect compile error**

```bash
cargo test -p yserver --lib -- composite_glyphs 2>&1 | grep -E "error|FAILED|ok" | head -5
```

**Step 3: Add `composite_glyphs_onto` free function**

Add just before `mod tests`:

```rust
/// Composite a CompositeGlyphs item stream from `gs` using `src` as the colour
/// source onto `dst`. `minor` = 23/24/25 → id_size 1/2/4.
/// `pen_x`/`pen_y` are the starting pen position (already offset by src_x+x_off
/// and src_y+y_off at the call site in `render_composite_glyphs`).
pub(super) fn composite_glyphs_onto(
    gs: &GlyphSetState,
    src: &PixmanImage,
    dst: &mut PixmanImage,
    minor: u8,
    pen_x: i32,
    pen_y: i32,
    items: &[u8],
) {
    let id_size = match minor {
        23 => 1usize,
        24 => 2,
        _  => 4,
    };

    // Read the solid colour from `src` (1×1 REPEAT_NORMAL image).
    // SAFETY: src is a 1×1 pixman image we own; data() returns a valid pointer.
    let argb: u32 = unsafe { *src.0.data() };
    let a = (((argb >> 24) & 0xFF) as u16) * 0x101;
    let r = (((argb >> 16) & 0xFF) as u16) * 0x101;
    let g = (((argb >>  8) & 0xFF) as u16) * 0x101;
    let b = ((argb         & 0xFF) as u16) * 0x101;
    let pen_color = Color::new(r, g, b, a);

    let dst_w = dst.0.width() as i32;
    let dst_h = dst.0.height() as i32;
    let mut pen_x = pen_x;
    let mut pen_y = pen_y;
    let mut pos = 0usize;

    while pos + 8 <= items.len() {
        let count = items[pos] as usize;
        if count == 255 {
            // Glyphset-switch sentinel: 8 bytes (count, pad×3, gs_xid×4).
            // We ignore the new glyphset XID — the caller resolves glyphsets.
            pos += 8;
            continue;
        }
        let dx = i16::from_le_bytes([items[pos+4], items[pos+5]]) as i32;
        let dy = i16::from_le_bytes([items[pos+6], items[pos+7]]) as i32;
        pen_x += dx;
        pen_y += dy;

        let payload_start = pos + 8;
        let payload_bytes = count * id_size;
        let padded = (payload_bytes + 3) & !3;
        if payload_start + padded > items.len() {
            break;
        }

        for i in 0..count {
            let id_off = payload_start + i * id_size;
            let glyph_id: u32 = match id_size {
                1 => items[id_off] as u32,
                2 => u16::from_le_bytes([items[id_off], items[id_off+1]]) as u32,
                _ => u32::from_le_bytes([items[id_off], items[id_off+1],
                                         items[id_off+2], items[id_off+3]]),
            };

            let Some(glyph) = gs.glyphs.get(&glyph_id) else {
                continue;
            };
            let gw = glyph.width as usize;
            let gh = glyph.height as usize;
            // Draw position (see spec: pen_x - glyph.x because wire x is -bitmap_left).
            let dst_x = pen_x - glyph.x as i32;
            let dst_y = pen_y - glyph.y as i32;

            if dst_x + gw as i32 <= 0 || dst_y + gh as i32 <= 0
                || dst_x >= dst_w || dst_y >= dst_h
            {
                pen_x += glyph.x_off as i32;
                continue;
            }

            // Build 1×1 solid-colour source (same as render_text_string).
            let mut color_img = match Image::new(FormatCode::A8R8G8B8, 1, 1, true) {
                Ok(img) => img,
                Err(_) => { pen_x += glyph.x_off as i32; continue; }
            };
            let _ = color_img.fill_rectangles(
                Operation::Src, pen_color,
                &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
            );
            color_img.set_repeat(Repeat::Normal);

            // Build A8 mask from densely-packed glyph pixels.
            let Ok(glyph_img) = Image::new(FormatCode::A8, gw, gh, true) else {
                pen_x += glyph.x_off as i32;
                continue;
            };
            let stride_bytes = glyph_img.stride();
            // SAFETY: gdata points into a pixman A8 image we just allocated.
            // We write only within [0, (gh-1)*stride_bytes + (gw-1)].
            let gdata = unsafe { glyph_img.data() } as *mut u8;
            for row in 0..gh {
                for col in 0..gw {
                    unsafe {
                        *gdata.add(row * stride_bytes + col) = glyph.pixels[row * gw + col];
                    }
                }
            }

            dst.0.composite32(
                Operation::Over,
                &color_img,
                Some(&glyph_img),
                (0, 0),
                (0, 0),
                (dst_x, dst_y),
                (gw as i32, gh as i32),
            );

            pen_x += glyph.x_off as i32;
        }

        pos += 8 + padded;
    }
}
```

Then implement the stub method:

```rust
fn render_composite_glyphs(
    &mut self,
    _origin: Option<OriginContext>,
    minor: u8,
    _op: u8,
    host_src: u32,
    host_dst: u32,
    _mask_fmt: u32,
    host_gs: u32,
    src_x: i16,
    src_y: i16,
    items: &[u8],
    x_off: i16,
    y_off: i16,
) -> io::Result<()> {
    // Resolve src picture — must be SolidFill (Drawable fallback: opaque black).
    let src_img = match self.pictures.get(&host_src) {
        Some(PictureState::SolidFill { image }) => {
            // Clone the colour so we can drop the pictures borrow.
            let argb = unsafe { *image.borrow().0.data() };
            let mut img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true)?;
            let a = (((argb >> 24) & 0xFF) as u16) * 0x101;
            let r = (((argb >> 16) & 0xFF) as u16) * 0x101;
            let g = (((argb >>  8) & 0xFF) as u16) * 0x101;
            let b = ((argb         & 0xFF) as u16) * 0x101;
            let _ = img.0.fill_rectangles(
                Operation::Src, Color::new(r, g, b, a),
                &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
            );
            img.0.set_repeat(Repeat::Normal);
            img
        }
        _ => {
            log::debug!("render_composite_glyphs: host_src 0x{host_src:x} is not SolidFill; using black");
            let mut img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true)?;
            let _ = img.0.fill_rectangles(
                Operation::Src, Color::new(0, 0, 0, 0xFFFF),
                &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
            );
            img.0.set_repeat(Repeat::Normal);
            img
        }
    };

    let (dst_xid, clip) = match self.pictures.get(&host_dst) {
        Some(PictureState::Drawable { host_xid, clip }) => (*host_xid, clip.clone()),
        _ => return Ok(()),
    };

    let Some(gs) = self.glyphsets.get(&host_gs) else { return Ok(()); };
    // SAFETY: gs is behind an immutable ref; we pass it into a closure that
    // only calls composite_glyphs_onto which reads from gs and writes to dst.
    // No aliasing: gs and dst are in separate data structures.
    let gs_ptr: *const GlyphSetState = gs;

    let pen_x = src_x as i32 + x_off as i32;
    let pen_y = src_y as i32 + y_off as i32;
    let items_owned = items.to_vec(); // avoid lifetime entanglement with self

    self.with_image_mut(dst_xid, |dst| {
        if let Some(ref rects) = clip {
            use pixman::{Box32, Region32};
            let boxes: Vec<Box32> = rects.iter().map(|r| Box32 {
                x1: r.x as i32, y1: r.y as i32,
                x2: r.x as i32 + r.width as i32,
                y2: r.y as i32 + r.height as i32,
            }).collect();
            let region = Region32::init_rects(&boxes);
            let _ = dst.0.set_clip_region32(Some(&region));
        }
        // SAFETY: gs_ptr points to a GlyphSetState in self.glyphsets; dst is
        // from self.windows/pixmaps. They are disjoint. The glyphset is not
        // freed during this closure because no other thread runs concurrently
        // (KmsBackend is !Sync and this method takes &mut self).
        let gs_ref = unsafe { &*gs_ptr };
        composite_glyphs_onto(gs_ref, &src_img, dst, minor, pen_x, pen_y, &items_owned);
        if clip.is_some() {
            let _ = dst.0.set_clip_region32(None);
        }
    });

    Ok(())
}
```

**Step 4: Run tests — expect PASS**

```bash
cargo test -p yserver --lib -- composite_glyphs 2>&1 | grep -E "ok|FAILED"
```

**Step 5: Full test suite**

```bash
cargo test -p yserver --lib -- --skip glyph_render_gray 2>&1 | tail -5
```
Expected: `20 passed` → now more (the 3 new tests added).

**Step 6: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): implement render_composite_glyphs with A8 glyph compositing"
```

---

## Task 3: `render_composite`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

**Context:**
- Stub is at line ~2533.
- Uses `pixman::ffi::pixman_image_composite32` directly (same module as `pixman_composite_trapezoids` already used in `render_trapezoids`).
- `pixman::ffi` is accessible as `pixman::ffi::pixman_image_composite32(op, src, mask, dst, src_x, src_y, mask_x, mask_y, dst_x, dst_y, width, height)`.
- `src_ptr` obtained from `pictures` map while immutably borrowed; `pictures` borrow released before `with_image_mut` is called.
- For Drawable src: need `image_ptr_for_xid` helper.

**Step 1: Write failing test**

Add to `mod tests`:

```rust
#[test]
fn render_composite_solid_fill_onto_drawable() {
    // Simulate: composite a 1×1 red SolidFill picture onto a 4×4 white drawable.
    // The SolidFill image is a 1×1 REPEAT_NORMAL red pixel.
    // We call pixman_image_composite32 directly (same path as the impl will use).

    let mut src_img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
    let _ = src_img.0.fill_rectangles(
        Operation::Src,
        Color::new(0xFFFF, 0, 0, 0xFFFF),  // opaque red
        &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
    );
    src_img.0.set_repeat(Repeat::Normal);

    let mut dst_img = PixmanImage::new(FormatCode::X8R8G8B8, 4, 4, true).unwrap();
    fill_image(&mut dst_img, 0x00FF_FFFF); // white

    let src_ptr = src_img.0.as_ptr();
    let dst_ptr = dst_img.0.as_ptr();

    unsafe {
        pixman::ffi::pixman_image_composite32(
            pixman::ffi::pixman_op_t_PIXMAN_OP_OVER,
            src_ptr, std::ptr::null_mut(), dst_ptr,
            0, 0, 0, 0,
            1, 1, // dst at (1,1)
            2, 2, // 2×2 region
        );
    }

    let p = read_pixel(&dst_img, 1, 1);
    assert_ne!(p & 0x00FF_0000, 0, "pixel (1,1) should have red; got 0x{:08x}", p);
    assert_eq!(p & 0x0000_FFFF, 0, "pixel (1,1) should have no blue/green; got 0x{:08x}", p);
}
```

**Step 2: Run — expect PASS** (this tests pixman FFI directly, no new code needed yet)

```bash
cargo test -p yserver --lib -- render_composite_solid_fill_onto_drawable 2>&1 | grep -E "ok|FAILED"
```

This test validates the FFI call pattern before we wire it up.

**Step 3: Add `image_ptr_for_xid` helper**

Add after `with_image_mut`:

```rust
/// Return a raw pixman pointer for a window or pixmap drawable.
/// SAFETY: The pointer is valid as long as the drawable is not removed from
/// self.windows / self.pixmaps. Caller must not call any method that could
/// remove the drawable while holding the pointer.
fn image_ptr_for_xid(&self, host_xid: u32) -> Option<*mut pixman::ffi::pixman_image_t> {
    if let Some(w) = self.windows.get(&host_xid) {
        Some(w.image.borrow().0.as_ptr())
    } else {
        self.pixmaps.get(&host_xid).map(|p| p.image.0.as_ptr())
    }
}
```

**Step 4: Implement `render_composite`**

Replace the stub (~line 2533):

```rust
fn render_composite(
    &mut self,
    _origin: Option<OriginContext>,
    op: u8,
    host_src: u32,
    host_mask: u32,
    host_dst: u32,
    src_x: i16,
    src_y: i16,
    _mask_x: i16,
    _mask_y: i16,
    dst_x: i16,
    dst_y: i16,
    width: u16,
    height: u16,
) -> io::Result<()> {
    if host_mask != 0 {
        log::warn!("render_composite: mask compositing not supported (host_mask=0x{host_mask:x}); skipping");
        return Ok(());
    }

    let pixman_op = op as u32;

    // Extract src raw pointer while pictures is immutably borrowed.
    let src_ptr: *mut pixman::ffi::pixman_image_t = match self.pictures.get(&host_src) {
        Some(PictureState::SolidFill { image }) => image.borrow().0.as_ptr(),
        Some(PictureState::Drawable { host_xid, .. }) => {
            let xid = *host_xid;
            match self.image_ptr_for_xid(xid) {
                Some(ptr) => ptr,
                None => {
                    log::debug!("render_composite: src drawable 0x{xid:x} has no image");
                    return Ok(());
                }
            }
        }
        None => {
            log::debug!("render_composite: host_src 0x{host_src:x} not found");
            return Ok(());
        }
    };

    let (dst_xid, clip) = match self.pictures.get(&host_dst) {
        Some(PictureState::Drawable { host_xid, clip }) => (*host_xid, clip.clone()),
        _ => {
            log::debug!("render_composite: host_dst 0x{host_dst:x} is not a Drawable picture");
            return Ok(());
        }
    };

    self.with_image_mut(dst_xid, |dst| {
        let dst_ptr = dst.0.as_ptr();
        if let Some(ref rects) = clip {
            use pixman::{Box32, Region32};
            let boxes: Vec<Box32> = rects.iter().map(|r| Box32 {
                x1: r.x as i32, y1: r.y as i32,
                x2: r.x as i32 + r.width as i32,
                y2: r.y as i32 + r.height as i32,
            }).collect();
            let region = Region32::init_rects(&boxes);
            let _ = dst.0.set_clip_region32(Some(&region));
        }
        // SAFETY: src_ptr and dst_ptr are both valid pixman images we own.
        // They are distinct: src is from self.pictures (SolidFill or a different
        // drawable), dst is from self.windows/pixmaps for dst_xid. KmsBackend
        // is !Sync so no concurrent access. pixman_image_composite32 does not
        // retain the pointers after return.
        unsafe {
            pixman::ffi::pixman_image_composite32(
                pixman_op,
                src_ptr, std::ptr::null_mut(), dst_ptr,
                src_x as i32, src_y as i32,
                0, 0,
                dst_x as i32, dst_y as i32,
                width as i32, height as i32,
            );
        }
        if clip.is_some() {
            let _ = dst.0.set_clip_region32(None);
        }
    });

    Ok(())
}
```

**Step 5: Run full tests**

```bash
cargo test -p yserver --lib -- --skip glyph_render_gray 2>&1 | tail -5
```
Expected: all tests pass.

**Step 6: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): implement render_composite via pixman_image_composite32 FFI"
```

---

## Task 4: Smoke validation — fvwm3 panel text + wmaker / e16 on bare KMS

**Files:**
- Modify (docs only): `docs/status.md`

**Context:**
- Use the virtme-ng (`vng`) invocation from `memory/reference_virtme_ng_drm_harness.md`.
- The sandbox has X access + `import`/Xephyr available (`memory/feedback_visible_smoke_testing.md`).
- Results go in a new `### Phase 6.6 — RENDER completion on KMS (complete)` section at the end of the Phase 6 block in `docs/status.md`.

**Step 1: Run fvwm3 + panel text smoke**

Launch yserver with the KMS backend under `vng` and start fvwm3 with FvwmPager active. Observe whether panel text (desktop names etc.) now renders.

Expected: FvwmPager / FvwmIconMan panel labels are visible as text.

**Step 2: Run wmaker smoke**

Launch yserver under `vng`, start wmaker, open xterm. Confirm:
- wmaker starts and draws its dock / clip panel.
- xterm is managed (appears in wmaker window list).
- No crash or busy-loop in yserver log.

**Step 3: Run e16 smoke**

Same as Step 2 with e16 instead of wmaker.

**Step 4: Update `docs/status.md`**

Add a `### Phase 6.6` section after the Phase 6.5 section (around line 1647) documenting:
- What was implemented (CompositeGlyphs, Composite, glyphset lifecycle).
- Validation results (fvwm3 panel text visible, wmaker/e16 KMS smoke pass/fail).
- Test count.
- Out-of-scope deferred to 6.7.

**Step 5: Commit**

```bash
git add docs/status.md
git commit -m "docs: Phase 6.6 — RENDER completion on KMS complete"
```

---

## Execution

Plan complete and saved to `docs/plans/2026-05-04-phase6-6-render-completion.md`. Two execution options:

**1. Subagent-Driven (this session)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Parallel Session (separate)** — Open a new session in a worktree, use superpowers:executing-plans for batch execution with checkpoints.

**Which approach?**
