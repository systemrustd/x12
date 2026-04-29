# Phase 1 Outstanding Items Design

**Goal:** close the remaining Phase 1 protocol gaps and the known
Phase-1-adjacent follow-ups that block useful window-manager testing after
`xeyes`, `xclock`, `xterm`, and `xev` already run.

This is not a design for a full ICCCM/EWMH desktop layer. The goal is to land
the minimum core-protocol behavior needed before Phase 2 starts: lifecycle
events, synthetic event delivery, subtree unmap behavior, drawable coordinate
correctness, and enough scrollback behavior to keep `xterm` usable.

---

## Scope

Implement or design the following outstanding items:

- `ReparentWindow` and `ReparentNotify` for basic WM reparenting.
- `ClientMessage` encoding and delivery, primarily through `SendEvent`.
- `SendEvent` (opcode 25) with the event-byte `SEND_EVENT` flag.
- `UnmapSubwindows` (opcode 11).
- `UnmapNotify.from_configure = true` when configure-induced visibility loss
  is modeled.
- Handler-level fanout tests for `DestroyNotify` and `UnmapNotify`.
- xterm scrollback investigation and the likely pixmap/copy path fixes.
- `PendingDestroy` extraction to replace the current destroy-path tuple.
- Non-zero child-window coordinate translation for host-routed drawing,
  including `CopyArea` and `PutImage`.

Out of scope:

- Full ICCCM/EWMH compliance.
- `SubstructureRedirect` / `ResizeRedirect` request interception.
- Focus policy, grabs, selections, clipboard, DND, root WM properties.
- Extension work such as SHAPE, RENDER, XFIXES, COMPOSITE, DAMAGE, XKB,
  XInput2, MIT-SHM, and BIG-REQUESTS.
- Security policy for cross-client synthetic events. Phase 1 remains
  Xorg-compatible here; restrictions belong in the later security phase.

---

## Current State

- `DestroyNotify`, `UnmapNotify`, `MapNotify`, `ConfigureNotify`,
  `PropertyNotify`, keyboard events, pointer events, and crossing events are
  already encoded and delivered through per-client event-mask fanout.
- `ReparentWindow` (opcode 7), `UnmapSubwindows` (opcode 11), and `SendEvent`
  (opcode 25) are still stubs.
- `ClientMessage` has no encoder yet.
- `UnmapNotify` can encode `from_configure`, but all current callers pass
  `false`.
- The destroy path carries a five-element pending tuple through opcode 4 and
  disconnect cleanup.
- Host-routed drawing can resolve top-level host windows and host-backed
  pixmaps. Child-window drawing with non-zero accumulated offsets is still
  dropped in several paths.
- `CopyArea` and `PutImage` support enough `ZPixmap` behavior for xterm to
  draw and scroll forward; scrollback via scrollbar or shift-pageup still
  fails.

---

## Event Masks and Delivery

Use the existing `ServerState::subscribers(window, mask_bit)` fanout model.
The relevant core mask bits are:

| Mask | Value | Use |
| --- | ---: | --- |
| `StructureNotify` | `0x0002_0000` | Events about the selected window itself. |
| `SubstructureNotify` | `0x0008_0000` | Events about children of the selected window. |
| `SubstructureRedirect` | `0x0010_0000` | Phase 2 redirect behavior; not implemented here. |

Phase 1 should deliver notifications but should not implement redirect policy.
That means `ReparentWindow`, `MapWindow`, and `ConfigureWindow` continue to
perform the requested mutation directly for now. Phase 2 will insert
`MapRequest` / `ConfigureRequest` / WM arbitration once redirect ownership is
tracked.

All fanout keeps the existing lock discipline:

```text
lock ServerState -> mutate state and snapshot EventTarget values
drop ServerState lock
write events/errors/replies to client streams
```

Do not hold the global server lock across host-X11 calls or client writer
locks.

---

## ReparentWindow and ReparentNotify

### Protocol Behavior

`ReparentWindow` request body:

```text
u32 window
u32 parent
i16 x
i16 y
```

Required Phase 1 behavior:

