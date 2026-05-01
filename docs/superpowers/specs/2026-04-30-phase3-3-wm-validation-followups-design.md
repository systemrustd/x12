# Phase 3.3 — WM Validation Follow-Ups Design

## Goal

Close the WM-validation polish gaps surfaced during Phase 3.2's wmaker/e16
bring-up, and bring Openbox up cleanly under `ynest`. Success
means real window managers render their chrome correctly and behave
correctly through normal user interactions: drags, popups, cursor changes,
host-container resize.

Phase 3.3 is a polish phase, not a feature phase. No new extensions are
advertised; the protocol surface only grows by single opcodes (XFIXES
`ChangeCursorByName`) plus host-side forwarding of opcodes we already
handle locally.

## Validation Targets

In order:

1. **Regression set:** `gtk3-demo`, `xeyes`, `xclock`, `xterm`, fvwm3
   startup. Must continue to pass after each item lands.
2. **wmaker re-validation:** items #1 (SHAPE forwarding) and #3 (cursor-
   by-name) clean up the artifacts noted in Phase 3.2's pass; items
   #2/#4 drag-redraw verification runs here.
3. **e16 re-validation:** same items as wmaker, plus item #5 (RENDER
   audit).
4. **fvwm3 re-validation:** item #6 (root resize plumbing) — fvwm3
   reflows desktop bg correctly. Item #7 (apps disappear after host
   resize under fvwm) is deferred; expect that symptom to still
   reproduce and document it.
5. **Openbox bring-up:** first time under ynest. Expected discoveries:
   chrome glitches, missing opcodes, possibly grab/focus oddities.

## Scope

In scope (six items from `status.md`):

1. SHAPE forwarding to host for top-levels.
2. Verify (and if needed, supplement with synthetic Expose) the
   backing-store mitigation for occluded / off-screen drags.
3. XFIXES `ChangeCursorByName` (minor 23).
4. Sub-window Expose for cross-border / behind-sibling drags — joins
   #2's verification, same fix surface.
5. Verify e16 RENDER coverage. Investigation deliverable; one-line
   wire-shape fixes may land if discovered.
6. Root window resize plumbing: `ConfigureNotify` on root, `GetGeometry`
   agreement, container `bg_pixel` re-tile verification.

## Non-Goals

- Item #7 from `status.md` (apps disappear after host resize, fvwm) is
  deferred. Root cause unknown; scoping risk too high for this phase.
- Item #8 from `status.md` (fvwm segfault on host close) is deferred.
  Same reason.
- Issues discovered during Openbox validation are filed as
  Phase 3.4 follow-up bullets in `status.md`, not fixed in 3.3.
  Exception: a one-line opcode dispatch or missing-reply fix may land
  if it unblocks the validation run itself.
- Fluxbox is not installed in this environment, so it is not a Phase
  3.3 validation target here. If it becomes available later, validate
  it as a separate follow-up rather than blocking Phase 3.3 closeout.
- No new extensions advertised. No protocol surface expansion beyond
  what items #1, #3, and #6 require.
- GLX, MIT-SHM, accelerated paths, real frame scheduling — Phase 4.

## SHAPE Host Forwarding (item #1)

### Problem

`handle_shape_request` updates per-window `ServerState::shape_windows`
but never tells the host. The host treats every top-level subwindow as a
full rectangle, so themed WM frames render with extra/missing pixels.
e16 alone issues ~1900 `SHAPE::Mask` calls per session that go nowhere.

### Design

**Host extension detection.** During `HostX11` initialization, issue
`QueryExtension("SHAPE")` over the host connection. Cache the major
opcode and present-flag on `HostX11` (new field `host_shape_opcode:
Option<u8>`). If absent, log once at INFO and fall back to current
local-only behavior.

**Forwarding strategy: resolved-rect mirror, not per-opcode mirror.**
Forward only `ShapeRectangles` to the host, regardless of which local
opcode the client used. The local handler already resolves
`Rectangles`/`Mask`/`Combine`/`Offset` into a final per-window
bounding/clip rectangle list in `ServerState::shape_windows`. After any
mutation that changes the resolved list for a top-level with a
`host_xid`, replay the new list to the host as a single
`ShapeRectangles(kind, op=Set, ordering=Unsorted, rects=resolved)` call.

