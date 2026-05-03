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
- [ ] **XInput2 GenericEvents (event type 35) silently dropped by the
      dispatcher.** Phase 6.3 regression. Affects every XI2-using
      client: xeyes (visible symptom: doesn't track cursor), modern
      toolkits relying on XI2 motion / scroll valuators / etc.

      **Root cause:** `host_x11/mod.rs:1566-1577` in
      `read_dispatch_message` discards `header[0] == 35` frames with
      a comment "XInput2 lives on the per-client kb fanout below,
      not the host pump." That fanout was dissolved in Step 4 (the
      Big Flip), so the dispatcher's drop-on-skip path now leaks
      every GenericEvent. Pre-Phase-6.3, the per-client kb pump's
      separate connection received and fanned out XI2 GenericEvents
      directly to its client.

      **Fix shape:** dispatcher classifies GenericEvent (type 35) +
      its variable-length extra payload as a new
      `BackendEvent::HostEvent(HostEvent::Generic { ... })` carrying
      the full bytes. Sink fans it out to whichever client selected
      XI2 events on the relevant window (`xi2_select_masks`
      bookkeeping in `nested.rs` already exists from Phase 3.x —
      reuse). Either translate to a typed XI2 event variant in
      `decode_host_event` or pass the raw 32+extra bytes through
      and let the client-side fanout do the parsing.

      Reproduction: `RUST_LOG=debug target/release/ynest 99 &;
      DISPLAY=:99 xeyes &`; move mouse over xeyes; observe that
      xeyes never queries position (no `QueryPointer` in log) and
      that no XI2 GenericEvent reaches a client. xev works in the
      same setup because xev selects core `PointerMotion` (event
      type 6), which decode_host_event still handles.

## Drawing / rendering artifacts

- [ ] **xclock seconds hand not drawn.** Pre-existing — was working
      at some earlier point; not a Phase 6.3 regression. The minute
      and hour hands render. Investigation: trace which RENDER /
      core drawing op xclock uses for the seconds hand vs the others;
      likely a clip / coord / opcode subtlety in one path that the
      others avoid.
- [ ] **wmaker icon-edge clamped to 800×600 with larger geometries.**
      Start ynest with e.g. `--geometry 1200x900`, run wmaker:
      icons/dock are laid out as if the right edge is at column 800.
      Windows can be dragged beyond that edge into the actually-empty
      area to the right, so the X11 root size is correct end-to-end —
      this is wmaker holding a stale screen-size somewhere. Maybe a
      RANDR `RRGetScreenInfo` / `RRGetScreenResources` reply path that
      still reports 800×600 even when the container was created
      larger. Worth a `xrandr -d :99` check after a startup vs. later.
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
