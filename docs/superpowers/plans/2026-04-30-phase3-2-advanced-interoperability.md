# Phase 3.2 Advanced Interoperability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Run at least one Qt Widgets app and one SDL2 or GLFW X11 app
interactively under `ynest` with fvwm3, while keeping `gtk3-demo`, `xeyes`,
`xclock`, `xterm`, and fvwm3 working.

**Spec:** [`docs/superpowers/specs/2026-04-30-phase3-2-advanced-interoperability-design.md`](../specs/2026-04-30-phase3-2-advanced-interoperability-design.md).

**Architecture:** Add small, coherent extension subsets. Do not advertise an
extension until its blocking request/reply paths have exact wire shapes.
Prefer local resource tracking for extension objects; proxy to the host only
where the existing host-backed drawable/picture model already makes that safe.

**Project conventions:**

```sh
RUSTC_WRAPPER= cargo +nightly fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
RUSTC_WRAPPER= cargo clippy --workspace
```

Manual validation is mandatory. Toolkit startup failures often come from exact
wire-shape or extension-advertisement mistakes that unit tests will not catch.

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `crates/yserver-core/src/nested.rs` | modify | Extension dispatch, registration, per-client event selection, host nested resize plumbing |
| `crates/yserver-core/src/server.rs` | modify | Extension resource ownership, cleanup, event fanout helpers |
| `crates/yserver-core/src/resources.rs` | modify | Shape/damage/drawable lifecycle integration if needed |
| `crates/yserver-core/src/host_x11.rs` | modify | Host RENDER/SHAPE forwarding and optional host extension helpers |
| `crates/yserver-core/src/randr.rs` | modify | Host resize updates, modern query stubs, event subscription |
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Core wire helpers, reply/event encoders, request parsers |
| `crates/yserver-protocol/src/x11/*.rs` | add/modify | Prefer separate modules for larger extension wire code |
| `docs/status.md` | modify | Track advertised extensions, validation results, known limitations |

The implementation should land as compile-safe commits:

1. **RENDER/RANDR safe replies** — close known blocking reply gaps and modern RANDR queries.
2. **Extension registry** — central metadata for optional Phase 3.2 extensions, initially disabled unless handlers are ready.
3. **XFIXES subset** — QueryVersion, selections, cursor image, regions, cursor hide/show no-ops.
4. **SHAPE subset** — shape state, query/select/input-selected/get-rectangles, host forwarding when available.
5. **SYNC subset** — counters, alarms, initialize/list/query paths, non-blocking await.
6. **DAMAGE subset** — damage objects, accumulated rectangles, add/subtract, optional events.
7. **COMPOSITE subset** — version 0.4, overlay window, redirect state, guarded NameWindowPixmap.
8. **PRESENT subset** — version/capabilities/select, no-sync PresentPixmap, counters/events.
9. **RANDR resize events** — host resize propagation, root geometry consistency, subscriber notify.
10. **Toolkit validation and docs** — run Qt/SDL/GLFW/Electron probes, tune, update status.

---

## Commit 1 - RENDER and RANDR Blocking Gaps

