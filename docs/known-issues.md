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

- [ ] **Caja mouse wheel doesn't work until a view-switch (yserver
      event-delivery bug).** Observed 2026-05-13 on dual-screen MATE
      smoke. When caja launches in its default view (list or icon),
      the mouse wheel does nothing. After toggling the view mode once
      (View → Icon View / View → List View), the wheel works in
      whichever view caja is in, and keeps working across subsequent
      view switches. Pre-regression (before 3E text-run migration),
      list view's wheel worked from launch — so this is a real
      regression, not just GTK initialization quirk.
      yserver does emit core button-4/5 press+release pairs from the
      libinput scroll path (verified in `yserver-hw.log` —
      `libinput button code=0x181 pressed=true → X11 detail=5`,
      138 events in the smoke run). So the wheel reaches X11. The
      stateful "fix after view-switch" pattern points at:
      (a) initial event-mask subscription on caja's view widget not
          including ButtonPressMask in the right window, OR
      (b) pointer-grab state from caja's initial focus chain
          consuming wheel events before they reach the view widget,
          OR
      (c) some Enter/Leave / crossing-event sequence on view-switch
          that re-establishes the focus chain.
      Bisect candidates among recent input/render changes: the 3E
      text-run migration (no input code, but changes render timing
      → could affect when/which Configure/Map/Expose events caja
      sees relative to its event-mask setup); the scroll-wheel
      commit `b7d17a1`; the `has_axis` fix `56f93d9`.
      Investigation path: log every PointerButton(4/5) event's
      target window + propagation chain in the first 5 seconds
      after caja launches, vs after a view-switch. The difference
      tells which window the wheel events are landing on (vs which
      window caja's view widget expects). Filed 2026-05-13.
- [ ] **Caja right-click context menu pops up offset (too far right
      and down).** Observed 2026-05-13 on dual-screen MATE smoke
      (5120x1440 = 2× 2560x1440). Right-clicking an item in caja
      produces a context menu that appears displaced from the click
      origin — both axes off. Click events themselves look correctly
      coordinate-translated in pointer_fanout debug logs (`root=(x,y)
      event_xy=(rx,ry)` with sane window-relative deltas). So the
      bug is most likely in either (a) the popup-window placement
      math caja does (it queries pointer / window position and adds
      an offset; one of those queries returns the wrong value), or
      (b) some dual-screen origin confusion when caja places the
      popup. Worth investigating with xtrace on a single-screen
      yserver first — the dual-screen geometry adds confounders.
      Could also be xfixes ShapeExtents or the popup's
      synthesize-ConfigureNotify path. Filed for later.
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
- [ ] **Crossing-event `child` field hard-coded to 0.** `EnterNotify`
      and `LeaveNotify` always wire `child = 0` —
      `encode_crossing_event` at
      `crates/yserver-protocol/src/x11/mod.rs:2621` does
      `write_u32(order, out, 0); // child — descendant hit-testing
      not implemented`, and `CrossingEvent` in the same file lacks a
      `child` field entirely. WMs that select Enter/Leave on the
      root and gate behavior on `child` (is the pointer over bare
      root or over a child of root?) can't distinguish, so e16's
      hover popup over the desktop *also* shows when the cursor
      moves over xterm and other top-levels. Same bug-class as the
      old `PointerEvent.child = 0` issue (fixed for ButtonPress via
      `pointer_propagation_target_by_id`'s `propagation_child`); fix
      is to add `child` to `CrossingEvent`, compute the topmost
      descendant of `event` containing the pointer, and thread it
      through the encoder. Bare-HW e16 surfaced this; observed
      2026-05-09. (Phase 1/2 follow-up.)
- [ ] **GTK3 tree-view expander triangles don't reliably toggle.**
      Surfaced during Phase J ynest+fvwm3+gtk3-demo / ynest+e16+gtk3-demo
      smoke. After fixing two prerequisite routing bugs (the
      `PointerEvent.child` field was hard-coded to 0 → fvwm3
      MainMenu fired on every click; XI2 events were being stolen by
      core grabs → GTK never saw ButtonRelease) and the XI2
      `buttons` mask post-event semantics (was always-set on Release;
      now correctly reflects post-event button state), gtk3-demo's
      tree expanders work for *one* click — the first expand triggers,
      then subsequent collapse / expand clicks on triangles don't
      register. Trace logging confirmed the failing clicks deliver
      byte-identical XI2 events to gtk3-demo as the working click
      (same target, same xi2_targets, same coords ±1px, same `state`,
      reasonable time deltas, comfortably outside double-click
      threshold). Suspect remaining issues are XI2 wire-encoding
      details GTK is sensitive to but we don't currently emit:
      `flags` field is hard-coded to 0 (could need `XIPointerEmulated`
      or similar), `valuators_len` is 0 (GTK tree views may rely on
      X+Y valuators for hit-testing inside a column), or device-
      hierarchy / device-changed events that GTK uses to track the
      master pointer's identity. Not a regression of the
      single-threaded core — same-shaped event flow worked
      immediately for xterm, xclock, xeyes, and for gtk3-demo's main
      list-row clicks.

## Drawing / rendering artifacts

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
- [ ] **KMS: `render_set_picture_filter` is not picture-local.** The
      filter is set on the picture's *backing pixman image* via
      `pixman_image_set_filter`. If two RENDER pictures wrap the same
      drawable (a common GTK pattern: client creates a Picture for
      drawing and another for compositing onto the same window), the
      latter SetPictureFilter wins for both and any composite using
      either picture sees the wrong filter. Real fix: track
      `filter: pixman_filter_t` on `PictureState::Drawable`, apply
      around each composite call (set → composite → restore), same
      shape as the existing per-picture clip path in `render_composite`
      / `render_fill_rectangles`. Touches every RENDER op call site.
- [ ] **KMS: `MapSubwindows` doesn't re-Expose deep descendants
      promoted by the map_window viewable cascade.** After commit
      `304858f` (`fix(resources): propagate Viewable down through
      Unviewable descendants on map`), mapping a window correctly
      flips deep descendants from `Unviewable` to `Viewable`.
      `handle_map_window` already calls
      `emit_expose_subtree_to_state` which walks descendants, so
      that path is fine. But `handle_map_subwindows` only emits
      MapNotify+Expose for the children it directly maps, not for
      their grandchildren that may have just been promoted by the
      cascade. Edge case (most clients only have one level of
      MapSubwindows children) but spec-incorrect. Fix: in
      `handle_map_subwindows`, after the per-child loop call
      `emit_expose_subtree_to_state` on each child whose viewability
      transitioned. ~10 LoC.
- [ ] **Damage accumulation on RENDER drawing ops.** Phase 3.5's
      first-cut `accumulate_damage` covers core drawing only.
      RENDER-driven damage (composite, fill rectangles, glyphs) is
      not accumulated. Matters once a real client (compositor /
      screen recorder) drives the path.
- [x] **~~Background image colors look R↔B swapped.~~ FIXED
      2026-05-14.** yserver advertises X.Org-standard masks
      (`red=0x00FF0000`, `green=0x0000FF00`, `blue=0x000000FF`) and
      echoes the client's byte_order as `image_byte_order`, so the
      spec-correct LE wire encoding of a ZPixmap pixel is `[B,G,R,A]`
      — already matching the mirror's `B8G8R8A8_UNORM` layout.
      `try_vk_put_image` was reading the wire as `[r,g,b,a]` and
      permuting to `[b,g,r,a]`, double-swapping any spec-compliant
      client. Fix: straight `copy_nonoverlapping` for depth-32; same
      for depth-24 with a `byte[3]:=0xFF` post-pass to keep the
      mirror opaque for RENDER composites. Confirmed on fuji: MATE
      wallpaper + Chrome icon now render correctly. Test fixture
      doc + the three PutImage/CopyArea alpha_invariant inputs
      updated to feed wire bytes in spec order.
- [ ] **Text rendering broken under xfce4 / GTK heavy workloads.**
      Observed 2026-05-13 with `just yserver-xfce-hw`: xfwm4
      decorations render fine, but text inside two pop-up dialogs
      was illegible (user: "can't read the text"). Black background
      where panel/wallpaper should be (separate issue — see listener
      starvation entry below). Likely candidates: glyph atlas upload
      timing (the L2-deferred MaskScratch / glyph-atlas migration in
      phase 3E will touch this); the CompositeGlyphs xSrc/ySrc-vs-pen
      bug pattern (already in feedback memory); or a GTK font-rendering
      pipeline that uses RENDER paths yserver hasn't fully wired.
      Repro under ynest first to isolate from KMS-side issues. Defer
      until phase 3E lands text-run migration; revisit then.

## wmaker on KMS

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
- [ ] **`poly_line` thick lines.** GC `line_width` ignored; we always
      rasterise as 1-pixel Bresenham. Most clients use line_width=0
      (server-discretion thin) but anything wanting a 3- or 5-px line
      would render too thin.
- [ ] **Opcode 58 (SetDashes) unsupported.** Logged as
      `unsupported opcode 58` from fvwm modules; means dashed lines
      aren't honoured.  Cosmetic — dashes fall back to solid.
- [ ] **Opcode 81 (InstallColormap) unsupported.** fvwm3 calls it once.
      Safe to ignore on a TrueColor backend; could just reply "did it"
      to silence the unsupported-opcode log.
- [ ] **Can't switch VT while yserver is running.** The startup
      `KDSKBMODE=K_OFF` blocks the kernel from interpreting
      Ctrl+Alt+Fn as a VT switch — it's the same mechanism that
      stops keystrokes from leaking to the underlying TTY. yserver
      currently doesn't implement the standard
      `VT_PROCESS` / `VT_RELDISP` cooperative-VT-switch protocol
      (Xorg's `xf86OpenConsole` / `xf86VTSwitch`), so even if the
      keys reached us we'd have nowhere to dispatch them. Workaround
      from a remote shell: `sudo chvt N`, or `sudo systemctl restart
      display-manager` to land back in the original session.
      Fix: install a SIGUSR1/SIGUSR2 handler bound via
      `VT_SETMODE` and release/acquire the DRM master in lock-step
      with the kernel-driven switch. Mostly cosmetic until the
      multi-server use case (X+yserver in parallel) becomes real.
- [ ] **Crash before `console::Drop` leaves the host TTY unusable.**
      yserver's startup grabs the VT with `KDSKBMODE=K_OFF +
      KD_GRAPHICS` (kernel keystroke→TTY translation suppressed) and
      relies on the matching restore in `console::Drop` for cleanup.
      Any abnormal exit that bypasses `Drop` — SEGV/SIGABRT, an
      unhandled signal whose default action is "terminate"
      (e.g. SIGUSR1 on a binary without our handler), DPMS-induced
      power-save with the box still alive, etc. — leaves the kernel
      in K_OFF on that VT. From then on Ctrl+Alt+Fn does nothing,
      the VT can't be switched away from, and the screen looks
      frozen even though the box is responsive over SSH. Recovery
      from a remote shell: `sudo systemctl restart display-manager`
      (gdm/sddm/lightdm) — clears the state and gets you back to
      Wayland/Xorg login. Hardening: install a SIGSEGV/SIGABRT
      signal handler that explicitly calls the console restore
      before re-raising, and propagate the K_OFF restore through
      panic hooks too.
- [x] **~~P0: KMS teardown leaves DRM state that breaks Wayland host
      sessions.~~ FIXED via failure-path disarm (2026-05-13).**
      Diagnosis was correct: yserver's `disable_output` left
      framebuffers bound to CRTCs that the host Wayland compositor
      (labwc/dms/Sway) then couldn't recover. Fix landed via the KMS
      teardown plan
      (`docs/superpowers/plans/2026-05-13-kms-teardown-fix.md`,
      results at
      `docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md`):
      6-step shutdown sequence + per-output `disarm` paths on
      `ScanoutBo` and `drm::buffer::Buffer` so failed atomic disables
      no longer cascade into `destroy_framebuffer` on KMS-held FBs.
      Hardware-validated: dms+labwc session recovers cleanly when
      user switches back to F1 after running yserver on F3. Kernel
      `atomic remove_fb failed with -22` WARN is gone.
- [ ] **Atomic `disable_output` returns EINVAL on AMD Polaris
      (residual bug after the P0 fix).**
      `crates/yserver/src/drm/modeset.rs:387` builds a single atomic
      commit that clears plane FB_ID/CRTC_ID, sets CRTC ACTIVE/MODE_ID
      to 0, and clears connector CRTC_ID. The kernel rejects this
      with `-EINVAL` on both DP-1 and HDMI-A-1. The teardown disarm
      path makes this harmless (failed outputs leak their FBs instead
      of being destroyed, so dms recovers), but the warning still
      fires on every shutdown and the leaked handles persist until
      DRM-fd close at process exit. Two likely causes per codex's
      original diagnosis:
      1. `MODE_ID = 0` on a blob property may need the property
         unset specifically rather than set to integer 0.
      2. Some drivers want the plane cleared in a separate atomic
         commit BEFORE the CRTC is deactivated.

      Fix recipe: split the single atomic commit into per-stage
      commits — (a) clear plane FB_ID + CRTC_ID, (b) deactivate
      CRTC + clear MODE_ID blob, (c) clear connector CRTC_ID. Each
      with `ALLOW_MODESET`. Cross-reference Mesa libdrm fallback or
      Xorg's `drmModeAtomicCommit` deactivation sequence. Medium
      priority — no user-visible regression today; cleaner shutdown
      logs + zero-leak success path are the goals.
- [ ] **xeyes-on-e16 window drag is sluggish on bare HW.** Observed
      on a real DP-attached AMD card via `yserver-e16-...-hw` (KMS).
      During a drag the dragged frame visibly lags the cursor.
      `yserver-hw.log` over the drag window shows the compositor
      saturating vblank (pageflip-complete + composite resubmit
      pairs back-to-back at ~60 Hz) but only ~20 `ConfigureWindow`/s
      reaching the WM despite continuous pointer motion — input
      delivery, e16 throttling, or composite wall-time per frame
      could each be the bottleneck and the log alone doesn't
      distinguish them. To diagnose: add timestamped tracing on
      (libinput motion → MotionNotify dispatched) and (composite
      start → flip submitted) for one drag, then correlate. The
      compositor never falls behind vblank in the log, so the most
      suspicious link is input-event delivery rate.

- [ ] **mate-control-center under adapta-nokto: catastrophic mouse
      lag.** Observed 2026-05-13 on `bee`. The moment mate-cc is
      open under the adapta-nokto GTK theme, the whole desktop becomes
      unusable — Alt-F2 takes minutes to surface, cursor lags by
      minutes behind the actual input. `amdgpu_top` shows continuous
      GPU work, so this is real submission saturation, not yserver
      spinning on CPU. Closing mate-cc (or switching to the default
      Menta theme) restores responsiveness — under default theme
      mate-cc is mostly OK, only briefly laggy. Reproduces on `master`
      (predates Phase 3F-1), so this is not a 3F-1 regression. The
      hardware angle is unconfirmed — adapta-nokto was not tested on
      `silence`, so we don't yet know whether the cliff is
      hardware-class-dependent or just absolute theme-workload-driven.

      **Post-3F-2 update (2026-05-13)**: 3F-2 retired
      `try_vk_render_traps_or_tris`'s per-call `vkQueueWaitIdle`
      (the traps half of the original two-part hypothesis), but the
      lag is **unchanged** — adapta-nokto + mate-cc still brings the
      machine down to the point where `amdgpu_top` (a separate process,
      unrelated to yserver) stops redrawing. That confirms the
      bottleneck is GPU-submission saturation, not input-loop
      starvation, which in turn rules in the remaining hypothesis:
      `GlyphAtlas::intern`'s per-glyph `queue_wait_idle` is the
      dominant remaining cost (each new glyph drains the queue
      completely before the next can be uploaded). Phase 5 scope.
      Phase 4 (`vkQueueWaitIdle` retirement from
      `PaintBatch::submit_and_wait`) helps input fluidity but does
      not reduce the absolute submission count; both Phase 4 and
      Phase 5 likely need to land before adapta-nokto becomes usable
      on `bee`. A reproducer on `silence` with adapta-nokto would
      still help separate "hardware drains too slowly" from "absolute
      op count exceeds frame budget on any hardware."

## WM-specific behaviour

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

## Extension polish

- [x] **~~Composite extension: xfwm4 / picom / xcompmgr can't
      redirect (`update=1` Manual mode rejected with BadValue).~~
      FIXED 2026-05-14.** Surfaced as the MATE notification-area
      applet crashing on every login (Gdk error handler aborts on
      `BadValue, request_code 144 (Composite), serial 282`).
      Fix shape per the previous "register the record but skip
      `activate_redirect_backing_for`" recommendation: both
      `update=0` (Automatic) and `update=1` (Manual) now insert the
      redirect record, but the backing-pixmap activation is dropped
      from all three call sites (composite handler x2 + CreateWindow
      child-of-redirected hook). NameWindowPixmap reads the record;
      no consumer reads the backing today. `activate_redirect_backing_for`
      kept under `#[allow(dead_code)]` for revival when the
      backing-as-source compositor path lands. Confirmed on fuji:
      notification-area-applet survives login.
- [ ] **e16 RENDER coverage audit.** Was deferred in Phase 3.4 because
      e16 didn't reach a stable rendering state. Phase 3.4's atom-name
      fix unblocked e16 startup, so this audit is now actionable. Run
      e16 for ~60s, capture all RENDER opcodes touched, compare against
      the implemented set in status.md's RENDER section.
- [ ] **xclock "Missing charsets in String to FontSet conversion"
      warning at startup.** Benign and matches stock Xorg behaviour
      without CJK bitmap fonts installed — libXt walks the locale's
      full charset wishlist (jisx*/gb2312*/ksc*/big5*/various
      iso8859-N) and warns for those it can't load, then proceeds
      with whatever it found (iso8859-1 + iso10646-1). Fix would be
      to probe each font's `FcCharSet` in `build_font_catalog` and
      emit jisx*/gb2312*/ksc*/big5 entries when a real CJK font is
      installed. Defer until a CJK rendering client matters.
- [ ] **MIT-SHM `XShmPutImage` host fast path.** Currently we chunk
      regular `PutImage` because the 16-bit length field caps a single
      image at ~262 KB. `XShmPutImage` against a host-shared shm
      segment would avoid the chunking. Revisit if large-image upload
      latency becomes a bottleneck.

## Validation surface

- [ ] **gtk3-demo / gtk4 demo runs in this dev environment.**
      gtk-demo (gtk4) starts then exits silently on the host's `:0`
      regardless of whether it's run nested under ynest or directly.
      Pre-existing environment / dconf / stdin quirk; needs
      investigation. Blocks the gtk3-demo arm of the WM smoke matrix.

## Core loop fairness

- [ ] **Listener accept starves under high-volume per-client request
      streams.** Observed 2026-05-13 with `just yserver-xfce-hw`:
      xfce4-panel made 5 attempts to open `:7.0` over ~50ms, all
      returned "cannot open display"; yserver-hw.log shows zero new
      client setups between 13:00:48 (client 38) and the user's
      Ctrl-Alt-Backspace zap at 13:00:59, despite the kernel-side
      `connect()` succeeding for those 5 panel attempts.
      Meanwhile client 13 (likely xfdesktop or xfsettingsd) sent
      **32,293 QueryPointer requests in 11 seconds** (~3000/sec) and
      client 6 (xfwm4) was hammering SHAPE::Combine. The single-threaded
      core loop's per-iteration work was dominated by reading existing
      clients' floods; mio readiness for the LISTENER_TOKEN was queued
      but never serviced, so the new fds languished without an X11
      setup handshake completing.
      Root cause: `core_loop::run_core`'s mio poll iteration likely
      drains each ready fd until WouldBlock before checking the next
      one (a classic head-of-line blocking pattern). Fix shape: cap
      per-client read budget per poll iteration (e.g., 16-32 requests
      per client per tick) so a chatty client doesn't monopolize the
      core. Alternatively, prioritize LISTENER_TOKEN at the top of the
      iteration. Touches `core_loop/run.rs`; medium scope.
      Secondary observation: client 13's 3000 Hz QueryPointer polling
      is itself worth investigating — that's likely yserver returning
      a stale or wrong reply to xfdesktop's pointer query, causing it
      to retry indefinitely. Spec compliance issue.

## Dev-loop / observability

- [ ] **Down-grade known-benign host errors to DEBUG.** A small
      classifier in `BackendEventSink::handle_backend_event` that
      maps specific `(major, minor, code)` tuples to DEBUG level
      so the WARN log is actually scannable.
- [ ] **X11 error encoder hard-codes `minor_code = 0` for extension
      errors.** `emit_x11_error(state, client_id, sequence, code,
      bad_value, major_opcode)` has no `minor` parameter and bakes
      `minor_code: 0` into the encoded error reply (see callers in
      `crates/yserver-core/src/core_loop/process_request.rs`; the
      Composite handler around line 2583 is one of many). Real X.Org
      threads the per-extension minor opcode through. The wire bug:
      a client receives `error_code 2 (BadValue), request_code 144
      (Composite), minor_code 0` regardless of whether the failing
      request was `RedirectSubwindows` (minor 2),
      `NameWindowPixmap` (minor 6), etc. — confusing debugging.
      Surfaced 2026-05-13 when chasing the xfwm4 Composite startup
      failure (the error said minor 0 = QueryVersion, but the
      actual failing request was `RedirectSubwindows` = minor 2 —
      the inverted Automatic/Manual mode constants bug). Fix:
      thread the minor opcode through `emit_x11_error` (add a
      parameter; default to 0 for core requests; pass the request
      minor for extension requests). Touches every `emit_x11_error`
      call site (~60-80 across the file). Cosmetic but high impact
      on future debugging sessions.
- [ ] **README pointer to this file.** So the next reader knows
      where the bug ticklist lives.
