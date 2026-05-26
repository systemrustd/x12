# Known Issues

Cross-cutting bugs, limitations, and dev-loop friction that surface
during validation, debugging, or interactive use. Phase-bound feature
work lives in [`status.md`](status.md) under each phase's follow-ups
section; this file is for issues that don't fit a phase or aren't
worth a phase of their own.

Add items as you find them. Tick them off when fixed. Prefer concise
entries with enough context for a future debugging session to start
from.

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
- [~] **Crossing-event `child` field + detail codes.** Partially
      fixed 2026-05-15. The protocol-level wire issues that this
      entry originally claimed (no `child` field, hardcoded
      `detail=0`) are addressed: `CrossingEvent` now has a `child`
      field, encoder writes it, and `update_pointer_window`'s chain
      walk computes proper detail codes
      (NotifyInferior/Ancestor/Virtual/Nonlinear/NonlinearVirtual)
      via `crossings::normal_mode_crossings`, plus
      `crossings::implicit_grab_crossings` now also populates
      `child` per spec.
      Original repro (e16 hover popup fires over xterm) is **not
      fixed** by these changes. x11trace analysis showed e16 IS
      selecting Enter/Leave + Motion on root, and is receiving
      Enter events with correct detail codes after the fix — but
      the popup still fires. Whatever signal e16 uses for the
      "click on desktop" hover gating isn't load-bearing on
      Enter/Leave-with-detail semantics in a way these changes
      affect. Needs a side-by-side wire-trace comparison against
      real Xorg+e16 to identify the actual signal e16 relies on
      (candidate: motion-event delivery semantics for embedded
      widgets, or a XEmbed-protocol interaction). Deferred.
      Followup gap noted during investigation: yserver's
      `update_pointer_window` only emits crossings on **top-level**
      transitions. Per X11 spec, when cursor enters a top-level
      with descendants, Enter events should fire on EACH window in
      the chain (deepest descendant → ancestors), with detail codes
      indicating position-in-chain. yserver instead fires only on
      the top-level; for now the propagation walk in
      `pointer_fanout::pointer_propagation_target_by_id` covers the
      common case (subscribers on ancestors get events from
      descendants via walk-up). This is non-spec-compliant but
      practically works for xfce/mate/marco. Re-do as deepest-target
      tracking if a real client surfaces a bug here.
## Drawing / rendering artifacts

