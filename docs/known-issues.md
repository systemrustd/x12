# Known Issues

Cross-cutting bugs, limitations, and dev-loop friction that surface
during validation, debugging, or interactive use. Phase-bound feature
work lives in [`status.md`](status.md) under each phase's follow-ups
section; this file is for issues that don't fit a phase or aren't
worth a phase of their own.

Add items as you find them. Tick them off when fixed. Prefer concise
entries with enough context for a future debugging session to start
from.

## Host-error noise (surfaced by Phase 6.3)

Phase 6.3's `OriginContext` made async host errors visible. Most are
pre-existing host complaints that ynest has been generating since
earlier phases; they were silently absorbed by the legacy
`reply_buffer`. Logged at WARN today; could be downgraded to DEBUG
once the underlying patterns are understood.

- [ ] **CopyArea BadMatch (~100/wmaker startup).** `error 8 major=64
      minor=0 bad=<container_xid>`. wmaker calls `CopyArea` between
      drawables of differing depth and the host rejects. Investigation:
      grep wmaker's source / x11trace one wmaker session and compare
      the failing CopyArea source/dest pairs against ynest's depth
      tracking. Visible behavior is unaffected (other CopyAreas in
      the cascade succeed) so this is purely log-noise.
- [ ] **PolyFillRectangle BadMatch (~15/wmaker startup).** `error 8
      major=70`. Same shape as the CopyArea pattern; likely the same
      root cause (depth/drawable mismatch on the container).
- [ ] **Composite NameWindowPixmap BadAccess (~28/wmaker startup).**
      `error 1 major=138 minor=23` on un-redirected windows. Phase 3.5
      added partial COMPOSITE support but ynest calls `NameWindowPixmap`
      on windows that haven't been redirected — host returns BadAccess.
      Either ynest should redirect first, or only call NameWindowPixmap
      from a context where redirection is guaranteed. Affects: nothing
      under non-compositing WMs; matters for picom-like compositors.

## Input, grabs, event routing

- [ ] **`UnmapNotify.from_configure = true` never wired.** Encoder
      accepts the byte for wire correctness; every call site currently
      passes `false`. The `true` path fires when a parent's
      `ConfigureWindow` shrinks a child out of view. Wire once we
      track parent-resize-driven implicit unmaps. (Phase 1 follow-up.)
- [ ] **`RRSelectInput` mask storage.** `RRSelectInput` is accepted
      but the mask is not stored; `RRScreenChangeNotify` is never
      delivered to RANDR clients. (Phase 2 follow-up.)
- [ ] **`GrabButton` Sync mode replay.** Current implementation stores
      the frozen event but `AllowEvents(ReplayPointer)` requires access
      to the `xid_map` which (pre-Phase-6.3) lived on the pump thread.
      Phase 6.3 moved `xid_map` into `HostX11Backend`; revisit whether
      replay is now wirable, or whether it still needs an inter-thread
      channel. (Phase 2 follow-up.)
- [ ] **`SendEvent` parent-tree propagation.** Current impl delivers
      to direct window subscribers; does not propagate up the window
      tree (`event_mask=0` with `PointerWindow` / `InputFocus`
      destinations). (Phase 2 follow-up.)
- [ ] **Input-shape hit testing in pointer pump.** Deferred since
      Phase 3.2. SHAPE input rectangles are stored locally but pointer
      hit tests don't honor them — clicks land on shape-rejected
      regions when they should pass through.
- [ ] **`CreateCursor` `XColor` struct layout.** Xlib `XColor` layout
      must match the system Xlib headers; verify on non-CachyOS target
      platforms. (Phase 2 follow-up.)
