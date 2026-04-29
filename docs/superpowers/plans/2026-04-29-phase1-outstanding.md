# Phase 1 Outstanding Items Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the remaining Phase 1 protocol gaps and near-term follow-ups after `xeyes`, `xclock`, `xterm`, and `xev` already run: lifecycle/WM events, synthetic `ClientMessage` delivery, subtree unmap, child drawable translation, and xterm scrollback.

**Architecture:** Keep the existing nested-server model: protocol parsers/encoders in `yserver-protocol`, resource/window-tree mutation in `ResourceTable`, cross-client event snapshots through `ServerState::subscribers`, host forwarding in `HostX11`, and opcode wiring in `nested.rs`. Preserve the lock rule: mutate/snapshot under `ServerState`, drop the lock, then write to clients or call the host X server.

**Spec:** [`docs/superpowers/specs/2026-04-29-phase1-outstanding-design.md`](../specs/2026-04-29-phase1-outstanding-design.md).

**Project conventions:**

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
```

Run the full set before marking the plan complete. Manual smoke testing is required because several outcomes are visible only through real clients and host X behavior.

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Add `ReparentWindow` / `SendEvent` parsers and `ReparentNotify` / `ClientMessage` encoders with tests |
| `crates/yserver-core/src/resources.rs` | modify | Add window reparenting, descendant checks, mapped-children snapshots, owner lookup helpers, and child-offset tests |
| `crates/yserver-core/src/server.rs` | modify | Add event-target helpers for mask intersection, owner/direct event routing, and tests |
| `crates/yserver-core/src/host_x11.rs` | modify | Add host `ReparentWindow` if mirroring top-level host windows is needed; add optional debug logging hooks if not already sufficient |
| `crates/yserver-core/src/nested.rs` | modify | Wire opcodes 7, 11, 25; extract `PendingDestroy`; translate non-zero child drawable coordinates; add xterm scrollback instrumentation |
| `docs/status.md` | modify | Mark lifecycle/WM events complete only after smoke tests pass; move remaining deferred items to follow-ups |

The implementation is seven compile-safe commits:

1. **Destroy-path cleanup** — extract `PendingDestroy` and shared fanout helper.
2. **Protocol parsers/encoders** — add pure wire types for `ReparentWindow`, `ReparentNotify`, `SendEvent`, and `ClientMessage`.
3. **Resource tree operations** — add reparenting and mapped-child helpers.
4. **Opcode 11 + opcode 7** — implement `UnmapSubwindows`, then `ReparentWindow` / `ReparentNotify`.
5. **ClientMessage + SendEvent** — implement synthetic `ClientMessage` delivery.
6. **Child drawable translation + scrollback instrumentation/fixes** — route non-zero child drawing and investigate xterm scrollback.
7. **Tests + status update** — add focused tests, run full checks, update docs.

---

## Commit 1 — Destroy-Path Cleanup

### Task 1.1: Extract `PendingDestroy`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Add a local struct near `collect_destroy_order` or the other fanout helpers**

```rust
struct PendingDestroy {
    window: ResourceId,
    parent: ResourceId,
    was_mapped: bool,
    host_xid: Option<u32>,
    on_window: Vec<crate::server::EventTarget>,
    on_parent: Vec<crate::server::EventTarget>,
}
```

Include `host_xid` because the current disconnect and opcode 4 paths destroy
host subwindows before protocol fanout.

- [ ] **Step 2: Replace both tuple vectors**

Replace the `#[allow(clippy::type_complexity)] Vec<(...)>` uses in:

- explicit `DestroyWindow` handling.
- client-disconnect cleanup in `handle_client`.

Build each `PendingDestroy` with the same fields currently pushed into the tuple.

- [ ] **Step 3: Add shared emit helper**

Add a helper that emits protocol events only:

```rust
fn fanout_destroy_sequence(pending: &PendingDestroy) {
    if pending.was_mapped {
        fanout_event(&pending.on_window, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, pending.window, pending.window, false);
        });
        fanout_event(&pending.on_parent, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, pending.parent, pending.window, false);
        });
    }
    fanout_event(&pending.on_window, |buf, seq, order| {
        x11::encode_destroy_notify_event(buf, seq, order, pending.window, pending.window);
    });
    fanout_event(&pending.on_parent, |buf, seq, order| {
        x11::encode_destroy_notify_event(buf, seq, order, pending.parent, pending.window);
    });
}
```

Keep host cleanup in the call sites for now, because it needs `host` and `input_handle`.

- [ ] **Step 4: Verify no behavior change**

Run:

```sh
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test -p yserver-core unmap
```

Expected: no behavior change; existing destroy/unmap tests pass.

---

## Commit 2 — Protocol Parsers and Encoders

### Task 2.1: Add `ReparentWindow` parser

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add request type**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReparentWindowRequest {
    pub window: ResourceId,
    pub parent: ResourceId,
    pub x: i16,
    pub y: i16,
}
```

- [ ] **Step 2: Add parser**

```rust
pub fn reparent_window_request(body: &[u8]) -> Option<ReparentWindowRequest> {
    Some(ReparentWindowRequest {
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        parent: ResourceId(read_u32_le(body.get(4..8)?)),
        x: read_i16_le(body.get(8..10)?),
        y: read_i16_le(body.get(10..12)?),
    })
}
```

- [ ] **Step 3: Add parser tests**

Cover full 12-byte parsing and short-body `None`.

### Task 2.2: Add `ReparentNotify` encoder

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add encoder near other notify encoders**

```rust
pub fn encode_reparent_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
    parent: ResourceId,
    x: i16,
    y: i16,
    override_redirect: bool,
) {
    out.push(21);
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    write_u32(order, out, parent.0);
    write_i16(order, out, x);
    write_i16(order, out, y);
    out.push(u8::from(override_redirect));
    out.extend_from_slice(&[0; 11]);
}
```

Expected length: 32 bytes.

- [ ] **Step 2: Add encoder test**

Assert event code, sequence, event/window/parent ids, coordinates, override byte,
and total length.

### Task 2.3: Add `SendEvent` parser and `ClientMessage` helpers

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add request type**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendEventRequest<'a> {
    pub propagate: bool,
    pub destination: ResourceId,
    pub event_mask: u32,
    pub event: &'a [u8; 32],
}
```

- [ ] **Step 2: Add parser**

```rust
pub fn send_event_request(propagate: u8, body: &[u8]) -> Option<SendEventRequest<'_>> {
    let event: &[u8; 32] = body.get(8..40)?.try_into().ok()?;
    Some(SendEventRequest {
        propagate: propagate != 0,
        destination: ResourceId(read_u32_le(body.get(0..4)?)),
        event_mask: read_u32_le(body.get(4..8)?),
        event,
    })
}
```

- [ ] **Step 3: Add `ClientMessageEvent` and encoder**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientMessageEvent {
    pub sequence: SequenceNumber,
    pub send_event: bool,
    pub format: u8,
    pub window: ResourceId,
    pub r#type: AtomId,
    pub data: [u8; 20],
}

