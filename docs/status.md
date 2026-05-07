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

## Phase 3.6 — Sub-window mirroring (Xnest model) (in progress)

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

## Phase 3.7 — Event-flow + popup rendering fixes (in progress)

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

### Phase 6.1 — DRM/KMS bootstrap (in progress)

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
