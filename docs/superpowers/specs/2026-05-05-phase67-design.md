# Phase 6.7 Design — Full X11 Implementation Pass

**Date:** 2026-05-05  
**Status:** Approved  
**Scope:** 13 items across 7 steps, all implemented to full X11 spec correctness (not POC stubs).

---

## Context

Phase 6.6 proved the KMS backend viable across the major WM matrix (fvwm3 fully working,
wmaker mostly, e16 partially). Phase 6.7 transitions from proof-of-concept to production
quality: every item landed is correct and complete. Items too large for one session are
deferred to 6.8+, but nothing in 6.7 is a stub we intend to revisit.

Known WM matrix blockers addressed here:
- e16 widget clicks dead → AllowEvents ReplayPointer
- wmaker title-bar glyphs missing → CompositeGlyphs A1 + y_off
- SHAPE-dependent window outlines → SHAPE compositing
- WMs querying XKB blocking → XKB proxy

---

## Step 6.7.1 — Input: warp_pointer + AllowEvents ReplayPointer

### warp_pointer

`KmsBackend::warp_pointer` currently ignores the call. Full implementation:

1. Reconstruct absolute destination from the target window's position in `self.windows`
   plus `(dst_x, dst_y)`. Clamp to screen bounds.
2. Update `self.cursor_x / cursor_y`.
3. Push a synthetic `PointerEventKind::MotionNotify` through `BackendEventSink`.

The server layer already handles MotionNotify → window-under-cursor tracking and
EnterNotify/LeaveNotify generation for window boundary crossings. No new backend
machinery needed for crossing events.

### AllowEvents ReplayPointer

Per X11 spec: the frozen ButtonPress is **not** re-delivered to the grab owner. It is
routed as if no passive grab had matched — to the deepest window under the cursor that
has a ButtonPress event mask (or SubstructureNotify parent), with the passive grab released.

Current state: `frozen_pointer_event` is stored on `ServerState` when a sync passive grab
fires; mode=2 in `nested.rs` opcode 35 handler just clears it.

Full implementation:

1. `nested.rs` AllowEvents mode=2 handler:
   - Lock server state.
   - Extract and clear `frozen_pointer_event`.
   - Clear `pointer_grab`, `pointer_grab_is_passive`.
2. Pass the extracted event to a new `route_button_press_no_grab(event, server, writers)`
   free function that:
   - Finds the deepest child window under event coords via `pointer_target_at`.
   - Fans out the ButtonPress to clients with matching event masks.
   - Skips passive grab matching entirely (no re-grab loop).
3. `route_button_press_no_grab` is extracted from / shared with the existing `server.rs`
   ButtonPress delivery path so the logic is not duplicated.

**Correctness invariant:** the grab owner does not receive a second copy of the event.

---

## Step 6.7.2 — Drawing Primitives: copy_plane + poly_text16 + image_text16

### copy_plane

X11 spec: copies a single bit-plane from src drawable to dst, substituting the GC's
`foreground` for set bits and `background` for clear bits. `plane` is a power-of-two
bitmask selecting which bit in each source pixel to test.

Implementation in `KmsBackend::copy_plane`:

1. Read each source pixel from `src_host_xid`'s `PixmanImage`.
2. Test `(pixel & plane) != 0` → write `foreground`; else write `background`.
3. Apply `current_function` via `fill_rects_with_gc_function` (XOR mode must work).
4. Apply src/dst offsets; clip to both image bounds.

Depth-1 source (wmaker icon masks) is the common case; depth-24/32 handled by same loop.

### poly_text16 / image_text16

`CHAR2B` encoding: each character is two bytes `(high, low)` → codepoint `(high << 8) | low`.

Structure mirrors the 8-bit variants exactly:
- `poly_text16`: delta + string items; sentinel 255 = font change (3 pad + 4-byte fontable);
  advances `cursor_x` by `character_width` per glyph.
- `image_text16`: draws background rectangle first, then renders each glyph.

Both reuse `render_text_string` via `face.load_char(codepoint, RENDER)` — freetype already
supports Unicode; no new font infrastructure needed.

---

## Step 6.7.3 — RENDER: render_change_picture + Gradients + Transforms

### render_change_picture

Full CPxxx attribute support. `PictureState::Drawable` gains new fields for all attributes:

| Attribute | Storage | Effect |
|---|---|---|
| `CPRepeat` | `repeat: Repeat` | call `img.set_repeat()` before composite |
| `CPAlphaMap` | `alpha_map: Option<u32>` (host pic XID) | feed as alpha arg to `composite32` |
| `CPAlphaXOrigin / CPAlphaYOrigin` | `(alpha_x, alpha_y): (i16, i16)` | offset when sampling alpha map |
| `CPClipXOrigin / CPClipYOrigin` | stored on `PictureState` | applied to clip rects |
| `CPClipMask` | XID=0 → clear clip; non-zero → resolve to pixmap, create 1-bit clip region | applied as destination clip |
| `CPGraphicsExposure` | stored flag | no-op until exposure tracking added |
| `CPSubwindowMode` | stored | deferred |
| `CPPolyEdge / CPPolyMode / CPDither` | stored | rendering quality hints, no-op |
| `CPComponentAlpha` | `component_alpha: bool` | each ARGB channel of mask used independently against corresponding source channel |