### Task 1.1: RENDER minor audit

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs`
- Modify: `docs/status.md`

- [x] **Step 1: Verify RENDER minor numbers against system headers.**

Confirm these mappings are represented in dispatch comments/tests:

```text
QueryPictIndexValues  = 2
Trapezoids            = 10
Triangles             = 11
TriStrip              = 12
TriFan                = 13
ReferenceGlyphSet     = 18
FreeGlyphs            = 22
QueryFilters          = 29
CreateAnimCursor      = 31
AddTraps              = 32
CreateSolidFill       = 33
CreateLinearGradient  = 34
CreateRadialGradient  = 35
CreateConicalGradient = 36
```

- [x] **Step 2: Add `QueryFilters` reply.**

Write a failing wire-shape test first, then implement a valid empty reply for
minor 29 unless host proxying is trivial. Empty reply shape:

```text
byte  0:    reply type = 1
byte  1:    unused = 0
bytes 2-3:  sequence
bytes 4-7:  reply_length = 0
bytes 8-11: num_filters = 0
bytes 12-15: num_aliases = 0
bytes 16-31: padding
```

- [x] **Step 3: Implement `QueryPictIndexValues` minimal reply.**

Write a failing wire-shape test first, then return a valid empty reply when the
target pict format has no indexed values. Empty reply shape:

```text
byte  0:    reply type = 1
byte  1:    unused = 0
bytes 2-3:  sequence
bytes 4-7:  reply_length = 0
bytes 8-11: num_values = 0
bytes 12-31: padding
```

- [x] **Step 4: Implement `ReferenceGlyphSet` and `FreeGlyphs`.**

Write failing lifecycle/forwarding tests first.

`ReferenceGlyphSet` (minor 18) body is:

```text
bytes 0-3: new_glyphset
bytes 4-7: existing_glyphset
```

It creates a second client XID that aliases the same host glyphset XID. Track
reference counts or alias ownership so the host glyphset is freed only after
all client aliases are freed.

`FreeGlyphs` (minor 22) body is:

```text
bytes 0-3: glyphset
bytes 4..: N * glyph_id CARD32
```

It removes individual glyphs and does not destroy the glyphset. Translate only
the glyphset XID to the host glyphset XID and forward the full request body to
the host.

- [x] **Step 5: Keep no-op geometry paths explicit.**

Leave `CreateAnimCursor`, `AddTraps`, and `CreateConicalGradient` as explicit
logged no-ops unless validation proves forwarding is needed.

### Task 1.2: Modern RANDR query coverage

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/randr.rs`
- Modify: `crates/yserver-protocol/src/x11/randr.rs`

- [x] **Step 1: Confirm `RRGetScreenResourcesCurrent` minor 25 exists and aliases current state.**

Write or keep a regression test that minor 25 returns the same single-output
topology as `RRGetScreenResources`.

- [x] **Step 2: Confirm `RRGetMonitors` minor 42 exists and returns `ynest-0`.**

Qt uses this path. Add a reply-shape test for one primary monitor covering the
current root dimensions.

- [x] **Step 3: Add `RRGetOutputProperty` minor 15.**

Write a failing empty-reply shape test first. Return a valid empty property
reply for EDID/backlight and unknown properties. Do not block, and do not
return malformed lengths.

- [x] **Step 4: Run validation checks.**

Run all project checks before continuing.

---

## Commit 2 - Extension Registry for Phase 3.2

### Task 2.1: Centralize extension metadata

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs` if constants belong there

- [x] **Step 1: Extract advertised extension metadata if still hardcoded.**

`QueryExtension`, `ListExtensions`, and extension dispatch should consume the
same metadata source.

- [x] **Step 2: Allocate Phase 3.2 extension opcodes and bases.**

Add metadata records for XFIXES, SHAPE, SYNC, DAMAGE, COMPOSITE, and PRESENT.
Keep `advertised=false` until each extension subset lands.

- [x] **Step 3: Add uniqueness tests.**

Assert unique major opcodes and non-zero first event/error bases across RANDR,
RENDER, BIG-REQUESTS, XKEYBOARD, XI2, and every enabled Phase 3.2 extension.

- [x] **Step 4: Add unknown-minor handling policy.**

Make it obvious per extension whether unsupported minors are no-op, error, or
unadvertised. Avoid silent success for reply-producing requests.

---

## Commit 3 - XFIXES Minimal Subset

### Task 3.1: Wire and state

**Files:**
- Add/modify: `crates/yserver-protocol/src/x11/xfixes.rs`
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [x] **Step 1: Add XFIXES request parsers and reply encoders.**

Cover `QueryVersion`, `GetCursorImage`, region replies, and event encoders for
selection notify if delivered.

- [x] **Step 2: Write resource lifecycle tests.**

Before implementation, add failing tests for create/destroy cleanup, duplicate
resource IDs, unknown resource lookup, and client-disconnect cleanup.

- [x] **Step 3: Add extension resources.**

Track XFIXES region resources by client-owned XID. Use rectangle lists with a
practical rectangle-count cap.

- [x] **Step 4: Add selection/cursor event mask storage.**

Store `SelectSelectionInput` and `SelectCursorInput` masks per client/window or
client/selection as the protocol requires.

### Task 3.2: Region operations

- [x] **Step 1: Write region operation tests.**

Add failing tests for empty region normalization, rectangle cap behavior,
copy/translate/extents, and fetch-region reply length.

- [x] **Step 2: Implement region lifecycle.**

Handle `CreateRegion`, `CreateRegionFromBitmap`, `CreateRegionFromWindow`,
`DestroyRegion`, `SetRegion`, `CopyRegion`, `TranslateRegion`, and
`RegionExtents`.

`CreateRegionFromWindow` uses the target window's current bounding geometry.
If SHAPE is not live yet, use the full window rectangle as the fallback shape.

- [x] **Step 3: Implement simple region algebra.**

Implement union/intersect/subtract/invert correctly enough for rectangle-list
regions. Prefer deterministic simple algorithms over complex coalescing.

- [x] **Step 4: Implement `FetchRegion`.**

Return rectangles with exact reply length. Add shape tests for empty and
multi-rect regions.

### Task 3.3: Cursor and selection behavior

- [x] **Step 1: Write cursor/selection tests.**

Add failing tests for `GetCursorImage` reply length, hide/show no-op dispatch,
and selection mask storage.

- [x] **Step 2: Implement `GetCursorImage`.**

Return the current cursor if available, otherwise a valid default/empty image.

- [x] **Step 3: Stub `HideCursor` and `ShowCursor`.**

Accept minors 28/29 as void no-ops with debug logs.

- [x] **Step 4: Emit selection notify if masks are selected.**

Hook existing selection ownership changes into XFIXES selection notifications
where straightforward. If deferred, document the limitation in `status.md`.

- [x] **Step 5: Advertise XFIXES.**

Only advertise once query/reply paths and region lifecycle tests pass.

---

## Commit 4 - SHAPE Minimal Subset

### Task 4.1: Shape resources and wire

**Files:**
- Add/modify: `crates/yserver-protocol/src/x11/shape.rs`
- Modify: `crates/yserver-core/src/resources.rs`
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/host_x11.rs` if host forwarding is used

