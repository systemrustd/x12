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

- [ ] **e16 popup menu compatibility.** Root/container pointer events
      are selected in ynest, `QueryPointer` reports nested
      root-relative coordinates, and `ConfigureWindow` now preserves
      and forwards `CWSibling`/`CWStackMode` for host-backed
      top-levels. Current investigation is focused on e16's reparented
      menu subtree, SHAPE requests, and child-window redraw/clipping
      semantics; core GC `GCClipMask=None` now clears stored clip
      rectangles. SHAPE `Mask(source=None)` now clears the stored client
      shape instead of recording an empty region, and non-None mask
      bookkeeping uses the source pixmap dimensions.

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

## Phase 3.3 — Window manager validation follow-ups (in progress)

Goal: clean up rendering artifacts and missing chrome that surfaced
while validating wmaker and e16. None were blockers — the WMs came up
and apps were usable — but each one was a visible glitch under real WM
sessions.

Design:
[`2026-04-30-phase3-3-wm-validation-followups-design.md`](superpowers/specs/2026-04-30-phase3-3-wm-validation-followups-design.md).
Plan:
[`2026-04-30-phase3-3-wm-validation-followups.md`](superpowers/plans/2026-04-30-phase3-3-wm-validation-followups.md).

### Landed

- [x] **Forward SHAPE to host for top-levels.** `HostX11` now probes
      the host's SHAPE extension during init and caches its major
      opcode. After every `Rectangles`/`Mask`/`Combine`/`Offset`
      mutation that targets a top-level with a `host_xid`,
      `handle_shape_request` mirrors the resolved bounding/clip rect
      list to the host as a single `ShapeRectangles(op=Set,
      ordering=Unsorted)`. Local `shape_windows` remains the source
      of truth for `QueryExtents`/`GetRectangles`/`InputSelected`.
      Sub-windows without host backing keep local-only behavior; the
      parent's host shape already clips them.

- [x] **XFIXES `ChangeCursorByName` (minor 23).** Forward to the host
      so it can resolve the cursor name against its own theme. We
      cache the host XFIXES major opcode at init and translate the
      nested cursor XID to the host XID via `cursor_host_xid` before
      forwarding. e16 stops logging `XFIXES::unknown minor=23`.

- [x] **Multi-glyph `CompositeGlyphs` delta patching.**
      `render_composite_glyphs` previously patched only the first
      non-sentinel glyph command's delta and stopped, so multi-run
      composites that switch glyphsets via the 255 sentinel landed
      later runs at the wrong destination origin. The patch loop now
      walks every glyph command, advancing past each non-sentinel
      header plus its `count * id_size` payload (id_size = 1 / 2 / 4
      for `CompositeGlyphs8`/`16`/`32`).

- [x] **Root window resize plumbing — core `ConfigureNotify`.**
      `handle_host_container_resize` already updated `RandrState`
      and root geometry and fanned out `RRScreenChangeNotify` plus
      CRTC/output events. It now also emits a core `ConfigureNotify`
      on `ROOT_WINDOW` to clients selecting `StructureNotifyMask`
      *before* the RANDR fanout, so panels and "fill the screen"
      apps without RANDR awareness reflow correctly. Tests assert
      `RandrState` + `ROOT_WINDOW` geometry + the
      `StructureNotify`-only client path. A small probe lives at
      [`docs/root-resize-probe.py`](root-resize-probe.py) for
      manual validation.

- [x] **RENDER opcode-table reconciliation.** `ChangePicture` is
      no longer marked `∅` (it forwards scalar attributes and
      explicit-None XID attributes); `SetPictureClipRectangles` is
      `✓` (forwarded with picture x/y offset translation).

### Validation outcomes

Smoke pass under each WM (release ynest on `:99` + xterm + xclock,
host container at default 800×600):

- **wmaker:** WM starts; xterm and xclock render; window chrome
  draws. Drag/restack works (Phase 3.2 backing-store covers the
  drag-redraw scenarios from the design's items #2/#4 — no
  artifacts under the validation set, so synthetic Expose was not
  needed in this phase). Two pre-existing rendering gaps remain
  (filed below as Phase 3.4).
- **e16:** Regressed since the last documented startup pass —
  exits with status 1 during initial setup, before any window is
  mapped. Pre-Phase-3.3 build also exits the same way, so the
  regression predates this phase. Filed as Phase 3.4.
- **fvwm3:** Starts and renders. The host-container-resize path
  (host xdotool windowsize) reliably crashes fvwm3 itself with a
  segfault — reproducible without ynest changes. Likely an fvwm3
  bug; deferred.
- **openbox:** WM starts and clients connect, but the rendered
  container is blank — no client chrome draws. Filed as Phase 3.4.
- **Fluxbox:** Not installed in the validation environment.

## Phase 3.4 — Bugfixing the Phase 3.3 follow-ups (in progress)

### Landed

- [x] **e16 startup regression — fixed.** Root cause: atom IDs leak
      from the host into our protocol stream via host-proxied replies
      (most notably the `FONTPROP` atoms inside `ListFontsWithInfo`).
      Clients then call `XGetAtomName` on those atoms. Previously we
      synthesized a successful reply with the placeholder name
      `"UNKNOWN"` for any atom we hadn't seen, which fooled clients
      into believing the atom existed under that name. e16 hit this
      via FONT properties returned from `ListFontsWithInfo`, then
      silently exited later when atom didn't behave as expected.
      Fix: forward `GetAtomName` for atoms not in our local table to
      the host (`host_x11::get_atom_name`) and proxy the host's name
      back. If the host doesn't know either, return `BadAtom` —
      spec-correct. e16 now starts and renders apps under `ynest`.

- [x] **e16 popup-menu groundwork (cherry-pick of `origin/e16-popup-tests`).**
      Forward `CWSibling` / `CWStackMode` to host `ConfigureWindow`
      for host-backed top-levels so e16's popup raises land in the
      right stacking order. Local `ResourceTable::configure_window`
      now restacks the parent's children list per the request.
      `SHAPE::Mask(source = None)` clears the stored shape (back to
      the default unshaped rectangle) instead of recording an empty
      region, so e16's menu reparenting no longer leaves popups
      invisibly clipped. Non-`None` `SHAPE::Mask` source uses the
      source pixmap's geometry. Core GC `GCClipMask = None` clears
      stored clip rectangles in `ResourceTable`. Map / Reparent /
      ConfigureWindow / SHAPE handlers now log enough detail to
      drive the next iteration. e16's "Enlightenment Message Dialog"
      now renders text and buttons through `ynest`.

### Phase 3.4 follow-ups

- **wmaker duplicate-Expose on top-level Map (fixed).** ynest's
  `MapWindow` handler emitted an `Expose(window)` even when the
  window has a `host_xid` — the host server then fired its own
  Expose for the same subwindow via the host pump, so wmaker saw
  every appicon get two consecutive Exposes and reacted by
  re-creating its appicon background pixmap on each one (3 CWAs
  on a single appicon vs 1 on Xephyr). Fixed: gate the manual
  Expose-emit in MapWindow on `host_xid.is_none()` so we only
  synthesize when the host doesn't.
- **wmaker appicon icon graphic still missing.** Confirmed via
  x11trace + 48×48 / 64×64 PutImage byte dumps: wmaker correctly
  PutImages a 48×48 d24 icon graphic into its `WM_HINTS`
  icon_pixmap, but the 64×64 appicon-bg pixmap that gets attached
  to the appicon window contains only the tile bg + 3D border —
  no `CopyArea` from icon-pixmap to bg-pixmap appears in the
  trace. Same flow on Xephyr+MIT-SHM (no CopyArea after the
  shm-CopyArea). The icon composition step is in wmaker's
  client-side `wraster` and only lands in the pixmap on the
  MIT-SHM path. Without MIT-SHM (which we don't yet implement),
  wmaker doesn't composite the icon onto the bg, so appicons stay
  empty. Implementing MIT-SHM is the path forward.
- **wmaker title-bar close button missing.** Same shape: title-bar
  itself draws but the close-button glyph in its corner is absent.
  Same suspected root cause — sub-window-of-titlebar drawing not
  reaching the host. Pre-existing.
- **e16 popup/dialog "wireframe" rendering — environmental, not
  ynest.** Phase 3.4 follow-up: the e16 dialog visible after the
  popup cherry-pick draws as ~17 nested rectangles per dialog
  frame because e16's draw stream contains *only* outlines —
  26 `PolyRectangle`, 64 `PolySegment`, 76 `PolyText8`, 109
  `ChangeGC`, **zero** `PolyFillRectangle`/`CopyArea`/`PutImage`.
  e16's normal chrome is drawn from theme tile pixmaps composed
  via `CopyArea`; here e16 can't load its theme (because the
  real `/home/jos` is read-only in this sandbox and e16 falls
  back to a no-theme dialog) so the fills are absent and the
  3D-stepped widget borders look like wireframe trails. Real e16
  popups in a writable HOME would render as proper filled chrome.
  Re-test once the validation env has writable user state.
- **openbox WM chrome not drawn.** With the atom fix, openbox
  starts and apps render correctly (`xeyes` is fully visible
  inside the openbox frame). What's still missing is the openbox
  *frame chrome itself* — the title bar, label text, and border
  are not drawn, so the frame appears as a black/empty
  rectangle around the client. Repro: `ynest 99` + `openbox` +
  `xeyes` (or `xclock`), screenshot the container; you'll see the
  client content but no visible WM decorations. Suspect: openbox
  draws frame decorations into child sub-windows of the frame
  (label, button windows visible in the trace as 1×1 children of
  `0x100139`), and that drawing path doesn't reach the host —
  same family of bug as the wmaker chrome issues.
- **fvwm3 segfaults on host container resize.** `xdotool
  windowsize 0x400000 1024 768` while fvwm3 is running reliably
  segfaults fvwm3 immediately after the new core
  `ConfigureNotify(root)` is emitted. Reproduces with no clients
  attached. Likely an fvwm3 bug (real Xorg also emits root
  ConfigureNotify on screen resize via xrandr). Deferred per the
  design's "swap and document" rule until a target client
  misbehaves under another order. Re-test after a fvwm3 update.
- **Apps disappear after host resize (fvwm).** Original Phase 3.2
  observation; not addressed in 3.3. Now superseded by the
  segfault above for fvwm3 specifically.
- **fvwm segfault when host window is closed.** Original Phase 3.2
  observation; not addressed in 3.3.
- **Sub-window Expose for fully off-screen / behind-sibling drags
  (synthetic Expose / Phase B).** Backing-store mitigation from
  commit `93b988a` covers the smoke pass; not exercised heavily
  enough to need synthetic Expose in 3.3. Defer until a real
  validation scenario demonstrates a backing-store gap.
- **e16 RENDER coverage audit.** Audit instrumentation deferred —
  e16 doesn't reach a stable rendering state under ynest right
  now, so a 60-second e16 audit run isn't actionable. Re-open
  after the e16 startup regression is fixed.
- **Input-shape hit testing in the pointer pump.** Already
  deferred from Phase 3.2; still deferred.

## Phase 3.5 — Extension completion (MIT-SHM, DAMAGE, COMPOSITE, RENDER, GC clip-mask) ✓ COMPLETE

Goal: land four extension-completion items unblocking real WM /
desktop / compositor workloads — MIT-SHM v1.2, DAMAGE
auto-accumulation, COMPOSITE `NameWindowPixmap`, and the missing
RENDER `ChangePicture` XID-attribute paths. Plus an out-of-scope
GC clip-mask pixmap forwarding fix that surfaced during validation
and a host `PutImage` chunking fix for >256 KB images. Design doc:
`docs/superpowers/specs/2026-05-01-phase3-5-extension-completion-design.md`.

### Landed

- [x] **MIT-SHM v1.2 (full).** New `unix_fd` module wrapping
      `recvmsg(SCM_RIGHTS)` + an `FdReader` adapter so the X11 byte
      reader can pull file descriptors out of the connection alongside
      the protocol stream. Implemented `AttachFd`, `Detach`,
      `CreatePixmap`, `PutImage`, `GetImage`, the legacy SysV `Attach`
      (minor 1, used by libwraster and older toolkits), and
      `CreateSegment` (minor 7, server-allocated `memfd` returned via
      SCM_RIGHTS in the reply). Depth-1 `ZPixmap` stride math handles
      the 24×24 / 40×24 / 48×48 d1 alpha masks wmaker uses for appicon
      compositing. wmaker appicons now show their xterm / xclock icon
      graphic (the smoking-gun fix from Phase 3.4 follow-ups).
- [x] **DAMAGE auto-accumulation.** `accumulate_damage` /
      `accumulate_damage_full` helpers fire `DamageNotify` at most once
      per `Subtract` cycle (level 1, 2, 3); `Subtract` resets
      `pending_notify_fired`. Wired into `PutImage`, `CopyArea`,
      `CopyPlane`, `ClearArea`, MIT-SHM `PutImage` (exact rect) and
      `PolyPoint`, `PolyLine`, `PolySegment`, `PolyRectangle`,
      `PolyArc`, `FillPoly`, `PolyFillRectangle`, `PolyFillArc`,
      `PolyText8/16`, `ImageText8/16` (full drawable rect —
      conservative). Compositors now receive `DamageNotify` whenever
      the client draws.
- [x] **COMPOSITE `NameWindowPixmap`.** Forwards to host instead of
      returning `BadMatch`; host COMPOSITE major opcode is probed at
      `HostX11` init. `Vec<NamedCompositePixmap>` per `Window` tracks
      multiple aliases; resize / destroy / `DestroySubwindows` free
      every alias both locally and on the host. Returns `BadAlloc` if
      the host lacks COMPOSITE (cleaner than fake aliases that break
      pixmap-only request paths).
- [x] **RENDER `ChangePicture` XID translation.** `CPClipMask` /
      `CPAlphaMap` pixmap XIDs are now translated to host XIDs (was
      silently dropped before). wmaker close-button mask + GTK font
      rendering now land correctly on the host.
- [x] **GC clip-mask pixmap forwarding.** `ChangeGC` `clip_mask =
      Pixmap` is now forwarded to the host's shared GC via a new
      `HostClipState::Pixmap` state machine; `SetClipRectangles`
      supersedes any prior clip-mask and vice versa per the X11 spec.
      Without this, wmaker's close-button "X" / miniaturise dot
      symbols vanished — the depth-1 mask never landed on the host.
      MIT-SHM `PutImage` and the synthetic `put_image` used by MIT-SHM
      `CreatePixmap` now clear the host clip before uploading, so a
      clip-mask left over from an unrelated draw doesn't restrict the
      image upload.
- [x] **Host `PutImage` chunking.** Standard `XPutImage` has a 16-bit
      length field (max body ≈262 KB). e16's root background was an
      800×600 d24 image (≈1.9 MB) which overflowed the length field
      and made `HostX11::put_image` disconnect the client. Split by
      row count to stay under the limit. (Xephyr avoids this by
      attaching its image to a host shm segment and using
      `XShmPutImage`; we don't share memory with the host yet, so
      chunking is the local equivalent.)

### Validation

- 174 tests passing (added 12 across MIT-SHM, RENDER, DAMAGE,
  COMPOSITE, GC clip-mask).
- wmaker + xterm: appicon, title bar with miniaturise + close
  buttons, fish welcome banner all visible. Appicon icon graphic
  is now correct (was empty in Phase 3.4).
- e16 starts and runs.
- gtk3-demo, Java Swing, picom unaffected.

### Phase 3.5 follow-ups

- **RENDER coverage audit on real e16 / GTK3 clients.** Only the
  two known XID-attribute drops are fixed; anything else surfacing
  during validation rolls into Phase 3.6.
- **Damage on RENDER drawing ops.** First-cut accumulator covers
  core drawing only. RENDER-driven damage is a follow-up if a real
  client (compositor or screen recorder) needs it.
- **MIT-SHM `XShmPutImage` host fast path.** We chunk regular
  `PutImage` instead of sharing memory with the host. Revisit if
  large-image upload latency becomes a bottleneck.

## Phase 3.6 — Sub-window mirroring (Xnest model) (complete)

Goal: mirror every InputOutput client window to a host X window
parented under the client-side parent's host xid (Xnest's
`Window.c` model), replacing the prior "top-levels host-backed,
sub-windows virtual + draws translated by `(x_offset, y_offset)`"
hack. Closes the long-tail of correctness gaps that the offset-
hack created: sibling clipping is fictional, sub-window stacking
ordered, `CopyArea` from a partially-occluded source returns
wrong pixels, per-sub-window cursors don't work, every drawing
handler carries the offset translation, etc.

Plan: `docs/superpowers/plans/2026-05-02-phase3-6-plan.md`.
Design: `docs/superpowers/specs/2026-05-02-phase3-6-design.md`.

### Landed

- [x] **Step 1a — `restack_window` stack-mode semantics.**
      Pre-existing bug surfaced by codex review on the plan:
      `TopIf` / `BottomIf` / `Opposite` were treated as unconditional
      raise / lower / push-to-top. Fixed via a `RestackAction`
      resolver applying X11's spec-correct conditional occlusion
      checks (mapped + bbox overlap + correct stack position).
      +17 unit tests covering all 5 modes × {sibling, no-sibling}.
- [x] **Step 1b — minimal visual table + CreateWindow visual /
      colormap rule.** Added `Visual` and `Colormap` resource types,
      seeded with root TrueColor and ARGB TrueColor visuals; ARGB
      visual probed from host setup reply, ARGB colormap allocated
      on host at init. CreateWindow forwarding picks `CopyFromParent`
      when child visual matches parent and `CWColormap`+explicit
      visual otherwise. Unknown visual on `CreateWindow` rejected
      locally with `BadMatch` (no host roundtrip). CopyFromParent
      fixed to inherit `parent.visual`/`parent.depth` rather than
      always `ROOT_VISUAL` / 24.
- [x] **Step 2 — every InputOutput window gets a host xid (dormant).**
      `Window.host_xid` is now always Some for class != InputOnly.
      Sub-windows get host children created with `event_mask = 0`,
      bg-pixel forwarded; `border_pixmap_host_xid` field added,
      `uses_synthetic_expose` flag added. The post-CreateWindow
      `GetInputFocus` sync removed (re-introduced as a fence in
      Step 3+4b once we found the cross-connection race). Bug
      fixed in followup commit: the gate guarding host-xid
      allocation was `class == InputOutput`, missing the
      `CopyFromParent` class that wmaker/xterm/GTK pass at
      CreateWindow time; switched to `class != InputOnly` per the
      plan's invariant.
- [x] **Step 4a — host XReparentWindow + sub-window CWA bg +
      Configure border_width.** Forwarding infrastructure that
      Step 3 needed: ReparentWindow forwarded so the host tree
      mirrors local under WM frames, `ChangeWindowAttributes`
      forwards bg-pixmap / bg-pixel to sub-window host children,
      `ConfigureWindow` carries `border_width` alongside x/y/w/h.
- [x] **Step 3 + 4b combined — per-window drawing + host-driven
      Expose.** The plan's split between Step 3 (drawing rewire) and
      Step 4b (Expose source switch + map + stacking) couldn't be
      landed independently — the X11 model requires the drawing
      route, host-side mapping, and Expose source to flip together.
      `top_level_host_target` shortcuts to the start window's
      host_xid + zero offsets when one exists. Sub-window host
      children are mapped on host, registered with the input pump
      via a new `register_subwindow` (ExposureMask only — input
      bubbles up to the container per X11 propagation rules,
      Xnest `Window.c:91`). `uses_synthetic_expose` flips to
      `false` for every host-mirrored window after registration.
      Cross-connection sync fence (`sync_main_connection`) added
      after host CreateWindow so the pump's
      ChangeWindowAttributes(ExposureMask) can't race the main
      connection's CreateWindow under heavy WM activity. A leftover
      Phase-2 line in `resources::reparent_window` that cleared
      `host_xid` on reparent-away-from-root removed — that single
      line was defeating the per-window drawing route under any WM
      that reparents (i.e. all of them).
- [x] **Per-client keyboard pump fix.** Latent bug pre-dating
      Phase 3.6 that became user-visible only once interactive
      testing was the norm post-mirroring: X11 ButtonPress is
      exclusive (one client per window). The main `HostInputPump`
      already selected `POINTER_EVENT_MASK` (which includes
      ButtonPress) on the container; each per-client keyboard
      pump's `select_keyboard_events` tried to also include
      `POINTER_EVENT_MASK`, the host returned `BadAccess` for
      the entire `ChangeWindowAttributes`, the kb pump
      connection ended up with no mask at all, and keyboard
      input silently never arrived at any client. Fixed by
      having the kb pump select only `KeyPress | KeyRelease |
      StructureNotify` — pointer events stay on the main pump's
      connection where they belong.
- [x] **Step 6 — cleanup: drop the offset-translation machinery.**
      With every InputOutput window owning its own host xid (Step 2)
      and host-driven Expose (Step 3+4b), drawing handlers no longer
      need to translate (x, y) by an accumulated parent offset. Deleted:
      `top_level_host_target` + `TopLevelTarget` (the walk-up helper),
      the `(x_offset, y_offset)` fields on `HostDrawableTarget::Window`
      and `PictureState`, the `(x_off, y_off)` parameters on
      `apply_gc_clip` / `host_x11::set_clip_rectangles` /
      `host_x11::set_clip_pixmap`, the offset-translation helpers
      (`translate_i16`, `translate_i16_pair`, `translated_records`,
      `translated_points`, `translated_segments`, `translated_text_body`,
      `read_i16_from`, `write_i16_to`), and the
      `Window.uses_synthetic_expose` flag with its accompanying
      synthetic-Expose path on MapWindow (host pump is the sole Expose
      source now). All ~30 drawing call sites updated to pass raw
      coordinates / slices through to the host. Drawing on InputOnly
      drawables (which never had a host xid) still drops silently as
      before. Phase 3.7 (per-client GC) is unblocked.
- [x] **Step 5 — NameWindowPixmap retention across DestroyWindow.**
      Per the COMPOSITE spec, named pixmaps outlive the source
      window's destroy and remain valid until the client calls
      `FreePixmap`. The `DestroyWindow` (op 4) and
      `DestroySubwindows` (op 5) handlers in `nested.rs` no longer
      free the local Pixmap or the host pixmap for entries in
      `Window.composite_named_pixmaps`; the local Pixmap remains
      in `ResourceTable` with its `host_xid` intact and `FreePixmap`
      (op 54) cleans up on the eventual client request.
      Resize-driven invalidation (`invalidate_composite_named_pixmaps`
      from the `ConfigureWindow` resize path) still frees aliases —
      that's the spec-mandated case. The Step-2 BadValue gate
      rejecting `NameWindowPixmap` on mirrored sub-windows was
      lifted; sub-windows now use the same redirect-required +
      host-required validation as top-levels. Reparent already
      did not invalidate — added a regression test.
      Stress gate: 200 iterations of create + alias + destroy
      retain every Pixmap and panic-free.
      Generation-tagging / per-window-serialise host ops
      (originally listed in the plan for Step 5) was scoped down
      to "verify with stress test, host BadDrawable absorption is
      already in place" — no draw-after-destroy bug surfaced
      under the stress test, so no generation system this PR.

### Verified working

- xterm under wmaker: chrome + content area + shell prompt + typing.
- xterm without WM: full prompt rendered (was working pre-PR).
- xterm under fvwm3: chrome renders.
- xeyes / xclock: rendering unchanged.
- wmaker chrome: clip + dock + appicons + xterm titlebar.
- e16 startup: significantly more chrome renders than the Phase
  3.4 environmental baseline — top bar, two pagers, minimap.
