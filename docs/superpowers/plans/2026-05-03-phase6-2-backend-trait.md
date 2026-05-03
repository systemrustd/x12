# Phase 6.2 — `Backend` trait extraction implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Carve a `Backend` trait out of `yserver-core` so `nested.rs`
becomes one impl (`HostX11Backend`) and a future KMS backend slots in.
Lands three of the five C-prework items from the 6.1 design (per-kind
handle newtypes; bundle `allocate_xid` into `create_*`; `&DrawState`
by-borrow per drawing call). The pump/main connection merge is
explicitly **deferred** to its own slice.

**Architecture:** Same `Arc<Mutex<dyn Backend>>` shape as today's
`Arc<Mutex<HostX11>>`. `HostX11Backend` retains today's `HostInputPump`
thread and per-client kb pumps as implementation details — only the
main pump is routed through the new `BackendEventSink`. Drawing
methods take `&DrawState` (no `GcHandle` on the trait). Per-kind
handle newtypes (`WindowHandle`, `PixmapHandle`, …) replace
`Option<u32>` slots.

**Tech Stack:** Rust 2024 edition, existing workspace deps. No new
crate dependencies. The trait surface is ~95 methods; module layout
adds `crates/yserver-core/src/backend/` and converts
`crates/yserver-core/src/host_x11.rs` (3,643 lines) into a
`host_x11/` module.

**Branch:** Create `phase6-2-backend-trait` for development.
Squash-merge to master matching Phase 6.1's pattern. Per-step commits
during development for bisect.

**Companion design:** `docs/superpowers/specs/2026-05-03-phase6-2-backend-trait-design.md`.

---

## Status

Not started. Six steps pending.

## Strategy

Each numbered Step is one logical commit on `phase6-2-backend-trait`.
The order is chosen so `cargo build` + `cargo test --workspace` are
green at every commit. The branch squash-merges at the end; per-step
history is preserved during development for bisection.

After every commit:

```sh
cargo +nightly fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

All three must pass. Manual smoke gates per the design doc fire at
Steps 2, 3, 5, 6.

The design doc's risk table calls out three explicit risks that the
plan must defend against: (1) trait shape baking in HostX11
assumptions — Step 5's `RecordingBackend` is the existence proof;
(2) `&DrawState` per-call resolution being slower than today's cached
state — Step 3's HostX11 impl preserves the apply-when-changed
optimization; (3) `Arc<Mutex<dyn Backend>>` vs
`Arc<Mutex<HostX11>>` — assumed wash, validated by Step 6 manual smoke.

## Pre-flight

Before Step 1 starts, on master:

```sh
git checkout master && git pull
git checkout -b phase6-2-backend-trait
```

Sanity-check the baseline:

```sh
cargo +nightly fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

All three must pass before starting. If any fails on master, stop and
fix that first — Phase 6.2 doesn't introduce baseline regressions.

---

## Step 1 — Per-kind handle newtypes (prework #3 + #4)

**Goal:** Replace the 16 `Option<u32>` host-XID slots in `yserver-core`
with `Option<KindHandle>` newtypes, propagating through every
call-site. Pure type churn; no behavior change.

**Files:**
- Create: `crates/yserver-core/src/backend/mod.rs`
- Create: `crates/yserver-core/src/backend/handles.rs`
- Modify: `crates/yserver-core/src/lib.rs` (add `pub mod backend;`)
- Modify: `crates/yserver-core/src/resources.rs` (~10 field types, ~60 call sites)
- Modify: `crates/yserver-core/src/host_x11.rs` (~100 call sites)
- Modify: `crates/yserver-core/src/nested.rs` (~170 call sites)

**Estimate:** ~10 files touched, ~400 LoC churn, ~380 call sites.
Mostly compiler-driven (the type checker walks every site).

### Task 1.1 — Create the handles module

Write `crates/yserver-core/src/backend/handles.rs`:

```rust
//! Per-kind newtypes wrapping host XIDs (or, in future backends,
//! native resource handles). All are `NonZeroU32` so that
//! `Option<KindHandle>` is one word and the X11 reserved value
//! `0` (= None) is statically unrepresentable in the success type.

use std::num::NonZeroU32;

macro_rules! handle {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
        pub struct $name(NonZeroU32);

        impl $name {
            pub fn from_raw(raw: u32) -> Option<Self> {
                NonZeroU32::new(raw).map($name)
            }

            pub fn as_raw(self) -> u32 {
                self.0.get()
            }

            #[cfg(test)]
            pub fn from_raw_for_test(raw: u32) -> Self {
                Self::from_raw(raw).expect("test handle must be non-zero")
            }
        }
    };
}

handle!(WindowHandle, "Backend handle for an X11 InputOutput / InputOnly window.");
handle!(PixmapHandle, "Backend handle for a pixmap.");
handle!(PictureHandle, "Backend handle for a RENDER picture.");
handle!(GlyphSetHandle, "Backend handle for a RENDER glyphset.");
handle!(FontHandle, "Backend handle for an opened font.");
handle!(CursorHandle, "Backend handle for a cursor.");
handle!(ColormapHandle, "Backend handle for a colormap.");

/// Drawable that may be a window or a pixmap. Drawing methods take this.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub enum AnyHandle {
    Window(WindowHandle),
    Pixmap(PixmapHandle),
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum HandleKind {
    Window,
    Pixmap,
    Picture,
    GlyphSet,
    Font,
    Cursor,
    Colormap,
}

impl AnyHandle {
    pub fn kind(self) -> HandleKind {
        match self {
            AnyHandle::Window(_) => HandleKind::Window,
            AnyHandle::Pixmap(_) => HandleKind::Pixmap,
        }
    }

    pub fn as_raw(self) -> u32 {
        match self {
            AnyHandle::Window(h) => h.as_raw(),
            AnyHandle::Pixmap(h) => h.as_raw(),
        }
    }
}

impl From<WindowHandle> for AnyHandle {
    fn from(h: WindowHandle) -> Self {
        AnyHandle::Window(h)
    }
}

impl From<PixmapHandle> for AnyHandle {
    fn from(h: PixmapHandle) -> Self {
        AnyHandle::Pixmap(h)
    }
}
```

### Task 1.2 — Create backend module entry point

Write `crates/yserver-core/src/backend/mod.rs`:

```rust
//! Backend abstraction. Currently HostX11Backend is the sole impl;
//! Phase 6.3+ will add a KMS backend.

pub mod handles;

pub use handles::{
    AnyHandle, ColormapHandle, CursorHandle, FontHandle, GlyphSetHandle,
    HandleKind, PictureHandle, PixmapHandle, WindowHandle,
};
```

### Task 1.3 — Wire backend module into the crate root

Modify `crates/yserver-core/src/lib.rs`. Add `pub mod backend;` near
the other `pub mod` declarations (alphabetical placement preferred).

Run `cargo build -p yserver-core`. Expect: success (no consumers yet).

### Task 1.4 — Re-type `Window.host_xid`

In `crates/yserver-core/src/resources.rs`, locate the `Window` struct
and change:

```rust
pub host_xid: Option<u32>,
```

to:

```rust
pub host_xid: Option<crate::backend::WindowHandle>,
```

Run `cargo build -p yserver-core`. The compiler will surface every
reader and writer of `Window.host_xid` as a type error. **Do not
proceed to Task 1.5 yet** — this task ends with a build failure list.

### Task 1.5 — Walk every `Window.host_xid` site

For each compiler error from Task 1.4, apply the right transform:

- **Reader** (`some_value = window.host_xid` or `window.host_xid.unwrap()`):
  the target variable's type changes too. Often this means the
  receiving slot in another struct also needs re-typing (e.g.
  `xid_map: HashMap<u32, ResourceId>` may need its key type
  preserved as `u32` — call `.as_raw()` on the handle in that case).
- **Writer** (`window.host_xid = Some(xid)` where `xid: u32`):
  replace with `window.host_xid = WindowHandle::from_raw(xid);`.
- **Comparison** (`window.host_xid == Some(other_xid)` where `other_xid: u32`):
  replace with `window.host_xid.map(|h| h.as_raw()) == Some(other_xid)` or
  re-type `other_xid` if it should also be a handle.

Use `cargo build -p yserver-core` after each batch of fixes to drive
the next set of errors.

### Task 1.6 — Re-type the other resource structs

In the same pattern as Task 1.4–1.5, re-type these fields one at a time:

- `Pixmap.host_xid` → `Option<PixmapHandle>`
- `Picture` (and `PictureState`): every `host_*_xid` of picture kind →
  `Option<PictureHandle>`. Note: `PictureState` may carry references to
  pixmaps (alpha mask, clip mask) that should become `Option<PixmapHandle>`.
- `GlyphSetState.host_glyphset_xid` → `Option<GlyphSetHandle>`
- `Font.host_xid` → `Option<FontHandle>`
- `Cursor.host_xid` → `Option<CursorHandle>`
- `Colormap.host_xid` → `Option<ColormapHandle>`
- `GcState.host_xid` → `Option<GcHandle>` *only if the field exists* —
  the design says no `GcHandle` on the trait. If `GcState` holds a host
  GC XID, that lives inside `HostX11Backend`'s implementation detail
  and is NOT exposed on the trait. Skip the re-type for `GcState`'s
  field; the backend keeps the raw `u32` privately.

After each, build and walk errors.

### Task 1.7 — Re-type GC-state enums

`GcClipState::Pixmap.host_pixmap` — change to `PixmapHandle` (no
`Option<>` because the variant only exists when there *is* a pixmap).
Same for `GcFillState::Tiled.host_pixmap`,
`GcFillState::Stippled.host_pixmap`, and any
`GcFillState::OpaqueStippled.host_pixmap`. Build, walk errors.

### Task 1.8 — Re-type `NamedCompositePixmap.host_pixmap`

→ `PixmapHandle`. Build, walk errors.

### Task 1.9 — Patch `xid_map` consumers in host_x11.rs

`HostX11`'s reverse map for host events (`xid_map: HashMap<u32,
ResourceId>` or similar) keeps `u32` keys for now (the wire-side host
XID is what arrives in events). Anywhere the code converts a
`WindowHandle` to a map key, call `.as_raw()`. Build, walk errors.

### Task 1.10 — Run the full check suite

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

Expect: all green. If `cargo test` surfaces failures, the most likely
cause is a test that constructed `Window { host_xid: Some(0xdeadbeef),
... }` directly. Use `WindowHandle::from_raw_for_test(0xdeadbeef)` to
fix.

### Task 1.11 — Commit

```sh
git add crates/yserver-core/src/backend/ crates/yserver-core/src/lib.rs \
        crates/yserver-core/src/resources.rs \
        crates/yserver-core/src/host_x11.rs \
        crates/yserver-core/src/nested.rs
git commit -m "feat: Phase 6.2 Step 1 — per-kind handle newtypes for host XIDs"
```

---

## Step 2 — Bundle `allocate_xid` into `create_*` (prework #1)

**Goal:** Eliminate the two-phase pattern `let xid = host.allocate_xid();
host.create_window(xid, ...)` in favor of `let h =
host.create_window(...)?;`. Mechanical refactor that cleans up call
sites and pre-shapes the future trait method signatures.

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/resources.rs` (a small number of allocate-then-store sites)

**Estimate:** ~5 files, ~300 LoC, ~50 call sites.

### Task 2.1 — Identify creation method signatures in host_x11.rs

```sh
grep -n "fn create_\|fn open_font\|fn allocate_xid" crates/yserver-core/src/host_x11.rs
```

List the methods to change:
- `create_window(xid, …)` → `create_window(…) -> io::Result<WindowHandle>`
- `create_pixmap(xid, …)` → `create_pixmap(…) -> io::Result<PixmapHandle>`
- `create_picture` (likely takes xid today) → returns `PictureHandle`
- `create_glyphset` → returns `GlyphSetHandle`
- `create_cursor` → returns `CursorHandle`
- `open_font` (already returns a handle today, just re-type) → returns `(FontHandle, FontMetrics)`
- `create_colormap` → returns `ColormapHandle`

Note: `allocate_xid()` is the public method that pre-allocated host
XIDs before calling create. After Step 2 it becomes a private helper
(`fn next_xid(&mut self) -> u32`) used internally by the bundled
methods. Step 5 will move this internal helper into the trait impl
detail.

### Task 2.2 — Refactor `create_window`

Inside `host_x11.rs`, modify `create_window` to call the now-private
`next_xid()` itself, return `io::Result<WindowHandle>` (constructed
via `WindowHandle::from_raw(xid).expect("non-zero")`).