- [x] **Step 1: Add SHAPE protocol encoders/parsers.**

Cover `QueryVersion`, `Rectangles`, `Mask`, `Combine`, `Offset`,
`QueryExtents`, `SelectInput`, `InputSelected`, and `GetRectangles`.

- [x] **Step 2: Write SHAPE state tests.**

Add failing tests for default window rectangle shape, rectangles/mask/combine
state changes, query extents, input-selected, and get-rectangles reply length.

- [x] **Step 3: Store per-window shapes.**

Track bounding, clip, and input shape as rectangle-list regions. Default shape
is the window rectangle.

- [x] **Step 4: Implement query/select replies.**

Add exact wire-shape tests for extents, input-selected, and get-rectangles.

### Task 4.2: Backend behavior

- [x] **Step 1: Forward SHAPE to host when available.**

Probe host SHAPE if needed. If host SHAPE is absent, keep local state and do
not rely on host visuals matching shape.

- [x] **Step 2: Apply input shape to pointer hit testing if practical.**

At minimum, document if input shape is only stored/reported but not used for
hit testing. Deferred for the first SHAPE cut: input shape is stored/reported
but not yet used in pointer target selection.

- [x] **Step 3: Advertise SHAPE.**

Advertise only after the selected request subset has tests and does not break
fvwm3/gtk3-demo.

---

## Commit 5 - SYNC Minimal Subset

### Task 5.1: Counter and alarm resources

**Files:**
- Add/modify: `crates/yserver-protocol/src/x11/sync.rs`
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [x] **Step 1: Add SYNC resource tables.**

Track counters and alarms by client-owned XID. Use signed 64-bit values.

- [x] **Step 2: Write SYNC counter tests.**

Add failing tests for counter create/query/set/change/destroy, duplicate IDs,
unknown IDs, and client-disconnect cleanup.

- [x] **Step 3: Implement basic requests.**

Handle `Initialize`, `ListSystemCounters`, `CreateCounter`, `DestroyCounter`,
`QueryCounter`, `SetCounter`, and `ChangeCounter`.

- [x] **Step 4: Write SYNC alarm tests.**

Add failing tests for alarm create/query/change/destroy state and exact reply
lengths.

- [x] **Step 5: Implement alarm requests.**