- e16 popup *sometimes* opens, *sometimes* selects items (was
  100% broken before Phase 3.6). **Phase 3.7 below resolves
  these.**

## Phase 3.7 — Event-flow + popup rendering fixes (complete)

Goal: turn the partial e16 popup behaviour from Phase 3.6 into a
working popup → menu-item click flow. Six interlocking bugs surfaced
during e16 validation; commit `51afa21` lands them as a single squashed
change. Each was independently observable as part of the broken-popup
symptom but the dependency graph required the whole set.

### Landed (commit `51afa21`, 2026-05-02)

- [x] **Container POINTER_EVENT_MASK regression.** Phase-3.6 commit
      `1f43914` fixed the per-client kb pump's BadAccess by removing
      `POINTER_EVENT_MASK` from `select_keyboard_events`, but the
      same function was used by the main pump's
      `HostInputPump::open_from_env`. The container stopped receiving
      pointer events. Clicks on the desktop area where no top-level
      child intervenes (e16's first desktop = root with "Root-bg"
      cover) silently dropped on the host. Restore via dedicated
      `select_pointer_events_on_container` called only from the main
      pump.
- [x] **ButtonPress propagation stopped at top_level_id.**
      `pointer_event_fanout` walked `target → top_level_id` only,
      never reached root. e16's `Root-bg` full-screen child of root
      has only `EnterWindow` mask; per X11 spec the click must
      propagate to root where the WM listens. Add
      `ServerState::pointer_propagation_target` walking the parent
      chain to root with coord translation; honour from
      `ButtonPress` / `ButtonRelease` / `MotionNotify` dispatch.
      +4 unit tests.
- [x] **Missing CreateNotify / parent-side MapNotify /
      ConfigureNotify fanout.** Three handlers in `nested.rs` only
      fired StructureNotify on the window itself, not
      SubstructureNotify on the parent (`CreateNotify` never fired
      at all). e16's popup state machine couldn't proceed because
      it never got `MapNotify` for its own popups despite selecting
      `SubstructureNotify` on root. Add `encode_create_notify_event`;
      emit `CreateNotify` on `CreateWindow` and `SubstructureNotify`
      on parent from `MapWindow` / `MapSubwindows` / `ConfigureWindow`.
- [x] **Popup body rendered solid black.** e16 paints popup chrome
      via GCs with `fill-style=Tiled` and `tile=theme_pixmap`, then
      `PolyFillRectangle` onto a destination pixmap which is later
      `CopyArea`-sliced into per-menu-item bg-pixmaps. ynest's
      `CreateGcRequest` only parsed foreground/background/line_width/
      font/clip_mask — `fill_style` and `tile` were silently dropped.
      Parse `fill_style` + `tile` + `stipple` + `tile_x_origin` +
      `tile_y_origin`; resolve via new
      `ResourceTable::gc_fill_state` into a `GcFillState` enum; add
      `HostX11::set_gc_fill_tiled` / `set_gc_fill_solid`; wire
      `apply_gc_fill_state` into `PolyFillRectangle` /
      `PolyFillArc` / `FillPoly` with a reset to Solid after each
      draw so unrelated draws on the shared host GC don't inherit
      the tile.
- [x] **`SHAPE` `OP_SUBTRACT` collapsed regions.** `apply_shape_op`
      for `OP_SUBTRACT` returned either the unchanged current region
      or empty `Vec`, never doing actual region subtraction. Add
      `subtract_rect` (per-rect 4-strip split around the
      intersection) and `subtract_regions` (iterate source). +4 unit
      tests. The current e16 popup uses Set + Intersect so this
      doesn't visibly change those popups, but any client doing
      Set + Subtract for region arithmetic was being silently
      corrupted.
- [x] **Click on widgets in WM-managed windows didn't activate.**
      Two interlocked input bugs:
      - Grab path encoded `event_x`/`event_y` as raw `root_x`/
        `root_y` instead of grab-window-relative. Per X11 spec,
        when a pointer grab is active the event-window is the
        grab_window and `event_x`/`y` are relative to it; the grab
        owner uses these coords to locate the child widget that was
        clicked. Resolve via
        `ResourceTable::window_absolute_position`.
      - `ReparentWindow` handler unregistered the host_xid → nested
        mapping when a window left root, but didn't switch the host
        event-mask back to the sub-window default (`ExposureMask`).
        The host kept `POINTER_EVENT_MASK` selected on the now-sub-
        window so events still arrived on its host_xid; the empty
        `xid_map` entry then made `pointer_event_fanout` drop them
        silently. Result: clicks on every reparented child of root
        (e16 popup items, openbox/fvwm pager workspace cells,
        dialog OK buttons inside frames) were discarded. Switch
        `unregister_top_level` → `register_subwindow` which both
        updates the map AND CWAs the host mask back to
        `ExposureMask`, so events bubble up to the new top-level
        ancestor and route correctly.

**Validated** under e16 + ynest:99 + DISPLAY=:0 host clicks:
right-click on first desktop opens the popup menu (was 100% broken
on master), popup body renders with theme gradient + submenu
indicators (was 100% black), and clicking "Settings" opens the
Enlightenment Settings dialog (was 100% silent).

### Phase 3.7 follow-ups

- **Rounded corners cosmetic.** ynest's e16 popup outer shape is
  Set + Intersect (rectangular bounding) — the rounded look comes
  from the bg-pixmap content, with small black pixels at the very
  corners visible because the popup outer's bg isn't auto-filled
  there (default bg = None). Xephyr e16 sometimes uses a 14-rect
  staircase Set for rounded corners; both ynest and Xephyr show
  the same e16 binary running but the popup shape differs by run.
  Investigate why (e16 might inspect render extension caps or
  popup-size thresholds), and consider `ParentRelative` bg
  forwarding so the popup outer inherits its parent's bg colour
  instead of leaving uninitialised pixels.
- **Intermittent popup mapping.** Largely subsumed by the Phase 3.7
  fixes above (the "first desktop never works" was the
  POINTER_EVENT_MASK regression). The "second desktop works" was
  pure top-level subscription via Phase 3.6's `register_top_level`,
  bypassing the broken first-desk path. Re-evaluate after smoke
  testing under fvwm3 / wmaker.
- **Cross-connection sync fence is a design smell.** The pump and
  main connections race in several places; the per-CreateWindow
  fence is a duct-tape fix. Folding pump and main into one
  connection (or properly serialising via a message queue) is the
  structural fix.
