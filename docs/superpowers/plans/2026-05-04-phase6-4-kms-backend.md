# Phase 6.4 — KMS backend + `yserver` integration

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a `KmsBackend` impl of the `Backend` trait and wire `yserver-core` into the `yserver` binary. First real X client (xterm, xeyes) running on bare DRM/KMS — the headline deliverable of the Phase 6 trajectory.

**Branch:** `phase6-4-kms-backend`

**Architecture:** Per-window Pixman images composited to a shared scanout buffer. `yserver-core` (`nested.rs`) is backend-agnostic — no changes needed to request handlers. `ynest` passes a `HostX11Backend`; `yserver` passes a `KmsBackend`.

```
yserver binary
  -> KmsBackend::open()              // DRM + libinput + swapchain + xkbcommon
  -> ServerState::with_geometry()    // shared state (from yserver-core)
  -> backend.set_event_sink(sink)    // event routing
  -> Unix socket listener on :7      // /tmp/.X11-unix/X7 (review #14)
  -> epoll loop [unix_listener, drm_fd, libinput_fd, signalfd]
     -> accept client → spawn handle_client thread (nested.rs)
     -> drm page-flip → composite_all → submit next flip
     -> libinput → xkbcommon → HostKeyEvent → sink → key subscribers
     -> signalfd → clean shutdown
```

**Tech Stack:** Rust 2024, Pixman 0.2.1 (rasterization), freetype (font loading), xkbcommon 0.9 (keycode→keysym). Reuse Phase 6.1's `drm::{Device, Buffer, Swapchain}`, `modeset`, `page_flip`, and `input::Context`.

## Status

Not started. Eight steps pending.

### Review notes (pre-implementation)

The plan has been reviewed twice. The following critical fixes have been incorporated:

#### First review (15 findings)

1. **`scanout_image` removed from struct** — mmap'd buffer lifetime doesn't support `'static`. Create scanout Pixman image per-flip in the page-flip handler, use it, drop it.
2. **`pixman::Image` `!Send`/`!Sync`** — wrapped in `PixmanImage` newtype with `unsafe impl Send` (Mutex serializes access; main thread owns scanout).
3. **Pixman API verification** — `from_slice_mut` may not exist in 0.2.1. Verify with `cargo doc -p pixman` before Step 0; likely `Image::from_raw_mut` or `Image::new` are the real constructors.
4. **`HostKeyEvent` corrected** — actual struct (pump.rs:183) has: `pressed: bool`, `keycode: u8`, `time: u32`, `root_x: i16`, `root_y: i16`, `event_x: i16`, `event_y: i16`, `state: u16`. **No `keysym` field.** X11 clients do their own keysym lookup; drop keysym computation entirely.
5. **Expose synthesis added to Step 3** — required for xeyes/xterm to draw initial content. Synthesize on MapWindow, ConfigureWindow resize, and when unmapped/destroyed windows uncover regions.
6. **Composite child→parent, not child→scanout** — Step 3.2 composites children into parent's image (natural clipping), then top-levels to scanout.
7. **XLFD → freetype fallback** — xterm calls `OpenFont("-misc-fixed-medium-r-...")` which isn't a filesystem path. Hardcoded fallback: any XLFD pattern → DejaVu Sans Mono 12pt.
8. **CharInfo cache** — populate font metrics from freetype that match what image_text8/poly_text8 actually rasterize, or xterm mis-positions characters.
9. **Modifier serialization** — query each named mod explicitly: `xkb_state_mod_name_is_active(state, "Shift", XKB_STATE_MODS_EFFECTIVE)` etc. Don't assume xkb mod-index ordering matches X11 bit ordering.
10. **Keymap construction** — use explicit names `("evdev", "pc105", "us")` as primary, empty-string defaults as fallback. vng minimal image may lack XKB config files.
11. **Button mapping table** — libinput BTN_LEFT=0x110 etc. → X11 1=left, 2=middle, 3=right, 4/5=scroll. Explicit mapping added to Step 4.
12. **Step 5 budget increased** — ~350–400 LoC for freetype + glyph compositing + CharInfo cache + XLFD fallback (was underestimated at 200).
13. **xeyes SHAPE deferred** — xeyes uses SHAPE for non-rectangular windows; deferred to 6.5+. Gate: "pupils track cursor, accept rectangular chrome ugliness."
14. **Display number** — `DISPLAY=:7` to avoid colliding with host X server on `:0`.
15. **`xid_map()` returns same `Arc`** — the sink holds a clone for lookups; must be the same `Arc` every call.

#### Second review (pre-implementation fixes)

- **Fix #4 (v1 carry-over): HostKeyEvent.keycode** — Changed to `(evdev_keycode + 8) as u8`. X11 keycode for evdev KEY_A (30) is 38, not 30.
- **Fix #5: Expose pseudocode type errors** — `handle_backend_event` (not `push_event`), `BackendEvent::HostEvent(HostEvent::Expose(HostExposeEvent {...}))`, `&mut self`, split trigger list (MapWindow→self, Unmap/Destroy/Restack→siblings).
- **Fix #6: Compositor borrow-check** — `RefCell<PixmanImage>` in WindowState/PixmapState for interior mutability; renamed `paint_child_into_parent` → `composite_window_into`.
- **Fix #7: freetype::new_face arity** — Fixed all calls to `new_face(path, 0)` (was one-arg).
- **Fix #8: CharInfo eager population** — Populate at `open_font` for 0x20..=0x7E. `min_bounds`/`max_bounds` in FontState.
- **Fix #10: xkbcommon API arity** — Standardized to 5 strings `(rules, model, layout, variant, options)` + flags.
- **Send bounds verification** — Added to Task 0.0 for `xkb::Context/Keymap/State`, `freetype::Face`, `input::Context`.
- **Task 0.0 promoted** — API verification gate is now the first task, runs `cargo doc` against all three deps before Step 0.1.
- **get_keyboard_mapping / get_modifier_mapping** — Promoted from Step 1 stub to Task 4.9, driven by xkbcommon.
- **change_subwindow_attributes** — Tracks per-window `bg_pixel`/`bg_pixmap` (CWBackPixel / CWBackPixmap), not no-op.
- **InputEvent::Button → HostPointerEvent** — Explicit press/release split with `PointerEventKind::ButtonPress` / `ButtonRelease`.
- **Pointer motion accumulation** — Explicit `dx/dy` accumulation + clamping to scanout bounds in Task 4.4.
- **Scroll wheel** — Formally deferred to 6.5+ (requires `InputEvent::Scroll` variant, a Phase 6.1-area change).
- **Page-flip drain contract** — Noted in Task 6.2 to verify `drain_events` behavior; `while acquire().is_some()` catch-up if needed.
- **CPU-bound idle render** — Acknowledged in Risks; damage tracking in 6.5+.
- **name_window_pixmap** — Returns `Unsupported` (was `BadAlloc`).
- **Reparent Expose edge case** — Noted in Task 3.3b as deferred to 6.5+.

#### Third review (concrete code-shape fixes)

