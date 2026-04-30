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
  `PolyRectangle`, `PolyFillRectangle`, `ClearArea`, `CopyArea`,
  `PutImage`, `ImageText8`, `ImageText16`, `PolyText8`.
- GC lifecycle/state: `CreateGC`, `ChangeGC`, `FreeGC`,
  `SetClipRectangles`.
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
- [x] **Lifecycle / WM events.** `ReparentWindow` now updates the
      resource tree and emits `ReparentNotify`. `SendEvent` supports
      synthetic `ClientMessage` delivery for the Phase 1 route.
      (`DestroyNotify` and `UnmapNotify` already shipped.) Design:
      [`2026-04-29-phase1-outstanding-design.md`](superpowers/specs/2026-04-29-phase1-outstanding-design.md).
      Plan:
      [`2026-04-29-phase1-outstanding.md`](superpowers/plans/2026-04-29-phase1-outstanding.md).
- [x] **Per-window clipping in the ynest backend.** Each nested
      top-level now gets its own host subwindow; drawing is routed via
      `top_level_host_target`. Child-window drawing now applies
      accumulated host offsets for the existing host-routed drawing
      paths.
- [x] **Pointer events.** `ButtonPress` / `ButtonRelease`,
      `MotionNotify`, `EnterNotify` / `LeaveNotify` delivered via
      `HostInputPump` + `xid_map` fanout. `xeyes` now tracks cursor
      via real `MotionNotify` events.
- [x] **`CopyArea` and `PutImage`.** Phase 1 supports `ZPixmap` into
      host-backed windows and pixmaps; host-backed pixmaps created for
      depths 1, 24, and 32. `CopyArea` handles host-backed
      window/window, pixmap/window, window/pixmap, and pixmap/pixmap
      copies with drawable-depth validation. `XYBitmap`/`XYPixmap`
      remain follow-ups.
- [x] **xterm redraw requests observed so far.** Added forwarding for
      `PolyRectangle` and `ImageText16`, plus per-GC
      `SetClipRectangles` storage and host application. These removed
      the unsupported/stubbed requests seen in scrollback logs.

### Known follow-ups

Small items already identified during recent work, captured here so
they don't get lost. Not yet sized into punch-list bullets; mostly
contingent on landed work.

Consolidated design:
[`2026-04-29-phase1-outstanding-design.md`](superpowers/specs/2026-04-29-phase1-outstanding-design.md).
Implementation plan:
[`2026-04-29-phase1-outstanding.md`](superpowers/plans/2026-04-29-phase1-outstanding.md).

- **`UnmapNotify.from_configure = true`.** Encoder accepts the byte
  for wire correctness; every call site currently passes `false`. The
  `true` path fires when a parent's `ConfigureWindow` shrinks a child
  out of view. Wire it once we track parent-resize-driven implicit
  unmaps.
- **Broader `SendEvent` event types.** Opcode 25 now supports all
  event types (sent-event bit set, propagation up window tree, broadcast
  to root subscribers). Previously ClientMessage-only.
- **Handler-level integration tests for opcode 10 / opcode 4 fanout.**
  Today only the encoder, the `subscribers()` snapshot, and the
  `ResourceTable` state machine are unit-tested; a true
  request → fanout → wire-bytes test would require driving
  `handle_request` against mock writers. Deferred — spec already
  notes the gap.
- **xterm scrollback.** `seq 1 200` scrolls forward cleanly via
  `CopyArea`, and xterm otherwise renders/accepts input. Scrollback via
  scrollbar, mouse wheel, PageUp, and Shift+PageUp still does not behave
  correctly after implementing `ImageText16`, `PolyRectangle`, and
  `SetClipRectangles`. Latest logs show no unsupported opcodes on the
  tested path, so this is parked for now; next investigation should
  focus on xterm input semantics, scrollbar state/grabs, or a subtler
  copy/clip coordinate mismatch rather than a plainly missing opcode.

### Out of scope for Phase 1

- BIG-REQUESTS, MIT-SHM, XKB, XFIXES, DAMAGE, COMPOSITE, SYNC,
  PRESENT, SHAPE, XInput2, GLX. These are Phase 3+.
- RANDR moved to Phase 2 (compatibility stub landed).
- RENDER: partial implementation landed in Phase 2 to satisfy fvwm3
  cursor + Xft-based title text rendering. Full coverage still Phase 3.
- Big-endian clients.
- Selections / clipboard (Phase 2).

## Phase 2 — Desktop semantics

Goal: ICCCM and EWMH behavior, selections, clipboard, focus, grabs,
configure requests, reparenting, override-redirect, root-window
properties. Run a simple WM (Openbox / i3 / awesome / fluxbox).

### Pending — Phase 2 punch list

In rough priority order:

