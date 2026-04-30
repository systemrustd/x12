# Phase 3.2 - Advanced Interoperability Design

## Goal

Move from "GTK3 works" to "modern toolkit startup paths work" under `ynest`
with a real window manager. The target clients are Qt 5/6 widgets, SDL2, GLFW,
and Electron/Chromium in software-rendered or non-accelerated modes.

Success means these clients can create windows, render basic UI, receive input,
resize cleanly, and exit without hangs. This phase is still about nested X11
compatibility, not accelerated GL/Vulkan or standalone KMS.

## Validation Targets

Primary validation should use locally available applications, in this order:

- Qt: `qt5ct`, `qt6ct`, `qterminal`, or another simple Qt Widgets app.
- SDL2: a minimal SDL2 X11 test window or installed SDL2 demo.
- GLFW: a minimal X11 GLFW window test with keyboard/mouse callbacks.
- Electron/Chromium: a small Electron app or Chromium launched with GPU
  acceleration disabled.

Run at least one target under `ynest + fvwm3` and one directly under `ynest`
where that makes sense. Existing regressions must continue to pass:
`gtk3-demo`, `xeyes`, `xclock`, `xterm`, and fvwm3 startup.

## Scope

Phase 3.2 adds coherent compatibility subsets for:

- RENDER completion needed by Qt/Cairo/Xft/pango paths.
- XFIXES for selection, cursor, and region-related probes.
- SHAPE for non-rectangular windows and toolkit capability checks.
- SYNC for counter/alarm probes and basic fence-free synchronization paths.
- DAMAGE for repaint tracking probes and compositor/toolkit fallbacks.
- COMPOSITE for overlay-window queries and compositor capability checks.
- PRESENT for vblank/media-stream counter queries used by SDL/GLFW/Chromium.
- RANDR resize events, because real toolkit windows respond badly to stale
  screen state after the host `ynest` window is resized.

Phase 3.2 should prefer small, correct subsets over large stubs. Do not
advertise an extension until every advertised request that can block a client
has a correct wire reply or deliberate protocol error.

## Non-Goals

- GLX, EGL, Vulkan, DRI3, PRIME, DMA-BUF, or direct rendering. These are Phase 4.
- MIT-SHM data paths. Most clients can fall back to socket PutImage/RENDER.
- Full external compositor support.
- Full region algebra for every possible XFIXES/SHAPE use case.
- Full XKB event forwarding beyond what Phase 3.1 already needs.
- Big-endian clients.

## Extension Policy

### Advertisement

Each extension gets a central metadata entry with:

- name
- major opcode
- first event
- first error
- request dispatcher

`QueryExtension` and `ListExtensions` must use the same registry. Add a startup
assertion or unit test that major opcodes and non-zero event/error bases are
unique.

### Reply Discipline

Every reply-producing request must send exactly the number of bytes advertised
by the reply length field. This was a real source of GTK3/XCB failures in Phase
3.1, so every new extension reply encoder should have a wire-shape test.

### Error Discipline

Unknown extension requests should not silently succeed if the real protocol
would return an extension-specific error for invalid resources. For this phase:

- Bad request minor: return a protocol error when the client expects a reply,
  unless a no-op is known to be harmless.
- Unknown extension resource: return the extension's resource error where
  available, otherwise `BadValue`.
- Unsupported mutation with valid resources: prefer `BadImplementation` only if
  the client can recover; otherwise provide a minimal no-op success path.

## RENDER Completion

### Purpose

Qt, Chromium, Cairo, and Xft use more of RENDER than fvwm3 did. Phase 3.1
already forwards many RENDER operations to the host and maps picture/glyphset
XIDs. Phase 3.2 should close the known gaps that can affect clipping,
gradients, glyph rendering, and fallback compositing.

### Required Work

- Keep forwarding these Phase 3.1 paths:
  `QueryVersion`, `QueryPictFormats`, `CreatePicture`, `ChangePicture`,
  `SetPictureClipRectangles`, `FreePicture`, `Composite`, glyphset operations,
  `FillRectangles`, `CreateCursor`, solid/linear/radial gradients,
  `SetPictureTransform`, and `SetPictureFilter`.
- Implement or deliberately answer `QueryPictIndexValues` (minor 2).
  Minimal valid reply is acceptable for DirectColor-like formats if no client
  needs real index values.
- Implement `ReferenceGlyphSet` (minor 18) as an alias/refcount to an existing
  glyphset and host glyphset.
- Implement `FreeGlyphs` (minor 22) forwarding.
- Implement `QueryFilters` (minor 29) with a valid empty filter/alias reply if
  host proxying is not needed.
- Accept `CreateAnimCursor` (minor 31) as a void no-op unless a validation
  target needs animated cursors.
