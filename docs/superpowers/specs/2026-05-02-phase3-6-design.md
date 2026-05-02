# Phase 3.6 — Per-client GC mirroring + e16 popup body rendering

## Status: design

Follow-up to Phase 3.5 (`2026-05-01-phase3-5-extension-completion-design.md`).
Phase 3.5 finished the extension surface (MIT-SHM, DAMAGE, COMPOSITE,
RENDER ChangePicture XID translation, GC clip-mask forwarding, host
PutImage chunking). e16 starts cleanly and runs, wmaker renders a
complete frame with appicons + close button + miniaturise dot.

The remaining e16 visible regression is **popup bodies render as
solid grey** (no gradient image, no menu items). Investigation in
Phase 3.5 (task #23 / `git stash` `wip-e16-popup-tile-forwarding`)
located the cause and points at an architectural change.

## Root cause

e16 renders popup bodies via `XCopyAreaTiled` (e16 source
`src/menus.c:917`, `src/x.c:1656`):

```c
gcv.fill_style = FillTiled;
gcv.tile = src;             // pre-rendered popup body image pixmap
gcv.ts_x_origin = sx;
gcv.ts_y_origin = sy;
gcv.clip_mask = mask;
gc = EXCreateGC(dst, GCFillStyle | GCTile | GCTileStipXOrigin |
                     GCTileStipYOrigin | GCClipMask, &gcv);
XFillRectangle(disp, dst, gc, dx, dy, w, h);
```

That is, e16 uses `PolyFillRectangle` as a "tiled blit": create a GC
with `tile = <pre-uploaded image pixmap>` and `fill_style = Tiled`,
then `XFillRectangle` to copy the tile into a destination pixmap.
Confirmed in our log:

```
#2336 CreateGC gc=0x10011d mask=0x90500
       fill_style=Some(1) tile=Some("0x100128") clip_mask=Some("0x0")
#2347 PolyFillRectangle …
```

ynest currently parses none of `fill_style`, `tile`, `stipple`,
`ts_x_origin`, `ts_y_origin` from `CreateGC` / `ChangeGC`. The host's
GC stays in default `Solid` fill, `XFillRectangle` paints the
destination with the foreground colour, the destination pixmap is
solid grey, and the popup body shows that solid grey.

## Architecture problem

ynest uses **one shared host GC** (`host.gc_id` in
`crates/yserver-core/src/host_x11.rs`). Drawing ops bind the client's
GC state to the shared host GC just-in-time via:

- `apply_gc_clip(gc_state, x_off, y_off)` — clip rectangles or pixmap
- foreground colour (`change_foreground` in host_x11)
- (proposed in WIP) `apply_gc_fill(fill_state, x_off, y_off)` — tile / stipple

This works but is fragile when more state needs to be tracked. The
WIP attempt to add `fill_style` to the state machine broke right-click
delivery to e16 — likely a state-leak between two GCs sharing the
host GC, but the exact failure isn't pinned down.

[Xnest](https://gitlab.freedesktop.org/xorg/xserver/-/blob/master/hw/xnest/GC.c)
solves this with **per-client-GC → per-host-GC mapping**: every client
GC creates a matching host GC at `XCreateGC` time, and every
`XChangeGC` mirrors all relevant fields to the host GC. Drawing ops
use the per-client host GC directly. No state synchronisation, no
shared mutable state across clients.

## Plan

Refactor ynest to the Xnest model.

### Resource changes

`Gc` (in `resources.rs`) gains:

```rust
pub struct Gc {
    // existing
    pub host_gc_xid: Option<u32>,            // NEW

    // newly-tracked GC fields (already in WIP):
    pub fill_style: u8,                       // 0=Solid, 1=Tiled, 2=Stippled, 3=OpaqueStippled
    pub tile_pixmap: Option<ResourceId>,
    pub stipple_pixmap: Option<ResourceId>,
    pub ts_x_origin: i16,
    pub ts_y_origin: i16,
    // (more fields listed below — function, plane_mask, line_style, etc.)
}
```

### CreateGC handler

On client `CreateGC(gc, drawable, value-list)`:
1. Resolve the client drawable to a host drawable (existing
   `host_drawable_target`). Pull the depth.
2. `host.create_gc(depth, host_drawable, ...)` allocates a host XID
   and sends `CreateGC(host_gc, host_drawable, value-mask, value-list)`
   with translated tile / stipple / clip-mask XIDs.
3. Store `host_gc_xid` on the local `Gc` resource.

### ChangeGC handler

On client `ChangeGC(gc, value-list)`:
1. Translate any tile / stipple / clip-mask XIDs in the value-list
   from client space to host space.
2. `host.change_gc(host_gc_xid, value-mask, translated_values)`.
3. Update local `Gc` fields.

### Drawing ops

Each drawing op handler (PolyFillRectangle, CopyArea, PolySegment,
PolyText, etc.) currently does:

```rust
host.poly_fill_rectangle(target.host_xid(), foreground, &translated)?;
```

…where `host.poly_fill_rectangle` uses `self.gc_id`. After the
refactor:

```rust
let host_gc = s.resources.gc_host_xid(gc_id);
host.poly_fill_rectangle(target.host_xid(), host_gc, &translated)?;
```

Each `host.*` wrapper takes the host gc xid as a parameter.
`host.gc_id` (the bootstrapped fallback GC) stays around for internal
operations that don't have a client GC (the container's bg-pixel
fill, CreateSegment's synthetic put_image).

### What gets dropped

- `current_clip` / `current_fill` state machines on `HostX11`
- `apply_gc_clip` / `apply_gc_fill` / `set_clip_pixmap` /
  `set_fill_tile` helpers — replaced by direct ChangeGC forwarding
- `gc_clip_state` / `gc_fill_state` resolvers in `resources.rs`
- The "translate clip origin by sub-window offset" trick — instead
  the clip / tile origins are stored on the host GC and the host
  handles offset-into-top-level itself when drawing on a sub-window's
  translated coordinates.

### Sub-window offset

Drawing on a sub-window currently translates the destination
coordinates by the sub-window's offset to the top-level (since
sub-windows have no host backing). With per-client GCs the **clip and
tile origins also need to shift by the sub-window offset on each draw**
— either by re-issuing ChangeGC right before the draw, or by tracking
the last-applied origin and re-issuing only when it changes. Easiest:
re-issue clip/tile origins per draw when the sub-window's offset
differs from the last call. This is cheaper than the current state
machine because we only re-issue *origin* (8 bytes) rather than the
whole clip/tile state (28 bytes).

### CopyGC

Forward to host: `XCopyGC(host_src_gc, value_mask, host_dst_gc)`.

### FreeGC

Forward to host: `XFreeGC(host_gc)` and clear the local mapping.

## Open questions

1. **Which depth to bind a host GC to?** Clients can change the GC's
   drawable across draws (XChangeGC takes no drawable, but the GC's
   *initial* drawable's depth fixes its compatible drawable set).
   ynest's current `ensure_gc_for_depth` sidesteps this by having one
   GC per depth. With per-client GCs we use the depth at CreateGC
   time and assume depth never changes — true for most clients.
   wmaker / e16 / GTK / Qt all create depth-specific GCs.

2. **Do we still need `current_foreground` caching?** Probably yes —
   `change_foreground` short-circuits when the same colour is reused.
   With per-client GCs, foreground is set on the host GC at create /
   change time and persists, so `change_foreground` becomes a no-op
   (the GC already has the right fg). One less ChangeGC per draw.

## Tests

- Unit: gc_host_xid is set after CreateGC and cleared after FreeGC.
- Unit: ChangeGC tile pixmap translates client → host xid.
- Integration: e16 popup with menu items renders correctly in the
  visible test (manual screenshot diff against Xephyr).
- Regression: wmaker close button and miniaturise dot still render
  (the existing GC clip-mask forwarding test).

## Out of scope (defer to Phase 3.7+)

- **Stipple / OpaqueStippled fill_style.** wmaker / e16 / GTK don't
  use these in our traces. Leave fill_style 2 / 3 as a no-op and
  document.
- **Per-client-GC for the bootstrap GC** (`host.gc_id`). Keep that
  internal-only.
- **GraphicsExposures from host GC.** When forwarding, set
  `graphics_exposures = false` on host GCs to avoid spurious
  GraphicsExpose / NoExpose events leaking back through the host pump.
- **Render Picture caching.** Picture XIDs are already translated for
  ChangePicture; no change here.
