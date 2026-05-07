# Phase 6.10 — Multi-monitor on KMS

Goal: drive every connected DRM connector as an independent X11 RANDR
output, laid out side-by-side in a single virtual screen, so real
multi-head desktops work on the bare-metal `KmsBackend`. Validate end
to end under `vng` with `virtio-gpu-pci,max_outputs=2`, then on bare
metal where physical outputs are available.

## 1. Problem Statement

Phase 6.1's `discover_output` was deliberately scoped to a single
connector + CRTC + plane + mode. Phases 6.4 → 6.9 inherited that
shape: `KmsBackend` holds one `Output` and one `Swapchain`,
`composite_and_flip` paints into one scanout buffer, and
`yserver-core::randr::RandrState` hard-codes one output / one CRTC /
one mode behind module-level `OUTPUT_ID` / `CRTC_ID` / `MODE_ID`
constants.

A real X server has to:

- Enumerate every connected output and modeset each one independently.
- Lay them out in a single virtual screen coordinate space so window
  geometry, pointer coords, and damage all share one frame of
  reference (this is the X11 model — Xinerama / RANDR 1.2+ surface
  per-output rectangles within that shared root).
- Composite each output independently, painting only the windows that
  intersect that output's screen-space rectangle.
- Page-flip each CRTC independently, identifying which CRTC fired so
  the right swapchain advances.
- Report all of that through RANDR `RRGetScreenResources` /
  `RRGetMonitors` / `RRGetCrtcInfo` so WMs and panels can place
  themselves correctly.
- Hot-reconfigure when the layout changes (deferred to 6.10.x — the
  initial slice is "boot with N outputs, never reconfigure").

This phase delivers the static-layout case end to end, with hotplug,
mode switching at runtime, and multi-plane / overlay-plane usage
deferred.

## 2. Design

### 2.1 Per-output `Output` lifecycle

`drm/modeset.rs::discover_output` currently:

```rust
for &handle in resources.connectors() {
    let info = device.get_connector(handle, false)?;
    if info.state() == connector::State::Connected && !info.modes().is_empty() {
        connected = Some(info);
        break;        // ← drops every output after the first
    }
}
```

Becomes:

```rust
pub fn discover_outputs(device: &Device) -> io::Result<Vec<Output>> {
    let mut outs = Vec::new();
    for &handle in resources.connectors() {
        let info = device.get_connector(handle, false)?;
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        outs.push(build_output(device, &resources, &info, &claimed_crtcs)?);
        claimed_crtcs.insert(out.crtc);
    }
    if outs.is_empty() {
        return Err(io::Error::other("no connected outputs"));
    }
    Ok(outs)
}
```

`build_output` is `discover_output`'s existing per-connector body
(encoder → CRTC → primary plane → property handles), with one
addition: the CRTC search must skip CRTCs already claimed by an
earlier output. `claimed_crtcs: HashSet<crtc::Handle>` accumulates
each pick. Same logic for primary planes — `pick_primary_plane`
accepts an "exclude" set of already-claimed plane handles.

**CRTC assignment policy (scope-narrowed).** Greedy
"pick first unclaimed" is unsafe on real hardware: Intel/AMD
encoders share CRTC pools, and a greedy walk can paint the last
connector into a corner even though a valid full assignment
exists. Phase 6.10 is **explicitly scoped to virtio-gpu**, where
each connector has its own dedicated encoder + CRTC and greedy
matching always succeeds. If `build_output` cannot find an
unclaimed CRTC for a connected connector, bring-up **fails** with
a hard error — we do not silently skip an output, because that
would mean the user sees a black monitor without a clear
diagnostic. Real-hardware (full encoder/CRTC matching search) is
deferred to Phase 6.10.x with an explicit subsequent slice. The
implementation note `// TODO(phase-6.10.x): replace greedy with
full encoder→CRTC bipartite match for non-virtio-gpu hardware`
goes on `discover_outputs`.

**Partial-modeset rollback.** With multiple outputs, a
`commit_modeset` failure on output N after outputs `0..N-1` have
already been committed leaves the card half-programmed. The
caller (in `KmsBackend::open`) wraps the loop in a rollback:

```rust
let mut committed: Vec<&Output> = Vec::new();
for output in &outputs {
    if let Err(err) = drm::modeset::commit_modeset(&device, output, ...) {
        for done in committed.iter().rev() {
            let _ = drm::modeset::disable_output(&device, done);
        }
        return Err(err);
    }
    committed.push(output);
}
```