- **Per-client GC mirroring** (originally Phase 3.7 plan, task
  #26). The shared host GC creates subtle bugs; per-client GC
  removes them. The Phase 3.7 fill-style fix proved this — adding
  state to the shared GC required careful reset-to-Solid after
  each draw to avoid leaking tile state between clients. A real
  per-client GC is cleaner.

## Phase 4 — Accelerated clients

Goal: modern GLX/EGL/Vulkan direct-rendering paths and host buffer
sharing. Validate real GPU-accelerated clients. (MIT-SHM landed in
Phase 3.5.)

### Phase 4.1 — Vulkan compositor on KMS (complete)

Replaced the pixman CPU compositor in `crates/yserver/src/kms/` with a
Vulkan compositor built on a per-window-texture scene graph. The
`pixman` crate is no longer in the workspace dep tree. Squash-merged
to master in commit `51e0612` on 2026-05-09.

Spec:
[`2026-05-07-phase4-1-vulkan-compositor-design.md`](superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md).
Plan:
[`2026-05-08-phase4-1-vulkan-compositor.md`](superpowers/plans/2026-05-08-phase4-1-vulkan-compositor.md).

#### Landed

- [x] **Sub-phase 4.1.1 — Vulkan plumbing, idle.** Workspace deps
      (`ash`, `gpu-allocator`, `gbm`, `thiserror`); `kms/vk/` module
      tree; `VkContext` (instance + physical device picker + logical
      device + debug messenger + drop order); wired into
      `KmsBackend::open_with_commit` as best-effort init (logs warning
      and falls back to pixman-only if no ICD). `just yserver-venus`
      recipe added; verified `vulkan initialised on physical device
      Virtio-GPU Venus (AMD Radeon 680M (RADV REMBRANDT))` under Venus
      passthrough; rendercheck `fill` 48/48 + `blend` 4/4 confirm
      pixman path unaffected. Validation layer + device-extension
      list both filtered to what the picked device actually supports.

- [x] **Sub-phase 4.1.2 — Vulkan-fed scanout (Tasks 2.1–2.8).**
      End-to-end Vulkan-fed atomic-fence scanout, opt-in via
      `YSERVER_VK_SCANOUT=pixman_shadow`. Default `Pixman` mode keeps
      the existing dumb-buffer path unchanged — parity bar (xts5,
      rendercheck) holds because no rendering code path was modified.

      Architecture: **Vulkan-first allocation** (originally GBM-first
      per the spec; pivoted because Polaris/RADV gfx8 lacks
      `VK_EXT_image_drm_format_modifier` and Mesa Venus can't
      dma-buf-import guest-allocated GBM bos via virtgpu cross-driver
      handle translation). Each `ScanoutBo` allocates a `VkImage`
      with `TILING_LINEAR` + `VkExportMemoryAllocateInfo(DMA_BUF)`,
      exports the bound memory via `vkGetMemoryFdKHR`, imports as a
      DRM GEM via `PRIME_FD_TO_HANDLE`, and registers as a DRM
      framebuffer with plain `add_fb2` (no MODIFIERS flag).

      Per-bo state machine: `Free → Recording → Submitted → Pending →
      OnScreen → Retiring → Free`. Atomic-commit explicit-fence flips
      via `submit_flip_with_fences` (IN_FENCE_FD plane property +
      OUT_FENCE_PTR CRTC property). Pageflip-complete events advance
      bo state; per-output skip when the previous flip is still
      pending (avoids -EBUSY thrash). Drain on shutdown via
      `ScanoutBoPool::drain_all_pending` (`vkDeviceWaitIdle` + close
      fence fds).

      Verified:
      - **Mesa Venus / RADV REMBRANDT** (vng harness): pool allocates
        cleanly, `present_frame_via_vulkan` runs end-to-end, atomic
        explicit-fence flips, pageflip-completes cycle bos through
        the state machine.
      - **Bare-metal AMD Radeon RX 580 (Polaris)**: dual-monitor
        5120×1440 setup, Vulkan-fed scanout active, cursor moves at
        vsync, no fallback warnings, kernel retires every flip
        (`pageflip-complete: first event` logged for both outputs).
      - **xts5 / rendercheck** in default Pixman mode: 100% pass on
        the smoke subset (fill 48/48, blend 4/4, bug7366 1/1) — Phase
        4.1 work is purely additive, parity bar unaffected.

      66 host-side unit tests covering bo state machine + 6-frame
      fence-cycle integration test (`scanout::tests::six_frames_cycle_through_pool_without_leaking_fences`).

- [x] **Sub-phase 4.1.3 — Per-window VkImage mirrors + scene-graph
      composite.** Architectural checkpoint per the plan: pixman
      scanout replaced by a Vulkan composite pass that walks the
      window tree and draws one textured quad per visible drawable
      sampling its `vk_mirror`. PixmanShadow bridge mode deleted;
      only `Pixman` (legacy fallback) and `VkComposite` (active)
      modes remain.

      Architecture:

      - **Per-drawable mirror.** Every `WindowState` / `PixmapState`
        / `CursorState` holds an `Option<DrawableImage>` containing
        a `VkImage` + `VkImageView`. Created at CreateWindow /
        CreatePixmap / cursor-create; reallocated on
        ConfigureWindow resize.
      - **Damage-driven upload (`MirrorUploader`).** Every
        `with_image_mut` site marks the mirror fully damaged.
        Pre-composite each frame, a single CB batches one
        `vkCmdCopyBufferToImage` per dirty mirror from the host
        pixman buffer into the mirror image (one submit + one
        `vkQueueWaitIdle` per frame, growable host-visible staging
        buffer).
      - **Composite pipeline (`pipeline.rs`).** GLSL textured-quad
        shaders compiled to SPIR-V at build time via `glslc`
        (build.rs); single graphics pipeline with src-over blend
        and dynamic viewport+scissor; 1024-set descriptor pool
        reset per frame.
      - **Composite pass
        (`compositor::record_and_present_composite`).** Scans bg
        pixmap (per-output slice of a virtual-screen-sized
        wallpaper), walks the window tree depth-first stacking
        order back-to-front emitting per-window quads, appends a
        cursor quad with src-over alpha. AABB cull skips windows
        whose absolute rect is entirely outside the output.
        UNDEFINED → COLOR_ATTACHMENT_OPTIMAL barrier, dynamic
        rendering with `loadOp=CLEAR` (bg color), per-draw
        bind+push+draw, end_rendering, COLOR_ATTACHMENT_OPTIMAL →
        GENERAL barrier, atomic flip with explicit IN_FENCE_FD /
        OUT_FENCE_PTR (same fence handshake as PixmanShadow).

      Verified bare-metal AMD Radeon RX 580 (Polaris) dual-monitor
      5120×1440 under wmaker + xterm: VkComposite enabled, both
      outputs paint cleanly, cursor tracks at vsync, mirror upload
      pipe runs without warnings, no fallback to pixman path.

      Carry-overs from 4.1.3's deferred list folded into 4.1.4 / 4.1.5:
      `bg_pixmap_image` rescue path was deleted with the rest of pixman
      in 4.1.5; xts5 + rendercheck full sweep ran as part of the
      conformance push. Scissor-on-`frame_damage`, occlusion cull, a
      lavapipe integration harness, and full SHAPE clipping remain
      open follow-ups (no longer blockers).

- [x] **Sub-phase 4.1.4 — Drawing-op family port.** Every drawing op
      writes to its `DrawableImage` mirror via Vulkan; pixman call
      sites deleted as each family landed.
      - **4.1.4.1 — Solid fill** via `vkCmdClearAttachments` /
        `record_fill_rectangles`.
      - **4.1.4.2 — `CopyArea`** via `vkCmdCopyImage`.
      - **4.1.4.3 — `PutImage` / `GetImage`** via `vkCmdCopyBuffer
        ToImage` / `vkCmdCopyImageToBuffer`.
      - **4.1.4.4 — Stroke / poly ops** routed through the fill
        pipeline.
      - **4.1.4.5 — Glyphs.** Glyph atlas + dedicated text-render
        pipeline.
      - **4.1.4.6 — RENDER `Composite` + `FillRectangles`.** Affine
        transforms, all four repeat modes, mask sources, dst-readback
        for Disjoint / Conjoint via per-format `DstReadback` scratch
        + shader-side blend, dual-source blend for `component_alpha`.
        Full PictOp enum (Saturate, Disjoint 16-27, Conjoint 32-43)
        backed by lazy per-`(op, dst_format, dst_has_alpha,
        component_alpha)` pipeline cache.
      - **4.1.4.7 — RENDER `Trapezoids` / `Triangles` /
        `CompositeGlyphs`.** CPU-rasterised mask composited through
        the existing Composite pipeline; `try_vk_render_traps_or_tris`
        resolves Drawable + Gradient sources (alpha-vs-no-alpha view
        selection mirrors the regular path), wires up dst-readback
        for Disjoint / Conjoint, and composites across the full dst
        for "zero source has effect" ops (Clear / Src / In /
        InReverse / Out / AtopReverse + Disjoint+Conjoint variants),
        matching pixman's `pixman-trap.c::get_trap_extents`.
      - **GC functions.** Xor / And / Or / Invert / etc. via a
        dedicated `VkLogicOp` pipeline.

- [x] **Sub-phase 4.1.5 — Pixman removal.** `WindowState.image`,
      `PixmapState.image`, `CursorState.image`, `bg_pixmap_image`,
      the `MirrorUploader` pump, `PixmanImage` newtype — all gone.
      `cpu_types::{Rectangle16, Repeat, PictTransform}` now own the
      X-protocol data types; the `pixman` crate is no longer in the
      workspace dep tree. ~926 lines of pixman-direct unit tests
      removed; remaining tests cover the Vk path.

#### Validation

Rendercheck (yserver in vng + lavapipe):

- `fill` / `dcoords` / `scoords` / `mcoords` / `tscoords` /
  `tmcoords` / `blend` / `repeat` / `bug7366`: 100%.
- `gradients`: 3649/3649. XRenderColor stops on
  `CreateLinearGradient` / `CreateRadialGradient` arrive STRAIGHT
  (per protocol + rendercheck `t_gradient.c:119-123`); the LUT fill
  premultiplies.
- `triangles`: 456/456.
- `composite` / `cacomposite`: correctness OK on the subsets that
  finish, but per-op `vkQueueSubmit2` + `vkQueueWaitIdle` pushes the
  full suite past the 600 s / 1800 s budgets. Batching is the open
  follow-up below.

xts5 protocol-test parity vs the pixman baseline (commit `108e0ad`):

| scenario | now PASS | baseline PASS | per-test PASS  |
|----------|---------:|--------------:|---------------:|
| Xproto   | 332      | 358           | 91.7% vs 92.0% |
| Xlib3    | 98       | 100           | 61.3% vs 64.1% |

FAIL / UNRES counts match the baseline exactly; the Xproto delta is
entirely UNTESTED tests the per-op cadence ran out of time on at the
same wall-clock budget.

### Phase 4.2 — DRI3 + Present + GLX (code complete; smoke deferred)

Wire surface and KMS-side machinery for accelerated direct-rendering
clients (vkcube, glxgears, glxinfo) on top of the Phase 4.1 Vulkan
compositor.

Spec:
[`2026-05-09-phase4-2-dri3-present-glx-design.md`](superpowers/specs/2026-05-09-phase4-2-dri3-present-glx-design.md).
Plan:
[`2026-05-09-phase4-2-dri3-present-glx.md`](superpowers/plans/2026-05-09-phase4-2-dri3-present-glx.md).

#### Sub-phase 4.2.1 — DRI3 wire surface + dma-buf import

Code-complete; live smoke deferred until vng + Venus run picks up the
new code path.

- [x] DRI3 v1.4 wire encoders / decoders
      (`yserver-protocol::x11::dri3`).
- [x] Extension registration with major opcode 147; capability-aware
      `extension_query_reply` / `advertised_extension_names` so DRI3
      hides when `Dri3Caps::version == (0, 0)`.
- [x] Render-node fd inventory at backend init via sysfs walk +
      `/dev/dri/renderD*` enumeration fallback (no hardcoded
      `/dev/dri/renderD128`).
- [x] `Backend::dri3_open` dups the render-node fd for each `Open`
      call; SCM_RIGHTS reply path verified.
- [x] `kms::vk::dri3::supported_modifiers` queries
      `vkGetPhysicalDeviceImageFormatProperties2` with chained
      `VkPhysicalDeviceExternalImageFormatInfo` and
      `VkPhysicalDeviceImageDrmFormatModifierInfoEXT`. LINEAR-only
      fallback when `VK_EXT_image_drm_format_modifier` is missing.
- [x] `DrawableImage::from_dmabuf` — explicit-modifier import chain;
      §3.2 fd-ownership rule (close on every error path between dup
      and successful `vkAllocateMemory`).
- [x] Dispatcher: `PixmapFromBuffer`, single-plane `PixmapFromBuffers`,
      `BufferFromPixmap` (export via `vkGetMemoryFdKHR`),
      `GetSupportedModifiers` (per-window vs per-screen split).
      Multi-plane `PixmapFromBuffers` and `BuffersFromPixmap` BadAlloc
      stubs per design scope.
- [x] Per-request capability gates: `fence_fd` /  `syncobj` cap unset
      → `BadImplementation` for `FenceFromFD`/`FDFromFence` /
      `ImportSyncobj`/`FreeSyncobj`. Real handlers arrive in 4.2.2.
- [x] fd-leak harness scaffold (`tests/dri3_fd_leak.rs`,
      `#[ignore]`-marked pending live ICD).

Smoke pending: a custom xcb client `PixmapFromBuffers`-ing a
checkerboard, then `xcb_copy_area`-ing to a window and asserting
pixel readback under vng + Venus.

#### Sub-phase 4.2.2 — XSync fences + DRI3 syncobj

Code-complete; live smoke deferred until vng + Venus run picks up the
new code path.

- [x] XSync handler audit
      ([note](superpowers/notes/2026-05-09-sync-audit.md)).
- [x] `Fence` resource lifecycle (CreateFence / DestroyFence /
      TriggerFence / ResetFence / QueryFence / AwaitFence) with a
      server-side triggered bit on `ServerState::sync_fences`.
      AwaitFence is a non-blocking stub for first cut.
- [x] `kms::vk::sync` — `import_sync_file` (binary semaphore from
      sync_file fd, TEMPORARY semantics), `import_drm_syncobj`
      (timeline semaphore from DRM_SYNCOBJ fd, PERMANENT), and
      `export_sync_file`. fd-ownership rule per design §3.2.
      `VK_KHR_timeline_semaphore` enabled at device init.
- [x] DRI3 `FenceFromFD` / `FDFromFence` round-trip: imports back the
      VkSemaphore stash on `KmsBackend::dri3_sync_resources`,
      dispatcher mirrors the fence onto the XSync resource table so
      `TriggerFence`/`QueryFence` keep working.
- [x] DRI3 `ImportSyncobj` / `FreeSyncobj` (gated on
      `Dri3Caps::syncobj`, currently false until the kernel-side
      DRM_SYNCOBJ ioctls are wired in 4.2.3+).

Smoke pending: two-client `FenceFromFD` import → `TriggerFence` →
`AwaitFence` round trip; `FenceFromFD` → `FDFromFence` export
round-trip with fd signalled in lockstep.

#### Sub-phase 4.2.3 — Present v1.4 Copy path

Code-complete; live smoke deferred until vng + Venus run picks up
the new code path.

- [x] `PresentPixmapSynced` v1.4 wire decoder (timeline syncobj +
      acquire/release values).
- [x] `PresentCaps` (`flip_path`, `async_may_tear`, `syncobj`) on
      Backend trait. `KmsBackend` overrides; conservative defaults
      until Tasks 30-32 wire alien-BO scanout. `QueryCapabilities`
      reply uses `PresentCaps::encode()`.
- [x] `present_scheduler::PresentScheduler` — per-window FIFO,
      `pick_at_vblank` collapses to latest + emits skipped tail,
      `record_flipped` for the presentproto retention rule, schedule
      predicate per §3.3.3.
- [x] `choose_path` selector per §3.3.1 decision table; flip_path
      cap short-circuits to Copy.
- [x] `IdleNotify` (32-byte) and `CompleteNotify` (40-byte) GE
      event encoders.
- [x] PIXMAP dispatcher enqueues + drains synchronously (live
      vblank-driven path lands with KMS integration); fans
      CompleteNotify { mode: Copy } / IdleNotify to all matching
      `present_event_selections`. AsyncMayTear silent-clear at
      ingress per design §4.

Smoke pending: `vkcube --present-mode fifo` rendering through the
Copy path under vng + Venus, with `RUST_LOG=present=debug`
confirming Copy is chosen and `IdleNotify` / `CompleteNotify` arrive
at the test client.

#### Sub-phase 4.2.4 — Flip + Direct-scanout

Wire surface and decision logic in place; live alien-BO scanout
integration with KMS atomic commit deferred until §5.5 hardware
coverage smoke picks it up.

- [x] `ScanoutBo::is_alien` flag and `ScanoutBoPool::register_alien`
      / `unregister_alien` shapes (live registration stubs Err today).
- [x] `choose_path` already covers Flip / DirectScanout decisions
      (Task 25); the dispatcher's defensive `alien_bo_for()`
      fallback selects Copy when registration isn't wired.
- [x] AsyncMayTear silent-clear at request ingress per design §4.
- [x] DestroyWindow teardown drains
      `state.present_scheduler.drain_window` for every destroyed
      window. No IdleNotify/CompleteNotify because the window is
      gone (design §3.3.2 row 1).

Smoke pending: `vkcube --present-mode mailbox` (Flip), fullscreen
`vkcube` (DirectScanout) under vng + Venus once the alien-BO
registration bridge from VkDeviceMemory → DRM GEM handle is wired.

#### Sub-phase 4.2.5 — GLX framing

Code complete. `glxinfo` should report `direct rendering: Yes`,
vendor `yserver`, and the §3.5 extensions list under vng + Venus.

- [x] GLX wire-protocol module — opcodes per `glxproto.h`,
      QueryVersion / QueryServerString / QueryExtensionsString /
      IsDirect / GetFBConfigs / GetVisualConfigs / context-lifecycle /
      VendorPrivate stubs / GLXBadRequest for indirect rendering.
- [x] Major opcode 148 (per Codex plan-review correction;
      upstream X.Org 149 collides with the local table).
- [x] `handle_glx_request` dispatcher per design §3.5
      (identification + bookkeeping; never executes GL).

Smoke pending: `glxinfo` reports `direct rendering: Yes` + vendor
`yserver`; `glxgears` runs at vsync under vng + Venus with DRI3 /
Present integration smoke from Sub-phases 4.2.1-4.2.4.

#### Phase 4.2 live-smoke (2026-05-09) — vkcube renders

`vkcube --c 30` under vng + Venus passthrough runs end-to-end:
30 `PRESENT::Pixmap` calls, clean exit (`rc=0`). Full code path:
DRI3 1.4 negotiation, real Venus modifier list (1 window + 7
screen mods at 1024×768 B8G8R8A8_UNORM), 3 dma-buf-backed
swapchain pixmaps (500×500 LINEAR), `IdleNotify` + `CompleteNotify`
fanout per frame.

**Key finding:** Mesa's `loader_dri3` uses **xshmfence**
(memfd-backed shared memory + futex) for `FenceFromFD`, *not*
sync_file or DRM_SYNCOBJ. `vkImportSemaphoreFdKHR(SYNC_FD_BIT)`
rejects the fd because it isn't a sync_file — Xorg's reference
goes through its internal `misync` layer. yserver now mmaps the
fd via `libxshmfence` (FFI in `crates/yserver/src/kms/xshmfence.rs`)
and calls `xshmfence_trigger` on `idle_fence` when the synchronous
CopyArea completes. That's the wakeup Mesa's WSI thread needs;
without it `vkAcquireNextImage` blocks on the futex forever.

The Vulkan-import path is kept as a fallback for clients that
genuinely send sync_file or DRM_SYNCOBJ fds.

#### Phase 4.2 deferred follow-ups

- **Live KMS Flip / DirectScanout integration** — the wire surface
  and dispatcher path-selection are in place; the alien-BO →
  framebuffer registration bridge (VkDeviceMemory → DRM GEM handle
  → `add_fb2`) lands when the §5.5 hardware coverage smoke picks up
  the new code path.
- **DRI3 syncobj support** — `Dri3Caps::syncobj` stays false until
  the kernel-side DRM_SYNCOBJ ioctls are wired through Present's
  submit-time handshake.
- **GetFBConfigs population** — ~30 (visual × double_buffered ×
  depth × stencil × samples) configs synthesised from the X visual
  table. Mesa survives an empty list today.
- **AwaitFence blocking semantics** — non-blocking stub; full
  implementation needs per-client wait queues hooked into the
  core-loop event scheduler.
- **`BuffersFromPixmap` (multi-plane export)** — BadAlloc stub
  (design §6 open question).
- **Multi-plane (YCbCr) `PixmapFromBuffers`** — out of scope per
  design §1; rejected with BadAlloc.
- **Indirect GLX** — out of scope per design §1; rejected with
  GLXBadRequest.

#### Phase 4.1 follow-ups

- **Composite / cacomposite batching.** Eager
  `vkQueueSubmit2` + `vkQueueWaitIdle` per RENDER op is the bottleneck;
  batching at the command-buffer level is needed before the full
  composite suite finishes inside the rendercheck budget.
- **Scissor on `frame_damage`** (originally plan 3.4 step 3). Requires
  per-frame damage tracking + bo-content preservation across the
  rotating 3-bo pool.
- **Occlusion cull** beyond AABB.
- **Lavapipe integration harness.** Pure-logic precedent doesn't
  compose with `DrawableImage`'s real `VkContext` requirement;
  bare-metal smoke is currently the load-bearing verification.
- **SHAPE clipping** beyond the empty-region skip.

## Phase 5 — Full desktop sessions

Goal: run Xfce, MATE, LXQt, and standalone WM sessions end to end.
Validate panels, launchers, notification daemons, clipboard managers,
screen lockers, global shortcuts, external compositors.

Not started.

## Phase 6 — Standalone DRM/KMS

Goal: replace the nested backend with a real backend on libinput, udev,
GBM, EGL/Vulkan, and atomic KMS. Hotplug, multi-monitor presentation,
session management, fullscreen / direct-scanout paths.

### Phase 6.1 — DRM/KMS bootstrap (complete)

Goal: replace the placeholder `crates/yserver/src/bin/yserver.rs` with
a real DRM/KMS binary that boots in `virtme-ng`, sets a mode on
virtio-gpu, runs a libinput-driven single-thread `epoll` loop, and
paints a moving rectangle into a dumb-buffer swapchain. Slice B of the
B → C trajectory; explicitly excludes any `yserver-core` integration.

Design:
[`2026-05-02-phase6-bootstrap-design.md`](superpowers/specs/2026-05-02-phase6-bootstrap-design.md).
Plan:
[`2026-05-02-phase6-bootstrap.md`](superpowers/plans/2026-05-02-phase6-bootstrap.md).

#### Landed (branch `phase6-bootstrap`)

- [x] **Step 1 — C-trait spike.** Read-only audit across
      `resources.rs` / `host_x11.rs` / `nested.rs` confirmed a future
      `Backend` trait can be carved out (~60–80 methods clustered into
      seven groups; 52 `host.lock()` call-sites, mostly uniform single-
      call leaves with a documented minority of multi-call transactions;
      16 host-XID slots in `resources.rs` for opaque-handle migration;
      `sync_main_connection` duct-tape dissolves for non-X-host
      backends). Verdict: **go for B**, with a 5-item C-prework reading
      list captured in the design doc.
- [x] **Step 2 — Workspace skeleton.** Added `drm = 0.15`,
      `input = 0.10`, `nix = 0.31` (event/fs/ioctl/mman/poll/signal),
      `signal-hook = 0.4`. `crates/yserver` now hosts a lib (`src/lib.rs`
      with `pub mod drm/input/present`) plus the existing `ynest` and
      `yserver` binaries. The yserver binary is now a thin shell calling
      `yserver::run()`.
- [x] **Step 3 — DRM Device + master.** `drm::Device` wraps
      `std::fs::File` for `/dev/dri/card0`, impls `drm::Device` +
      `drm::control::Device`, acquires master on open, releases on
      Drop. Distinct error messages per `io::ErrorKind` for
      NotFound / PermissionDenied / EBUSY.
- [x] **Step 4 — `pick_mode` policy (TDD).** Pure-logic: prefer the
      connector's preferred mode → fall back to 1024×768@60 → first
      → None. Four unit tests written first.
- [x] **Step 5 — Atomic modeset.** Enable `Atomic` +
      `UniversalPlanes` client capabilities. `discover_output`
      walks connector → encoder → CRTC → primary plane (via
      `possible_crtcs` filter and the "type" property).
      `dump_properties` debug-logs every property. `commit_modeset`
      builds an `AtomicModeReq` with name-resolved properties
      (CRTC_ID, MODE_ID blob, ACTIVE, FB_ID, SRC_*/CRTC_*) and
      commits with `ALLOW_MODESET`.
- [x] **Step 6 — Buffer wrapper.** RAII `drm::Buffer` owning a
      DumbBuffer + framebuffer + raw mmap (escapes `DumbMapping`'s
      lifetime via `mem::forget`, takes over the unmap). Drop order:
      destroy_framebuffer → munmap → destroy_dumb_buffer. Holds
      `Arc<Device>` so `Vec<Buffer>` works in `Swapchain`.
- [x] **Step 7 — Swapchain state machine (TDD).** Four-state enum
      `{Free, Acquired, Submitted, Scanout}` with `acquire / submit /
      complete` transitions. `with_initial_scanout(n, idx)` for the
      first-modeset case. `submitted_idx()` is the workaround for the
      drm 0.15 crate stripping the kernel `user_data` from
      `PageFlipEvent` — invariant: at most one buffer is `Submitted`
      at a time. Nine unit tests.
- [x] **Step 8 — Page-flip events.** `submit_flip` atomic-commits
      a new FB_ID on the primary plane (with unchanged CRTC_ID for
      the bind) and `PAGE_FLIP_EVENT | NONBLOCK`. `drain_events`
      reads `Device::receive_events()` and dispatches PageFlip
      completions to a closure. The flipped buffer's index is
      identified via `swapchain.submitted_idx()`.
- [x] **Step 9 — libinput.** `input::Context` wraps `input::Libinput`
      via udev seat0 with a `LibinputInterface` honouring the flags
      libinput requests. `dispatch()` translates keyboard / pointer
      motion / button events to a yserver-local `InputEvent` enum
      (keycodes only — no xkbcommon, that's C's job).
- [x] **Step 10 — Throwaway painter (TDD).** `present::State`
      (rect + cursor + velocity) + `update` advances state and
      bounces the rect off framebuffer edges, applies pointer motion
      to the cursor. `paint` writes a 60×60 magenta rect, 4×4 white
      cursor, dark-grey background into a `Buffer`. Three TDD tests
      cover velocity, edge bounce, cursor follow.
- [x] **Step 11 — Single-thread epoll loop.** `present::run_loop`
      uses `nix::sys::epoll::Epoll` over `[drm.fd, libinput.fd]`
      (signalfd added in Step 12). Always-animating: every flip
      completion computes `dt`, advances state, paints the next
      acquired buffer, submits the next flip. CPU is proportional
      to refresh rate.
- [x] **Step 12 — signalfd + clean shutdown.** SIGINT/SIGTERM
      blocked via `sigprocmask(SIG_BLOCK)` *before* any thread
      spawn so libinput's internal threads inherit the mask.
      `SignalFd` added to the epoll set as the third source. On
      signal: log, set `running = false`, fall through to break.
      After loop exit: `disable_output` atomic-commits FB_ID=0,
      CRTC_ID=0 on the plane, ACTIVE=0 + MODE_ID=0 on the CRTC,
      CRTC_ID=0 on the connector. RAII unwinds: swapchain → device.

#### Validation

End-to-end vng-headless smoke (`just yserver-headless` and
`just yserver-headless-shutdown`):

- DRM open + master acquire + atomic capabilities ✓
- Connector discovery: virtio-gpu's `Virtual-1` connected;
  `pick_mode` selects the preferred 1280×800@75 ✓
- Modeset commit: kernel accepts the full `AtomicModeReq`
  (connector + CRTC + plane + FB + mode blob) ✓
- Page-flip cadence ≈ refresh rate: 217 flips in 3.0 s = 72.3 Hz
  on a 75 Hz mode (within 4%), 19 flips in 250 ms = 76 Hz over
  short windows; well within the plan's ±10% gate ✓
- Clean shutdown: SIGTERM observed via signalfd, loop exits,
  `disable_output` succeeds, master released ✓
- libinput attaches to seat0 inside the guest (input event
  count is 0 in headless because no QEMU input source) ✓

Re-run idempotence: the second back-to-back run succeeds without
EBUSY on master acquire (the explicit `disable_output` +
RAII Drop on Buffer + Drop on Device clear the prior state).

Static gates: `cargo build --bin yserver` green; `cargo clippy
-p yserver --all-targets` clean (one
`#[allow(clippy::too_many_arguments)]` on `run_loop`); `cargo
test -p yserver --lib` 16 passing (4 pick_mode + 9 swapchain +
3 painter).

Code review via codex on Steps 1, 5, 8, 12 (the design-heavy
ones). Review pass on Step 1 surfaced three real issues (call-site
uniformity overstated, missing resource-coupling rows, hidden
two-phase allocation pattern) that were folded back in. Reviews
on 5/8/12 found no blocking issues.

The bwrap sandbox running this Claude Code session has no
working virtio-gpu inside vng (different from the host shell);
all vng smokes were run by the human on the host outside bwrap.

#### Phase 6.1 follow-ups (out of scope for the bootstrap)

Explicitly deferred — each is a self-contained Phase 6.x slice:

- **Hotplug.** No connector-add / -remove handling. Single
  output, single mode, modeset at startup only.
- **Multi-output / multi-plane.** `discover_output` picks the
  first connected connector and walks to one CRTC + one primary
  plane. Overlay/cursor planes ignored.
- **GBM / EGL / GLES / Vulkan.** Dumb buffers + CPU painting
  is the entire render path. GL is a Phase 6.x optimization.
- **logind / VT switching / suspend-resume / console restore.**
  Not implemented; B is vng-only by design and bare-metal is a
  Phase 6.x slice.
- **Bare-metal targets** (real Intel/AMD/NVIDIA on the
  CachyOS host). The core DRM/KMS path is **validated on bare metal**
  on the CachyOS host: with `YSERVER_DRM_DEVICE` auto-probing
  card0/card1, `sudo target/release/yserver` paints the moving
  rectangle and shuts down cleanly. Caveat: the active VT's master
  holder must be released first — on this host kmscon (the userspace
  console renderer) holds master per active VT, so
  `sudo systemctl stop kmsconvt@ttyN.service` for the current VT is
  required before yserver can take master. The polished case
  (logind handoff + VT_SETMODE coordination so yserver and kmscon
  can coexist on different VTs) remains Phase 6.x.
- **xkbcommon.** Keycodes are emitted as raw Linux input
  keycodes; keysym translation is C's responsibility.
- **`yserver-core` integration.** Slice B explicitly excludes
  any X11 protocol code. The `Backend` trait extraction +
  C-prework items captured in the design doc are slice C.
- **Bigger validation surface.** `just yserver` is now validated
  graphically. Root cause of the previous exit 255: `vng
  --graphics` starts a guest Xorg session and runs the payload as
  the host user (`jos`), so `yserver` failed to acquire DRM master
  on `/dev/dri/card0` with `EACCES`. The graphical recipes now use
  normal vng execution (payload stays root) plus explicit QEMU
  options `-display gtk -vga none -device virtio-gpu-pci`, avoiding
  the guest-Xorg wrapper while still opening a QEMU window.
  Validated: `just yserver` shows the moving rectangle in the QEMU
  window; a 3-second auto-shutdown smoke produced 177 flips and
  clean signalfd shutdown.

### Phase 6.2 — Backend trait extraction (complete)

Goal: carve a `Backend` trait out of `yserver-core` so request
handlers call into it (via `Arc<Mutex<dyn Backend>>` for the hot
path) and a future KMS backend slots in. Lands three of the five
C-prework items from the 6.1 design. Pump construction sites
(`HostInputPump` and per-client kb pumps) keep a separate concrete
`Arc<Mutex<HostX11Backend>>` clone via natural unsized coercion
(no downcast helper). Pump/main connection merge is deferred to
its own slice.

Design:
[`2026-05-03-phase6-2-backend-trait-design.md`](superpowers/specs/2026-05-03-phase6-2-backend-trait-design.md).
Plan:
[`2026-05-03-phase6-2-backend-trait.md`](superpowers/plans/2026-05-03-phase6-2-backend-trait.md).
Pre-step audit:
[`2026-05-03-phase6-2-host-surface-audit.md`](superpowers/notes/2026-05-03-phase6-2-host-surface-audit.md).

#### Landed (branch `phase6-2-backend-trait`)

- [x] **Step 0 — Host-X11 surface audit.** Concrete enumeration of
      every `host.X` / `h.X` / `hh.X` / inline-locked call in
      nested.rs and server.rs, plus every host_xid-bearing field
      in resources.rs. Source of truth for Steps 1, 2, 3, 5.
- [x] **Step 1 — Per-kind handle newtypes.** 8 newtypes
      (`WindowHandle`, `PixmapHandle`, `PictureHandle`,
      `GlyphSetHandle`, `FontHandle`, `CursorHandle`,
      `ColormapHandle`, `VisualHandle`) plus `AnyHandle` for
      drawables. Replaces 18 host_xid slots (8 required, 9
      optional, 1 map key) across resources.rs. ~280 call sites
      adjusted.
- [x] **Step 2 — Bundle `allocate_xid` into `create_*`.** 11
      creator methods refactored to atomic create-returns-handle
      (create_subwindow, create_pixmap, create_cursor, open_font,
      name_window_pixmap, render_create_picture/glyphset/solid_fill/
      linear_gradient/radial_gradient/cursor). `next_xid` private to
      host_x11 module.
- [x] **Step 3 — `Gc` expansion + `DrawState` resolution.** `Gc`
      gains 11 fields (line_style, cap_style, join_style,
      fill_rule, function, plane_mask, subwindow_mode,
      graphics_exposures, dashes, dash_offset, arc_mode) — additive
      scope. CreateGC/ChangeGC parsers handle all 23 mask bits.
      CopyGC handler updated. `ResourceTable::resolve_draw_state`
      computes a `DrawState` snapshot with graceful degradation on
      missing pixmaps. 15 drawing call sites refactored to resolve
      once + apply_draw_state. The new GC fields are forwarded to
      the host's shared GC (line_style / function / plane_mask /
      arc_mode etc. now honored — was silently dropped before).
- [x] **Step 4 — Module split.** `host_x11.rs` (3,889 lines) →
      `host_x11/{mod, request, pump}.rs`. (No `sync.rs` — sync
      logic woven through several request methods, kept in
      `mod.rs`.)
- [x] **Step 5 — `Backend` trait carve.** ~80-method trait surface
      enumerated from the audit. `HostX11Backend` is the sole impl.
      `nested.rs` request-handler hot path uses
      `Arc<Mutex<dyn Backend>>`. Pump-construction sites hold a
      separate concrete-typed `Arc<Mutex<HostX11Backend>>` clone
      alongside the dyn one (natural unsized coercion at the
      `Arc::clone` site — no downcast helper needed).
      `RecordingBackend` test double + 4 integration tests proving
      the trait is implementable by something other than
      `HostX11Backend`. **Sink wiring deferred** —
      `register_event_sink` / `BackendEventSink` / sink-routing
      moves to its own follow-up.

#### Validation (Step 6)

End-to-end manual smoke against the Phase 3.x WM matrix on the
trait-carved ynest. 341 tests passing
(16 yserver + 9 ynest + 229 yserver-core + 87 yserver-protocol).

WMs available in this environment: wmaker, fvwm3, openbox.
Not installed (skipped, not regressions): enlightenment-16,
gtk3-demo. (gtk-demo is gtk4 only on the host and exits silently
when nested — pre-existing gap, unrelated to Phase 6.2.)

- **wmaker** (`/tmp/wmaker-smoke.png`): chrome + clip + dock +
  appicons render. xterm/xclock/xeyes appicons show correct icon
  graphics (xterm "X" colour icon visible). Close buttons on
  title bars. xclock title bar text via RENDER intact. ynest log
  WARN/ERROR count: 0.
- **fvwm3** (`/tmp/fvwm3-smoke2.png`): chrome renders. xclock
  title bar text via RENDER intact. FVWM right panel
  (Pager / IconMan / FvwmScript-DateTime "15:30 zo mei 03")
  renders. xterm framed with title bar. ynest log WARN/ERROR
  count: 0. Programmatic widget click not exercised — XTEST
  extension is unimplemented in ynest, so xdotool can't drive
  clicks; the Phase 3.7 input-routing fix is structurally in
  place (event masks intact in carved trait surface, pump
  threads still attached), but verifying it requires a manual
  click. gtk-demo (gtk4) starts but exits silently in this
  environment regardless of WM — pre-existing limitation.
- **e16** (skipped): `enlightenment-16` not installed.
- **openbox** (`/tmp/openbox-smoke.png`): clients render inside
  openbox frames with correct title bars (xclock, xeyes labels +
  buttons). The "openbox frame chrome is a known pre-existing
  gap" caveat in the plan turned out to be obsolete — frame
  chrome renders correctly. ynest log WARN/ERROR count: 0.
- **gtk3-demo** (skipped): gtk3-demo binary not installed; only
  gtk4's `gtk-demo` is available, and it exits silently when
  launched headlessly regardless of which X server it targets
  (reproduces against host `:0` too).

No regressions surfaced against the Phase 3.x baseline. ynest
ran cleanly across all three WM smokes — zero log lines at
`RUST_LOG=warn`. The structural decoupling (newtypes / atomic
creates / DrawState / module split / dyn trait) introduced no
observable behavioural change.

### Phase 6.3 — Pump / sink rework (complete)

Goal: fold today's three host X11 connections (main + `HostInputPump`
+ per-client kb pumps) into one, behind a clean
`Backend::dispatch()` / `BackendEventSink` shape on the trait. This
is Phase 3.7's "cross-connection sync is a design smell" follow-up
and Phase 6.2's deferred prework item #5, designed as one slice
because the pieces are mutually load-bearing.

Scope:

- **Single-X11-connection merge.** `HostX11Backend` owns one X11
  connection; the pump and per-client kb pump connections fold in.
  Eliminates the cross-connection race that motivated
  `sync_main_connection` and `reply_buffer`.
- **`fd()` / `dispatch()` / `drain_events()` on the `Backend` trait.**
  Replaces the `register_event_sink` deferral from Phase 6.2's
  Step 5. Backend exposes a readiness fd; `yserver-core`'s main
  loop epolls it; `dispatch()` drains and classifies bytes into
  replies (resolved against an internal `ReplyMap`) and events
  (queued for `drain_events(sink)`).
- **`BackendEventSink` trait + sink-routing.** Implementation in
  `yserver-core` wraps the existing per-client fanout
  (`pointer_event_fanout`, `expose_event_fanout`, etc.). Backend's
  internal threading feeds the sink; `nested.rs` event handlers
  are unchanged.
- **Per-client kb pump dissolution.** Each kb pump's `client_id` /
  `focused_window` / writer / sequence state moves into the central
  fanout that the sink drives. Eliminates the per-client thread.
- **Pump-construction migration to `dyn Backend`.** Today's
  separate concrete-typed `Arc<Mutex<HostX11Backend>>` clone for
  pump construction goes away — pump becomes a backend internal
  detail, not a `nested.rs` responsibility.
- **64-bit `seq_full` tracking + wrap retention window.** X11
  wire seq is 16-bit and wraps every 65,536 requests. The reply
  demux tracks `seq_full: u64` with extended-sequence promotion
  on incoming wire seq; void-request errors retain a window of
  ~50,000 sequences before being dropped to a debug log.
- **`OriginContext` plumbing for async host-error attribution.**
  Today every host error arrives synchronously on the calling
  request handler's call stack; after the merge, void-request
  errors arrive later and need attribution back to (client_id,
  nested_seq, nested_opcode). Each trait method that may produce
  a host error takes an `OriginContext`; the backend stores it
  alongside the seq_full and includes it in delivered errors.

