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

2. **Missing ClipByChildren subtraction on the painted drawable.**
   When `picture(W)` has the default `subwindow-mode = ClipByChildren`
   and paint targets `W` directly, the effective paint region is
   `W's geometry MINUS W's mapped children`. In the systray case the
   applet's `FillRectangles op=Clear` targets `picture(proxy)`; the
   proxy is fully covered by its embedded-icon child, so the effective
   region is empty — Xorg produces no paint and no damage. yserver
   skips this subtraction, emits damage on the full proxy drawable,
   and the applet's own `DamageCreate` on `proxy` fires its own
   notify → loop.

   **Important framing nuance** (per codex 2026-06-01): this is NOT
   "a parent paint fans damage into its children." The backtraces
   show the damage emit is on the painted drawable itself (the
   proxy / socket) — the applet wakes because it has DamageCreate
   on that same drawable. The ClipByChildren machinery needed is
   "subtract this drawable's children from its own paint region,"
   not "stop damage from descending into children" (`damage_fanout`
   already ascends only, confirmed by code read).

## Recommended staged approach

The two problems are layered. Land the narrower fix first and
verify whether the storm collapses before broadening scope:

### Stage 1 — narrow: damage only the actual fill rects

At `process_request.rs:1616-1622`, replace
`accumulate_damage_full_to_state(state, dst_drawable)` with damage
emits keyed to `req.rects` (or their bounding box). Verify with a
fresh `YSERVER_DAMAGE_BACKTRACE=1` capture whether the storm rate
drops materially.

Expected outcome: limited. In the captured systray traces the
applet's `Clear` rects span the full drawable (`{x=0 y=0 w=24 h=27}`
on a 24×27 proxy), so the rect-bounding-box damage == full-drawable
damage. Stage 1 will tighten correctness for other clients without
necessarily quieting the systray loop.

If Stage 1 fixes the storm: great, done. Otherwise proceed to
Stage 2.

### Stage 2 — broader: ClipByChildren subtraction on the painted drawable

For paint ops targeting a window picture with `subwindow-mode =
ClipByChildren` (the default), subtract the union of mapped
child-window regions from the paint's rect set before emitting
damage AND (ideally) before issuing the actual paint to the
engine. When the result is empty, both are no-ops — matching
Xorg's behaviour.

Stage 2 wants to be a shared helper applied across all RENDER
paint sites:

- RENDER FillRectangles (opcode 26, `process_request.rs:1622`).
- RENDER Composite (opcode 8, `process_request.rs:1592` —
  `accumulate_damage_full_to_state(dst_drawable)` with no
  ClipByChildren intersection).
- Trapezoids / Triangles / TriStrip / AddTraps (all use
  `accumulate_damage_full_to_state` after their respective backend
  calls).

Whether Stage 2 also needs to gate the actual paint (not just the
damage emit) depends on how the applet's `Clear` interacts with
the backing under Manual redirect. Under ClipByChildren in Xorg
the paint is also a no-op, which is why the embedded icon's
pixels in the shared backing survive each cycle (load-bearing for
icons-visible). yserver's paint path may need the same gate, but
that's the second-order concern after the storm itself stops.

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