`PictureState::SolidFill` only needs `CPRepeat` (already set at creation) and
`CPComponentAlpha`.

### render_create_linear_gradient / render_create_radial_gradient

New `PictureState::Gradient { image: PixmanImage }` variant.

- **LinearGradient**: parse two `PointFixed` (16.16) endpoints + N `(Fixed stop, PictureColor)`
  pairs → `pixman_image_create_linear_gradient` FFI. Set `Repeat::Normal` by default.
- **RadialGradient**: parse inner circle `(cx, cy, r)` + outer circle `(cx, cy, r)` + stops
  → `pixman_image_create_radial_gradient` FFI.

If `pixman-rs` does not expose gradient creation in its safe API (likely), add `unsafe` FFI
wrappers in the existing pixman module alongside the existing `composite32` /
`composite_trapezoids` helpers.

Gradient pictures participate in `render_composite` and `render_composite_glyphs` as source
pictures — same dispatch path as `SolidFill`.

### render_set_picture_transform

Parse 36-byte body as nine `Fixed` (16.16) values forming a 3×3 projective matrix. Store as
`transform: Option<pixman::Transform>` on `PictureState::Drawable` and
`PictureState::Gradient`. Before each `composite32` call that uses this picture as source,
call `pixman_image_set_transform()`. Reset to identity when transform is all-zero or cleared.

---

## Step 6.7.4 — CompositeGlyphs Improvements

Three fixes to `composite_glyphs_onto` and supporting infrastructure.

### y_off per-glyph advancement

`delta_y` from each item is parsed but pen Y is never advanced. Fix: `pen_y += delta_y` per
item, mirroring the existing `pen_x += delta_x`. Unblocks vertical text and mixed-direction
runs. No data structure changes needed.

### Mid-stream glyphset switch

Sentinel byte (count=255) followed by a non-zero 4-byte XID switches the active glyphset.
Fix: when sentinel carries non-zero XID, look it up in `self.glyphsets` and update the
`active_gs` local variable. Unknown XID → continue with previous glyphset, do not abort.

### A1 glyphset support

1. Add `GlyphSetFormat::A1` variant alongside `A8`.
2. `parse_add_glyphs` for A1: store pixel data as pixman `A1` image (1 bpp, MSB-first
   scanlines padded to 32-bit boundaries — same convention as X11 ZPixmap depth-1).
3. `composite_glyphs_onto` for A1: composite using the A1 mask image via
   `pixman_image_composite32(Over, src_color, a1_mask, dst, ...)`. Pixman handles A1
   masks natively.
4. `StoredGlyph` already holds a `PixmanImage`; its format is determined by the glyphset
   format at add time.

---

## Step 6.7.5 — SHAPE Compositing

SHAPE rects are already forwarded from `nested.rs` to `KmsBackend::set_shape_rectangles`
via `mirror_shape_to_host`. That method is currently a no-op.

### Storage

`KmsBackend` gains:

```rust
shape_bounding: HashMap<u32, Vec<RegionRect>>,  // kind=0
shape_clip:     HashMap<u32, Vec<RegionRect>>,  // kind=1
```

keyed by `host_xid`. `set_shape_rectangles` stores into the appropriate map by `kind`.
Empty rects → remove entry (restore default rectangular shape).
`DestroyWindow` / `free_pixmap` also removes entries.

### Compositor application

During the KMS scanout / compositing pass, when blitting a window's `PixmanImage` onto the
framebuffer:

- **Bounding shape (kind=0)**: if present, build a pixman region from the stored `RegionRect`
  list, set as clip on the destination image before `composite32`, clear after. No bounding
  shape → full rectangle (unchanged behaviour).
- **Clip shape (kind=1)**: used by the existing `window_under_cursor` hit-test path, which
  currently uses the full rectangle. Update to intersect with stored clip shape rects when
  present.

### ShapeNotify

`nested.rs` already handles ShapeNotify fan-out to subscribed clients. No new backend event
delivery needed.

---

## Step 6.7.6 — Font Enumeration

### Dependency

Add `fontconfig` crate to `yserver/Cargo.toml` and workspace `Cargo.toml`.

### list_fonts_proxy

1. Translate the X11 XLFD glob pattern to a fontconfig `FcPattern` + `FcObjectSet`
   (family, style, size, spacing, foundry, charset encoding).
2. Call `FcFontList`.
3. Map each result to a synthetic XLFD string via `fc_match_to_xlfd(pattern)`.
4. Truncate to `max_names`.
5. Encode as 32-byte header (name-count) + length-prefixed name strings padded to 4 bytes.

### list_fonts_with_info_proxy