- [x] ~~**xeyes doesn't track cursor.**~~ Fixed: xeyes selects
      `XI_RawMotion` (XI2 event type 17) on root, which our
      `pointer_event_fanout` was not synthesizing —
      pre-fix only synthesized `XI_Motion` (type 6). Initial
      "GenericEvent dropped" theory was wrong; the dispatcher does
      drop GenericEvents, but ynest never asks the host to send
      them so no XI2 events arrive from the host in the first
      place. Real fix: synthesize `XI_RawMotion` (and
      `XI_RawButtonPress` / `XI_RawButtonRelease`) alongside the
      existing `XI_Motion` / `XI_ButtonPress` / `XI_ButtonRelease`
      synthesis in `server.rs::pointer_event_fanout`. Raw events
      include X+Y valuators with FP3232 root coords.

## Drawing / rendering artifacts

- [x] **wmaker icon-edge clamped to 800×600 with larger geometries.**
      Two stale-screen-size sources fed clients the wrong dimensions
      regardless of `--geometry` / actual scanout: the connection-setup
      `Screen` reply hardcoded `width_px=800, height_px=600` (which
      `DisplayWidth/Height` reads — wmaker icon placement), and
      `ResourceTable::new()` initialised the root window to 800×600
      (which `GetGeometry(root)` returns — e16 virtual-desktop layout).
      Fixed by sourcing both from the requested geometry; mm computed
      at 96 DPI from the actual pixel size. Verified visually on
      wmaker, fvwm3, e16.