- Validate `window` and `parent` as existing windows; otherwise `BadWindow`.
- Reject reparenting the root window with `BadMatch`.
- Reject making a window its own parent or descendant with `BadMatch`.
- Move `window` from its old parent's child list to `parent`'s child list.
- Update `window.parent`, `window.x`, and `window.y`.
- Preserve the window's mapped state. Do not implicitly map or unmap.
- Keep stacking simple: append to the new parent's child list, which places it
  above existing siblings in the current resource model.
- If the reparented window is a top-level host-backed window, keep its host
  subwindow alive and reparent it on the host if the new parent is still the
  root/top-level host container. For Phase 1, if non-root nested reparenting
  cannot be represented on the host, keep protocol state correct and rely on
  later child-coordinate translation for drawing.

### Event Delivery

Add `encode_reparent_notify_event`:

```text
event 21 ReparentNotify
u8  event type = 21
u8  unused
u16 sequence
u32 event
u32 window
u32 parent
i16 x
i16 y
u8  override_redirect
pad to 32 bytes
```

Deliver to:

| Recipient selection | `event` field |
| --- | --- |
| `StructureNotify` on `window` | `window` |
| `SubstructureNotify` on old parent | old parent |
| `SubstructureNotify` on new parent | new parent |

If the old parent and new parent are the same, avoid duplicate delivery to the
same `(client, event-window)` pair.

Event ordering for a direct `ReparentWindow` request:

```text
mutate parent/position
emit ReparentNotify to window subscribers
emit ReparentNotify to old-parent subscribers
emit ReparentNotify to new-parent subscribers
reply void
```

For future WM-managed reparenting, the WM will call this request to wrap client
windows in frame windows. Phase 2 redirect logic can reuse this event encoder
without changing its wire format.

---

## ClientMessage and SendEvent

### ClientMessage Encoder

Add a raw `ClientMessage` encoder for event 33:

```text
u8  event type = 33, or 33 | 0x80 when sent through SendEvent
u8  format        // 8, 16, or 32
u16 sequence
u32 window
u32 type          // atom
u8[20] data
```

The encoder should take:

```rust
pub struct ClientMessageEvent {
    pub sequence: SequenceNumber,
    pub send_event: bool,
    pub format: u8,
    pub window: ResourceId,
    pub r#type: AtomId,
    pub data: [u8; 20],
}
```

Validation:

- `format` must be `8`, `16`, or `32`; otherwise `BadValue`.
- `window` must exist unless it is `PointerWindow`/`InputFocus` in a later
  implementation. Phase 1 can reject non-resource destinations with
  `BadWindow`.
- `type` must be a known atom; otherwise `BadAtom`.

### SendEvent Parser

`SendEvent` request:

```text
u8  propagate      // request data byte
u16 length = 11
u32 destination
u32 event_mask
u8[32] event
```

Parser output:

```rust
pub struct SendEventRequest<'a> {
    pub propagate: bool,
    pub destination: ResourceId,
    pub event_mask: u32,
    pub event: &'a [u8; 32],
}
```

Phase 1 delivery rules:

- Support `ClientMessage` first. Other event types may be accepted later, but
  should initially return `BadValue` to avoid pretending to support incomplete
  event-specific validation.
- Set bit 7 on event byte 0 before delivery (`event[0] |= 0x80`).
- Preserve the event's sequence field as encoded by the sender. X11
  synthetic events carry the sender-provided event body with only the
  `SEND_EVENT` bit forced.
- If `event_mask == 0`, deliver directly to the destination client/window
  owner semantics needed for `ClientMessage`. For Phase 1, use all clients
  selecting any event mask on that window plus the window owner if available.
  This is enough for WM/client `WM_PROTOCOLS` traffic while the owner routing
  model is still minimal.
- If `event_mask != 0`, deliver to subscribers of `destination` whose selected
  mask intersects `event_mask`.
- If `propagate` is set and no recipient is found on the destination, walk up
  parent windows until the root. Stop at the first ancestor with matching
  recipients. Phase 1 may ignore `do-not-propagate-mask` until that field is
  modeled.

This intentionally separates two concerns:

- `ClientMessage` is the event payload used by ICCCM (`WM_DELETE_WINDOW`,
  `WM_PROTOCOLS`, `WM_TAKE_FOCUS`, etc.).
- `SendEvent` is the transport request that marks an event as synthetic and
  routes it.

---

## UnmapSubwindows

`UnmapSubwindows` request body:

```text
u32 window
```

Behavior:

- Validate `window`; otherwise `BadWindow`.
- Snapshot mapped children in bottom-to-top stacking order.
- For each mapped child, call the same `ResourceTable::unmap_window` helper
  used by opcode 10.
- Emit `UnmapNotify` for each child whose state changed.
- Use the same delivery semantics as explicit `UnmapWindow`:

| Recipient selection | `event` field |
| --- | --- |
| `StructureNotify` on child | child |
| `SubstructureNotify` on parent | parent |

Root protection remains inside `ResourceTable::unmap_window`; if a caller ever
tries to unmap root, it is a no-op and emits no event.

Ordering matters. The X11 request says subwindows are processed bottom to top.
Use the resource table's child order consistently and document whether the
front of the vector is bottom or top. If the current model treats append as
top, iterate from front to back for bottom-to-top.

---

## UnmapNotify From Configure

The protocol field `from_configure = true` is used when a window becomes
unviewable because its parent was resized or moved such that the child is no
longer visible.

Phase 1 should not fake this blindly. Implement it only after visibility state
is derived from geometry rather than only from explicit map state.

Required internal model:

```rust
pub enum MapState {
    Unmapped,
    Unviewable,
    Viewable,
}
```

On `ConfigureWindow` of a parent:

1. Recompute effective viewability for descendants.
2. For each descendant that transitions from `Viewable` to `Unviewable`
   because of the configure operation, emit `UnmapNotify` with
   `from_configure = true`.
3. For each descendant that transitions from `Unviewable` to `Viewable`,
   emit `MapNotify` if the core spec requires it for the modeled case. If this
   is uncertain, defer the upward transition rather than inventing behavior.

This item should wait until the window tree has a single helper that recomputes
viewability from:

- The window's own mapped/unmapped intent.
- Ancestor mapped state.
- Parent/child geometry.
- Root visibility.

Until then, all `UnmapNotify` call sites should continue passing `false`.

---

## Non-Zero Child Drawable Translation

Current host drawing is correct for top-level windows and child windows whose
accumulated offset is `(0, 0)`. Phase 1 should extend this so drawing into any
child window routes to the top-level host backing with translated coordinates.

For every host-routed draw request:

```text
host_x = request_x + target.x_offset
host_y = request_y + target.y_offset
```

Apply this to:

- `PolyLine`
- `PolyArc`
- `PolyFillArc`
- `PolyFillRectangle`
- `ClearArea`
- `ImageText8`
- `PolyText8`
- `CopyArea`
- `PutImage`

For `CopyArea`, translate source and destination independently:

```text
host_src_x = src_x + src_target.x_offset
host_src_y = src_y + src_target.y_offset
host_dst_x = dst_x + dst_target.x_offset
host_dst_y = dst_y + dst_target.y_offset
```

Pixmap targets have zero offset. Window targets use accumulated offset to the
host-backed top-level.

Phase 1 may still rely on host subwindow clipping for top-level windows. Child
window clipping should be conservative:

- If translating without clipping would draw outside the child bounds, either
  compute a clipped rectangle for rectangular operations or continue dropping
  that request until clip regions exist.
- `CopyArea` and `PutImage` should at least support the common case where the
  target rectangle lies fully within the child bounds.

---

## xterm Scrollback

Observed state:

- Forward scrolling with `seq 1 200` works through `CopyArea`.
- Scrollback via scrollbar or shift-pageup does not work yet.

Likely causes:

- xterm may allocate a scrollback pixmap and copy from pixmap to window.
- xterm may use a depth-1 pixmap or `XYBitmap` path for some text/cursor
  operations.
- Source or destination coordinates may refer to a child drawable with non-zero
  offset and are currently dropped.
- GC plane-mask/function/clip state may be relevant for scrollback redraw.

Instrumentation before changing behavior:

- Log `CreatePixmap` depth/size/drawable for xterm only when a debug flag is
  enabled.
- Log `CopyArea` source/destination drawable kinds, offsets, dimensions, and
  whether a host call was forwarded or dropped.
- Log unsupported `PutImage` formats and depths.

Acceptance checks:

- `xterm` draws the prompt.
- Typing echoes.
- `seq 1 200` scrolls forward.
- Shift-pageup redraws older lines.
- Scrollbar drag redraws older lines.
- Returning to the bottom redraws the prompt without stale blocks.