- [x] **Nested RANDR compatibility stub.** Advertises `RANDR` (major
      opcode 128) via `QueryExtension` and exposes one connected output
      (`ynest-0`), one CRTC, and one mode matching the current `ynest`
      screen size. Implements `RRQueryVersion` (1.5), `RRGetScreenSizeRange`,
      `RRGetScreenResources`, `RRGetScreenResourcesCurrent`, `RRGetOutputInfo`,
      `RRGetCrtcInfo`, `RRGetCrtcGammaSize` (size=0), `RRGetCrtcGamma` (size=0),
      `RRGetMonitors` (RANDR 1.5, returns single `ynest-0` monitor), and
      `RRSelectInput` (accepted, not stored). Mutation paths
      (`RRSetScreenConfig`, `RRSetCrtcConfig`) return `BadValue`.
      Also fixed `SetSelectionOwner`/`GetSelectionOwner` to use a real
      per-server selection ownership map (needed for ICCCM WM_S0 acquisition).
      Design:
      [`2026-04-29-nested-randr-compat-design.md`](superpowers/specs/2026-04-29-nested-randr-compat-design.md).
      Plan:
      [`2026-04-29-nested-randr-compat.md`](superpowers/plans/2026-04-29-nested-randr-compat.md).
      **Validated:** `xrandr -q` shows `ynest-0 connected 800x600`; `fvwm3`
      initializes RANDR 1.5, acquires WM_S0, and enters its main event loop.

      Follow-ups:
      - Host-window resize propagation → update `RandrState` dimensions.
      - `RRScreenChangeNotify` delivery to clients that called `RRSelectInput`.
      - Extension-specific error codes (`BadRROutput`, `BadRRCrtc`) instead of `BadValue`.

- [x] **SubstructureRedirect / MapRequest / ConfigureRequest.** When a WM
      registers `SubstructureRedirectMask` (0x100000) on the root window,
      `MapWindow` for unmapped top-level windows sends `MapRequest`
      (event type 20) to the WM instead of mapping directly; similarly
      `ConfigureWindow` sends `ConfigureRequest` (event type 23).
      Already working before Phase 2 plan.

- [x] **Phase 2 fvwm3 feature set.** Passive button grabs
      (GrabButton/UngrabButton/AllowEvents), FillPoly, PolyText16, CopyGC,
      PolyPoint, real TranslateCoordinates, ListProperties, CreateCursor,
      WarpPointer, ConvertSelection/SelectionRequest/SelectionClear, and
      SendEvent for all event types. See plan:
      [`2026-04-29-phase2-fvwm3.md`](superpowers/plans/2026-04-29-phase2-fvwm3.md).