Tests: explicit unit coverage for partial reads, sequence wrap,
late errors, `ListFontsWithInfo` multi-reply with interleaved
events, role-transitions on reparent, create-then-register
ordering. The unit test bench is the safety net for the merge
mechanic — manual smoke alone is insufficient.

#### Landed (branch `feature/phase-6-3-pump-sink-rework`)

- **Step 1 — `BackendEventSink` + `OriginContext` plumbing.**
  `OriginContext` threads through the `Backend` trait,
  `HostX11Backend`'s trait impl, and every `nested.rs` host-call
  path that issues a request on behalf of a client. Each trait
  method that may produce an async host error now takes
  `Option<OriginContext>`; the backend snapshots it onto the
  issued sequence so a late error can be logged with the
  originating `(client_id, nested_seq, opcode)` instead of a bare
  host error code.
- **Step 2 — `SequenceMap` + background dispatcher.**
  `host_x11::sequence_map::SequenceMap<T>` provides a sliding-
  window store keyed on promoted 64-bit sequences. The
  `hostx11-dispatch` thread owns a clone of the host stream's read
  half and is the *only* reader after `open_from_env` returns. It
  decodes replies / errors / events into a `HostMessage` enum and
  fans them out to `pending_replies` / `pending_errors` (replies)
  and the crossbeam `BackendEvent` channel (events). 16→64
  sequence promotion is anchored on a `seq_full_atomic` mirror so
  the dispatcher reads it without contending with the Backend
  mutex.
- **Step 3 — `wait_for_reply` Condvar pathway.** `PendingReplies`
  and `PendingErrors` are `Condvar`-guarded sliding maps; reply
  consumers block on the Condvar instead of draining the stream
  themselves. The Backend mutex stays locked while a waiter
  blocks — safe because the dispatcher only ever locks
  `PendingReplies::state`, never the Backend mutex. Synchronous
  `read_until_response` is preserved only for the init phase
  (`init_render` / `init_xkb` / `query_extension_opcode`) where
  the dispatcher hasn't been spawned yet.
- **Step 4 — connection merge ("Big Flip").** `HostInputPump` and
  per-client keyboard pumps are deleted. The container window
  selects a unioned `CONTAINER_EVENT_MASK` (KeyPress | KeyRelease
  | ButtonPress | ButtonRelease | Enter | Leave | PointerMotion |
  Exposure | StructureNotify) at create time so the merged
  dispatcher sees every event class on one connection. Per-client
  keyboard forwarders now read from a crossbeam channel fed by
  the dispatcher; each forwarder applies its own focus state on
  the events it receives. `register_top_level` /
  `register_subwindow` / `unregister_host_window` migrated onto
  the `Backend` trait — same wire side-effects, just on the
  merged main connection.
- **Step 5 — sink integration.** `host_pump_event_sink` lives in
  `server.rs` and wraps the existing pointer / expose / configure
  fan-outs as a `BackendEventSink`. The backend dispatcher feeds
  it via the `BackendEvent` channel; a `hostx11-sink` consumer
  thread (spawned by `set_event_sink`) drains the channel and
  drives the sink without touching the Backend mutex.
- **Step 6 — cleanup.** `sync_main_connection` removed (the
  cross-connection fence it implemented is unnecessary now that
  `CreateWindow` and the follow-up event-mask write travel on the
  same socket). `HostInputPumpHandle` removed: each `nested.rs`
  call site uses `Backend::register_top_level` /
  `register_subwindow` / `unregister_host_window` directly. The
  legacy `reply_buffer` codepath is gone; `read_until_response` /
  `stash_or_log_response` survive only as init-phase synchronous
  fallbacks (documented in `mod.rs`). Async errors on the merged
  connection log with `OriginContext` and emit
  `BackendEvent::HostError` for the sink.

#### Validation (Step 6)

End-to-end manual smoke against the Phase 3.x WM matrix on the
merged-connection runtime. 360 tests passing
(16 yserver + 9 ynest + 248 yserver-core + 87 yserver-protocol).

WMs available in this environment: wmaker, fvwm3, openbox.
Not installed (skipped, not regressions): enlightenment-16,
gtk3-demo.

- **wmaker** (`/tmp/phase6-3-wmaker.png`): chrome + clip + dock +
  appicons render. xterm with title bar shows the prompt cleanly;
  xclock with title bar + analog face visible. xterm/xclock
  appicons populate the dock. ynest log: zero panics, zero
  ERRORs; the WARNs are async host errors with attached
  `OriginContext` (the post-Phase-6.3 attribution path), all from
  XFIXES probes wmaker fires at startup — pre-existing condition,
  not a regression.
- **fvwm3** (`/tmp/phase6-3-fvwm3.png`): chrome renders. FVWM
  pager + IconMan + FvwmScript-DateTime ("23:25 Sun May 03")
  visible in the right panel. xclock with title bar text visible
  top-left. Zero panics, zero ERRORs.
- **openbox** (`/tmp/phase6-3-openbox.png`): xclock and xeyes
  render inside openbox frames with three-button title bars and
  correct widget chrome. Zero panics, zero ERRORs.

Programmatic widget click / xdotool key-synthesis is *not*
exercised — bwrap sandbox doesn't expose XTEST, so input-event
synthesis isn't possible. The merged-connection runtime is
validated structurally (ynest accepts clients, dispatcher is
alive, sink consumer drains BackendEvents, register/unregister
through the trait propagates to the host correctly, no
deadlocks). The canonical "type into xterm under wmaker" check
is left for the user to verify on real hardware. Residual risk:
keyboard fanout regression that only manifests under live
keypresses would slip past this gate; the unit-test bench covers
sequence wrap, late errors, multi-reply interleaving, and
create-then-register ordering, but not the full focus-routing
loop.

No regressions surfaced against the Phase 6.2 baseline. The
warning counts and screenshot quality match what 6.2 produced
under the dual-connection topology.

### Phase 6.4 — KMS backend + `yserver` integration (complete)

Goal: implement a `KmsBackend` impl of the `Backend` trait and
wire `yserver-core` into the `yserver` binary. First real X11
client (xterm, xeyes) running on bare DRM/KMS — the headline
deliverable of the Phase 6 trajectory.

Scope (minimum viable):

- **`KmsBackend` impl of the `Backend` trait.** Phase 6.1's DRM
  module (modeset, swapchain, dumb buffers, page-flip events,
  libinput) reshaped to satisfy the trait. Drawing methods
  rasterize directly into the dumb-buffer scanout (Pixman or
  hand-rolled) instead of forwarding XPolyLine etc. to a host X
  server. Event delivery feeds Phase 6.3's `BackendEventSink`
  from libinput input + DRM page-flip + signalfd.
- **Wire `yserver-core` into the `yserver` binary.** `lib.rs::run()`
  no longer constructs a throwaway `present::State` + bouncing
  rectangle. It constructs a `KmsBackend`, hands it to
  `yserver_core` as `Arc<Mutex<dyn Backend>>`, and calls the
  client-listener entry point. Listens on
  `/tmp/.X11-unix/X<display>` like `ynest`.
- **First X client on KMS.** `DISPLAY=:N xterm` or
  `DISPLAY=:N xeyes` against the running yserver, painting on
  bare metal. The success criterion that distinguishes Phase 6.4
  from "still the bouncing square."
- **xkbcommon.** Keymap translation from Linux input keycodes
  to X11 keysyms. Phase 6.1 deferred this; Phase 6.4 needs it
  for any real client to receive sensible keyboard input.

Out of scope (deferred to Phase 6.5+):

- Full WM session under yserver (wmaker/fvwm3/e16 etc.) — likely
  works once the basics are right, but validation is its own
  slice.
- VT_SETMODE coordination so `yserver` and `kmscon` (or any other
  DRM master) can coexist on different VTs. Today's bare-metal
  recipe is "stop the conflicting master first."
- logind handoff so non-root users can run `yserver` on their
  active session without `sudo`.
- Suspend / resume.
- Hotplug response.
- Multi-output / multi-plane.
- GBM / EGL / GLES / Vulkan render path.
- Bare-metal validation of the polished case (kmscon coexistence,
  logind handoff, console restore on exit). Today's bare-metal
  pass requires stopping kmscon on the target VT and accepting
  no console restore — Phase 6.1 follow-ups note above documents
  the recipe.

#### Landed (commit `7763055`)

- **Steps 0–6 — `KmsBackend` bring-up.** Scaffolding + deps,
  pixman rasterisation of core drawing primitives, swapchain +
  per-window image compositor, libinput / xkbcommon input
  pumping, freetype font loading + eager glyph cache, and the
  epoll event loop wiring `yserver-core` onto a bare DRM/KMS
  surface. The `Backend` trait shape from Phase 6.2 carried
  through unchanged — `nested.rs` is backend-agnostic, the
  `yserver` binary swaps in `KmsBackend` where `ynest` swaps in
  `HostX11Backend`.
- **Pixman bring-up fixes.** `compute_font_metrics` ascent /
  right-side-bearing sentinels were inverted (text background
  rects overflowed pixman). `poly_line` / `poly_segment` switched
  to i32 Bresenham (i16 subtract-then-bbox-rect overflowed in
  debug on extreme-coord probes from xterm and rendered diagonals
  as bounding boxes). `poly_fill_arc` / `poly_arc` got scanline
  ellipse fill / outline. `fill_poly` got even-odd scanline
  polygon fill. `create_subwindow` / `configure_subwindow` now
  pre-fill the new window image with `background_pixel` so
  clients that rely on auto-clear (xclock black-on-white) get
  the right backdrop.
- **Workarounds for surfaced quirks.** `pixman::fill_rectangles`
  segfaults on partly-out-of-bounds rects in our build —
  `clip_rects_to_image` works around it for the affected
  primitives. `list_fonts_proxy` /
  `list_fonts_with_info_proxy` synthesise a properly-formed
  empty / terminator reply so font-querying clients (xclock)
  don't block. `render_text_string` replaced an
  `as_ptr`-via-`RefCell` raw-pointer cast with a two-phase impl
  (collect glyphs while a `Ref` is held, composite after the
  borrow drops). Default mid-grey scanout when no root
  `bg_pixel` set. virtio-tablet absolute pointer support added
  via `InputEvent::PointerMotionAbsolute` +
  `process_pointer_absolute`.

#### Validation (commit `7763055`)

xeyes, xterm, and xclock connect, render, and respond to input
on bare DRM/KMS without a host X server. xeyes' pupils track
the cursor; xclock's analog face draws (the seconds hand is
invisible — known issue, GC `function` not honoured). xterm
runs without crashing and the white background is correct, but
glyph baseline is off (known issue).

WM-readiness on KMS is *not* in scope for 6.4 — that's the
6.5 deliverable. fvwm3 starts and reparents client windows but
modules wedge waiting for ConfigureNotify synthesis (see
`known-issues.md` "KMS backend (Phase 6.4)" and "fvwm3 modules
wedge on missing ConfigureNotify").

### Phase 6.5 — WM-readiness on KMS (complete)

Goal: make fvwm3 fully usable on the bare-metal `KmsBackend`
running standard X11 clients (xterm, xclock, xeyes). Achieved:
fvwm3 manages and frames clients on bare DRM/KMS without a host
X server; xclock renders its full anti-aliased dial and sweeping
seconds hand via RENDER trapezoids; xterm renders legible text.

#### Diagnostic discovery (Step 0 — pre-implementation trace diff)

The pre-implementation hypothesis from `known-issues.md` ("fvwm3
modules wedge on missing ConfigureNotify; fix is to synthesise
notify events from `KmsBackend::configure_subwindow`") turned out
to be *wrong*. `nested.rs` already synthesises ConfigureNotify
unconditionally for both backends at lines 5926–5962, plus the
SubstructureNotify variant on the parent. Trace-diff against
Xephyr-fvwm3.log showed the actual root cause: without RENDER,
fvwm3 builds a *two-level* frame hierarchy and places the
managed client at parent-relative `(0, 0)` inside the inner
frame. FvwmPager's init loop terminates only when `GetGeometry`
on its top-level returns `x != 0 || y != 0`; under Xephyr (with
RENDER) fvwm uses a single-level frame with non-zero placement
and the loop exits, but under bare KMS (RENDER absent) the loop
never terminates.

The full diagnostic note lives at
`docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md`.

#### Landed (branch `phase6-5-wm-readiness`)

- **Step 2 — GC `function` plumbing (`6972c39`).** Adds
  `current_function: GcFunction` on `KmsBackend`, captured in
  `apply_draw_state`, read by every client-draw primitive via a
  new `fill_rects_with_gc_function` helper. `GcFunction::Copy`
  → `pixman_op::SRC` (fast path); `GcFunction::Xor` →
  per-pixel bitwise XOR over the RGB channels via raw pixel
  manipulation. Pixman's `PIXMAN_OP_XOR` is *not* what X11
  `GXxor` requires (Porter-Duff `src×(1-dst.a)+dst×(1-src.a)`
  produces zero for fully-opaque images), so the Xor path uses
  manual XOR. All other GcFunction variants log-and-fall-back
  to `Src`. Unblocks xclock's seconds hand, will unblock WM
  rubber-band selection when the WMs that exercise it are
  smoke-tested.
- **Step 1A — RENDER advertise (`e19ca7c`).** Flips
  `KmsBackend::render_opcode()` from `None` to `Some(133)`. All
  21 `render_*` trait methods were already stubbed as no-ops, so
  RENDER capability is advertised without breaking any prior
  call site. This single line resolves the FvwmPager wedge by
  triggering fvwm's RENDER-aware single-level frame strategy.
- **Step 3 — xterm glyph rasterisation (`993c437`).** The 1×1
  pixman colour-source image used in `render_text_string`'s
  phase-2 composite was created with the default `REPEAT_NONE`,
  so pixman read transparent black for any source coordinate
  outside `(0,0)`. `Operation::Over` with transparent source is
  a no-op — only the leftmost column of each glyph (where the
  alpha mask is usually near-zero anyway) received the foreground
  colour, producing the "scattered black dots" symptom.  Fix:
  `color_img.set_repeat(Repeat::Normal)` so pixman tiles the
  solid colour uniformly across the entire glyph bounding box.
- **Step 1B — RENDER Trapezoids + Picture/SolidFill
  (`9bf29f1`).** Flipping render_opcode (Step 1A) regressed
  xclock — xclock switched to RENDER for all drawing and the
  no-op stubs dropped every trapezoid. Step 1B adds a
  `pictures: HashMap<u32, PictureState>` on `KmsBackend` with
  `Drawable { host_xid, clip }` and `SolidFill { image }`
  variants; implements `render_create_picture`,
  `render_create_solid_fill`, `render_free_picture`,
  `render_set_picture_clip_rectangles`, `render_trapezoids`
  (forwards to `pixman_composite_trapezoids` via `pixman-sys`
  FFI), and `render_format_for_ynest_id` (passes nonzero IDs
  through so `nested.rs` actually dispatches the trapezoid
  call). Trap wire format matches `pixman_trapezoid_t` byte-
  for-byte, decoded element-by-element to avoid alignment
  pitfalls. Adds 2 colocated unit tests verifying nonzero alpha
  in dst + center-pixel colour propagation.
- **Step 1B follow-up — clip-rect parser
  fix (`db54ce4`).** `render_set_picture_clip_rectangles`
  received the whole request body from nested.rs (picture XID
  at `body[0..4]`, then `clip_x_origin` / `clip_y_origin`,
  then rectangles). The initial parser treated bytes 0..4 as
  the origin and 4+ as rectangles — clip rects were the real
  rect bytes shifted up by one field, pixman clipped most of
  xclock's traps to a tiny region and only a partial arc of the
  dial appeared. One-line fix: skip past the picture-XID prefix
  in the parser.

#### Validation (commits `6972c39 .. db54ce4`)

xclock, xterm, xeyes, and fvwm3 all run together on bare
DRM/KMS without a host X server. fvwm3 manages and frames
xterm + xclock (visible title-bar chrome on both); xclock's
full dial renders with anti-aliased ticks, hour/minute/second
hands sweep correctly; xterm's text is legible (env output
verified end-to-end including a fish prompt status line).
fvwm3 modules (FvwmPager, FvwmIconMan, FvwmButtons) reach
idle without busy-looping — Step 0's busy-loop signature
(`ConfigureWindow → ChangeWindowAttributes → GetInputFocus`
on the same window with `(-1, -1)` coords) does not appear
in the post-1A log.

365 tests passing in workspace (16 yserver + 9 ynest + 248
yserver-core + 87 yserver-protocol + 5 misc). The yserver lib
test count grew from 18 → 21 with three new colocated tests:
`fill_rects_copy_overwrites_destination`,
`poly_segment_xor_inverts_destination_pixels`,
`render_trapezoids_over_produces_nonzero_alpha_in_dst`,
`render_trapezoids_center_pixel_carries_source_color`,
`glyph_render_gray_pixels_land_on_correct_rows`.

WM matrix: fvwm3 fully validated on KMS. wmaker / e16 not
re-tested on KMS; the WMs already worked under host backend
in 6.3, and 6.5's KMS-specific changes (GC function, glyph
REPEAT, RENDER stubs) are backend-internal and don't regress
the host path. Standalone WM smoke under bare KMS for
wmaker / e16 is parked as a 6.6 follow-up.

Out of scope (deferred to 6.6+):

- **`RENDER::CompositeGlyphs8` on KMS.** fvwm3's panel labels
  (FvwmPager desktop names, FvwmIconMan window titles,
  FvwmButtons text) are rendered via CompositeGlyphs8, which
  is still a no-op stub. fvwm3 manages clients correctly with
  the panel chrome rendered as solid backgrounds — text/icons
  on those panels are blank.
- **`RENDER::Composite` on KMS.** Off-screen-buffer-to-window
  blits via the generic Composite call are also no-op stubs.
  Used by some toolkits for double-buffered widget rendering.
- Real font enumeration on KMS (`list_fonts_proxy` still
  returns the empty terminator).
- Host (GTK) cursor and guest cursor drift / lock.
- pixman `fill_rectangles` partly-out-of-bounds segfault
  root-cause investigation.
- Full VT_SETMODE / logind / suspend-resume / hotplug polish.
- wmaker / e16 smoke under bare KMS.
- `SetDashes` no-op + reply (still surfaced as `unsupported
  opcode 58` in the log; cosmetic).
- `InstallColormap` no-op (still `unsupported opcode 81`;
  TrueColor backend so safe to ignore).
- `line_width` thick lines in `poly_line`.
- Partial-angle clipping for `poly_arc` / `poly_fill_arc`.

### Phase 6.6 — RENDER completion on KMS (complete)

Goal: implement `CompositeGlyphs` and `Composite` on `KmsBackend` so
fvwm3 panel text renders on bare DRM/KMS, then smoke-run wmaker/e16
on bare KMS to close the WM matrix.

#### Landed (branch `phase6.6`)

- **GlyphSet lifecycle (`5204d56 .. fcd17f9`).** Adds `GlyphSetState`
  (format + glyph map) and `StoredGlyph` (A8 pixel data, metrics) to
  `KmsBackend`. Implements `render_create_glyphset`, `render_free_glyphset`,
  and `render_free_glyphs`. Format 2 (A8) is supported; other formats
  stored as `Other` and silently skipped at render time.

- **`render_add_glyphs` (`724376f .. 0265373`).** Parses the AddGlyphs
  wire format (num_glyphs, glyph IDs, 12-byte glyph info structs,
  A8 pixel data with 4-byte-padded rows) and stores glyphs densely.
  Factored as `parse_add_glyphs` free function for direct unit testing.

- **`render_composite_glyphs` (`e66869c .. ff01dba`).** Processes the
  CompositeGlyphs item stream (count/dx/dy runs, count-255 sentinel,
  1/2/4-byte glyph IDs for minor 23/24/25). For each glyph: composite
  A8 mask × solid-colour source onto dst via pixman `composite32`.
  Clip regions applied when set. Colour image hoisted out of the inner
  loop for efficiency. Factored as `composite_glyphs_onto` free function
  for direct unit testing.

- **`render_composite` (`983b04f .. 1934769`).** Generic Composite
  operation via `pixman_image_composite32` FFI (same module as
  `render_trapezoids`). Supports SolidFill and Drawable sources, Drawable
  destination, optional clip region. Mask compositing not supported (warn
  + skip). Guards against self-composite (src and dst same drawable)
  to prevent pixman aliasing UB.

- **Software cursor (`e5edd4a .. f2ee936` + later).** Replaces 16×16
  white rectangle placeholder with a real ARGB cursor sourced from
  `RENDER::CreateCursor`. Adds `CursorState { image: PixmanImage, hot_x,
  hot_y }` and `cursors`/`active_cursor` fields to `KmsBackend`.
  `render_create_cursor` copies pixel data from the source picture's
  backing pixmap into a new A8R8G8B8 `PixmanImage`. `define_cursor` sets
  the global active cursor. `draw_cursor_onto` composites the cursor image
  at `(cursor_x − hot_x, cursor_y − hot_y)` using `Operation::Over`.
  No cursor is drawn until `define_cursor` is called. Requires visual
  smoke test under vng to confirm shape.

#### Tests

27 tests passing in `yserver` lib (up from 21 before Phase 6.6).
New tests: `add_glyphs_stores_pixel_data_correctly`,
`composite_glyphs_single_run_places_glyph_on_dst`,
`composite_glyphs_multi_run_advances_pen`,
`composite_glyphs_sentinel_does_not_panic`,
`render_composite_solid_fill_onto_drawable`,
`render_create_cursor_stores_image_and_hotspot`,
`draw_cursor_onto_composites_at_hotspot_adjusted_position`.

#### Validation

Headless KMS smoke test (vng + virtio-gpu-pci): yserver started cleanly —
opened `/dev/dri/card0`, selected `Virtual-1` 1280x800 mode, entered epoll
event loop, and shut down gracefully on SIGTERM with no panics or crashes.

Visual KMS smoke (vng `--graphics`, fvwm3 + xeyes): FvwmPager desktop
labels (`0 1 2 3`) and FvwmButtons clock (`19:42 Mon May 04`) rendered
correctly via `CompositeGlyphs8`. xeyes managed and framed by fvwm3.
CompositeGlyphs is confirmed working on bare DRM/KMS.

#### Phase 6.6 follow-up session (May 5)

After the initial RENDER completion landed, an extended session
added structural cleanups and a series of bug fixes that take the
WM matrix from "fvwm3 visually rendering" to "fvwm3 fully
interactive + wmaker mostly + e16 mostly":

- **Workspace clippy clean (`0a7994f .. 7c90c6b`).** Added
  `[workspace.lints.clippy]` allowing `too_many_arguments` (X11
  protocol encoders inherently have many parameters) and removed
  remaining warnings via `cargo clippy --fix` plus a few hand-fixes.
- **Pixman safety (`e63d87a`).** Wrapped the four ad-hoc
  `pixman::ffi::pixman_image_composite32` / `pixman_composite_trapezoids`
  unsafe blocks in two `composite32` / `composite_trapezoids` helpers
  taking `&mut PixmanImage` for dst; marked `PixmanImage::data()` as
  `unsafe fn` so every raw-pointer use surfaces in `unsafe` blocks
  with explicit SAFETY comments.
- **Mutex poison-tolerance + `DrawableGeometry` accessor (`2da0546`).**
  `lock_recover()` recovers from poison instead of double-panicking;
  `DrawableGeometry<'_>` consolidates the
  width/height/stride/data-pointer lookup that put_image and
  get_image both used (replaces 4 `.unwrap()` sites).