This avoids three failure modes that per-opcode mirroring has:

- `SHAPE::Mask` and `SHAPE::Combine` accept a source pixmap/window that
  may have no host XID. Per-opcode mirroring would have to fall back to
  local-only and silently diverge from the local truth. Resolved-rect
  mirroring always works because the local handler has already
  flattened the source into a rectangle list.
- `SHAPE::Combine` with a top-level destination and a local-only child
  source is a real case (titlebar masks combined into frame chrome) —
  per-opcode mirroring breaks here, resolved-rect mirroring doesn't.
- `SHAPE::Offset` semantics are preserved because the resolved list is
  re-derived after the offset and re-sent.

**Forwarding surface.** Add a helper
`host_x11::set_shape_rectangles(host_xid, kind, rects)` that issues
`ShapeRectangles(host_xid, kind, op=Set, ordering=Unsorted, rects)`.
Called from `nested.rs::handle_shape_request` after each mutation that
changed the resolved bounding or clip list for a window with a resolved
`host_xid`. Called once for bounding and once for clip if both changed.
Sub-windows without a `host_xid` keep their local-only behavior — the
parent's host shape already clips them.

**Coordinate translation.** SHAPE rectangles on a top-level are in the
top-level's local coords. The host subwindow's origin already matches
the top-level's local origin (offset is at the host-window level, not
the rect level — see `host_x11.rs::create_subwindow`), so no per-rect
translation is needed. Verified by a unit test that round-trips a known
rect set.

**State coherence.** Local `shape_windows` state remains the source of
truth for `QueryExtents`, `GetRectangles`, and `InputSelected`. Those
do not round-trip via the host. The host receives the same resolved
bounding/clip rect lists, so local and host shape stay in lockstep.
Input shape stays local-only.

### Deferred to Phase 3.4

- Input-shape hit testing in the pointer pump (already deferred from
  Phase 3.2).

### Test Surface

- Unit tests for `set_shape_rectangles`: rect-list encoding, kind enum
  mapping, no-host-opcode fallback path.
- Unit test that exercises the resolved-rect path for `Combine` with a
  local-only source: confirm the host call carries the correct merged
  rectangle list.
- Manual: rerun e16 and confirm themed frames render with correct
  silhouettes.

## Drag Redraw Verification + Synthetic Expose (items #2 and #4)

### Problem

- #2: dragging a frame off-screen and back, or fully behind a sibling
  top-level and back, leaves stale content until the client redraws.
- #4: cross-border / behind-sibling drags don't trigger sub-window
  Expose because the host can't generate Expose for areas it never had
  pixels for.

`backing-store=Always` + `bit-gravity=NW` already landed in commit
`93b988a`. Phase 3.3 verifies first, then implements the synthetic-
Expose path only if the symptoms reproduce.

### Phase A — Verification (mandatory)

Reproduce the original symptoms under each WM (wmaker, e16, fvwm3, plus
Openbox once it's up). Specifically:

1. **Off-screen drag:** drag an `xterm` frame so it is fully outside
   the ynest container, then back inside. Expected: full visual
   content restored without manual repaint.
2. **Behind-sibling drag:** stack `xterm` fully behind another top-
   level, then raise it. Expected: full visual content restored.
3. **Sub-window cross-border drag:** drag a top-level whose chrome
   includes nested sub-windows (wmaker titlebar, app content panes)
   so its sub-windows cross the container edge, then back.

If all three pass under all WMs: items #2 and #4 are marked resolved
in `status.md` with a note crediting `93b988a`, and Phase B is skipped.

### Phase B — Synthetic Expose (only if Phase A fails)

Two distinct triggers, each with a precise signal — no fragile
heuristics:

**Trigger 1: off-screen recovery (geometry-driven).** In the host
pump's `ConfigureNotify` handler for a top-level subwindow, compare
the previous host-reported geometry against the new geometry. If the
previous bounding rect had any portion outside the container's bounds
*and* the new bounding rect now lies inside (or partially inside) the
container, synthesize a full-window `Expose`. Geometry comparison alone
is reliable here.

**Trigger 2: occlusion recovery (visibility-driven).** Select host
`VisibilityNotify` (event mask `VisibilityChangeMask = 0x0001_0000`) on
each top-level subwindow when it is created in `host_x11.rs`. Pump
those events alongside `ConfigureNotify` and `Expose`. On a transition
from `VisibilityFullyObscured` → `VisibilityUnobscured` or
`VisibilityPartiallyObscured`, synthesize a full-window `Expose` for
the corresponding nested top-level. This replaces the configure-delta
heuristic, which would over-fire on every WM-driven drag.

**Synthesis.** Build an `Expose` event with `(x=0, y=0, width=full,
height=full, count=0)` in the top-level's local coordinates and route
via the existing per-(client, window) event-mask fanout. Reuse
`expose_event_fanout` so descendant sub-windows get their share.

**Suppression.** Track per-top-level "last host Expose serial seen
within N ms" in the host pump's per-window state (a simple
`Option<Instant>` on a struct keyed by host XID). If a covering host
Expose arrived within the suppression window, skip synthesis — the
host already did the work. Bookkeeping lives in the host pump alongside
the existing per-host-window state used for input fanout.