- **HostPointerEvent missing fields** — Added `host_xid`, `detail`, `time` to motion + button pseudocode in Task 4.4.
- **window_under_cursor hit-test** — New Task 3.3c. Walks top-level windows in reverse stacking order to find the window under cursor. Returns host_xid for pointer events. Without this, Step 7 fails.
- **PointerEventKind::Motion → MotionNotify** — Fixed variant name in Task 4.4.
- **get_keyboard_mapping signature** — Rewritten to match trait: `(&mut self, Option<OriginContext>, u8, u8) -> io::Result<(u8, Vec<u32>)>`. Flat-and-padded reply with NoSymbol padding.
- **get_modifier_mapping** — Returns conventional default mapping (Shift=0x32+0x3E, Lock=0x42, etc.) rather than a defer-to-be-determined comment.
- **Compositor borrow ergonomics** — Named `borrow_mut()` binding with `&mut *child_target` deref. Added `PixmanImage` forwarding methods for `composite32`/`fill_rectangles`. Removed `drop(window)` (no-op).
- **Step 0 renumbering** — Tasks now 0.0, 0.0b, 0.1–0.5 (0.4 was missing).
- **query_pointer return spec** — Explicit `PointerPosition { same_screen: true, win_x: cursor_x, win_y: cursor_y, mask }`.
- **WindowHandle allocator** — Monotonic counter `next_host_xid` starting at `0x00400000`, added to KmsBackend struct.
- **freetype::Library Send** — Added to Task 0.0 checks. Task 0.0b covers both Face and Library.
- **change_subwindow_attributes parsing** — CW-bit order documented (CWBackPixmap=0x01 first, CWBackPixel=0x02 second). Reuse nested.rs parser if available.
- **Cursor size** — 16×16 white rectangle in Task 3.4.
- **ConfigureWindow dual Expose** — Both pattern A (resize) and pattern B (stack-mode change) from same hook, Task 3.3b.
- **key_get_syms_by_level** — Verified via Task 0.0 (not `key_get_syms`). Added layout/level params.

## Strategy

Each numbered Step is one logical commit. `cargo build` + `cargo test` green at every commit. Manual smoke gates at Steps 4, 6, 7, 8.

After every commit:
```sh
cargo +nightly fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

## Dependencies

New workspace dependencies to add:

```toml
pixman = "0.2.1"
freetype = "0.7"
xkbcommon = "0.9"
```

All three are MIT-licensed (compatible with project's GPL-3.0-only).

---

## Step 0 — API verification gate + deps + kms module skeleton

**Task 0.0 is the highest-leverage task.** Run `cargo doc` against all three new deps before writing any code. Three days of API archaeology beats finding out at Step 1.11.

### Task 0.0 — API verification gate

Run `cargo doc -p pixman -p freetype -p xkbcommon` in a temporary crate with all three deps. Confirm:

1. **Pixman scanout constructor** — Does `Image::from_raw_mut`, `Image::from_slice_mut`, or `Image::create_bits` exist? Which signature? Option A/B/C in Task 2.1 will be pinned to whichever actually compiles.
2. **freetype::Library::new_face arity** — Confirm signature is `new_face(path: &str, face_index: isize)`. This was already flagged and must not regress.
3. **xkb::Keymap::new_from_names arity** — Takes a `RuleNames` struct or 5 strings `(rules, model, layout, variant, options)` + flags. Verify which overload exists in 0.9.
4. **xkb::State::mod_name_is_active** — Verify the exact API: is it `state.mod_name_is_active(name, flags)` or `xkb_state_mod_name_is_active(state, name, flags)`? What's the type of `flags`?
5. **xkb::STATE_MODS_EFFECTIVE** — Is it `xkb::STATE_MODS_EFFECTIVE` (top-level const) or `StateComponent::STATE_MODS_EFFECTIVE`?
6. **Send bounds** — Assert all types the struct holds are `Send`:
   ```rust
   fn assert_send<T: Send>() {}
   assert_send::<xkb::Context>();
   assert_send::<xkb::Keymap>();
   assert_send::<xkb::State>();
   // freetype::Face is historically !Send (shares state with Library)
   // May need wrapper or separate verification
   assert_send::<input::Context>();
   ```
7. **freetype::Face Send** — If `freetype::Face` is `!Send`, wrap in a newtype with documented invariant (single-threaded access via `Arc<Mutex<dyn Backend>>`), similar to `PixmanImage`. **Pin it:** If `!Send`, add Task 0.0b — write `FreetypeFace` newtype with `unsafe impl Send` and document the invariant.
8. **freetype::Library Send** — Also check `assert_send::<freetype::Library>()`. If `FontLoader` becomes a `KmsBackend` field (because `open_font` needs library access), `Library` must be `Send` too. If `!Send`, same `unsafe impl Send` newtype treatment.

Pin the actual function names into the plan before any code.

### Task 0.0b — Conditional: FreetypeFace newtype (if Task 0.0 confirms !Send)

If `freetype::Face` or `freetype::Library` is `!Send`, add this wrapper to `fonts.rs`:

```rust
pub struct FreetypeFace(freetype::Face);
unsafe impl Send for FreetypeFace {}
// SAFETY: All access is serialized through Arc<Mutex<dyn Backend>>.
// Single-threaded context makes this sound.
```

Replace all `freetype::Face` references in `FontState` with `FreetypeFace`. Same treatment for `freetype::Library` in `FontLoader` if needed.

### Task 0.1 — Add deps

Add to workspace `Cargo.toml` `[workspace.dependencies]`:
```toml
pixman = "0.2.1"
freetype = "0.7"
xkbcommon = "0.9"
```

Add to `crates/yserver/Cargo.toml` `[dependencies]`:
```toml
pixman.workspace = true
freetype.workspace = true
xkbcommon.workspace = true
```

### Task 0.2 — Create module skeleton

```
crates/yserver/src/kms/
  mod.rs          — pub mod backend, compositor, event, render, fonts; pub use KmsBackend; pub use backend::PixmanImage
  backend.rs      — KmsBackend struct + impl Backend (all stubs) + PixmanImage wrapper (!Send fix)
  compositor.rs   — window tree tracking + composite-to-scanout pass + Expose synthesis
  event.rs        — libinput → xkbcommon → HostKeyEvent / HostPointerEvent
  render.rs       — Pixman drawing helpers (X11 GC → Pixman mapping)
  fonts.rs        — freetype font loading + glyph rasterization + CharInfo cache + XLFD fallback
```

In `crates/yserver/src/lib.rs`, add `pub mod kms;`.

### Task 0.3 — KmsBackend stub struct

```rust
pub struct KmsBackend {
    device: Arc<drm::Device>,
    output: drm::modeset::Output,
    fb_w: u16,
    fb_h: u16,
    swapchain: drm::Swapchain,
    window_id: u32,
    root_visual_xid: u32,
    event_sink: Option<Box<dyn BackendEventSink>>,
    xid_map: HostXidMap,
    key_subscribers: Arc<Mutex<Vec<Sender<HostKeyEvent>>>>,
}
```

Note: `scanout_image` is NOT stored in the struct (created per-flip, review #1).

`impl Backend for KmsBackend` — all methods return stubs (`Ok(())`, `Ok(None)`, sentinel values). Just enough for the compiler to accept the type.

### Task 0.4 — Build gate

```sh
cargo build --workspace
cargo clippy --workspace --all-targets
```

All green. `yserver` binary still calls the existing `yserver::run()` (bouncing rectangle) — the new module is unused.

### Task 0.5 — Commit

```sh
git add Cargo.toml crates/yserver/Cargo.toml crates/yserver/src/kms/ crates/yserver/src/lib.rs
git commit -m "chore: Phase 6.4 Step 0 — API verification gate + pixman/freetype/xkbcommon deps + kms module skeleton"
```

---

## Step 1 — KmsBackend constructor + state accessors + stub lifecycle

Build a working `KmsBackend` that opens DRM, discovers an output, creates a swapchain, commits the initial modeset, and returns all required trait accessor values. No drawing or compositing yet.

### Task 1.1 — KmsBackend struct (full)

Expand the struct from Step 0 with all fields needed:

**IMPORTANT: `pixman::Image` is `!Send`/`!Sync` in 0.2.1.** The `Backend` trait requires `Send`. Wrap pixman images in a newtype with `unsafe impl Send` — the invariant is that all access is serialized through `Arc<Mutex<dyn Backend>>` (main thread owns scanout, window images are only accessed on the main thread):

```rust
/// Newtype wrapper around pixman::Image.
/// SAFETY: All access is serialized through Arc<Mutex<dyn Backend>>.
/// The main thread owns scanout; window/pixmap images are only touched on the main thread.
pub struct PixmanImage(pixman::Image<'static, 'static>);
unsafe impl Send for PixmanImage {}