Build will fail; the next task fixes call sites.

### Task 2.3 — Walk every `create_window` call site

For each call site (mostly in `nested.rs`):

Before:
```rust
let xid = host.allocate_xid();
host.create_window(xid, parent_xid, ...)?;
window.host_xid = Some(WindowHandle::from_raw(xid).unwrap());
```

After:
```rust
let h = host.create_window(parent_handle, ...)?;
window.host_xid = Some(h);
```

If the call site also passes the allocated xid to other code (e.g.
adds it to an `xid_map`), shift to `h.as_raw()` for the map insertion.

### Task 2.4 — Apply Task 2.2 + 2.3 pattern to the other create methods

In order:
- `create_pixmap` (~5 call sites)
- `create_picture` (~3 call sites)
- `create_glyphset` (~2 call sites)
- `create_cursor` (~3 call sites)
- `open_font` (already returns a handle; just re-type)
- `create_colormap` (~2 call sites)

Build between each method's batch.

### Task 2.5 — Audit any remaining `host.allocate_xid()` calls

```sh
grep -n "allocate_xid" crates/yserver-core/src/
```

Should return zero non-test hits (the public method is removed; the
private helper has a different name). If hits remain, they're either:
- Old call sites missed in 2.3/2.4 — fix in place.
- Tests that relied on the old method shape — port to the new
  bundled `create_*` shape.

### Task 2.6 — Run the full check suite + manual smoke

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green. Then **manual smoke gate** per the design doc:

```sh
just ynest &     # background
sleep 1
DISPLAY=:99 xterm    # type "ls", quit
```

Expected: xterm renders, accepts input, exits cleanly. No new errors
in the ynest log on stderr.

### Task 2.7 — Commit

```sh
git add crates/yserver-core/src/host_x11.rs \
        crates/yserver-core/src/nested.rs \
        crates/yserver-core/src/resources.rs
git commit -m "feat: Phase 6.2 Step 2 — bundle allocate_xid into create_* methods"
```

---

## Step 3 — `DrawState` and per-call resolution (prework #2 partial)

**Goal:** Define `DrawState` as a snapshot of GC state. Refactor
`apply_gc_clip` and friends to read from `&DrawState`. Refactor every
drawing call site in `nested.rs` to resolve once and pass `&DrawState`.

**Files:**
- Create: `crates/yserver-core/src/backend/params.rs`
- Modify: `crates/yserver-core/src/resources.rs` (add `resolve_draw_state`)
- Modify: `crates/yserver-core/src/host_x11.rs` (drawing methods take `&DrawState`)
- Modify: `crates/yserver-core/src/nested.rs` (resolve once per call site)
- Test: new tests in `crates/yserver-core/src/resources.rs` (or a sibling test module) for `resolve_draw_state`

**Estimate:** ~3 files, ~400 LoC.

### Task 3.1 — Define `DrawState` and supporting enums

Write `crates/yserver-core/src/backend/params.rs`:

```rust
//! Parameter types for the Backend trait. Snapshots of state that are
//! resolved by yserver-core once per request and passed to the backend.

use crate::backend::{FontHandle, PixmapHandle};

#[derive(Clone, Copy, Debug)]
pub enum LineStyle { Solid, OnOffDash, DoubleDash }
#[derive(Clone, Copy, Debug)]
pub enum CapStyle { NotLast, Butt, Round, Projecting }
#[derive(Clone, Copy, Debug)]
pub enum JoinStyle { Miter, Round, Bevel }
#[derive(Clone, Copy, Debug)]
pub enum FillStyle { Solid, Tiled, Stippled, OpaqueStippled }
#[derive(Clone, Copy, Debug)]
pub enum FillRule { EvenOdd, Winding }
#[derive(Clone, Copy, Debug)]
pub enum GcFunction { Clear, And, AndReverse, Copy, AndInverted, NoOp,
    Xor, Or, Nor, Equiv, Invert, OrReverse, CopyInverted, OrInverted, Nand, Set }
#[derive(Clone, Copy, Debug)]
pub enum SubwindowMode { ClipByChildren, IncludeInferiors }
#[derive(Clone, Copy, Debug)]
pub enum ArcMode { Chord, PieSlice }

#[derive(Clone, Debug)]
pub enum ClipState {
    None,
    Rectangles { origin: (i16, i16), rects: Vec<crate::Rect> },
    Pixmap { origin: (i16, i16), pixmap: PixmapHandle },
}

#[derive(Clone, Debug)]
pub enum FillState {
    Solid,
    Tiled { pixmap: PixmapHandle, origin: (i16, i16) },
    Stippled { pixmap: PixmapHandle, origin: (i16, i16) },
    OpaqueStippled { pixmap: PixmapHandle, origin: (i16, i16) },
}

#[derive(Clone, Debug)]
pub enum BgState {
    Pixel(u32),
    Pixmap(PixmapHandle),
    None,
}

/// Resolved snapshot of GC state for one drawing call. Passed by
/// reference to Backend drawing methods.
#[derive(Clone, Debug)]
pub struct DrawState {
    pub foreground: u32,
    pub background: u32,
    pub line_width: u16,
    pub line_style: LineStyle,
    pub cap_style: CapStyle,
    pub join_style: JoinStyle,
    pub fill_style: FillStyle,
    pub fill_rule: FillRule,
    pub function: GcFunction,
    pub plane_mask: u32,
    pub font: Option<FontHandle>,
    pub clip: ClipState,
    pub fill: FillState,
    pub subwindow_mode: SubwindowMode,
    pub graphics_exposures: bool,
    pub dashes: Vec<u8>,
    pub dash_offset: i16,
    pub arc_mode: ArcMode,
}

impl Default for DrawState {
    fn default() -> Self {
        Self {
            foreground: 0,
            background: 0xffffff,
            line_width: 0,
            line_style: LineStyle::Solid,
            cap_style: CapStyle::Butt,
            join_style: JoinStyle::Miter,
            fill_style: FillStyle::Solid,
            fill_rule: FillRule::EvenOdd,
            function: GcFunction::Copy,
            plane_mask: u32::MAX,
            font: None,
            clip: ClipState::None,
            fill: FillState::Solid,
            subwindow_mode: SubwindowMode::ClipByChildren,
            graphics_exposures: true,
            dashes: vec![4, 4],
            dash_offset: 0,
            arc_mode: ArcMode::PieSlice,
        }
    }
}
```