- Implement `AddTraps` (minor 32) forwarding if clients issue it; otherwise
  return a non-blocking no-op only after confirming callers tolerate it.
- Keep gradient minor numbers exact: `CreateSolidFill` is 33,
  `CreateLinearGradient` is 34, `CreateRadialGradient` is 35, and
  `CreateConicalGradient` is 36.
- Preserve picture clip state locally enough that coordinate-offset correction
  remains correct across `ChangePicture`, `SetPictureClipRectangles`, and
  `Composite`.

### RENDER Minor Reference

Use the X11 RENDER protocol/header numbering exactly:

| Request | Minor | Phase 3.2 behavior |
|---|---:|---|
| QueryPictIndexValues | 2 | Valid minimal reply or host proxy |
| Trapezoids | 10 | Existing host-forwarded path |
| Triangles | 11 | Deliberate omission unless observed |
| TriStrip | 12 | Deliberate omission unless observed |
| TriFan | 13 | Deliberate omission unless observed |
| ReferenceGlyphSet | 18 | Alias/refcount to existing glyphset |
| FreeGlyphs | 22 | Forward to host |
| QueryFilters | 29 | Valid empty reply or host proxy |
| CreateAnimCursor | 31 | Void no-op initially |
| AddTraps | 32 | Forward or explicit tolerated no-op |
| CreateSolidFill | 33 | Existing host-forwarded path |
| CreateLinearGradient | 34 | Existing host-forwarded path |
| CreateRadialGradient | 35 | Existing host-forwarded path |
| CreateConicalGradient | 36 | Deliberate omission unless observed |

### Correctness Notes

RENDER object IDs live in client namespace but host requests need host IDs.
Every request body containing picture, glyphset, pixmap, or window IDs must be
translated before forwarding.

Coordinates targeting a nested top-level's host subwindow must be adjusted by
the stored picture/window offset. Keep this invariant in tests: the same
logical client draw location should hit the same host pixels whether the target
is a window picture or pixmap picture later composited into a window.

## XFIXES

### Purpose

Modern toolkits query XFIXES early for selection notifications, cursor support,
and region operations. Chromium/Electron commonly uses XFIXES for selection and
cursor paths. Some compositors and WMs also probe it.

### Registration

Advertise XFIXES only after `QueryVersion` and the selected subset below are
implemented.

Suggested nested metadata:

- name: `XFIXES`
- major opcode: next free extension opcode after Phase 3.1/RENDER
- first_event: next free event base
- first_error: next free error base

### Required Requests

- `QueryVersion`: reply with at least version 5.0 if implementing cursor and
  region requests; otherwise advertise the highest actually supported version.
- `SelectSelectionInput`: store per-client selection event masks.
- `SelectCursorInput`: accept and store a mask for root/window cursor events.
- `GetCursorImage`: return current cursor metadata and pixels if available,
  or a valid empty/default cursor image.
- `HideCursor` and `ShowCursor` (minors 28/29): accept as void no-ops unless
  cursor visibility becomes observable in validation.
- Region lifecycle: `CreateRegion`, `CreateRegionFromBitmap`,
  `CreateRegionFromWindow`, `DestroyRegion`, `SetRegion`, `CopyRegion`,
  `UnionRegion`, `IntersectRegion`, `SubtractRegion`, `InvertRegion`,
  `TranslateRegion`, `RegionExtents`, `FetchRegion`.
- Optional but useful: `SetWindowShapeRegion` and `SetPictureClipRegion` if
  SHAPE/RENDER integration needs region IDs.

Deliberate omission unless observed: `CreateRegionFromGC` (minor 5). If a
client issues it and continues after a no-op/full-region answer, document that
behavior; otherwise implement it from the GC clip state.

### Events

Selection events should be emitted when selection ownership changes if any
client selected `SetSelectionOwnerNotify`, `SelectionWindowDestroyNotify`, or
`SelectionClientCloseNotify`.

Cursor notify events can be deferred unless a validation target blocks waiting
for them. If advertised masks are accepted but no events are delivered, record
that limitation in `status.md`.

### Region Model

Use a simple rectangle-list region representation. It does not need to be
optimal, but it must be deterministic and bounded:

- Normalize empty regions.
- Coalesce identical/adjacent rectangles only if simple.
- Clip arithmetic to signed 16-bit coordinate ranges where protocol fields
  require it.
- Put a practical cap on rectangle count to avoid hostile clients causing
  unbounded memory use.

## SHAPE

### Purpose

SHAPE is used by older WMs, toolkits, splash screens, tray icons, and
non-rectangular windows. Qt and Chromium may probe it even if they do not
depend on it for normal windows.

### Required Requests