pub struct KmsBackend {
    // DRM (Phase 6.1 reuse)
    device: Arc<drm::Device>,
    output: drm::modeset::Output,
    fb_w: u16,
    fb_h: u16,
    swapchain: drm::Swapchain,

    // NOTE: No persistent scanout_image field! The mmap'd buffer lifetime
    // doesn't support 'static. Create the scanout Pixman image per-flip
    // in the page-flip handler, use it for compositing, then drop it.

    // Window tracking: nested window resource ID → local window state
    windows: HashMap<u32, WindowState>,
    next_host_xid: u32,  // Monotonic counter, starts at 0x00400000

    // Backend trait state
    window_id: u32,
    root_visual_xid: u32,
    event_sink: Option<Box<dyn BackendEventSink>>,
    xid_map: HostXidMap,              // Arc<Mutex<XidMap>> — same Arc every call (review #15)
    key_subscribers: Arc<Mutex<Vec<Sender<HostKeyEvent>>>>,

    // xkbcommon
    xkb_context: xkb::Context,
    xkb_keymap: xkb::Keymap,
    xkb_state: xkb::State,

    // libinput
    input_ctx: Option<input::Context>,

    // Fonts (freetype)
    fonts: HashMap<u32, FontState>,

    // Pixman pixmaps (non-window drawables)
    pixmaps: HashMap<u32, PixmapState>,

    // Background state (root)
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,

    // Software cursor
    cursor_x: f32,
    cursor_y: f32,
}
```

Supporting structs:
```rust
struct WindowState {
    nested_id: ResourceId,
    x: i16, y: i16,
    width: u16, height: u16,
    border_width: u16,
    mapped: bool,
    override_redirect: bool,
    parent: Option<u32>,
    children: Vec<u32>,   // stacking order (bottom → top)
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,
    image: RefCell<PixmanImage>,  // RefCell for interior mutability (review borrow-fix)
    depth: u8,
    visual: u32,
}

struct FontState {
    handle: u32,
    face: freetype::Face,
    metrics: FontMetrics,
    char_info_cache: HashMap<char, CharInfo>,  // Eagerly populated at open_font for 0x20..=0x7E (review #8)
    min_bounds: CharInfo,
    max_bounds: CharInfo,
}

struct PixmapState {
    handle: u32,
    image: RefCell<PixmanImage>,  // RefCell for interior mutability
    depth: u8,
}

struct CharInfo {
    width: i16,
    left_side_bearing: i16,
    right_side_bearing: i16,
    ascent: i16,
    descent: i16,
    attributes: u32,
}
```

**Why `RefCell<PixmanImage>`:** The compositor needs to mutate a window's image while immutably borrowing the `windows` HashMap (to read sibling/parent data). Without `RefCell` you'd hold `&mut` on one entry and `&` on the map simultaneously — a borrow-checker violation. Single-threaded context (`Arc<Mutex<dyn Backend>>` serializes) makes `RefCell` sound. Alternative: a separate `HashMap<u32, RefCell<PixmanImage>>` sibling map.

### Task 1.2 — Constructor

`KmsBackend::open(device_path: &str) -> io::Result<Self>`:

1. `drm::Device::open(device_path)` — same as existing `lib.rs::open_drm_device`
2. `drm::modeset::discover_output(&device)` — first connected connector
3. Allocate swapchain with 2 buffers (`drm::Buffer::new`), dimensions from `output.picked`
4. `drm::modeset::commit_modeset(&device, &output, buffers[0].fb_id())` — initial modeset
5. `input::Context::new()` — seat0, non-fatal if unavailable
6. xkbcommon keymap (review #10, verified in Task 0.0):
   ```rust
   let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
   // Primary: explicit names (5 strings: rules, model, layout, variant, options)
   let keymap = xkb::Keymap::new_from_names(
       &ctx, "evdev", "pc105", "us", "", "",  // rules, model, layout, variant, options
       xkb::KEYMAP_COMPILE_NO_FLAGS,
   );
   // Fallback: empty strings (system default)
   let keymap = keymap.or_else(|| xkb::Keymap::new_from_names(
       &ctx, "", "", "", "", "",
       xkb::KEYMAP_COMPILE_NO_FLAGS,
   ));
   let keymap = keymap.ok_or_else(|| io::Error::new(ErrorKind::Other, "failed to create keymap"))?;
   let state = xkb::State::new(&keymap);
   ```
7. Initialize `xid_map` with `ROOT_WINDOW` mapping
8. **No scanout image creation here** — created per-flip (review #1)
9. Store `window_id = 1`, `root_visual_xid = 0x21`

### Task 1.3 — State accessor impls

| Trait method | KmsBackend return |
|---|---|
| `window_id()` | `1` |
| `root_visual_xid()` | `0x21` |
| `argb_visual_xid()` | `None` |
| `argb_colormap_xid()` | `None` |
| `render_opcode()` | `None` |
| `xkb_opcode()` | `None` |
| `xkb_info()` | `None` |
| `composite_opcode()` | `None` |
| `render_format_for_ynest_id()` | `None` |
| `ping()` | `Ok(())` |
| `set_event_sink(sink)` | Store the sink |
| `xid_map()` | Return clone of the same `Arc<Mutex<XidMap>>` every call (review #15) |
| `add_key_subscriber(tx)` | Push to `key_subscribers` |

### Task 1.4 — Subwindow lifecycle

**WindowHandle allocator (host_xid):** `WindowHandle` is a `u32` newtype that stands in for a host_xid. `KmsBackend` needs a monotonic counter starting at `0x00400000` to avoid colliding with the root xid `0x00000001`. Add field `next_host_xid: u32` to `KmsBackend` and allocate as:

```rust
self.next_host_xid = self.next_host_xid.checked_add(1).expect("xid space exhausted");
let host_xid = self.next_host_xid;
```

- `create_subwindow` → allocate `WindowHandle(host_xid)`, create `PixmanImage` via `pixman::Image::new(X8R8G8B8, w, h, true)`, insert into `windows`, add to parent's `children`
- `destroy_subwindow` → remove from `windows`, drop Pixman image, remove from parent's children
- `map_subwindow` → `mapped = true`
- `unmap_subwindow` → `mapped = false`
- `configure_subwindow` → update x/y/w/h; if resize, recreate Pixman image (preserve old content)
- `reparent_subwindow` → update parent, position; move children list
- `change_subwindow_attributes` → parse value_mask for `CWBackPixel` / `CWBackPixmap`. Bits in value_mask correspond to consecutive entries in `values: &[u32]` in CW-bit order: `CWBackPixmap=0x01` first, `CWBackPixel=0x02` second. Update the matching `WindowState.bg_pixel` / `WindowState.bg_pixmap` fields (xterm uses this). Other CWAttribute fields remain no-op. Reuse the existing parser helper from `nested.rs` if available rather than rewriting bit-ordering logic.
- `update_host_event_mask` → no-op
- `register_top_level` → insert `host_xid → nested_id` into `xid_map`
- `register_subwindow` → insert into `xid_map`
- `unregister_host_window` → remove from `xid_map`

### Task 1.5 — Resource ops

- `create_pixmap(depth, width, height)` → create Pixman image with format mapping (d1→A1, d8→A8, d24→X8R8G8B8, d32→A8R8G8B8), store in `pixmaps`, return `PixmapHandle`
- `free_pixmap` → remove from `pixmaps`
- `open_font(name)` → load via freetype (Step 5), return `(FontHandle, FontMetrics)`
- `close_font` → remove from `fonts`
- `create_cursor` → allocate fake `CursorHandle`
- `define_cursor` → no-op
- `name_window_pixmap` → return `Unsupported` (not BadAlloc; with composite_opcode() = None this path shouldn't be reached)

### Task 1.6 — Container background

- `set_container_background_pixel` → store `bg_pixel`
- `set_container_background_pixmap` → store `bg_pixmap`

### Task 1.7 — GC state stubs

All GC methods return `Ok(())` — defer to Step 2.

### Task 1.8 — Drawing stubs

All drawing methods return `Ok(())` — defer to Step 2.

### Task 1.9 — Extension stubs

All `render_*`, `xkb_proxy`, `xfixes_*`, `set_shape_rectangles` → no-op / `Ok(None)`.

### Task 1.10 — Misc stubs

- `warp_pointer` → no-op
- `query_pointer` → return `PointerPosition { same_screen: true, win_x: cursor_x as i16, win_y: cursor_y as i16, mask: serialize_modifiers(&xkb_state) }`. The cursor is naturally absolute (root coords) for KmsBackend, so `win_x/y` = `cursor_x/y` since there's no separate root window in the KMS case.
- `list_fonts_*` → empty reply
- `get_atom_name` → `Ok(None)`
- `get_keyboard_mapping` → stub in Step 1, populated from xkbcommon in Task 4.9 (iterate keycodes, call `key_get_syms_by_level`)
- `get_modifier_mapping` → stub in Step 1, populated from xkbcommon in Task 4.9

### Task 1.11 — Build gate

```sh
cargo build --workspace
cargo clippy --workspace --all-targets
```

All green.

### Task 1.12 — Commit

```sh
git add crates/yserver/src/kms/
git commit -m "feat: Phase 6.4 Step 1 — KmsBackend constructor + state accessors + stub lifecycle"
```

---

## Step 2 — Pixman rasterization for core drawing

Implement the ~17 core drawing methods using Pixman. Each window owns a Pixman image; drawing ops target that image.

### Task 2.1 — Scanout Pixman image management (per-flip, review #1)

On swapchain `acquire()`, create Pixman image wrapping buffer's mmap. **Verify API before implementation** (review #3) — `from_slice_mut` may not exist in pixman 0.2.1. Run `cargo doc -p pixman` or check docs.rs. Likely alternatives:

```rust
// Option A: if from_raw_mut exists
let buf = swapchain.buffer_mut(idx);
let img = pixman::Image::from_raw_mut(
    pixman::FormatCode::X8R8G8B8,
    buf.pixels_mut(),
    buf.width() as usize, buf.height() as usize,
    buf.stride() as usize,
)?;