Add `pub mod params;` to `backend/mod.rs` and re-export the types
above. Build to verify the new module compiles in isolation.

### Task 3.2 — Add `ResourceTable::resolve_draw_state`

In `resources.rs`, add a method that converts a client's `GcState`
plus the relevant resource lookups into a `DrawState`:

```rust
impl ResourceTable {
    pub fn resolve_draw_state(&self, gc_id: ResourceId) -> Option<DrawState> {
        let gc = self.gcs.get(&gc_id)?;
        let mut state = DrawState::default();
        state.foreground = gc.foreground;
        state.background = gc.background;
        state.line_width = gc.line_width;
        state.line_style = gc.line_style;
        state.cap_style = gc.cap_style;
        state.join_style = gc.join_style;
        state.fill_style = gc.fill_style;
        state.fill_rule = gc.fill_rule;
        state.function = gc.function;
        state.plane_mask = gc.plane_mask;
        state.subwindow_mode = gc.subwindow_mode;
        state.graphics_exposures = gc.graphics_exposures;
        state.arc_mode = gc.arc_mode;
        state.dashes = gc.dashes.clone();
        state.dash_offset = gc.dash_offset;

        // Font handle resolution
        if let Some(font_id) = gc.font {
            state.font = self.fonts.get(&font_id).and_then(|f| f.host_xid);
        }

        // Clip state resolution
        state.clip = match &gc.clip_state {
            GcClipState::None => ClipState::None,
            GcClipState::Rectangles(rects) => ClipState::Rectangles {
                origin: (gc.clip_x_origin, gc.clip_y_origin),
                rects: rects.clone(),
            },
            GcClipState::Pixmap { host_pixmap } => ClipState::Pixmap {
                origin: (gc.clip_x_origin, gc.clip_y_origin),
                pixmap: *host_pixmap,
            },
        };

        // Fill state resolution
        state.fill = match &gc.fill_state {
            GcFillState::Solid => FillState::Solid,
            GcFillState::Tiled { host_pixmap } => FillState::Tiled {
                pixmap: *host_pixmap,
                origin: (gc.tile_stipple_x_origin, gc.tile_stipple_y_origin),
            },
            // …Stippled, OpaqueStippled
        };

        Some(state)
    }
}
```

(Adapt field names to whatever the actual `GcState` uses today —
`grep -n "pub struct Gc\|pub fn change_gc" resources.rs` to find them.)

### Task 3.3 — Add unit tests for `resolve_draw_state`

In `resources.rs` test module (or a sibling file), add tests for each
fill / clip / function variant. Aim for ~6 tests:

```rust
#[test]
fn resolve_draw_state_default_gc() { ... }
#[test]
fn resolve_draw_state_solid_fill_with_clip_rectangles() { ... }
#[test]
fn resolve_draw_state_tiled_fill() { ... }
#[test]
fn resolve_draw_state_stippled_fill() { ... }
#[test]
fn resolve_draw_state_pixmap_clip() { ... }
#[test]
fn resolve_draw_state_unknown_gc_returns_none() { ... }
```

Run `cargo test -p yserver-core resolve_draw_state -- --nocapture`. All
six should pass.

### Task 3.4 — Refactor `apply_gc_clip` to take `&DrawState`

In `host_x11.rs`, locate `apply_gc_clip` (and any neighbors like
`set_clip_pixmap`, `set_gc_fill_state`, `set_gc_fill_solid`). They
currently read from per-client `GcState`. Refactor each to take
`&DrawState` and read its fields directly.

The cache that `HostX11` maintains for "most-recently-applied state
per host depth" stays — but its key fields now compare against the
`DrawState` parameter instead of a client-side `GcState`. The
optimization (don't re-push state that's unchanged) is preserved.

Build between sub-changes; the caller side will fail next.

### Task 3.5 — Refactor a sample drawing method (`poly_line`)

In `host_x11.rs`:

Before:
```rust
pub fn poly_line(&mut self, dst: u32, gc: &GcState, points: &[Point]) -> io::Result<()> {
    self.apply_gc_clip(gc, ...)?;
    // … write XPolyLine bytes …
}
```

After:
```rust
pub fn poly_line(&mut self, dst: AnyHandle, state: &DrawState, points: &[Point]) -> io::Result<()> {
    self.apply_draw_state(state)?;
    // … write XPolyLine bytes (use dst.as_raw()) …
}
```

Build; caller side in `nested.rs` will fail.

### Task 3.6 — Walk every `nested.rs` `host.poly_line` call site

For each `nested.rs` site that calls `host.poly_line(window.host_xid.unwrap(), &gc, points)`:

Before:
```rust
let gc = resources.gcs.get(&gc_id).ok_or(...)?;
host.poly_line(window.host_xid.unwrap().as_raw(), gc, points)?;
```

After:
```rust
let state = resources.resolve_draw_state(gc_id).ok_or(...)?;
host.poly_line(window.host_xid.unwrap().into(), &state, points)?;
```

Build between batches.

### Task 3.7 — Apply the same pattern to every other drawing method

In rough alphabetical order:
- `clear_area` (special: takes `BgState`, not `&DrawState` — see trait
  surface in design)
- `copy_area`
- `copy_plane`
- `fill_poly`
- `image_text8` / `image_text16`
- `poly_arc`
- `poly_fill_arc`
- `poly_fill_rectangle`
- `poly_point`
- `poly_rectangle`
- `poly_segment`
- `poly_text8` / `poly_text16`
- `put_image`

For each: refactor the host_x11.rs method, walk every nested.rs site.
This is the largest sub-task by line count — expect ~250 LoC of
mechanical changes.

Also extension drawing methods that take GC state:
- RENDER `Composite` (uses GC?)
- RENDER `FillRectangles` (uses GC?)
- Any other RENDER methods that do clipping / filling — refactor to
  take `&DrawState` if they read from it today.

### Task 3.8 — Composite call sequences collapse

The 6.1 design's prework #2 named four composite operations. Three of
them collapse now that `DrawState` is by-value:

- **`put_image_with_clear`**: today's call sequence is "set
  clip-mask=None, then put_image". Now: caller passes a `DrawState`
  with `clip: ClipState::None` and calls `put_image` normally.
  Audit nested.rs for the existing pattern (likely a helper function
  with this name); inline its body into the call site, taking care
  to construct the cleared-clip `DrawState` correctly.
- **`fill_with_state`**: today's "set fill_style=Tiled, then
  poly_fill_rectangle". Now: caller passes a `DrawState` with
  `fill: FillState::Tiled{...}` and calls `poly_fill_rectangle`
  normally.
- **`clear_area_with_bg`**: this one uses `BgState`, not `DrawState`.
  Confirm `clear_area`'s trait signature in the design — it takes a
  `BgState` parameter directly. Audit and inline the existing helper.

The fourth (`list_fonts_proxy`) survives as its own composite method —
it is genuinely multi-reply streaming and stays outside the
`DrawState` model.

### Task 3.9 — Run the full check suite + manual smoke

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green. Then **manual smoke gate** per the design doc:

```sh
just ynest &
sleep 1
DISPLAY=:99 wmaker &
sleep 2
DISPLAY=:99 xterm -e fish &
DISPLAY=:99 xclock &
# Interactively: type into xterm (input works), drag a window
# (rendering survives drag), close xterm (no crash).
```

Acceptance: chrome renders, drags work, xterm input echoes, xclock
ticks. If anything is visibly broken, the most likely culprit is a
mis-resolved `DrawState` field (e.g. forgot to copy `dashes` or
`fill_style`). Diff against the previous commit's `apply_gc_clip` to
find the missing field.

### Task 3.10 — Commit

```sh
git add crates/yserver-core/src/backend/ \
        crates/yserver-core/src/resources.rs \
        crates/yserver-core/src/host_x11.rs \
        crates/yserver-core/src/nested.rs
git commit -m "feat: Phase 6.2 Step 3 — DrawState by-borrow per drawing call"
```

---

## Step 4 — Mechanical module split of `host_x11.rs`

**Goal:** Convert the single 3,643-line `host_x11.rs` into a
`host_x11/` module split across `mod.rs`, `request.rs`, `pump.rs`,
`sync.rs`. Pure file moves; no behavior change. Lands separately so
Step 5's diff is focused on logic.

**Files:**
- Create: `crates/yserver-core/src/host_x11/mod.rs`
- Create: `crates/yserver-core/src/host_x11/request.rs`
- Create: `crates/yserver-core/src/host_x11/pump.rs`
- Create: `crates/yserver-core/src/host_x11/sync.rs`
- Delete: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-core/src/lib.rs` (already exposes `host_x11`; the rename from file to dir is transparent)

**Estimate:** Pure file moves; ~0 LoC churn but lots of `pub`
adjustments for cross-module access.

### Task 4.1 — Move `host_x11.rs` to `host_x11/mod.rs`

```sh
mkdir -p crates/yserver-core/src/host_x11
git mv crates/yserver-core/src/host_x11.rs crates/yserver-core/src/host_x11/mod.rs
cargo build -p yserver-core
```

Should succeed unchanged — the module path is the same.

### Task 4.2 — Carve out `pump.rs`

`pump.rs` gets the `HostInputPump` thread, its event-translation logic,
and the helpers it calls. Cut from `mod.rs`, paste into `pump.rs`.

Add `mod pump;` in `mod.rs` and `pub use pump::HostInputPump;` (and
any other types that `nested.rs` consumes).

For symbols that `pump.rs` uses from `mod.rs` (e.g. `xid_map` on the
shared `HostX11` struct), they must be `pub(super)` or `pub(crate)`.
The compiler will guide this.

### Task 4.3 — Carve out `sync.rs`

`sync.rs` gets `sync_main_connection` and `reply_buffer` handling.
Cut from `mod.rs`, paste into `sync.rs`. `mod sync;` and re-exports
as needed.

### Task 4.4 — Carve out `request.rs`

The bulk of `mod.rs` is request-side methods (the `create_*`,
drawing, extension proxies, etc. — basically everything the trait
will define in Step 5). Cut these into `request.rs` as
`impl HostX11 { ... }` blocks. `mod request;` is enough to expose
them — Rust automatically picks up `impl` blocks across files
within the same module.

### Task 4.5 — Inspect `mod.rs`

After Task 4.4, `mod.rs` should hold:
- The `HostX11` struct definition
- The constructor (`HostX11::open_from_env` or similar)
- Module declarations and re-exports
- A short prelude / common helpers if needed

If it's still over ~500 lines, more carving is possible — but Phase
6.2 doesn't require optimal split, just *some* split. Don't over-
engineer.

### Task 4.6 — Run the full check suite

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green. No manual smoke needed for pure file moves — the unit
tests + build are sufficient.

### Task 4.7 — Commit

```sh
git add crates/yserver-core/src/host_x11/
git commit -m "refactor: Phase 6.2 Step 4 — split host_x11.rs into host_x11/{mod,request,pump,sync}.rs"
```

---

## Step 5 — Carve the `Backend` trait

**Goal:** Define the `Backend` trait, rename `HostX11` →
`HostX11Backend`, `impl Backend for HostX11Backend`. Wire
`register_event_sink` for the main `HostInputPump` only (per-client
kb pumps stay direct-to-client per the design's pass-3 codex finding).
Add `RecordingBackend` test double + 2–4 `nested.rs` integration tests.

**Files:**
- Modify: `crates/yserver-core/src/backend/mod.rs` (add trait + sink + event types)
- Create: `crates/yserver-core/src/backend/sink.rs` (`BackendEventSink` impl wrapping the existing fanout)
- Create: `crates/yserver-core/src/backend/recording.rs` (test double, `#[cfg(test)]`)
- Modify: `crates/yserver-core/src/host_x11/mod.rs` (rename struct, impl trait)
- Modify: `crates/yserver-core/src/nested.rs` and `server.rs`
  (`Arc<Mutex<HostX11>>` → `Arc<Mutex<dyn Backend>>`)
- New tests in `crates/yserver-core/src/backend/recording.rs` or a
  sibling test module.

**Estimate:** ~6 files, ~400 LoC + ~150 LoC for `RecordingBackend`.

### Task 5.1 — Define the `Backend` trait

In `crates/yserver-core/src/backend/mod.rs`, add the trait definition.
Copy method signatures from the design doc's "Trait surface" section
verbatim. Mark the trait `pub trait Backend: Send`.

