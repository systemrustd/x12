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
- [ ] **Background image colors look R↔B swapped.** Observed
      visually under MATE: a JPEG wallpaper rendered with cat eyes
      that should be green/yellow comes out blue/cyan. Yellow = R+G;
      losing R or swapping R↔B turns yellow into cyan/blue, which
      matches. Pre-existing (predates phase 3 work). The depth-24/32
      byte permutation in `try_vk_put_image` (`backend.rs` `match
      depth { 24 | 32 => ... }`) reads source `[r,g,b,a]` and writes
      `[b,g,r,a]` to the `B8G8R8A8_UNORM` mirror — but X11 PutImage in
      ZPixmap form sends bytes in the visual's native order. For a
      typical TrueColor BGRA visual at depth 24, the wire bytes are
      already `[B,G,R,_]`. If yserver's depth-24 visual advertises
      BGRA byte order but PutImage assumes the source is RGBA, the
      permutation effectively swaps R and B. Investigation path:
      `xtruss` a PutImage from a known client, compare the wire bytes
      against the visual byte order yserver advertises in the
      connection setup, then either drop the permutation for BGRA
      visuals or fix the visual advertisement to match. Same
      permutation lives in pixman path historically; check both.

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
- [ ] **`disable_output` atomic commit rejected with EINVAL on
      shutdown — leaves DRM state that breaks Wayland host sessions.**
      `KmsBackend::disable_output` builds an atomic request that clears
      plane FB_ID/CRTC_ID, sets CRTC ACTIVE/MODE_ID to 0, and clears
      connector CRTC_ID — all in one commit (`drm/modeset.rs:387`).
      The kernel rejects it with `-EINVAL` (`os error 22`), so on exit
      the framebuffer stays attached to the CRTC. Kernel emits
      `WARNING ... atomic remove_fb failed with -22` from
      `drm_framebuffer_remove → drm_mode_rmfb_work_fn` (deferred RMFB
      work) — see the journal traces. **The user-visible consequence**
      is that the host's Wayland compositor (labwc / dms / Sway /
      anything that expects clean modeset state) cannot recover the
      output after yserver exits; user must reboot. X-based hosts
      (Xorg + lightdm/MATE) survived because Xorg's startup runs a
      more aggressive DRM reset before grabbing outputs.
      Workaround: run yserver from a separate TTY (Ctrl+Alt+F3, run,
      Ctrl+Alt+F1 to return) — the kernel VT-switch back tends to
      force a saner reset than letting the host compositor try to
      take over a still-bound CRTC. Real fix: split the disable into
      steps the kernel will accept — clear plane first (single commit),
      then deactivate CRTC (separate commit with MODE_ID cleared as
      its own blob unset), then clear connector binding. Probably
      needs cross-reference with Mesa's libdrm fallback or Xorg's
      `drmModeAtomicCommit` deactivation sequence. **High priority**
      for anyone testing yserver on a daily-driver machine.
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