**What this won't fix.** The off-screen-recovery trigger relies on
host-side geometry reports. If the host coalesces a "moved off-screen
then back" sequence into a single net configure, no transition is
visible. Acceptable: backing-store should already have preserved
content for that case; Trigger 1 is for the case where backing-store
couldn't preserve pixels at all.

### Test Surface

- Phase A: manual repro of all four scenarios under all WMs:
  1. Off-screen drag and back.
  2. Behind-sibling drag and back.
  3. Sub-window cross-border drag and back.
  4. **Pure restack:** fully obscure a window by raising a sibling on
     top of it (without moving either window), then lower the sibling.
     This isolates the occlusion case from any geometry-change traffic
     and is the cleanest test of the synthetic-Expose visibility
     trigger.
- Phase B (if required): unit test for off-screen-recovery geometry
  comparison and Expose wire shape; unit test for the
  `VisibilityNotify` transition state machine (only synthesize on
  `FullyObscured → {Unobscured, PartiallyObscured}`); manual repro of
  the four scenarios above.

## XFIXES `ChangeCursorByName` (item #3)

### Problem

e16 calls `XFIXES::ChangeCursorByName` (minor 23) 7+ times during cursor
theming. We log it as `XFIXES::unknown`. No reply is expected, so it
doesn't block — silent UX failure.

### Design

**Request shape.** Minor 23: `cursor: CURSOR (4 bytes)`, `nbytes: u16`,
padding, then `name` (UTF-8, `nbytes` bytes, padded to 4-byte boundary).

**Approach: forward minor 23 to the host as-is.** `host_x11.rs` is raw
wire code — there is no Xlib/Xcursor integration available, and adding
one would be disproportionate scope. The host server (typically Xorg
or Xephyr/Xwayland) already implements `XFIXES::ChangeCursorByName`
itself; the right move is to forward the request and let the host
resolve the name against its own cursor theme.

**Capability detection.** During `HostX11` initialization, issue
`QueryExtension("XFIXES")` over the host connection. Cache the major
opcode and present-flag on `HostX11` (new field `host_xfixes_opcode:
Option<u8>`). If absent, log once at INFO and accept the request as a
local no-op. (This mirrors the SHAPE detection pattern.)

**Forwarding.** Translate the client cursor XID to its host XID via
the existing cursor map, then write a `XFIXES::ChangeCursorByName`
request to the host with the cached host XFIXES opcode, the translated
cursor XID, and the original `nbytes` + `name` bytes (no name parsing,
no cache, no Xcursor calls — pass through).

If the cursor XID has no host mapping (rare; suggests a client bug),
log at DEBUG and drop the request — don't crash and don't fail the
client.

**Implementation locality.** New branch in
`nested.rs::handle_xfixes_request`. New helper
`host_x11::xfixes_change_cursor_by_name(host_cursor_xid, name_bytes) -> Result<()>`.

### Test Surface

- Unit test for request parsing: cursor xid + name extraction with
  padding.
- Manual: rerun e16 and confirm cursor changes when hovering over chrome
  regions; check that `XFIXES::unknown` no longer appears for minor 23.

