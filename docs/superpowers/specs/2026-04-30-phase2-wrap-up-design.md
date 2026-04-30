# Phase 2 wrap-up — opcodes and extensions

Date: 2026-04-30
Status: draft (autonomous; pending user/codex review)

## Purpose

Phase 2's stated goal in `docs/high-level-design.md` is to support
ICCCM/EWMH desktop semantics and run a simple WM (Openbox, i3, awesome,
fluxbox). Today fvwm3 comes up under `ynest`. The remaining gap is the
set of opcodes and extension behavior that the simpler reparenting WMs
exercise that fvwm3 happens not to.

This spec scopes the residual core opcode work and the Phase 2 RANDR /
RENDER / event follow-ups already noted in `docs/status.md` so we can
declare Phase 2 done with at least Openbox or Fluxbox running. i3 and
awesome require XKB and are deliberately deferred to Phase 3.

## Out of scope

- Extensions named Phase 3+ in `high-level-design.md`: BIG-REQUESTS,
  MIT-SHM, XKB, XFIXES, DAMAGE, COMPOSITE, SYNC, PRESENT, SHAPE,
  XInput2, GLX. `QueryExtension` keeps returning "absent" for all of
  them; clients that hard-require any of these are not Phase 2
  validation targets.
- Big-endian client support.
- Full ICCCM/EWMH text-property semantics. The server provides storage
  and event delivery; interpretation lives in the WM.

## Validation targets

- **Primary:** Openbox runs end to end — manages a top-level (xterm or
  xclock), key shortcuts (Alt+F4 close, Alt+drag move, Alt+right-drag
  resize) work, the root menu opens.
- **Secondary:** Fluxbox runs through its splash and shows its menu.
- Existing fvwm3 + xclock/xterm flow still works (regression gate).

## Scope: opcodes and behavior

Grouped by subsystem. Each item lists the wire-level work plus the
state it must touch in `ResourceTable` / `ServerState`.

### A. Keyboard for WMs

The keyboard forwarder (`spawn_keyboard_forwarder` in `nested.rs`)
currently routes every key event to the focused client. WMs need
*passive key grabs* that pre-empt the focus path: when a grabbed
keycode+modifier combination fires, the event must go to the grab
owner instead.

1. **`GrabKey` (33)** — store
   `(window, keycode, modifiers, owner_event, pointer_mode, keyboard_mode)`
   in a per-server `KeyGrab` table. Modifiers may include `AnyModifier`
   (0x8000) and keycode may be `AnyKey` (0); both wildcards must match
   in the lookup.
2. **`UngrabKey` (34)** — remove matching entries.
3. **`GrabKeyboard` (31)** — replace the stub `GrabSuccess` with real
   active-grab tracking; on success, every key event routes to the grab
   owner until `UngrabKeyboard`.
4. **`UngrabKeyboard` (32)** — clear the active keyboard grab.
5. **Routing in the keyboard forwarder** — before falling through to
   the focus path, look up `(keycode, state)` in the `KeyGrab` table.
   X11 semantics for "grab applies":
   - grab-window is an ancestor of (or equal to) the focused window, or
   - grab-window is a descendant of the focused window *and* contains
     the pointer.

   Phase 2 implements the first case only (ancestor / equal). The
   descendant-containing-pointer path is deferred unless validation
   shows a real WM uses it; reparenting WMs put their grabs on root,
   which the ancestor walk already covers.

   On a matching `KeyPress`, the spec says the passive grab activates
   into a temporary *active* keyboard grab held by the same client on
   the same grab window until the matching `KeyRelease`. We model
   this: when the passive lookup hits, install an
   `ActiveKeyboardGrab { source: PassiveKey { keycode } }` so all
   subsequent key events (including modifier sequences pressed during
   the chord) route to the same owner; the next `KeyRelease` of that
   keycode tears the active grab down. While *any* active keyboard
   grab is held, passive grabs from other clients do not fire.

   Deliver `KeyPress`/`KeyRelease` to the grab owner with `event` set
   to the grab window.
6. **`GetKeyboardMapping` (101)** — replace the hard-coded
   `keysyms.rs` table with a host proxy: `XGetKeyboardMapping` from the
   host server, cache the result, return the requested slice. Falls
   back to the existing table if the host call fails.
7. **`GetModifierMapping` (119)** — proxy to host
   `XGetModifierMapping`, cache, return real reply.
8. **`ChangeKeyboardMapping` (100)** — accept silently and emit a
   `MappingNotify` event (request 0=Modifier, 1=Keyboard, 2=Pointer)
   to all clients. The actual mapping is host-controlled via the
   nested user's session, so this is effectively a refresh signal.

State: new `pub struct KeyGrab` array on `ServerState`; lookup helper
`grab_owner_for_key(focus, keycode, state) -> Option<(ClientId,
ResourceId)>` that walks the focus → ancestor chain.

### B. Window operations for reparenting WMs

Reparenting WMs (Openbox, Fluxbox) call `ChangeSaveSet` on every
managed client window. The semantics: if the WM dies while holding
reparented children, the server reparents those children back to the
root before destroying them.