- **Top-level stacking + `ConfigureWindow` `stack_mode` (`bf39c3e`).**
  Added `top_level_order: Vec<u32>` so compositor and hit-test walk
  windows in deterministic stacking order rather than HashMap-iteration
  order. `restack_window()` honours X11 Above/Below modes.  Fixed
  fvwm3 popup menus that were non-deterministically hidden behind
  other top-levels.
- **`YSERVER_MODE` env-var override for resolution (`99e21d3`).**
  virtio-gpu's `xres`/`yres` hint is unreliable; `pick_mode()` now
  honours `YSERVER_MODE=WxH` to force a specific mode from the
  advertised list.
- **`event_x` / `event_y` wire-correct (`31d777b`).** Pointer events
  were sending root-relative cursor coords in the X11 spec's "event
  window relative" fields, breaking server.rs's descend-into-children
  logic — the click landed on the top-level instead of the deepest
  descendant. Fix: subtract the top-level's `(x, y)` from the cursor
  before populating event_x/event_y. Unblocked fvwm3 pager clicks
  and decoration buttons.
- **Held-button mask in `state` field + dedicated input thread
  (`877a399`).** Pointer event `state` field now includes
  Button1Mask..Button5Mask bits while the corresponding button is
  held — without this, motion events during a drag had `state=0x0`
  and fvwm3's drag detector saw "not actually dragging". Same commit
  moves libinput dispatch off the main epoll thread so motion events
  flow continuously instead of in batches gated by per-client X11
  request handlers holding the backend mutex.
- **Per-window cursor + root-cursor forwarding (`954fa1e`).**
  Replaced the global `active_cursor` with per-window cursors that
  inherit up the parent chain (X11 spec). Resolves the "every window
  shows the same wrong cursor" symptom. nested.rs forwards root-window
  CWCursor changes to the backend via `Backend::window_id()` (root
  has no `host_xid` in resources, so the call previously dropped).
- **Built-in default X cursor + lock-order fix (`6f7754b`).**
  `KmsBackend::open()` now installs a 16×16 X-shaped cursor as the
  fallback, so something is always drawn before any client calls
  DefineCursor. Also fixed a server→backend / backend→server
  lock-order inversion in the root-cursor path that wedged fvwm3
  startup (the input pump acquires backend→server through the
  event-sink path; the new CWCursor handler was acquiring
  server→backend inside the same critical section).
- **GetImage wire-format fix (`911fa38`).**
  `KmsBackend::get_image` was returning raw pixel bytes instead of
  a complete X11 wire reply (header + data). nested.rs treats the
  return value as a full reply and patches sequence/visual into it,
  so the first byte (a pixel value, often 0) ended up looking like
  an X11 error to the client. wmaker's `catchXError` then logged
  "internal X error: 0" and short-circuited frame mapping —
  preventing xterm under wmaker from ever showing.
- **Depth-1 PutImage (`93742cc`).** Was a no-op; now a row-wise
  memcpy (X11 ZPixmap d1 and pixman A1 share scanline conventions).
  Restored wmaker's appicon (was clipped diagonally) — it uses
  depth-1 ZPixmaps via MIT-SHM as icon shape masks.
- **Click-on-desktop falls through to root (`5f8f1f0`).**
  `window_under_cursor()` returned `None` when the cursor was over
  the wallpaper, so events were delivered with `host_xid=0` and
  dropped. Fall back to the root container so right-click-desktop
  menus reach the WM. Unblocked e16's main menu.

#### WM matrix on bare KMS

| WM    | Status                                                              |
|-------|---------------------------------------------------------------------|
| fvwm3 | fully working: pager click, decoration buttons, drag, resize, cursors, panel text, popup menus |
| wmaker| mostly: dock + appicon render, xterm framed and usable, drag, resize. Title-bar close/minimize button glyphs missing (cosmetic). |
| e16   | comes up, dock + workspace previews render, right-click menu opens, dialogs render. Widget actions don't fire on click — likely Sync-grab replay semantics not fully implemented. |

Tracked in `docs/known-issues.md` under "wmaker on KMS" and the e16
section.

Out of scope (deferred to 6.7+):

- Sync-mode passive-grab replay (`AllowEvents(ReplayPointer)`). e16
  widget clicks block on this.
- wmaker title-bar close/minimize glyph rendering.
- `y_off` per-glyph pen advancement (horizontal text only for now).
- CompositeGlyphs glyphset-switch mid-stream (sentinel switches to
  a new glyphset; current impl ignores the switch and uses the
  original glyphset for all glyphs).
- `CompositeGlyphs` RENDER operator (always composites with `Over`;
  other ops ignored).
- Mask compositing in `render_composite`.
- CompositeGlyphs for formats other than A8 (A1, ARGB32).
- Real font enumeration / `list_fonts_proxy` populated.
- Host (GTK) cursor and guest cursor drift / lock.
- VT_SETMODE / logind / suspend-resume / hotplug polish.

### Phase 6.7 — Full X11 implementation pass (complete)

Goal: replace every Phase 6.6 stub on `KmsBackend` with a spec-correct
implementation across input, drawing, RENDER, glyphs, SHAPE, font
enumeration, and XKB. TDD throughout: failing test → minimal impl →
commit, one task per sub-phase. Squash-merged from `phase6.7` branch.

Design:
[`2026-05-05-phase67-design.md`](superpowers/specs/2026-05-05-phase67-design.md).
Plan:
[`2026-05-05-phase67-full-x11-pass.md`](superpowers/plans/2026-05-05-phase67-full-x11-pass.md).

#### Landed

- **6.7.1 — input.** `warp_pointer` updates `cursor_x`/`cursor_y`
  (relative to destination window if given, otherwise current pointer
  position) and pushes a synthetic `MotionNotify` through the event
  sink; coords clamped to the framebuffer. `AllowEvents` mode=2
  (ReplayPointer) thaws the frozen `ButtonPress` and re-routes via a
  new `route_button_press_no_grab` free function in `server.rs` that
  walks the window-tree from the deepest window under the event coords
  upward until it finds a window with `ButtonPressMask` — without
  re-checking passive grabs and without delivering to the former grab
  owner. e16 widget clicks now activate.
- **6.7.2 — drawing.** `copy_plane` builds a temporary ARGB image by
  testing the plane bit on every source pixel and substituting
  foreground/background colour, then composites onto the destination
  through `fill_rects_with_gc_function`. `poly_text16` and
  `image_text16` parse CHAR2B (`high<<8 | low`) text items, advancing
  the pen by per-glyph `character_width` from the font's
  `char_info_cache`; sentinel `len=255` skips the 7-byte font-change
  payload, and `image_text16` paints the background rect first.
- **6.7.3 — RENDER attributes / gradients / transforms.**
  `PictureState::Drawable` extended with `repeat`, `alpha_map`,
  `alpha_x/y`, `clip_x/y`, `component_alpha`, `transform`, plus stored-
  but-no-op fields for `graphics_exposure`, `subwindow_mode`,
  `poly_edge`, `poly_mode`. `render_change_picture` parses every
  CPxxx bit (CPRepeat, CPAlphaMap/Origin, CPClip, CPGraphicsExposure,
  CPSubwindowMode, CPPolyEdge/Mode/Dither, CPComponentAlpha) — clearing
  clip on `CPClipMask=0`. New `PictureState::Gradient` variant backed
  by pixman `pixman_image_create_linear_gradient` /
  `pixman_image_create_radial_gradient` via FFI; both wired into
  `render_composite` source dispatch with `set_repeat` /
  `set_transform` applied per-composite. `render_set_picture_transform`
  decodes the 9×4-byte 16.16 Fixed matrix, stores it on the picture
  (skipping if it's the identity), and applies in `render_composite`
  via `pixman_image_set_transform`, resetting to identity afterwards.
- **6.7.4 — CompositeGlyphs.** Pen advancement now applies both
  `delta_x` and `delta_y` per item run; mid-stream glyphset switch via
  the count=255 sentinel + 4-byte XID is honoured (function signature
  expanded to take `&HashMap<u32, GlyphSetState>` so the active
  glyphset XID can change per group). `GlyphSetFormat::A1` added with
  X11 ZPixmap-style 32-bit-padded MSB-first scanlines stored verbatim;
  `parse_add_glyphs` routes A1 glyphs into the new format and
  `composite_glyphs_onto` feeds them through pixman as `FormatCode::A1`
  masks. fvwm3 / xclock / xterm panel text is correct and toolkits
  using A1 cursor or icon glyph fonts now render.
- **6.7.5 — SHAPE.** `KmsBackend` gains
  `shape_bounding`/`shape_clip`/`shape_input` `HashMap<u32, Vec<RegionRect>>`
  fields. `set_shape_rectangles` stores rects keyed by `kind`; empty
  rects encode "window clips to nothing" (still hittable=false) and a
  separate `clear_shape_rectangles` removes the entry to restore the
  default rectangular shape. `DestroyWindow` sweeps all three maps.
  Compositor pass installs each window's bounding region via
  `pixman_image_set_clip_region` before its composite call and clears
  it after. `window_under_cursor` checks input shape first
  (precedence), bounding shape second, with empty regions = no hit.
- **6.7.6 — font enumeration.** `fontconfig = "0.7"` added to the
  workspace. `list_fonts_proxy` queries fontconfig for matching
  patterns and synthesises XLFD names via `fc_match_to_xlfd` (encodes
  family, weight, slant, size, spacing). `list_fonts_with_info_proxy`
  reuses the same enumeration and attaches `FontMetrics` from the
  freetype loader. `FontLoader::open_font` detects XLFDs (leading `-`)
  and parses the family / style / size fields into a fontconfig
  pattern before opening — clients selecting fonts by XLFD now
  succeed instead of falling back to the default face.
- **6.7.7 — XKB proxy.** New `crates/yserver/src/kms/xkb.rs` builds
  reply payloads for `UseExtension` (minor 0), `GetMap` (8) with
  correct min/max keycode from `xkbcommon::xkb::Keymap`, `GetNames`
  (17), `GetCompatMap` (20), and `GetControls` (24) with default
  500ms / 33ms repeat delay / interval. Unknown reply-requiring minors
  fall through to a 32-byte minimal reply. `xkb_proxy` on
  `KmsBackend` dispatches on minor and consults the backend-owned
  keymap. Clients calling `XkbUseExtension` / `XkbGetMap` no longer
  hang waiting for replies on bare KMS.

#### Validation

- TDD-driven: every task lands its failing test before the impl. New
  unit tests across `backend.rs` (~12 new), `server.rs` (1 new for
  `route_button_press_no_grab`), and `xkb.rs` (4 new). Workspace test
  suite green.
- `clippy -p yserver -- -D warnings` clean.
- The merged-master commits cover all 13 spec items in the plan's
  self-review table.

#### Phase 6.7 follow-ups

- **CompositeGlyphs ARGB32 format.** A1 + A8 supported; ARGB32 still
  routes to `Other` and is silently skipped.
- **CPClipMask non-zero pixmap path.** Clip from a 1-bit pixmap is
  approximated as a full-rect clip derived from the pixmap geometry;
  honouring the actual bit pattern would require building a pixman
  region from the mask.
- **XKB GetMap full table encoding.** Current reply carries correct
  min/max keycode + present=0 (no tables). Toolkits that introspect
  key types / sym maps / modifier maps still see empty data; promote
  to full table encoding if a real client needs it.
- **`fontconfig`-crate API surface.** `list_fonts_proxy` uses the 0.7
  surface — revisit if the crate adds a richer object-set API.
- **Host (GTK) cursor and guest cursor drift / lock** — still open.
- **VT_SETMODE / logind / suspend-resume / hotplug polish** — still
  open.

### Phase 6.8 — Single-threaded core (complete)

Goal: eliminate the `ServerState` ↔ `Backend` lock-inversion deadlock
class by collapsing all core mutation onto one thread, reducing every
other thread to an I/O-only `mpsc` producer. Land on the
`single-threaded-core` branch with green workspace tests + full
multi-WM smoke before merging to `master`.

Spec:
[`2026-05-05-single-threaded-core-design.md`](superpowers/specs/2026-05-05-single-threaded-core-design.md).
Plan:
[`2026-05-06-single-threaded-core.md`](superpowers/plans/2026-05-06-single-threaded-core.md).

#### Architecture landed

A single core thread owns a plain `ServerState` and `Box<dyn Backend>`
(no `Arc<Mutex<>>`). The core's mio poller owns the listener fd, every
client read+write fd, libinput fd, drm fd, signalfd, host-X11 fd, and
a `Waker` for the message channel. Per-client *reader threads* are
the only producers that turn raw bytes into `Message::Request`s;
libinput, DRM page-flip, signalfd, and host-X11 all drive the core
poller directly. Replies/events go out via per-client non-blocking
write halves with bounded outbound buffers; on `EAGAIN` we register
`WRITABLE` interest, on overflow we disconnect.

#### Phases A → I (landed before Phase J smoke)

- **A1**: rename `ClientHandle` → `ClientState`, add `outbound`,
  `watching_writable`, `focused_window`, `reader_control` fields.
- **B1–B4**: `Message` enum (with two-stage client lifecycle —
  `SetupAllocate` round-trip + `ClientSetupComplete` hand-off),
  `CoreSender`/`CoreReceiver` over `crossbeam-channel` + mio
  `Waker`, reshape `Backend` trait (`on_host_input`,
  `on_page_flip_ready`, `poll_fds`), `run_core` skeleton.
- **C1–C3**: per-client outbound write helper (4 MB cap), setup
  thread that owns the full handshake + teardown registry,
  per-client reader thread with reader-side BigRequests barrier
  via `ReaderControl::Apply/Ignore/Shutdown` and SCM_RIGHTS fd
  pass-through.
- **D1–D6**: lift every `process_request` opcode (1–127) plus all
  13 extension dispatchers (RANDR, MIT-SHM, RENDER, BIG-REQUESTS,
  XKB, XI2, GE, XFIXES, SHAPE, SYNC, DAMAGE, COMPOSITE, PRESENT)
  to state-borrowing `&mut ServerState` + `&mut dyn Backend`,
  delete the per-client keyboard forwarder thread, listener fd
  owned by the core poller. Workspace stays green throughout.
- **E1–E4**: KMS senders + libinput motion coalescing (latest
  position wins per batch; non-motion events flush pending).
  Delete `pending_pointer_events`, `BackendEventSink` impl on
  `KmsBackend`, `key_subscribers` field. `yserver::run` rewritten
  on top of `run_core`.
- **F1–F2**: host-X11 dispatcher thread deleted; the core thread
  owns the host fd and drives reads + reply waits directly via
  `drain_host_socket` + `dispatch_pending_host_events`.
  Reentrancy invariant: `drain_host_socket` only enqueues; event
  fanout runs at the outer-loop boundary.
  `nested::run` rewritten on top of `run_core`.
- **G1–G3**: spec-correct implicit-grab crossings via
  `crossings::implicit_grab_crossings` (path-walk between focus
  and grab windows, NotifyGrab on press / NotifyUngrab on
  release).
- **I1–I5**: backpressure poller integration. Per-client tokens
  + monotonic `ClientIdAllocator` (no reuse across
  disconnect/connect). `WRITABLE` interest registered per
  iteration via `reconcile_client_writable_interest` + a
  proactive `drain_outbound` to avoid edge-triggered EPOLLOUT
  races. Slow-client disconnect on `OUTBOUND_CAP` overflow.

#### Phase H — dead-code deletion (complete)

- **H1**: deleted `nested::handle_client` / `handle_request` /
  `lock_server` and ~9.7k lines of legacy code that the
  `process_request` lift superseded; ported the relevant tests
  to the state-borrowing helpers.
- **H2**: deleted `BackendEventSink` trait, `HostPumpEventSink`,
  `host_pump_event_sink`, the `event_sink` field on
  `KmsBackend`, and `synthesize_expose` (no production callers
  since E4).
- **H3**: dropped the dead `KmsBackend.key_subscribers` field
  and stale comment references to `spawn_keyboard_forwarder`,
  `add_key_subscriber`, `BackendEventSink`,
  `process_one_input_event`, and `pending_pointer_events`.
- **H4**: dropped the `pub type ClientHandle = ClientState`
  alias; renamed all 25+ remaining `ClientHandle` references to
  `ClientState`.
- **H5**: lifted `server::handle_host_container_resize` into
  `core_loop::run::handle_host_container_resize` (state-borrowing,
  full RANDR ScreenChangeNotify / CrtcChangeNotify /
  OutputChangeNotify fanout, now via `client_io::write_or_buffer`
  for backpressure honouring). Final grep: zero
  `Arc<Mutex<ServerState>>` production references, zero
  `lock_server` references.

#### Phase J — smoke matrix (run; bugs fixed inline; merge pending)

Phase J shook out 9 protocol bugs that the lift surfaced once
real WMs and apps started driving the new path. All fixed inline:

- `process_disconnect` not idempotent (writer EPIPE racing
  reader EOF could fire it twice).
- `OUTBOUND_CAP` set to 64 KB → spurious slow-client disconnect
  when a single legitimate large reply (e.g., ISO10646 font's
  786 KB QueryFont reply) overflowed the buffer. Bumped to 4 MB.
- Edge-triggered EPOLLOUT race in
  `reconcile_client_writable_interest`: the kernel could
  transition the fd writable before we registered for WRITABLE,
  and we'd miss the edge. Fix: proactive `drain_outbound` at
  the top of every iteration.
- `PointerEvent.child` field always `0` — fvwm3's
  `Mouse 1 R A Menu MainMenu` binding fired on every click
  anywhere on the screen because every event looked like a
  bare-root click. Threaded the propagation child through
  `pointer_propagation_target_by_id` and the wire encoder.
- XI2 events stolen by core grabs. After fvwm3 received a
  propagated core ButtonPress and replied with XGrabPointer,
  our active-grab path redirected ALL subsequent events
  (Release, Motion) to fvwm3 via core; XI2 selectees got
  nothing. Per X11 spec, XI2 grabs and core grabs are
  independent. Fix: split `pointer_event_fanout_to_state` so
  the XI2 fanout always runs, regardless of any active core
  grab.
- XI2 `buttons` mask wrong on Release: was always
  `1 << (button-1)`; per spec it's the post-event button state.
  Made gtk3 tree-view expanders work for the first click.
- `SetInputFocus` not mirrored across clients. The per-client
  `focused_window` field is documented as a global value
  mirrored across every client's row, but the implementation
  only updated the caller. Symptom: keyboard input didn't reach
  xterm under wmaker because both wmaker and xterm called
  SetInputFocus on their own internal windows; `current_focus`
  picked whichever HashMap iteration visited first. Fix: in
  `set_focused_window_to_state`, write the new focus into every
  `state.clients[*].focused_window`.
- `HostInputEvent::PointerButton.button` was `u8` but Linux
  input button codes are u32 with `BTN_LEFT = 0x110` —
  truncated to `0x10` on the wire and dropped by the KMS
  backend's `0x110 => 1` mapping. Symptom on yserver: every
  click logged `unmapped libinput button code 0x10, dropping`
  and never reached the WM or apps. Widened to `u16`.
- `compute_font_metrics` on KMS returned empty `properties`,
  which Xt/Athena/fvwm3 menu code treats as "font unusable".
  `handle_open_font` now synthesizes the standard XLFD property
  set (FONT, FOUNDRY, FAMILY_NAME, …, CHARSET_ENCODING) when
  the backend returns an empty list.

#### Smoke matrix — current state

| Backend          | WM     | Status |
|------------------|--------|--------|
| `ynest`          | fvwm3  | ✅ |
| `ynest`          | wmaker | ✅ except keyboard regression after sustained interaction |
| `ynest`          | e16    | ✅ |
| `ynest`          | gtk3-demo | ✅ except tree-view expander quirk |
| `yserver` (KMS)  | e16    | ✅ |
| `yserver` (KMS)  | wmaker | ✅ same as pre-refactor |
| `yserver` (KMS)  | fvwm3  | ⚠️ menus render with no text (font-rendering gate beyond properties) |

Two regressions filed in
[`known-issues.md`](known-issues.md): xterm KeyPress drops after
focus / state-changing interaction (WM- and backend-independent —
likely a regression from the refactor — needs a stuck-grab
investigation), and yserver/KMS fvwm3 menu text. The original
`ServerState ↔ Backend` lock-inversion deadlock class —
the whole point of the refactor — is gone, and both backends
boot every WM in the matrix.

#### Phase 6.8 follow-ups (deferred)

- merge to `master` (J5: codex review pass + squash, awaiting
  user sign-off).
- xterm KeyPress-stops bug (needs key tracing in
  `host_x11/pump.rs::decode_host_event` + `key_event_fanout_to_state`'s
  grab-activation / grab-release branches; suspect a stuck
  `state.active_keyboard_grab` whose release condition only
  fires on KeyRelease of the *exact* triggering keycode).
- yserver/KMS fvwm3 empty-menus (font-rendering gate beyond
  properties — needs a diff between xterm's QueryFont reply
  path, which works, and fvwm3's, which doesn't).
- gtk3-demo tree-view expander single-click toggle reliability.
- legacy `&Mutex<ServerState>`-shaped fanout helpers in
  `server.rs` (`pointer_event_fanout`, `route_button_press_no_grab`)
  are dead in production but kept under `#[allow(dead_code)]`
  for their test fixtures — fold those tests onto the
  state-borrowing helpers and delete.

### Phase 6.9 — XTEST + xts5 regression coverage (BadLength + BE client support landed)

Goal: stand up the X.Org X Test Suite (xts5) as the primary
protocol-coverage feedback loop, replacing ad-hoc manual WM smoke for
regression detection. Land just enough XTEST for xts to *run*; let
the suite tell us where the real gaps are.

#### Landed

- **XTEST extension (major opcode 146).** `GetVersion` (replies
  2.2), `CompareCursor` (stub: `same=1`), `FakeInput` (full —
  KeyPress/KeyRelease/ButtonPress/ButtonRelease/MotionNotify),
  `GrabControl` (no-op). FakeInput translates to `HostInputEvent`
  and feeds `Backend::on_host_input`, so both backends route
  synthesized input through their existing fan-out paths.
- **`HostX11Backend::on_host_input`** flipped from no-op to
  enqueue-to-`pending_events` so XTEST FakeInput on `ynest` reaches
  the same fanout the host-pump path uses. KMS already handled
  `HostInputEvent` uniformly via libinput.
- **Tooling:** `tools/xts-run.sh` wraps `xts/check.sh` +
  `xts-report` to emit a per-scenario tally (
  `CASES TESTS PASS UNSUP UNTST NOTIU WARN FIP FAIL UNRES UNIN ABORT`).
  `just xts-ynest scenario=…` boots release `ynest` on `:99` and
  runs the scenario, killing ynest on exit.
- **Baseline + run history:** [`docs/xts-baseline.md`](xts-baseline.md)
  tracks each xts run as a row in the run-history table. The headline
  is that **ynest survives the entire battery without panic, hang, or
  crash**. Per-test result counts (Xproto scenario, 122 cases / 389
  purposes):

  | Date       | PASS | FAIL | UNRES | UNIN | NORES | Change |
  |------------|-----:|-----:|------:|-----:|------:|--------|
  | 2026-05-06 |    1 |  210 |   160 |   11 |     7 | First run after XTEST landed. |
  | 2026-05-06 |    1 |   74 |   296 |   11 |     7 | `BadLength` enforcement at the top of `process_request`. |
  | 2026-05-06 |   26 |   91 |   252 |    0 |     0 | BE client support phases 0+A+B+C+D+D1 (reader, setup, errors, replies, events, shared `wire_swap` module). |
  | 2026-05-06 |  195 |   78 |    97 |    0 |     0 | Phase E — per-opcode inbound request body swap. **PASS 26 → 195**. |
  | 2026-05-06 |  229 |   40 |    99 |    0 |     0 | Phases D2 + F — raw event templates per-recipient + content-aware BadLength. **PASS 195 → 229**. |
  | 2026-05-06 |  337 |   25 |     7 |    0 |     0 | xproto branch — residual fixes (missing replies, BadAlloc/BadAccess/BadValue/BadIDChoice, Expose/GraphicsExpose, content-shape BadLength, max length, error-resilience). **PASS 229 → 337**. |

  Xlib3 scenario (162 tests / 109 cases):

  | Date       | PASS | FAIL | UNRES | Change |
  |------------|-----:|-----:|------:|--------|
  | 2026-05-06 |   96 |   31 |     3 | First Xlib3 run on top of all Xproto fixes. |
  | 2026-05-06 |  110 |   17 |     3 | xts-xlib3 branch — vendor string, release_number, 7 pixmap formats, screen mm dimensions, SetCloseDownMode validation. **PASS 96 → 110**. |

  ShapeExt scenario (11 tests / 11 cases):

  | Date       | PASS | FAIL | Change |
  |------------|-----:|-----:|--------|
  | 2026-05-07 |    5 |    6 | First ShapeExt run; `GetRectangles` reported `Unsorted` and `QueryExtents` ignored `border_width`. |
  | 2026-05-07 |   11 |    0 | `GetRectangles` reports `YXBanded`, `normalize_region_rects` sorts by (y, x), `default_shape_rect` is kind-aware (BOUNDING includes border, CLIP/INPUT do not). Plus a wmaker-spotted typo: opcode 36 in the FreeColors content-aware shape gate is GrabServer; arm relabelled to opcode 88. **PASS 5 → 11**. |

  XI scenario (36 cases / 316 tests) — previously timed out at 600s:

  | Date       | PASS | FAIL | UNRES | UNTST | Change |
  |------------|-----:|-----:|------:|------:|--------|
  | 2026-05-07 |    0 |   21 |     0 |   289 | `xts-followups` branch — XI 1.x reply-required minor stubs (22 minors, 32-byte zero-fill replies). Suite no longer hangs in `_XReply`; most tests UNTST because `ListInputDevices` advertises 0 devices, so device-dependent preconditions go unmet. |

  XIproto scenario (35 cases / 107 tests) — previously timed out at 600s:

  | Date       | PASS | FAIL | UNRES | UNTST | Change |
  |------------|-----:|-----:|------:|------:|--------|
  | 2026-05-07 |    0 |    0 |     0 |   107 | Same XI 1.x stub branch; suite completes cleanly, all UNTST on missing-device preconditions. |

- **`BadLength` enforcement landed.** A per-opcode length contract
  table covers all of opcodes 1–127 in
  `crates/yserver-protocol/src/x11/request_lengths.rs`: `Fixed(n)`
  for fixed-length requests, `AtLeast(n)` for variable.
  `process_request` validates `header.length_units` against the
  contract before dispatch and replies `BadLength` on mismatch.
  A second pass (`exact_required_length`) fires content-aware
  `BadLength` for variable-length opcodes by computing the actual
  required length from the body (popcount of value-masks, length-
  prefixed string sizes, `value_len * format / 8` for
  `ChangeProperty`, etc.).

- **Big-endian client support landed.** xts5 opens a reversed-byte-
  sex probe connection on every test purpose; previously the setup
  gate refused those, gating the entire pass count. Now BE clients
  are accepted end-to-end:

  - **Phase 0 — request reader.** `read_request` decodes the 16-bit
    length field and the BIG-REQUESTS extended length in the client's
    declared byte order.
  - **Phase A — setup.** The BE rejection in `setup_thread` is gone;
    `write_setup_success` and `write_screen` thread `byte_order`.
  - **Phase B — errors.** `write_error` and `emit_x11_error` honour
    the client byte order.
  - **Phase C — replies.** ~70 reply encoders across `mod.rs`,
    `randr.rs`, `shape.rs`, `xfixes.rs`, `present.rs`, `composite.rs`,
    `damage.rs`, `mit_shm.rs`, `sync.rs`, `xtest.rs` take a
    `byte_order` parameter; the `wire::fixed_reply` helper plus four
    private `fixed_reply` helpers in extension modules and one raw-
    `to_le_bytes` site in `present.rs` all updated.
  - **Phase D + D1.** Selection events and the rest of the event
    encoders honour their `order` parameter; new
    `crates/yserver-protocol/src/x11/wire_swap.rs` defines shared
    `FieldKind` / `FieldEntry` types + `swap_in_place`.
  - **Phase D2 — raw event templates.** `fanout_raw_event_to_clients`
    takes `template_byte_order` and re-encodes the 32-byte template
    per recipient via `core_event_swap_table`. Source order is LE
    for server-built events (SelectionNotify, RANDR notify) and the
    sender's byte order for `SendEvent`.
  - **Phase E — inbound request body swap.** New
    `crates/yserver-protocol/src/x11/request_swap.rs` holds a per-
    opcode swap table (~70 core opcodes). The per-client reader
    thread calls `swap_request_body` after `read_request`; the rest
    of the dispatch path keeps reading bytes as little-endian.

  End-to-end the BE branch lifts xts Xproto from **1 PASS to 229
  PASS** (out of 389) and drops UNRES from 296 to 99.

#### Phase 6.9 remaining follow-ups

Most of the originally-listed gaps landed in commit `b12d25f`
("residual xts Xproto fixes — PASS 229 → 337") and the `xts-followups`
branch (`816d6f4` + `d00191a`):

- [x] **Missing reply implementations** (6 opcodes — `b12d25f`).
      GetMotionEvents (39), GetFontPath (52), ListInstalledColormaps
      (83), GetKeyboardControl (103), GetPointerControl (106),
      GetScreenSaver (108) all return sane stub replies; handlers
      wired at `process_request.rs:172-180`.
- [x] **Per-opcode validation gaps** (`b12d25f`). `BadIDChoice` on
      duplicate XIDs (CreateColormap, CopyColormapAndFree,
      CreateCursor); `BadAlloc` for AllocColorCells/Planes on
      TrueColor; `BadAccess` for StoreColors/StoreNamedColor on
      TrueColor; `ChangeKeyboardControl` mask validation.
- [x] **Event-generation gaps** (`b12d25f`). `SetPointerMapping` /
      `SetModifierMapping` fan out `MappingNotify` before the reply;
      `MapWindow` emits `Expose` on the window itself when newly
      Viewable; `MapSubwindows` emits `Expose` only when the child
      becomes Viewable; `ClearArea` / `CopyArea` / `CopyPlane` emit
      `Expose` / `GraphicsExpose` per GC settings.
- [x] **`request_swap_table` for less-common opcodes** (`816d6f4`).
      Added entries for opcodes 27 (UngrabPointer) and 32
      (UngrabKeyboard); they were the only core opcodes with
      multi-byte body fields not in the table.
- [x] **xts `XI` / `XIproto` baseline** (`d00191a`). 22 reply-required
      XI 1.x minors (2, 3, 5, 7, 9–13, 20, 22, 24, 26–30, 33–36, 39)
      now emit a 32-byte zero-fill stub reply. Both suites complete
      without hanging; see XI/XIproto run-history tables above.

Still open:

- **Real XI device advertisement.** Current `ListInputDevices` stub
  returns 0 devices, so most XI tests are UNTST (preconditions
  unmet). To get actual PASSes we'd need to surface
  pointer + keyboard with valid class info (key/button/valuator
  records), wire `XOpenDevice` to allocate per-device state, and
  honour the XI 1.x grab/event request paths properly.
- **`yserver` (KMS) baseline.** Deferred — running xts in a vng
  guest needs either an in-guest xts build or a tunneled DISPLAY.
- **Residual ~25 Xproto FAIL.** Per `b12d25f` commit log: mostly
  BIG-REQUESTS oversized-length tests that xts itself documents as
  "no known portable test method", plus a few host-backend
  forwarding edge cases. Not addressable without xts-side changes.

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

### XTEST extension (major opcode 146)

| Minor | Name           | Status | Notes |
|-------|----------------|--------|-------|
|   0   | GetVersion     | ↩ | replies 2.2 |
|   1   | CompareCursor  | ↩ | stub: always reports `same=1` |
|   2   | FakeInput      | ✓ | KeyPress/KeyRelease/ButtonPress/ButtonRelease/MotionNotify; translates to `HostInputEvent` and feeds `Backend::on_host_input`; relative motion (`detail=1`) and X buttons 4–7 (wheel) deferred |
|   3   | GrabControl    | ∅ | accepted no-op |

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
|   5   | ChangePicture         | ✓ | scalar attributes + None XID attributes forwarded; non-None CPClipMask/CPAlphaMap dropped (XID translation not yet wired) |
|   6   | SetPictureClipRectangles | ✓ | forwarded with picture x/y offset translation |
|   7   | FreePicture           | ✓ | |
|   8   | Composite             | ✓ | dst_xy patched with dst picture's x/y offset; mask=0 forwards as host xid 0 |
|  17   | CreateGlyphSet        | ✓ | format ID translated to host equivalent |
|  18   | ReferenceGlyphSet     | ✓ | aliases existing host glyphset with refcount |
|  19   | FreeGlyphSet          | ✓ | |
|  20   | AddGlyphs             | ✓ | body padding/length corrected against Xephyr trace |
|  22   | FreeGlyphs            | ✓ | glyphset XID translated; request forwarded to host |
|  23   | CompositeGlyphs8      | ✓ | every non-sentinel glyphcmd's delta patched with picture offset (multi-run safe) |
|  24   | CompositeGlyphs16     | ✓ | same patching as 8 (16-bit glyph-id stride) |
|  25   | CompositeGlyphs32     | ✓ | same patching as 8 (32-bit glyph-id stride) |
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
  `GetGeometry`) for sync, with `PendingReplies` / `PendingErrors`
  queues for over-read responses and structured host errors, so
  unrelated RENDER errors interleaved with sync replies no longer
  cause the drain loop to hang.

