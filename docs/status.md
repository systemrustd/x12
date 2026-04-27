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
- Per-client resource ID space, per-client atom namespace with 68
  predefined atoms.
- Window tree: `CreateWindow`, `DestroyWindow` (recursive), `MapWindow`,
  `UnmapWindow`, `ConfigureWindow`, `GetGeometry`, `QueryTree`,
  `ChangeWindowAttributes`, `GetWindowAttributes`.
- Drawing forwarded to host: `PolyLine`, `PolyArc`, `PolyFillArc`,
  `PolyFillRectangle`, `ClearArea`, `ImageText8`, `PolyText8`.
- GC lifecycle: `CreateGC`, `ChangeGC`, `FreeGC`.
- Pixmap/cursor lifecycle (allocation only).
- Events emitted: `Expose`, `MapNotify`, `ConfigureNotify`, `KeyPress`,
  `KeyRelease`, `FocusIn`, `FocusOut`.
- Keyboard forwarding from the host window to the focused nested client.
- `xeyes`, `xclock`, and `xterm` come up; `xterm` accepts input.

### Pending — Phase 1 punch list

In rough priority order:

- [ ] **Real font metrics in `QueryFont`.** `QueryFont` currently returns
      zero per-character `CharInfo` entries and hardcoded ascent/descent.
      Source metrics from the host's `fixed` font and emit a real
      `CharInfo` array. Fixes xterm cell-width / right-prompt alignment.
- [ ] **`QueryTextExtents` (opcode 48).** Currently returns
      "unsupported opcode". xterm uses it to measure text it can't deduce
      from per-char metrics.
- [ ] **`ListFonts` / `ListFontsWithInfo` (opcode 49).** At minimum
      return `fixed` so xterm font discovery doesn't fall over.
- [ ] **Property storage.** `ChangeProperty`, `DeleteProperty`, and
      `GetProperty` are no-ops; `GetProperty` always returns `type=None`.
      Implement real per-window property storage and emit
      `PropertyNotify`.
- [ ] **Lifecycle / WM events.** Emit `DestroyNotify`, `UnmapNotify`,
      `ReparentNotify`, and `ClientMessage` so window managers and
      toolkits behave.
- [ ] **Per-window clipping in the ynest backend.** All nested top-level
      windows currently render into a single host window with no
      coordinate translation or clipping. Give each nested top-level its
      own host subwindow.
- [ ] **Pointer events.** `ButtonPress` / `ButtonRelease`,
      `MotionNotify`, `EnterNotify` / `LeaveNotify`. Today xeyes only
      animates because it polls `QueryPointer`.
- [ ] **`CopyArea` and `PutImage`.** Both are stubs; xterm uses them for
      scrolling and some text paths.

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