Handle `CreateAlarm`, `ChangeAlarm`, `DestroyAlarm`, and `QueryAlarm`.

- [x] **Step 6: Implement non-blocking `Await`.**

`Await` is a void request, so no reply is needed. The blocking risk is not Xlib
waiting for a reply; it is a client/WM waiting forever for AlarmNotify. For the
first cut, accept and discard unsupported waits, document that alarm events are
deferred, and revisit only if a validation target actually waits on an alarm.

- [x] **Step 7: Advertise SYNC.**

Advertise after reply wire tests pass. Document if AlarmNotify is not emitted.

---

## Commit 6 - DAMAGE Minimal Subset

### Task 6.1: Damage object lifecycle

**Files:**
- Add/modify: `crates/yserver-protocol/src/x11/damage.rs`
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/resources.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [x] **Step 1: Add DAMAGE wire helpers.**

Implement `QueryVersion`, `Create`, `Destroy`, `Subtract`, and `Add` parsing
and replies/events where needed.

- [x] **Step 2: Write DAMAGE lifecycle tests.**

Add failing tests for create/destroy, drawable cleanup, duplicate/unknown IDs,
and accumulated rectangle clearing.

- [x] **Step 3: Track damage objects.**

Associate damage XIDs with drawables. Clean them up when the owning client or
drawable dies.

- [x] **Step 4: Record damage from drawing paths.**

Hook host-routed drawing, CopyArea/CopyPlane, PutImage, ClearArea, and RENDER
operations to accumulate conservative damaged rectangles.

Deferred in the first DAMAGE cut. Explicit `DamageAdd` records conservative
damage; automatic accumulation from drawing paths is still pending.

- [x] **Step 5: Implement `Subtract`.**

Clear accumulated damage or move it into repair/parts XFIXES regions.

`Subtract` takes two optional XFIXES region XIDs: `repair` and `parts`. If
either is non-zero, look it up in the XFIXES region table from Commit 3 and
write the appropriate accumulated/remaining region. This is a hard
cross-extension dependency, not a no-op detail.

- [x] **Step 6: Advertise DAMAGE.**

Advertise after create/subtract/destroy paths pass tests. DamageNotify delivery
can be deferred if validation targets do not require it, but must be documented.

---

## Commit 7 - COMPOSITE 0.4 Subset

### Task 7.1: Overlay and redirect state

**Files:**
- Add/modify: `crates/yserver-protocol/src/x11/composite.rs`
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/resources.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [x] **Step 1: Implement `QueryVersion` as 0.4.**

Reply 0.4 explicitly. Add a wire-shape test.

- [x] **Step 2: Write COMPOSITE state tests.**

Add failing tests for QueryVersion wire shape, stable overlay window identity,
release behavior, redirect state storage, and guarded NameWindowPixmap errors.

- [x] **Step 3: Implement overlay window lifecycle.**

Return a stable server-created overlay window for `GetOverlayWindow`; track
release counts for `ReleaseOverlayWindow`.

- [x] **Step 4: Store redirect requests.**

Accept `RedirectWindow`, `RedirectSubwindows`, `UnredirectWindow`, and
`UnredirectSubwindows`. Store state, but do not change rendering semantics yet.

- [x] **Step 5: Decide `NameWindowPixmap` behavior.**

If current window contents can be represented by a host-backed pixmap, return
one. Otherwise return a safe protocol error and document the limitation.

First cut returns `BadMatch`; real named window pixmaps are deferred until the
window backing model can expose a stable pixmap.

- [x] **Step 6: Advertise COMPOSITE.**

Advertise only when 0.4 query, overlay, redirect, and guarded
NameWindowPixmap behavior are complete.

---

## Commit 8 - PRESENT No-Sync Subset

### Task 8.1: Present request path

**Files:**
- Add/modify: `crates/yserver-protocol/src/x11/present.rs`
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [x] **Step 1: Implement `QueryVersion` and `QueryCapabilities`.**

Reply with conservative 1.0 behavior and no special capabilities.

- [x] **Step 2: Add PresentPixmap parser tests.**

Use xcb-proto `present.xml` as the source of truth for the request body. The
request is 72+ bytes and includes window, pixmap, serial, valid/update regions,
x/y offsets, target CRTC, wait/idle fence XIDs, options, target MSC, divisor,
remainder, and optional notifies. Add tests for at least `fence=0`, `idle_fence=0`,
and a non-zero fence path.