### Known follow-ups (RENDER)

- ChangePicture forwards scalar attributes and explicit-None XID
  attributes (CPClipMask=None, CPAlphaMap=None) but drops non-None
  CPClipMask/CPAlphaMap because XID translation isn't wired yet —
  proper pixmap-clip / alpha-map mapping would require it.
- DestroyWindow should release any retained bg-pixmap host XIDs
  (`Window.background_pixmap_host_xid`); currently they leak on
  window destroy.
- Sub-window expose handling: when the host top-level is re-exposed,
  ynest doesn't re-paint fvwm3 sub-window backgrounds because the
  sub-windows themselves have no host backing. Currently fine because
  the host's own backing store keeps the rendered output, but tiled
  bg pixmaps wouldn't survive a forced expose.

## Phase 6.10 — Multi-monitor on KMS (complete)

Goal: drive every connected DRM connector as an independent X11 RANDR
output, laid out side-by-side in a single virtual screen. Validated
under `vng` with `virtio-gpu-pci,max_outputs=2`.

**Spec:** [`superpowers/specs/2026-05-07-phase6-10-multi-monitor-design.md`](superpowers/specs/2026-05-07-phase6-10-multi-monitor-design.md)
**Plan:** [`superpowers/plans/2026-05-07-phase6-10-multi-monitor.md`](superpowers/plans/2026-05-07-phase6-10-multi-monitor.md)
**Validation note:** [`superpowers/notes/2026-05-07-phase6-10-validation.md`](superpowers/notes/2026-05-07-phase6-10-validation.md)
**vng recipe:** [`superpowers/notes/2026-05-07-phase6-10-vng-recipe.md`](superpowers/notes/2026-05-07-phase6-10-vng-recipe.md)

### Working

- `discover_outputs(&Device) -> io::Result<Vec<Output>>` walks every
  connected connector with usable modes; pure `assign_outputs` helper
  greedily pairs each with an unclaimed CRTC + primary plane and
  hard-errors on stranded connectors.
- `KmsBackend.outputs: Vec<OutputLayout>` — each layout owns its own
  `Output`, `Swapchain`, virtual-screen `(x, y, width, height)`. Bring-up
  loops with rollback (any modeset or buffer-allocation failure tears
  down already-committed outputs in reverse order).
- Per-output paint loop in `composite_and_flip`: top-level windows
  pre-filtered by bbox intersection with each output's rect (avoids
  descending whole off-screen subtrees), origins translated by
  `(-layout.x, -layout.y)`, cursor drawn at scanout-relative coords.
- Page-flip events route via `crtc::Handle` from `drain_events` to the
  matching `OutputLayout`; one swapchain completion per CRTC.
- `RandrState` carries `Vec<RandrOutput>` + deduped `Vec<RandrMode>` +
  `primary_output` + aggregated `screen_*` / `*_mm`. Every RANDR
  handler (`RRGetScreenResources`, `RRGetOutputInfo`, `RRGetCrtcInfo`,
  `RRGetMonitors`, `RRGetOutputPrimary`, ...) reads from the new
  collections; `OUTPUT_ID`/`CRTC_ID`/`MODE_ID` consts removed.
- ID allocation per spec: outputs `1..=N`, CRTCs `(N+1)..=2N`, modes
  `2N+1..` with `(w, h, vrefresh)` dedup.
- ynest wire bytes preserved: single output `ynest-0`, output_id=1,
  crtc_id=2, mode_id=3, position (0, 0). xts Xrandr fixtures still
  green.
- `just yserver-multihead` recipe; GTK display backend (SDL collapses
  the second connector — see vng-recipe note).

### Validation gate (plan §5)

`xrandr -q` under `virtio-gpu-pci,max_outputs=2` with
`YSERVER_MODE=1024x768`:

```
Screen 0: minimum 2048 x 768, current 2048 x 768, maximum 2048 x 768
Virtual-1 connected primary 1024x768+0+0 27mm x 20mm
   1024x768      46.02*+
Virtual-2 connected 1024x768+1024+0 27mm x 20mm
   1024x768      46.02*+
```

`xrandr --listmonitors`: 2 monitors, first marked primary, both at
matching mm dimensions, second at `+1024+0` past the seam. xdpyinfo
root dimensions `2048x768`. Mode line shared (dedup verified).

ynest single-output regression: `xrandr -q` byte-identical to
pre-Phase-6.10. Single output `ynest-0 connected primary 1024x768+0+0
27mm x 20mm`.

### Out of scope (deferred to Phase 6.10.x)

- Real-hardware encoder/CRTC matching (Intel/AMD shared encoder pools);
  current greedy first-fit will strand connectors that share encoder
  pools.
- Hotplug (connector add/remove at runtime, KMS uevent drain,
  `RRScreenChangeNotify` fanout).
- Runtime mode switching (`RRSetCrtcConfig`).
- Mirror/clone mode.
- Overlay / cursor planes.
- Per-output EDID-derived physical mm (currently 96 DPI assumption).
- `YSERVER_LAYOUT` env override (default horizontal-by-enumeration is
  enough).
- xrandr-driven layout reconfigure.
- Bare-metal multi-output validation (Phase 6.10 is virtio-gpu-scoped;
  bare-metal is informational follow-up only).

## Phase 6.11 — Bare-metal session polish (in progress)

Started 2026-05-09 against real DP-attached AMD (bee, Beelink APU)
and an Intel iGPU laptop (fuji). Phase 6.10 closed the multi-monitor
RANDR shape on virtio-gpu; this slice focuses on the rough edges that
only surface when yserver is launched from a real TTY login on bare
hardware. No spec/plan doc — small, opportunistic fixes driven by
each `just yserver-…-hw` smoke.

### Landed

- **Console TTY takeover (commit `f4b4539`).** Ported the relevant
  half of `xf86OpenConsole` from `xserver/hw/xfree86/os-support/linux/lnx_init.c`:
  `KDSKBMODE = K_OFF` (with `K_RAW` fallback), `KDSETMODE =
  KD_GRAPHICS`, raw termios for the lifetime of the server. RAII
  guard restores all three on graceful exit / panic / signalfd
  shutdown. Skipped silently when the controlling TTY isn't a Linux
  VC (pty under SSH, graphical terminal emulator). Linux-only —
  `crates/yserver/src/lib.rs::run` panics at startup on other
  targets, which is honest about the existing DRM/KMS/libinput
  dependencies. Fixes: physical Ctrl-C inside an xterm killing the
  entire bare-HW session because the kernel keyboard layer was
  generating SIGINT on the controlling TTY in parallel with the
  evdev path. Confirmed working on AMD APU (bee) and Intel iGPU
  (fuji).

- **Real font catalog from fontconfig (commit `abcb9e6`).** Replaced
  the 20-entry hand-written `CURATED_XLFDS` and the terminator-only
  `list_fonts_with_info_proxy` stub with a real
  `build_font_catalog(&fc)` that calls `fontconfig::list_fonts` at
  `FontLoader` init and synthesises one XLFD per
  (face × pixel-size × charset) combination. Charsets limited to
  `iso8859-1` + `iso10646-1` (universal subset every scalable font
  satisfies through FreeType — locale-specific charsets stay absent
  rather than stubbed). `list_fonts_proxy` now glob-matches the
  catalog through `xlfd_pattern_matches` (case-insensitive shell-
  glob with `*` spanning dashes); `list_fonts_with_info_proxy` opens
  each match through `FontLoader::open_font` and emits real
  FreeType-derived metrics via a new sibling protocol helper
  `write_list_fonts_with_info_reply`. Mirrors `write_query_font_reply`
  field-for-field, so the LFWI metrics agree with later QueryFont
  metrics on the same XLFD. Unblocks xclock and any other Xt-based
  client whose `XCreateFontSet` was previously aborting with
  "Unable to load any usable fontset". Aliases `fixed`, `cursor`,
  `nil2` are kept as catalog entries — the loader handles them
  directly without an XLFD parse pass.

  ListFonts and ListFontsWithInfo handlers also gained pattern +
  reply-count debug logging so the next round of font diagnosis
  doesn't have to guess at what clients are asking for.

- **Diagnostic logging for cursor install path (commit `3577d6d`).**
  `handle_change_window_attributes` logs `CWA cursor: window 0xW ←
  cursor 0xC` whenever a client passes the `CWCursor` value-mask
  bit; `KmsBackend::define_cursor` logs the install with `(UNKNOWN)`
  flags on either xid that isn't tracked. Added to triage a
  reported "cursor stays as default arrow" regression on bare HW
  — used to narrow whether the failure is no-DefineCursor-issued,
  xid-mismatch, or render-side. Not a fix.

- **Core CreateCursor rasterization (commit `c7f0ed6`).**
  `KmsBackend::create_cursor` was a stub — allocated a host xid
  and returned without inserting into `self.cursors`. DefineCursor
  on that xid then silently bailed out of the composite let-chain
  (`self.cursors.get(&cursor_xid) → None`). Now reads the source
  (and optional mask) depth-1 pixmap mirrors back via
  `read_mirror_pixels` (R8_UNORM, 0xFF/0x00 per pixel), composes
  BGRA per X11 semantics — visible iff mask bit set (or always if
  no mask), fore where source bit set else back — and uploads to
  a fresh cursor mirror. Defensive on size mismatch (warn + treat
  as no-mask); proper BadMatch validation deferred to the core
  layer.

- **Core CreateGlyphCursor through FreeType (commit `1757918`).**
  `handle_create_glyph_cursor` used to register the cursor
  resource and return — never calling the backend, leaving
  `host_xid` unset so the CWA cursor handler quietly skipped
  `backend.define_cursor`. Every WM that uses
  `XCreateFontCursor` (almost all of them: e16, fvwm3, wmaker,
  xterm-without-Xcursor-theme) hit this path. New
  `Backend::create_glyph_cursor` trait method carries
  `(source_font, mask_font, source_char, mask_char, fore, back)`;
  `host_x11` forwards the wire request, KMS rasterizes both
  glyphs through `FontLoader` (FreeType `RENDER` mode), aligns
  them at glyph origins, and walks each cursor pixel placing
  source bits as fore/back inside the mask region. No-mask case
  treats source as both source and mask per spec (visible iff src
  bit set, color always fore). Hotspot is the source-glyph origin
  in pixmap coords, matching Xorg `dix/cursor.c::AllocGlyphCursor`.

  Together with `c7f0ed6`, both core cursor-creation opcodes now
  produce real cursor mirrors. Per-window I-beams over xterm text
  and resize cursors over WM frames now actually display on bare
  HW — confirmed on AMD APU (bee).

- **Justfile cleanup (commits `1a5c235`, `860b967`).** Dropped
  dead `scanout` parameter and `YSERVER_VK_SCANOUT={{scanout}}`
  plumbing — the env var stopped being consulted in `51739cc`
  ("retire legacy pixman scanout path") so all `*-hw` recipes were
  passing it through to no effect. Added `yserver-e16-xterm-hw`
  matching the existing `-fvwm3-xterm-hw` / `-wmaker-xterm-hw`
  shape.

- **e16 popup rendering on bare HW (commits `7ea2f09`, `4e99925`,
  `c5959af`).** Three coupled changes that together make e16's
  "thinking cloud" hover popup render correctly:

  - `7ea2f09` adds debug logging for every SHAPE minor opcode
    (Rectangles / Mask / Combine emit dest, op, kind, src/rects)
    and for `CWA bg_pixmap` with a None / ParentRelative / pixmap
    tag. Used to diff our wire trace against an `x11trace` of the
    same e16 session under Xephyr, which surfaced both follow-up
    bugs.

  - `4e99925` fixes depth-1 ZPixmap PutImage to unpack bits
    LSB-first per `bitmap_format_bit_order` advertised at setup
    (was MSB-first). Mismatch reversed each byte's 8 pixels,
    producing a per-byte sawtooth on shape masks → triangular
    spikes around the popup's perimeter.

  - `c5959af` honours the per-window SHAPE bounding region in the
    composite pass (was always pushing one quad covering the full
    window mirror, so pixels outside the shape rendered with
    bg-fill content — typically black) and adds a real
    `bitmap_to_yx_banded_rects` for SHAPE Mask sources via a new
    `Backend::read_depth1_pixmap` (KMS impl uses
    `read_mirror_pixels`; host-X11 stays best-effort). Previously
    `shape_mask_source_rects` returned the source pixmap's full
    bounding box, which made e16 see a single rect from
    GetRectangles where it expected ~7 bands and triggered a
    spurious `Mask Set src=None` recovery clear that then
    wiped the shape entirely.

  **Validated:** confirmed by photo on Intel iGPU laptop (fuji)
  — the cloud popup now shows the smooth rounded outline.

- **Ctrl-Alt-Backspace zap.** Bare-metal escape hatch when
  something gets stuck. The libinput thread tracks
  Ctrl/Alt-pressed state off the kernel evdev codes (LEFT or
  RIGHT) and, on a Backspace press while both are held, drops
  any pending pointer motion + the Backspace itself and sends
  `Message::Shutdown` straight into the core message channel —
  the same code path SIGINT/SIGTERM use, so the existing
  graceful-shutdown / DRM-master-release / console-restore path
  runs unchanged. Modifier tracking lives on the libinput
  thread because the X side may have a grabbing client
  consuming modifiers or a remapped keymap, and zap needs to
  fire even when X dispatch is wedged. Tests cover the four
  branches (zap fires; lone Backspace doesn't; modifier release
  disarms; right-side modifiers also arm).

