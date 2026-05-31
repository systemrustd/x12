# Tray-loop emit site confirmed — 2026-06-01

`YSERVER_DAMAGE_BACKTRACE=1` capture on yoga (one socket — drawable `0x2400011`,
the notification-area-applet's tray-icon proxy, 638 damage emits during the
trace):

| Emits | Source |
|-------|--------|
| 630 (98.7%) | `handle_render_request` at `process_request.rs:1622` — RENDER FillRectangles (opcode 26) |
| 6 | `handle_map_window` at `process_request.rs:12015` |
| 2 | `handle_configure_window` at `process_request.rs:10253` |

The 630 figure matches the steady-state storm; the 8 others are one-off
setup events. The FillRectangles emit is the loop driver.

## The code

`process_request.rs:1616-1623`:

```rust
if !req.rects.is_empty()
    && let Some(dst_drawable) = state.resources.picture(req.dst).and_then(|p| p.drawable)
{
    let _dropped = accumulate_damage_full_to_state(state, dst_drawable);
}
```

## Two structural problems

1. **Over-broad damage area.** `accumulate_damage_full_to_state` damages
   the entire drawable extent, ignoring `req.rects`. Should at minimum
   damage only the bounding box of the supplied rectangles.

2. **Missing ClipByChildren.** When a parent window draws into itself
   under the default `subwindow-mode = ClipByChildren`, pixels covered
   by child window regions belong to the children, not the parent —
   the parent's paint cannot touch them, and damage on the parent
   should not include them. yserver's accumulate path doesn't enforce
   that, so the damage region overlaps the socket children, the
   fanout matches the sockets' `DamageCreate` handles, and the
   applet's per-vsync loop closes.

## Scope of the fix

Same shape applies to other RENDER paint paths:

- RENDER Composite (opcode 8, `process_request.rs:1592` —
  `accumulate_damage_full_to_state(dst_drawable)` with no
  ClipByChildren intersection).
- Trapezoids / Triangles / TriStrip / AddTraps (all use
  `accumulate_damage_full_to_state` after their respective backend
  calls).

The ClipByChildren machinery wants to be a shared helper applied
across all RENDER damage-accumulation sites, not a one-off patch at
line 1622.

## Reference: Xorg behaviour

- Picture's default `subwindow-mode = ClipByChildren`:
  `xserver/render/picture.c:719`.
- `miValidatePicture` uses the window's clip-list for ClipByChildren:
  `xserver/render/mipict.c:112-118`.
- `miColorRects` passes the picture's `subWindowMode` into the
  scratch GC: `xserver/render/mirect.c:55`.
- `damagePolyFillRect`'s `TRIM_BOX` against `pGC->pCompositeClip`
  reduces damage to the GC-visible region:
  `xserver/miext/damage/damage.c:438`, `:1193`.

Net effect in Xorg: a parent's FillRectangles Clear on a window
fully covered by child windows is a true no-op — empty paint clip,
empty damage region. Nothing wakes the children's damage subscribers.