- `QueryVersion`: reply with 1.1.
- `Rectangles`: apply or store bounding/input/clip rectangles.
- `Mask`: apply bitmap-derived shape if the source bitmap is locally known;
  otherwise degrade to full rectangle.
- `Combine`: combine source and destination shapes.
- `Offset`: translate stored shape.
- `QueryExtents`: reply with stored bounding/clip extents.
- `SelectInput`: store shape-event mask.
- `InputSelected`: reply from stored mask.
- `GetRectangles`: return stored rectangles.

### Backend Behavior

In nested mode, shape should be forwarded to the host when possible using the
host SHAPE extension. If host SHAPE is absent, keep local state and use it for
hit testing/input clipping where feasible, but do not advertise SHAPE until
the wire-visible request subset is coherent.

Input shape should affect pointer target selection once implemented. Bounding
shape may initially only affect reported extents and host forwarding.

## SYNC

### Purpose

Qt, Chromium, and compositors probe SYNC. Some clients use counters/alarms for
frame pacing or WM protocols. A minimal counter/alarm implementation avoids
startup hangs while keeping real fence work out of Phase 3.2.

### Required Requests

- `Initialize`: reply with supported version.
- `ListSystemCounters`: return at least an empty list, or one monotonic
  `SERVERTIME` counter if easy.
- `CreateCounter`, `DestroyCounter`, `QueryCounter`, `SetCounter`,
  `ChangeCounter`: maintain signed 64-bit counter values.
- `CreateAlarm`, `ChangeAlarm`, `DestroyAlarm`, `QueryAlarm`: maintain alarm
  state enough to reply correctly.
- `Await`: do not block the server thread indefinitely. Either immediately
  satisfy already-true waits or return a recoverable error/no-op for unsupported
  waits.

### Events

AlarmNotify can be deferred unless a validation target requires it. If alarms
are stored but never fired, document the limitation clearly.

## DAMAGE

### Purpose

DAMAGE is primarily for compositors and repaint tracking. Some clients probe it
and some WMs/compositors require basic damage objects.

### Required Requests

- `QueryVersion`: reply with 1.1.
- `Create`: create a damage object associated with a drawable.
- `Destroy`: destroy it.
- `Subtract`: clear or move accumulated damage into repair/parts regions.
- `Add`: manually add damage to a drawable.

### Events

When host-routed drawing mutates a drawable with registered damage objects,
record the damaged rectangle. Deliver DamageNotify to selected clients if a
validation target needs it. For the first cut, recording plus correct
`Subtract` semantics is more important than perfect event timing.

## COMPOSITE

### Purpose

COMPOSITE is queried by Chromium/Electron and WMs/compositors. This phase does
not need to run a real external compositor, but it should provide enough
protocol behavior for capability checks and simple overlay-window queries.

### Required Requests

- `QueryVersion`: reply with version 0.4. Chromium expects at least 0.4 for
  `GetOverlayWindow`; advertising an older version while implementing overlay
  requests is internally inconsistent.
- `GetOverlayWindow`: return a stable server-created overlay window ID for the
  root, or the root itself only if clients tolerate it. Prefer a real hidden
  resource entry to avoid ID confusion.
- `ReleaseOverlayWindow`: decrement/release overlay use.
- `RedirectWindow`, `RedirectSubwindows`, `UnredirectWindow`,
  `UnredirectSubwindows`: accept and store redirect mode, but do not change
  rendering semantics in Phase 3.2 unless required by validation.
- `NameWindowPixmap`: if implemented, return a pixmap representing current
  window contents. If not implemented, do not advertise a COMPOSITE version
  that encourages clients to depend on it.

### Non-Goal

Do not implement full off-screen redirected windows or compositor ownership in
Phase 3.2. That belongs with a real compositor architecture.

## PRESENT

### Purpose

SDL2, GLFW, and Chromium often query PRESENT for media stream counters,
capabilities, and vblank-style presentation. Phase 3.2 should prevent startup
failures while leaving real frame scheduling to Phase 4.

### Required Requests

- `QueryVersion`: reply with a conservative version such as 1.0.
- `QueryCapabilities`: report no special capabilities.
- `SelectInput`: store event masks.
- `Pixmap`: accept pixmap presentation and copy/composite it immediately to the
  target window in nested mode, or return a recoverable error if the pixmap is
  unsupported.
- `NotifyMSC`: do not block; reply/event with current monotonic counters if
  needed by clients.

`PresentPixmap` carries a fence XID. Phase 3.2 supports the no-sync path:
`fence = 0` is accepted. If `fence != 0`, return `BadImplementation` rather
than crashing or treating an unknown fence as a real synchronization object.

### Events and Counters

Maintain per-window monotonically increasing serial/msc counters sufficient for
well-formed CompleteNotify/IdleNotify events if clients select them. Precise
vblank timing is not required in Phase 3.2.