Same fontconfig enumeration, then for each font load via freetype (`FontLoader`, already in
`KmsBackend`) to extract `FontMetrics` (ascent, descent, max char width). Cache the freetype
load in `FontLoader` keyed by resolved file path. Return one variable-length per-font reply (32-byte header + metrics fields + padded name)
with metrics + name, followed by the zero-length terminator. Truncate to `max_names`.

### XLFD construction

Helper `fc_match_to_xlfd(pattern: &FcPattern) -> String` assembles the 14-field XLFD:
`-foundry-family-weight-slant-width-addstyle-pixelsize-pointsize-resx-resy-spacing-avgwidth-registry-encoding`.

Fields not available from fontconfig (`avgwidth`, `resx/resy`) use conventional defaults
(`0`, `75`).

### OpenFont by XLFD

`KmsBackend::open_font` is extended to accept XLFD names: parse the XLFD fields, build an
`FcPattern`, call `FcFontMatch`, get the file path, load with freetype. This makes every
font enumerated by `list_fonts_proxy` openable.

---

## Step 6.7.7 — XKB Proxy

No host X11 on KMS — all XKB replies constructed from `self.xkb_keymap` (xkbcommon).

### Module

New `crates/yserver/src/kms/xkb.rs` — one function per implemented minor, each returning
`Vec<u8>`. Unit-testable in isolation (length checks, field spot-checks, no live X11).

### Minor dispatch

`xkb_proxy` switches on `minor`:
- Reply-required minors → return `Some(Vec<u8>)` (never return `None` for these).
- Event-only minors (e.g. SelectEvents, minor 1, already handled in `nested.rs`) → return
  `None`.

### Minor 0 — UseExtension

32-byte reply: `success=1`, `server-major=1`, `server-minor=0`.

### Minor 24 — GetControls

92-byte reply. Populate from xkbcommon: `repeat_delay`, `repeat_interval`, `groups_wrap`,
`num_lock_mask`. Fields with no xkbcommon equivalent zeroed. Enable-flags word set to
`PerKeyRepeat | RepeatKeys`.

### Minor 8 — GetMap

Variable-length reply encoding key types, sym maps, explicit maps, and modifier map — all
derived from xkbcommon by iterating
`xkb_keymap.min_keycode()..=xkb_keymap.max_keycode()`:

- **Key types**: enumerate via `xkb_keymap.key_get_type(kc, group)`, deduplicate by name,
  encode as `(mods_mask, num_levels, map_entries[], preserve[])`.
- **Sym maps** (`KeySymMap`): per keycode + group, collect
  `xkb_keymap.key_get_syms_by_level(kc, group, level)`, encode as
  `(kt_index[4], group_info, width, syms[])`.
- **Modifiers map** (`ModMapKey`): for each keycode with a modifier mapping, encode
  `(keycode, mods)`.
- **Virtual modifiers**: encode name-atom list from keymap.

Reply header: `present` bitmask (which tables included), `min_key_code`, `max_key_code`,
per-section lengths. All lengths must be byte-exact.

### Minor 17 — GetNames

Returns atom-indexed names for key groups, virtual modifier names, indicator names, key
aliases, and radio group names. Group names and vmod names derive from xkbcommon directly.
Key aliases and radio groups: empty. Indicator names: standard set (`Caps Lock`, `Num Lock`,
`Scroll Lock`).

### Minor 20 — GetCompatMap

Returns compatibility map (sym interprets + group compats). xkbcommon does not expose
sym-interpret internals — return empty compat map with correct wire header. Most WMs do not
block on this.

### All other reply-requiring minors

Return a minimal valid reply (32-byte header, `success=1`, all-zero payload) so clients
unblock. Log `debug!` for each unimplemented minor.

---

## Testing

Each step has unit tests in `kms/backend.rs` (or `kms/xkb.rs` for step 6.7.7):

- **6.7.1**: ReplayPointer delivers to correct window, not grab owner; warp updates cursor.
- **6.7.2**: copy_plane fg/bg substitution correct for depth-1 and depth-24; text16 renders
  Unicode codepoint (high<<8|low).
- **6.7.3**: ChangePicture CPRepeat/CPAlphaMap/CPClipMask round-trips; gradient picture
  composite produces non-zero pixels; transform matrix stored and applied.
- **6.7.4**: y_off advances pen; glyphset switch uses new set; A1 glyph renders correct
  pixels.
- **6.7.5**: shape rects stored and applied as clip; hit-test excludes pixels outside clip
  shape.
- **6.7.6**: list_fonts returns non-empty list; open_font by XLFD loads successfully.
- **6.7.7**: each XKB minor reply has correct byte length; UseExtension success=1; GetMap
  min/max keycode match keymap.

---

## Out of scope (deferred to 6.8+)

- SyncPointer one-event thaw (AllowEvents mode=1).
- CPSubwindowMode, CPPolyEdge/Mode/Dither effects.
- Host-cursor drift / lock.
- VT_SETMODE / logind / suspend-resume / hotplug.
- Full XKB compat map (sym interprets).
- MIT-SHM shared memory pixmap path.