## e16 RENDER Coverage Audit (item #5)

### Problem

e16 sends 5400+ `RENDER::CreatePicture`/`FreePicture` and 4400+
`RENDER::Composite` calls per session. Visible chrome artifacts may be
caused by missing opcodes, wire-shape bugs, or by items #1/#2/#4. We
don't yet know.

### Design

This is an investigation deliverable. Three steps, each with a written
outcome.

**Step 1 — Instrumentation.** Add a debug-only RENDER request counter
gated behind a `RUST_LOG=ynest::render_audit=debug` target. For each
RENDER minor, count *what the nested side can observe directly*:

- total calls,
- calls forwarded to the host,
- calls dropped/no-op'd locally (with the reason: missing XID
  translation, unsupported attribute, etc.).

Emit a summary on client disconnect.

**Note on host errors.** Most RENDER requests are void writes, and
`HostX11` does not currently provide generic per-request error
attribution (errors arrive asynchronously and are logged but not tied
back to a specific minor). Counting "host returned a protocol error
for this minor" is therefore best-effort: the audit can correlate on
sequence numbers when we have them recorded, but a complete
host-error metric would require new plumbing. The audit treats this as
optional output — if available, include it; if not, the
forwarded/dropped counts plus xtrace cross-reference are enough to
drive triage.

**Step 2 — e16 session capture.** Run a 60-second e16 session with a
fixed scenario (start e16, open a couple of themed apps, hover/click
chrome, close them). Capture the audit output. Cross-reference with
the `xephyr-xclock-fvwm3-trace.log` parity check from Phase 3.1 — run
the same scenario under Xephyr where practical and diff `xtrace`
output (or our forwarded request stream) against ynest counts.

**Step 3 — Triage.** For each minor where audit shows host errors or
count divergence:

- **Wire-shape bug:** fix within Phase 3.3 (one-line padding/length
  fixes, comparable to the three bugs found in Phase 3.1).
- **Missing opcode:** file as Phase 3.4 follow-up — do not expand the
  opcode set in this phase.
- **Client-side ID translation bug:** fix within Phase 3.3.

### Concrete Spot-Check List

- `ChangePicture` (minor 5): forwards scalar attributes but blocks
  non-`None` XID attributes — specifically `CPClipMask` (nonzero) and
  `CPAlphaMap`. Verify e16 doesn't depend on either. (Repeat values
  *are* forwarded; the earlier draft of this spec was wrong about
  that.)
- `SetPictureClipRectangles` (minor 6): the opcode table in
  `status.md` marks it `✗`, but the code in `nested.rs` already
  forwards it with offset adjustment. The fix is to update the opcode
  table — the implementation is fine.
- `Composite` (minor 8): glyph offset patching in
  `host_x11.rs::compose_glyph_command` patches the *first* non-255
  glyph command's delta and stops. Multi-glyph-run composites can have
  later runs mispositioned. Real bug; if e16 hits the multi-run path,
  fix within Phase 3.3 (loop the patch instead of stopping at the
  first cmd).

### Deliverables

- Updated opcode table in `status.md` (including reconciling the
  `SetPictureClipRectangles` `✗`/forwarded mismatch).
- `Composite` multi-glyph delta-patching fix (extend the patch loop in
  `host_x11.rs::compose_glyph_command` to apply across all non-255
  glyph commands, not just the first) — this is a confirmed bug, not
  contingent on the audit.
- Any other wire-shape fixes that fall out of the audit (with byte-
  exact encoder tests).
- Phase 3.4 follow-up bullet listing any unmet RENDER opcodes e16
  uses.

## Root Window Resize Plumbing (item #6)

### Problem

`handle_host_container_resize` already updates `RandrState`, the
`ROOT_WINDOW` dimensions, and emits `RRScreenChangeNotify` plus CRTC/
output events. Three gaps remain:

- No `ConfigureNotify` is sent on `ROOT_WINDOW` to clients with
  `StructureNotify` selected on root. Panels and "fill the screen"
  clients without RANDR awareness rely on this.
- The container's `bg_pixel` re-tile across the new bounds is host-
  driven; we don't verify it actually happens correctly.