## RANDR Resize Follow-Up

Phase 2 has a RANDR stub but `RRSelectInput` is not stored and host-window
resize does not update RANDR state. Phase 3.2 should make resize visible to
modern toolkits.

Required behavior:

- Store `RRSelectInput` masks per client/window.
- Detect host container resize in `ynest`.
- Update the single-output `RandrState` mode, CRTC, monitor, and screen size.
- Emit `RRScreenChangeNotify` and relevant output/CRTC events to subscribers.
- Ensure `GetGeometry(root)` and RANDR resources agree after resize.
- Keep these modern query paths implemented and coherent:
  `RRGetScreenResourcesCurrent` (minor 25), `RRGetMonitors` (minor 42), and
  `RRGetOutputProperty` (minor 15, safe empty reply for EDID/backlight
  properties).

## XKB Follow-Ups

Phase 3.1 proxies XKB requests. Phase 3.2 should add only the follow-ups that
validation targets actually need:

- Treat all reply-producing XKB minors correctly. Missing a reply causes Xlib
  hangs. Known reply-producing minors are:
  `UseExtension` (0), `GetState` (4), `GetMap` (8), `GetCompatMap` (10),
  `GetIndicatorState` (14), `GetNames` (17), `GetControls` (20),
  `GetPerClientFlags` (21), `GetDeviceInfo` (24), and
  `SetDebuggingFlags` (101).
- Store `SelectEvents` masks per client.
- Forward or synthesize `XkbStateNotify` when keyboard layout/modifier state
  changes if Qt/Electron requires it.
- Avoid leaking host atom IDs into local atom paths where clients later send
  them back to ynest.

## XI2 Follow-Ups

Phase 3.1 delivers basic GenericEvents. Phase 3.2 should add:

- Scroll valuators for mouse wheel/smooth scrolling if Qt/Chromium use XI2
  valuator state rather than core buttons 4/5.
- `XIQueryPointer` and `XIChangeCursor` should remain wire-correct.
- Raw events and hierarchy events only if validation targets request them.
- Tests for event masks with device IDs 0, 1, 2, and 3.

## Resource Tracking

Add explicit resource tables for extension objects:

- XFIXES regions.
- SYNC counters and alarms.
- DAMAGE objects.
- COMPOSITE overlay/pixmap names.
- PRESENT per-window event selections and counters.

Each resource must be cleaned up on:

- explicit destroy/free request;
- owning client disconnect;
- associated drawable/window destruction where the protocol requires it.

Avoid reusing core XID tracking shortcuts that make extension resources
ambiguous. Toolkit bugs are hard to diagnose when `BadIDChoice`, `BadMatch`,
and silent no-op behavior differ from Xorg.

## Testing Strategy

### Unit Tests

Every new extension must have tests for:

- `QueryExtension`/`ListExtensions` metadata.
- At least one request parser per non-trivial request body.
- Reply wire shape: exact total length and reply length field.
- Resource lifecycle cleanup on destroy/disconnect where local state exists.
- Event wire shape for every delivered extension event.

### Integration/Manual Tests

Run:

```sh
RUSTC_WRAPPER= cargo +nightly fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
RUSTC_WRAPPER= cargo clippy --workspace
```

Manual validation:

```sh
RUST_LOG=debug cargo run --release --bin ynest 99
DISPLAY=:99 fvwm3
DISPLAY=:99 gtk3-demo
DISPLAY=:99 qt5ct        # or another Qt Widgets app
DISPLAY=:99 xterm
DISPLAY=:99 xeyes
```

Add SDL/GLFW/Electron commands to the implementation plan based on what is
installed locally or what small test programs are added to the repo.

## Risks

- Advertising too much too early can make clients choose paths that are worse
  than absence fallback. Each extension should land behind a coherent subset.
- XID translation in RENDER/COMPOSITE/PRESENT is easy to get subtly wrong.
- Event sequence numbers and reply lengths must be exact; Xlib/XCB hangs often
  look like application bugs but are usually wire-shape bugs.
- Host-proxying extension requests can deadlock if no-reply requests use a
  reply-waiting helper.
- Region and damage objects can grow without bounds if rectangle counts are not
  capped.

## Done Criteria

Phase 3.2 is done when:

- At least one Qt Widgets app runs interactively under `ynest + fvwm3`.
- At least one SDL2 or GLFW X11 window opens, receives input, redraws, resizes,
  and exits cleanly.
- Electron/Chromium either starts in a documented software mode or has a clear
  captured blocker assigned to Phase 4 acceleration work.
- `gtk3-demo`, `xeyes`, `xclock`, `xterm`, and fvwm3 still work.
- `status.md` lists every newly advertised extension, implemented request
  subset, validation commands, and known limitations.
