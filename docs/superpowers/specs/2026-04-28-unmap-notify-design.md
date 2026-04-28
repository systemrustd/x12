# UnmapNotify Design

**Goal:** emit `UnmapNotify` (event 18) on every mapped → unmapped transition,
both explicit (opcode 10 `UnmapWindow`) and implicit (a mapped window destroyed
via opcode 4 `DestroyWindow` or via client disconnect cleanup).

**Non-goals:**

- BadWindow validation on opcode 10 (pre-existing silent behavior; out of scope).
- `from_configure = true` semantics — we don't yet track parent-resize-driven
  unmaps. Encoder takes the parameter for wire correctness; callers always
  pass `false`.
- Opcode 11 `UnmapSubwindows` — performs `UnmapWindow` on all mapped children
  bottom-to-top per X11 spec. Currently a stub; out of scope here. Once this
  plan lands, opcode 11 becomes a thin loop over the child list reusing this
  plan's `unmap_window` helper. Tracked separately.
- ReparentNotify, ClientMessage, SendEvent — separate plans.

**Spec reference:** X11 protocol spec §11 (Events), event 18 (UnmapNotify).
Recipient semantics match the existing `DestroyNotify` work
([`2026-04-27-property-storage-design.md`](2026-04-27-property-storage-design.md)).

---

## Architecture

`UnmapNotify` follows the same delivery pattern as `DestroyNotify`:

| Recipient mask                          | `event_window` field |
|-----------------------------------------|----------------------|
| `StructureNotify` on the window itself  | the window           |
| `SubstructureNotify` on the parent      | the parent           |

For the destroy path we already snapshot both subscriber sets per destroyed
window (Stage 5 of property storage). Reuse those snapshots to fan out
`UnmapNotify` immediately before `DestroyNotify`. For the explicit
`UnmapWindow` path, snapshot subscribers once after the state transition.

State-change detection is the only new requirement.
`ResourceTable::unmap_window` currently sets `map_state = Unmapped`
unconditionally. Change it to return `bool` indicating whether the call
caused a mapped → `Unmapped` transition (where "mapped" means either
`Viewable` or `Unviewable`). Per X11 spec, `UnmapNotify` fires only on
actual transitions.

The root window is never unmappable per X11 spec. Encode that as a
no-op-and-return-`false` directly inside `unmap_window` so every caller
(opcode 10, future opcode 11, internal paths) gets root protection
without each having to re-check.

Per-X11 ordering: when both `UnmapNotify` and `DestroyNotify` fire for the
same window, the spec leaves order undefined. We emit `UnmapNotify` first
(matches Xorg's behavior).

---

## Components

### `crates/yserver-protocol/src/x11/mod.rs`

Add one encoder near the existing `encode_destroy_notify_event`:

```rust
pub fn encode_unmap_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
    from_configure: bool,
) {
    out.push(18); // UnmapNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    out.push(u8::from(from_configure));
    out.extend_from_slice(&[0; 19]);
}
```

Total 32 bytes (1 + 1 + 2 + 4 + 4 + 1 + 19).

### `crates/yserver-core/src/resources.rs`

Change the existing helper:

```rust
pub fn unmap_window(&mut self, id: ResourceId) -> bool {
    if id == ROOT_WINDOW {
        return false;
    }
    let Some(window) = self.windows.get_mut(&id.0) else { return false; };
    let was_mapped = window.map_state != MapState::Unmapped;
    window.map_state = MapState::Unmapped;
    was_mapped
}
```

Return value semantics:

- `true` — window existed and transitioned from `Viewable` or `Unviewable`
  to `Unmapped`.
- `false` — window did not exist, was already `Unmapped`, or is the root
  window. The root case is silently no-op'd here (no state change, no
  emission) since the root is never unmappable per X11 spec.

`unmap_window` is `pub` (used by `nested.rs`); signature change is
source-incompatible for any future internal caller, so update all call
sites in this plan.

### `crates/yserver-core/src/nested.rs`

**Opcode 10 (`UnmapWindow`).** Replace the current handler body with the
snapshot-then-fanout shape used by Stage 4/5 events:

```rust
10 => {
    if let Some(window) = x11::map_window_id(body) {
        let snapshot = {
            let mut s = lock_server(server)?;
            let was_mapped = s.resources.unmap_window(window);
            if was_mapped {
                let parent = s.resources
                    .window(window)
                    .map(|w| w.parent)
                    .unwrap_or(ROOT_WINDOW);
                let on_window = s.subscribers(window, 0x0002_0000); // StructureNotify
                let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
                Some((parent, on_window, on_parent))
            } else {
                None
            }
        };
        if let Some((parent, on_window, on_parent)) = snapshot {
            for target in on_window {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_unmap_notify_event(&mut buf, seq, target.byte_order, window, window, false);
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
            for target in on_parent {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_unmap_notify_event(&mut buf, seq, target.byte_order, parent, window, false);
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
        }
    }
    log_void(client_id, sequence, "UnmapWindow")
}
```

**Opcode 4 (`DestroyWindow`).** Extend the existing `pending` tuple to capture
`was_mapped` per window. Inside the locked block, change:

```rust
let parent = s.resources.window(*w).map_or(ROOT_WINDOW, |win| win.parent);
let on_window = s.subscribers(*w, 0x0002_0000);
let on_parent = s.subscribers(parent, 0x0008_0000);
pending.push((*w, parent, on_window, on_parent));
```

to:

```rust
let (parent, was_mapped) = s.resources
    .window(*w)
    .map_or((ROOT_WINDOW, false), |win| {
        (win.parent, win.map_state != MapState::Unmapped)
    });
let on_window = s.subscribers(*w, 0x0002_0000);
let on_parent = s.subscribers(parent, 0x0008_0000);
pending.push((*w, parent, was_mapped, on_window, on_parent));
```

In the post-lock fanout, emit `UnmapNotify` first (when `was_mapped`), then
`DestroyNotify`. Same `on_window` / `on_parent` sets for both events.

**Disconnect cleanup.** Identical extension to the `pending_destroys`
accumulator in `handle_client`.

---

## Data flow

### Explicit unmap (opcode 10)

```
client → handle_request opcode 10
  ↓ lock_server
  was_mapped = unmap_window(target)
  if was_mapped:
    parent = window(target).parent
    on_window = subscribers(target, StructureNotify)
    on_parent = subscribers(parent, SubstructureNotify)
  ↓ drop server lock
  if was_mapped:
    fanout UnmapNotify(event_window=target, window=target, false) → on_window
    fanout UnmapNotify(event_window=parent, window=target, false) → on_parent
```

### Implicit unmap (opcode 4 and disconnect)

```
collect_destroy_order(root) → order   // post-order: children before parent
for w in order:
  was_mapped = window(w).map_state != Unmapped
  parent     = window(w).parent
  on_window  = subscribers(w, StructureNotify)
  on_parent  = subscribers(parent, SubstructureNotify)
  pending.push((w, parent, was_mapped, on_window, on_parent))
destroy_window(root)
drop_window_subscriptions(&order)
↓ drop server lock
for (w, parent, was_mapped, on_w, on_p) in pending:
  if was_mapped:
    fanout UnmapNotify(event_window=w,      window=w, false) → on_w
    fanout UnmapNotify(event_window=parent, window=w, false) → on_p
  fanout DestroyNotify(event_window=w,      window=w) → on_w
  fanout DestroyNotify(event_window=parent, window=w) → on_p
```

`UnmapNotify` always precedes `DestroyNotify` per window. Across the subtree,
post-order traversal preserves child-before-parent ordering for both event
types.

---

## Error handling

| Condition                                  | Behavior                          |
|--------------------------------------------|-----------------------------------|
| `UnmapWindow` on unknown window            | `unmap_window` returns `false`; no event. Pre-existing silent behavior. |
| `UnmapWindow` on already-unmapped window   | Returns `false`; no event (per X11 spec). |
| `UnmapWindow` on the root                  | `unmap_window` no-ops and returns `false`; root `map_state` is unchanged; no event emitted. Root protection lives in `unmap_window` itself, not the handler. |
| Window destroyed while unmapped            | No `UnmapNotify`; `DestroyNotify` only. |
| Subscriber writer poisoned                 | Skipped via `if let Ok(mut w) = target.writer.lock()`. Same as existing fanouts. |

---

## Testing

### Unit tests

`crates/yserver-core/src/resources.rs` (new `#[cfg(test)] mod tests`):

1. `unmap_window_returns_true_on_transition_from_viewable` — create + map a
   window (Viewable), call `unmap_window`, assert returns `true`, assert
   `map_state == Unmapped`.
2. `unmap_window_returns_true_on_transition_from_unviewable` — set
   `map_state = Unviewable` directly, call `unmap_window`, assert returns
   `true`, assert `map_state == Unmapped`.
3. `unmap_window_returns_false_when_already_unmapped` — call `unmap_window`
   twice, assert second returns `false`.