pub fn encode_client_message_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    event: ClientMessageEvent,
) {
    out.push(33 | if event.send_event { 0x80 } else { 0 });
    out.push(event.format);
    write_u16(order, out, event.sequence.0);
    write_u32(order, out, event.window.0);
    write_u32(order, out, event.r#type.0);
    out.extend_from_slice(&event.data);
}
```

- [ ] **Step 4: Add tests**

Cover:

- parser reads `propagate`, destination, mask, and 32-byte payload.
- parser rejects short bodies.
- encoder sets byte 0 to `33` for normal events and `33 | 0x80` for synthetic.
- encoder total length is 32 bytes.

- [ ] **Step 5: Verify commit**

Run:

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo test -p yserver-protocol reparent
RUSTC_WRAPPER= cargo test -p yserver-protocol send_event
RUSTC_WRAPPER= cargo test -p yserver-protocol client_message
```

---

## Commit 3 — Resource Tree Operations

### Task 3.1: Add descendant and owner helpers

**Files:**
- Modify: `crates/yserver-core/src/resources.rs`

- [ ] **Step 1: Add `is_descendant_of`**

```rust
pub fn is_descendant_of(&self, candidate: ResourceId, ancestor: ResourceId) -> bool;
```

Walk parent links from `candidate` toward root. Return `true` if `ancestor` is
encountered. Defend against missing parents and self-loops by returning `false`
once traversal cannot advance.

- [ ] **Step 2: Add `window_owner`**

```rust
pub fn window_owner(&self, id: ResourceId) -> Option<ClientId> {
    self.windows.get(&id.0).map(|w| w.owner)
}
```

This is needed for direct `ClientMessage` delivery when `SendEvent.event_mask == 0`.

- [ ] **Step 3: Add tests**

Cover direct child, grandchild, unrelated window, unknown window, and root cases.

### Task 3.2: Add mapped-child snapshot helper

**Files:**
- Modify: `crates/yserver-core/src/resources.rs`

- [ ] **Step 1: Add helper**

```rust
pub fn mapped_children_bottom_to_top(&self, parent: ResourceId) -> Option<Vec<ResourceId>>;
```

Return `None` when `parent` is not a known window. Return only children whose
`map_state != MapState::Unmapped`.

Use current child vector ordering consistently. If appended children are topmost,
bottom-to-top is the existing vector order.

- [ ] **Step 2: Add tests**

Cover unknown parent, no children, mixed mapped/unmapped children, and ordering.

### Task 3.3: Add reparent operation

**Files:**
- Modify: `crates/yserver-core/src/resources.rs`

- [ ] **Step 1: Add result type**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReparentResult {
    pub window: ResourceId,
    pub old_parent: ResourceId,
    pub new_parent: ResourceId,
    pub x: i16,
    pub y: i16,
    pub override_redirect: bool,
    pub host_xid: Option<u32>,
}
```

- [ ] **Step 2: Add mutation method**

```rust
pub fn reparent_window(
    &mut self,
    request: ReparentWindowRequest,
) -> Result<ReparentResult, ReparentWindowError>;
```

Add a local/core error enum:

```rust
pub enum ReparentWindowError {
    BadWindow,
    BadMatch,
}
```

Rules:

- `window` and `parent` must exist.
- `window != ROOT_WINDOW`.
- `window != parent`.
- `parent` must not be a descendant of `window`.
- Remove `window` from old parent's children.
- Append `window` to new parent's children.
- Update `parent`, `x`, `y`.
- Return old/new parent, coordinates, `override_redirect`, and `host_xid`.

- [ ] **Step 3: Add tests**

Cover success, old-parent child removal, new-parent append, coordinate update,
root rejection, self-parent rejection, descendant-parent rejection, unknown
window, and unknown parent.

- [ ] **Step 4: Verify commit**

Run:

```sh
RUSTC_WRAPPER= cargo test -p yserver-core reparent
RUSTC_WRAPPER= cargo test -p yserver-core mapped_children
RUSTC_WRAPPER= cargo check --workspace
```

---

## Commit 4 — UnmapSubwindows and ReparentWindow Wiring

### Task 4.1: Implement opcode 11 `UnmapSubwindows`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Replace the opcode 11 stub if present, or insert handler between opcodes 10 and 12**

Behavior:

- Parse with `x11::map_window_id(body)`.
- Under the server lock, validate parent exists.
- Snapshot mapped children via `mapped_children_bottom_to_top`.
- For each child, call `s.resources.unmap_window(child)`.
- For changed children, snapshot `StructureNotify` subscribers on the child and
  `SubstructureNotify` subscribers on the parent.
- Drop the server lock.
- Unmap host subwindows for children with `host_xid`.
- Fan out `UnmapNotify` to child and parent subscribers.

- [ ] **Step 2: Error behavior**

If the parent is unknown, emit `BadWindow` for opcode 11. Use the existing
`emit_x11_error` style already used in `nested.rs`.

- [ ] **Step 3: Smoke with existing clients**

Run ynest and verify `xeyes`, `xclock`, and `xterm` still come up.

### Task 4.2: Implement opcode 7 `ReparentWindow`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Optionally modify: `crates/yserver-core/src/host_x11.rs`

- [ ] **Step 1: Wire request parsing and validation**

Replace:

```rust
7 => log_void(client_id, sequence, "ReparentWindow"),
```

with a real handler that calls `x11::reparent_window_request(body)`.

- [ ] **Step 2: Mutate and snapshot under lock**

Call `s.resources.reparent_window(request)` and map errors:

- `BadWindow` -> `x11::error::BAD_WINDOW`
- `BadMatch` -> `x11::error::BAD_MATCH`

On success, snapshot:

- `StructureNotify` subscribers on `window`.
- `SubstructureNotify` subscribers on `old_parent`.
- `SubstructureNotify` subscribers on `new_parent`.

- [ ] **Step 3: Avoid duplicate recipient groups**

If `old_parent == new_parent`, emit only one parent-group event. Do not try to
deduplicate across different event-window values unless it becomes necessary;
the same client may legitimately receive multiple events with different
`event` fields.

- [ ] **Step 4: Host mirroring policy**

For Phase 1:

- If `host_xid` exists and `new_parent == ROOT_WINDOW`, keep the existing host
  subwindow under the ynest container and call `configure_subwindow` with the
  new x/y if needed.
- If `new_parent != ROOT_WINDOW`, do not attempt host reparenting yet. Protocol
  state remains correct; child drawable translation handles drawing.

Add `HostX11::reparent_window` only if manual testing shows a real need.

- [ ] **Step 5: Fan out `ReparentNotify`**

Emit:

- event window `window` to `StructureNotify` subscribers on the child.
- event window `old_parent` to `SubstructureNotify` subscribers on old parent.
- event window `new_parent` to `SubstructureNotify` subscribers on new parent.

Use `x11::encode_reparent_notify_event`.

- [ ] **Step 6: Verify commit**

Run:

```sh
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test -p yserver-core reparent
RUSTC_WRAPPER= cargo test -p yserver-protocol reparent
```

---

## Commit 5 — ClientMessage and SendEvent

### Task 5.1: Add event-target routing helpers

**Files:**
- Modify: `crates/yserver-core/src/server.rs`

- [ ] **Step 1: Add mask-intersection subscribers**

```rust
pub fn subscribers_intersecting(&self, window: ResourceId, event_mask: u32) -> Vec<EventTarget>;
```

Return clients where `(selected_mask & event_mask) != 0`.

- [ ] **Step 2: Add owner target helper**

```rust
pub fn client_target(&self, client_id: ClientId) -> Option<EventTarget>;
```

Use this for direct owner delivery.

- [ ] **Step 3: Add tests**

Cover intersection matching, no-match, and client-target lookup.

### Task 5.2: Implement opcode 25 `SendEvent` for `ClientMessage`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Parse request**

Use:

```rust
let Some(req) = x11::send_event_request(header.data, body) else { ... };
```

Validate body length through the parser. Malformed request can remain a logged
void/no-op if that is current malformed-request policy, or emit `BadLength` if
the surrounding handler supports it consistently.

- [ ] **Step 2: Validate supported event type**

Support only `ClientMessage` initially:

```rust
let event_type = req.event[0] & 0x7f;
if event_type != 33 { BadValue }
```

Validate:

- `format` byte is `8`, `16`, or `32`.
- destination window exists.
- `ClientMessage.type` atom exists in the atom table.

- [ ] **Step 3: Build synthetic event bytes**

Copy the 32-byte event body, set `event[0] |= 0x80`, and preserve the rest of
the sender-provided bytes.

Do not re-encode unless needed. Re-encoding risks changing the sender-provided
sequence field or data layout. The protocol encoder tests still exist for
server-originated `ClientMessage` events later.

- [ ] **Step 4: Choose recipients**

If `event_mask == 0`:

- Deliver to the owner of `destination` using `ResourceTable::window_owner` and
  `ServerState::client_target`.

If `event_mask != 0`:

- Deliver to `subscribers_intersecting(destination, event_mask)`.

If `propagate == true` and recipient list is empty:

- Walk parent windows toward root and try `subscribers_intersecting` on each
  ancestor.
- Stop at the first ancestor with recipients.

Do not model do-not-propagate masks in Phase 1.

- [ ] **Step 5: Fan out raw event**

Add or use a helper:

```rust
fn fanout_raw_event(targets: &[EventTarget], event: &[u8; 32]);
```

This is separate from `fanout_event` because the bytes are sender-provided, not
encoded per target. Phase 1 supports little-endian clients only, so no
byte-swapping is needed.

- [ ] **Step 6: Verify commit**

Run:

```sh
RUSTC_WRAPPER= cargo test -p yserver-core subscribers
RUSTC_WRAPPER= cargo test -p yserver-protocol send_event
RUSTC_WRAPPER= cargo check --workspace
```

Manual smoke:

```sh
DISPLAY=:<ynest-display> xev
DISPLAY=:<ynest-display> xterm
```

Look for regressions in key/pointer/property event delivery.

---

## Commit 6 — Child Drawable Translation and xterm Scrollback

### Task 6.1: Add coordinate translation helpers

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Add small helpers near existing drawable routing helpers**

```rust
fn add_offset_i16(value: i16, offset: i16) -> i16 {
    value.wrapping_add(offset)
}
```

If host X rejects out-of-range values in practice, replace wrapping with
saturating or `checked_add` plus request drop. Start with current Rust-safe
wrapping behavior to match existing offset accumulation.

- [ ] **Step 2: Add `HostDrawableTarget` convenience use**

Use `HostDrawableTarget::Window { x_offset, y_offset, .. }` to translate
window targets and zero offsets for pixmaps. If helper methods are clearer,
add them in `resources.rs`:

```rust
pub fn host_xid(self) -> u32;
pub fn x_offset(self) -> i16;
pub fn y_offset(self) -> i16;
```

### Task 6.2: Translate existing host-routed drawing

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Remove zero-offset-only drops where safe**

Find handlers that currently require:

```rust
target.x_offset == 0 && target.y_offset == 0
```

Replace with translated coordinates for:

- `PolyLine`
- `PolyArc`
- `PolyFillArc`
- `PolyFillRectangle`
- `ClearArea`
- `ImageText8`
- `PolyText8`
- `CopyArea`
- `PutImage`

- [ ] **Step 2: Handle `CopyArea` carefully**

Translate source and destination independently:

```rust
src_x + src_x_offset
src_y + src_y_offset
dst_x + dst_x_offset
dst_y + dst_y_offset
```

Window-to-pixmap and pixmap-to-window paths should use zero offset on pixmap
targets.

- [ ] **Step 3: Keep conservative clipping**

If a request rectangle is clearly outside the child bounds and no clipping
exists yet, keep dropping that request rather than drawing outside the child.
Do not build a full clip-region system in this commit.

- [ ] **Step 4: Verify simple clients**

Run:

```sh
RUSTC_WRAPPER= cargo check --workspace
DISPLAY=:<ynest-display> xeyes
DISPLAY=:<ynest-display> xclock
DISPLAY=:<ynest-display> xterm
```

### Task 6.3: Instrument and fix xterm scrollback

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify if needed: `crates/yserver-core/src/host_x11.rs`
- Modify if needed: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add temporary debug logging behind existing logging macros**

Log only at `debug!` level:

- `CreatePixmap`: pixmap id, depth, size, source drawable.
- `CopyArea`: source/destination id, target kind, offsets, size, forwarded vs dropped.
- `PutImage`: format, depth, target kind, forwarded vs dropped.

Do not use `println!`.

- [ ] **Step 2: Reproduce**

Manual:

```sh
DISPLAY=:<ynest-display> xterm
seq 1 200
```

Then test shift-pageup and scrollbar scrollback.

- [ ] **Step 3: Classify the blocker**

Based on logs, fix the smallest real issue:

- If copies are dropped due to child offsets, the translation work above should
  resolve it.
- If pixmap-to-window copies fail, inspect host pixmap allocation and
  `HostDrawableTarget::Pixmap` routing.
- If xterm sends unsupported `PutImage` `XYBitmap` or `XYPixmap`, document the
  observed request and implement the minimum format only if it is required for
  scrollback.
- If GC function/plane-mask/clip state blocks redraw, add only the specific GC
  fields observed.

- [ ] **Step 4: Remove or keep logs appropriately**

Keep low-noise `debug!` logs that help future protocol work. Remove noisy
per-request logs once scrollback is fixed unless they are guarded by a focused
debug flag.

- [ ] **Step 5: Acceptance**

xterm must satisfy:

- Prompt draws.
- Typing echoes.
- `seq 1 200` scrolls forward.
- Shift-pageup redraws older lines.
- Scrollbar drag redraws older lines.
- Returning to bottom redraws current prompt without stale black blocks.

---

## Commit 7 — Tests, Deferred From-Configure, Status

### Task 7.1: Add focused tests

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`
- Modify: `crates/yserver-core/src/resources.rs`
- Modify: `crates/yserver-core/src/server.rs`
- Modify if feasible: handler tests in `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Protocol tests**

Ensure tests cover:

- `ReparentWindow` parser.
- `ReparentNotify` encoder.
- `SendEvent` parser.
- `ClientMessage` encoder.

- [ ] **Step 2: Resource tests**

Ensure tests cover:

- `reparent_window` success and errors.
- `mapped_children_bottom_to_top`.
- child drawable offset accumulation still passes existing proptests.

- [ ] **Step 3: Server routing tests**

Ensure tests cover:

- `subscribers_intersecting`.
- `client_target`.

- [ ] **Step 4: Handler fanout tests if practical**

Add integration-style tests only if the current `UnixStream` writer model can
be driven without a large refactor. Minimum valuable cases:

- `UnmapSubwindows` event order.
- `SendEvent` sets high bit and delivers one raw 32-byte event.
- `ReparentWindow` emits child, old-parent, and new-parent notifications.

If this is too invasive, leave a precise TODO in the plan/status rather than
blocking implementation.

### Task 7.2: Keep `from_configure = true` deferred

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Do not wire fake configure unmaps**

Do not emit `UnmapNotify(from_configure = true)` until viewability is recomputed
from mapped intent, ancestors, and geometry. The design explicitly defers this.

- [ ] **Step 2: Track it as Phase 2 or later model work**

Move it from immediate Phase 1 follow-up to a deferred geometry/viewability
follow-up unless the implementation added that model.

### Task 7.3: Final verification

- [ ] **Step 1: Run full automated checks**

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
```

- [ ] **Step 2: Run manual smoke**

```sh
DISPLAY=:<ynest-display> xeyes
DISPLAY=:<ynest-display> xclock
DISPLAY=:<ynest-display> xev
DISPLAY=:<ynest-display> xterm
```

Inside xterm:

```sh
seq 1 200
```

Test typing, scrollback, scrollbar drag, and return-to-bottom redraw.

- [ ] **Step 3: Update `docs/status.md`**

Mark lifecycle/WM events complete only when:

- `ReparentWindow` / `ReparentNotify` work.
- `ClientMessage` through `SendEvent` works for the supported Phase 1 route.
- `UnmapSubwindows` works.
- Existing smoke clients still run.

Keep these as known follow-ups if not completed:

- Full `from_configure = true` geometry/viewability model.
- Broader synthetic event types beyond `ClientMessage`.
- Full handler-level tests if the test harness refactor was deferred.
- Full clipping/clip regions if only safe translated rectangles are supported.

---

## Manual WM Probe After Completion

This plan does not claim Phase 2 WM support, but it should make the first WM
probe meaningful:

```sh
DISPLAY=:<ynest-display> openbox
```

Expected at this stage:

- It may still fail due to missing `SubstructureRedirect`, configure-request,
  focus, grabs, selections, or EWMH root properties.
- It should no longer fail solely because `ReparentWindow`,
  `ReparentNotify`, `ClientMessage`, or `SendEvent` are absent.