// Option B: if only Image::new exists, manually copy to a Vec
// (less ideal but works for validation)
let mut img = pixman::Image::new(pixman::FormatCode::X8R8G8B8, w, h, true)?;
// then blit from buffer to img

// Option C: if pixman uses raw pointers, wrap with unsafe
let img = unsafe {
    pixman::Image::create_bits(
        pixman::FormatCode::X8R8G8B8,
        buf.width() as i32, buf.height() as i32,
        buf.pixels_mut() as *mut u32,
        buf.stride() as i32,
    )
};
```

The scanout image is created here, used for `composite_all`, then dropped — never stored in the struct.

### Task 2.2 — Pixman drawing helpers (`render.rs`)

```rust
fn gc_function_to_pixman(fn_: GcFunction) -> pixman::Operation {
    match fn_ {
        GcFunction::Copy => pixman::Operation::Src,
        GcFunction::Xor => pixman::Operation::Xor,
        GcFunction::Set => pixman::Operation::Src,
        GcFunction::Clear => pixman::Operation::Clear,
        GcFunction::NoOp => pixman::Operation::Dst,
        GcFunction::And => pixman::Operation::In,
        GcFunction::Or => pixman::Operation::Over,
        // ... remaining functions
    }
}

fn color_from_u32(pixel: u32) -> pixman::Color {
    let r = ((pixel >> 16) & 0xFF) as u16;
    let g = ((pixel >> 8) & 0xFF) as u16;
    let b = (pixel & 0xFF) as u16;
    pixman::Color::new(r << 8, g << 8, b << 8, 0xFFFF)
}
```

### Task 2.3 — Implement core drawing methods

| Method | Pixman mapping |
|---|---|
| `fill_rectangle` | `dst_img.fill_rectangles(Src, color, &[rect])` |
| `poly_fill_rectangle` | `dst_img.fill_rectangles(Src, color, &rects)` |
| `copy_area` | `dst_img.composite32(Src, src_img, None, ...)` |
| `copy_plane` | Same with plane mask |
| `put_image` | Write ZPixmap data into `dst_img.data()` |
| `get_image` | Read from `dst_img.data()` |
| `poly_point` | 1×1 `fill_rectangles` per point |
| `poly_segment` | Thin rectangles |
| `poly_line` | Connected thin rectangles |
| `poly_rectangle` | Outline edges via `fill_rectangles` |
| `poly_arc` | Trapezoids via `composite_trapezoids` |
| `poly_fill_arc` | Filled trapezoids |
| `fill_poly` | Region fill or trapezoids |
| Text methods | Defer to Step 5 |

### Task 2.4 — GC state integration

Replace stub implementations with real GC state usage: clip regions, fill styles, etc.

### Task 2.5 — Pixmap format mapping

| X11 depth | Pixman FormatCode |
|---|---|
| 1 | `A1` |
| 8 | `A8` |
| 24 | `X8R8G8B8` |
| 32 | `A8R8G8B8` |

### Task 2.6 — Tests

```rust
#[test] fn fill_rectangle_writes_pixels() { ... }
#[test] fn copy_area_blits_correct_region() { ... }
#[test] fn put_image_writes_zpixbuf_data() { ... }
#[test] fn poly_fill_rectangle_fills_multiple() { ... }
#[test] fn pixmap_format_mapping_depth1_is_a1() { ... }
```

### Task 2.7 — Build gate

```sh
cargo build --workspace
cargo test -p yserver --lib
cargo clippy --workspace --all-targets
```

### Task 2.8 — Commit

```sh
git add crates/yserver/src/kms/
git commit -m "feat: Phase 6.4 Step 2 — Pixman rasterization for core drawing ops"
```

---

## Step 3 — Compositor + window tracking

Implement proper window compositing: walk the window tree, paint each window's Pixman image onto the scanout buffer, respect stacking order and clipping.

### Task 3.1 — Window tree management

Ensure `create_subwindow`, `reparent_subwindow`, `destroy_subwindow`, `configure_subwindow` correctly maintain parent/children relationships and stacking order.

### Task 3.2 — Composite pass (`compositor.rs`)

Composite child→parent, not child→scanout (review #6). Children composite into their parent's image, giving natural clipping. Top-level windows composite to scanout.

**Borrow-checker fix:** Use `RefCell<PixmanImage>` in `WindowState` so each image can be mutably borrowed independently while the `windows` HashMap is immutably borrowed for traversal.

**PixmanImage forwarding methods:** Add impl to `PixmanImage` so `composite32`, `fill_rectangles`, etc. are callable on the wrapper:
```rust
impl PixmanImage {
    pub fn fill_rectangles(&self, op: pixman::Operation, color: pixman::Color, rects: &[pixman::Rectangle]) {
        self.0.fill_rectangles(op, color, rects)
    }
    pub fn composite32(&self, op: pixman::Operation, src: &pixman::ImageRef, mask: Option<&pixman::ImageRef>,
        src_x: i32, src_y: i32, mask_x: i32, mask_y: i32, dst_x: i32, dst_y: i32, w: i32, h: i32) {
        self.0.composite32(op, src, mask, src_x, src_y, mask_x, mask_y, dst_x, dst_y, w, h)
    }
    // ... other forwarding methods as needed
}
```

```rust
pub fn composite_all(
    scanout: &mut PixmanImage,
    windows: &HashMap<u32, WindowState>,
    bg_pixel: Option<u32>,
    bg_pixmap: Option<&PixmanImage>,
) {
    // Fill scanout with root background
    if let Some(pixel) = bg_pixel {
        let color = color_from_u32(pixel);
        scanout.fill_rectangles(pixman::Operation::Src, color, &[root_rect]);
    } else if let Some(pixmap_img) = bg_pixmap {
        scanout.composite32(pixman::Operation::Src, &pixmap_img.0, None, ...);
    }

    // Walk top-level windows in stacking order
    for window_id in find_top_level_windows(windows) {
        composite_window_into(scanout, windows, window_id);
    }
}