- [ ] **Per-client GC mirroring** (Phase 3.7 task #26). The shared
      host GC creates subtle bugs when GC state leaks between clients.
      Phase 3.7's fill-style fix needed careful "reset to Solid after
      each draw" to avoid tile state bleed; Phase 6.3 added similar
      careful state management for `function`/`plane_mask`/etc. via
      `apply_draw_state`. Real per-client GC mirroring would
      eliminate the reset-after-draw discipline. ~Medium scope.
- [ ] **Sub-window Expose for off-screen / behind-sibling drags.**
      Backing-store mitigation from Phase 3.6 covers the common case;
      synthetic Expose for the corner cases (window dragged fully
      off-screen / fully behind another) is the proper fix and is
      deferred. Re-open if a real validation scenario demonstrates a
      backing-store gap.
- [ ] **Damage accumulation on RENDER drawing ops.** Phase 3.5's
      first-cut `accumulate_damage` covers core drawing only.
      RENDER-driven damage (composite, fill rectangles, glyphs) is
      not accumulated. Matters once a real client (compositor /
      screen recorder) drives the path.

## wmaker on KMS

- [x] **Window Maker reparents but never `MapWindow`s its own frame.**
      Fixed by `911fa38`. `KmsBackend::get_image` was returning raw
      pixel bytes instead of a complete X11 wire reply, so wmaker's
      first GetImage during MapRequest handling produced what looked
      like a malformed error reply (byte 0 = pixel byte ≈ 0 → wmaker
      treated it as Error). wmaker's catchXError logged "internal X
      error: 0" and short-circuited the rest of the frame
      installation, leaving the frame and client unmapped. Fix:
      prepend the standard 32-byte X11 reply header in
      `get_image`, matching what `nested.rs:7744` expects (it
      patches sequence + visual into bytes 2..4 / 8..12 there).
- [x] **wmaker appicon clipped diagonally.** Fixed by `93742cc`.
      Root cause was depth-1 PutImage being a no-op, so wmaker's
      icon shape masks (24×24 ZPixmap d1 via MIT-SHM) never reached
      our pixmaps. Implementing the depth-1 path (row-wise memcpy —
      X11 and pixman both use 32-bit-aligned MSB-first scanlines)
      restored the appicon.
- [ ] **e16 widget clicks don't activate.** Enlightenment 16 comes
      up on KMS, the right-click-desktop menu opens, the post-startup
      "Menu generation complete" dialog renders with an OK button,
      and the settings menu opens — but clicks on any of those
      widgets (OK, menu items, settings buttons) don't activate
      anything. Click delivery does happen (we deliver ButtonPress
      via e16's passive-grab on `button=AnyButton
      modifiers=AnyModifier`, e16 calls `AllowEvents` and redraws),
      but the action behind the widget doesn't fire.

      Likely root cause: e16 uses Sync-mode passive grabs and
      depends on a particular event-replay sequence (frozen pointer
      event → AllowEvents(ReplayPointer) → server replays the press
      to the actual subwindow target). Our `pointer_grab` machinery
      stores `frozen_pointer_event` when `pointer_mode == 0` (Sync)
      but doesn't have a full replay path on AllowEvents. e16
      visually works otherwise — drag, resize, switch workspaces,
      menu navigation via Alt+arrows function — so xterm under e16
      is usable, just menu/dialog clicks aren't.

- [ ] **wmaker title-bar close/minimize button glyphs missing.**
      Same general area as the appicon (CWBackPixmap-driven small
      icons) but the depth-1 PutImage fix didn't cover it. The
      buttons render as plain coloured 25×25 squares without their
      X / − glyphs. Probably a different drawing path — wmaker may
      be using PolySegment / PolyLine to stroke the glyphs after a
      ClearArea, and either the glyph strokes are clipped or our
      drawing primitives don't honour something they need. Cosmetic;
      drag/move/close-via-menu still work.
      wmaker comes up on KMS and draws its dock/clip; xterm under
      wmaker connects, sends MapRequest, gets reparented into a
      wmaker frame, gets configured to fit, and renders text into
      its (now-hidden) backing pixmap — but the frame itself is
      never mapped, so xterm is invisible.

      Trace from client 0 (wmaker) shows the typical sequence:
      `GrabServer` → frame `CreateWindow` → frame-children
      `CreateWindow` + `MapWindow` → long restack chain across all
      existing top-levels → frame `ConfigureWindow` (geometry + border
      width) → `ReparentWindow` xterm into frame → `ConfigureWindow`
      xterm to fit. After that point wmaker continues drawing
      decorations and creating an appicon, but no `MapWindow` is
      ever sent on the frame xid (0x10051b in the captured run) and
      no `MapWindow` on xterm's own xid follows the reparent.

      wmaker also logs `internal X error: 0 Request code: 0 DUMMY
      Request minor code: 2 Resource ID: 0xff000000 Error serial:
      10172` early — the resource id `0xff000000` doesn't match any
      xid yserver allocates and `code: 0` isn't a real X11 error
      code, so wmaker is either mis-parsing one of our replies or
      reacting to something our error encoder produced with a wrong
      `code`/`major_opcode`/`bad_value` triple. That error happens
      well before xterm starts, so it may or may not be related to
      the missing `MapWindow`.

      fvwm3 (`yserver-fvwm3-xterm` recipe) is fully working;
      wmaker's flow has different expectations that we don't meet.

      **Diff against ynest (where wmaker+xterm works):** in ynest,
      wmaker's MapRequest handling continues past frame creation to:
      ```
      CreatePixmap 64x64 (appicon)
      MIT-SHM::PutImage (upload appicon image)
      MIT-SHM::Detach
      ChangeSaveSet (on the xterm window)
      MapWindow frame
      MapWindow xterm
      ```
      In yserver, wmaker stops before any of these — the `ChangeSaveSet`
      / `MapWindow frame` / `MapWindow xterm` lines are entirely absent
      from the trace, and the appicon MIT-SHM upload for the new
      window never happens (MIT-SHM count: ynest 430, yserver 409).
      Combined with the early `internal X error: 0 ... Error serial:
      10172` wmaker logs to its catchXError handler, the most likely
      story is that we're sending wmaker a malformed error reply
      somewhere during frame setup, and wmaker's XSetErrorHandler
      branches into a "skip mapping" recovery path. Bisect candidates:
      the long restack chain (~150 ConfigureWindow with stack_mode +
      sibling), one of the SHAPE::SelectInput / SHAPE::QueryExtents
      probes, or a CWCursor / CWBorderPixmap attribute we silently
      drop in change_subwindow_attributes.

## KMS backend (Phase 6.4 / 6.5)

Surfaced while bringing up xeyes / xterm / xclock and fvwm3 against
`KmsBackend`. The backend is the bare-metal counterpart to
`HostX11Backend` — primitives go straight to a pixman scanout buffer
instead of a host X server, so gaps in our rasterisation surface here
that the host hides for us.

- [x] **GC `function` not honoured.** Fixed in Phase 6.5 Step 2
      (`6972c39`). `apply_draw_state` now captures `state.function` and
      every client-draw primitive routes through
      `fill_rects_with_gc_function` which maps `GcFunction::Copy →
      pixman SRC` and `GcFunction::Xor →` per-pixel bitwise XOR. (Pixman
      `PIXMAN_OP_XOR` is Porter-Duff and produces zero for opaque
      pixels — not what X11 GXxor wants — so the Xor path uses raw
      pixel manipulation.) Other GcFunction variants log-and-fall-back
      to Src.
- [x] **xterm glyph baseline / placement.** Fixed in Phase 6.5 Step 3
      (`993c437`). The 1×1 solid-colour source image in
      `render_text_string` was created with `REPEAT_NONE` (pixman
      default), so every source read outside pixel (0,0) returned
      transparent — Operation::Over was a no-op for all glyph columns
      except the leftmost, producing scattered dots. Fix:
      `color_img.set_repeat(Repeat::Normal)`.
- [x] **fvwm3 modules wedge.** Fixed in Phase 6.5 Step 1A (`e19ca7c`)
      via `KmsBackend::render_opcode() → Some(133)`. The original
      "synthesise ConfigureNotify from configure_subwindow" hypothesis
      was incorrect: nested.rs already synthesises ConfigureNotify
      backend-agnostically. Real cause was that without RENDER fvwm3
      uses a two-level frame hierarchy that places the client at
      parent-relative `(0,0)`, which traps FvwmPager's "have I been
      placed?" loop forever. Trace-diff in
      `docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md`.
- [x] **`Opcode 58` (SetDashes), `Opcode 81` (InstallColormap)
      unsupported.** Still logged as `unsupported opcode N` (cosmetic).
      Both intentionally left as-is in 6.5 — dashes fall back to
      solid (visually fine), InstallColormap is a no-op on TrueColor.
- [ ] **`RENDER::CompositeGlyphs8` is a no-op stub on KMS.** Visible:
      fvwm3 panel labels (FvwmPager desktop names, FvwmIconMan window
      titles, FvwmButtons text) render as solid-colour rectangles with
      no glyphs; fvwm3's window-list popup shows rows but each row's
      title is empty. Implementation outline: glyphset registry +
      `AddGlyphs` decode (per-glyph bitmap into a HashMap on
      `KmsBackend`) + `render_composite_glyphs` walking the glyph
      stream and compositing each glyph alpha mask onto the dst image
      with `Operation::Over` (mirroring the existing
      `render_text_string` path for core text). Phase 6.6.
- [ ] **`RENDER::Composite` is a no-op stub on KMS.** Off-screen-
      buffer-to-window blits via the generic Composite call drop
      silently. Visible: fvwm3 popup chrome shows garbage where
      double-buffered widgets blit their content. Implementation:
      look up src + dst pictures, call `dst.0.composite32(...)` with
      the appropriate Operation. Bounded scope (~50 LoC) once the
      picture-state model from 6.5 Step 1B is in place. Phase 6.6.
- [ ] **Window drag does not work under fvwm3/KMS.** Click-and-drag
      on a fvwm-framed window does not move the window; click/focus/
      keyboard input all work. Most likely a pointer-grab / motion-
      routing gap surfaced by KmsBackend's pointer pump (the host
      backend gets grab semantics for free from the host X server).
      Investigate: does KmsBackend deliver MotionNotify while a
      ButtonPress grab is active? Does the grab path correctly route
      events to the grabbing client? Phase 6.6.
- [ ] **xterm text corrupts after scrolling on KMS.** After running
      a command that scrolls the buffer (e.g. `ls`), some glyphs
      appear doubled / overlapped. Probably a CopyArea-within-window
      stride/clip issue surfaced when xterm scrolls its content via
      CopyArea. Pre-existing scrollback issue (already tracked under
      "WM-specific behaviour") may be related. Phase 6.6.
- [ ] **`poly_arc` / `poly_fill_arc` partial-angle clipping.** Both
      treat any arc as a full ellipse regardless of `angle1`/`angle2`.
      Fine for xeyes (full circles) but anything that draws actual
      pie slices renders as full discs. Add an angular mask: for each
      candidate pixel, check `atan2(py - cy, px - cx)` against
      `[angle1, angle1 + angle2)` (with X11's "0 = 3 o'clock,
      counter-clockwise" convention).
- [ ] **`poly_arc` outline only handles full ellipses.** Same root
      cause as above — the cap/connector logic doesn't know about
      partial arcs. Once angle clipping is in, the outline algorithm
      needs the same treatment plus proper arc endpoints (so a
      half-arc outline doesn't close itself across the chord).
- [ ] **Pixman `fill_rectangles` segfaults on partly-out-of-bounds
      rects.** `clip_rects_to_image` works around it for `poly_line`,
      `poly_segment`, `poly_arc`, `poly_fill_arc`. Other call sites
      (`fill_rectangle`, `poly_fill_rectangle`, `image_text8`'s
      background rect, …) currently rely on either pre-clamping or on
      the rect being in-bounds by construction. Audit them all and
      either route through the helper or guarantee bounds. Investigate
      *why* pixman segfaults — our build / version may be misbehaving
      and an upgrade could let us drop the workaround.
- [ ] **Host (GTK) cursor and guest cursor drift.** virtio-tablet gives
      libinput absolute positions, but the host's QEMU GTK window also
      shows its own cursor at a different spot — they diverge after a
      few movements because libinput rescales by device range and we
      rescale to scanout. Either lock the GTK cursor to the guest
      cursor (so user only sees one) or pass through *true* host
      coordinates and skip the libinput rescale.
- [ ] **`list_fonts_proxy` returns empty list.** We synthesise a valid
      32-byte "no fonts" reply so font-querying clients (xclock) don't
      block, but real font enumeration is missing.  XListFonts /
      XListFontsWithInfo against a real font path would let the host
      pretend to have e.g. the standard `*-fixed-*` set. Cosmetic —
      every client we tested falls back to the built-in font and
      proceeds.
- [ ] **`poly_line` thick lines.** GC `line_width` ignored; we always
      rasterise as 1-pixel Bresenham. Most clients use line_width=0
      (server-discretion thin) but anything wanting a 3- or 5-px line
      would render too thin.
- [ ] **fvwm3 modules wedge on missing ConfigureNotify.** fvwm3 itself
      starts and reparents client windows correctly (SubstructureRedirect
      / MapRequest forwarding works), but at least one module
      (FvwmIconMan / FvwmPager / FvwmButtons depending on config) goes
      into a tight `ConfigureWindow → ChangeWindowAttributes →
      GetInputFocus` busy loop reconfiguring the same panel windows to
      `(-1, -1)` and back. The pattern matches "module configured a
      window and is waiting for a ConfigureNotify before continuing,
      never gets one, retries". Likely fix: have
      `KmsBackend::configure_subwindow` synthesize a ConfigureNotify to
      StructureNotify-subscribed clients (and SubstructureNotify-
      subscribed parents) the way the host backend gets via the host X
      server. Reference: x11trace of fvwm3 under Xephyr at
      `Xephyr-fvwm3.log` shows 25× ConfigureNotify, 24× MapNotify, 21×
      ReparentNotify, 16× CreateNotify delivered during fvwm startup.
- [ ] **Opcode 58 (SetDashes) unsupported.** Logged as
      `unsupported opcode 58` from fvwm modules; means dashed lines
      aren't honoured.  Cosmetic — dashes fall back to solid.
- [ ] **Opcode 81 (InstallColormap) unsupported.** fvwm3 calls it once.
      Safe to ignore on a TrueColor backend; could just reply "did it"
      to silence the unsupported-opcode log.

## WM-specific behaviour

- [ ] **xterm scrollback misbehaviour.** `seq 1 200` scrolls forward
      cleanly but scrollback via scrollbar / mouse wheel / PageUp /
      Shift+PageUp doesn't behave correctly. No unsupported opcodes
      in the trace; likely a coord/grab/clip subtlety. Phase 1
      follow-up, parked since the rest of xterm works.
- [ ] **e16 popup rounded corners.** Cosmetic — popup outer shape is
      Set+Intersect rectangular bounding; rounded look comes from
      bg-pixmap content, with small black pixels at the very corners
      because the popup outer's bg isn't auto-filled there
      (default bg = None). Investigate `ParentRelative` bg forwarding.
- [ ] **fvwm3 segfaults on host container resize / window close.**
      Pre-existing fvwm3 bugs (reproduce without ynest changes).
      Re-test after the next fvwm3 update.
- [ ] **fvwm: apps disappear after host container resize.** Original
      Phase 3.2 observation. Separate symptom from the segfault above
      — sometimes apps just vanish from the WM's view rather than
      crashing it. Pre-existing.
- [ ] **openbox frame chrome.** Frame title bars / labels / buttons
      don't draw under openbox even after the Phase 3.4 atom fix;
      clients render correctly inside the frames. Suspect: openbox
      draws frame decorations into 1×1 sub-windows of the frame and
      that drawing path doesn't reach the host (same family as the
      old wmaker chrome bugs).
- [ ] **e16 intermittent popup mapping.** Largely subsumed by the
      Phase 3.7 fixes — the "first desktop never works,
      second always does" symptom was the
      `POINTER_EVENT_MASK` regression. Re-evaluate after a wmaker /
      fvwm3 / e16 smoke cycle on current master to confirm.

## Extension polish

- [ ] **e16 RENDER coverage audit.** Was deferred in Phase 3.4 because
      e16 didn't reach a stable rendering state. Phase 3.4's atom-name
      fix unblocked e16 startup, so this audit is now actionable. Run
      e16 for ~60s, capture all RENDER opcodes touched, compare against
      the implemented set in status.md's RENDER section.
- [ ] **MIT-SHM `XShmPutImage` host fast path.** Currently we chunk
      regular `PutImage` because the 16-bit length field caps a single
      image at ~262 KB. `XShmPutImage` against a host-shared shm
      segment would avoid the chunking. Revisit if large-image upload
      latency becomes a bottleneck.

## Validation surface

- [ ] **XTEST extension in ynest.** Implementing `XTestFakeKeyEvent` /
      `XTestFakeButtonEvent` (extension major 138, opcodes 2 and 3)
      would let `xdotool key`/`xdotool click` drive ynest in headless
      smoke tests. ~50–100 LoC. Today the bwrap-sandbox X test runs
      can only validate visual rendering and client connect, not
      input event delivery.
- [ ] **gtk3-demo / gtk4 demo runs in this dev environment.**
      gtk-demo (gtk4) starts then exits silently on the host's `:0`
      regardless of whether it's run nested under ynest or directly.
      Pre-existing environment / dconf / stdin quirk; needs
      investigation. Blocks the gtk3-demo arm of the WM smoke matrix.

## Dev-loop / observability

- [ ] **Down-grade known-benign host errors to DEBUG.** A small
      classifier in `BackendEventSink::handle_backend_event` that
      maps specific `(major, minor, code)` tuples to DEBUG level
      so the WARN log is actually scannable.
- [ ] **`just yserver-bare-metal` recipe.** Automate the kmscon
      dance (stop kmscon for current VT → run yserver → restart
      kmscon on exit). Already documented as a manual recipe in
      status.md's Phase 6.1 follow-ups; ~10 lines of just/sh.
- [ ] **README pointer to this file.** So the next reader knows
      where the bug ticklist lives.