- [x] **Step 3: Store `SelectInput` masks.**

Track per-client/window PRESENT event masks.

- [x] **Step 4: Implement no-sync `PresentPixmap`.**

Accept `fence=0`. Copy/composite the pixmap immediately to the target window
when both drawables are host-backed.

- [x] **Step 5: Handle `fence != 0`.**

Return `BadImplementation` rather than panicking or silently treating an unknown
fence as a real synchronization object.

- [x] **Step 6: Implement `NotifyMSC` as non-blocking.**

Use per-window monotonic counters. Emit events only if selected and needed by
validation.

- [x] **Step 7: Advertise PRESENT.**

Advertise after reply/event wire tests pass and SDL/GLFW does not regress.

---

## Commit 9 - RANDR Resize Events

### Task 9.1: Store subscriptions

**Files:**
- Modify: `crates/yserver-core/src/randr.rs`
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-protocol/src/x11/randr.rs`

- [x] **Step 1: Store `RRSelectInput` masks.**

Track masks per client/window. Clean up on client disconnect.

- [x] **Step 2: Encode RANDR screen/output/crtc notify events.**

Add exact wire-shape tests for every event delivered.

### Task 9.2: Host resize propagation

- [x] **Step 1: Detect host container resize.**

Wire host ConfigureNotify for the container window into server state.

- [x] **Step 2: Update root geometry and RANDR state together.**

Ensure `GetGeometry(root)`, RANDR screen resources, CRTC info, monitor info,
and root window dimensions agree after resize.

- [x] **Step 3: Emit subscriber notifications.**

Deliver `RRScreenChangeNotify` and relevant output/CRTC events to clients that
selected masks.

- [ ] **Step 4: Validate manual resize.**

Run a Qt app and resize the host `ynest` window. Confirm the app sees updated
screen dimensions and keeps rendering.

---

## Commit 10 - XKB/XI2 Follow-Ups and Validation

### Task 10.1: XKB reply-producing minor audit

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [x] **Step 1: Audit reply-producing XKB minors.**

Ensure the proxy waits for replies for minors:
0, 4, 8, 10, 14, 17, 20, 21, 24, and 101.

- [x] **Step 2: Ensure no-reply minors never use reply-waiting helpers.**

Keep `SelectEvents` and other void requests fire-and-forget or local-only.

- [x] **Step 3: Store XKB `SelectEvents` masks if not already stored.**

Forward or synthesize `XkbStateNotify` only if validation requires it.

### Task 10.2: XI2 scroll and mask polish

**Files:**
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [x] **Step 1: Add tests for XI2 device wildcard masks.**

Cover device IDs 0, 1, 2, and 3 for keyboard and pointer event delivery.

- [x] **Step 2: Investigate scroll valuators.**

If Qt/Chromium relies on XI2 valuators instead of core buttons 4/5, add
valuator data to XI2 motion/button events. Otherwise document deferral.

### Task 10.3: Manual validation

- [ ] **Step 1: Run baseline applications.**

```sh
RUST_LOG=debug cargo run --release --bin ynest 99
DISPLAY=:99 fvwm3
DISPLAY=:99 gtk3-demo
DISPLAY=:99 xeyes
DISPLAY=:99 xclock
DISPLAY=:99 xterm
```

- [ ] **Step 2: Run Qt validation.**

Use installed apps such as `qt5ct`, `qt6ct`, `qterminal`, or another simple Qt
Widgets app. Validate window creation, text, clicks, menus, resize, close.

- [ ] **Step 3: Run SDL2/GLFW validation.**

Use installed demos or add tiny test programs. Validate create, redraw, input,
resize, close.

- [ ] **Step 4: Run Electron/Chromium probe.**

Use software/no-GPU flags if necessary. If acceleration becomes the blocker,
record it as Phase 4 rather than stretching Phase 3.2.

- [ ] **Step 5: Update `docs/status.md`.**

List every newly advertised extension, implemented request subset, validation
commands, and known limitations.

- [ ] **Step 6: Final checks.**

Run formatting, check, tests, and clippy before the final commit.