Do not add MIT-SHM for this. If xterm requires shared images in some
configuration, that is Phase 4; first exhaust core pixmap/copy/image paths.

---

## Handler-Level Fanout Tests

Existing unit tests cover encoders, resource transitions, and subscriber
snapshots. Add integration-style tests around request handlers once test
helpers can drive `handle_request` with in-memory writers.

Minimum cases:

- `DestroyWindow` on a mapped child emits `UnmapNotify` before
  `DestroyNotify` to `StructureNotify` subscribers on the child.
- `DestroyWindow` emits both events to `SubstructureNotify` subscribers on
  the parent.
- `UnmapWindow` on an already-unmapped window emits no `UnmapNotify`.
- `UnmapSubwindows` emits one `UnmapNotify` per mapped child in bottom-to-top
  order.
- `SendEvent` with `ClientMessage` sets the high bit on event byte 0 and
  delivers exactly one 32-byte event to the selected recipient.
- `ReparentWindow` emits `ReparentNotify` to the child, old parent, and new
  parent subscriber sets without duplicates.

Recommended helper:

```rust
struct TestClient {
    id: ClientId,
    byte_order: ClientByteOrder,
    rx: Vec<u8>,
}
```

If replacing `UnixStream` in `ClientHandle` is too invasive, introduce a small
`ClientWriter` trait for tests and production streams. Do not block Phase 1
behavior on this refactor if manual smoke tests are already proving the path.

---

## PendingDestroy Struct

Replace the current tuple with a named struct when adding the next event to the
destroy path:

```rust
struct PendingDestroy {
    window: ResourceId,
    parent: ResourceId,
    was_mapped: bool,
    on_window: Vec<EventTarget>,
    on_parent: Vec<EventTarget>,
}
```

This struct belongs in `nested.rs` unless destroy fanout is moved to
`server.rs`. It should be used by both explicit `DestroyWindow` and
disconnect cleanup so ordering and event behavior cannot drift.

Optional follow-up:

```rust
fn emit_destroy_sequence(pending: &[PendingDestroy]);
```

That helper should emit `UnmapNotify` first when `was_mapped`, then
`DestroyNotify`, matching current behavior.

---

## Rollout Order

1. Extract `PendingDestroy` and shared destroy fanout helper. This is a safe
   refactor before adding more lifecycle events.
2. Implement `UnmapSubwindows` using existing `unmap_window` and
   `UnmapNotify` fanout.
3. Add `ClientMessage` encoder and `SendEvent` parser/handler for
   `ClientMessage`.
4. Implement `ReparentWindow` state mutation and `ReparentNotify` fanout.
5. Add non-zero child drawable coordinate translation for existing host-routed
   drawing paths.
6. Instrument and fix xterm scrollback based on observed pixmap/copy/image
   behavior.
7. Add handler-level fanout tests where the current test harness permits it.
8. Defer `from_configure = true` until viewability is recomputed from
   geometry rather than patched into individual handlers.

---

## Smoke Tests

Manual:

```sh
cargo run -p ynest
DISPLAY=:<ynest-display> xeyes
DISPLAY=:<ynest-display> xclock
DISPLAY=:<ynest-display> xev
DISPLAY=:<ynest-display> xterm
```

Inside `xterm`:

```sh
seq 1 200
```

Then test:

- Cursor tracking in `xeyes`.
- Keyboard and pointer events in `xev`.
- Typing and colored prompt rendering in `xterm`.
- Shift-pageup and scrollbar scrollback in `xterm`.
- A simple WM once Phase 2 starts; `ReparentNotify` and `ClientMessage` are
  prerequisites, not sufficient by themselves.

Automated:

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
```

---

## Open Questions

- Should Phase 1 direct `SendEvent(event_mask = 0)` delivery target the window
  owner only, all clients with any mask on that window, or a dedicated
  per-window client-owner route? The owner-only route is closest to the usual
  `ClientMessage` case, but WM traffic may need explicit selection behavior.
- Should host reparenting mirror nested reparenting immediately, or should the
  host tree remain top-level-only until the compositor layer owns child
  clipping? Keeping protocol state correct first is lower risk.
- Is xterm scrollback blocked by pixmap copy behavior, image format support, GC
  state, or coordinate translation? Instrumentation should answer this before
  adding broader image support.