- `GetGeometry(root)` agreement after resize isn't tested.

### Design

**ConfigureNotify on root.** Inside `handle_host_container_resize`,
after the `RandrState`/`ROOT_WINDOW` dimensions update and before the
RANDR event fanout, build a `ConfigureNotify` with `event = root`,
`window = root`, `above_sibling = None`, `x = 0`, `y = 0`, `width/
height = new size`, `border_width = 0`, `override_redirect = false`.
Fan out via the existing `subscribers_for_event_mask(root,
StructureNotifyMask)` path. Reuse the existing `ConfigureNotify`
encoder.

Order: `ConfigureNotify` first, then `RRScreenChangeNotify`. Rationale:
core events before extension events lets non-RANDR clients react first
and gives RANDR-aware toolkits a stable core-geometry view by the time
they receive `RRScreenChangeNotify`. (Not yet verified against Xorg's
exact emission order — if a target client misbehaves under this
ordering, swap and document.)

**bg_pixel re-tile verification.** Verify (no code change unless
broken) that the host auto-clears the newly-exposed area to `bg_pixel`
when the container grows. If it doesn't, fix is to re-apply
`XSetWindowBackground` on the container after resize. Same "verify
first" approach as items #2/#4.

**GetGeometry agreement.** Add a unit test that calls
`handle_host_container_resize(new_w, new_h)` and asserts:

- `RandrState.screen_width == new_w` (and screen_height).
- `ROOT_WINDOW`'s stored geometry width/height match.
- `GetGeometry(ROOT_WINDOW)` reply encoder produces `(new_w, new_h)`.

**StructureNotify subscriber selection.** Most clients select
`SubstructureNotifyMask`, but the few that select `StructureNotifyMask`
on root are exactly the targets of this fix. Use the existing event-
mask subscriber lookup.

### Edge Case

Clients with both `StructureNotifyMask` on root and `RRSelectInput` for
`RRScreenChangeNotify` will see both events. Spec-correct — modern
toolkits expect both.

### Test Surface

- Unit test for the resize pipeline post-conditions (above).
- Unit test for `ConfigureNotify` on root wire shape (likely already
  covered by existing encoder tests; verify).
- Unit test asserting that a client which selected `StructureNotify`
  on root *and not* `RRSelectInput` receives a `ConfigureNotify` after
  `handle_host_container_resize`. This is the cleanest proof that the
  new core-event path works independently of RANDR.
- **Manual non-RANDR root-listener probe.** A WM "looking right" after
  resize is not proof — modern WMs use RANDR. Add a tiny standalone
  probe (Python or C) that selects only `StructureNotifyMask` on root,
  prints every `ConfigureNotify` it receives, and exits on Ctrl-C. Run
  it under `ynest`, resize the container, confirm the probe prints a
  `ConfigureNotify(root, w=new_w, h=new_h)`. Document the probe in
  `docs/` so future regressions can use it.
- Manual: also confirm a panel-style client (`feh --bg-tile` +
  re-tile-on-root-config, or similar) reflows on container resize.

## Validation Strategy

### Per-WM Scenario

For each WM in the validation order:

1. Start the WM under `ynest`.
2. Open `xterm`, `xclock`, `xeyes`. Confirm rendering and chrome.
3. Drag each frame: across the container, off-screen and back, behind
   a sibling and back. Drives items #2/#4 verification.
3a. **Pure restack** (separate from the drag step): with no movement,
   raise a sibling fully on top of `xterm`, then lower it. Confirms the
   visibility-driven occlusion-recovery path independent of any
   geometry change.
4. Hover and click chrome regions to drive cursor changes. Drives
   item #3 under e16, plus general cursor sanity.
5. Resize the ynest container by 50% in each dimension. Confirm
   desktop bg re-tiles, panels reflow, app frames remain visible.
   Drives item #6.
