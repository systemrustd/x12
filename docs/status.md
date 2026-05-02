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

### Verified working

- xterm under wmaker: chrome + content area + shell prompt + typing.
- xterm without WM: full prompt rendered (was working pre-PR).
- xterm under fvwm3: chrome renders.
- xeyes / xclock: rendering unchanged.
- wmaker chrome: clip + dock + appicons + xterm titlebar.
- e16 startup: significantly more chrome renders than the Phase
  3.4 environmental baseline — top bar, two pagers, minimap.
- e16 popup *sometimes* opens, *sometimes* selects items (was
  100% broken before Phase 3.6).

### Phase 3.6 follow-ups

- **Rendering quality under e16 popups / chrome.** Theme
  gradients missing, popup background black instead of themed
  gradient, root background pattern missing, colours generally
  wrong. e16 paints gradients via many small `CopyArea` calls
  from theme pixmaps; pixmap creation + MIT-SHM `PutImage` to
  it + the `CopyArea` forwarding all reach the host correctly,
  but the resulting pixels don't show. Likely candidates:
  shared host GC state interference with per-window drawables,
  CopyArea result being overdrawn by something on the
  sub-window, or the destination's host-side clipping is wrong.
  Diagnostic plan: capture an x11trace dump of e16 startup
  under ynest vs under Xephyr (same theme), compare host-side
  request streams side-by-side, identify the divergence.
- **Intermittent popup mapping.** e16 sometimes opens a popup
  whose host child is `IsUnMapped` even though the local
  Window is `Viewable`. After interaction (e.g. switching
  desktops), it works. Race in the CreateWindow / Reparent /
  MapWindow flow that the per-CreateWindow `sync_main_connection`
  fence doesn't fully close.
- **Settings sub-window doesn't open** after selecting the
  popup item. Likely the same race on a different code path.
- **Step 5 — host xid + NameWindowPixmap retention.** Lock
  down race surface that grew with sub-window mirroring; lift
  the NameWindowPixmap-on-mirrored-sub-window block from
  Step 2. Touched but not implemented this phase.
- **Step 6 — cleanup.** Delete `top_level_host_target` and
  the `(x_off, y_off)` second-return on `host_drawable_target`;
  delete the `(x_off, y_off)` parameters on `apply_gc_clip`;
  remove the `uses_synthetic_expose` flag (always false now
  for InputOutput windows). Pure deletions.
- **Cross-connection sync fence is a Phase-3.7 design smell.**
  The pump and main connections race in several places; the
  per-CreateWindow fence is a duct-tape fix. Folding pump and
  main into one connection (or properly serialising via a
  message queue) is the structural fix. Phase 3.7+ work.
- **Per-client GC mirroring** (originally Phase 3.7). The
  shared host GC creates whole categories of subtle bugs;
  per-client GC removes them. Now visibly motivated by the
  e16 rendering-quality issues above.

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
  `GetGeometry`) for sync, with a `reply_buffer` for over-read
  responses, so unrelated RENDER errors interleaved with sync replies
  no longer cause the drain loop to hang.

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
