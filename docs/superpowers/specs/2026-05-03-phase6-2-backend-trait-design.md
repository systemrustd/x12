# Phase 6.2 — `Backend` trait extraction

Carve a `Backend` trait out of `yserver-core` so `nested.rs` becomes one impl
(`HostX11Backend`) and a future KMS backend in Phase 6.3+ slots in without
further refactoring of `yserver-core`. Land three of the five C-prework items
from the 6.1 design as part of the same change.

This is the first half of **slice C** of the B → C trajectory described in
[`2026-05-02-phase6-bootstrap-design.md`](2026-05-02-phase6-bootstrap-design.md).
The KMS backend itself, and "first X client on KMS," are Phase 6.3+. The
pump/main connection merge (Phase 3.7's structural-fix follow-up) is
explicitly **deferred to its own slice** — see "Deferred to a separate
phase" below.

This doc was reviewed by codex twice before commit; the two-pass review
narrowed scope, sharpened semantics, and removed several would-be design
holes. See "Codex review log" at the end.

## Goal

Prove that `yserver-core` is implementation-agnostic on the
backend-rendering/input boundary, *without* bundling the riskier
pump/main merge:

- `Backend` trait defines the operations `nested.rs` (and other request
  handlers in `server.rs`) call when they need to mutate or query host /
  rendering state.
- `HostX11Backend` is the sole impl after 6.2; behavior under the four
  validated WMs (wmaker, fvwm3, e16, openbox) and gtk3-demo is unchanged.
- `nested.rs` no longer touches raw host XIDs — every cross-layer reference
  is a typed `Backend`-handle.
- The trait is minimal: it does not pretend to abstract event delivery
  topology, does not expose a single fd, does not require synchronous
  attribution of asynchronous host errors. Each of those decisions is
  deferred to a future slice that can give it appropriate design rigor.

## Scope decisions

| Topic | Choice | Reason |
|---|---|---|
| Slice boundary | Prework #1 / #3 / #4 + trait extraction; **no KMS backend, no pump/main merge, no async-error attribution** | Trait shape with one impl already bakes in some HostX11 assumptions; we accept that risk because KMS isn't ready. The merge was found to introduce non-trivial new design holes (event wakeup, async error attribution, sequence wrap) and deserves its own design doc. |
| Threading model | `Arc<Mutex<dyn Backend>>`; **mutex survives** | Client topology stays thread-per-client. Same shape as today's `Arc<Mutex<HostX11>>`. Per-call `host.lock()` discipline preserved. |
| Pump connection | **Stays separate.** `HostX11Backend` internally retains today's `HostInputPump` thread on its own host connection. | Out of scope. Phase 3.7's structural fix is its own slice; this slice does the trait-extraction prework. |
| Per-client kb pumps | **Stay, and not routed through the new sink.** Each client thread retains its own host connection for kb events; writes directly to its client, as today. | Same client_id / focused-window / writer / sequence-state coupling as today. Folding them through `BackendEventSink` would require a per-event source-client/target-writer channel that the trait surface deliberately doesn't carry; that's deferred-merge work. |
| `sync_main_connection` | **Stays internal to `HostX11Backend`.** Surfaced as `Backend::sync()` for callers that need an explicit fence. | The fence still has Phase 3.6/3.7-era reasons to fire (race between main `CreateWindow` and pump `ChangeWindowAttributes`). |
| Host-handle allocation | **Bundle** `allocate_xid` into `create_*` (single trait method per resource); **not "atomic" in the X11-error sense** | The bundling cleans up the call sites and the trait surface. We do *not* claim that an Ok return means the host accepted the create — X11 errors are async. |
| Handle representation | **Per-kind newtypes** (`WindowHandle`, `PixmapHandle`, …) | Compile-time kind correctness during a refactor that touches 16 slot definitions and ~380 call sites. |
| GC shape | **By-borrow resolved `&DrawState` per drawing call**; no `GcHandle` on the trait | Today's HostX11 uses a shared host GC + per-client `GcState` resolved on every draw via `apply_gc_clip`. Per-client real host GCs are a deferred Phase 3.7 follow-up. The trait should not pretend per-client GCs exist; instead, every drawing method takes `&DrawState`. KMS-natural: it gets the same resolved snapshot and rasterizes directly. `&DrawState` (not owned) avoids a per-call clone; HostX11Backend caches the most-recently-applied `DrawState` for incremental updates. |
| Event delivery | `register_event_sink(Arc<dyn BackendEventSink + Send + Sync>)` called once at startup; backend internally calls into it from whichever thread it manages | Backend keeps full control of its event topology. HostX11Backend's existing pump thread feeds the sink. Future KMS backend feeds from libinput. No fd / dispatch / drain on the trait. |
| Composite trait methods | Only `list_fonts_proxy` survives; the other three from the 6.1 reading list collapse into normal drawing methods now that `&DrawState` is the calling convention | Honest about what's a leaf vs a sequence. `put_image_with_clear`, `clear_area_with_bg`, `fill_with_state` collapse because their state lives in `DrawState`. |
| Validation bar | Manual smoke under wmaker, fvwm3, e16, openbox + gtk3-demo, plus 174 existing tests + new ones | Aggressive refactor needs an integration safety net. |
| Landing | Squashed feature branch `phase6-2-backend-trait` matching Phase 6.1's pattern | Project rhythm. |

## Out of scope

Explicitly deferred:

- **Pump/main connection merge.** Phase 3.7's structural fix lives in its
  own future slice — call it Phase 6.2.5 or fold into Phase 6.3 design
  work. The merge needs explicit treatment of: event-queue wakeup
  topology when there's no central epoll loop, async host-error
  attribution when sync round-tripping disappears, X11 16-bit sequence
  wrap with retention windows for late void-request errors. None of
  these are in 6.2.
- KMS backend. `Backend` trait carved with one impl.
- Per-client GC mirroring (Phase 3.7 follow-up `#940`).
- Single-thread epoll rewrite of client topology.
- Command-channel marshalling of backend calls onto an owner thread.
- xkbcommon. KMS-time, not now.
- Hotplug, multi-output, GBM/EGL, logind/VT switching, bare-metal
  validation.
- Any X11 protocol opcode work. This is pure refactor.

## Architecture

### Module layout (`crates/yserver-core/src/`)

```
backend/
  mod.rs           — pub trait Backend, BackendEventSink, BackendEvent, BackendError
  handles.rs       — WindowHandle, PixmapHandle, …, AnyHandle
  params.rs        — CreateWindowParams, ConfigureParams, ChangeAttrsParams,
                     DrawState, …
host_x11/          (renamed from host_x11.rs — the file is overdue for splitting)
  mod.rs           — pub struct HostX11Backend; impl Backend for HostX11Backend
  request.rs       — request-side methods (the bulk of today's host_x11.rs)
  pump.rs          — internal HostInputPump thread (unchanged behavior)
  sync.rs          — sync_main_connection internals (now Backend::sync)
nested.rs          — same file; bodies updated to call self.backend.lock().*
                     through Arc<Mutex<dyn Backend>>
server.rs          — same file; same change shape
resources.rs       — Window/Pixmap/etc. structs; host_xid fields re-typed to
                     per-kind handles
```

Note: the `host_x11.rs` → `host_x11/` module split is its own
mechanical step (Step 4) separate from any logic changes — pure file
moves.

### Trait surface

```rust
pub trait Backend: Send {
    // --- Lifecycle / event integration ---
    fn register_event_sink(&mut self, sink: Arc<dyn BackendEventSink + Send + Sync>);
    /// Called once at startup. Backend internally arranges to invoke
    /// sink.deliver_event(...) from whichever thread it manages
    /// (HostX11Backend's pump thread; KMS backend's libinput epoll loop).
    /// Implementations must not call deliver_event from within other
    /// trait methods that hold the outer Mutex<Backend> — sink delivery
    /// happens on the backend's own threading.

    fn sync(&mut self) -> io::Result<()>;
    /// Round-trip GetInputFocus on the main connection (HostX11Backend);
    /// no-op on backends without a host (KMS). Used as an explicit fence
    /// where the existing Phase 3.6/3.7 code calls sync_main_connection.

    // --- Resource creation (allocate_xid bundled, but NOT atomic in the
    //     X11-error sense — Ok return only means bytes were sent on the
    //     main connection. Today's synchronous round-trip discipline is
    //     preserved: host protocol errors arrive on the call stack of
    //     the originating handler via the existing main-connection reply
    //     demux, NOT via the sink. The merge slice will revisit this.) ---
    fn create_window(&mut self, params: CreateWindowParams) -> io::Result<WindowHandle>;
    fn create_pixmap(&mut self, depth: u8, w: u16, h: u16, drawable: AnyHandle)
        -> io::Result<PixmapHandle>;
    fn create_picture(&mut self, params: CreatePictureParams) -> io::Result<PictureHandle>;
    fn create_glyphset(&mut self, format: PictureFormatId) -> io::Result<GlyphSetHandle>;
    fn create_cursor(&mut self, params: CreateCursorParams) -> io::Result<CursorHandle>;
    fn open_font(&mut self, name: &str) -> io::Result<(FontHandle, FontMetrics)>;
    fn create_colormap(&mut self, params: CreateColormapParams) -> io::Result<ColormapHandle>;

    // --- Resource lifecycle (per-kind) ---
    fn destroy_window(&mut self, h: WindowHandle) -> io::Result<()>;
    fn free_pixmap(&mut self, h: PixmapHandle) -> io::Result<()>;
    fn free_picture(&mut self, h: PictureHandle) -> io::Result<()>;
    fn free_glyphset(&mut self, h: GlyphSetHandle) -> io::Result<()>;
    fn free_cursor(&mut self, h: CursorHandle) -> io::Result<()>;
    fn close_font(&mut self, h: FontHandle) -> io::Result<()>;
    fn free_colormap(&mut self, h: ColormapHandle) -> io::Result<()>;

    // --- Window ops ---
    fn map_window(&mut self, h: WindowHandle) -> io::Result<()>;
    fn unmap_window(&mut self, h: WindowHandle) -> io::Result<()>;
    fn configure_window(&mut self, h: WindowHandle, params: ConfigureParams) -> io::Result<()>;
    fn reparent_window(&mut self, child: WindowHandle, parent: WindowHandle,
        x: i16, y: i16) -> io::Result<()>;
    fn change_window_attributes(&mut self, h: WindowHandle, params: ChangeAttrsParams)
        -> io::Result<()>;

    // --- Drawing — every method takes resolved &DrawState (no GcHandle) ---
    fn put_image(&mut self, dst: AnyHandle, state: &DrawState, img: ImageData)
        -> io::Result<()>;
    fn copy_area(&mut self, src: AnyHandle, dst: AnyHandle, state: &DrawState,
        params: CopyAreaParams) -> io::Result<()>;
    fn copy_plane(&mut self, src: AnyHandle, dst: AnyHandle, state: &DrawState,
        params: CopyPlaneParams) -> io::Result<()>;
    fn poly_line(&mut self, dst: AnyHandle, state: &DrawState, points: &[Point])
        -> io::Result<()>;
    fn poly_segment(&mut self, dst: AnyHandle, state: &DrawState, segs: &[Segment])
        -> io::Result<()>;
    fn poly_rectangle(&mut self, dst: AnyHandle, state: &DrawState, rects: &[Rect])
        -> io::Result<()>;
    fn poly_arc(&mut self, dst: AnyHandle, state: &DrawState, arcs: &[Arc_])
        -> io::Result<()>;
    fn poly_fill_rectangle(&mut self, dst: AnyHandle, state: &DrawState, rects: &[Rect])
        -> io::Result<()>;
    fn poly_fill_arc(&mut self, dst: AnyHandle, state: &DrawState, arcs: &[Arc_])
        -> io::Result<()>;
    fn fill_poly(&mut self, dst: AnyHandle, state: &DrawState, params: FillPolyParams)
        -> io::Result<()>;
    fn poly_text8(&mut self, dst: AnyHandle, state: &DrawState, params: PolyTextParams<u8>)
        -> io::Result<()>;
    fn poly_text16(&mut self, dst: AnyHandle, state: &DrawState, params: PolyTextParams<u16>)
        -> io::Result<()>;
    fn image_text8(&mut self, dst: AnyHandle, state: &DrawState, params: ImageTextParams<u8>)
        -> io::Result<()>;
    fn image_text16(&mut self, dst: AnyHandle, state: &DrawState, params: ImageTextParams<u16>)
        -> io::Result<()>;
    fn poly_point(&mut self, dst: AnyHandle, state: &DrawState, points: &[Point])
        -> io::Result<()>;
    fn clear_area(&mut self, win: WindowHandle, area: Rect, bg: BgState) -> io::Result<()>;

    // --- Composite (genuinely multi-call / streaming) ---
    fn list_fonts_proxy(&mut self, pattern: &str, max: u16,
        sink: &mut dyn FontReplySink) -> io::Result<()>;

    // --- Atoms / properties ---
    fn intern_atom(&mut self, name: &str, only_if_exists: bool) -> io::Result<u32>;
    fn get_atom_name(&mut self, atom: u32) -> io::Result<Option<String>>;

    // --- Extension proxies (one method per minor opcode that needs to round-trip) ---
    fn render_query_pict_formats(&mut self) -> io::Result<RenderPictFormatsReply>;
    // … one method per extension request that the existing host_x11.rs forwards;
    //   roughly: ~30 RENDER, ~10 SHAPE, ~6 DAMAGE, ~4 COMPOSITE, ~10 SYNC,
    //   ~6 PRESENT, ~10 XFIXES, ~12 RANDR, "any minor" XKB proxy, ~8 XI2,
    //   plus MIT-SHM file-descriptor passing.
    //
    // Extension drawing methods that take GC state (e.g. RENDER Composite,
    // RENDER FillRectangles, etc.) follow the same &DrawState rule
    // as the core drawing methods.

    // --- Misc ---
    fn warp_pointer(&mut self, params: WarpPointerParams) -> io::Result<()>;
    fn query_pointer(&mut self, win: WindowHandle) -> io::Result<QueryPointerReply>;
    fn translate_coordinates(&mut self, src: WindowHandle, dst: WindowHandle,
        x: i16, y: i16) -> io::Result<TranslateCoordsReply>;
    fn get_geometry(&mut self, h: AnyHandle) -> io::Result<GetGeometryReply>;
}
```

Total: ~95 trait methods.

### `DrawState` shape

`DrawState` is the snapshot of GC state needed to execute one drawing call.
It is *resolved* by `resources.rs` from the client's `GcState` immediately
before each backend drawing call. Cheap to construct — most fields are
small.

```rust
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
    pub clip: ClipState,         // None | Rectangles(Vec<Rect>) | Pixmap(PixmapHandle)
    pub clip_origin: (i16, i16),
    pub fill: FillState,         // Solid | Tiled { pixmap: PixmapHandle, origin: (i16, i16) }
                                 //       | Stippled { … } | OpaqueStippled { … }
    pub subwindow_mode: SubwindowMode,
    pub graphics_exposures: bool,
    pub dashes: Vec<u8>,
    pub dash_offset: i16,
    pub arc_mode: ArcMode,
}
```

Drawing trait methods take `&DrawState` (borrowed, not owned). The
HostX11Backend implementation caches the most-recently-applied
`DrawState` per host depth and only re-pushes diff'd fields. This
preserves today's apply_gc_clip-style optimization. KMS impl uses the
borrowed reference directly during rasterization.

### Companion types

```rust
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct WindowHandle(NonZeroU32);
// … plus PixmapHandle, PictureHandle, GlyphSetHandle, FontHandle,
//   CursorHandle, ColormapHandle. All NonZeroU32 newtypes (zero is reserved
//   per X11 — None mask values stay representable as Option<…Handle>).

pub enum AnyHandle {
    Window(WindowHandle),
    Pixmap(PixmapHandle),
}

impl AnyHandle {
    pub fn kind(self) -> HandleKind { … }
}

pub trait BackendEventSink: Send + Sync {
    fn deliver_event(&self, ev: BackendEvent);
    /// Fatal connection-level error from the backend (e.g. host
    /// connection drops). The backend will not deliver any further
    /// events after this; the main loop should treat this as terminal.
    /// Per-client-attributable host *protocol* errors do NOT come
    /// through here in 6.2 — they are returned synchronously by the
    /// trait method that triggered them, preserving today's call-stack
    /// attribution. See "Deferred to a separate phase" for why.
    fn deliver_fatal(&self, err: BackendFatalError);
}

pub enum BackendEvent {
    Expose { window: WindowHandle, x: i16, y: i16, w: u16, h: u16, count: u16 },
    ConfigureNotify { window: WindowHandle, x: i16, y: i16, w: u16, h: u16, … },
    MapNotify { window: WindowHandle, override_redirect: bool },
    UnmapNotify { window: WindowHandle, from_configure: bool },
    KeyPress { window: WindowHandle, keycode: u8, state: u16, root_x: i16, root_y: i16, … },
    KeyRelease { … },
    ButtonPress { … },
    ButtonRelease { … },
    MotionNotify { … },
    EnterNotify { … }, LeaveNotify { … },
    FocusIn { … }, FocusOut { … },
    PropertyNotify { … },
    SelectionRequest { … }, SelectionClear { … }, SelectionNotify { … },
    // Extension events: DamageNotify, RRScreenChange, etc.
}

pub enum BackendError {
    /// Synchronous protocol error returned by the trait method that
    /// triggered it. Carries no originating-client context because the
    /// caller (the client request handler) already has it on the stack.
    Protocol { major: u8, minor: u16, code: u8, bad_value: u32 },
    /// Connection IO error reaching the host.
    Connection(io::Error),
    /// Server bug — wrong handle kind passed to a method.
    HandleKind { expected: HandleKind, got: HandleKind },
}

pub enum BackendFatalError {
    /// Backend's underlying transport is gone; no further events will
    /// arrive via `deliver_event`. Examples: host X11 connection
    /// dropped; KMS DRM device gone.
    TransportClosed(io::Error),
}
```

Note: `BackendError::Protocol` does *not* carry an originating client id
or nested sequence number. Today's `host_x11.rs` returns errors
synchronously to the calling request handler (which knows the originating
client because it *is* the client thread); after 6.2 this is preserved
because the merge is deferred. Async-error attribution becomes a real
problem at merge time and will be addressed in that future slice.

### Identity ownership

Three layers of identity for every resource:

| Layer | Owner | Key | Value |
|---|---|---|---|
| Wire | client | client-allocated XID (`u32`) | client-side-only |
| Core | `yserver-core` | `ResourceId` | `WindowHandle` (and other handle kinds) |
| Backend | `HostX11Backend` | host XID (`u32`) | `WindowHandle` |

`BackendEvent::Foo` carries `WindowHandle` (Layer 2/3). `yserver-core`
maintains a `HashMap<WindowHandle, ResourceId>` (the inverse of resource
record's `host_xid` field, kept alongside) for event routing.
`HostX11Backend` maintains its own `HashMap<u32, WindowHandle>` for
host-event translation. The two maps don't collide; each is private to
its layer.

KMS backend has no equivalent of the host-XID map; the `WindowHandle` is
the native identity directly.

### Concurrency model

Unchanged from today, modulo renames:

- `nested.rs` and `server.rs` hold `Arc<Mutex<dyn Backend>>` (was
  `Arc<Mutex<HostX11>>`).
- Client threads acquire the lock to call `Backend` methods.
- `HostX11Backend` internally retains today's `HostInputPump` thread and
  per-client kb pump connections — the merge is deferred.
- `HostX11Backend::register_event_sink` is called once at startup. The
  *main* `HostInputPump` thread translates raw host events from its host
  connection into `BackendEvent` and calls `sink.deliver_event(...)`
  directly. The sink is `Send + Sync`, so this is thread-safe without
  acquiring the `Mutex<Backend>`.
- **Per-client kb pumps stay as today.** They are NOT routed through
  the new sink in 6.2: each kb pump owns its host connection, knows its
  client_id, focused window, writer, and sequence state, and writes
  directly to its client (`nested.rs:910-955`). Folding them into a
  generic sink would require a per-event source-client and target-writer
  channel that the current `BackendEvent::KeyPress` shape does not carry,
  and that work belongs in the deferred merge slice. The trait
  extraction simply does not touch this code path; per-client kb pumps
  remain a `HostX11Backend` implementation detail invisible to the
  trait.
- `sync_main_connection` survives behind the trait as
  `Backend::sync()`. Phase 3.7 already lives with the cross-connection
  fence; 6.2 doesn't make it worse.

## Error handling

Same as today. Synchronous request handlers receive `BackendError` from
backend method calls and convert to client X11 errors via the existing
`nested.rs` error-encoding path. The conversion path is unchanged —
`BackendError::Protocol` carries the same fields the current ad-hoc
structs do, minus the originating-client context (preserved on the call
stack in the synchronous case).

`BackendError::HandleKind` (drawable kind mismatch — `PixmapHandle`
where `WindowHandle` is required, etc.) is a server bug. Panic in debug;
log + `BadDrawable`-equivalent in release.

Panic policy: invariants in `yserver-core` panic; protocol violations
from clients return errors; host failures bubble up.

## Landing sequence

Squashed feature branch `phase6-2-backend-trait`. Step ordering chosen
so each commit keeps `cargo test` + manual smoke green.

### Step 1 — Per-kind handle newtypes (prework #3 + #4)

Define `WindowHandle`, `PixmapHandle`, etc. in a new `backend::handles`
module. Replace `Option<u32>` slots in `Window`, `Pixmap`,
`PictureState`, `GlyphSetState`, `Font`, `Cursor`, `Colormap`, `GcState`,
plus the GC-state enums (`GcClipState::Pixmap.host_pixmap`,
`GcFillState::Tiled.host_pixmap`) and `NamedCompositePixmap`, all in
lockstep. Pure type churn; no behavior change. Compiler-driven.

`Option<WindowHandle>` survives; `WindowHandle` itself is `NonZeroU32`,
so the optional version is one word.

Per-kind `from_raw_for_test(u32) -> Self` constructors under
`#[cfg(test)]` so existing assertions adapt mechanically.

**Estimated touch:** ~10 files, ~400 LoC, ~380 call sites adjusted
mechanically.

### Step 2 — Bundle `allocate_xid` into `create_*` (prework #1)

Fold `allocate_xid` into `create_*` methods on `HostX11`. Audit and
reshape every `let xid = allocate_xid(); pub_field = xid; create_*(xid,
...)` call site to `let h = host.create_*(...)?; pub_field = h;`. Still
no trait yet — `HostX11` keeps its concrete type.

Naming: not "atomic" — the bundling is mechanical (cleans the call sites
and the future trait surface). Async X11 errors still arrive separately.

**Estimated touch:** ~5 files, ~300 LoC, ~50 call sites.

### Step 3 — `DrawState` and per-call resolution (prework #2 partial)

Define `DrawState` in `backend::params`. Refactor `apply_gc_clip` and
peers in `host_x11.rs` to read from a `&DrawState` parameter. Refactor
`resources.rs` to expose `resolve_draw_state(&self, gc_id: ResourceId)
-> DrawState`. Update every drawing call site in `nested.rs` to resolve
once and pass `&DrawState`.

The four composite call sequences from the 6.1 reading list collapse:
- `put_image_with_clear`: clip-cleared `DrawState` is passed; normal
  `put_image` call.
- `clear_area_with_bg`: `clear_area(win, area, BgState)` is the leaf
  shape; bg lookup happens in `resources.rs` before the call.
- `fill_with_state`: `DrawState.fill = Tiled{…}`; normal
  `poly_fill_rectangle`.
- `list_fonts_proxy`: still composite (multi-reply streaming).

**Estimated touch:** ~3 files, ~400 LoC.

### Step 4 — Mechanical module split

Split `host_x11.rs` (3,643 lines) into `host_x11/{mod, request, pump,
sync}.rs`. Pure file moves + `pub` adjustments; no behavior change.
Lands separately from the trait carve so the diff for Step 5 stays
focused on logic.

**Estimated touch:** 1 file becomes 4; ~0 LoC churn (pure moves).

### Step 5 — Carve the `Backend` trait

Extract trait definition, rename `HostX11` → `HostX11Backend`, `impl
Backend for HostX11Backend`. Adapt `nested.rs` and `server.rs` to hold
`Arc<Mutex<dyn Backend>>` (renamed from `Arc<Mutex<HostX11>>`).

Add `register_event_sink` on the trait; route only HostX11Backend's
*main* `HostInputPump` thread through the sink. The sink impl in
`yserver-core` is a thin wrapper over the existing event-fanout
mechanism (today's pointer_event_fanout, expose_event_fanout, etc.) —
trait-extraction adapts the entry points but does not change the
fanout logic.

**Per-client keyboard pumps are NOT routed through the sink in this
step.** They remain as today, owning their per-client host connections
and writing directly to their client. This preserves their existing
client_id / focused-window / writer / sequence-state coupling that the
`BackendEvent` shape can't carry. Folding them into the sink belongs to
the deferred merge slice; this slice leaves them untouched as a
`HostX11Backend` implementation detail.

Add the recording test double (`RecordingBackend`) under `#[cfg(test)]`
in `backend/mod.rs`. Use it in 2–4 new tests that drive request handlers
through `nested.rs` and assert call sequences. Existence proof that the
trait is implementable by something other than `HostX11Backend`.

**Estimated touch:** ~6 files, ~400 LoC + ~150 LoC for `RecordingBackend`.

### Step 6 — Manual validation pass

Run all four WMs + gtk3-demo end to end. Fix anything that surfaces.
Update `docs/status.md` Phase 6.2 section.

### Estimated step sizes

| Step | Files touched | LoC churn | Risk |
|---|---|---|---|
| 1 | ~10 | ~400 | Low (compiler-driven) |
| 2 | ~5 | ~300 | Low–medium |
| 3 | ~3 | ~400 | Medium |
| 4 | 1 → 4 | ~0 (moves) | Low |
| 5 | ~6 | ~400 + ~150 (test) | Medium |
| 6 | docs only + bug fixes | varies | Validation |

Total ~1,500 LoC churn across `yserver-core`. Significantly smaller than
the original v2 design because the merge (~700 LoC + ~10 unit tests +
the riskiest manual smoke gate) is deferred.

## Testing

### Existing test inventory

`yserver-core` has 174 unit tests. Most exercise `ResourceTable` and the
request handlers' state-machine pieces directly; relatively few touch
`host_x11`. They keep passing through Steps 1–5 with mechanical
adaptations.

### Per-step test work

| Step | Test changes |
|---|---|
| 1 | `from_raw_for_test` constructors; existing assertions adapt mechanically |
| 2 | Update tests that mocked `allocate_xid` separately; they now mock `create_*` |
| 3 | +6 unit tests for `DrawState` resolution (with each fill / clip / function variant) |
| 4 | None (pure file moves) |
| 5 | +2–4 unit tests using `RecordingBackend` to drive request handlers through `nested.rs` (`CreateWindow + MapWindow + ChangeProperty + DestroyWindow` minimum) |
| 6 | None |

### Manual smoke (success bar)

Per-step manual gate:

| Step | Manual gate |
|---|---|
| 1 | None — pure type churn |
| 2 | xterm under no WM — sanity check that create-side hasn't broken |
| 3 | xterm under one WM (wmaker) — sanity check `DrawState` resolution call sites |
| 4 | None — pure file moves |
| 5 | xterm under no WM + xterm under wmaker — sanity check the trait indirection |
| 6 | **Full validation matrix** — wmaker + fvwm3 + e16 + openbox + gtk3-demo |

### Validation script

```sh
just ynest                           # background: ynest on :99
DISPLAY=:99 wmaker &                 # repeat for fvwm3 / e16 / openbox
DISPLAY=:99 xterm                    # type, drag, scroll
DISPLAY=:99 xclock
DISPLAY=:99 xeyes
DISPLAY=:99 gtk3-demo                # toolkit smoke
```

### Acceptance per WM

Matches the existing Phase 3.x validated behavior.

- **wmaker:** chrome + clip + dock + appicons render; xterm/xclock open;
  appicon icon graphics correct; close button visible; drag/restack
  works.
- **fvwm3:** chrome renders; widget clicks activate (the Phase 3.7 fix);
  xclock title-bar text via RENDER; gtk3-demo sidebar nav.
- **e16:** top bar + pagers render; right-click popup opens; popup body
  has theme tile; menu-item click opens Settings dialog.
- **openbox:** clients render inside openbox frames (frame chrome itself
  is a known pre-existing gap; not a regression target).
- **gtk3-demo:** main window + sidebar nav + child dialogs work; sidebar
  labels rendered.

### Out-of-scope test work

- No new `host_x11` integration tests against a real host X server.
- No fuzzing / property tests for the trait.
- No KMS-backend test. Phase 6.3+.
- No tests for ReplyMap wrap, partial reads, async error attribution,
  pump merge mechanics — all deferred with the merge.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Trait shape with one impl bakes in HostX11 assumptions | `RecordingBackend` test double in Step 5 surfaces the worst offenders. `&DrawState` (not owned) and `register_event_sink` (no fd/dispatch on the trait) are explicit choices to avoid known traps. |
| Per-kind newtype churn breaks an unobvious call site | Compiler walks every site; type system enforces correctness. |
| `&DrawState` resolution per-call is slower than today's per-GC cached state | HostX11Backend caches the most-recently-applied `DrawState` per host depth and only re-pushes diff'd fields. Benchmark in Step 3 if perf-sensitive call sites surface. |
| `Arc<Mutex<dyn Backend>>` perf regression vs `Arc<Mutex<HostX11>>` | Today's `Arc<Mutex<>>` already has indirection cost; trait-object indirection should be wash. Switch to `<B: Backend>` generic if measurable difference. |
| Async X11 error attribution still breaks if the merge happens later | The merge is its own slice with its own design doc. 6.2 does not change synchronous attribution. |
| Codex finds more design holes on a third review pass | Acceptable: 6.2's reduced scope means fewer surfaces for new holes. The big surfaces (event topology, async errors, sequence wrap) are explicitly deferred. |

## Deferred to a separate phase

Everything below moves to a future "pump/main merge" slice (call it
Phase 6.2.5 or fold into 6.3 design):

- Single-X11-connection pump/main merge.
- `fd()` / `dispatch()` / `drain_events()` on the trait.
- Per-client kb pump dissolution.
- 64-bit `seq_full` tracking for X11 16-bit sequence wrap.
- Retention window for late void-request errors.
- `OriginContext` plumbing for async host-error attribution.
- Reply demux / `ReplyMap` rework.
- The structural fix Phase 3.7 punted on.

That slice's design will need to handle all of those *together* —
deferring just one of them creates the same hole-pattern that codex
surfaced in this doc's first two passes. Good engineering: don't bundle
the merge with prework; don't bundle the merge with KMS; give it its
own design doc.

## Codex review log

This doc went through two passes of codex review (gpt-5.5, 2026-05-03):

**Pass 1 (against the original draft)** found nine issues:
1. Blocking methods had no event-sink path.
2. `&mut self` on the trait conflicted with thread-per-client.
3. `ReplyMap` keyed by `u16` was unsafe across X11 sequence wrap.
4. "Atomic create" claim was wrong — X11 errors are async.
5. GC trait shape (`create_gc`/`change_gc`/etc.) assumed per-client GCs.
6. Composite methods were insufficient without GC mirroring.
7. Event identity ownership was ambiguous.
8. `fd()` baked in a narrower event model than KMS naturally has.
9. Step 4 mixed two risky changes (file split + merge logic).

**Pass 2 (against the v2 revision)** found five more, three of which
revealed that the merge was load-bearing on more than codex's pass-1
issues admitted:

1. Event delivery had no real owner / wakeup path — the doc described a
   "main thread epoll loop" that doesn't exist in current ynest topology.
2. The `ReplyMap` wrap rule contradicted its own stated late-error test.
3. The `create_window` example reintroduced "drains until matching reply"
   for a void request.
4. Async host-error attribution was under-specified — bundled creates
   plus async errors required mapping host `seq_full` back to nested
   client/seq/opcode, which the trait surface didn't carry.
5. `DrawState` ownership was inconsistent (text said by-value, signatures
   used `&DrawState`).

**Pass 2 verdict drove the de-scope.** Issues 1, 2, 4 were all rooted in
the pump/main merge attempting to live inside the trait-extraction
slice. The merge is a real Phase 3.7 follow-up that deserves its own
design doc; bundling it with prework was making the design fragile.
This doc (v3) defers the merge entirely. Pass-2's #3 and #5 were
mechanical fixes folded into v3.

**Pass 3 (against v3, post-de-scope)** found two further leakages where
the merge's complexity had snuck back in despite the de-scope:

1. The doc said per-client kb pumps would route through the new sink,
   which both contradicted the explicit per-client-kb-pump-dissolution
   deferral and required a `BackendEvent` shape that carried source
   client / target writer / sequence state — work that belongs in the
   merge slice. v3 fix: per-client kb pumps stay outside the sink in
   6.2; they remain a `HostX11Backend` implementation detail invisible
   to the trait. Documented in the scope table and in Step 5.
2. `BackendEventSink::deliver_error` was promising async delivery of
   per-client-attributable host *protocol* errors via the sink, even
   though `OriginContext` plumbing for attribution was deferred. v3
   fix: split errors into `BackendError` (synchronous, returned by the
   trait method that triggered them — preserves today's call-stack
   attribution) and `BackendFatalError` (delivered via
   `deliver_fatal` for transport-level failures only — no
   attribution needed because it's terminal). Documented in the
   trait surface, the sink trait, and the error-handling section.

The remaining v3 design surface — handles, `DrawState`, trait
extraction (excluding kb pump fold-in and async errors), validation —
is what survives. It is a clean, small, mechanical refactor with
well-understood risks. Phase 3.7's structural fix lives on, untouched
and waiting, for its own slice.