### Follow-ups

Open items moved to [`known-issues.md`](known-issues.md): the
xeyes-on-e16 drag latency lives under the KMS-backend section
(already there since Phase 6.10); the benign xclock CJK-charset
warning lives under "Extension polish".

## Perf retrospective — `perf` branch experiment (2026-05-10)

Attempted to remove the per-RENDER-op `vkQueueWaitIdle` from
`run_one_shot_op` ("Cause 2" of the perf investigation) by
chaining submits via a per-context timeline semaphore (each submit
waits on the prior ticket and signals the next). Composite joined
the same chain. Eight commits on branch `perf` (kept as a
reference checkpoint).

### Cause-1 (idle composite gate)

- Status: **shipped on master** (`d56c63f`).
- Hardware A/B: 9.6 % → 0 % yserver CPU at idle, 0.6 % with an
  idle xterm. Confirmed live on TTY3.

### Cause-2 (per-op `wait_idle`) — negative result

- **Throughput**: indistinguishable from master on xts-Xproto
  (4 m04 s safe-mode vs 4 m06 s chain). rendercheck composite/
  cacomposite TIMEOUT in **both** modes — they're GPU-bound on
  this hardware regardless of CPU-side stalls.
- **Interactivity under load**: chain noticeably better — under
  rendercheck composite, mouse pointer was responsive in chain
  mode and visibly laggy in safe mode. Real second-order benefit
  but not enough to outweigh the correctness regression.
- **Correctness**: chain broke staging-buffer readback paths
  (`read_mirror_pixels`, `try_vk_get_image_pixels`). The
  `wait_ticket(returned_ticket)` after `run_one_shot_op` evidently
  doesn't actually block long enough for the GPU copy to finish
  on this driver — bytes are read pre-copy. Cascades into:
  - `xwd` returns all-white for windows that visibly contain
    text on screen.
  - SHAPE::Mask handling (which calls `read_depth1_pixmap`
    → `bitmap_to_yx_banded_rects`) decomposes garbage bytes
    into hundreds of phantom rects per window. Composite scene
    inflates from ~10 quads to 4000–8000+, blows past the
    `MAX_DESCRIPTOR_SETS_PER_FRAME = 1024` cap, every quad
    after slot 1024 silently dropped → only the first one or
    two windows render visibly.
  - Text rendering shows wrong glyphs (likely sampling stale
    glyph atlas via the same broken readback path).

### What we'd do differently

1. **Profile first**. The chain experiment proceeded on
   theory (per-op stall is the bottleneck) without measuring.
   `perf record -g` against rendercheck composite would've shown
   that the actual cost driver isn't `vkQueueWaitIdle` —
   composite/cacomposite are GPU-bound. The chain spent days
   addressing a non-bottleneck.
2. **Run with `YSERVER_VK_VALIDATION=1`** for any sync change.
   The diagnostic toggle (now on master) surfaces VUIDs in the
   release build at acceptable overhead. Would've caught the
   readback bug on the first hw run.
3. **Smaller scope, narrower intervention**. The chain touched
   every `run_one_shot_op` call site at once. A safer rollout
   would gate one specific request handler (e.g. `Composite`)
   on a feature flag, prove it works, then expand.
4. **Validation in real WM session, not synthetic**. xts and
   rendercheck both passed on the chain branch's release build
   in vng — neither caught the SHAPE::Mask cascade. The user's
   e16+xterm session did, instantly. Acceptance criteria for any
   future Phase-4.1.4.6-style work needs to include a real-WM
   smoke ("does e16+xterm look right after 30 s") before
   merging.

### Surface left on master

- `YSERVER_VK_VALIDATION=1` — opt-in Vulkan validation in release
  builds (`de0da83`).
- SIGUSR1 → `./yserver-scanout-N.ppm` dump (`a3e3e2b` +
  `ccb9f2a`). Useful for any future diagnosis of "what's actually
  in the scanout BO".
- known-issues entries for the K_OFF-after-crash recovery and
  VT-switch-blocked-while-running (both
  [`docs/known-issues.md`](known-issues.md)).

### Real next steps for perf

Open in [`known-issues.md`](known-issues.md) under "KMS backend":

1. `perf record -g` against rendercheck composite + xts-Xlib9 on
   hw, build a flame graph, pick the top hot path. Until that
   exists, all "the bottleneck is X" claims are guesses.
2. Persistent CB pool — `vkAllocateCommandBuffers` /
   `vkFreeCommandBuffers` per op is wasted CPU regardless of
   sync model. A small ring of reusable CBs cuts measurable cost
   without touching synchronisation.
3. Per-window descriptor caching — composite reallocates a fresh
   descriptor set per draw, but the `image_view` for a given
   window doesn't change between frames. A `(image_view → set)`
   cache + reset only on view-invalidation.
4. If a follow-up wants to retry the per-op-wait-removal idea, do
   it incrementally + with validation enabled + against an
   e16-style real session, not synthetic test suites.

## GLX/DRI3/XKB hardening session (2026-05-10) — wezterm shipped

Branch `fix/sync-fence-opcodes` landed seven fixes that took the
hw KMS backend from "glxinfo hangs" to "wezterm runs". Each fix
is grounded in an external source, not reasoning from first
principles — the recurring theme of the session.

### Fixes (in order of discovery)

1. **SYNC fence opcodes were shifted** (`525529e`). Our constants
   shipped as `Trigger=18, Reset=19, Destroy=15, Query=20,
   Await=21`. Canonical (xsyncproto.h + xcb sync.xml):
   `Trigger=15, Reset=16, Destroy=17, Query=18, Await=19`. Mesa's
   `xcb_sync_trigger_fence` was routing through our DestroyFence
   handler — it removed the fence record without triggering it,
   and Mesa hung in `xshmfence_await`. Regression test pins the
   constants.

2. **`TriggerFence` didn't propagate to backend** (`c86b1bd`).
   Even with correct opcodes, the handler only flipped
   `state.sync_fences[xid].triggered = true`. For DRI3-imported
   xshmfence-backed fences (Mesa's `FenceFromFD`), Mesa
   client-side waits on the futex-backed memfd via
   `xshmfence_await`. Server has to call
   `backend.dri3_trigger_fence(xid)` (which calls
   `xshmfence_trigger` → futex wake) for that wait to clear.

3. **DRI3::Open dup'd one server-held fd** (`f0361de`). Caused
   the second amdgpu client per session to segfault in
   `amdgpu_winsys_create` — libdrm_amdgpu keeps GEM-handle and
   context state per kernel struct file, so dup is wrong; every
   call has to `open()` the render-node path fresh, like
   `glamor_dri3_open_client`. We now keep
   `render_node_path: Option<PathBuf>` alongside the long-lived
   fd and re-open per call.

4. **XKB GetMap/GetNames/GetControls published empty placeholders**
   (`e49b6e7`). xkbcommon-x11 ran its validation and rejected
   the keymap (`numGroups=0`, `which=0`, wrong field offsets in
   GetControls). Rebuilt the three replies against
   `XKBproto.h` field offsets + xkbcommon's `FAIL_UNLESS` masks
   (objdump on `libxkbcommon-x11.so.0.13.1`). Two KeyTypes
   (`ONE_LEVEL`, `TWO_LEVEL`), per-key syms from xkbcommon,
   modifier-map keyed off level-0 keysyms.

5. **`xcb_xkb_get_device_info_reply_t` is 36 bytes, not 32**
   (`9682c83`). The C struct embeds `nameLen` (CARD16 at offset
   32) plus 2 bytes of trailing pad; clients cast the libxcb
   reply pointer to the struct and read past a 32-byte
   allocation. Caused OOB reads on the heap, which surfaced as
   garbage atom values in subsequent `GetAtomName` traffic. Fix:
   publish 36 bytes with `length=1`, explicit `nameLen=0`.

6. **xcb's `value_list_unpack` leaves absent-bitcase struct fields
   uninitialized** (`ccf6317`). xkbcommon-x11's `get_names`
   unconditionally reads `list.{keycodesName, symbolsName,
   typesName, compatName}` from a stack-local
   `xcb_xkb_get_names_value_list_t list;` — for `which` bits
   that aren't set, those fields hold stack garbage, which the
   atom interner dispatches as `GetAtomName(garbage)` requests.
   Each `BadAtom` flips the interner's `had_error` flag, and the
   keymap returns NULL. Fix: set bits
   `Keycodes|Symbols|Types|Compat (0x35)` on top of the existing
   `0xAC0`, and prepend four zero ATOMs (16 bytes) to the body
   so xcb writes zeros, atom-interner short-circuits on `atom ==
   0`, and the bogus traffic vanishes.

7. **Void XKB requests can't carry a reply** (`967319f`). Our
   `xkb_proxy` returned a 32-byte minimal reply for every minor
   we didn't model specifically, including void requests like
   `SelectEvents`. xcb's `xcb_request_check` asserts
   `!reply` (`xcb_in.c:757`), aborting wezterm-gui the instant
   it issues a checked `XkbSelectEvents`. Enumerated void minors
   per `xkb.xml` (`1, 3, 5, 7, 9, 11, 14, 16, 18, 20, 25`) and
   return `None` for them; reply minors we don't model still
   get the 32-byte placeholder.

### Outcomes

- glxinfo / glxgears: ✓ render on hw (fixes 1, 2, 3 sufficient).
- wezterm: ✓ runs on hw (all seven fixes required).
- vkcube / vkgears: window opens and survives setup, then hangs
  in `drmSyncobjTimelineWait` — Mesa-internal, not yserver. The
  GPU work radv issues just before the first
  `vkAcquireNextImageKHR` never returns its timeline value. Same
  symptom regardless of whether the swapchain went down the
  implicit-sync (`DRI3 FenceFromFD`) or syncobj path. Worth its
  own debugging session with `RADV_DEBUG=startup,info` against a
  symbolised libvulkan_radeon.
- vng-side: a separate yserver crash under vng is still
  outstanding when running vkcube — out of scope this session.

### What worked

The whole session moved on **external-source grounding** —
`xsyncproto.h`, `xcb/sync.xml`, `XKBproto.h`,
`/usr/share/xcb/xkb.xml`, gcc `sizeof()` against the installed
xcb headers, objdump on `libxkbcommon-x11.so.0.13.1`, and the
xkbcommon-x11 source pulled from upstream tarball. The
`feedback_test_vectors_must_be_external` memory paid off
several times — the first "fixed it, tests pass" XKB attempt
shipped tests that asserted my own assumptions, and didn't
actually fix wezterm. The follow-up that read xkbcommon's
`get_names` source directly identified the real bug
(unconditional reads of unpopulated struct fields) in one pass.

### New durable lessons (memory)

- `feedback_xcb_extension_reply_traps.md` — the three xcb
  client-side gotchas (uninit unpack fields, struct size > wire
  fixed header, void-checked aborts). Applies to any future
  extension proxy.
- `feedback_dri3_open_fresh_fd.md` — fresh open, not dup, per
  DRI3::Open. Cross-references `glamor_dri3_open_client` for
  authority.

### Real next steps

1. Vulkan WSI hang (`drmSyncobjTimelineWait` after first
   `vkAcquireNextImageKHR`). Needs `RADV_DEBUG=info,startup`
   output + a libvulkan_radeon-with-symbols backtrace to pin
   which radv internal sync is parked. Two distinct fixes
   prepared on this branch in the lead-up to the XKB path didn't
   help here, so the wait is on Mesa-side state our DRI3
   imports haven't satisfied — most likely the buffer's
   reservation_object having an unresolved write fence from our
   Vulkan import path.
2. vng-side server crash on vkcube — separate triage; reproduce
   under `vng` and capture the panic / coredump.

## Late-night session — vkcube renders, xeyes works, gtk3-demo starts (2026-05-10 → 2026-05-11)

Continuation of the GLX/DRI3/XKB hardening day. Same external-source-
grounding playbook (`/usr/share/xcb/*.xml`, `xkb/XKBMAlloc.c`,
`libX11/src/xkb/XKBGetMap.c`, GTK's `gdkkeys-x11.c`) carried six more
fixes through the input + drawing stack. Each one of these would have
been Xorg parity by inspection; they were never going to surface
without a client that exercised the specific path.

### Fixes

1. **`PresentPixmapSynced` body length 92 → 84** (`80f5376`). Our
   parser invented a 4-byte pad after `release_syncobj` that doesn't
   exist in `/usr/share/xcb/present.xml`. Mesa anv WSI shipped a
   canonical 84-byte body and we rejected every first frame as
   `BadLength`. vkcube + vkgears hung in `drmSyncobjTimelineWait`
   because the swapchain's `xcb_wait_for_special_event` thread never
   received a `PresentCompleteNotify`.

2. **VK_EXT_image_drm_format_modifier always-on for LINEAR**
   (`80f5376`). The explicit-modifier path was skipped for
   `DRM_FORMAT_MOD_LINEAR`, dropping the client's `stride`. Mesa anv
   allocates 300×BGRA8 swapchain BOs at `stride=1280` (vs the 1200
   our import auto-computed), so every row drifted +80 bytes —
   diagonal smear on Intel iGPU. bee/radv happened to compute a
   matching pitch, which is why the same code looked fine there.

3. **Wire-constant audit punch list** (`839054b`). One systematic
   sweep against the canonical XML for every extension we publish.
   Caught:
   - `XFIXES ChangeCursorByName` was opcode 23 (which is actually
     `SetCursorName`); canonical is 27. We were dispatching
     `SetCursorName` traffic through our `ChangeCursorByName` arm.
   - `GLX_BadRenderRequest` shipped as 0 (= `BadContext`); canonical
     is 6. Active in the dispatcher, so every unsupported indirect-
     rendering minor reported the wrong error code.
   - `GLX_UnsupportedPrivateRequest` shipped as 11
     (= `BadCurrentDrawable`); canonical is 8.
   - `GLX CopyContext` shipped as 12 (= `UseXFont`); canonical is 10.
   - XKB host_x11 `xkb_minor_has_reply` listed void minors 14, 16, 18,
     20 (`SetIndicatorMap`/`SetNamedIndicator`/`SetNames`/
     `SetGeometry`) as reply-bearing; would block on synthetic replies
     that can't come.
   Canonical pin tests added so re-regression is locked.

4. **xeyes — depth-1 PolyFill R8 channel + QueryPointer window-relative
   coords** (`dc7b58d`).
   - `try_vk_solid_fill` hard-coded the X11 0xRRGGBB unpack into
     `[R, G, B, A]`. Correct for `B8G8R8A8_UNORM` mirrors but writes
     zero into the R channel of `R8_UNORM` mirrors (depth-1 shape
     masks, depth-8 alpha masks). xeyes draws its elliptical mask
     with `ChangeGC fg=1; PolyFillArc` onto a depth-1 pixmap, and
     every pixel was being stored as 0. The cascade was
     invisible-xeyes: empty bitmap → `bitmap_to_yx_banded_rects`
     returns 0 rects → xeyes's bounding shape empty → WM's
     `SHAPE::Combine` mirrors an empty shape into the WM frame →
     `walk_subtree_into_draws` early-returns on
     `shape_bounding empty` *without* descending into children → no
     quads in the composite scene. Fix: branch on `mirror.format` so
     R8_UNORM mirrors put the fg byte in `color[0]`.
   - `QueryPointer` returned the cursor's root-absolute coords as
     both `root_x/root_y` *and* `win_x/win_y`. Worked at window
     origin (0, 0); broke as soon as the WM dragged the window.
     Fix: parse the window xid from the request body and subtract
     `state.resources.window_absolute_position(window)` from the
     cursor's absolute position. xeyes iris now tracks correctly
     through window drags.

5. **Server-side key auto-repeat** (`353c1b0`). yserver was
   advertising `global_auto_repeat=1` + `RepeatKeys` in
   `GetKeyboardControl` / XKB Controls but never generated repeat
   events — libinput emits one press/release per physical event and
   the consumer has to fire repeats. xterm/wezterm saw a single key
   per press and didn't fall back to client-side repeat because
   we'd promised to handle it. Now: a single `KeyRepeatState` on
   `ServerState` (X11 mandates only the last-pressed key repeats),
   `handle_host_input` arms / replaces / clears on key events,
   the mio poll uses a finite timeout = `next_fire - now` while a
   key is held (idle server still uses `None` — no idle wakeups,
   §"Cause-1 idle composite gate" still wins), and end-of-iteration
   `fire_pending_repeats` fans out paired KeyRelease+KeyPress in
   `REPEAT_PERIOD = 40 ms` steps after a `REPEAT_INITIAL_DELAY =
   660 ms` hold. Classic mode (no `XkbPerClientFlags`
   `DetectableAutoRepeat` opt-in); every client handles it. Defaults
   match `xset -r`; pulling delay/rate from the XKB Controls block
   for live `xset r rate N M` is a small follow-up.

6. **XKB `nTypes ≥ XkbNumRequiredTypes` (= 4)** (`7065eda`). GTK3
   uses Xlib's classic `XkbGetMap()`, which internally calls
   `XkbAllocClientMap(xkb, mask, rep->nTypes)`; that helper's
   precondition in `libX11/src/xkb/XKBMAlloc.c:46-48` rejects any
   `nTotalTypes < XkbNumRequiredTypes` (= 4) with `BadValue`.
   `_XkbReadGetMapReply` propagates `BadAlloc`, `XkbGetMap` returns
   `NULL`, GDK fires `g_error("Failed to get keymap")` and gtk3-demo
   aborts. We shipped `nTypes = 2` (ONE_LEVEL + TWO_LEVEL); X11
   reserves the first four indices (ONE_LEVEL / TWO_LEVEL /
   ALPHABETIC / KEYPAD) and every server is required to publish at
   least the four. xkbcommon-x11 doesn't enforce the minimum (just
   unpacks what's there via xcb's iterators), which is why wezterm
   and the morning's hardening commits cleared validation while
   GTK3 didn't. Fix: `reply_get_map` ships 4 types (two real +
   two minimal placeholders); `reply_get_names` mirrors with
   `nTypes=4`, `nLevelsPerType=[1, 2, 1, 1]`, matching ATOM-list
   sizes — required so xkbcommon-x11's
   `FAIL_UNLESS(reply->nTypes == keymap->num_types)` still holds
   for the wezterm path. Verified by a tiny direct-Xlib probe
   (`./test-xkbgetmap`) returning `XkbGetMap OK` against yserver.

7. **RENDER ARGB32 glyphsets accepted** (`1cf8b25`). gtk3-demo
   reached its main window after the XKB fix but every widget came
   up empty. Diagnosis chain: instrumented
   `try_vk_render_composite_glyphs` with per-bail reason logs
   (`vk text bail: ...`) — all 184 calls passed every early-return
   gate. A per-call summary then showed two distinct shapes:
   `seen=N missing=0 pushed=N` for A8 sets (working);
   `seen=N missing=N pushed=0` for ARGB32 sets (every glyph absent
   from `active_gs.glyphs`). One `render_create_glyphset` log line
   pinned it: `client_format=0x4 -> Other`. GTK3/Pango/Cairo
   defaults to ARGB32 glyphsets for body text under modern themes
   (so the format can carry colour glyphs / subpixel coverage);
   `parse_add_glyphs` was silently dropping every `Other`-format
   upload. Fix: new `GlyphSetFormat::Argb32` variant,
   `parse_add_glyphs` accepts ARGB32, extracts the alpha channel
   from each 4-byte BGRA pixel into a densely-packed A8 buffer,
   stores the glyph as `format=A8` (so the downstream atlas + text
   pipeline path is identical from there on — no shader change).
   Subpixel detail and emoji colour are lost; glyph mask shape
   survives, which is enough for grayscale text on any background.
   Standard Xorg fallback when the dst can't carry the glyph's
   full channel data.

### Surface left on master

- Diagnostic breadcrumbs (all only fire on bail / once per init, no
  per-call noise) for the RENDER glyph + fill paths:
  `vk text bail: ...` (six reasons),
  `render_create_glyphset: client_format=0xN -> {fmt:?}`,
  `parse_add_glyphs bail: format=...`,
  `render_fill_rectangles bail: ...`,
  `vk copy diag: ...` (four reasons in `try_vk_copy_area`).
- `walk diag: skip xid=... (reason)` in `walk_subtree_into_draws`
  (kept the skip path; stripped the per-walk per-window line).
- `test-xkbgetmap.c` + `test-xkbgetmap` in the project root — minimal
  Xlib-only probe that asserts `XkbGetMap` returns non-NULL. Useful
  regression check for XKB GetMap parsing without needing a full
  GTK app.

### Memory updates

- New: `feedback_wire_format_external_source.md` — every wire-format
  / opcode / struct ABI / driver-interop layout must be grounded in
  the canonical external source (`/usr/share/xcb/*.xml`, proto
  headers, `objdump` against the linked client, upstream client
  source). Today produced ≥ 13 distinct bugs of this exact shape;
  every one was findable in one pass from an external source and
  invariably wrong when reasoned from scratch.

### Known issues, deferred

- **gtk3-demo TreeView left-pane labels still missing**. Right-pane
  text (description paragraphs, `Application Class` heading) renders
  fine after the ARGB32 fix; the `Run` toolbar button label renders
  inside its button; but the demo-category labels next to the
  expander arrows in the left pane are absent or displaced.
  Initial diagnostic showed glyphs reaching the atlas, so it's a
  *placement* issue, not an upload one. Two small block-shaped
  artifacts at the bottom-left and bottom-right of the scanout in
  the diagnostic dumps are suspected displaced labels but
  unconfirmed. Next session.
- **ARGB32 colour glyphs / subpixel detail lost**. Today's fix
  downgrades ARGB32 to A8 via the alpha channel. Emoji and LCD-
  subpixel-rendered text will show shape but no colour / no
  per-channel coverage. Proper support needs a second atlas
  (`B8G8R8A8`) and a colour-glyph text pipeline variant.
- **`XkbPerClientFlags` reply is a 32-byte zero placeholder**.
  `DetectableAutoRepeat` opt-in is unsupported (we always fan out
  paired Release+Press); fine because classic auto-repeat works
  universally and our key-repeat implementation generates the right
  pairs already.

## vng GL support — use zink, not virgl (2026-05-11)

wezterm worked on bare-metal (radeon + intel iGPU) but rendered as
a uniform black rectangle under vng. Same symptom for glxgears.
Vulkan apps (vkcube) worked fine under vng. Investigated the DRI3
path top-to-bottom; the actual fix is a one-line env-var change.

### Diagnosis

1. **First symptom — `BadAlloc` on DRI3::PixmapFromBuffers.** Mesa
   error: `dri3_alloc_render_buffer:1691 xcb_dri3_pixmap_from_buffer[s]
   failed; X error: 11`. Expanded the BadAlloc log in
   `process_request.rs` to dump the (modifier, stride, offset,
   width×height, depth, bpp) tuple and the actual modifier values
   returned from `GetSupportedModifiers`.

2. **Pinned the rejection inside `from_dmabuf`**: `vkCreateImage` with
   `VkImageDrmFormatModifierExplicitCreateInfoEXT(modifier=LINEAR,
   row_pitch=3280)` returns
   `ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`. wezterm
   client picked LINEAR (the only window-mod we advertise), and
   `820 × 4 = 3280` is tight — 16-byte aligned, not 256.

