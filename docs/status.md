# Status

Tracks progress against the phases in [`high-level-design.md`](high-level-design.md).
Update as work lands.

## Phase 1 — Nested protocol core (in progress)

Goal: accept X11 clients on a Unix socket, complete setup/auth, implement
resource IDs, atoms, properties, windows, basic events, and errors. Run
`xeyes`, `xclock`, `xterm`, `xev`.

### Working

- Unix socket listener, per-client thread, setup handshake (little-endian
  clients only — by design).
- Per-client resource ID space (allocated via `IdAllocator`),
  server-global atom table with 68 predefined atoms, per-client
  event_masks per window.
- Property storage: `ChangeProperty`, `DeleteProperty`, `GetProperty`
  with cross-client `PropertyNotify` fanout.
- Window tree: `CreateWindow`, `DestroyWindow` (recursive), `MapWindow`,
  `UnmapWindow`, `ConfigureWindow`, `GetGeometry`, `QueryTree`,
  `ChangeWindowAttributes`, `GetWindowAttributes`.
- Drawing forwarded to host: `PolyLine`, `PolyArc`, `PolyFillArc`,
  `PolyFillRectangle`, `ClearArea`, `ImageText8`, `PolyText8`.
- GC lifecycle: `CreateGC`, `ChangeGC`, `FreeGC`.
- Pixmap/cursor lifecycle (allocation only).
- Events emitted: `Expose`, `MapNotify`, `ConfigureNotify`, `KeyPress`,
  `KeyRelease`, `FocusIn`, `FocusOut`, `PropertyNotify`,
  `DestroyNotify`, `UnmapNotify` (cross-client subscriber fanout via
  per-window event masks).
- Keyboard forwarding from the host window to the focused nested client.
- `xeyes`, `xclock`, and `xterm` come up; `xterm` accepts input.

### Pending — Phase 1 punch list

In rough priority order:

- [x] **Real font metrics in `QueryFont`.** `OpenFont` now opens the
      same font on the host server, issues `QueryFont`, and caches the
      full `FontMetrics` (header + properties + per-glyph `CharInfo`).
      `QueryFont` replies with the cached data. `FONTABLE` resolution
      handles both Font and GC ids (GC carries a `font` attribute).
- [x] **`QueryTextExtents` (opcode 48).** Computed locally from the
      cached `CharInfo` array — no host round trip per call.
- [x] **`ListFonts` / `ListFontsWithInfo` (opcode 49 / 50).** Proxied
      to the host. `ListFontsWithInfo` forwards each per-font reply
      until the trailing sentinel reply.
- [x] **Property storage.** Real per-window property storage with
      `ChangeProperty` / `DeleteProperty` / `GetProperty` and
      cross-client `PropertyNotify` fanout via per-(client, window)
      event masks.
- [x] **`UnmapNotify`.** Fired on every mapped → unmapped transition,
      both explicit (`UnmapWindow`) and implicit (`DestroyWindow` and
      client-disconnect cleanup). Cross-client subscriber fanout via
      the same per-(client, window) event masks; root window is
      protected from unmap inside `ResourceTable::unmap_window`.
- [ ] **Lifecycle / WM events.** Emit `ReparentNotify` and
      `ClientMessage` so window managers and toolkits behave.
      (`DestroyNotify` and `UnmapNotify` already shipped.)
- [ ] **Per-window clipping in the ynest backend.** All nested top-level
      windows currently render into a single host window with no
      coordinate translation or clipping. Give each nested top-level its
      own host subwindow.
- [ ] **Pointer events.** `ButtonPress` / `ButtonRelease`,
      `MotionNotify`, `EnterNotify` / `LeaveNotify`. Today xeyes only
      animates because it polls `QueryPointer`.
- [ ] **`CopyArea` and `PutImage`.** Both are stubs; xterm uses them for
      scrolling and some text paths.

### Known follow-ups

Small items already identified during recent work, captured here so
they don't get lost. Not yet sized into punch-list bullets; mostly
contingent on landed work.

- **`UnmapSubwindows` (opcode 11).** Currently a stub. Per X11 spec,
  performs `UnmapWindow` on every mapped child bottom-to-top. Becomes
  a thin loop over the child list reusing
  `ResourceTable::unmap_window` and the `UnmapNotify` fanout
  introduced for opcode 10. Spec:
  [`2026-04-28-unmap-notify-design.md`](superpowers/specs/2026-04-28-unmap-notify-design.md)
  ("Non-goals").
- **`UnmapNotify.from_configure = true`.** Encoder accepts the byte
  for wire correctness; every call site currently passes `false`. The
  `true` path fires when a parent's `ConfigureWindow` shrinks a child
  out of view. Wire it once we track parent-resize-driven implicit
  unmaps.
- **`SendEvent` (opcode 25).** Lets clients synthesize events for any
  window. Required by ICCCM (`WM_DELETE_WINDOW` etc.) so Phase 2 work
  unblocks on it. Same fanout machinery as the existing `*Notify`
  events, plus the `SEND_EVENT` flag (high bit of event byte 0).
- **Handler-level integration tests for opcode 10 / opcode 4 fanout.**
  Today only the encoder, the `subscribers()` snapshot, and the
  `ResourceTable` state machine are unit-tested; a true
  request → fanout → wire-bytes test would require driving
  `handle_request` against mock writers. Deferred — spec already
  notes the gap.
- **`PendingDestroy` struct.** The 5-tuple
  `(window, parent, was_mapped, on_window, on_parent)` carried
  through opcode 4 and disconnect cleanup is currently silenced with
  `#[allow(clippy::type_complexity)]`. Extract a named struct the
  next time an event joins the destroy path (e.g. `ReparentNotify`).

### Out of scope for Phase 1

- BIG-REQUESTS, MIT-SHM, RANDR, XKB, XFIXES, DAMAGE, COMPOSITE, SYNC,
  PRESENT, SHAPE, RENDER, XInput2, GLX. These are Phase 3+.
- Big-endian clients.
- Selections / clipboard (Phase 2).

## Phase 2 — Desktop semantics

Goal: ICCCM and EWMH behavior, selections, clipboard, focus, grabs,
configure requests, reparenting, override-redirect, root-window
properties. Run a simple WM (Openbox / i3 / awesome / fluxbox).

Not started. Blocked on Phase 1 property storage and lifecycle events.

## Phase 3 — Toolkit compatibility

Goal: extensions and behavior needed for GTK, Qt, SDL, GLFW, Electron.
Implement enough XKB, XInput2, RENDER, SHAPE, DAMAGE, COMPOSITE, SYNC,
and PRESENT for real applications.

Not started.

## Phase 4 — Accelerated clients

Goal: modern GLX/EGL/Vulkan direct-rendering paths, MIT-SHM, buffer
sharing. Validate real GPU-accelerated clients.

Not started.

## Phase 5 — Full desktop sessions

Goal: run Xfce, MATE, LXQt, and standalone WM sessions end to end.
Validate panels, launchers, notification daemons, clipboard managers,
screen lockers, global shortcuts, external compositors.

Not started.

## Phase 6 — Standalone DRM/KMS

Goal: replace the nested backend with a real backend on libinput, udev,
GBM, EGL/Vulkan, and atomic KMS. Hotplug, multi-monitor presentation,
session management, fullscreen / direct-scanout paths.

Not started. The `yserver` binary is a placeholder.

## Phase 7 — Security hardening

Goal: per-client capabilities, permission prompts or launch-time
configuration, dummy responses for unauthorized requests, an
Xorg-compatible compatibility mode.

Not started.
