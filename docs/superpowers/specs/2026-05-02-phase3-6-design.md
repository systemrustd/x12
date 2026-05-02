# Phase 3.6 — Full window-tree mirroring (Xnest model)

## Status: design

Supersedes the earlier per-client-GC scope (now Phase 3.7). Architectural
audit against Xnest (`xserver/hw/xnest/`) showed sub-window virtualisation
to be the deeper structural divergence; per-client-GC mirroring becomes
much smaller after sub-window mirroring lands, so we tackle this first.

Revised after a codex review pass — flagged corrections folded into the
relevant sections below: visual/colormap-at-CreateWindow promoted into
scope (BadMatch on ARGB clients otherwise), stacking semantics for
TopIf/BottomIf/Opposite, MapWindow realize sequence, host-xid lifetime
race, NameWindowPixmap lifetime, GetInputFocus-sync removal, coordinate
invariant.

## Goal

Mirror **every** client window — top-level *and* sub-window — to a host
X window, parented under the client-side parent's host xid. This is the
Xnest architecture and replaces ynest's "top-levels host-backed,
sub-windows virtual + draws translated by `(x_offset, y_offset)`" model.

## Why

ynest currently host-backs only top-level windows (parent == ROOT).
Sub-window draws are rerouted to the top-level via
`resources::top_level_host_target` with a synthesised offset. This
produces a long tail of correctness gaps:

- No host-side clipping by sub-window rect (we paper over it via
  `apply_gc_clip` only when the GC carries an explicit clip)
- Sibling stacking among sub-windows is fictional — siblings paint in
  registration order with no occlusion
- `ConfigureWindow` on a sub-window emits no host invalidation, no
  Expose, no auto-clear of vacated regions
- `CopyArea` from a partially-occluded sub-window source returns wrong
  pixels (host source is the entire top-level)
- `bit_gravity` / `win_gravity` on resize: lost
- Per-sub-window cursors can't take effect (`define_cursor` short-
  circuits when `host_xid` is None)
- Every drawing handler carries the `(x_off, y_off)` translation hack

Side benefit: the original Phase 3.6 (per-client-GC mirroring → e16
popup tiles) shrinks dramatically. Without sub-window offsets, clip and
tile origins don't need re-issuing per draw — the host GC's stored
origin is correct as-is.

## Reference: Xnest

Read end-to-end before implementing:

- `Window.c` `xnestCreateWindow` — every window calls `XCreateWindow`
- `XNWindow.h` macros — `xnestWindow(pWin)`, `xnestWindowParent(pWin)`
  (parent's host xid OR screen default window if root),
  `xnestWindowSiblingAbove(pWin)`
- `Window.c` `xnestConfigureWindow` — early-outs on no-change, walks
  siblings on `CWStackingOrder` and re-emits per-sibling
  `XConfigureWindow(stack_mode + sibling)`
- `Window.c` `xnestChangeWindowAttributes` — drops dix-handled bits
  (`CWBackingStore`, `CWBackingPlanes`, `CWBackingPixel`, `CWSaveUnder`,
  `CWWinGravity`, `CWEventMask`, `CWDontPropagate`); forwards the rest
- `Window.c:91` — child windows get `event_mask = ExposureMask` only;
  input bubbles to the screen default window
- `Events.c` `xnestCollectExposures` — host Expose → translate to
  top-level coords → `miSendExposures`

## Resource changes

`resources::Window`:

- `host_xid: Option<u32>` becomes effectively always-Some for class
  `InputOutput` (kept as `Option` for ergonomics with `InputOnly` and
  during construction; access through a `host_xid_required()` helper
  with `.expect()` on the InputOutput invariant).
- Add `border_pixmap_host_xid: Option<u32>` in parallel to the existing
  `background_pixmap_host_xid` — same snapshot pattern, used by
  `ChangeWindowAttributes` forwarding.
- `composite_named_pixmaps` semantics are unchanged but now usable on
  sub-windows too (NameWindowPixmap can target any host-backed window).

No changes to `Pixmap` or `Gc` in this phase — those belong to Phase 3.7.

## Handler changes

### CreateWindow (opcode 1)

Currently: only allocates `host_xid` when `parent == ROOT_WINDOW`
(nested.rs around line 4866 / 4899).

After: for any `class != InputOnly` window:

1. Look up parent → `parent.host_xid` (must be Some by invariant).
2. **Visual / colormap selection** (per Xnest `Window.c:94-118`): if
   `pWin->visual == parent.visual`, pass `visual = CopyFromParent` and
   omit `CWColormap`. If they differ (32-bit ARGB client on a 24-bit
   parent — common for GTK3/Qt/conky), look up the host visual via the
   visual table and pass `CWColormap` with either the client's
   colormap's host xid (if mirrored) or the host's default colormap for
   that visual. **In scope for 3.6** — without it, ARGB clients get
   `BadMatch` from the host on CreateWindow. A minimal visual table
   (root-visual + a 32-bit ARGB visual) is sufficient; full visual
   mirroring (audit #6) remains deferred.
3. `host.create_window(host_parent, x, y, w, h, border, depth, class,
   visual, event_mask, override_redirect, ...)`. Children get
   `event_mask = ExposureMask`. Top-levels (only the implicit container
   for now, since clients' top-levels reparent to the container) get
   `ExposureMask | StructureNotifyMask`.
4. Forward `background_pixel`, `background_pixmap` (translated),
   `border_pixel`, `border_pixmap` (translated), `bit_gravity`,
   `override_redirect` if present in the value-list.
5. Store host xid on the new Window. **Drop the post-CreateWindow
   `GetInputFocus` sync** that `host_x11::create_subwindow` currently
   performs — full mirroring multiplies that roundtrip across every
   toolkit widget tree. Reserve syncs for explicit error fences /
   debug paths.

`InputOnly` windows do not get a host backing (Xnest does, but their
only purpose is event dispatch, which ynest handles internally — no
host counterpart needed). **Add a regression test for InputOnly
overlays above InputOutput children** before committing to skip; if
grabs/crossing on InputOnly siblings break, revisit.

### DestroyWindow (4) / DestroySubwindows (5)

`host.destroy_window(host_xid)` for each window being destroyed. The
resource cascade is already correct.

### ChangeWindowAttributes (2)

Forward via `host.change_window_attributes(host_xid, mask, values)`:

- `background_pixmap` (translate to host xid; None / ParentRelative
  passed through as-is)
- `background_pixel`
- `border_pixmap` (translate)
- `border_pixel`
- `bit_gravity`
- `override_redirect`
- `cursor` (translate; replaces today's `define_cursor` flow which
  currently only works for top-levels)
- `colormap` — pass through unchanged for now (TrueColor passthrough);
  proper translation deferred to colormap phase

Drop (per Xnest):

- `backing_store`, `backing_planes`, `backing_pixel`, `save_under` —
  no-op
- `event_mask`, `do_not_propagate` — DIX-equivalent (ynest dispatches
  events itself; host child windows always get ExposureMask only)
- `win_gravity` — DIX handles geometry; host doesn't need to know

### ReparentWindow (7)

`host.reparent_window(child_host, new_parent_host, x, y)`. Update
`parent` on the child's resource. Re-issue stacking on the new
parent's children list (Xnest does this implicitly by maintaining
`sibling_above`).

**Coordinate invariant**: the host x/y for CreateWindow / Reparent /
ConfigureWindow is `origin - border_width` (Xnest `Window.c:121-135,
184-212`). ynest stores protocol `(x, y)` (the origin); subtract the
window's `border_width` when building the host request, and document
this convention in `host_drawable_xid`. Add a bordered-subwindow
regression test so we don't shift twice or fail to shift.

Geometry mutations from reparent or border-width changes must reuse
the same host configure path as `ConfigureWindow` (Xnest's
`xnestPositionWindow` model). Don't bypass it from inside Reparent.

### ConfigureWindow (12)

1. For x/y/width/height/border-width: forward to
   `host.configure_window` if the value differs from the stored value
   (Xnest's early-out — saves host roundtrips on noop calls from
   toolkits that re-set unchanged geometry).
2. **Stacking — two-step.** First, in DIX-equivalent ynest code,
   resolve the client verb to a final child-list ordering. The
   protocol's stack modes are conditional, **not** unconditional
   raise/lower:
   - `Above [sibling]` — place above sibling (or topmost if no sibling)
   - `Below [sibling]` — place below sibling (or bottommost)
   - `TopIf [sibling]` — raise to top **only if** occluded by sibling /
     a sibling
   - `BottomIf [sibling]` — lower **only if** occluding a sibling
   - `Opposite [sibling]` — swap with TopIf/BottomIf depending on
     current occlusion
   `resources::restack_window` currently treats these as unconditional;
   that's a pre-existing bug to fix as part of this phase.
   Then mirror the resolved child-list to the host: issue `Above` on
   the topmost child, then walk downward issuing `Below sibling = the
   previously-placed sibling` for each subsequent child. Convention:
   ynest stores last-child-is-top.
3. **Cache `sibling_above` to suppress no-op restacks** (Xnest pattern,
   `Window.c:235-236`). When the resolved child-list is identical to
   the previous resolved order, emit no host configure requests at all.
   In widget-heavy apps a single client request can touch many siblings
   that didn't actually move; the cache pays for itself.

### MapWindow / MapSubwindows (8, 9)

Per Xnest `xnestRealizeWindow` (`Window.c:354-360`):

1. Pre-map host stacking refresh — re-issue the parent's resolved
   child-list to the host (windows configured while unmapped didn't
   propagate). Equivalent to `xnestConfigureWindow(CWStackingOrder)`.
2. SHAPE refresh hook — leave a call site even though SHAPE is
   deferred (see "SHAPE deferral" below); a no-op `host_shape_window`
   stub keeps the order honest for when SHAPE lands.
3. `host.map_window(host_xid)`.

Cascade via children list for MapSubwindows.

### UnmapWindow / UnmapSubwindows (10, 11)

`host.unmap_window(host_xid)`. Cascade.

### CirculateWindow (13)

`host.circulate_window`.

### Drawing handlers (62, 63, 65–79)

Currently every drawing op site does:

```rust
let (target_xid, x_off, y_off) =
    s.resources.top_level_host_target(req.drawable);
let shifted = translate_rects(&req.rects, x_off, y_off);
host.poly_fill_rectangle(target_xid, gc, &shifted)?;
```

After:

```rust
let target_xid = s.resources.host_drawable_xid(req.drawable)?;
host.poly_fill_rectangle(target_xid, gc, &req.rects)?;
```

The translation drops everywhere (~20 sites by `grep host_drawable_target`).

`top_level_host_target` and the `(x_off, y_off)` second-return on
`host_drawable_target` get deleted. `apply_gc_clip` loses its
`(x_off, y_off)` parameters — clip rectangles and tile origins are now
in sub-window-local coordinates as the client sent them.

## Input event path

Critical invariant: ynest dispatches input events itself, based on its
internal window tree. Host pointer / keyboard events must continue to
arrive *only* on the container, with container-local coordinates.

With sub-window mirroring, X11's default behaviour would deliver
ButtonPress to the leaf sub-window under the cursor. Prevent this by
selecting **only ExposureMask** (no input event masks) on host child
windows, exactly as Xnest does (`Window.c:91`). X11's
"events propagate to ancestors with matching mask" rule then naturally
bubbles input events up to the container.

Verify with an integration test using **XTest** (`XTestFakeMotionEvent` /
`XTestFakeButtonEvent`) — a synthetic `XSendEvent` can bypass propagation
semantics depending on the propagate flag and is not a faithful test.
Create a child host window with `event_mask = ExposureMask`; inject a
ButtonPress at coords inside the child; verify the event arrives on the
parent (container) and ynest's existing dispatch logic delivers it to
the right client window. Reference: X11 protocol §11
(libX11 docs `CH11.xml:93-101`, `CH10.xml:1111-1120`) — device events
propagate from the source window to the closest interested ancestor
unless `do_not_propagate` blocks them.

## Expose path

Today: host Expose on a top-level → look up window by `host_xid` →
emit Expose on that window (in window-local coords).

After: same logic, but the `host_xid` lookup may resolve to a
sub-window. The pump path at nested.rs ~line 966
(`s.resources.window_by_host_xid`) already does this lookup; verify
that emitted Expose coordinates are window-local (they should be — the
host already gives us window-local coords on the sub-window).

GraphicsExpose / NoExpose (architecture-audit item #2) is *not* fixed
here — that needs a sequence-routing infrastructure between the host
pump and the originating client. Phase 3.8.

## Host XID lifetime

A draw handler can resolve a host xid, then release the server lock
before issuing the host op; meanwhile another client can destroy the
window. Today this is masked by ynest's coarse server-state lock; with
sub-window mirroring there are O(N) more host xids in flight and the
race surface grows.

Approach:
1. **Unregister the `host_xid → window` mapping at DestroyWindow
   start**, before issuing `host.destroy_window`. Stale host Expose /
   ConfigureNotify arriving on the destroyed xid then falls through
   the lookup and is dropped, instead of being misrouted to a freshly
   re-allocated client window.
2. **Generation-tagged host ops** (or simpler: serialise host ops per
   window through the existing host pump) — a draw queued against a
   destroyed host xid completes as a host BadDrawable, which we already
   absorb. Verify this in a stress test.
3. Keep COMPOSITE named pixmaps alive (next section) so client-side
   pixmap resources don't dangle.

## COMPOSITE NameWindowPixmap lifetime

Define explicitly:

- **Reparent**: must NOT invalidate the named pixmap. The host pixmap
  binding is per-window-content, not per-tree-position; the COMPOSITE
  spec only requires invalidation on resize or redirect-state change.
- **Resize**: invalidate (existing behaviour from Phase 3.5).
- **DestroyWindow**: client-side pixmap resources from prior
  NameWindowPixmap calls remain valid until the *client* frees them
  via FreePixmap. Keep the host pixmap alive until then. Today's
  `composite_named_pixmaps: Vec<NamedCompositePixmap>` on Window must
  detach into a free-list-on-window-destroy, not free the host pixmaps
  immediately.
- **Redirect-state change**: invalidate (existing behaviour).

## Container resize / root

Container ConfigureNotify handling
(`handle_host_container_resize` at nested.rs ~line 1014) stays as-is.
**Filter** on `host_xid == container` to avoid treating a
sub-window resize as a root resize (architecture-audit item #8 —
small fix, fold in here).

## What gets deleted

- `resources::top_level_host_target`
- The `(x_off, y_off)` second return on `resources::host_drawable_target`
  for window targets
- The `(x_off, y_off)` parameters on `apply_gc_clip`
- The "draw on top-level + manually shift coords" path in every
  drawing handler
- The `ensure_top_level_host_window_for_subwindow` lazy-promote logic,
  if it exists today (it doesn't — sub-windows currently have no
  promotion path; this just makes sure we don't accidentally introduce
  one)

## What's preserved

- Container is still the input event sink
- Pixmap drawables unchanged — they remain host-backed pixmaps
- COMPOSITE NameWindowPixmap unchanged in shape (but now legitimately
  works on sub-windows, since they have a host xid to bind a named
  pixmap to)
- `background_pixmap_host_xid` snapshot pattern preserved and extended
  to `border_pixmap_host_xid`

## Open questions

1. **Top-level event mask.** Top-levels need
   `ExposureMask | StructureNotifyMask` (so root resize still pumps
   ConfigureNotify); children need `ExposureMask` only. The
   `is_top_level = parent == ROOT_WINDOW` condition determines which.

2. **Background-pixel on sub-windows.** Currently only set on root
   container at init. With sub-window forwarding, `background_pixel`
   reaches the host CreateWindow value-list directly. Verify that
   default-init paint of a newly-mapped sub-window with bg-pixel
   produces the right colour (host paints automatically on map).

3. **InputOnly windows.** Skip host backing entirely. ynest already
   dispatches events for them internally; nothing for the host to do.
   Confirm via Xnest that this matches behaviour (Xnest *does* create
   host InputOnly windows — they participate in stacking but not
   drawing. We may regret skipping if it turns out InputOnly stacking
   matters; **add an InputOnly-overlay-above-InputOutput-children
   regression test** before deciding to skip; revisit if a real
   client breaks.)

## Tests

- Unit (resources): `create_window` with non-root parent allocates the
  child Window resource and the `host_xid` field is set after the
  CreateWindow handler runs. Add a fake host that records calls.
- Unit (host_x11): `create_window` request emitted with
  `event_mask = ExposureMask` for child, `ExposureMask | StructureNotifyMask`
  for top-level, attribute mask for `background_pixel`, etc.
- Unit (nested): drawing on a sub-window resolves to its own host xid
  (not the top-level's), with no coordinate translation.
- Unit: ConfigureWindow stacking — separate tests for each of the five
  stack modes (`Above`, `Below`, `TopIf`, `BottomIf`, `Opposite`) plus
  no-op cases. Verify the resolved child-list mirrors to a host
  Above-then-Below-walking sequence and that an unchanged child-list
  emits zero host configure requests (cache hit).
- Unit: ChangeWindowAttributes(cursor) on a sub-window calls
  XDefineCursor on its host xid.
- Unit: container ConfigureNotify is treated as root resize; non-
  container ConfigureNotify is ignored (audit item #8).
- Unit: `host_drawable_xid` returns coordinates that account for
  border-width (Xnest `Window.c:121-135` invariant).
- Integration (XTest): ButtonPress injected inside a sub-window with
  `event_mask = ExposureMask` is delivered via the parent (container).
- Integration (visible): wmaker close button still renders, e16 starts.
  Drawing on a sub-window that's clipped by a sibling shows the
  sibling-clipped result.
- Integration: CopyArea from a partially-occluded sub-window source
  returns the visible portion only (currently returns the full source
  including the occluded region).
- Integration (visual): a 32-bit ARGB visual CreateWindow does not
  produce a host BadMatch (ARGB GTK3 / Qt / conky path).

## Phase 3.7 follow-up

Per-client GC mirroring (the original 3.6 design). After 3.6 lands:

- No `(x_off, y_off)` to thread through CreateGC/ChangeGC
- Clip rectangles and tile origins are stored on the host GC and never
  need re-issuing per draw
- `Gc.host_xid: Option<u32>`, `fill_style`, `tile_pixmap`,
  `stipple_pixmap`, `ts_x_origin`, `ts_y_origin` fields as drafted
- e16 popup body tiling resolves
- GC font field forwards correctly (audit item #4)

The WIP stash `wip-e16-popup-tile-forwarding` has reusable parser
extensions and a `gc_fill_state` resolver. Pull it back in for 3.7
once 3.6 is merged.

## Out of scope (defer)

- **SHAPE extension forwarding** (`xnestSetShape`, `xnestShapeWindow`).
  *Visible cost:* wmaker / e16 / fvwm shaped frames, dockapps,
  iconboxes, pagers, and shaped menus will render as their bounding
  rectangle (corners square instead of rounded; non-rectangular menus
  show their full bbox). Document this expectation; do not judge WM
  fidelity until SHAPE lands. The MapWindow realize sequence already
  reserves a SHAPE call site so adding it later is a no-op stub
  replacement.
- GraphicsExpose / NoExpose routing (audit #2 → Phase 3.8)
- **Glyph cursor support** (audit #3 partial). *Visible cost:* pointer
  feedback over sub-windows is wrong (no I-beam over text widgets, no
  resize cursor over WM frame edges) even after per-window cursor
  routing works. Not fatal — clients are usable.
- **Colormap mirroring** (audit #5). *Safe on default TrueColor; broken
  for legacy/private-colormap paths* — `xterm -cm`, 8-bit / PseudoColor
  clients, image viewers doing palette animation. Defer until a real
  client demands it.
- Full visual table mirroring (audit #6) — but *not* the
  visual/colormap rule on CreateWindow, which is now in scope above.
- InputOnly window host backing (revisit if real clients break)