/// (1) Paint this window's children INTO its own image (natural clipping).
/// (2) Composite the window's image (now containing children) into parent_img.
fn composite_window_into(
    parent_img: &mut PixmanImage,
    windows: &HashMap<u32, WindowState>,
    window_id: u32,
) {
    // Immutable borrow of the map entry for position/children
    let window = &windows[&window_id];
    if !window.mapped { return; }
    let children: Vec<u32> = window.children.clone();  // clone IDs, not images

    // (1) Paint children INTO the window's own image via RefCell
    for &child_id in &children {
        let mut child_target = window.image.borrow_mut();
        composite_window_into(&mut *child_target, windows, child_id);
    }
    // NLL ends the immutable borrow on `window` here naturally — no explicit drop needed

    // (2) Composite window image onto parent
    let window = &windows[&window_id];
    parent_img.composite32(
        pixman::Operation::Over,
        &window.image.borrow().0,
        None,
        0, 0,  // src offset
        window.x as i32, window.y as i32,  // dst offset
        window.width as i32, window.height as i32,
    );
}
```

Note: `drop(window)` is NOT used — `drop()` on a `&WindowState` is a no-op (drops a reference). NLL ends the borrow naturally when the scope ends.

Alternative approach if `RefCell` feels awkward: maintain a separate `HashMap<u32, RefCell<PixmanImage>>` sibling map alongside `windows`. Traversal borrows `windows` immutably for geometry; image access goes through the sibling map. Same soundness guarantee.

### Task 3.3 — Clip children to parent bounds

Use Pixman's `set_clip_region` or compute intersection rect before compositing child windows. Since we composite children into parent's image (Task 3.2), clipping is naturally enforced by the parent image dimensions — children outside parent bounds are simply not visible in the parent's Pixman image.

### Task 3.3c — Pointer hit-test (window_under_cursor)

Pointer events need `host_xid: u32` — the window the cursor is currently over. Without this, every pointer event arrives with `host_xid = 0` and the sink can't resolve it to a client via `xid_map`. **This blocks Step 7's xeyes validation.**

Add to `KmsBackend`:

```rust
fn window_under_cursor(&self) -> Option<u32> {
    // Walk top-level windows in reverse stacking order (top first)
    let top_levels = find_top_level_windows(&self.windows);
    for window_id in top_levels.into_iter().rev() {
        let w = &self.windows[&window_id];
        if !w.mapped { continue; }
        let x = self.cursor_x as f64;
        let y = self.cursor_y as f64;
        if x >= w.x as f64 && x < (w.x as f64 + w.width as f64)
            && y >= w.y as f64 && y < (w.y as f64 + w.height as f64) {
            // For greater fidelity, recurse into children to find
            // the topmost child containing the cursor. For v1,
            // returning the top-level host_xid is sufficient.
            return Some(window_id);  // this is the host_xid
        }
    }
    None  // cursor is over root
}
```

Use in Task 4.4 pointer event construction:
```rust
let host_xid = self.window_under_cursor().unwrap_or(0);
let ptr_event = HostPointerEvent {
    kind: PointerEventKind::MotionNotify,
    host_xid,
    detail: 0,
    time: current_time_ms(),
    root_x: self.cursor_x as i16,
    ...
};
```

### Task 3.3b — Expose event synthesis (review #5, REQUIRED for xeyes/xterm)

X11 clients depend on Expose events to draw initial content and repaint after occlusion. Without these, xeyes/xterm will show blank windows.

**Two distinct routing patterns:**

**A. Expose goes to the just-mapped/resized window itself:**
- **MapWindow**: when a window transitions from unmapped to mapped, send Expose for its full area to that window
- **ConfigureWindow resize**: when a window is resized, send Expose for the newly-exposed region to that window

**B. Expose goes to windows that were uncovered (not the source window):**
- **UnmapWindow**: when a window is unmapped, send Expose to all sibling windows that were partially covered by it
- **DestroyWindow**: same as UnmapWindow — send Expose to siblings that were covered
- **ConfigureWindow stack-mode change**: when a window is raised/lowered via ConfigureWindow, send Expose to windows that were uncovered by the reordering
- **ReparentWindow**: can re-expose siblings at both old and new parent positions — defer full handling to 6.5+, but log the gap

**Implementation in `backend.rs`** (note `&mut self` and correct types):

```rust
fn synthesize_expose(&mut self, host_xid: u32, x: u16, y: u16, w: u16, h: u16) {
    // Construct an Expose event and route through the sink
    let expose_event = HostEvent::Expose(HostExposeEvent {
        host_xid,
        x, y, width: w, height: h,
        count: 0,  // 0 = last expose in series
    });
    if let Some(ref mut sink) = self.event_sink {
        sink.handle_backend_event(BackendEvent::HostEvent(expose_event));
    }
}
```

**Sink routing detail:** `BackendEventSink::handle_backend_event` resolves `host_xid` → `ResourceId` via the cloned `xid_map` (same `Arc` returned by `KmsBackend::xid_map()`), then delivers the Expose to the owning client through `expose_event_fanout`. State this explicitly — Step 6's sink wiring will use this path.

Hook into `map_subwindow` (pattern A), `configure_subwindow` (pattern A on resize, pattern B on stack-mode change), `unmap_subwindow` (pattern B), `destroy_subwindow` (pattern B).

### Task 3.4 — Software cursor

Draw cursor as a **16×16 white rectangle** on scanout after all window compositing. Store cursor position from pointer events. The 16×16 size is conventional for a software cursor and visible enough for xeyes validation.

### Task 3.5 — Integration with swapchain

On page-flip: acquire buffer → **create temporary scanout Pixman image** (review #1) → `composite_all` (child→parent, review #6) → draw cursor → drop image → submit flip.

### Task 3.6 — Tests

```rust
#[test] fn composite_paints_windows_in_stacking_order() { ... }
#[test] fn unmapped_window_not_composited() { ... }
#[test] fn child_window_clipped_to_parent_bounds() { ... }
#[test] fn root_bg_pixel_fills_scanout() { ... }
#[test] fn software_cursor_drawn_at_last_position() { ... }
#[test] fn expose_sent_on_map_window() { ... }
#[test] fn expose_sent_on_unmap_uncovers_siblings() { ... }
```

**Note on SHAPE extension (review #13):** xeyes uses SHAPE for non-rectangular pupil windows. This is deferred to Phase 6.5+. The validation gate for xeyes should be: "pupils track cursor, accept rectangular chrome ugliness."

### Task 3.7 — Build gate

```sh
cargo build --workspace
cargo test -p yserver --lib
cargo clippy --workspace --all-targets
```

### Task 3.8 — Commit

```sh
git add crates/yserver/src/kms/
git commit -m "feat: Phase 6.4 Step 3 — compositor + window tracking + per-window Pixman images"
```

---

## Step 4 — xkbcommon integration + libinput event routing

Integrate xkbcommon for Linux evdev keycode → X11 keysym translation. Route libinput events through `BackendEventSink`.

### Task 4.1 — Keyboard state

Already present from Step 1. Verify keymap created from system defaults.

### Task 4.2 — libinput → xkbcommon → HostKeyEvent (`event.rs`)

**IMPORTANT: `HostKeyEvent` shape (review #4).** Actual struct in `pump.rs:183`:
```rust
pub struct HostKeyEvent {
    pub pressed: bool,        // not `is_press`
    pub keycode: u8,          // not u32
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
    // NO `keysym` field — X11 clients do their own keysym lookup
}
```

Option 1 (preferred): Drop keysym computation entirely. X11 clients receive the raw keycode and do their own keysym lookup via the core keyboard mapping protocol. This matches how HostX11Backend works.

Option 2: Extend HostKeyEvent to carry keysym (requires yserver-core change). Only needed if we later want to inject synthetic key events.

```rust
pub fn translate_keyboard_event(
    evdev_keycode: u32,
    is_press: bool,
    xkb_state: &mut xkb::State,
    cursor_x: i16,
    cursor_y: i16,
    time_ms: u32,
) -> HostKeyEvent {
    // xkbcommon keycodes are evdev keycode + 8
    let xkb_keycode = xkb::Keycode::new(evdev_keycode + 8);
    let direction = if is_press {
        xkb::KeyDirection::Down
    } else {
        xkb::KeyDirection::Up
    };
    xkb_state.update_key(xkb_keycode, direction);

    // X11 keycode on the wire is also evdev + 8 (evdev KEY_A=30 → X11 keycode 38)
    HostKeyEvent {
        pressed: is_press,
        keycode: (evdev_keycode + 8) as u8,
        time: time_ms,
        root_x: cursor_x,
        root_y: cursor_y,
        event_x: cursor_x,
        event_y: cursor_y,
        state: serialize_modifiers(xkb_state),
    }
}
```

### Task 4.3 — Modifier serialization (review #9)

**Do NOT assume xkb mod-index ordering matches X11 bit ordering.** Query each named mod explicitly:

```rust
fn serialize_modifiers(state: &xkb::State) -> u16 {
    let mut mask: u16 = 0;
    // Verify const name in Task 0.0 — may be xkb::STATE_MODS_EFFECTIVE or StateComponent::STATE_MODS_EFFECTIVE
    let flags = xkb::STATE_MODS_EFFECTIVE;

    if state.mod_name_is_active("Shift", flags) { mask |= 0x01; }
    if state.mod_name_is_active("Lock", flags)  { mask |= 0x02; }
    if state.mod_name_is_active("Control", flags) { mask |= 0x04; }
    if state.mod_name_is_active("Mod1", flags)  { mask |= 0x08; }
    if state.mod_name_is_active("Mod2", flags)  { mask |= 0x10; }
    if state.mod_name_is_active("Mod3", flags)  { mask |= 0x20; }
    if state.mod_name_is_active("Mod4", flags)  { mask |= 0x40; }
    if state.mod_name_is_active("Mod5", flags)  { mask |= 0x80; }

    mask
}
```

Also add button mapping for pointer events (review #11):

```rust
/// libinput button code → X11 button number
fn libinput_button_to_x11(button: u32) -> u8 {
    match button {
        0x110 => 1, // BTN_LEFT → button 1
        0x111 => 3, // BTN_RIGHT → button 3
        0x112 => 2, // BTN_MIDDLE → button 2
        0x113 => 8, // BTN_SIDE → button 8
        0x114 => 9, // BTN_EXTRA → button 9
        _ => 0,     // unknown
    }
}
```

**Scroll wheel:** Deferred to 6.5+. The local `InputEvent` enum has no `Scroll` variant today (only `Button`, `KeyPress`, `KeyRelease`, `PointerMotion`), and adding one is a Phase 6.1-area change. Remove scroll mapping comments to avoid confusion.

### Task 4.4 — libinput pointer events → HostPointerEvent

Reuse existing `input::Context::dispatch()`. Map `InputEvent` variants to `HostPointerEvent` / `HostKeyEvent`:

**Pointer motion (relative → absolute accumulation):**
```rust
InputEvent::PointerMotion { dx, dy } => {
    self.cursor_x = (self.cursor_x + dx as f32).clamp(0.0, self.fb_w as f32 - 1.0);
    self.cursor_y = (self.cursor_y + dy as f32).clamp(0.0, self.fb_h as f32 - 1.0);
    let host_xid = self.window_under_cursor().unwrap_or(0);
    let ptr_event = HostPointerEvent {
        kind: PointerEventKind::MotionNotify,
        host_xid,
        detail: 0,  // 0 for motion events
        time: current_time_ms(),
        root_x: self.cursor_x as i16,
        root_y: self.cursor_y as i16,
        event_x: self.cursor_x as i16,
        event_y: self.cursor_y as i16,
        state: serialize_modifiers(&self.xkb_state),
    };
    if let Some(ref mut sink) = self.event_sink {
        sink.handle_backend_event(BackendEvent::HostEvent(HostEvent::Pointer(ptr_event)));
    }
}
```

**Button press/release:**
```rust
InputEvent::Button { code, pressed } => {
    let host_xid = self.window_under_cursor().unwrap_or(0);
    let ptr_event = HostPointerEvent {
        kind: if pressed { PointerEventKind::ButtonPress } else { PointerEventKind::ButtonRelease },
        host_xid,
        detail: libinput_button_to_x11(code),
        time: current_time_ms(),
        root_x: self.cursor_x as i16,
        root_y: self.cursor_y as i16,
        event_x: self.cursor_x as i16,
        event_y: self.cursor_y as i16,
        state: serialize_modifiers(&self.xkb_state),
    };
    if let Some(ref mut sink) = self.event_sink {
        sink.handle_backend_event(BackendEvent::HostEvent(HostEvent::Pointer(ptr_event)));
    }
}
```

**Key press/release:** → `translate_keyboard_event` from Task 4.2, then fan to key_subscribers.

### Task 4.5 — Event sink integration

- Keyboard events → fan to `key_subscribers`
- Pointer events → `BackendEvent::HostEvent(Pointer(...))` → sink → fanout

### Task 4.6 — Tests

```rust
#[test] fn evdev_keycode_30_produces_x11_keycode_38() { ... }  // evdev + 8
#[test] fn shift_modifier_mask_set_when_shift_held() { ... }
#[test] fn control_modifier_mask_set_when_ctrl_held() { ... }
#[test] fn pointer_motion_accumulates_relative_deltas() { ... }
#[test] fn pointer_motion_translates_to_host_pointer_event() { ... }
#[test] fn key_subscribers_receive_keyboard_events() { ... }
#[test] fn libinput_btn_left_maps_to_x11_button_1() { ... }
#[test] fn libinput_btn_right_maps_to_x11_button_3() { ... }
```

### Task 4.7 — Build gate

```sh
cargo build --workspace
cargo test -p yserver --lib
cargo clippy --workspace --all-targets
```

### Task 4.8 — Commit

```sh
git add crates/yserver/src/kms/
git commit -m "feat: Phase 6.4 Step 4 — xkbcommon integration + libinput event routing"
```

### Task 4.9 — get_keyboard_mapping / get_modifier_mapping from xkbcommon

Upgrade these from Step 1 stubs. xterm calls `GetKeyboardMapping` at startup to map keycodes to keysyms.

**Important: trait signature mismatch.** The Backend trait defines:
```rust
fn get_keyboard_mapping(
    &mut self,
    origin: Option<OriginContext>,
    first_keycode: u8,
    count: u8,
) -> io::Result<(u8, Vec<u32>)>;
```
Return is `(keysyms_per_keycode, flat_keysyms)` — a flat `Vec<u32>` of length `count * keysyms_per_keycode`, with shorter rows padded to `keysyms_per_keycode` with `NoSymbol` (0).

Implementation:
```rust
fn get_keyboard_mapping(
    &mut self, _origin: Option<OriginContext>,
    first_keycode: u8, count: u8,
) -> io::Result<(u8, Vec<u32>)> {
    // 1. Gather Vec<Vec<Keysym>> for each keycode
    let mut rows: Vec<Vec<u32>> = Vec::new();
    for kc in first_keycode..(first_keycode + count) {
        let xkb_kc = xkb::Keycode::new(kc as u32);
        // Verify API name in Task 0.0 — may be key_get_syms_by_level or key_get_syms
        let syms = self.xkb_keymap.key_get_syms_by_level(xkb_kc, 0, 0);
        rows.push(syms.iter().map(|s| s.raw()).collect());
    }
    // 2. Compute max_levels (keysyms_per_keycode)
    let max_levels = rows.iter().map(|r| r.len()).max().unwrap_or(1) as u8;
    // 3. Flatten with NoSymbol padding
    let mut flat = Vec::with_capacity((count as usize) * (max_levels as usize));
    for row in &rows {
        flat.extend_from_slice(row);
        for _ in row.len()..(max_levels as usize) {
            flat.push(0); // NoSymbol
        }
    }
    Ok((max_levels, flat))
}
```

**get_modifier_mapping** — return the conventional default mapping. xterm works fine with this:
```rust
fn get_modifier_mapping(
    &mut self, _origin: Option<OriginContext>,
) -> io::Result<Vec<Vec<u8>>> {
    // 8 rows: Shift, Lock, Control, Mod1..Mod5
    // Each row lists up to 4 keycodes
    // Return conventional defaults matching a standard US layout
    Ok(vec![
        vec![0x32, 0x3E, 0, 0],  // Shift  (keys 50, 62)
        vec![0x42, 0, 0, 0],      // Lock   (key 66)
        vec![0x25, 0x69, 0, 0],  // Control (keys 37, 105)
        vec![0x40, 0x6C, 0, 0],  // Mod1   (keys 64, 108)
        vec![0x4D, 0, 0, 0],      // Mod2   (key 77, NumLock)
        vec![0x73, 0, 0, 0],      // Mod3   (key 115)
        vec![0x85, 0x86, 0, 0],  // Mod4   (keys 133, 134, Super)
        vec![],                    // Mod5   (empty)
    ])
}
```

---

## Step 5 — Font rendering via freetype

Implement font support for xterm text rendering. Load system fonts via freetype, rasterize glyphs into Pixman images.

### Task 5.1 — freetype font loading (`fonts.rs`)

**XLFD pattern fallback (review #7):** xterm calls `OpenFont("-misc-fixed-medium-r-...")` which isn't a filesystem path. Any XLFD pattern (starts with `-`) maps to the hardcoded fallback font.

```rust
pub struct FontLoader {
    library: freetype::Library,
}