3. **Probed RADV's actual requirement** via a one-off in-process
   sweep (image-create-and-destroy at strides 3280 / 3296 / 3328 /
   3584 / 4096 with various usage flags). RADV's LINEAR layout for
   B8G8R8A8 wants **256-byte pitch alignment**. 3328 onward passes
   with `mem_align=256`; 3280 and 3296 fail. Reduced usage flags
   don't relax the requirement. Implicit-LINEAR `vkCreateImage`
   (no modifier chain) accepts stride=3280 but lies — reports
   `layout.row_pitch=3328` for a BO laid out at 3280, which would
   produce vertical-smear at sample time (the inverse of the
   anv 1200→1280 bug `80f5376` fixed). On bare-metal both sides
   share addrlib so this never fires.

### Built a sw-copy shadow fallback, then reverted it

Implemented a fallback path: on the `INVALID_DRM_FORMAT_MODIFIER_
PLANE_LAYOUT_EXT` failure for LINEAR, allocate a fresh server-
owned LINEAR `HOST_VISIBLE` `VkImage` at RADV's preferred pitch,
mmap the client's dma-buf, and `memcpy` row-by-row with stride
translation. Bracketed the memcpy with `DMA_BUF_IOCTL_SYNC`
(`SYNC_START`/`SYNC_END`) for kernel-level GPU→CPU sync. Hook
in `copy_area` so any pixmap→window blit (Present's main path)
refreshes the shadow before reading. Cleanup in `free_pixmap`.

The mechanism worked — DRI3::PixmapFromBuffers stopped returning
BadAlloc, the shadow allocation succeeded, refresh fired on each
Present. But wezterm's window was uniformly **black** instead of
showing terminal content. Diagnostic sampled the mmap'd dma-buf
bytes after `DMA_BUF_IOCTL_SYNC(SYNC_START | READ)` returned 0:
**all bytes were `0x00000000`** across the entire image. virgl's
host GPU writes never made it to the guest-visible dma-buf
mapping at all.

The qemu stderr explained why:
```
vrend_check_no_error: context error reported 5 "wezterm-gui" Unknown 1286
context 5 failed to dispatch DRAW_VBO: 22
vrend_decode_ctx_submit_cmd: context error reported 5 "wezterm-gui" Illegal command buffer 786440
```

`virgl_renderer` on the host was rejecting wezterm's GL command
stream as malformed/unsupported. The dma-buf stayed zero-init
because the host GPU never actually rendered anything into it.
That's a virgl_renderer feature-coverage gap, not anything the
X server can fix from its side.

The shadow path got reverted (git checkout HEAD --) since it
fixes a symptom (RADV's strict LINEAR pitch) that doesn't apply
once the real root cause is addressed. ~600 lines net deleted.

### Actual fix: `MESA_LOADER_DRIVER_OVERRIDE=zink`

Mesa in the guest has two GL drivers:

- **virgl** (default): GL → custom wire protocol → host
  `virgl_renderer` → host GL stack. Has feature gaps; rejects
  wezterm/glxgears command streams.
- **zink**: GL → Vulkan (in-guest, by zink's GL state-tracker) →
  Venus protocol → host Vulkan (RADV/anv/...) → host GPU.
  Bypasses `virgl_renderer` entirely.

With `MESA_LOADER_DRIVER_OVERRIDE=zink`, wezterm renders correctly
under vng — text visible, btop runs inside it, regular DRI3
dmabuf import path (no shadow needed because both sides are now
Venus → modifier+stride round-trip is consistent). vkcube already
worked because it was Vulkan all the way. glxgears + vkgears now
also work under the same recipe.

### Surface left

- **vng recipes** in `Justfile` (`yserver-fvwm3-xterm`,
  `yserver-glxgears`, `yserver-e16-xterm`, `yserver-wmaker-xterm`)
  export `MESA_LOADER_DRIVER_OVERRIDE=zink` before launching
  clients. New recipe `yserver-e16-wezterm` for the canonical
  vng+wezterm session.
- **Expanded BadAlloc + GetSupportedModifiers logs** kept on the
  `process_request.rs` DRI3 path. Useful diagnostic baseline for
  any future allocator-mismatch issue (modifier list + the full
  request tuple visible on import failure).
- **Hw path is untouched.** Preflight + shadow code is gone;
  `from_dmabuf` and `dri3_import_pixmap` are back to their
  pre-investigation state. The expanded log lines are
  diagnostic-only; they don't change behaviour.

### Memory note

`reference_vng_use_zink.md` — recipe + reason. Includes the
explicit "don't try sw-copy fallback; that was a wrong-layer fix"
note so future-me doesn't re-walk this road.

## xfwm4 cleanups — XSync 3.1 + X-Resource stub (2026-05-11)

xfwm4 on hw logged `XSync extension too old (3.0).` and `The
display does not support the XRes extension.` Neither was fatal,
xfwm4 just falls back, but both are easy to silence.

- **XSync 3.1.** We were advertising `3.0` from the `Initialize`
  reply even though all the 3.1 fence requests (opcodes 14–19) are
  already wired (525529e, the GLX/DRI3 hardening day). One-line
  bump in `crates/yserver-protocol/src/x11/sync.rs` to set
  `MINOR_VERSION = 1`. Matches `/usr/share/xcb/sync.xml`.

- **X-Resource extension stub.** New extension at major opcode 149.
  New protocol module `crates/yserver-protocol/src/x11/x_resource.rs`
  with version 1.2 constants, opcodes 0–5, and reply encoders that
  return well-formed but empty/zero counts. Dispatcher arm in
  `process_request.rs`. Registered in `nested.rs` EXTENSIONS. xfwm4
  + lxqt-panel + plasma-applet-systemload now see XRes "present"
  instead of "absent"; tools like `xrestop` will run but display
  a blank table (no real resource accounting). That's the same
  observable behaviour as a real X server with XRes builtin but
  no clients tracked; just without the warning. Canonical layout
  per `/usr/share/xcb/res.xml`.

## MATE desktop comes up — partial render (2026-05-11)

First serious test of full MATE on yserver-hw. Three protocol-level
bugs fixed, mate-session bootstraps, mate-panel renders to the
screen, then the visible rendering exposes deeper issues that didn't
trip on simpler WMs (fvwm3 / xfwm4 / e16 / wmaker / gtk3-demo).

### Fixed

1. **`fix(req)` — exact-length validator now accepts BIG-REQUESTS**
   (`17eeb26`). `validate_exact_request_length` compared the client's
   `length_units` against `exact_required_length()`'s output without
   accounting for the extra header unit that BIG-REQUESTS inserts.
   For a BIG request, the wire is `length_units*4 - 8 == body.len()`
   (8-byte header: 4-byte normal header + 4-byte extended length),
   not `length_units*4 - 4`. Detected from the body/length-units
   relationship; bump the required by one when BIG is used.

   Without this: every `_NET_WM_ICON`-sized ChangeProperty (~256 KB)
   from any GTK client got `BadLength`. mate-panel + caja both
   `Gdk-WARNING: poly request too large` and died, then respawned
   in tight loops via session manager. With the fix, zero BadLength
   errors across a full mate-session run.

2. **`fix(randr)` — pixel→mm formula off by 10×** (`89f1136`). The
   integer-math formula for per-output / per-screen mm dimensions
   used divisor 9600 and rounding bias 4800; the correct values for
   96 DPI are 960 and 480 (mm = px * 25.4 / 96 → (px*254 + 480)/960).

   Net effect: yserver advertised a 2560 px monitor as **68 mm**
   wide, which clients infer as **≈ 956 DPI**. GTK's auto-scale
   rounds that to a 10× multiplier, so mate-panel laid out content
   for a "logical" ~256 px display and only painted the leftmost
   1136 px of the actual 2560 px panel window. SIGUSR1 scanout
   verified before/after — panel now paints edge-to-edge.

   Three call sites + two tests updated; the setup_thread.rs
   `1024 px → 677 mm` reference scaler already used the right ratio.

3. **`just(mate)` — full mate-session bootstrap** (`cad37bb`).
   `yserver-mate-hw` was launching bare `marco` + `wezterm`. Bare
   marco from a TTY crashes inside libX11's `XQueryExtension` /
   `_XInitDisplayLock` due to an `unsetenv()`-after-threads heap
   corruption that's an environment issue on this Arch + cachyos
   host (reproduces against vanilla Xephyr too — `tools/marco-xephyr.sh`
   was written to prove this and stays around).

   Recipe now goes through `dbus-run-session mate-session --display :7`
   with `WAYLAND_DISPLAY` / `WAYLAND_SOCKET` stripped and
   `GDK_BACKEND=x11` / `XDG_SESSION_TYPE=x11` forced — otherwise
   GTK auto-detects Wayland and `gdk_x11_window_get_xid()` assertion-
   fails inside mate-settings-daemon. Companion `yserver-mate` recipe
   added for vng (virtio-vga-gl + Venus + zink) iteration.

### Diagnostic surface left

- **`exact-length BadLength` diagnostic at the request-length emit
  site in `process_request.rs`.** Dumps opcode, header.data,
  length_units, body.len, computed-required, and a 64-byte body
  preview. Caught today's BIG-REQUESTS off-by-one in one log line.

- **`ChangeProperty BadLength` per-branch diagnostic** in
  `handle_change_property` (parse-fail and length-mismatch paths).
  Redundant with the request-length one for ChangeProperty
  specifically, but the parse-fail branch is more granular about
  *which* field parsing fell off.

- **`tools/marco-{gdb,watch,xephyr}.sh` + `tools/marco-gdb-watch.gdb`**
  (`155efe7`). The whole hardware-watchpoint-on-libX11-state and
  marco-under-Xephyr setup, ready for re-use.

### Visible after the fixes

mate-session bootstraps cleanly, marco runs without crashing,
mate-panel sizes correctly (2560 px wide on the primary monitor at
sensible icon height), caja launches, mate-settings-daemon configures
cursor theme, system tray populates (network, sync, weather, security
icons). Second monitor (HDMI-A-1) is on, cursor crosses freely
between outputs; default MATE config doesn't put a panel on the
second screen.

### Known issues, deferred

- **GTK widget flicker (irregular, rapid).** Only GTK widgets
  flicker — gray desktop and static regions are stable. With marco's
  compositor disabled (`gsettings set org.mate.Marco.general
  compositing-manager false`) the panel doesn't draw at all (likely
  needs marco compositor for ARGB transparency) but the notification
  popup still flickers — so the flicker isn't two-compositors
  competing, it's in our RENDER / glyph / damage path itself. Same
  family of bugs as yesterday's gtk3-demo work. No `vk text bail`
  / `parse_add_glyphs bail` / `render_fill_rectangles bail` lines
  in the run, so we're not falling into the obvious sw-fallback
  paths. Next session: add per-frame RENDER::Composite logging on
  the notification window, or x11trace the notification daemon.

- **Text-rendering residuals** (carry-over from `1cf8b25` ARGB32
  glyphset accept). User flagged "bad text rendering like residuals
  of the gtk3 demo fixes we did yesterday — and weren't fully fixed
  yet". TreeView left-pane labels still missing per yesterday's
  notes; same bug now visible on mate-panel + notification text.

- **COMPOSITE BadAlloc** (1 error in the run, mate-panel client
  on resource `0xe00041`). One-off. Likely a redirect-pixmap path
  we don't fully model. mate-panel proceeds despite it.

- **XFIXES minor=22 unhandled** — mate-panel calls it ≥ 5×. Modern
  XFIXES request (1.4+ era). Returns a `debug!("XFIXES::unknown")`
  but no error; possibly causing mate-panel to compute the wrong
  visible region for its applets. Worth implementing as a stub
  next session.

- **Compositor 3-deep BO pool / cursor on second monitor.** Earlier
  in the day the cursor showed up "huge and flickering" on a smaller
  notification-shaped window; that resolved once DPI math was fixed
  and marco's compositor took over (mate-panel needs it for ARGB).
  Still — multi-monitor cursor handoff during composite hasn't been
  smoke-tested end-to-end.

### Memory updates

- Updated `feedback_wire_format_external_source.md` applies again —
  the BIG-REQUESTS unit-count behavior is in `bigreq.xml` /
  `xproto.xml` and the X.org server's `bigreq.c`; we re-derived it
  from log evidence instead of checking the canonical source first.

## gtk3-demo TreeView labels — pen off by xSrc/ySrc (2026-05-11)

Targeted bisect session against `yserver-e16-xterm-hw` + gtk3-demo
launched from wezterm. The visible bug across MATE, gtk3-demo, and
xfwm4 was missing / displaced GTK widget text (TreeView row labels
invisible, "Run" button label below+right of its button). Two
distinct root causes surfaced; one fixed today.

### Root cause — CompositeGlyphs pen initialised to xSrc/ySrc

`try_vk_render_composite_glyphs` set the dst pen to `src_x + x_off`
/ `src_y + y_off`. X RENDER protocol semantics:

- `xSrc` / `ySrc` are the **source picture sampling origin** (used
  when the source is a Drawable / Gradient; no-op for SolidFill).
- The dst pen starts at (0, 0); the first glyph element's
  `deltax` / `deltay` sets the **absolute** pen position;
  subsequent elements accumulate.

GTK conventionally sends `xSrc==first_deltax` and
`ySrc==first_deltay`. Our impl added them, so every glyph rendered
at `(2 × xSrc, 2 × ySrc)`. The visual effect was a uniform
displacement of every glyph by `(xSrc, ySrc)` — invisible in most
contexts (right-pane title still rendered, just shifted), but
catastrophic for the gtk3-demo TreeView:

1. `FillRect cyan` at `y=0..21` (selected row bg)
2. `CompositeGlyphs xSrc=22 ySrc=15 first_delta=(22,15)` — should
   pen at (22, 15), instead pen at (44, 30). Label image lands at
   `y=22..29` (inside row 2, not row 1).
3. `FillRect dark y=21..42` (row 2 bg) — correctly fills row 2,
   wiping the displaced label that landed there.
4. Same pattern for every subsequent row.

Result: every row's label vanished into the next row's FillRect.
The selected row was the only one where the label survived (because
the cyan FillRect ran before the label) — but it appeared just
below the cyan bar instead of inside it.

Fix: `crates/yserver/src/kms/backend.rs` `try_vk_render_composite_glyphs`
initialises `pen_x = x_off`, `pen_y = y_off` (both 0 from the
dispatcher). `src_x` / `src_y` are now correctly used only for
source picture sampling — currently a no-op in our SolidFill-only
glyph path, kept in the function signature for trait parity.

### Diagnosis path — what worked

Standard PPM probes ended up being the right tool but only after a
detour. Useful steps in order:

1. **`xtrace` against gtk3-demo** showed every CompositeGlyphs +
   Composite from the client side. Reconstructed the render graph:
   labels render into ARGB32 pixmap 0x3a (Picture 0x3b), then a
   single `Composite Over src=0x3b dst=0x34 219×598` blits the
   pane to the main window backing.
2. **Bail diagnostics in `try_vk_render_composite`** —
   one `log::debug!` per silent `return false` site. Surfaced the
   e16 pager flicker as a separate bug (24 bails on
   `0x4000ad → 0x4000af 336×48 op=Src` with src pixmap freed). The
   gtk3-demo labels Composite **didn't bail** — moved the
   investigation past the Composite into glyph rendering.
3. **Per-call entry log in `render_composite` and
   `render_composite_glyphs`** mapped each call to its dst kind /
   depth / source color. Established: glyph runs onto depth-24
   pixmaps + windows render correctly; runs onto the depth-32
   TreeView offscreen pixmap don't.
4. **PPM probes** of dst mirrors after first / 2nd / 5th / 20th
   glyph run and after each FillRect into 0x400128. The decisive
   sequence: `glyph-probe-run01` shows "Application Class" white
   on cyan; `fill-probe-run03` (immediately after the FillRect for
   row 2) shows the label is gone (unique_R drops from 154 to 3).
   That pinpointed the FillRect for row 2 as the eraser — even
   though geometrically `(0, 21, 219, 21)` shouldn't touch row 1.
   The "shouldn't" was the clue: the label was actually IN row 2.

### Mechanic that made the bug invisible until this client

The displacement is **uniform** for every CompositeGlyphs. Right-
pane title, body, tab text, wezterm — all shifted by the same
`(xSrc, ySrc)`. Users (and we) didn't notice because:

- Text moves DOWN by ySrc within the same pixmap. As long as the
  shifted text is still within the pixmap and isn't immediately
  overwritten, it looks "correct enough".
- TreeView is the unique case where text shifted by exactly one
  row pitch lands inside the NEXT row's geometry, which is
  guaranteed to be FillRect-painted. The same effect would also
  bite list boxes, menus, and any widget where consecutive
  FillRect-then-CompositeGlyphs operations alternate on stacked
  rectangles.

### Surface left on master

- **Bail diagnostics in `try_vk_render_composite`** kept. These
  surfaced the e16 pager flicker root cause (`vk composite bail:
  src mirror not sampleable`) and are cheap (only fire on error
  paths). Useful for future "Composite silently does nothing"
  debugging.
- **All inline PPM probes and per-call entry logs reverted.**
  PPM dumps were the right tool for this session but too noisy
  for routine debug.

### Separate bug — e16 pager flicker (open)

Same investigation surfaced a second bug: 24 + N composite bails
per session on `src_pic → dst_pic 336×48 op=Src` with both
`in_windows=false` and `in_pixmaps=false` for the source pixmap.
e16 pager refreshes follow a "create thumbnail pixmap → Composite
to pager cell → free pixmap" pattern; the FreePixmap arrives at
yserver before the Composite is dispatched, so the source mirror
is gone by the time `try_vk_render_composite` looks it up.

Two reasonable fixes:
1. Reference-count pixmap mirrors against Pictures + GCs (X11 spec
   semantics — drawable's lifetime extends until last reference).
2. Quick: keep the Vk mirror alive for one composite frame after
   FreePixmap, then drop. Crude but matches the typical "use just
   after free" pattern.

Tracked for next session.

### Memory updates

- New: `feedback_compositeglyphs_xsrc_not_pen.md` — the protocol
  lesson. xSrc/ySrc sample the source picture; pen starts at (0, 0).
- Updated `project_mate_desktop_partial.md` — GTK widget flicker
  bullet now points at the CompositeGlyphs pen fix as the likely
  root cause; needs MATE re-test to confirm. Added e16 pager
  flicker as a separate open item.

## MATE clicks investigation — popup grab semantics (2026-05-11)

Bisect session against `yserver-mate-hw`. After the CompositeGlyphs
fix, MATE labels render correctly and **hover effects work** (tray
applets respond to enter/leave). But **clicking panel items does
nothing** — except the calendar applet "reacted once".

### Click delivery: working

Promoted several pointer-event logs from `trace` to `debug` and
captured a click session. Each `libinput button` event:

- Reaches yserver from libinput ✓ (`code=0x110 pressed=true → X11
  detail=1`).
- Routes through `pointer_event_fanout_to_state` with correct hit
  target.
- Is delivered via XI2 to the right client(s). Example:
  `pointer_fanout XI2: kind=ButtonPress target=0x2200003
  top_level=0x1000003 xi2_targets=[16, 34] root=(2520,5)
  event_xy=(149,5)` — mate-panel (client 16) and a tray applet
  (client 34) both receive the event.

GTK clients use XI2 exclusively for input. Our `xi2_mask_for_client`
correctly walks the `[2, 1, 0]` device-candidate list and matches
mate-panel's `XISelectEvents window=0x1000001 deviceid=1
mask=0x1c01f0` against the dispatched event. No part of the input
or fanout path is the bug.

### What the calendar click revealed

```
ButtonPress  target=0x2200003 (calendar applet)   xi2_targets=[16, 34]
ButtonPress  target=0x2200003                     xi2_targets=[16, 34]
   ↓ client 43 maps a new top-level 0x2b00003 (calendar popup overlay)
ButtonPress  target=0x2b00003 root=(0,0)          xi2_targets=[43]
ButtonPress  target=0x2b00003 root=(31,21)        xi2_targets=[43]   ← clicks now stick here
ButtonPress  target=0x2b00003 root=(53,19)        xi2_targets=[43]
…
```

The first calendar click opens the popup (`client 43` maps
`0x2b00003`, a full-screen transparent input-shield typical of GTK
popups). From that moment, `event_xy == root_xy` confirming the
popup is at `(0, 0)` and covers the whole screen. The popup is the
topmost mapped window over every `root=(x, y)` we see, so our
hit-test correctly routes all subsequent clicks to it.

**The popup never unmaps.** `0x2b00003` is `MapWindow`'d once in
the entire log and we see no matching `UnmapWindow`. GTK's
convention is that a click *outside* the visible popup widget
(but inside the shield) should make the popup dismiss itself. That
dismissal isn't happening on our server.

### Hypothesis: active-pointer-grab semantics missing

The most likely cause is missing or incomplete handling of XInput2
active grabs around popup lifecycle:

1. GTK opens the popup. It may call `XIGrabDevice` to grab the
   pointer so it can detect click-outside and dismiss itself.
   **No `XIGrabDevice` from client 43 appears in the log** — either
   our server isn't logging it (need to add a debug line in the XI2
   dispatch arm for opcodes 51/52/54), or GTK is relying on a
   different mechanism (focus / crossing events with
   `mode=NotifyGrab`).
2. With no active grab honored, the popup's "click outside →
   dismiss" code path may be guarded behind grab events we don't
   emit. The popup widget sees the ButtonPress arrive on
   `0x2b00003` but doesn't know it's part of a grab cycle and
   ignores it.

### Diagnostic surface added on master

Per-event 1-line debug logs (all kept; together they're ~3 lines
per click — manageable on master at `RUST_LOG=debug`):

- `libinput button code=… pressed=… → X11 detail=…` —
  `kms/backend.rs::process_pointer_button`.
- `pointer_fanout: kind=… host_xid=… top_level=… target=…
  propagation_window=… child=… core_targets=… root=(…,…)
  event_xy=(…,…)` — fires on ButtonPress / ButtonRelease only.
- `pointer_fanout XI2: kind=… target=… top_level=…
  xi2_targets=[…] xi2_raw_targets=[…] root=(…) event_xy=(…)
  state=…` — fires on ButtonPress / ButtonRelease only.
- `SendEvent type=… dest=… event_mask=… propagate=…
  targets=[…]` — now shows resolved target client list, useful
  for confirming IPC ClientMessage delivery.
- `log_hit_test_diagnostic` reverted to `trace` — produces 25+
  lines per click (every top-level + descend) and would drown
  the log at `debug`.

Together these proved that **all the delivery layers work**; the
bug is downstream in popup grab/dismissal semantics.

### Real next steps

1. Add a `debug!` line in the XI2 dispatch path for opcodes 51
   (`XIGrabDevice`), 52 (`XIUngrabDevice`), 54
   (`XIPassiveGrabDevice`). Re-run, click the calendar, see whether
   GTK is even attempting an active grab.
2. If yes: check our implementation of those opcodes — are we
   honouring the grab when routing subsequent events, generating
   the `mode=NotifyGrab` crossing events GTK expects?
3. If no: GTK is relying on a different signal we're not sending.
   Candidates: `FocusIn`/`FocusOut` on the popup,
   `EnterNotify`/`LeaveNotify` with `mode=NotifyGrab` when the
   shield is mapped, or `XISelect` device-changed events.
4. Cross-check against the gtk3-demo flow: same renderer, did
   any of its popup menus (e.g. right-click) work? If yes the
   diff would isolate mate-panel-specific behaviour.