- [x] **RENDER extension proxy (subset).** Forwards CreatePicture,
      FreePicture, CreateGlyphSet, FreeGlyphSet, AddGlyphs,
      CompositeGlyphs8/16/32, Composite, FillRectangles, CreateSolidFill,
      CreateCursor, and QueryVersion/QueryPictFormats to the host.
      Glyphset and picture XIDs are mapped between client and host
      ID spaces. ChangePicture and SetPictureClipRectangles are stubs.
      Coordinate offsets (top-level → host) are patched into the first
      glyphcmd of CompositeGlyphs, into Composite's dst_xy, and into
      FillRectangles' rects. Window bg-pixmap retention is tracked on
      the Window struct so fvwm3's "render to pixmap → set as bg →
      free pixmap → ClearArea" pattern works. See
      [RENDER opcode table](#render-extension-major-opcode-139).
      **Validated:** xclock under fvwm3 shows title bar text and the
      coords popup; cursor creation works; FvwmButtons RightPanel
      shows its bar icon.

- [x] **Root drawing routed to host container.** ROOT_WINDOW now wires
      its host_xid to the host container window at startup so root-
      targeted drawing (ChangeWindowAttributes(root, bg-pixmap) +
      ClearArea(root) — fvwm3's desktop-background pattern) lands in
      the visible viewport. top_level_host_target falls through to
      root with zero offset.

- [x] **Per-depth host GCs.** The default host GC is bound to a depth-24
      drawable, so PutImage onto non-depth-24 pixmaps (e.g. FvwmButtons'
      depth-8 alpha masks for icon compositing) BadMatched on the host
      and silently discarded the image data. ynest now caches one host
      GC per pixmap depth, created on demand using the target drawable
      as the depth/screen reference.

- [x] **Phase 2 wrap-up: WM-grade opcodes.** Spec
      [`2026-04-30-phase2-wrap-up-design.md`](superpowers/specs/2026-04-30-phase2-wrap-up-design.md);
      plan
      [`2026-04-30-phase2-wrap-up.md`](superpowers/plans/2026-04-30-phase2-wrap-up.md).
      Reviewed by codex before execution.

      Landed:
      - `GrabKey` (33) / `UngrabKey` (34): per-server `KeyGrab` table
        with `AnyKey` (keycode=0) and `AnyModifier` (0x8000) wildcards;
        `find_key_grab` walks focus → ancestor chain → root.
      - `GrabKeyboard` (31) / `UngrabKeyboard` (32): real
        `ActiveKeyboardGrab` tracking; explicit-source.
      - Key event routing: `route_key_event` pre-empts focus delivery
        with active grabs; passive-key matches on `KeyPress` install a
        temporary active keyboard grab tagged
        `PassiveKey { keycode }` released on the matching `KeyRelease`.
      - `GetKeyboardMapping` (101) / `GetModifierMapping` (119):
        proxied to host with arbitrary `keysyms_per_keycode` and
        `keycodes_per_modifier`; falls back to local stub on host
        failure.
      - `ChangeKeyboardMapping` (100): host-mediated no-op that
        broadcasts `MappingNotify(Keyboard)` to all clients.
      - `ChangeSaveSet` (6): per-client save-set storage on
        `ClientHandle.save_set`. Spec-correct disconnect restore
        (closest non-dying ancestor + remap + coord preservation) is a
        follow-up.
      - `DestroySubwindows` (5): recursive destroy of each child via
        the existing `destroy_window` pipeline.
      - `CirculateWindow` (13) + `CirculateNotify` (26) +
        `CirculateRequest` (27): SubstructureRedirect path emits
        request, otherwise rotates children naively (Phase-2 stacking
        approximation).
      - `CopyPlane` (63): host XCopyPlane forwarding mirroring CopyArea
        plus the trailing plane mask.
      - `ChangeActivePointerGrab` (30): mutates the
        `ActivePointerGrab` record (event_mask / cursor / time);
        `GrabPointer` / `UngrabPointer` updated to maintain the record.
      - Bug: `DestroyWindow` now frees retained bg-pixmap host XIDs
        across the destroyed subtree.

      Deferred (still Phase 2, not yet landed): save-set restore on
      disconnect (closest non-dying ancestor + coord preservation +
      remap-if-unmapped), `GrabButton` sync replay through pump-thread
      channel, RANDR `RRSelectInput` mask storage and host-resize
      `RRScreenChangeNotify` delivery, `SendEvent` parent propagation,
      `UnmapNotify.from_configure` for shrunk children, manual
      Openbox/Fluxbox validation runs.

### Known follow-ups

- **RRSelectInput mask storage.** `RRSelectInput` is accepted but the mask is not
  stored; `RRScreenChangeNotify` is never delivered. Wire up when needed.
- **GrabButton Sync mode freeze.** The current implementation stores the frozen
  event but AllowEvents(ReplayPointer) requires access to the xid_map which is
  only available in the pointer_event_fanout thread. Needs a proper inter-thread
  channel for replay.
- **CreateCursor XColor struct.** Xlib `XColor` layout must match the system
  Xlib headers; verify on target platform.
- **SendEvent propagation.** Current impl delivers to direct window subscribers;
  does not propagate up the window tree (event_mask=0 with PointerWindow/InputFocus
  destinations).

## Phase 3.1 — Toolkit Compatibility (GTK3) ✓ COMPLETE

Goal: run a simple GTK3 application interactively under `ynest`.

- [x] Implement BIG-REQUESTS (required by modern toolkits).
- [x] Implement XKB proxying to host (GTK3 requires XKB QueryExtension).
  - Fixed: `XkbPerClientFlags` (minor=21) has a reply — Xlib blocks in `_XReply()`
    until it arrives; omitting it caused gtk3-demo to hang at startup.
- [x] Basic XInput2 support (`XIQueryVersion`, `XIGetClientPointer`,
  `XISelectEvents`, `XIQueryDevice`, `XIGetSelectedEvents`, `XIChangeCursor`).
- [x] XI2 keyboard, pointer, crossing, and focus events delivered as GenericEvent
  type 35 to selected clients.
- [x] `XIQueryPointer` (XI2 minor=40): stub reply with correct wire format.
  - `GroupInfo` is 4×CARD8 = 4 bytes; length field = 6 (24 extra bytes).
    Wrong size caused xcb assertion `extra_reply_data_left`.
- [x] RENDER `SetPictureClipRectangles` (minor=6): forward to host with
  window-offset adjustment so clip aligns with `Composite`'s dst translation.
- [x] RENDER `CreateRadialGradient` (minor=35): create host picture like LinearGradient.
- [x] RENDER `ChangePicture` (minor=5): forward when no non-None XID attributes.
  - Critical: `CPClipMask=None` (mask=0x40, value=0) clears the clip after
    clipped glyph rendering. Without forwarding this, stale clips persisted
    and caused all sidebar labels to disappear on the next redraw.
- [x] RENDER `SetPictureTransform` (minor=28), `SetPictureFilter` (minor=30),
  `CreateLinearGradient` (minor=34): forwarded to host.

**Validation Result — gtk3-demo running under ynest + fvwm3:**
- gtk3-demo main window appears with fvwm3 decorations.
- Clicking sidebar items navigates to the correct content pane.
- Clicking "Run" opens child dialogs (fvwm3 handles stacking).
- Sidebar labels, content text, and widget rendering all visible.
- `cargo test --workspace`: all tests pass (114 in yserver-core).

## Phase 3.2 — Advanced Interoperability (in progress)

Goal: extensions and behavior needed for Qt, SDL, GLFW, Electron.
Implement enough RENDER, SHAPE, DAMAGE, COMPOSITE, SYNC,
and PRESENT for real applications.

Design:
[`2026-04-30-phase3-2-advanced-interoperability-design.md`](superpowers/specs/2026-04-30-phase3-2-advanced-interoperability-design.md).

Implementation plan:
[`2026-04-30-phase3-2-advanced-interoperability.md`](superpowers/plans/2026-04-30-phase3-2-advanced-interoperability.md).

Landed so far:
- RENDER blocking reply gaps: `QueryPictIndexValues` and `QueryFilters`
  return exact 32-byte empty replies.
- RENDER glyphset gaps: `ReferenceGlyphSet` aliases existing host glyphsets
  with local refcounts; `FreeGlyphs` translates the glyphset and forwards to
  the host without destroying the glyphset.
- RANDR modern query coverage confirmed: `RRGetScreenResourcesCurrent`,
  `RRGetMonitors`, and empty `RRGetOutputProperty` replies have wire-shape
  tests. `RRSelectInput` masks are stored, and host container ConfigureNotify
  updates root geometry/RANDR state while emitting `RRScreenChangeNotify`,
  CRTC-change, and output-change events to subscribers.
- Extension metadata is centralized for `QueryExtension`, `ListExtensions`,
  and dispatch constants.
- XFIXES is now advertised as version 2.0. Implemented paths include
  `QueryVersion`, `SelectSelectionInput`, `SelectCursorInput`,
  `GetCursorImage` with a valid empty cursor image reply, local region
  lifecycle/algebra/fetch, and no-op `HideCursor`/`ShowCursor`. Selection and
  cursor event masks are stored, but XFIXES events are not emitted yet.
- SHAPE is now advertised as version 1.1. Implemented paths include
  `QueryVersion`, `Rectangles`, `Mask`, `Combine`, `Offset`, `QueryExtents`,
  `SelectInput`, `InputSelected`, and `GetRectangles` backed by local
  per-window bounding/clip/input rectangle lists. Host SHAPE forwarding and
  input-shape-aware pointer hit testing are deferred.
- SYNC is now advertised as version 3.0. Implemented paths include
  `Initialize`, empty `ListSystemCounters`, local counter
  create/set/change/query/destroy, local alarm create/change/query/destroy,
  `GetPriority`, `SetPriority` no-op, and non-blocking `Await`. AlarmNotify
  events and fence requests are deferred.
- DAMAGE is now advertised as version 1.1. Implemented paths include
  `QueryVersion`, damage object create/destroy, explicit `DamageAdd`, and
  `Subtract` with optional XFIXES repair/parts region integration. Automatic
  damage accumulation from drawing paths and DamageNotify events are deferred.
- Composite is now advertised as version 0.4. Implemented paths include
  `QueryVersion`, redirect/unredirect bookkeeping, border-clip region creation,
  `GetOverlayWindow`, `ReleaseOverlayWindow`, and guarded `NameWindowPixmap`
  returning `BadMatch`. Real named window pixmaps are deferred.
- Present is now advertised as version 1.0. Implemented paths include
  `QueryVersion`, `QueryCapabilities` with no optional capabilities,
  `SelectInput` bookkeeping, no-sync `PresentPixmap` pixmap-to-window copy
  when host-backed, non-blocking `NotifyMSC`, and `BadImplementation` for
  non-zero fence requests. Present events and real fence synchronization are
  deferred.
- XKB proxy reply handling was audited against the known reply-producing
  minors, including `GetDeviceInfo` and `SetDebuggingFlags`; void requests
  remain fire-and-forget. `XkbSelectEvents` masks are retained locally for
  future state-notify synthesis.
- XI2 exact-device and wildcard mask matching is covered for keyboard and
  pointer delivery. Scroll valuator packets remain deferred; current wheel
  support still uses core button 4/5 plus XI2 button events.
- Window Maker validation pass (commit `3d3e9da`):
  - `MotionNotify` delivery now matches `ButtonMotion` (0x2000) and the
    per-button `ButtonNMotion` bits (0x100..0x1000) when their button is
    held in `event.state`, in addition to `PointerMotion` (0x40). wmaker
    drives drag tracking through `ButtonMotion`, so without this it
    received zero motion during a drag.
  - Pointer events carry `root_x`/`root_y` in nested-root coordinates
    (translated from the host pump's host-screen values via the top-
    level's known nested position), so popups land at the click site
    instead of being offset by the host container's screen origin.
  - Host pump selects `Exposure` on the container window and maps the
    container `host_xid` to `ROOT_WINDOW`, so Expose events on the
    desktop area reach `expose_event_fanout`. `ChangeWindowAttributes`
    on root forwards `background_pixel` (in addition to
    `background_pixmap`) to the host container so the host auto-clears
    uncovered desktop regions during drags.
  - `expose_event_fanout` walks mapped descendants of an exposed
    top-level and synthesizes `Expose` events for each that overlaps
    the area, in the descendant's local coords. Without this,
    nested-only sub-windows (wmaker's titlebar, resize handle, app
    content panes) never repaint after another top-level moves across
    them. Skipped when the exposed window is `ROOT_WINDOW` since
    top-levels of root have their own host counterparts.
  - **Validated:** wmaker comes up; clicking opens popups under the
    cursor; dragging xterm/xclock frames moves them and the desktop
    repaints with the configured `bg_pixel`. Some artifacts remain
    when frames are dragged off-screen or fully behind another
    top-level (host doesn't preserve those pixels and we don't yet
    use backing-store).
- RANDR 1.0 `RRGetScreenInfo` (minor 5) now replies with the single
  synthetic mode and a 60Hz refresh list. Old clients (e16) probe this
  at startup and block waiting for the reply, so a missing handler
  hung the session before the first window appeared. Encoder lives in
  `yserver_protocol::x11::randr::encode_get_screen_info_reply`.
  **Validated:** Enlightenment e16 starts and a window can be dragged.

## Phase 3.3 — Window manager validation follow-ups (not started)

Goal: clean up rendering artifacts and missing chrome that surfaced
while validating wmaker and e16. None are blockers — the WMs come up
and apps are usable — but each one is a visible glitch under real WM
sessions.

- **Forward SHAPE to host for top-levels.** Currently `SHAPE::Rectangles`,
  `SHAPE::Mask`, `SHAPE::Combine`, and `SHAPE::Offset` only update the
  per-window state stored in `ServerState::shape_windows`. The host
  treats every top-level subwindow as a full rectangle, so themed
  frames that depend on shaped chrome (e16 issues ~1900 `SHAPE::Mask`
  calls per session) render with extra/missing pixels. Plan: detect
  the SHAPE extension on the host connection during `HostX11`
  initialization, cache its major opcode, and on every shape mutation
  targeting a window with a `host_xid` forward `ShapeRectangles` to
  the host with the resolved rect list. Sub-windows without a
  `host_xid` keep their local-only behavior (the parent's host shape
  already clips them). Localized to `host_x11.rs` (a new SHAPE
  forwarder) and the SHAPE branches in `nested.rs::handle_shape_request`.

- **Preserve pixels for fully occluded / off-screen drags.** The host
  doesn't preserve subwindow pixels that are scrolled off-screen or
  fully behind a sibling top-level, and we don't enable backing-store
  on host subwindows. Dragging a frame off-screen and back, or fully
  behind another top-level and back out, leaves stale content on the
  uncovered area until the client redraws. Two options: (a) request
  `BackingStore=Always` on each top-level subwindow at create time,
  trading host memory for correctness, or (b) emit a synthetic
  `ConfigureNotify` + full-window Expose to the affected client when
  we detect the configure that re-exposes the area. Option (a) is
  simpler and what most nested servers do.

- **XFIXES `ChangeCursorByName` (minor 23).** e16 calls this 7+ times
  during cursor theming and we currently log it as `XFIXES::unknown`.
  No reply is expected, so it doesn't block, but the cursor never
  changes. Plan: parse the request (cursor xid + name string), look
  up or create a host cursor by name via the host connection, and
  apply via `ChangeCursor` on the affected windows. Pairs naturally
  with the existing XFIXES cursor implementation.

- **Sub-window Expose covering cross-border / behind-sibling drags.**
  Our `descendants_in_exposed_area` walker (Phase 3.2) handles the
  common case where a top-level moves and its sibling sub-windows
  need repaint, but it relies on the host generating Expose for the
  uncovered region. When the host can't (because the area was
  off-screen or fully behind a stacked top-level), no Expose is
  generated. The same fix as the backing-store item above resolves
  this — either backing-store or a synthetic full-window Expose on
  the configure boundary.

- **Verify e16 RENDER coverage.** e16 sends 5400+ `RENDER::CreatePicture`
  / `FreePicture` and 4400+ `RENDER::Composite` calls per session.
  Spot-check that all of these succeed against the host (no silent
  drops on opcodes we have but parse incorrectly), since the visible
  artifacts may also include subtly mis-encoded glyphs or composites
  for e16's themed buttons.

- **Validation runs:** Openbox, Fluxbox (called out under Phase 2 as
  deferred), then back to wmaker/e16 once SHAPE forwarding lands to
  confirm the chrome artifacts go away.

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

## Opcode implementation status

**Key:**
- ✓ full — implemented with correct side effects
- ↩ reply — sends a reply (may be stub/partial in content)
- ∅ no-op — accepted silently, no error, no meaningful effect
- ✗ not handled — falls to "unsupported opcode" log; fire-and-forget
  opcodes silently succeed, reply opcodes will block the client

### Core X11 opcodes (1–127)

#### Window management

| Op | Name                  | Status | Notes |
|----|-----------------------|--------|-------|
|  1 | CreateWindow          | ✓ | host subwindow allocated for top-levels |
|  2 | ChangeWindowAttributes | ✓ | event_masks, cursor, override_redirect, background-pixel, background-pixmap |
|  3 | GetWindowAttributes   | ↩ | |
|  4 | DestroyWindow         | ✓ | recursive; fires DestroyNotify/UnmapNotify; frees retained bg-pixmap host XIDs |
|  5 | DestroySubwindows     | ✓ | recursively destroys each child via the existing destroy pipeline |
|  6 | ChangeSaveSet         | ✓ | per-client storage; restore on disconnect is a follow-up |
|  7 | ReparentWindow        | ✓ | fires ReparentNotify |
|  8 | MapWindow             | ✓ | SubstructureRedirect to WM if registered |
|  9 | MapSubwindows         | ✓ | |
| 10 | UnmapWindow           | ✓ | fires UnmapNotify |
| 11 | UnmapSubwindows       | ✓ | |
| 12 | ConfigureWindow       | ✓ | SubstructureRedirect to WM if registered |
| 13 | CirculateWindow       | ✓ | SubstructureRedirect emits CirculateRequest; otherwise naive child rotation + CirculateNotify |
| 14 | GetGeometry           | ↩ | |
| 15 | QueryTree             | ↩ | |

#### Atoms, properties, selections

| Op | Name                  | Status | Notes |
|----|-----------------------|--------|-------|
| 16 | InternAtom            | ↩ | server-global atom table, 68 predefined |
| 17 | GetAtomName           | ↩ | |
| 18 | ChangeProperty        | ✓ | fires PropertyNotify cross-client |
| 19 | DeleteProperty        | ✓ | fires PropertyNotify |
| 20 | GetProperty           | ↩ | |
| 21 | ListProperties        | ✓ | returns all property atoms on window |
| 22 | SetSelectionOwner     | ✓ | per-server ownership map |
| 23 | GetSelectionOwner     | ↩ | |
| 24 | ConvertSelection      | ✓ | delivers SelectionRequest to owner; SelectionNotify(None) if no owner |

#### Input grabs and focus

| Op | Name                      | Status | Notes |
|----|---------------------------|--------|-------|
| 25 | SendEvent                 | ✓ | all event types; sent-event bit set; propagation and broadcast |
| 26 | GrabPointer               | ✓ | records grab owner; all pointer events redirected until UngrabPointer |
| 27 | UngrabPointer             | ✓ | clears active grab |
| 28 | GrabButton                | ✓ | passive grabs stored; ButtonPress activates transient grab |
| 29 | UngrabButton              | ✓ | removes matching passive grabs |
| 30 | ChangeActivePointerGrab   | ✓ | mutates active pointer grab record (event_mask / cursor / time) |
| 31 | GrabKeyboard              | ✓ | installs explicit ActiveKeyboardGrab; returns GrabSuccess |
| 32 | UngrabKeyboard            | ✓ | clears active keyboard grab if held by client |
| 33 | GrabKey                   | ✓ | passive grab table; AnyKey/AnyModifier wildcards |
| 34 | UngrabKey                 | ✓ | removes matching passive grabs |
| 35 | AllowEvents               | ✓ | AsyncPointer/SyncPointer clears freeze; ReplayPointer re-routes |
| 36 | GrabServer                | ∅ | |
| 37 | UngrabServer              | ∅ | |
| 38 | QueryPointer              | ↩ | delegates to host |
| 39 | GetMotionEvents           | ✗ | |
| 40 | TranslateCoordinates      | ✓ | real absolute-position walk; child-window lookup |
| 41 | WarpPointer               | ✓ | warps to host subwindow with offset translation |
| 42 | SetInputFocus             | ✓ | routes keyboard events to focused client |
| 43 | GetInputFocus             | ↩ | |
| 44 | QueryKeymap               | ↩ | stub — all zeros |

#### Fonts

| Op | Name                  | Status | Notes |
|----|-----------------------|--------|-------|
| 45 | OpenFont              | ✓ | opens on host, caches full FontMetrics |
| 46 | CloseFont             | ∅ | |
| 47 | QueryFont             | ↩ | from metrics cache; FONTABLE resolves GC font |
| 48 | QueryTextExtents      | ↩ | computed locally from CharInfo cache |
| 49 | ListFonts             | ↩ | proxied to host |
| 50 | ListFontsWithInfo     | ↩ | proxied to host, multi-reply sentinel forwarded |
| 51 | SetFontPath           | ✗ | |
| 52 | GetFontPath           | ✗ | |

#### Pixmaps and GCs

| Op | Name                  | Status | Notes |
|----|-----------------------|--------|-------|
| 53 | CreatePixmap          | ✓ | host pixmap for depths 1/24/32 |
| 54 | FreePixmap            | ✓ | |
| 55 | CreateGC              | ✓ | |
| 56 | ChangeGC              | ✓ | |
| 57 | CopyGC                | ✓ | copies selected GC attributes by value_mask |
| 58 | SetDashes             | ✗ | |
| 59 | SetClipRectangles     | ✓ | stored and applied on host GC |
| 60 | FreeGC                | ✓ | |

#### Drawing

| Op | Name                  | Status | Notes |
|----|-----------------------|--------|-------|
| 61 | ClearArea             | ✓ | respects background-pixmap (CopyArea) or background-pixel fill |
| 62 | CopyArea              | ✓ | host-backed win↔win, pixmap↔win etc. |
| 63 | CopyPlane             | ✓ | host XCopyPlane; mirrors CopyArea drawable matrix |
| 64 | PolyPoint             | ✓ | forwarded to host; coord translation applied |
| 65 | PolyLine              | ✓ | forwarded to host; pixmap drawables supported |
| 66 | PolySegment           | ✓ | forwarded to host; both endpoints translated; pixmap drawables supported |
| 67 | PolyRectangle         | ✓ | forwarded to host; pixmap drawables supported |
| 68 | PolyArc               | ✓ | forwarded to host; pixmap drawables supported |
| 69 | FillPoly              | ✓ | forwarded to host via XFillPolygon; coord translation applied |
| 70 | PolyFillRectangle     | ✓ | forwarded to host; pixmap drawables supported |
| 71 | PolyFillArc           | ✓ | forwarded to host; pixmap drawables supported |
| 72 | PutImage              | ✓ | ZPixmap; XYBitmap/XYPixmap unsupported |
| 73 | GetImage              | ✓ | proxied to host; blank fallback if no host backing |
| 74 | PolyText8             | ✓ | forwarded to host |
| 75 | PolyText16            | ✓ | forwarded to host; coord translation applied |
| 76 | ImageText8            | ✓ | forwarded to host |
| 77 | ImageText16           | ✓ | forwarded to host |

#### Colormaps and colours

| Op | Name                  | Status | Notes |
|----|-----------------------|--------|-------|
| 78 | CreateColormap        | ∅ | |
| 79 | FreeColormap          | ✗ | |
| 80 | CopyColormapAndFree   | ✗ | |
| 81 | InstallColormap       | ✗ | |
| 82 | UninstallColormap     | ✗ | |
| 83 | ListInstalledColormaps | ✗ | |
| 84 | AllocColor            | ↩ | echoes requested RGB |
| 85 | AllocNamedColor       | ↩ | named colour table, fallback gray |
| 86 | AllocColorCells       | ✗ | |
| 87 | AllocColorPlanes      | ✗ | |
| 88 | FreeColors            | ✗ | |
| 89 | StoreColors           | ✗ | |
| 90 | StoreNamedColors      | ✗ | |
| 91 | QueryColors           | ↩ | returns pixel mapped back to RGB |
| 92 | LookupColor           | ↩ | named colour table |

#### Cursors

| Op | Name                  | Status | Notes |
|----|-----------------------|--------|-------|
| 93 | CreateCursor          | ✓ | XCreatePixmapCursor; applied via ChangeWindowAttributes |
| 94 | CreateGlyphCursor     | ✓ | cursor ID allocated and tracked |
| 95 | FreeCursor            | ✓ | |
| 96 | RecolorCursor         | ∅ | |
| 97 | QueryBestSize         | ↩ | echoes requested width/height |

#### Extensions and misc

| Op | Name                      | Status | Notes |
|----|---------------------------|--------|-------|
|  98 | QueryExtension           | ✓ | RANDR, RENDER, BIG-REQUESTS, XKEYBOARD, XInputExtension |
|  99 | ListExtensions           | ✓ | returns list of supported extensions |
| 100 | ChangeKeyboardMapping    | ✓ | host-mediated no-op; broadcasts MappingNotify(Keyboard) |
| 101 | GetKeyboardMapping       | ↩ | proxied to host; falls back to local stub on host failure |
| 103 | Bell                     | ∅ | |
| 104 | ChangeKeyboardControl    | ∅ | |
| 108 | SetScreenSaver           | ∅ | |
| 111 | ListHosts                | ↩ | empty list |
| 115 | RotateProperties         | ↩ | stub — no-op with empty reply |
| 116 | SetPointerMapping        | ∅ | |
| 117 | GetPointerMapping        | ↩ | stub — buttons 1,2,3 |
| 118 | SetModifierMapping       | ∅ | |
| 119 | GetModifierMapping       | ↩ | proxied to host; arbitrary keycodes_per_modifier |
| 127 | NoOperation              | ∅ | |

### BIG-REQUESTS extension (major opcode 135)

| Minor | Name                  | Status | Notes |
|-------|-----------------------|--------|-------|
|   0   | Enable                | ✓ | enables 32-bit length support; max 1MB advertised |

### XKEYBOARD extension (major opcode 136)

| Minor | Name                  | Status | Notes |
|-------|-----------------------|--------|-------|
|   *   | (any)                 | ✓ | proxied to host after opcode substitution |

### XInput2 extension (major opcode 137)

| Minor | Name                  | Status | Notes |
|-------|-----------------------|--------|-------|
|  44   | XISetClientPointer    | ∅ | accepted no-op |
|  45   | XIGetClientPointer    | ↩ | returns virtual core pointer |
|  46   | XISelectEvents        | ✓ | mask storage in ClientHandle |
|  47   | XIQueryVersion        | ↩ | replies with version 2.2 |
|  48   | XIQueryDevice         | ↩ | returns virtual core pointer and keyboard |
|  60   | XIGetSelectedEvents   | ↩ | returns stored masks for the calling client |
|   *   | (other)               | ∅ | stubs / ignored |

Supported XI2 events: `KeyPress`, `KeyRelease`, `ButtonPress`,
`ButtonRelease`, `Motion`, `Enter`, `Leave`, `FocusIn`, and `FocusOut`.

### RANDR extension (major opcode 128)

Fully described in the Phase 2 RANDR item above. All read-only queries
implemented as stubs; mutation paths return `BadValue`.

### RENDER extension (major opcode 139)

Subset implementation landed during Phase 2 to support fvwm3's
cursor allocation and Xft-based title bar text rendering. Picture and
glyphset resources are tracked in `ResourceTable`; XIDs are mapped
between client and host ID spaces.

| Minor | Name                  | Status | Notes |
|-------|-----------------------|--------|-------|
|   0   | QueryVersion          | ↩ | proxied — replies with host's version |
|   1   | QueryPictFormats      | ↩ | replies with 4 synthetic formats (A1, A8, RGB24, ARGB32) |
|   2   | QueryPictIndexValues  | ↩ | exact empty reply |
|   4   | CreatePicture         | ✓ | format ID translated; coord offset stored on `PictureState` |
|   5   | ChangePicture         | ∅ | clip mask / repeat not applied to host picture |
|   6   | SetPictureClipRectangles | ✗ | |
|   7   | FreePicture           | ✓ | |
|   8   | Composite             | ✓ | dst_xy patched with dst picture's x/y offset; mask=0 forwards as host xid 0 |
|  17   | CreateGlyphSet        | ✓ | format ID translated to host equivalent |
|  18   | ReferenceGlyphSet     | ✓ | aliases existing host glyphset with refcount |
|  19   | FreeGlyphSet          | ✓ | |
|  20   | AddGlyphs             | ✓ | body padding/length corrected against Xephyr trace |
|  22   | FreeGlyphs            | ✓ | glyphset XID translated; request forwarded to host |
|  23   | CompositeGlyphs8      | ✓ | first glyphcmd's deltax/deltay patched with picture offset |
|  24   | CompositeGlyphs16     | ✓ | same patching as 8 |
|  25   | CompositeGlyphs32     | ✓ | same patching as 8 |
|  26   | FillRectangles        | ✓ | rectangle coords offset by picture's x/y offset |
|  27   | CreateCursor          | ✓ | from picture; cursor XID allocated locally |
|  29   | QueryFilters          | ↩ | exact empty filter/alias reply |
|  31   | CreateAnimCursor      | ∅ | accepted no-op |
|  32   | AddTraps              | ∅ | accepted no-op |
|  33   | CreateSolidFill       | ✓ | |
|  34   | CreateLinearGradient  | ✓ | |
|  35   | CreateRadialGradient  | ✓ | |
|  36   | CreateConicalGradient | ∅ | accepted no-op |

Notes:
- Wire encoding parity with the host was verified against
  `docs/assets/xephyr-xclock-fvwm3-trace.log`. Three off-by-one length
  bugs were fixed (CreatePicture, FillRectangles, AddGlyphs); each
  caused either wire misalignment with the host or trailing-zero
  pollution of glyph data.
- `host_x11.rs::create_subwindow` uses `GetInputFocus` (not
  `GetGeometry`) for sync, with a `reply_buffer` for over-read
  responses, so unrelated RENDER errors interleaved with sync replies
  no longer cause the drain loop to hang.

### Known follow-ups (RENDER)

- ChangePicture is a no-op stub; clip-mask and repeat values aren't
  forwarded. fvwm3 doesn't seem to need them, but proper Xft clipping
  would require this.
- DestroyWindow should release any retained bg-pixmap host XIDs
  (`Window.background_pixmap_host_xid`); currently they leak on
  window destroy.
- Sub-window expose handling: when the host top-level is re-exposed,
  ynest doesn't re-paint fvwm3 sub-window backgrounds because the
  sub-windows themselves have no host backing. Currently fine because
  the host's own backing store keeps the rendered output, but tiled
  bg pixmaps wouldn't survive a forced expose.