Failures during disable_output in the rollback path are logged
but not propagated — we already have a fatal error to surface,
and the kernel's master-release on Drop will clear residual
state. `disable_output` is otherwise unchanged from today.

### 2.2 Virtual-screen layout

Outputs live at `(x, y)` positions inside a single screen-space
rectangle. The minimum viable policy:

```rust
pub struct OutputLayout {
    pub output:    drm::modeset::Output,
    pub swapchain: drm::Swapchain,
    pub x:         i32,        // top-left in virtual screen coords
    pub y:         i32,
    pub width:     u16,        // == output.picked.width
    pub height:    u16,
}
```

Default placement: connectors enumerated in the order
`discover_outputs` returns them, laid out left-to-right at `y=0`,
each starting at the previous output's right edge. So two
1920×1080 monitors land at `(0,0)..(1920,1080)` and
`(1920,0)..(3840,1080)`.

`YSERVER_LAYOUT` env var override (deferred — first cut is
horizontal-default):

```
YSERVER_LAYOUT="HDMI-A-1:0,0,1920x1080;DP-1:1920,0,2560x1440"
```

The virtual-screen size is `(max(x+width), max(y+height))` over all
outputs. This is what gets reported as `screen_width`/`screen_height`
through RANDR and as the root window's dimensions in the X11 setup
reply.

### 2.3 `KmsBackend` field changes

Today (`kms/backend.rs:500-503`):

```rust
pub struct KmsBackend {
    output:    drm::modeset::Output,
    swapchain: drm::Swapchain,
    fb_w:      u16,
    fb_h:      u16,
    ...
}
```

After:

```rust
pub struct KmsBackend {
    outputs:   Vec<OutputLayout>,
    /// Virtual-screen extent: max-x, max-y across all outputs.
    fb_w:      u16,
    fb_h:      u16,
    ...
}
```

`fb_dimensions()` returns the virtual-screen extent, unchanged from
the caller's perspective. `lib.rs:38` (`backend.fb_dimensions()` →
`ServerState::with_geometry`) keeps working.

### 2.4 Per-CRTC page-flip identification

`drm/page_flip.rs:42-48` is the load-bearing fix:

```rust
pub fn drain_events<F: FnMut()>(device: &Device, mut on_page_flip: F) -> io::Result<()> {
    for event in device.receive_events()? {
        match event {
            Event::PageFlip(_) => on_page_flip(),
            ...
        }
    }
}
```

The `_` discards a `PageFlipEvent` whose `crtc` field is exactly the
identifier we need. Becomes:

```rust
pub fn drain_events<F: FnMut(crtc::Handle)>(
    device: &Device,
    mut on_page_flip: F,
) -> io::Result<()> {
    for event in device.receive_events()? {
        if let Event::PageFlip(ev) = event {
            on_page_flip(ev.crtc);
        }
    }
}
```

`drain_page_flips_and_composite` (`backend.rs:2090`) becomes:

```rust
pub fn drain_page_flips_and_composite(&mut self) -> io::Result<()> {
    let mut flipped: Vec<crtc::Handle> = Vec::new();
    drm::page_flip::drain_events(&self.device, |crtc| flipped.push(crtc))?;

    for crtc in flipped {
        let Some(layout) = self.outputs.iter_mut().find(|o| o.output.crtc == crtc) else {
            continue;        // unknown CRTC — log and skip
        };
        if let Some(idx) = layout.swapchain.submitted_idx() {
            layout.swapchain.complete(idx)
                .map_err(|e| io::Error::other(format!("swapchain.complete: {e}")))?;
        }
    }

    self.composite_and_flip()?;
    Ok(())
}
```

The `submitted_idx()` invariant (at most one buffer Submitted at a
time) is now per-swapchain rather than global, so it survives
multi-output unchanged — each `OutputLayout` owns its own
`Swapchain`.

### 2.5 Per-output composite + flip

`composite_and_flip` (`backend.rs:1664-1749`) currently allocates
one screen-sized scanout image, paints all top-levels into it, and
flips. Becomes a per-output loop:

```rust
pub fn composite_and_flip(&mut self) -> io::Result<()> {
    // Snapshot stacking order once; all outputs see the same windows.
    let top_levels: Vec<u32> = self.top_level_order.clone();

    // Pre-filter: which top-levels intersect each output? Avoid
    // descending whole off-screen subtrees per-output. With six 4K
    // monitors and 50 windows, "trust pixman dst-clip" walks ~300
    // composite chains per frame; the intersection check collapses
    // most of them to a no-op before we touch pixman at all.
    let visible_per_output: Vec<Vec<u32>> = self.outputs.iter()
        .map(|layout| top_levels.iter()
            .copied()
            .filter(|&id| self.window_intersects(id, layout.rect()))
            .collect())
        .collect();

    for (layout, visible) in self.outputs.iter_mut().zip(&visible_per_output) {
        let Some(buf_idx) = layout.swapchain.acquire_idx() else { continue };
        let buf = layout.swapchain.buffer_mut(buf_idx);

        let mut scanout = unsafe {
            PixmanImage::from_buffer(
                FormatCode::X8R8G8B8,
                buf.width(), buf.height(),
                buf.pixels_mut().as_mut_ptr(),
                buf.stride() as usize,
                false,
            )?
        };

        // Paint root background, bg pixmap, top-levels, cursor — all
        // existing paths, but with origin translated by (-layout.x, -layout.y)
        // so a window at virtual-screen (3000, 100) lands at (1080, 100)
        // on the right-hand 1920-wide output. Only the pre-filtered
        // visible top-levels are walked.
        self.paint_output(&mut scanout, layout, visible);

        drop(scanout);
        let fb_id = layout.swapchain.buffer(buf_idx).fb_id();
        drm::page_flip::submit_flip(&self.device, &layout.output, fb_id)?;
        layout.swapchain.submit(buf_idx)
            .map_err(|e| io::Error::other(format!("swapchain.submit: {e}")))?;
    }
    Ok(())
}
```

`paint_output` is a refactor of the existing body (root bg, optional
`bg_pixmap`, `composite_window_into` per top-level, `draw_cursor_onto`)
that takes an explicit `OutputLayout` and translates every paint by
`(-layout.x, -layout.y)`. Pixman composite ops already accept an
`(x, y)` destination — the translation is a single offset per
composite call. `composite_window_into` is naturally clipped by the
scanout image's `(width, height)`, so windows fully outside this
output's rectangle just produce no-op composite calls. Windows
straddling two outputs paint into both (each call is independent —
this is exactly Xinerama / RANDR 1.2+ behavior).

Cursor draw: `draw_cursor_onto` already takes pixman destination
coords; it just gets called per output with `(cursor_x - layout.x,
cursor_y - layout.y)`.

### 2.6 RANDR multi-output

`yserver-core/src/randr.rs` is the most invasive change but
mechanically simple — the protocol-side encoders in
`yserver-protocol/src/x11/randr.rs:316-540` already iterate over
`Vec`s of CRTCs / outputs / modes. They've been multi-output-shaped
since Phase 3.2. Only the state struct + reply builders are
single-output.

#### 2.6.1 `RandrState` shape

Today (one record):

```rust
pub const OUTPUT_ID: u32 = 1;
pub const CRTC_ID: u32 = 2;
pub const MODE_ID: u32 = 3;

pub struct RandrState {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub screen_width: u16,
    pub screen_height: u16,
    pub width_mm: u32,
    pub height_mm: u32,
}
```

After:

```rust
pub struct RandrOutput {
    pub output_id: u32,           // unique per-output
    pub crtc_id:   u32,           // unique per-CRTC
    pub mode_id:   u32,           // unique per-mode (may be shared across outputs)
    pub name:      String,        // connector name, e.g. "HDMI-A-1"
    pub x:         i16,
    pub y:         i16,
    pub width:     u16,
    pub height:    u16,
    pub width_mm:  u32,
    pub height_mm: u32,
    pub vrefresh:  u32,
}

pub struct RandrState {
    pub timestamp:        u32,
    pub config_timestamp: u32,
    pub screen_width:     u16,
    pub screen_height:    u16,
    pub screen_width_mm:  u32,        // sum of horizontal mm across outputs
    pub screen_height_mm: u32,        // max of per-output mm
    pub primary_output:   u32,        // output_id of the primary; 0 if none
    pub outputs:          Vec<RandrOutput>,
    pub modes:            Vec<RandrMode>,  // deduped — see ID allocation
}
```

`OUTPUT_ID` / `CRTC_ID` / `MODE_ID` consts go away. ID allocation:

- Outputs get `1..=N`.
- CRTCs get `(N+1)..=2N`.
- **Modes are deduped** by `(width, height, vrefresh)`. Two outputs
  running the same resolution + refresh share one `RandrMode`
  record and one mode ID. Each `RandrOutput.mode_id` references
  the deduped record. Codex called this out: clients comparing
  mode IDs by equality (e.g. "are both monitors running the same
  mode?") get the right answer for free, and the cost is one
  `HashMap<(w,h,vrefresh), u32>` during construction.

**Screen physical dimensions.** RRGetScreenSizeRange and the X11
setup reply expose root `width_mm` / `height_mm`. With multiple
outputs in a horizontal layout, the convention used by Xorg's
RANDR implementation and by Wayland/wlroots is:
`screen_width_mm = Σ output.width_mm`, `screen_height_mm =
max(output.height_mm)`. This is the same as the screen pixel
extent under `(x, y)` placement. Vertical / mixed layouts swap
the role accordingly — for the horizontal-only first cut, sum-w
and max-h is correct.

**Primary output.** Phase 6.10 designates index 0 (first
enumerated connector) as primary. `RRGetOutputPrimary` returns
`outputs[0].output_id` instead of today's hard-coded `0`. The
spec deliberately does not expose `RRSetOutputPrimary` yet —
that's a mutation path, lumped with the other deferred RANDR
mutations.

`screen_resources_current()` returns `Vec<crtc>`, `Vec<output>`,
`Vec<mode>` covering all outputs. `crtc_info(crtc_id)` finds the
matching `RandrOutput` and returns its position + size.
`output_info(output_id)` similarly looks up by ID.

`CrtcInfoData` gets `(x, y)` fields:

```rust
pub struct CrtcInfoData {
    pub timestamp: u32,
    pub x:         i16,        // NEW
    pub y:         i16,        // NEW
    pub width:     u16,
    pub height:    u16,
}
```

The `RRGetCrtcInfo` encoder in `yserver-protocol/src/x11/randr.rs`
already reserves bytes 8-11 for x/y per the wire spec — it's just
been writing zeros. Two `put(byte_order, &mut out, …)` calls land
the new fields.

#### 2.6.1a RANDR request-handler audit

Codex's spec review surfaced that the wire-encoder side
(`yserver-protocol/src/x11/randr.rs`) is already vec-shaped, but
the **dispatch handlers** in
`crates/yserver-core/src/core_loop/process_request.rs` (~line
1274 onward) still serve singletons. Phase 6.10 must update these
in lockstep with the `RandrState` refactor:

- `RRGetScreenResources` / `RRGetScreenResourcesCurrent`: replace
  the `vec![CRTC_ID]` / `vec![OUTPUT_ID]` / single-mode literals
  with the actual collections from `RandrState`.
- `RRGetOutputInfo(output_id)`: look up the matching
  `RandrOutput`, return its CRTC list (today, one CRTC), connected
  state, mm dimensions, and connector name from
  `RandrOutput.name`.
- `RRGetCrtcInfo(crtc_id)`: look up the matching `RandrOutput` by
  `crtc_id`, return `(x, y, width, height, mode_id, [output_id])`.
- `RRGetCrtcGamma` / `RRGetCrtcGammaSize`: continue returning
  size=0 per output.
- `RRGetOutputPrimary`: return `RandrState.primary_output`.
- `RRGetMonitors`: see 2.6.2.
- `RRSelectInput`: per-client mask storage already correct, no
  change needed.

Mutation paths (`RRSetCrtcConfig`, `RRSetOutputPrimary`,
`RRSetCrtcGamma`, etc.) continue to return `BadValue`. None of
these are needed to read multi-monitor state correctly; they're
deferred to the hotplug / runtime-reconfig slice.

#### 2.6.2 `RRGetMonitors` (RANDR 1.5)

Today's stub returns one synthetic `ynest-0` monitor. The new
implementation returns one monitor entry per output with name
atom = interned connector name (`HDMI-A-1`, `DP-1`, etc.), output
list = `[output.output_id]`, and primary flag set on the
zero-indexed output.

#### 2.6.3 RANDR construction from KMS

`KmsBackend` doesn't talk to `RandrState` directly — `ServerState`
owns it (`yserver-core/src/server.rs:228`:
`randr: RandrState::nested(0, width, height)`). For Phase 6.10 we
add a constructor that takes pre-computed output records:

```rust
impl RandrState {
    pub fn from_outputs(timestamp: u32, screen: (u16, u16), outputs: Vec<RandrOutput>) -> Self;
}
```

`yserver::run` (`lib.rs:36-40`) constructs the backend, asks for
its layout via a new `Backend::randr_outputs()` trait method, and
seeds `ServerState::with_randr` (new constructor) with the
result. `HostX11Backend` returns a single-element `Vec` matching
today's behavior — no behavioral change for `ynest`.

### 2.7 Pointer routing and layout

The libinput pump produces motion in absolute coords clamped to the
virtual-screen extent. `input_thread::run` (`lib.rs:77`) takes
`u32::from(fb_w)` / `u32::from(fb_h)` today; those become the
virtual-screen extent, so absolute coords from libinput already
land in the right space. virtio-tablet absolute coords are scaled
to the virtual extent.

No per-output pointer logic needed — pointer events carry
screen-space `(root_x, root_y)`, and the existing
`window_under_cursor()` walk is unchanged.

`WarpPointer`, hover crossings, and grab-window-relative event
coords are all already in virtual screen space, so they don't
care about output boundaries.

### 2.8 vng test recipe

Add to `Justfile`:

```just
yserver-multihead:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display sdl,gl=on -vga none -device virtio-gpu-pci,max_outputs=2 \
                     -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- target/debug/yserver
```

`virtio-gpu-pci,max_outputs=2` makes the kernel expose two
connectors. `-display sdl,gl=on` is **the validation hypothesis,
not a verified recipe** — codex's spec review flagged that this
QEMU/vng/SDL combination has not been exercised in this repo
before. Step 0 of the implementation plan is "verify the host
QEMU exposes both scanouts the way we expect" *before* writing
any backend code. If SDL doesn't fan out scanouts to separate
windows on this host, alternatives include the GTK backend (uses
tabs — visually awkward but functional) or running two separate
QEMU instances (loses the shared-process model). The kernel
virtio-gpu driver assigns each connector a default mode based on
QEMU `-device` `xres`/`yres` hints; `YSERVER_MODE=1024x768`
already works per-output.

## 3. Implementation Plan

### Step 0 — vng multi-scanout verification (no backend code)

Boot the existing `yserver` build under
`virtio-gpu-pci,max_outputs=2` with `-display sdl,gl=on` and
confirm the kernel exposes two connected connectors (`drm-info`
or `cat /sys/class/drm/*/status`) and that QEMU presents two
windows. If SDL collapses scanouts into one window, switch to
GTK with tabs and document. If neither works, escalate before
spending implementation effort.

### Step 0a — Per-CRTC page-flip identifier

Plumbing-only change: `page_flip::drain_events` closure takes
`crtc::Handle`. Single-output callers ignore it. Lands without
touching `discover_output` or compositor. Workspace stays green
under existing single-output behavior.

### Step 1 — `discover_outputs` returning `Vec<Output>`

Refactor `drm/modeset.rs`. Existing `discover_output` becomes a
thin wrapper that returns `outs.into_iter().next()` so any
remaining single-output call sites (notably tests) keep working.
Add CRTC-claim-set logic with **hard error on unplaceable
connector** per §2.1 — virtio-gpu scope. Add unit test
simulating two connectors → two distinct CRTCs picked, plus a
test for the failure case (greedy-strands a connector → returns
err, no partial state).

### Step 2 — `KmsBackend` carrying `Vec<OutputLayout>`

Replace single `output` + `swapchain` fields. Initial layout =
horizontal, in connector order. `composite_and_flip` becomes the
per-output loop **with the explicit window-rect vs output-rect
intersection pre-filter from §2.5**; existing single-output
behavior preserved when `outputs.len() == 1`. `disable_output`
loops too. **Bring-up wraps the `commit_modeset` loop in the
rollback-on-failure pattern from §2.1**. Unit tests cover:
`composite_and_flip` painting different top-level windows into
different outputs; pre-filter excludes a window fully outside an
output's rect; rollback on a synthetic Nth-modeset failure
disables 0..N-1.

### Step 3 — `RandrState` multi-output + handler audit

Drop the `OUTPUT_ID` / `CRTC_ID` / `MODE_ID` consts. Add
`RandrOutput` and `RandrMode` (deduped by `(w, h, vrefresh)`).
Add `screen_width_mm` / `screen_height_mm` / `primary_output`
fields. Migrate every reader of those consts (grep across
`yserver-core` + `yserver-protocol` — there are roughly a
dozen). Add `(x, y)` to `CrtcInfoData` and propagate through
the encoder. Add `from_outputs` constructor. Existing
`RandrState::nested` becomes `from_outputs` with a single
synthetic record.

**RANDR request handler audit per §2.6.1a**: walk every
`RR…` arm in `core_loop/process_request.rs` (~line 1274), update
each to read from the new collections instead of singleton
literals. `RRGetOutputPrimary` returns the new
`primary_output` field. xts `Xrandr` scenario, if it runs at
all, should not regress.

### Step 4 — `Backend::randr_outputs()` trait method

Add a method to the trait returning `Vec<RandrOutput>`.
`HostX11Backend` returns `vec![<single host-derived record>]` —
unchanged behavior. `KmsBackend` returns one per
`OutputLayout`. Wire `yserver::run` and `nested::run` to use
the result when constructing `ServerState`.

### Step 5 — vng smoke

Add `yserver-multihead` Justfile target. Bring up under
`max_outputs=2`, verify in two SDL windows: cursor moves
across the seam, `xrandr -q` reports two outputs at the
correct positions, fvwm3 panel appears on the primary, an
`xterm` started with `-geometry +1100+100` (past the first
output's right edge) lands on the secondary.

### Step 6 — Bare-metal smoke

On the CachyOS host with two physical displays connected,
bring up yserver via the existing bare-metal recipe. Same
xrandr / xterm placement / cursor traversal checks. Capture
state as a follow-up note.

## 4. Out of scope (deferred to 6.10.x)

- **Real-hardware encoder/CRTC matching.** Phase 6.10 is scoped
  to virtio-gpu where greedy assignment is always correct.
  Intel/AMD hardware with shared encoder pools needs a real
  bipartite matching pass. Tracked as a follow-up slice; the
  TODO marker on `discover_outputs` flags the gap.
- **Hotplug.** Connector add / remove handled at startup only.
  KMS uevents aren't drained. Adding a dynamic `RRSetCrtcConfig` /
  `RRScreenChangeNotify` path is its own slice — needs
  RANDR mutation paths (today they `BadValue`) wired through to
  real CRTC reconfigure.
- **Mode switching at runtime.** Each output is bound to its
  startup-picked mode for the session. `xrandr --mode` returns
  `BadValue` like today.
- **Overlay / cursor planes.** Hardware cursor and overlay-plane
  scanout are still single-plane primary-only.
- **Per-output DPI in RANDR.** Reported `width_mm` / `height_mm`
  uses the 96-DPI heuristic; real EDID-derived physical dimensions
  are a follow-up.
- **Mirror / clone mode.** Layout is purely tiled in this slice.
  RANDR's `RRSetCrtcConfig` lets clients request clone, but until
  hot-reconfigure exists this is moot.
- **GBM / EGL multi-output zero-copy.** Dumb buffers per output
  is the entire path. Per-output GL contexts are a Phase 6.x
  optimization.
- **`YSERVER_LAYOUT` env override.** Default horizontal-by-
  enumeration order is enough for the first validation. Custom
  layouts land when a real workflow needs them.
- **xrandr-driven layout reconfigure.** Moving outputs around
  with `xrandr --output … --right-of …` requires `RRSetCrtcConfig`
  to actually reconfigure the layout. Today and in 6.10 it stays
  a `BadValue`-returning stub.

## 5. Validation gate

Phase 6.10 is complete when:

1. `cargo test --workspace` green.
2. `cargo clippy -p yserver -- -D warnings` clean.
3. `just yserver-multihead` brings up two SDL windows showing
   distinct portions of the virtual screen, with a cursor that
   crosses the seam smoothly.
4. Inside the guest, `xrandr -q` reports two `Virtual-N` outputs
   at the expected `(x, y)` positions and modes.
5. `fvwm3` runs with both outputs available; an `xterm
   -geometry +1100+100` lands on the second output (assuming a
   1024-wide first output).
6. xts `Xrandr` scenario does not regress relative to its
   pre-Phase-6.10 baseline.
7. ynest still passes the Phase 6.9 xts matrix unchanged
   (host backend reports a single output → no behavior delta).
8. `xdpyinfo` reports root `width_mm` / `height_mm` ≈ sum-of-w /
   max-of-h across the two outputs (sanity-check DPI).
9. `xrandr --query` reports `primary` on the first output.
10. Two outputs running the same resolution share a single mode
    line in `xrandr --query` output (mode dedup verification).