impl FontLoader {
    pub fn new() -> io::Result<Self> {
        Ok(Self { library: freetype::Library::init()? })
    }

    /// Check if a font name is an XLFD pattern (starts with '-')
    fn is_xlfd_pattern(name: &str) -> bool {
        name.starts_with('-')
    }

    pub fn open_font(&self, name: &str) -> io::Result<(freetype::Face, FontMetrics)> {
        // If it looks like an XLFD pattern, use the fallback font (review #7)
        let path = if Self::is_xlfd_pattern(name) {
            None  // skip to fallback candidates
        } else {
            // Try as a direct filesystem path first
            self.library.new_face(name, 0).ok().map(|face| (face, name.to_string()))
        };

        let candidates = [
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/gnu-free/FreeMono.ttf",
            "/usr/share/fonts/freefonts/FreeMono.ttf",
            "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        ];

        // If we already loaded from path, return it
        if let Some((face, _)) = path {
            let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
            return Ok((face, compute_font_metrics(&face)?));
        }

        // Try fallback candidates
        for candidate in &candidates {
            if let Ok(face) = self.library.new_face(candidate, 0) {
                let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
                return Ok((face, compute_font_metrics(&face)?));
            }
        }

        Err(io::Error::new(ErrorKind::NotFound, format!("font not found: {name}")))
    }
}
```

### Task 5.2 — Glyph rasterization with eager CharInfo cache (review #8)

**Eager population at open_font:** `QueryFont` and `QueryTextExtents` are typically called by xterm before it draws any text — to learn the font's advance width and decide column geometry. With a lazy cache, those queries see an empty map and return zeros, producing a 0-wide character cell.

At `open_font`, after loading the face, eagerly populate the CharInfo cache for ASCII printable range:

```rust
fn populate_char_info_cache(face: &freetype::Face) -> (HashMap<char, CharInfo>, CharInfo, CharInfo) {
    let mut cache = HashMap::new();
    let mut min_bounds = CharInfo { width: i16::MAX, left_side_bearing: i16::MAX, right_side_bearing: i16::MIN, ascent: i16::MIN, descent: i16::MIN, attributes: 0 };
    let mut max_bounds = CharInfo { width: i16::MIN, left_side_bearing: i16::MIN, right_side_bearing: i16::MAX, ascent: i16::MAX, descent: i16::MAX, attributes: 0 };

    for code in 0x20u32..=0x7E {
        let ch = char::from_u32(code).unwrap();
        let ci = compute_char_info(face, ch);
        // Update min/max bounds
        if ci.width < min_bounds.width { min_bounds.width = ci.width; }
        if ci.left_side_bearing < min_bounds.left_side_bearing { min_bounds.left_side_bearing = ci.left_side_bearing; }
        if ci.right_side_bearing > max_bounds.right_side_bearing { max_bounds.right_side_bearing = ci.right_side_bearing; }
        if ci.ascent > max_bounds.ascent { max_bounds.ascent = ci.ascent; }
        if ci.descent > max_bounds.descent { max_bounds.descent = ci.descent; }
        if ci.ascent < min_bounds.ascent { min_bounds.ascent = ci.ascent; }
        if ci.descent < min_bounds.descent { min_bounds.descent = ci.descent; }
        cache.insert(ch, ci);
    }

    (cache, min_bounds, max_bounds)
}
```

Then at `poly_text8` / `image_text8` rasterization time:
1. Look up character in the pre-populated CharInfo cache (don't recompute)
2. `face.load_glyph(char_idx, LoadRender)`
3. Create Pixman A8 image from glyph bitmap:
   ```rust
   let bitmap = glyph.bitmap();
   let mut glyph_img = pixman::Image::new(
       pixman::FormatCode::A8,
       bitmap.width() as usize,
       bitmap.rows() as usize,
       true,
   )?;
   // Copy bitmap data into glyph_img
   ```
4. Composite solid color source through glyph alpha mask onto destination:
   ```rust
   dst_img.composite32(
       pixman::Operation::Over,
       &color_src_img,
       Some(&glyph_img),
       0, 0,                    // src offset
       glyph_left, glyph_top,    // mask offset
       dst_x, dst_y,             // dst offset
       glyph_w, glyph_h,
   );
   ```

The CharInfo cache in `FontState` stores per-character metrics so that `QueryFont`, `QueryTextExtents`, and text positioning operations return accurate data. `min_bounds` and `max_bounds` populate the FontMetrics reply structure.

### Task 5.3 — OpenFont / CloseFont

`open_font(name)` → `FontLoader::open_font`, store `FontState`, return `(FontHandle, FontMetrics)`.
`close_font` → remove from `fonts`.

### Task 5.4 — QueryFont / ListFonts

Return cached metrics / font names from loaded fonts.

### Task 5.5 — Tests

```rust
#[test] fn open_font_xlfd_pattern_maps_to_fallback() { ... }
#[test] fn open_font_loads_dejavu_sans_mono() { ... }
#[test] fn render_text_produces_nonzero_pixels() { ... }
#[test] fn query_font_returns_cached_char_info_metrics() { ... }
#[test] fn char_info_cache_matches_rasterized_glyph() { ... }
```

### Task 5.6 — Build gate

```sh
cargo build --workspace
cargo test -p yserver --lib
cargo clippy --workspace --all-targets
```

### Task 5.7 — Commit

```sh
git add crates/yserver/src/kms/
git commit -m "feat: Phase 6.4 Step 5 — freetype font loading + glyph rasterization via Pixman"
```

---

## Step 6 — yserver-core integration + epoll event loop

Replace the `yserver::run()` bouncing rectangle with a proper server loop driving `yserver-core` through `KmsBackend`.

### Task 6.1 — Replace `lib.rs::run()`

New entry point:
1. Open DRM device, create `KmsBackend`
2. Create `ServerState::with_geometry(w, h)`
3. Wrap backend as `Arc<Mutex<dyn Backend>>`
4. Set up `HostPumpEventSink`
5. Create Unix socket listener at `/tmp/.X11-unix/X7` (review #14)
6. Set up epoll over `[listener, drm_fd, libinput_fd, signalfd]`
7. Submit initial flip with root background
8. Main loop: dispatch events by token
   - `LISTENER` → accept client, spawn `handle_client` thread
   - `DRM` → drain page-flip events, `handle_page_flip` (composite + resubmit)
   - `INPUT` → `handle_input_events` (xkbcommon + sink fanout)
   - `SIGNAL` → break loop, disable output, exit

### Task 6.2 — Page-flip handler

On page-flip completion: `swapchain.complete` drains events → `acquire` → create scanout Pixman image → `composite_all` → draw cursor → drop image → `submit_flip`.

**Multi-flip drain:** Verify `drm/page_flip.rs:41`'s `drain_events` contract. If it drains all pending events in a loop (not one per call), submitting one new flip per drain is correct. If it returns after one event, wrap in `while swapchain.acquire().is_some() { ... }` to catch up on overrun. Add a `multi_flip_event_drain` test to confirm.

**Idle render caveat:** This is a self-driving loop at vsync cadence (~60 fps). With xterm idle, you're recompositing 60 fps for nothing. Functional, acknowledged in Risks as "CPU-bound idle render until damage tracking lands in 6.5+."

### Task 6.3 — Input handler

`input_ctx.dispatch()` → translate keyboard via xkbcommon → fan to key subscribers; translate pointer → push to sink.

### Task 6.4 — Client handler integration

Existing `nested.rs::handle_client` already takes `Arc<Mutex<dyn Backend>>` — works with `KmsBackend` via trait dispatch.

### Task 6.5 — Initial flip

Composite root background into buffer[0] and submit first flip so screen isn't black on startup.

### Task 6.6 — Tests

```rust
#[test] fn page_flip_triggers_composite_and_resubmit() { ... }
#[test] fn libinput_events_routed_to_sink() { ... }
#[test] fn signalfd_triggers_clean_shutdown() { ... }
```

### Task 6.7 — Build gate

```sh
cargo build -p yserver --bin yserver
cargo test -p yserver --lib
cargo clippy -p yserver --all-targets
```

### Task 6.8 — Commit

```sh
git add crates/yserver/src/lib.rs crates/yserver/src/kms/
git commit -m "feat: Phase 6.4 Step 6 — epoll event loop + yserver-core integration"
```

---

## Step 7 — Validation: xeyes

First visual test. xeyes needs: pointer events, filled arcs, window creation, Expose events.

### Task 7.1 — Smoke recipe

```sh
cargo build --release --bin yserver
just yserver-headless
# From vng guest (review #14: use :7 to avoid colliding with host X on :0):
DISPLAY=:7 xeyes
```

### Task 7.2 — Debug path

`RUST_LOG=debug just yserver-headless` → check for unsupported opcodes. Compare with x11trace of xeyes on Xephyr.

### Task 7.3 — Validation gate

xeyes window appears on DRM scanout, pupils track cursor. Clean shutdown on SIGTERM.

### Task 7.4 — Commit

```sh
git add docs/status.md
git commit -m "docs: Phase 6.4 Step 7 — xeyes validation"
```

---

## Step 8 — Validation: xterm

Headline deliverable. Needs: font rendering, text ops, CopyArea scrollback, keyboard input, pointer events.

### Task 8.1 — Smoke recipe

```sh
just yserver-headless &
DISPLAY=:7 xterm
```

### Task 8.2 — Debug path

`RUST_LOG=debug`, x11trace comparison. Likely issues: font path in vng, CopyArea for scrollback, keyboard routing.

### Task 8.3 — Validation gate

xterm renders shell prompt, accepts keyboard input, scrollback works via CopyArea. Clean shutdown.

### Task 8.4 — Update status.md

Mark Phase 6.4 complete. Document validation outcomes and follow-ups.

### Task 8.5 — Commit

```sh
git add crates/yserver/ docs/status.md
git commit -m "feat: Phase 6.4 Step 8 — xterm validation + status update"
```

---

## Known follow-ups (deferred to Phase 6.5+)

- **RENDER extension** — Full implementation for GTK3/Qt/Xft text rendering
- **Multi-monitor** — RANDR with multiple outputs/CRTCs
- **Hardware cursor plane** — DRM cursor/overlay plane instead of software sprite
- **GLX/EGL/Vulkan** — GPU-accelerated clients
- **VT switching / logind / VT_SETMODE** — Proper session management
- **SHAPE / COMPOSITE / DAMAGE / XFIXES** — Extension support for WMs
- **XTEST extension** — Automated/headless testing
- **Damage tracking** — Partial repaint optimization

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Pixman `Image` is `!Send`/`!Sync` | Wrapped in `PixmanImage` newtype with `unsafe impl Send` — all access serialized through `Arc<Mutex<dyn Backend>>` |
| Pixman API mismatch in 0.2.1 | Task 0.0 verifies before any code; three fallback approaches documented in Task 2.1 |
| Send bounds fail for xkb/freetype types | Task 0.0 asserts `Send` for `xkb::Context/Keymap/State`, `freetype::Face`, `input::Context`. If `freetype::Face` is `!Send`, wrap in newtype like `PixmanImage` |
| Font loading fails in vng | Fallback: bundle bitmap font as bytes; try multiple system paths |
| Compositor performance | Full-scanout composite OK for validation; damage tracking is 6.5+ |
| CPU-bound idle render (60 fps vsync) | Acknowledged; damage tracking in 6.5+ will only repaint changed regions |
| xkbcommon keymap failure | Fall back to hardcoded US QWERTY layout |
| freetype unavailable in vng | `libfreetype` is standard; if missing, bundle as static dep |
| DRM device unavailable | Graceful error; `YSERVER_DRM_DEVICE` env var for explicit path |
| Expose synthesis missed for edge cases | Gate xeyes on initial draw + resize repaint; reparent edge cases deferred to 6.5+ |
| Scroll wheel not implemented | Formally deferred to 6.5+ (requires InputEvent::Scroll variant + Phase 6.1 change) |

## Estimated scope

| Step | Content | Est. LoC |
|---|---|---|
| 0 | Deps + skeleton | ~50 |
| 1 | KmsBackend struct + constructor + stubs | ~400 |
| 2 | Pixman rasterization | ~300 |
| 3 | Compositor + window tracking + Expose synthesis | ~250 |
| 4 | xkbcommon integration + libinput event routing | ~150 |
| 5 | freetype font rendering + CharInfo cache + XLFD fallback | ~400 |
| 6 | Event loop + yserver-core wiring | ~250 |
| 7-8 | Validation + debugging | ~50 |
| **Total** | | **~1850** |