- [ ] **wezterm client content briefly blank after heavy drag
      stress (yoga / Snapdragon X1 / Turnip, release build,
      observed once 2026-05-19 PM).** During a deliberate
      window-drag stress test (purpose: check whether v2 spikes
      CPU under marco-comp drag — it doesn't), wezterm's client
      area went blank/white for a few seconds while marco's frame
      decoration kept rendering normally. Resolved on its own.
      Not reproducible on a debug build (timing-sensitive). No
      telemetry capture at the moment of the symptom.
      Two working hypotheses:
      1. Buffer-age history loss during the drag-induced
         ConfigureNotify storm + Stage 2e failed-flip recovery
         momentarily presenting a stale/uninit BO for wezterm's
         backing.
      2. Marco briefly dropping its `NameWindowPixmap` alias on
         wezterm mid-drag, so its compositor reads nothing for a
         few frames and the COW shows marco's frame-fill (white)
         through where wezterm content should be.
      Reproduction hint: heavy drag of any window while wezterm is
      running with btop or similar continuously-painting workload.
      Capture `yserver-mate-hw-telemetry` log during a repro
      attempt — diff per-second counters across the symptom
      window for clues (especially `composite_submit`,
      `frame_present`, scene-structure damage path).

- [ ] **Stage 4c follow-up: multi-output coord-space for
      scene-structure damage rects.** `SceneCompositor::
      mark_scene_structure_damage_rects` (4c.1, scene.rs:410)
      clips each input rect by `output_extent` only, assuming
      every output starts at origin (0, 0). Stage 4c.4 feeds it
      screen-absolute rects from `window_absolute_rect`
      (backend.rs:558). Under single-output (layout_x0 = layout_y0
      = 0) screen-absolute ≡ output-local, so the clip is correct.
      Under multi-output deployments with non-(0,0) layouts the
      helper would clip in the wrong frame and a damage rect on
      output 2+ would either be dropped (rect outside its
      extent-from-origin) or land at the wrong screen offset.
      Either: (a) add `layout_origin: vk::Offset2D` to
      `OutputSceneState` and threading through `SceneCompositor::
      new`, or (b) translate at the call site in
      `set_window_scene_participation`. Not a regression vs the
      4c.1 stub (same shape), but undocumented before this entry
      and tracked here to surface when multi-output work begins.

- [ ] **Stage 4c follow-up: `src_size = [1, 1]` after redirected W
      resizes past B's extent.** `build_scene`'s redirect-aware
      indirection (Stage 4c.3, commit `b9a9f3a`) emits
      `CompositeDraw.src_size = [1.0, 1.0]` (normalized UV "sample
      whole texture") for the W entry. When W is Automatic-redirected
      to backing B and W later RESIZES past B's extent, the scene
      stretches B onto W's new larger `dst_size` instead of leaving
      the now-uncovered region undefined. Conversely on W-shrink the
      scene samples a B-region larger than W now is, with the same
      undefined result. Either: (a) resize B in lockstep with W (per
      Composite spec NameWindowPixmap aliases freeze at pre-resize
      content so this is delicate), or (b) clamp `src_size` to the
      W/B extent ratio. Plan §4c didn't pin a behaviour; revisit
      before the 4c hardware smoke gate (mate + marco) or risk
      stretched window content on resize.

- [ ] **xeyes resize-DOWN artefact on v2 + mate + marco
      (2026-05-17, open).** Continuous-drag-shrink leaves xeyes
      with eye geometry sized for a *wider* window than what's
      now displayed — eye 2 visibly cut off at the right edge.
      Resize-UP works clean after the 2026-05-17 fix chain
      (MaskScratch swizzle + trap-shader AA + xid-detach +
      pixmap zero-fill). Two hypotheses, neither confirmed:
      1. xeyes' eye-geometry state stale during rapid drag —
         pixmap dims caught up but draw geometry didn't. Verify
         by stopping the drag and waiting 2-3 seconds before
         dumping the scanout; if the eyes settle to correct
         smaller shape, it's an xeyes-side race.
      2. v2 scene compositor blits the wrong storage — perhaps
         a pending-ack carrying an old DrawableId. Less likely
         given the test coverage, but worth re-checking the
         `pending_acks` capture path.
      Also visible (orthogonal): many `render_composite gap:
      host_src 0x40xxxx not resolvable` lines from marco's
      decoration compositing, depending on `name_window_pixmap`
      stubbed `Err` on v2 (Stage 4). Real fix is Stage 4 — the
      gaps aren't a v2 regression, but the noise complicates
      diagnosis here.
- [ ] **MATE panel flicker on v2 (2026-05-17, open).** Reported
      with the resize-down session above; not yet diagnosed.
      Could share a root cause with the xeyes shrink bug (rapid
      configure_subwindow on panel applet activity) or be its
      own scene-damage issue. Worth capturing a focused
      x11trace + RUST_LOG=debug session when picked up.

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
- [ ] **`render_set_picture_filter` honoured only as Nearest.**
      v1: `render_set_picture_filter`
      (`crates/yserver/src/kms/backend.rs:12392`) accepts the
      request but ignores the filter — Vk Composite shader uses
      a fixed sampler. v2: filter is stored on
      `PictureRecord::Drawable` (3b) but the engine still
      samples Nearest regardless of the requested filter
      (`Bilinear` / `Convolution` deferred per spec § "Out of
      scope" — Stage 5 perf work). Clients that depend on
      `Bilinear` for picture-source scaling see Nearest output.
      Stage 3 status quo per the Stage 3 plan §"Non-goals" #8.

- [ ] **Compositor shadow margins render as opaque bars
      (NameWindowPixmap → BadAlloc).** Diagnosed 2026-05-15 from
      a fresh `xfce.xtrace`. Visible as thin perpendicular dark
      bars to the right and below xfce pop-up menus (any GTK
      Client-Side-Decoration popup with alpha-shadow margins).
      Same gap will affect every Manual-mode compositor: xfwm4,
      picom's xrender backend, xcompmgr, compton.

      **Five-step fault chain:**

      1. xfwm4 issues `RedirectSubwindows update=Manual(0x01)` on
         root. Accepted by the 2026-05-14 fix
         (`feedback_composite_manual_redirect_trap`).
      2. That fix registered the redirect *record* but **skipped**
         `activate_redirect_backing_for`, so
         `host_window_to_backing` stays empty for every redirected
         window.
      3. xfwm4 issues `NameWindowPixmap` for the menu (depth-32,
         override-redirect).
      4. yserver's `name_window_pixmap`
         (`kms::backend::KmsBackend`, `backend.rs:10506`) looks up
         `host_window_to_backing`, finds nothing → returns
         `Err(NotFound)` → wire-level **BadAlloc** on
         `Composite-Request(144, 6)`.
      5. xfwm4 falls back to `CreatePicture` directly on the
         **live window** instead of its offscreen pixmap, then
         composites from that picture. The live window only
         carries visible RGB; the GTK CSD shadow-alpha that lives
         in the offscreen backing is unreachable, so xfwm4 emits
         opaque shadow-color pixels where the alpha gradient
         should fade.

      **Status:** the 2026-05-15 attempt to fix this in the
      current rendering model (the abandoned
      `render-convolution-filter` branch, T1-T4 of the Manual-
      redirect plan) made the BadAlloc go away at the protocol
      level but exposed the deeper structural mismatch — yserver's
      scanout walks per-window mirrors and never displays root's
      mirror, so the compositor's RENDER paint to root is
      invisible. **Deferred** to the v2 rendering-model rewrite,
      where COMPOSITE redirect collapses to "this window's storage
      target moves" and the compositor's paint to root is
      naturally what `SceneCompositor` consumes. See
      `docs/superpowers/specs/rendering-model-v2.md` (TBD).
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
- [ ] **PictFormat tracking + ARGB picture intent (Stage 4e
      candidate).** Surfaced as residual from Stage 4 close
      (2026-05-21). v2 forces opaque on depth-24 source pictures
      (`d20f279`) and routes xRGB/RGB24 through the resolver as
      forced-opaque, but doesn't track per-picture PictFormat
      semantics (xRGB-as-render-intent vs ARGB) end-to-end. Full
      compositor-WM correctness (consistent ARGB-aware shadow /
      transparency / multi-layer Render::Composite) needs
      PictFormat tracking on `PictureRecord::Drawable` + the
      backend sampler, plus picture-source alpha interpretation
      per format. Until then the cow-authoritative scene gating
      keeps the compositor's path working but full Render
      Composite correctness in complex DE chrome (e.g. tooltip
      drop-shadows blending against arbitrary backgrounds) is
      not guaranteed. Marked candidate "Stage 4e" in earlier
      planning; not on the Stage 5 perf path.

      **2026-05-22 investigation status — UNRESOLVED yserver bug:**
      gtk3-demo + tooltip CSD shadows render visibly different on
      yserver than on Xorg-native (denser dark band at the window
      edge). Setup: **same machine, same MATE installation, same
      theme/wallpaper files, same GTK/Cairo binaries** — only the
      X server differs. Pixel diff against the Xorg reference:
      inner-shadow RGB `(31,59,48)` on yserver vs `(42,81,68)`
      on Xorg over wallpaper RGB `(75,144,117)` vs
      `(75,144,121)` — implied stored α ≈ 151 on yserver vs
      ≈ 112 on Xorg. **Even the wallpaper green channel differs
      by 4 LSB despite identical wallpaper file**, indicating a
      color-pixel-rendering divergence in addition to the shadow
      one. Per `AGENTS.md`: yserver must match Xorg, this is a
      real yserver bug, root cause not yet identified.

      Ruled out so far:
      1. Pixel-level compose math: marco's `XRenderComposite(Over,
         src=ARGB32_window_picture, mask=None, dst=offscreen_d24)`
         produces `result.rgb = src.rgb + (1-src.a)*dst.rgb`
         exactly — scanout = marco offscreen bit-for-bit. So the
         divergence is upstream of marco's compose.
      2. Force-opaque intent: `picture_pict_format` returns
         `RENDER_FMT_ARGB32` for the GTK CSD picture →
         `resolve_force_opaque_pict_format` returns false → α
         is not stomped to 1.0. Not an ARGB-vs-xRGB picture
         intent bug.
      3. Marco shadow plugin: `window_has_shadow()`
         (`compositor-xrender.c:1013-1088`) returns FALSE for
         `cw->mode == WINDOW_ARGB` with no marco frame on either
         Xorg or yserver — neither adds a Gaussian soft halo
         around CSD windows. Asymmetry vs SSD MATE apps is
         marco's intentional policy, identical on both servers.
      4. RANDR / SETUP physical-mm dimensions: confirmed
         bit-identical GTK CSD backing alpha before and after
         fixing SETUP mm (38-DPI → 96-DPI synth) and per-output
         mm (96-DPI synth → real EDID via `drmModeConnector.size()`).
         GTK does not read RANDR or SETUP mm to compute CSD
         shadow density on this path. Both fixes shipped
         2026-05-22 as a separate correctness win, see
         [feedback_dpi_hardcoded_matters.md].

      Candidates not yet ruled out — where the divergence likely
      lives in yserver:
      - **Visual / PictFormat advertisement shape** — yserver
        advertises ~5 visuals vs Xorg's ~272 (visible in
        `xdpyinfo`). GTK may pick a different visual for the
        CSD pixmap or root window based on what's listed, and
        the chosen visual's bit/mask layout drives Cairo's
        rendering path.
      - **`PutImage` byte-order / depth-32 handling** — the
        wallpaper RGB `B` channel differs by 4 LSB
        (`75,144,117` vs `75,144,121`) despite the same source
        file. This is a real color divergence in something
        upstream of the CSD shadow problem. Could be wire-byte
        permutation in `put_image` for depth-24/32, a swizzle
        in DRI3 import, or the root storage's interpretation
        of client-uploaded bytes.
      - **Cairo Xlib surface selection** — GTK calls
        `cairo_xlib_surface_create_with_xrender_format` which
        picks a format based on the server's `QueryPictFormats`
        reply shape. yserver's format-list ordering or content
        could be steering Cairo into a different rendering
        path.
      - **`RENDER QueryFilters` reply** — affects which Cairo
        filtering paths GTK uses (Nearest/Bilinear/Convolution).
        Need to confirm yserver advertises identical filter set
        to Xorg.
      - **XSETTINGS** — `mate-settings-daemon` is the same
        binary on both servers, but its initial-write may
        depend on what it reads from the server first. Compare
        the `_XSETTINGS_SETTINGS` property between sessions.
      - **`gdk-window-scaling-factor` setting source** — could
        yserver end up reporting something that makes GdkWindow
        compute a different scale?

      Next concrete diagnostic step: capture a side-by-side
      gtk3-demo run on Xwayland (96-DPI synth, no real EDID,
      same as yserver's fallback path) versus yserver, same
      session, same theme, same wallpaper file. If Xwayland's
      gtk3-demo matches Xorg-native, the divergence is yserver-
      specific (and not "Xwayland-style 96-DPI" causing it). If
      Xwayland matches yserver, the divergence is in something
      yserver-and-Xwayland both lack vs Xorg-native (likely
      something EDID-derived beyond mm dimensions, or color/
      gamma path).

      Marked candidate "Stage 4e" in earlier planning; not on
      the Stage 5 perf path.
- [ ] **KmsCore.pictures disconnect cleanup.** Stale Picture
      records from disconnected clients (e.g.
      mate-session-check) can persist in v2's `KmsCore.pictures`,
      causing `render_composite gap` noise lines.
      `yserver-core/src/core_loop/process_disconnect.rs:261`
      already loops `removed.freed_pictures` calling
      `backend.render_free_picture`; verify v2's impl actually
      evicts from `KmsCore.pictures` and add the missing eviction
      if it doesn't. ~50 LoC. Surfaced from Stage 4d open items
      (now in
      [`status-archive-2026-05-21.md`](status-archive-2026-05-21.md)).
- [ ] **MATE Control Center yellow group-header labels missing
      (mate-no-comp).** Group headers in the Control Center
      sidebar ("Filter", "Groups", "Common Tasks") render
      invisible. Reproducible without a compositor, so not a
      COW/redirect bug. Likely a colored-source + glyph-mask
      Render::Composite path issue — yserver's Render glyph
      compositing with a non-default source colour is the most
      plausible suspect. Surfaced from the Stage 4d hardware
      smoke (now archived).

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

      **Post-Phase-5 / pool / GPU-trap update (2026-05-15)**: Phase 5
      sync rework, the pixmap pool, and GPU trap rasterization all
      landed; perceived lag on fuji (Intel) improved dramatically.
      `bee` (AMD RDNA2) is "no GPU faults but laggy" per status.md.
      `silence` (powerful machine with discrete GPU) **still
      reproduces the catastrophic mate-cc + adapta-nokto cliff**,
      which rules OUT "the slow machine just can't keep up" — the
      absolute submission count or per-frame work exceeds the
      budget even on capable hardware. Remaining suspects:
      `GlyphAtlas::intern` per-glyph wait still present (filed
      separately); RENDER Composite path's per-op submission rate
      under heavy GTK-glyph workloads; or a higher-level frame
      management gap that lets one workload starve subsequent ones.
      Cross-vendor reproduction (Intel-fuji OK, AMD-bee residual,
      AMD-silence catastrophic) suggests hardware/driver factors
      compound on top of an absolute-rate problem.

## WM-specific behaviour

- [ ] **e16 popup rounded corners.** Cosmetic — popup outer shape is
      Set+Intersect rectangular bounding; rounded look comes from
      bg-pixmap content, with small black pixels at the very corners
      because the popup outer's bg isn't auto-filled there
      (default bg = None). Investigate `ParentRelative` bg forwarding.
- [ ] **openbox frame chrome.** Frame title bars / labels / buttons
      don't draw under openbox even after the Phase 3.4 atom fix;
      clients render correctly inside the frames. Suspect: openbox
      draws frame decorations into 1×1 sub-windows of the frame and
      that drawing path doesn't reach the host (same family as the
      old wmaker chrome bugs).

## Extension polish

- [ ] **Real RANDR `SetCrtcConfig` / `SetScreenConfig` (runtime
      mode change).** The stub above always replies Success
      without actually switching modes. To honour the client's
      request we'd need ~2–3 phases of work spanning the protocol
      and KMS backend:
      1. Enumerate all KMS-supported modes per connector via
         `drmModeGetConnector` and surface them through
         `GetScreenResources` / `GetOutputInfo` with stable XIDs.
      2. Round-trip mode XID → `drmModeModeInfo` for atomic
         commit.
      3. Runtime modeset entry point in `kms::backend` (we have
         startup `enable_output` + `disable_output`; need a
         "switch this CRTC to mode M" that re-allocates scanout
         BOs at the new size, reprograms the atomic commit, and
         rolls back on kernel rejection).
      4. Resize composite pass attachments + Vulkan framebuffers;
         drain in-flight work cleanly across the switch.
      5. Resize the X11 root, fire ConfigureNotify/ResizeRequest
         to top-levels, recompute the window-tree layout against
         the new screen extent.
      6. Send `RRCrtcChangeNotify` + `RRScreenChangeNotify` to
         every RANDR client that selected input (depends on the
         separate "RRSelectInput mask storage" followup
         landing first).
      7. Multi-output sequencing + partial-failure semantics when
         the client configures multiple CRTCs in one request flow.
      8. Wire-level validation: reject mode XIDs not ours, reject
         kernel-rejected configurations.
      Each piece is reasonable on its own; together it touches the
      compositor critical path and is its own phase of work.
      Deferred until the use case (interactive resolution change /
      hot-plug reconfiguration) becomes load-bearing.
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
## Core loop fairness

- [~] **Listener accept starves under high-volume per-client request
      streams.** Not currently reproducible (as of 2026-05-15) but
      the unbounded per-iteration read budget that underlies it is
      still present, so the shape remains. Original observation
      from 2026-05-13 with `just yserver-xfce-hw`:
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

- [ ] **README pointer to this file.** So the next reader knows
      where the bug ticklist lives.

## Archived: ynest-era (pre-rendering-rework, KMS-direct supersedes)

yserver now runs KMS-direct (no host X server). The items below
were filed when yserver ran nested under `ynest` against the
host Xorg; many of the failure modes simply don't exist on the
KMS path. Kept here for reference if we ever revive ynest-based
WM smoke testing.

- [ ] **CopyArea BadMatch (~100/wmaker startup).** ynest passed
      `CopyArea` requests through to the host X server, which
      rejected drawable pairs of mismatching depth. Pre-rework
      noise from wmaker on ynest.
- [ ] **PolyFillRectangle BadMatch (~15/wmaker startup).** Same
      shape as the CopyArea pattern (ynest forwarded; host
      rejected).
- [ ] **Composite NameWindowPixmap BadAccess (~28/wmaker
      startup).** ynest called `NameWindowPixmap` on un-redirected
      windows; host rejected with BadAccess. Mattered for
      picom-like compositors under the old setup.
- [ ] **MIT-SHM `XShmPutImage` host fast path.** Would have let
      ynest forward a single 256-KB+ image to the host without
      chunking. Irrelevant on the KMS path (no host server).
- [ ] **gtk3-demo / gtk4 demo silent-exits.** gtk-demo started
      and exited silently against the host `:0`, nested or not.
      Pre-existing dconf/stdin quirk in the dev environment.
- [ ] **Down-grade known-benign host errors to DEBUG.** Small
      classifier in `BackendEventSink::handle_backend_event` to
      drop the WARN noise from ynest-host error replies. No host
      server on the KMS path.
- [ ] **fvwm3 segfaults on host container resize / window close.**
      Triggered by ynest-host window resize. yserver KMS doesn't
      currently support output resize at all, so this code path is
      not reachable. Pre-existing fvwm3 bug independent of yserver.
- [ ] **fvwm: apps disappear after host container resize.** Same
      ynest-host-resize trigger as the segfault above. Not
      reachable on KMS-direct.