4. `unmap_window_returns_false_for_unknown_window` — call on a never-created
   id, assert `false`.
5. `unmap_window_no_ops_on_root` — call `unmap_window(ROOT_WINDOW)` after
   the table is initialized (root is Viewable by default), assert returns
   `false`, assert root `map_state` is still `Viewable`.

`crates/yserver-protocol/src/x11/mod.rs::tests` (new submodule
`unmap_notify_tests`):

6. `shape` — encode with known values, assert byte 0 = 18, sequence at 2..4,
   `event_window` at 4..8, `window` at 8..12, `from_configure` at 12, total
   length 32.

`crates/yserver-core/src/server.rs::tests` (new — colocated with
`subscribers` tests, which already use `UnixStream::pair()`):

7. `unmap_notify_fanout_reaches_only_subscribed_clients` — given two
   clients with a `UnixStream::pair()` writer each, where client A has
   `StructureNotify` (mask 0x0002_0000) on window 0x100 and client B has
   only `KeyPress` on the same window, call `subscribers(0x100, 0x0002_0000)`
   and verify exactly one target. Encode an `UnmapNotify` to A's writer
   peer and read 32 bytes back; assert byte 0 == 18. (Verifies that the
   subscriber-snapshot + encode pipeline produces a wire-correct unmap
   event; the actual handler integration is exercised by smoke tests.)

### Property tests

`crates/yserver-core/src/resources.rs::tests`:

8. `unmap_window_state_machine` (proptest) — for any `initial_state ∈
   {Viewable, Unviewable, Unmapped}` and `n ∈ 1..=5` calls on a window
   seeded with that state, assert:
   - First call returns `true` iff `initial_state != Unmapped`.
   - All subsequent calls return `false`.
   - Final `map_state == Unmapped`.

`crates/yserver-protocol/src/x11/mod.rs::tests::unmap_notify_tests`:

9. `encoder_round_trip` (proptest) — for arbitrary
   `(sequence, event_window, window, from_configure, order ∈ {LE, BE})`:
   - Buffer length is 32.
   - Bytes 0=18, 1=0.
   - Bytes 2..4 = `sequence` in `order`.
   - Bytes 4..8 = `event_window.0` in `order`.
   - Bytes 8..12 = `window.0` in `order`.
   - Byte 12 = `u8::from(from_configure)`.
   - Bytes 13..32 are all zero (catches padding bugs).

### Test plan limitations

The handler-level integration (opcode 10 → fanout → subscribed clients
receive bytes) is not covered by an automated test in this plan. Test 7
verifies the subscriber-snapshot + encoder primitives that the handler
composes. End-to-end correctness is verified by the existing `xev`-based
smoke test described in the property-storage spec (`xev` on a window,
`xdotool windowunmap`, observe `UnmapNotify` print). Adding a true
handler-level integration test would require mocking the request loop
and is deferred.

### Expected counts

| Crate              | Before | After |
|--------------------|--------|-------|
| `yserver-core`     | 42     | 47    |
| `yserver-protocol` | 7      | 9     |
| **Total**          | **49** | **56** |

(Five unit + one proptest = 6 added in core; one unit + one proptest = 2
added in protocol. Total 7 new tests, but one of the core tests
(`unmap_notify_fanout_reaches_only_subscribed_clients`) lands in
`server.rs::tests`, not `resources.rs::tests`.)

---

## Implementation staging

Single small plan, three commits:

1. **Add `encode_unmap_notify_event` + tests** in `yserver-protocol`. Pure
   addition; no callers.
2. **Change `unmap_window` to return `bool` + add root no-op** in
   `yserver-core/resources.rs`, with the new unit + proptest coverage.
   The single existing caller (opcode 10 handler in `nested.rs`) needs
   to be updated to compile; for this commit it just `let _ =
   s.resources.unmap_window(window);` to discard the new return value.
   The destroy path doesn't call `unmap_window` (it uses
   `destroy_window` which removes windows directly), so it isn't
   affected at this commit.
3. **Wire opcode 10, opcode 4, and disconnect cleanup** in `nested.rs` to
   emit `UnmapNotify`. Updates the existing destroy `pending` tuple shape
   to carry `was_mapped` (read from `Window.map_state` under the lock,
   alongside the existing `parent` snapshot). Adds the
   `server::tests::unmap_notify_fanout_reaches_only_subscribed_clients`
   integration-style test alongside the existing `subscribers` tests.

Each commit compiles, passes its tests, and ends with `cargo fmt`,
`cargo clippy -- -W clippy::pedantic`, `cargo test`.