6. Close apps via WM controls. Confirm clean teardown.
7. Quit the WM cleanly via its own menu. (Closing the host window —
   item #8 — is deferred; do not drive that path.)

### Discovery Handling

Per the scope rules: any new issue found during validation goes into
`status.md` as a Phase 3.4 follow-up bullet (one line each, with a
one-line repro). Exception: a one-line opcode-dispatch fix or missing-
reply fix may land within Phase 3.3 if it unblocks the validation
session itself.

### Build/Test Gate

Before each WM session:

```sh
cargo +nightly fmt
cargo clippy
cargo test --workspace
```

## Risks

- **SHAPE local-vs-host drift.** `QueryExtents`, `GetRectangles`, and
  `InputSelected` reply from the local `shape_windows` state, never
  from the host. If the resolved-rect mirror has a bug, protocol-
  visible queries can pass while host rendering is still wrong, making
  drift hard to detect in unit tests. Mitigation: add a manual
  diagnostic step that compares ynest-reported `GetRectangles` against
  a host-side `xtrace`/`xprop` capture for at least one e16 themed
  frame.
- **Synthetic Expose suppression timing** (Phase B of items #2/#4).
  The "host Expose seen within N ms" suppression is timing-based; if
  N is too short, double-paint storms; if too long, occlusion-
  recovery cases miss the synthesis. Mitigation: start with N = 50ms,
  bump if regressions appear during the four validation scenarios.
- **RENDER host-error attribution gap.** The audit relies on counts
  the nested side can observe; per-minor host-error counts are
  best-effort because `HostX11` doesn't have generic per-request error
  attribution. Triage may need to fall back on xtrace cross-reference
  for any divergence the audit alone can't explain.
- **Discovery scope creep.** Phase 3.3 explicitly defers new
  discoveries to 3.4. Trap: "this looks like a one-liner" snowballing
  into hours. Mitigation: hard rule that anything beyond a one-line
  dispatch/reply fix is bulleted in `status.md` for 3.4.
- **Root resize event ordering.** `ConfigureNotify` before
  `RRScreenChangeNotify` is a chosen ordering, not yet verified
  against Xorg. If a target client misbehaves, swap and document.
- **e16 RENDER audit may surface deep wire-shape bugs.** Triage step
  defers anything that isn't a one-line fix.

## Testing Strategy

### Unit Tests

- SHAPE: `host_x11::set_shape_rectangles` rect-list encoding, kind
  enum mapping, host-opcode-absent fallback. Plus a `Combine` test
  with a local-only source confirming the resolved-rect path emits the
  correct merged rectangle list to the host.
- XFIXES: `ChangeCursorByName` request parser (cursor xid + name
  extraction with padding); host-XFIXES-absent fallback.
- Root resize: `handle_host_container_resize` post-conditions
  (`RandrState` dims, `ROOT_WINDOW` geometry, `GetGeometry(root)`
  reply); `ConfigureNotify` on root wire shape; non-RANDR
  `StructureNotify`-only client receives `ConfigureNotify` after
  resize.
- Synthetic Expose (Phase B only, if needed): off-screen-recovery
  geometry comparison; `VisibilityNotify` transition state machine
  (only synthesize on `FullyObscured → {Unobscured, PartiallyObscured}`);
  Expose-event wire shape.
- Any RENDER wire-shape fixes that fall out of the audit get a byte-
  exact encoder test against trace data. The `Composite` multi-glyph
  fix gets a test with two glyph runs confirming both deltas are
  patched.

### Manual Validation

Per the per-WM scenario above, executed for wmaker, e16, fvwm3, and
Openbox.

### Build Gate

```sh
cargo +nightly fmt
cargo clippy
cargo test --workspace
```

## Done Criteria

Phase 3.3 is done when:

1. Items #1, #3, #5, and #6 land with their deliverables (code +
   tests + `status.md` update) and are verified under at least one WM.
2. Items #2 and #4 are either marked resolved by re-verification, or
   have the synthetic-Expose path landed and verified.
3. Openbox starts, runs the per-WM scenario, and exits cleanly. Any
   new issues are filed as Phase 3.4 bullets in `status.md`, not
   fixed.
4. `gtk3-demo`, `xeyes`, `xclock`, `xterm`, fvwm3 startup, wmaker, and
   e16 still pass their existing scenarios.
5. `status.md` reflects the final state of every item — resolved
   items moved out of the Phase 3.3 punch list with a brief note,
   deferred discoveries (including #7 and #8) explicitly listed under
   Phase 3.4.