The trait module also defines:

```rust
pub trait BackendEventSink: Send + Sync {
    fn deliver_event(&self, ev: BackendEvent);
    fn deliver_fatal(&self, err: BackendFatalError);
}

pub enum BackendEvent {
    Expose { window: WindowHandle, x: i16, y: i16, w: u16, h: u16, count: u16 },
    // … (full list from design doc)
}

pub enum BackendError {
    Protocol { major: u8, minor: u16, code: u8, bad_value: u32 },
    Connection(io::Error),
    HandleKind { expected: HandleKind, got: HandleKind },
}

pub enum BackendFatalError {
    TransportClosed(io::Error),
}
```

Build (`cargo build -p yserver-core`). Expect: success — the trait has
no impls yet, so it's free-standing.

### Task 5.2 — Plan the rename of `HostX11` → `HostX11Backend`

Before doing the actual rename, sanity-check that no consumer outside
`yserver-core` uses `HostX11` directly:

```sh
grep -rn "HostX11\b" crates/
```

The hits should all be inside `yserver-core` (and `yserver`'s binaries
which thread it through). If anything else references it, plan the
update too.

### Task 5.3 — Rename the struct

In `host_x11/mod.rs`:

```rust
// Before:
pub struct HostX11 { ... }
// After:
pub struct HostX11Backend { ... }
```

Run a project-wide rename via `sed` or rust-analyzer:

```sh
grep -rln "HostX11\b" crates/ | xargs sed -i 's/HostX11\b/HostX11Backend/g'
```

(Verify the regex doesn't match anything you don't want — e.g. a
comment containing "HostX11" on its own. `git diff` to confirm.)

Build. Should succeed — pure rename.

### Task 5.4 — Write `impl Backend for HostX11Backend`

In `host_x11/mod.rs` (or a new `host_x11/trait_impl.rs` to keep
`mod.rs` lean), write the impl. For each method:

- Resource creation: delegate to the existing `create_*` methods from
  Step 2 (which already return per-kind handles).
- Drawing: delegate to the existing methods from Step 3 (which already
  take `&DrawState`).
- Extension proxies: delegate to existing methods.
- `register_event_sink`: store the sink in a new field on
  `HostX11Backend` (`event_sink: Option<Arc<dyn BackendEventSink>>`).
  Spawn or signal the existing `HostInputPump` thread to use this sink
  when it has events to deliver.
- `sync`: delegate to existing `sync_main_connection`.
- `fd`: there is **no `fd` method** on the trait per the v3 design.
  Skip.

Build. Most failures will be inside the impl bodies where the existing
`HostX11` method took different parameter types than the trait
declares — adapt as you go.

### Task 5.5 — Wire the event sink into the HostInputPump

In `host_x11/pump.rs`, add a path for the pump thread to invoke
`sink.deliver_event(ev)` when it has translated a host event into a
`BackendEvent`.

The pump receives an `Arc<dyn BackendEventSink + Send + Sync>`
through whatever channel/mutex is appropriate (`Arc<Mutex<…>>` on
`HostX11Backend` already exists today for the equivalent state). The
sink is `Send + Sync`, so the pump can call into it directly without
holding any other lock.

**Per-client kb pumps stay UNTOUCHED in this step.** They keep
writing directly to their client connections. The trait's
`register_event_sink` covers only the main `HostInputPump`'s event
path.

### Task 5.6 — Write the sink impl in `yserver-core`

In `crates/yserver-core/src/backend/sink.rs`, write a struct that
wraps the existing event-fanout machinery (today's
`pointer_event_fanout`, `expose_event_fanout`, etc. — wherever
`HostInputPump` calls into for routing). The sink's `deliver_event`
calls into the existing fanout logic.

```rust
pub struct CoreEventSink {
    // Shared state for the existing fanout — likely an
    // Arc<Mutex<ServerState>> or whatever nested.rs uses today.
    state: Arc<Mutex<ServerState>>,
}

impl BackendEventSink for CoreEventSink {
    fn deliver_event(&self, ev: BackendEvent) {
        let mut state = self.state.lock().unwrap();
        match ev {
            BackendEvent::Expose { window, .. } => state.expose_fanout(window, ...),
            BackendEvent::ButtonPress { window, .. } => state.pointer_fanout(window, ...),
            // … one match arm per BackendEvent variant
        }
    }

    fn deliver_fatal(&self, err: BackendFatalError) {
        log::error!("backend transport closed: {:?}", err);
        // Signal main loop to terminate ynest. Mechanism: set a shared
        // AtomicBool, or use whatever shutdown path nested.rs already has.
    }
}
```

The exact shape depends on what `nested.rs` exposes today as the
event-fanout entry point; adapt accordingly.

### Task 5.7 — Switch `nested.rs` to `Arc<Mutex<dyn Backend>>`

In `nested.rs`, find every `Arc<Mutex<HostX11Backend>>` (or
equivalent typed reference) and change to `Arc<Mutex<dyn Backend>>`.
This is a project-wide grep.

```sh
grep -rn "Arc<Mutex<HostX11Backend>>\|Mutex<HostX11Backend>" crates/
```

For each hit: change the type. The borrow-checker will reject any
code that called methods that aren't on the trait — those methods need
to either be added to the trait or accessed through a downcast (which
should be unnecessary if the trait covers the right surface).

### Task 5.8 — Write `RecordingBackend` test double

`crates/yserver-core/src/backend/recording.rs`:

```rust
//! Test double for the Backend trait. Records every method call;
//! returns synthetic handles. Used to drive nested.rs request handlers
//! against without needing a real X server.

#![cfg(test)]

use super::*;
use std::sync::Mutex;

pub struct RecordingBackend {
    pub calls: Mutex<Vec<RecordedCall>>,
    next_handle: Mutex<u32>,
}

#[derive(Debug)]
pub enum RecordedCall {
    CreateWindow,
    DestroyWindow(WindowHandle),
    MapWindow(WindowHandle),
    UnmapWindow(WindowHandle),
    ConfigureWindow(WindowHandle),
    ChangeWindowAttributes(WindowHandle),
    CreatePixmap,
    FreePixmap(PixmapHandle),
    PutImage,
    CopyArea,
    PolyLine,
    PolyFillRectangle,
    ChangeProperty,
    InternAtom(String),
    Sync,
    // … add a variant per trait method, but keep simple
}

impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next_handle: Mutex::new(0x10000),
        }
    }

    fn allocate_handle(&self) -> u32 {
        let mut n = self.next_handle.lock().unwrap();
        let h = *n;
        *n = n.wrapping_add(1);
        h
    }
}

impl Backend for RecordingBackend {
    fn register_event_sink(&mut self, _sink: Arc<dyn BackendEventSink + Send + Sync>) {
        // No-op for recording.
    }

    fn sync(&mut self) -> io::Result<()> {
        self.calls.lock().unwrap().push(RecordedCall::Sync);
        Ok(())
    }

    fn create_window(&mut self, _params: CreateWindowParams) -> io::Result<WindowHandle> {
        self.calls.lock().unwrap().push(RecordedCall::CreateWindow);
        Ok(WindowHandle::from_raw(self.allocate_handle()).unwrap())
    }

    fn map_window(&mut self, h: WindowHandle) -> io::Result<()> {
        self.calls.lock().unwrap().push(RecordedCall::MapWindow(h));
        Ok(())
    }

    // … one impl per trait method. Most just record + return Ok with
    //   a synthetic handle.
}
```

This is ~150 LoC for ~95 trait methods, but most methods are 1–3 lines.

### Task 5.9 — Write 2–4 `RecordingBackend` integration tests