9. **`ChangeSaveSet` (6)** — store, per client, a
   `HashSet<ResourceId>` of foreign windows the client wants to keep
   alive (`mode=Insert/Delete`).

   Per the X11 spec, on client resource destruction, for each
   save-set window that is *inferior* to a window created by the
   dying client:
   - reparent it to the closest ancestor that was *not* created by
     the dying client (typically root for a single-WM session, but
     not always — preserve this rule);
   - preserve its absolute root-relative position across the
     reparent (compute `(new_x, new_y)` from old absolute coords
     minus new parent's absolute origin);
   - if the save-set window is currently unmapped, map it.

   Honor save-set on `DestroyWindow` of a parent: do not destroy
   save-set children — reparent them per the same rules and then
   destroy the empty parent.
10. **`CirculateWindow` (13)** — the request's `window` argument is
    the *container* whose children are being restacked. If the
    container has a substructure-redirect subscriber, emit
    `CirculateRequest` (event 27) to it with
    `parent = container, window = child` and do not change
    stacking. Otherwise reorder the container's children (Top/Bottom)
    and emit `CirculateNotify` (event 26) to subscribers of
    StructureNotify on the moved child and SubstructureNotify on the
    container.

    Phase-2 stacking is naive: rotate the back child to the front
    (RaiseLowest, dir=0) or front child to the back (LowerHighest,
    dir=1). True obscuring detection is a Phase 4+ compositor
    concern; Openbox/Fluxbox don't depend on it.
11. **`CirculateNotify` / `CirculateRequest` events** — encoders in
    `protocol/x11`; emit hooks in `nested.rs`.
12. **`DestroySubwindows` (5)** — recurse the existing destroy path
    over each child of the target window. Reuse `destroy_window`
    internals.

State: `Client.save_set: HashSet<ResourceId>` on the per-client
state; `ResourceTable::reparent_save_set_to_root(client_id)` helper
called from the disconnect path.

### C. Drawing

13. **`CopyPlane` (63)** — forward to host `XCopyPlane`. Same drawable
    matrix as `CopyArea` (window↔window, pixmap↔window, etc.). Only
    plane=1 is exercised in practice; pass through whatever the client
    sends.

### D. Pointer/grab follow-ups

14. **`ChangeActivePointerGrab` (30)** — update the *active* pointer
    grab's `event_mask` / `cursor` / `time` if there is one. The spec
    is explicit: this request does **not** affect passive button
    grabs. State must therefore live on the active-pointer-grab
    record, not on the `button_grabs` table. No-op when no active
    grab is held.
15. **`GrabButton` sync replay (known follow-up)** — replace the
    deferred-replay TODO. Move the replay path to a small command
    queue (`mpsc::Sender<ReplayCmd>`) consumed by the
    `pointer_event_fanout` thread, which already holds `xid_map`.
    `AllowEvents(ReplayPointer)` enqueues the frozen event; the pump
    thread re-routes it through the normal owner-lookup path.

### E. RANDR follow-ups

16. **Host window resize propagation** — the host watcher thread
    already monitors `Closed`; extend it to surface `ConfigureNotify`
    on the host container window. On size change update
    `RandrState { width, height }` and emit `RRScreenChangeNotify`
    (event 0 of RANDR's `first_event`) to clients that have selected
    `RRScreenChangeNotifyMask` via `RRSelectInput`.
17. **`RRSelectInput` mask storage** — accept the mask, store
    `(client_id, window) -> mask` in `RandrState`. Used by item 16.
18. **`RRGetScreenInfo` (RANDR 1.0)** — fluxbox probes legacy RANDR
    1.0 first. Implement as a stub that returns the single mode
    matching the current screen size.

### F. Cross-cutting bugs and known follow-ups

19. **`DestroyWindow` releases bg-pixmap host XIDs** — call
    `XFreePixmap` on `Window.background_pixmap_host_xid` during
    destroy if set. Listed as a known follow-up in `status.md`.
20. **`SendEvent` propagation** — when `event_mask == 0` and the
    destination subscribers don't carry the event, walk up the
    parent chain emitting to each ancestor whose mask covers the
    event type, until an ancestor has the
    "do-not-propagate" bit for that type. Today the impl delivers to
    direct subscribers only.
21. **`UnmapNotify.from_configure = true`** — on parent
    `ConfigureWindow` that shrinks a child out of view, emit the
    implicit unmap with `from_configure=true`. Encoder already
    accepts the byte.

## Architecture

No new modules. The work touches:

- `crates/yserver-protocol/src/x11/mod.rs` — encoders for
  `MappingNotify`, `CirculateNotify`, `CirculateRequest`,
  `RRScreenChangeNotify`; decoders for `ChangeSaveSet`, `GrabKey`,
  `UngrabKey`, `CirculateWindow`, `CopyPlane`, `ChangeActivePointerGrab`,
  `GetKeyboardMapping` proxy result, `GetModifierMapping` proxy result.
- `crates/yserver-core/src/resources.rs` — `Client.save_set`,
  `KeyGrab` table, `ChangeSaveSet` helpers, save-set restore on
  disconnect, bg-pixmap free on destroy.
- `crates/yserver-core/src/host_x11.rs` — host calls for
  `XGetKeyboardMapping`, `XGetModifierMapping`, `XCopyPlane`, host
  `ConfigureNotify` watcher.
- `crates/yserver-core/src/randr.rs` — `RandrState.subscribers`,
  `RRGetScreenInfo`, screen-change emission.
- `crates/yserver-core/src/nested.rs` — dispatch arms for the new
  opcodes, keyboard-forwarder grab lookup, save-set restore in the
  disconnect path, host-resize hook.

## Data flow

**Key grab path (illustrative, item A.5):**

```
HostInputPump::Key event
  → spawn_keyboard_forwarder thread
    → ServerState::lookup_key_grab(focus, keycode, state)
      ├── matches passive GrabKey       → deliver to grab owner+window
      ├── matches active GrabKeyboard   → deliver to grab owner+grab_window
      └── no match                      → existing focus delivery
```

**Save-set on disconnect (item B.9):**

```
client_disconnect(client_id)
  → for w in client.save_set:
      reparent w to root
      remap w if it was mapped under the dying parent
  → resource cleanup (existing path)
```

**Host resize (item E.16):**

```
Host watcher thread sees ConfigureNotify on container
  → RandrState::set_size(w, h)
  → for (cid, win) in subscribers: emit RRScreenChangeNotify
```

## Error handling

- Key grab table lookup is read-only on the hot key path; lock
  contention is bounded by the existing single `ServerState` mutex.
- Save-set restore tolerates already-destroyed windows (race with
  client disconnect): each restore wraps a `resources.lookup` and
  skips on `None`.
- Host proxy calls (`XGetKeyboardMapping` etc.) fall back to the
  existing stub on error so a flaky host doesn't fail the reply.
- New events use the existing `subscribers()` snapshot pattern; no
  new fanout machinery.

## Testing

Per-item: encoder/decoder unit tests in `protocol/x11` (this is the
established pattern — see `write_unmap_notify` tests, etc.). The
`ResourceTable` save-set state machine and `KeyGrab` lookup get
focused unit tests in `resources.rs`.

End-to-end the validation gates are the WM runs listed under
*Validation targets*. We don't have a harness for running a WM in
CI; manual validation under an existing `ynest` session is the bar,
same as previous Phase 2 items.

## Codex review notes (2026-04-30)

Submitted spec + plan to codex for review; key corrections folded
back here:

- MappingNotify byte layout: `request` is at byte 4, not byte 1.
  Plan encoder updated accordingly.
- `CirculateWindow`'s argument is the parent/container, not the
  child. Redirect check and event field assignment updated above.
- Passive key grabs activate into a temporary active keyboard grab
  on press, lasting until the matching release.
- Save-set restore reparents to closest non-dying ancestor,
  preserves absolute coordinates, and maps if unmapped.
- `ChangeActivePointerGrab` mutates the active grab record, not the
  `button_grabs` table.
- `CopyPlane` body length is 28 bytes after the request header.
- `GetModifierMapping` reply width is `8 * keycodes_per_modifier`,
  not a fixed 64 bytes.

The reviewer noted that for **Openbox/Fluxbox specifically**, our
deferral list is fine: XKB / BIG-REQUESTS / MIT-SHM / XFIXES /
SHAPE / XInput2 / DAMAGE / COMPOSITE / SYNC / PRESENT can stay
absent so long as `QueryExtension` answers cleanly. SYNC is
optional for `_NET_WM_SYNC_REQUEST` resize handshake; Openbox
should not block on it. i3 and awesome remain Phase 3 because
they have hard XKB dependencies.

Also flagged but not in immediate scope:

- `SetModifierMapping` could be promoted from no-op to a real
  reject ("Failed" status). Leaving as no-op is acceptable until a
  real client trips on it.
- `QueryKeymap`, `GetKeyboardControl`, `Bell` are already stubbed
  in the current implementation.
- `KillClient` is not needed for WM startup; deferred.
- Colormap install/uninstall is irrelevant on a true-color nested
  setup.

## Open issues (deliberate)

- We are not implementing XKB. If Openbox has a hard XKB dependency
  on this distro's Xlib build (it shouldn't — Openbox uses Xlib's
  core keymap helpers by default), this gets bumped to Phase 3.
- We are not implementing BIG-REQUESTS. Some Xlib clients call
  `XQueryExtension("BIG-REQUESTS")` early; absent is a valid answer
  and Xlib falls back to 256 KB max request size. If a Phase 2 target
  hits the cap (unlikely for menus and decorations), revisit then.
- Save-set is a per-client `HashSet`. The X11 spec also allows
  save-set entries to outlive a `DestroyWindow` of the *parent* —
  i.e. when the WM destroys a frame, its child should reparent to
  root. Our destroy path will need to call the save-set restore
  before the recursive destroy walk, in addition to the disconnect
  path.
