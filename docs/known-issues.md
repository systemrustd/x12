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
      tracking. Visible behavior is unaffected (other CopyAreas in the
      cascade succeed) so this is purely log-noise.
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

## Validation surface

- [ ] **XTEST extension in ynest.** Implementing `XTestFakeKeyEvent` /
      `XTestFakeButtonEvent` (extension major 138, opcodes 2 and 3)
      would let `xdotool key`/`xdotool click` drive ynest in headless
      smoke tests. ~50–100 LoC. Today the bwrap-sandbox X test runs
      can only validate visual rendering and client connect, not input
      event delivery.
- [ ] **gtk3-demo / gtk4 demo runs in this dev environment.** gtk-demo
      (gtk4) starts then exits silently on the host's `:0` regardless
      of whether it's run nested under ynest or directly. Pre-existing
      environment / dconf / stdin quirk; needs investigation. Blocks
      the gtk3-demo arm of the WM smoke matrix.

## Known limitations / minor follow-ups

- [ ] **xterm scrollback misbehaviour.** `seq 1 200` scrolls forward
      cleanly but scrollback via scrollbar / mouse wheel / PageUp /
      Shift+PageUp doesn't behave correctly. No unsupported opcodes in
      the trace; likely a coord/grab/clip subtlety. Phase 1 follow-up,
      parked since the rest of xterm works.
- [ ] **e16 popup rounded corners.** Cosmetic — popup outer shape is
      Set+Intersect rectangular bounding; rounded look comes from
      bg-pixmap content, with small black pixels at the very corners
      because the popup outer's bg isn't auto-filled there
      (default bg = None). Investigate `ParentRelative` bg forwarding.
- [ ] **fvwm3 segfaults on host container resize / window close.**
      Pre-existing fvwm3 bugs (reproduce without ynest changes).
      Re-test after the next fvwm3 update.
- [ ] **openbox frame chrome.** Frame title bars / labels / buttons
      don't draw under openbox even after the Phase 3.4 atom fix;
      clients render correctly inside the frames. Suspect: openbox
      draws frame decorations into 1×1 sub-windows of the frame and
      that drawing path doesn't reach the host (same family as the
      old wmaker chrome bugs).

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