In a new test module (e.g. `crates/yserver-core/src/nested_tests.rs`
or inside `nested.rs`'s `#[cfg(test)] mod tests`):

```rust
#[test]
fn create_map_change_destroy_window_calls_backend_correctly() {
    let backend = Arc::new(Mutex::new(RecordingBackend::new()));
    let mut server = ServerState::new(backend.clone());

    // Simulate a client sending CreateWindow + MapWindow + ChangeProperty + DestroyWindow.
    let client_id = server.register_test_client();
    let request = encode_create_window(/* args */);
    server.handle_request(client_id, &request).unwrap();
    // … similar for MapWindow, ChangeProperty, DestroyWindow

    let calls = backend.lock().unwrap().calls.lock().unwrap().clone();
    assert!(matches!(calls[0], RecordedCall::CreateWindow));
    assert!(matches!(calls[1], RecordedCall::MapWindow(_)));
    assert!(matches!(calls[2], RecordedCall::ChangeProperty));
    assert!(matches!(calls[3], RecordedCall::DestroyWindow(_)));
}
```

Aim for 2–4 tests covering: (a) basic create+map+destroy flow, (b)
draw operation routes through `&DrawState`, (c) sync is called when
expected.

The exact shape depends on how `nested.rs` exposes its handler entry
points to tests — adapt to existing test infrastructure.

### Task 5.10 — Run the full check suite + manual smoke

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green. Then **manual smoke gate**:

```sh
just ynest &
sleep 1
DISPLAY=:99 xterm    # rendering + input
DISPLAY=:99 wmaker & # WM startup
sleep 1
DISPLAY=:99 xterm    # second xterm under wmaker
```

Both xterms render and accept input; wmaker chrome appears on the
second one.

### Task 5.11 — Commit

```sh
git add crates/yserver-core/src/backend/ \
        crates/yserver-core/src/host_x11/ \
        crates/yserver-core/src/nested.rs \
        crates/yserver-core/src/server.rs
git commit -m "feat: Phase 6.2 Step 5 — carve Backend trait, HostX11Backend impl, RecordingBackend tests"
```

---

## Step 6 — Manual validation pass

**Goal:** Run the full Phase 3.x WM matrix + gtk3-demo end-to-end. Fix
any regressions. Update `docs/status.md`. Squash-merge to master.

**Files:**
- Modify: `docs/status.md` (add Phase 6.2 section under Phase 6)
- Optional: any bugfix commits surfaced by the validation matrix

**Estimate:** Validation work; size depends on what surfaces.

### Task 6.1 — Validate wmaker

```sh
just ynest &
sleep 1
DISPLAY=:99 wmaker &
sleep 2
DISPLAY=:99 xterm &
DISPLAY=:99 xclock &
DISPLAY=:99 xeyes &
```

Acceptance per design doc:
- Chrome + clip + dock + appicons render.
- xterm/xclock open with correct icon graphics in appicons.
- Close button visible on title bar.
- Drag a window and restack — both work.

If any acceptance criterion fails, debug. Most likely root causes:
- A `DrawState` field forgotten in `resolve_draw_state` (Step 3).
- A handle conversion error (Step 1) where a `u32` got compared to a
  `WindowHandle` raw without `.as_raw()`.
- A trait-impl method that delegates incorrectly (Step 5).

### Task 6.2 — Validate fvwm3

```sh
DISPLAY=:99 fvwm3 &
sleep 2
DISPLAY=:99 xclock &
DISPLAY=:99 gtk3-demo &
```

Acceptance:
- Chrome renders.
- Widget clicks activate (Phase 3.7 fix should still work).
- xclock title bar text via RENDER.
- gtk3-demo sidebar nav works.

### Task 6.3 — Validate e16

```sh
DISPLAY=:99 enlightenment-16 &
sleep 3
# Right-click on desktop area
```

Acceptance:
- Top bar + pagers render.
- Right-click popup opens.
- Popup body has theme tile (not solid black).
- Menu-item click on "Settings" opens the Enlightenment Settings dialog
  (Phase 3.7's primary smoke).

### Task 6.4 — Validate openbox

```sh
DISPLAY=:99 openbox &
sleep 2
DISPLAY=:99 xeyes &
DISPLAY=:99 xclock &
```

Acceptance:
- Clients render inside openbox frames.
- Note: openbox frame chrome (title bar text, decorations) is a
  pre-existing gap — not a regression target. Only check that
  *clients* render correctly within whatever frame openbox draws.

### Task 6.5 — Validate gtk3-demo

If not already covered in fvwm3 pass:

```sh
DISPLAY=:99 fvwm3 &
sleep 2
DISPLAY=:99 gtk3-demo &
```

Acceptance:
- Main window + sidebar nav + child dialogs work.
- Sidebar labels rendered.
- Click "Run" on a demo and it opens.

### Task 6.6 — Triage and fix regressions

If any WM or gtk3-demo regressed against the Phase 3.x baseline:

1. Capture the symptom (screenshot if rendering, log lines if logical).
2. Bisect against the Step 1–5 commits:
   ```sh
   git bisect start
   git bisect bad HEAD
   git bisect good <commit-before-step-1>
   ```
3. Fix in the offending step's commit if pre-merge, or as a follow-up
   commit on the branch if post-merge debugging is more tractable.

Per the design's Risk table, the three most likely culprits are:
- Step 1 (handle newtypes) — a missed `.as_raw()` call where a host
  XID is keyed into a `HashMap<u32, ...>`.
- Step 3 (`DrawState`) — a missed field in `resolve_draw_state` that
  was previously read directly from `GcState`.
- Step 5 (trait carve) — an `impl Backend` method that delegates to
  the wrong existing `HostX11Backend` method.

### Task 6.7 — Update `docs/status.md`

Add a new section under "Phase 6 — Standalone DRM/KMS":

```markdown
### Phase 6.2 — Backend trait extraction (complete)

Goal: carve a `Backend` trait out of `yserver-core` so `nested.rs`
becomes one impl (`HostX11Backend`) and a future KMS backend slots
in. Lands three of the five C-prework items from the 6.1 design.
Pump/main connection merge is deferred to its own slice.

Design:
[`2026-05-03-phase6-2-backend-trait-design.md`](superpowers/specs/2026-05-03-phase6-2-backend-trait-design.md).
Plan:
[`2026-05-03-phase6-2-backend-trait.md`](superpowers/plans/2026-05-03-phase6-2-backend-trait.md).

#### Landed (branch `phase6-2-backend-trait`, squash-merged to master)

- [x] **Step 1 — Per-kind handle newtypes (prework #3 + #4).**
      `WindowHandle`, `PixmapHandle`, `PictureHandle`, `GlyphSetHandle`,
      `FontHandle`, `CursorHandle`, `ColormapHandle`, plus `AnyHandle`
      for drawables. All `NonZeroU32` newtypes; `Option<KindHandle>`
      is one word. Replaces 16 `Option<u32>` host-XID slots across
      `Window`, `Pixmap`, `PictureState`, `GlyphSetState`, `Font`,
      `Cursor`, `Colormap`, plus `GcClipState::Pixmap`,
      `GcFillState::Tiled`, `NamedCompositePixmap`. ~380 call sites
      adjusted.
- [x] **Step 2 — Bundle `allocate_xid` into `create_*` (prework #1).**
      `host.create_window(...) -> io::Result<WindowHandle>` (and
      peers) replaces the two-phase `let xid = host.allocate_xid();
      host.create_window(xid, ...)?;` pattern. Cleans up the call
      sites and pre-shapes the trait method signatures.
- [x] **Step 3 — `DrawState` and per-call resolution (prework #2 partial).**
      `DrawState` is the resolved snapshot of GC state for one drawing
      call. `ResourceTable::resolve_draw_state(gc_id) -> DrawState`
      computes it; drawing methods take `&DrawState`. Three of the
      four composite operations from the 6.1 reading list collapse
      into normal calls (`put_image_with_clear`,
      `clear_area_with_bg`, `fill_with_state`). Only
      `list_fonts_proxy` survives as composite.
- [x] **Step 4 — Mechanical module split.** `host_x11.rs` (3,643
      lines) → `host_x11/{mod, request, pump, sync}.rs`. Pure file
      moves.
- [x] **Step 5 — `Backend` trait carve.** Trait defined in
      `crates/yserver-core/src/backend/`. `HostX11Backend` is the sole
      impl. `register_event_sink(Arc<dyn BackendEventSink + Send + Sync>)`
      routes the main `HostInputPump`'s events through the sink.
      Per-client keyboard pumps remain direct-to-client (folding them
      through the sink belongs to the deferred merge slice).
      `RecordingBackend` test double + 2–4 integration tests under
      `#[cfg(test)]`.
- [x] **Step 6 — Validation.** Manual smoke under wmaker, fvwm3, e16,
      openbox + gtk3-demo. All Phase 3.x acceptance criteria met.

#### Phase 6.2 follow-ups (deferred, all moved to the merge slice)

The pump/main connection merge — Phase 3.7's structural fix and
prework item #5 — is its own future slice (Phase 6.2.5 or fold into
Phase 6.3 design). It carries:

- Single-X11-connection merge.
- `fd()` / `dispatch()` / `drain_events()` on the trait.
- Per-client kb pump dissolution.
- 64-bit `seq_full` tracking for X11 16-bit sequence wrap.
- Retention window for late void-request errors.
- `OriginContext` plumbing for async host-error attribution.
- Reply demux / `ReplyMap` rework.
```

### Task 6.8 — Final commit on the branch + squash-merge

```sh
git add docs/status.md
git commit -m "docs: status.md — Phase 6.2 Backend trait extraction landed"
```

Then merge to master:

```sh
git checkout master
git merge --squash phase6-2-backend-trait
git commit -m "feat: Phase 6.2 — Backend trait extraction"
# (Use a multi-line commit message describing the six steps.)
```

Push when satisfied with the squashed history. Per the project memory,
pushing in the bwrap sandbox needs:

```sh
GIT_SSH_COMMAND="ssh -F /home/jos/realhome/Projects/dotfiles/ssh/config -o UserKnownHostsFile=/home/jos/realhome/.ssh/known_hosts" git push
```

---

## Open follow-ups (out of scope; tracked for later)

- **Pump/main connection merge.** Whole own slice. Brings async
  host-error attribution, sequence-wrap handling, per-client kb pump
  dissolution, and the `fd()/dispatch()/drain_events()` reshape of
  the trait. Cited in design doc's "Deferred to a separate phase"
  section.
- **Per-client GC mirroring** (Phase 3.7 follow-up `#940`). Trait
  shape is forward-compatible — a future per-client-GC backend
  caches resolved-state-per-`GcResourceId` internally without
  changing the trait surface.
- **KMS backend.** Phase 6.3+. The `RecordingBackend` test double
  established in Step 5 is the first existence proof that the trait
  is implementable by something other than `HostX11Backend`; the
  KMS backend will be the second.
- **Cross-trait extension drawing methods.** Several RENDER methods
  take GC state today and should follow the `&DrawState` rule once
  identified during Step 3. If any are missed, they'll surface as
  drawing regressions in Step 6 manual smoke.
