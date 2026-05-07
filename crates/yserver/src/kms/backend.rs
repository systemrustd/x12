use std::{cell::RefCell, collections::HashMap, io, sync::Arc};

use pixman::{Color, FormatCode, Image, Operation, Rectangle16, Repeat};
use yserver_core::{
    backend::{
        AnyHandle, Backend, ClipState, CursorHandle, DrawState, FillState, FontHandle, GcFunction,
        GlyphSetHandle, OriginContext, PictureHandle, PixmapHandle, WindowHandle,
    },
    host_x11::{
        HostKeyEvent, HostPointerEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap,
        PointerEventKind, PointerPosition,
    },
};
use yserver_protocol::x11::{
    CharInfo as ProtocolCharInfo, ClipRectangles, FontMetrics, RENDER_FMT_A1, RENDER_FMT_A8,
    ResourceId, xfixes,
};

use crate::drm;

/// Newtype wrapper around `freetype::Face`.
/// `repr(transparent)` is required so `RefCell::as_ptr` can be safely cast
/// from `*mut FreetypeFace` to `*mut freetype::Face` in `render_text_string`.
/// SAFETY: All access is on the single-threaded core thread.
/// Single-threaded context makes this sound. `Face` contains raw pointers
/// and `Rc<Vec<u8>>` by default, both `!Send`.
#[repr(transparent)]
pub struct FreetypeFace(#[allow(dead_code)] pub freetype::Face);
unsafe impl Send for FreetypeFace {}

/// Newtype wrapper around `xkb::Context`.
/// SAFETY: All access is on the single-threaded core thread.
/// The raw pointer in xkbcommon is not `Send`, but the C library is thread-safe.
pub struct XkbContext(pub xkbcommon::xkb::Context);
unsafe impl Send for XkbContext {}

/// Newtype wrapper around `xkb::Keymap`.
/// SAFETY: All access is on the single-threaded core thread.
pub struct XkbKeymap(pub xkbcommon::xkb::Keymap);
unsafe impl Send for XkbKeymap {}

/// Newtype wrapper around `xkb::State`.
/// SAFETY: All access is on the single-threaded core thread.
pub struct XkbState(pub xkbcommon::xkb::State);
unsafe impl Send for XkbState {}

/// Newtype wrapper around pixman::Image.
/// SAFETY: All access is on the single-threaded core thread.
/// The main thread owns scanout; window/pixmap images are only touched
/// on the main thread.
pub struct PixmanImage(pub Image<'static, 'static>);

unsafe impl Send for PixmanImage {}

impl PixmanImage {
    /// Create a blank Pixman image with the given format and dimensions.
    pub fn new(format: FormatCode, width: u16, height: u16, clear: bool) -> io::Result<Self> {
        Image::new(format, width as usize, height as usize, clear)
            .map(Self)
            .map_err(|_| io::Error::other("pixman image creation failed"))
    }

    /// Create a Pixman image wrapping an external buffer (for scanout).
    ///
    /// # Safety
    /// Caller guarantees the buffer outlives the image and is valid for the
    /// given dimensions and rowstride. The buffer must remain valid for the
    /// lifetime of the returned `PixmanImage`.
    pub unsafe fn from_buffer(
        format: FormatCode,
        width: u16,
        height: u16,
        bits: *mut u32,
        rowstride_bytes: usize,
        clear: bool,
    ) -> io::Result<Self> {
        unsafe {
            Image::from_raw_mut(
                format,
                width as usize,
                height as usize,
                bits,
                rowstride_bytes,
                clear,
            )
        }
        .map(Self)
        .map_err(|_| io::Error::other("pixman image creation from buffer failed"))
    }

    pub fn width(&self) -> usize {
        self.0.width()
    }

    pub fn height(&self) -> usize {
        self.0.height()
    }

    pub fn stride(&self) -> usize {
        self.0.stride()
    }

    /// Returns the raw pixel buffer pointer.
    ///
    /// # Safety
    /// The returned pointer is valid for the lifetime of the image. The
    /// caller must ensure no aliasing mutable access (e.g. via another
    /// `&mut PixmanImage` or another raw pointer obtained the same way).
    /// The buffer's element type depends on the image's `FormatCode` —
    /// dereferencing as `u32` is only correct for 32-bit-per-pixel
    /// formats (`A8R8G8B8`, `X8R8G8B8`). For sub-32bpp formats (`A8`,
    /// `A1`) cast the returned pointer to `*mut u8` and use the byte
    /// stride from `stride()`.
    pub unsafe fn data(&self) -> *mut u32 {
        // SAFETY: forwarded contract — caller guarantees serialized access.
        unsafe { self.0.data() }
    }

    /// Raw `*mut pixman_image_t` for the FFI helpers below. Call sites
    /// should use `composite32` / `composite_trapezoids` rather than
    /// invoking `pixman::ffi::*` directly.
    fn ffi_ptr(&self) -> *mut pixman::ffi::pixman_image_t {
        self.0.as_ptr()
    }
}

/// Composite using `pixman_image_composite32`.
///
/// `dst` is borrowed mutably; `src_ptr` and `mask_ptr` (null for "no mask")
/// are passed raw because typical call sites hold those images via
/// `HashMap` lookups whose lifetimes don't compose cleanly with a `&mut
/// PixmanImage` borrow on the destination.
///
/// **Aliasing.** Pixman supports `src` and `dst` referring to the same
/// image *when the source/destination rectangles do not overlap* (this is
/// the path used by `copy_area` for in-window scroll). Aliasing with
/// arbitrary clipping or compositor masks is undefined, so callers that
/// could hit that case (e.g. RENDER `Composite`) should pre-check and
/// skip. This wrapper does not enforce a generic alias guard.
///
/// `src_ptr` may not be null; `mask_ptr` may be null to disable masking.
///
/// `op` is the raw pixman / X-RENDER operator code (`Operation::Src as
/// u32`, or a wire `PictOp` value forwarded by RENDER); they share the
/// same numeric encoding.
fn composite32(
    op: u32,
    src_ptr: *mut pixman::ffi::pixman_image_t,
    mask_ptr: *mut pixman::ffi::pixman_image_t,
    dst: &mut PixmanImage,
    src_x: i32,
    src_y: i32,
    mask_x: i32,
    mask_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) {
    if src_ptr.is_null() {
        log::warn!("composite32: src is null, skipping");
        return;
    }
    let dst_ptr = dst.ffi_ptr();
    // SAFETY: src_ptr is non-null and dst_ptr is non-null (borrowed
    // through &mut PixmanImage from a live image). mask_ptr is null or a
    // valid pixman_image_t pointer obtained the same way. dst is uniquely
    // borrowed; pixman does not retain the pointers after return. Caller
    // is responsible for any composite-specific aliasing constraints (see
    // the doc comment).
    unsafe {
        pixman::ffi::pixman_image_composite32(
            op, src_ptr, mask_ptr, dst_ptr, src_x, src_y, mask_x, mask_y, dst_x, dst_y, width,
            height,
        );
    }
}

/// Composite a slice of trapezoids using `pixman_composite_trapezoids`.
///
/// Same aliasing contract as `composite32` — pixman's behaviour is only
/// well-defined when `src_ptr` and `dst` differ; for trapezoid composites
/// the typical use is a solid-fill source against a window destination,
/// so they shouldn't share a backing image. The wrapper does not enforce
/// a generic alias guard. `traps` is borrowed for the duration of the
/// call.
fn composite_trapezoids(
    op: u32,
    src_ptr: *mut pixman::ffi::pixman_image_t,
    dst: &mut PixmanImage,
    mask_format: pixman::ffi::pixman_format_code_t,
    x_src: i32,
    y_src: i32,
    x_dst: i32,
    y_dst: i32,
    traps: &[pixman::ffi::pixman_trapezoid_t],
) {
    if src_ptr.is_null() {
        log::warn!("composite_trapezoids: src is null, skipping");
        return;
    }
    if traps.is_empty() {
        return;
    }
    let dst_ptr = dst.ffi_ptr();
    // SAFETY: src_ptr is non-null and points at a live pixman_image_t
    // owned by some PixmanImage. dst_ptr comes from a uniquely-borrowed
    // PixmanImage. `traps` is a Rust slice with valid len/ptr. Caller is
    // responsible for any aliasing constraints (see the doc comment).
    unsafe {
        pixman::ffi::pixman_composite_trapezoids(
            op,
            src_ptr,
            dst_ptr,
            mask_format,
            x_src,
            y_src,
            x_dst,
            y_dst,
            traps.len() as std::os::raw::c_int,
            traps.as_ptr(),
        );
    }
}

/// Geometry + raw pixel pointer for a window or pixmap drawable.
///
/// `data_ptr` is a byte-granularity pointer; cast to `*mut u32` for 32bpp
/// formats (`A8R8G8B8`, `X8R8G8B8`) or use as-is for sub-32bpp formats
/// (`A8`). `stride_bytes` is the per-row stride in **bytes** as reported
/// by pixman (which may pad rows for alignment).
///
/// The lifetime parameter ties the pointer to a `&self` borrow on
/// `KmsBackend`: while a `DrawableGeometry` is live, no `&mut`
/// modification of `self.windows` / `self.pixmaps` can occur, so the
/// pointer remains valid for in-bounds reads and writes.
struct DrawableGeometry<'a> {
    width: usize,
    height: usize,
    stride_bytes: usize,
    data_ptr: *mut u8,
    _phantom: std::marker::PhantomData<&'a ()>,
}

fn read_drawable_pixel_for_plane(
    geom: &DrawableGeometry<'_>,
    depth: u8,
    x: usize,
    y: usize,
) -> u32 {
    if x >= geom.width || y >= geom.height {
        return 0;
    }
    // SAFETY: Bounds are checked above and data_ptr/stride_bytes come from a
    // live pixman image borrowed through drawable_geometry.
    unsafe {
        match depth {
            1 => {
                let byte = *geom.data_ptr.add(y * geom.stride_bytes + x / 8);
                u32::from((byte & (0x80 >> (x % 8))) != 0)
            }
            8 => u32::from(*geom.data_ptr.add(y * geom.stride_bytes + x)),
            _ => {
                let words = geom.data_ptr.cast::<u32>();
                let stride_words = geom.stride_bytes / 4;
                *words.add(y * stride_words + x)
            }
        }
    }
}

/// Convert an X11 24-bit pixel (0xRRGGBB) to a Pixman Color.
/// Append 1×1 rects covering a Bresenham line from (x0,y0) to (x1,y1).
fn bresenham_segment(x0: i32, y0: i32, x1: i32, y1: i32, out: &mut Vec<Rectangle16>) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        out.push(Rectangle16 {
            x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            width: 1,
            height: 1,
        });
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Scanline fill a polygon (even-odd rule).  Edges are pairs of i32
/// vertices.  Output is a Vec of 1-pixel-tall horizontal Rectangle16 spans.
fn scanline_fill_polygon(verts: &[(i32, i32)], out: &mut Vec<Rectangle16>) {
    if verts.len() < 3 {
        return;
    }
    let y_min = verts.iter().map(|&(_, y)| y).min().unwrap();
    let y_max = verts.iter().map(|&(_, y)| y).max().unwrap();
    let mut crossings: Vec<i32> = Vec::with_capacity(verts.len());
    for y in y_min..=y_max {
        crossings.clear();
        for i in 0..verts.len() {
            let (x0, y0) = verts[i];
            let (x1, y1) = verts[(i + 1) % verts.len()];
            // Skip horizontal edges; use half-open [min_y, max_y) so
            // shared vertices contribute exactly once across two edges.
            let (ya, yb) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
            if ya == yb || y < ya || y >= yb {
                continue;
            }
            // Linear interpolation: x at scanline y.
            let x = x0 as i64 + ((y - y0) as i64 * (x1 - x0) as i64) / (y1 - y0) as i64;
            crossings.push(x as i32);
        }
        crossings.sort_unstable();
        let mut i = 0;
        while i + 1 < crossings.len() {
            let x_start = crossings[i];
            let x_end = crossings[i + 1];
            if x_end > x_start {
                let w = (x_end - x_start) as i64;
                out.push(Rectangle16 {
                    x: x_start.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    width: w.min(u16::MAX as i64) as u16,
                    height: 1,
                });
            }
            i += 2;
        }
    }
}

/// Clip a list of `Rectangle16` to the bounds `[0, iw) × [0, ih)` and drop
/// rects that fall entirely outside.  Pixman's `fill_rectangles` is supposed
/// to clip on its own but in our build a partially-out-of-bounds rect
/// (especially with negative x/y) can segfault; pre-clipping is the cheap
/// defensive workaround.
fn clip_rects_to_image(rects: &[Rectangle16], iw: i32, ih: i32) -> Vec<Rectangle16> {
    let mut out = Vec::with_capacity(rects.len());
    for r in rects {
        let x1 = (r.x as i32).max(0);
        let y1 = (r.y as i32).max(0);
        let x2 = ((r.x as i32) + r.width as i32).min(iw);
        let y2 = ((r.y as i32) + r.height as i32).min(ih);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        out.push(Rectangle16 {
            x: x1 as i16,
            y: y1 as i16,
            width: (x2 - x1) as u16,
            height: (y2 - y1) as u16,
        });
    }
    out
}

fn color_from_u32(pixel: u32) -> Color {
    let r = ((pixel >> 16) & 0xFF) as u16;
    let g = ((pixel >> 8) & 0xFF) as u16;
    let b = (pixel & 0xFF) as u16;
    Color::new(r << 8, g << 8, b << 8, 0xFFFF)
}

/// Apply X11 GC `function` to a set of rectangles on `img`.
///
/// `GcFunction::Copy` maps to `PIXMAN_OP_SRC` (fast path).
/// `GcFunction::Xor` requires manual pixel manipulation: pixman's Porter-Duff
/// `PIXMAN_OP_XOR` is `src*(1-dst.a) + dst*(1-src.a)` which gives zero for
/// fully opaque images — NOT the bitwise XOR that X11 GXxor specifies.
/// All other GcFunction variants fall back to `Src` with a debug log.
fn fill_rects_with_gc_function(
    img: &mut PixmanImage,
    function: GcFunction,
    foreground_rgb: u32,
    rects: &[Rectangle16],
) {
    if matches!(function, GcFunction::Xor) {
        // Bitwise XOR over the RGB channels (X byte is preserved).
        // This fast path is only correct for 32bpp images (4 bytes/pixel)
        // where stride_words == width.  For A8 or A1 images the stride is
        // smaller than width * 4, so `ptr.add(y * stride_words + x)` would
        // walk past the allocation and SIGSEGV.  Guard: stride (bytes) must
        // equal width × 4.
        let stride_bytes = img.0.stride();
        let iw = img.0.width();
        let ih = img.0.height();
        if stride_bytes == iw * 4 {
            let xor_mask = foreground_rgb & 0x00FF_FFFF;
            let stride_words = stride_bytes / 4; // == iw for 32bpp
            // SAFETY: PixmanImage::data() is unsafe; we hold an exclusive &mut
            // reference to img so no other live references to the pixel buffer
            // exist. stride_words == iw, so ptr.add(y * iw + x) with x < iw
            // and y < ih is always within the ih * iw allocation.
            let ptr = unsafe { img.0.data() };
            for r in rects {
                let x0 = (r.x as i32).max(0) as usize;
                let y0 = (r.y as i32).max(0) as usize;
                let x1 = (r.x as i32 + r.width as i32).min(iw as i32).max(0) as usize;
                let y1 = (r.y as i32 + r.height as i32).min(ih as i32).max(0) as usize;
                for y in y0..y1 {
                    for x in x0..x1 {
                        unsafe {
                            let p = ptr.add(y * stride_words + x);
                            let old = *p;
                            *p = (old & 0xFF00_0000) | ((old ^ xor_mask) & 0x00FF_FFFF);
                        }
                    }
                }
            }
            return;
        }
        // Non-32bpp image: fall through to the pixman path below (treated as Src).
        log::debug!(
            "GcFunction::Xor on non-32bpp image (stride={stride_bytes}, width={iw}); falling back to Src"
        );
    }
    let op = match function {
        GcFunction::Copy => Operation::Src,
        other => {
            log::debug!(
                "GC function {:?} not implemented, falling back to Copy",
                other
            );
            Operation::Src
        }
    };
    let color = color_from_u32(foreground_rgb);
    // Clip rectangles to image bounds. pixman SHOULD do this internally but
    // crashes on rects that extend past the image (seen with wmaker drawing
    // rect (4,58,6,59) onto a 64×64 pixmap — y+h=117 > image height).
    let iw = img.0.width() as i32;
    let ih = img.0.height() as i32;
    let clipped: Vec<Rectangle16> = rects
        .iter()
        .filter_map(|r| {
            let x0 = (r.x as i32).max(0);
            let y0 = (r.y as i32).max(0);
            let x1 = (r.x as i32).saturating_add(r.width as i32).min(iw);
            let y1 = (r.y as i32).saturating_add(r.height as i32).min(ih);
            if x1 <= x0 || y1 <= y0 {
                return None;
            }
            Some(Rectangle16 {
                x: x0 as i16,
                y: y0 as i16,
                width: (x1 - x0) as u16,
                height: (y1 - y0) as u16,
            })
        })
        .collect();
    if !clipped.is_empty() {
        let _ = img.0.fill_rectangles(op, color, &clipped);
    }
}

/// Parse a packed pair of i16 values (2 bytes each) from a byte slice.
fn read_i16_pair(data: &[u8], offset: usize) -> Option<(i16, i16)> {
    if offset + 4 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    Some((x, y))
}

/// Build a pixman clip region from a window's `shape_bounding` rect list,
/// translated by `(x, y)` (the window's origin in destination coords).
/// Returns `None` if `rects` is `None` or empty (caller is responsible for
/// the "explicitly empty shape" skip; this helper just builds the region).
fn build_shape_clip(
    rects: Option<&Vec<xfixes::RegionRect>>,
    x: i32,
    y: i32,
) -> Option<pixman::Region32> {
    let rects = rects?;
    if rects.is_empty() {
        return None;
    }
    let boxes: Vec<pixman::Box32> = rects
        .iter()
        .map(|r| pixman::Box32 {
            x1: x + i32::from(r.x),
            y1: y + i32::from(r.y),
            x2: x + i32::from(r.x) + i32::from(r.width),
            y2: y + i32::from(r.y) + i32::from(r.height),
        })
        .collect();
    Some(pixman::Region32::init_rects(&boxes))
}

/// Parse a packed rectangle (x:i16, y:i16, w:u16, h:u16) from a byte slice.
fn read_rect(data: &[u8], offset: usize) -> Option<Rectangle16> {
    if offset + 8 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    let w = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
    let h = u16::from_le_bytes([data[offset + 6], data[offset + 7]]);
    Some(Rectangle16 {
        x,
        y,
        width: w,
        height: h,
    })
}

/// A simple integer rectangle in virtual-screen coordinates.
///
/// Used by `OutputLayout::rect()` to support the per-output window
/// pre-filter introduced in Step 4 of the multi-monitor work. We use
/// `i32` widths here (not `u16`) because the virtual-screen extent can
/// exceed `u16::MAX` for large multi-output setups.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A single DRM output and its dedicated swapchain, positioned in the
/// virtual screen. `KmsBackend` owns one of these per discovered
/// output. `fb_w` / `fb_h` on the backend describe the virtual-screen
/// extent (max(x + width), max(y + height)); per-output dimensions
/// live here.
pub(crate) struct OutputLayout {
    pub output: crate::drm::modeset::Output,
    pub swapchain: crate::drm::Swapchain,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl OutputLayout {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            w: i32::from(self.width),
            h: i32::from(self.height),
        }
    }
}

pub struct KmsBackend {
    // DRM (Phase 6.1 reuse)
    device: Arc<drm::Device>,
    outputs: Vec<OutputLayout>,
    fb_w: u16,
    fb_h: u16,

    // Window tracking: nested window resource ID -> local window state
    windows: HashMap<u32, WindowState>,
    next_host_xid: u32, // Monotonic counter, starts at 0x00400000

    // Stacking order for top-level windows (direct children of the root
    // container). Bottom-to-top: the last entry is on top. Updated by
    // create / destroy / reparent / configure_subwindow with stack_mode.
    // The compositor iterates this list in order; HashMap iteration is
    // unordered and doesn't preserve X11 stacking semantics.
    top_level_order: Vec<u32>,

    // Backend trait state
    window_id: u32,
    root_visual_xid: u32,
    xid_map: HostXidMap,

    // xkbcommon
    #[allow(dead_code)]
    xkb_context: XkbContext,
    xkb_keymap: XkbKeymap,
    xkb_state: XkbState,

    // libinput
    input_ctx: Option<crate::input::SendContext>,

    // Fonts (freetype)
    font_loader: FontLoader,
    fonts: HashMap<u32, FontState>,

    // Pixman pixmaps (non-window drawables)
    pixmaps: HashMap<u32, PixmapState>,

    // Background state (root)
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,
    // Rescued image from bg_pixmap after the client frees it (Esetroot pattern).
    bg_pixmap_image: Option<PixmanImage>,

    // Software cursor
    cursor_x: f32,
    cursor_y: f32,
    cursors: HashMap<u32, CursorState>,
    active_cursor: Option<u32>,

    // X11 KeyButMask bits for currently-held mouse buttons (Button1Mask
    // = 0x100 .. Button5Mask = 0x1000). OR'd into the `state` field of
    // MotionNotify (and ButtonRelease) events so WMs can detect drag
    // gestures. Without this, motion-during-press looks like idle
    // motion to fvwm and Move-or-Raise never starts a Move.
    button_mask: u16,

    // Top-level host_xid the cursor was last over. Drives synthetic
    // EnterNotify / LeaveNotify generation: when this changes between
    // motion events we emit Leave on the old window and Enter on the new.
    // ButtonPress / ButtonRelease additionally fire Enter(NotifyGrab) /
    // Leave(NotifyUngrab) for implicit-pointer-grab semantics — toolkits
    // (e16's button widgets in particular) won't fire click actions
    // without these crossing transitions around the press.
    prev_pointer_window: Option<u32>,

    // Pointer events generated by `process_one_input_event` are buffered
    // here instead of being dispatched directly through `event_sink`.
    // The input thread drains this buffer AFTER releasing the backend
    // mutex (see `drain_pending_pointer_events`) and only then forwards
    // events to the sink. This is mandatory: the sink calls
    // `pointer_event_fanout` which acquires `server.lock()`, while
    // request handlers regularly hold `server.lock()` and reach for the
    // backend mutex — a server→backend ordering on one side and a
    // backend→server ordering on the other deadlocks under load. Phase
    // 6.7's Enter(NotifyGrab) / Leave(NotifyUngrab) crossings tripled
    // the per-motion fanout count, exposing the latent race as a
    // routinely-reproducible freeze under e16.
    pending_pointer_events: Vec<HostPointerEvent>,

    // Current font for text rendering
    current_font: Option<u32>,

    // Current GC draw state (default GC values).
    current_function: GcFunction,
    current_foreground: u32,
    current_background: u32,

    // RENDER picture tracking
    pictures: HashMap<u32, PictureState>,

    // Images rescued from freed pixmaps still referenced by live pictures.
    // Keyed by picture host_xid. Cleaned up by render_free_picture.
    picture_rescued_images: HashMap<u32, PixmanImage>,

    // RENDER glyphset tracking
    glyphsets: HashMap<u32, GlyphSetState>,

    // SHAPE extension: per-window shape regions keyed by host XID.
    // None entry = no shape (full rectangle). Some(vec![]) = empty region.
    shape_bounding: HashMap<u32, Vec<xfixes::RegionRect>>, // kind=0
    shape_clip: HashMap<u32, Vec<xfixes::RegionRect>>,     // kind=1
    shape_input: HashMap<u32, Vec<xfixes::RegionRect>>,    // kind=2
}

/// State for a RENDER picture on the KMS backend.
enum PictureState {
    /// Picture wraps a window or pixmap drawable. Composites are forwarded
    /// to that drawable's Pixman image.
    Drawable {
        /// XID of the backing window or pixmap in self.windows / self.pixmaps.
        host_xid: u32,
        /// Optional clip rectangles set via SetPictureClipRectangles.
        clip: Option<Vec<Rectangle16>>,
        repeat: Repeat,
        alpha_map: Option<u32>,
        alpha_x: i16,
        alpha_y: i16,
        clip_x: i16,
        clip_y: i16,
        component_alpha: bool,
        transform: Option<pixman::ffi::pixman_transform_t>,
        graphics_exposure: bool,
        subwindow_mode: u8,
        poly_edge: u8,
        poly_mode: u8,
    },
    /// 1×1 solid colour image (CreateSolidFill). Used as composite source.
    SolidFill {
        image: RefCell<PixmanImage>,
        repeat: Repeat,
        component_alpha: bool,
    },
    Gradient {
        image: PixmanImage,
        repeat: Repeat,
        transform: Option<pixman::ffi::pixman_transform_t>,
    },
}

fn default_drawable_picture(host_xid: u32) -> PictureState {
    PictureState::Drawable {
        host_xid,
        clip: None,
        repeat: Repeat::None,
        alpha_map: None,
        alpha_x: 0,
        alpha_y: 0,
        clip_x: 0,
        clip_y: 0,
        component_alpha: false,
        transform: None,
        graphics_exposure: false,
        subwindow_mode: 0,
        poly_edge: 0,
        poly_mode: 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlyphSetFormat {
    A8,
    A1,
    Other, // ARGB32 etc — not supported for glyph masks yet
}

struct StoredGlyph {
    width: u16,
    height: u16,
    /// RENDER wire field: top-left of bitmap relative to glyph origin.
    /// This is the *negative* of FreeType's bitmap_left.
    /// Draw at pen_x - x, pen_y - y.
    x: i16,
    y: i16,
    x_off: i16,
    /// Vertical pen advance. Parsed from wire for fidelity but unused —
    /// horizontal-text rendering only advances the x pen between glyphs.
    #[allow(dead_code)]
    y_off: i16,
    /// Row-major A8 bytes, densely packed (no per-row padding).
    pixels: Vec<u8>,
    format: GlyphSetFormat,
}

pub(super) struct GlyphSetState {
    format: GlyphSetFormat,
    glyphs: HashMap<u32, StoredGlyph>,
}

struct CursorState {
    image: PixmanImage,
    hot_x: u16,
    hot_y: u16,
}

struct WindowState {
    _nested_id: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    mapped: bool,
    _override_redirect: bool,
    _parent: Option<u32>,
    children: Vec<u32>,
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,
    image: RefCell<PixmanImage>,
    #[allow(dead_code)]
    depth: u8,
    #[allow(dead_code)]
    visual: u32,
    /// Cursor XID set on this window via DefineCursor. `0` means
    /// "inherit from parent" (X11 `None`). The effective cursor for
    /// rendering walks up the parent chain until it finds a non-zero
    /// XID, falling back to whatever cursor was last installed for the
    /// root container.
    cursor: u32,
}

struct FontState {
    #[allow(dead_code)]
    handle: u32,
    face: RefCell<FreetypeFace>,
    metrics: FontMetrics,
    char_info_cache: HashMap<char, ProtocolCharInfo>,
}

struct PixmapState {
    #[allow(dead_code)]
    handle: u32,
    image: PixmanImage,
    #[allow(dead_code)]
    depth: u8,
}

/// Resolves X11 font names (aliases like `fixed`, XLFDs like
/// `-adobe-helvetica-bold-r-*-*-12-*-...`, or family names) to a filesystem
/// path via fontconfig, then opens the file with FreeType.
struct FontLoader {
    library: freetype::Library,
    fc: fontconfig::Fontconfig,
}

impl FontLoader {
    fn new() -> io::Result<Self> {
        let fc = fontconfig::Fontconfig::new()
            .ok_or_else(|| io::Error::other("fontconfig init failed"))?;
        Ok(Self {
            library: freetype::Library::init()
                .map_err(|e| io::Error::other(format!("freetype init failed: {e:?}")))?,
            fc,
        })
    }

    fn is_xlfd_pattern(name: &str) -> bool {
        name.starts_with('-')
    }

    /// Pull (family, style, pixel_size) hints out of an XLFD pattern.
    /// XLFD field indices after splitting on '-' (leading '-' produces an
    /// empty 0th element):
    ///   1=foundry 2=family 3=weight 4=slant 5=setwidth 6=addstyle
    ///   7=pixelsize 8=pointsize 9=resx 10=resy 11=spacing 12=avgwidth
    /// "*" or empty fields are treated as wildcards.
    fn parse_xlfd(name: &str) -> (Option<String>, Option<String>, Option<u32>) {
        let parts: Vec<&str> = name.split('-').collect();
        let take = |i: usize| -> Option<String> {
            parts
                .get(i)
                .filter(|s| !s.is_empty() && **s != "*")
                .map(|s| (*s).to_string())
        };
        let family = take(2);
        let weight = take(3);
        let slant = take(4);
        let style = match (weight.as_deref(), slant.as_deref()) {
            (None, Some("i" | "o")) => Some("Italic".to_string()),
            (Some(w), Some("i" | "o")) => Some(format!("{w} Italic")),
            (Some(w), _) => Some(w.to_string()),
            (None, _) => None,
        };
        let px = parts
            .get(7)
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&s| s > 0);
        (family, style, px)
    }

    fn open_font(
        &self,
        name: &str,
    ) -> io::Result<(freetype::Face, FontMetrics, HashMap<char, ProtocolCharInfo>)> {
        // Resolve the X11 font name to a file path via fontconfig. We can't
        // rely on the high-level `Fontconfig::find`: when the requested family
        // doesn't exist, fontconfig falls back to the *system default* (often
        // a proportional sans-serif), which makes xterm/wmaker render with
        // wrong metrics. Build the pattern ourselves and chain "monospace" as
        // a secondary family — fontconfig prefers the first listed family but
        // falls through the chain before reaching its system default.
        let (family, style, xlfd_px) = if Self::is_xlfd_pattern(name) {
            Self::parse_xlfd(name)
        } else {
            (Some(name.to_string()), None, None)
        };
        let query_family = family.as_deref().unwrap_or("monospace");

        let cfamily = std::ffi::CString::new(query_family)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font name has nul"))?;

        let mut pat = fontconfig::Pattern::new(&self.fc);
        pat.add_string(fontconfig::FC_FAMILY, &cfamily);
        if query_family != "monospace" {
            pat.add_string(fontconfig::FC_FAMILY, c"monospace");
        }
        let cstyle_storage;
        if let Some(style) = style.as_deref() {
            cstyle_storage = std::ffi::CString::new(style)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font style has nul"))?;
            pat.add_string(fontconfig::FC_STYLE, &cstyle_storage);
        }
        let matched = pat.font_match();
        let path = matched.filename().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("font not found: {name}"))
        })?;
        let face_index: isize = matched.face_index().unwrap_or(0) as isize;
        let face = self
            .library
            .new_face(path, face_index)
            .map_err(|e| io::Error::other(format!("freetype new_face({path}) failed: {e:?}")))?;

        // Honour XLFD PIXEL_SIZE if specified; otherwise default to 12pt @ 96dpi.
        if let Some(px) = xlfd_px {
            let _ = face.set_pixel_sizes(0, px);
        } else {
            let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
        }
        let (metrics, char_cache) = compute_font_metrics(&face);
        Ok((face, metrics, char_cache))
    }
}

fn compute_char_info(face: &freetype::Face, ch: char) -> ProtocolCharInfo {
    let glyph_idx = ch as usize;
    let _ = face.load_char(glyph_idx, freetype::face::LoadFlag::RENDER);
    let glyph = face.glyph();
    let bitmap = glyph.bitmap();
    let metrics = glyph.metrics();

    let width = (metrics.horiAdvance >> 6) as i16;
    let left_side_bearing = (metrics.horiBearingX >> 6) as i16;
    let right_side_bearing = left_side_bearing + bitmap.width() as i16;
    let ascent = (metrics.horiBearingY >> 6) as i16;
    let descent = (bitmap.rows() as i16) - ascent;

    ProtocolCharInfo {
        left_side_bearing,
        right_side_bearing,
        character_width: width,
        ascent,
        descent,
        attributes: 0,
    }
}

fn compute_font_metrics(face: &freetype::Face) -> (FontMetrics, HashMap<char, ProtocolCharInfo>) {
    let mut char_info_cache = HashMap::new();
    // min_bounds tracks the per-glyph minimum across each metric, so each
    // field starts at its type's MAX so the first observation overwrites it.
    let mut min_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MAX,
        right_side_bearing: i16::MAX,
        character_width: i16::MAX,
        ascent: i16::MAX,
        descent: i16::MAX,
        attributes: 0,
    };
    // max_bounds tracks the per-glyph maximum, so each field starts at MIN.
    let mut max_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MIN,
        right_side_bearing: i16::MIN,
        character_width: i16::MIN,
        ascent: i16::MIN,
        descent: i16::MIN,
        attributes: 0,
    };

    for code in 0x20u32..=0x7E {
        let ch = char::from_u32(code).unwrap();
        let ci = compute_char_info(face, ch);
        if ci.left_side_bearing < min_bounds.left_side_bearing {
            min_bounds.left_side_bearing = ci.left_side_bearing;
        }
        if ci.right_side_bearing > max_bounds.right_side_bearing {
            max_bounds.right_side_bearing = ci.right_side_bearing;
        }
        if ci.character_width < min_bounds.character_width {
            min_bounds.character_width = ci.character_width;
        }
        if ci.character_width > max_bounds.character_width {
            max_bounds.character_width = ci.character_width;
        }
        if ci.ascent > max_bounds.ascent {
            max_bounds.ascent = ci.ascent;
        }
        if ci.descent > max_bounds.descent {
            max_bounds.descent = ci.descent;
        }
        if ci.ascent < min_bounds.ascent {
            min_bounds.ascent = ci.ascent;
        }
        if ci.descent < min_bounds.descent {
            min_bounds.descent = ci.descent;
        }
        char_info_cache.insert(ch, ci);
    }

    let font_ascent = max_bounds.ascent;
    let font_descent = max_bounds.descent;

    let metrics = FontMetrics {
        min_bounds,
        max_bounds,
        min_char_or_byte2: 0x20,
        max_char_or_byte2: 0x7E,
        default_char: 0x20,
        draw_direction: 0, // LeftToRight
        min_byte1: 0,
        max_byte1: 0,
        all_chars_exist: true,
        font_ascent,
        font_descent,
        properties: Vec::new(),
        char_infos: char_info_cache.values().cloned().collect(),
    };
    (metrics, char_info_cache)
}

impl KmsBackend {
    pub fn open(device_path: &str) -> io::Result<Self> {
        Self::open_with_commit(device_path, drm::modeset::commit_modeset)
    }

    fn open_with_commit(
        device_path: &str,
        commit: fn(
            &crate::drm::Device,
            &crate::drm::modeset::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        let device = Arc::new(drm::Device::open(device_path)?);
        let outputs = drm::modeset::discover_outputs(&device)?;

        // Horizontal layout in connector order. If anything fails part
        // way through bring-up, disable everything we have already
        // committed so the next caller starts from a clean slate.
        // TODO(phase-6.10): no unit test exercises this rollback yet —
        // discover_outputs requires a real DRM device, so a synthetic
        // seam isn't cheap. Manual smoke at Step 5/6 covers it.
        let mut layouts: Vec<OutputLayout> = Vec::with_capacity(outputs.len());
        let mut next_x: i32 = 0;
        let mut bring_up_err: Option<io::Error> = None;
        for output in outputs {
            let w = output.picked.width;
            let h = output.picked.height;
            let mut buffers = Vec::with_capacity(2);
            let mut buffer_err: Option<io::Error> = None;
            for _ in 0..2 {
                match drm::Buffer::new(Arc::clone(&device), w, h) {
                    Ok(b) => buffers.push(b),
                    Err(e) => {
                        buffer_err = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = buffer_err {
                bring_up_err = Some(e);
                break;
            }
            let initial_fb = buffers[0].fb_id();
            if let Err(e) = commit(&device, &output, initial_fb) {
                bring_up_err = Some(e);
                break;
            }
            let swapchain = drm::Swapchain::with_initial_scanout(buffers, 0);
            layouts.push(OutputLayout {
                output,
                swapchain,
                x: next_x,
                y: 0,
                width: w,
                height: h,
            });
            next_x = next_x.saturating_add(i32::from(w));
        }
        if let Some(err) = bring_up_err {
            for done in layouts.iter().rev() {
                let _ = drm::modeset::disable_output(&device, &done.output);
            }
            return Err(err);
        }

        // fb_w / fb_h carry the virtual-screen extent. Saturating
        // cast: huge layouts that exceed u16 are clamped — the rest
        // of the backend assumes u16 framebuffer dims (changing that
        // is out of scope for Step 3).
        let fb_w: u16 = layouts
            .iter()
            .map(|l| u16::try_from(l.x.saturating_add(i32::from(l.width))).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);
        let fb_h: u16 = layouts
            .iter()
            .map(|l| u16::try_from(l.y.saturating_add(i32::from(l.height))).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);

        let input_ctx = match crate::input::SendContext::new() {
            Ok(ctx) => Some(ctx),
            Err(err) => {
                log::warn!("libinput unavailable, continuing without input: {err}");
                None
            }
        };

        let ctx = XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &ctx.0,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx.0,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .ok_or_else(|| io::Error::other("failed to create xkb keymap"))?;
        let xkb_state = XkbState(xkbcommon::xkb::State::new(&keymap));
        let xkb_keymap = XkbKeymap(keymap);

        let mut xid_map: HostXidMap = HashMap::new();
        xid_map.insert(0x0000_0001u32, ResourceId(0x0000_0100));

        let mut me = Self {
            device,
            outputs: layouts,
            fb_w,
            fb_h,
            windows: HashMap::new(),
            next_host_xid: 0x0040_0000,
            top_level_order: Vec::new(),
            window_id: 1,
            root_visual_xid: 0x21,
            xid_map,
            xkb_context: ctx,
            xkb_keymap,
            xkb_state,
            input_ctx,
            font_loader: FontLoader::new()?,
            fonts: HashMap::new(),
            pixmaps: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            bg_pixmap_image: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            cursors: HashMap::new(),
            active_cursor: None,
            button_mask: 0,
            prev_pointer_window: None,
            pending_pointer_events: Vec::new(),
            current_font: None,
            current_function: GcFunction::Copy,
            current_foreground: 0,
            current_background: 0x00ff_ffff,
            pictures: HashMap::new(),
            picture_rescued_images: HashMap::new(),
            glyphsets: HashMap::new(),
            shape_bounding: HashMap::new(),
            shape_clip: HashMap::new(),
            shape_input: HashMap::new(),
        };
        // Install a built-in X-shaped cursor as the universal fallback;
        // any later DefineCursor on the root window will override it.
        me.install_default_cursor();
        Ok(me)
    }

    fn next_host_xid(&mut self) -> u32 {
        self.next_host_xid = self
            .next_host_xid
            .checked_add(1)
            .expect("xid space exhausted");
        self.next_host_xid
    }

    /// Build the classic X-shaped default cursor and install it as the
    /// initial `active_cursor`. Used before any client calls
    /// DefineCursor — without it, the wallpaper area shows nothing
    /// during early startup (and after that, until fvwm sets a root
    /// cursor). 16×16, 2-pixel-thick black X with a 1-pixel white halo
    /// for visibility on dark backgrounds. Hotspot at the center.
    fn install_default_cursor(&mut self) {
        let w = 16u16;
        let h = 16u16;
        let img = match PixmanImage::new(FormatCode::A8R8G8B8, w, h, true) {
            Ok(i) => i,
            Err(_) => return,
        };
        let stride_words = img.0.stride() / 4;
        // SAFETY: img is a freshly-allocated 32bpp pixman image we
        // uniquely own; bounds: y in 0..h and x in 0..w stay within
        // the allocation of size h * stride_words u32s.
        let ptr = unsafe { img.0.data() };
        let last = (w as i32) - 1;
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                // Distance to either diagonal of the 16x16 box.
                let d1 = (x - y).abs();
                let d2 = (x + y - last).abs();
                let dist = d1.min(d2);
                let pixel: u32 = match dist {
                    0 => 0xFF00_0000, // black core
                    1 => 0xFFFF_FFFF, // white halo
                    _ => 0x0000_0000, // transparent
                };
                unsafe {
                    *ptr.add(y as usize * stride_words + x as usize) = pixel;
                }
            }
        }
        let xid = self.next_host_xid();
        self.cursors.insert(
            xid,
            CursorState {
                image: img,
                hot_x: w / 2,
                hot_y: h / 2,
            },
        );
        self.active_cursor = Some(xid);
    }

    /// Borrow a drawable's Pixman image and pass it to a closure.
    #[allow(dead_code)]
    fn with_image<F, R>(&self, host_xid: u32, f: F) -> Option<R>
    where
        F: FnOnce(&PixmanImage) -> R,
    {
        if let Some(w) = self.windows.get(&host_xid) {
            let img = w.image.borrow();
            Some(f(&img))
        } else {
            self.pixmaps.get(&host_xid).map(|p| f(&p.image))
        }
    }

    /// Resolve a drawable XID to its geometry (width, height, byte
    /// stride, and a raw byte pointer to its pixel buffer). Returns
    /// `None` if no window or pixmap matches.
    ///
    /// The returned geometry borrows from `self`; while it is live, no
    /// modification of `self.windows` / `self.pixmaps` is allowed, which
    /// guarantees the data pointer stays valid for reads and writes.
    /// The pointer itself is only valid in `unsafe` blocks; callers must
    /// stay within `[0, height) × [0, width)` and respect the format's
    /// bytes-per-pixel.
    fn drawable_geometry(&self, host_xid: u32) -> Option<DrawableGeometry<'_>> {
        if let Some(w) = self.windows.get(&host_xid) {
            let img = w.image.borrow();
            // SAFETY: img is borrowed from a live RefCell on self.windows;
            // the returned pointer aliases the same buffer that future
            // pixman ops may also write through, so callers must serialise
            // their use behind `&mut self` paths.
            Some(DrawableGeometry {
                width: img.width(),
                height: img.height(),
                stride_bytes: img.stride(),
                data_ptr: unsafe { img.data() } as *mut u8,
                _phantom: std::marker::PhantomData,
            })
        } else {
            // SAFETY: as above, but borrowing directly (no RefCell on
            // PixmapState).
            self.pixmaps.get(&host_xid).map(|p| DrawableGeometry {
                width: p.image.width(),
                height: p.image.height(),
                stride_bytes: p.image.stride(),
                data_ptr: unsafe { p.image.data() } as *mut u8,
                _phantom: std::marker::PhantomData,
            })
        }
    }

    fn drawable_depth(&self, host_xid: u32) -> Option<u8> {
        self.windows
            .get(&host_xid)
            .map(|w| w.depth)
            .or_else(|| self.pixmaps.get(&host_xid).map(|p| p.depth))
    }

    /// Mutably borrow a drawable's Pixman image and pass it to a closure.
    fn with_image_mut<F, R>(&mut self, host_xid: u32, f: F) -> Option<R>
    where
        F: FnOnce(&mut PixmanImage) -> R,
    {
        if let Some(w) = self.windows.get(&host_xid) {
            let mut img = w.image.borrow_mut();
            Some(f(&mut img))
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            Some(f(&mut p.image))
        } else {
            None
        }
    }

    // Return a raw pixman pointer for a window or pixmap drawable.
    // The pointer is valid as long as the drawable is not removed from
    // self.windows / self.pixmaps. Caller must not call any method that could
    // remove the drawable while holding the pointer.
    fn image_ptr_for_xid(&self, host_xid: u32) -> Option<*mut pixman::ffi::pixman_image_t> {
        if let Some(w) = self.windows.get(&host_xid) {
            Some(w.image.borrow().0.as_ptr())
        } else {
            self.pixmaps.get(&host_xid).map(|p| p.image.0.as_ptr())
        }
    }

    fn window_under_cursor(&self) -> Option<u32> {
        // Top-levels are tracked in stacking order (bottom-to-top) on
        // self.top_level_order. Walk back-to-front so the topmost match
        // wins.
        let cx = self.cursor_x as f64;
        let cy = self.cursor_y as f64;
        for &window_id in self.top_level_order.iter().rev() {
            let Some(w) = self.windows.get(&window_id) else {
                continue;
            };
            if !w.mapped {
                continue;
            }
            // Coarse bounding-box check first.
            if cx < w.x as f64
                || cx >= w.x as f64 + w.width as f64
                || cy < w.y as f64
                || cy >= w.y as f64 + w.height as f64
            {
                continue;
            }
            // Input shape (kind=2) takes precedence; fall back to bounding (kind=0).
            let shape = self
                .shape_input
                .get(&window_id)
                .or_else(|| self.shape_bounding.get(&window_id));
            if let Some(rects) = shape {
                // Empty shape = window is unhittable.
                let inside = rects.iter().any(|r| {
                    let rx = w.x as f64 + r.x as f64;
                    let ry = w.y as f64 + r.y as f64;
                    cx >= rx && cx < rx + r.width as f64 && cy >= ry && cy < ry + r.height as f64
                });
                if !inside {
                    continue;
                }
            }
            return Some(window_id);
        }
        None
    }

    /// Diagnostic-only: dump the window tree at click time so we can
    /// reason about hit-test correctness without changing routing.
    /// Active under `RUST_LOG=yserver::kms::backend=trace`.
    fn log_hit_test_diagnostic(&self) {
        if !log::log_enabled!(log::Level::Trace) {
            return;
        }
        let cx = self.cursor_x as f64;
        let cy = self.cursor_y as f64;
        let root_id = self.window_id;
        log::trace!("hit-test: cursor=({cx:.0},{cy:.0}) root_container=0x{root_id:x}");
        // Walk top-level stacking order from bottom (first painted) to
        // top (last painted) — same order as the compositor.
        let top_levels = self.top_level_order.clone();
        for tl in &top_levels {
            let w = &self.windows[tl];
            let hit = cx >= w.x as f64
                && cx < (w.x as f64 + w.width as f64)
                && cy >= w.y as f64
                && cy < (w.y as f64 + w.height as f64);
            log::trace!(
                "  top-level 0x{tl:x} mapped={} geo=({},{}, {}x{}) children={}{}",
                w.mapped,
                w.x,
                w.y,
                w.width,
                w.height,
                w.children.len(),
                if hit { " HIT" } else { "" }
            );
            if hit && w.mapped {
                self.log_descend_diagnostic(*tl, w.x as f64, w.y as f64, cx, cy, 2);
            }
        }
    }

    fn log_descend_diagnostic(
        &self,
        parent: u32,
        parent_origin_x: f64,
        parent_origin_y: f64,
        cx: f64,
        cy: f64,
        indent: usize,
    ) {
        let pad = " ".repeat(indent * 2);
        let Some(p) = self.windows.get(&parent) else {
            return;
        };
        for &child_id in &p.children {
            let Some(c) = self.windows.get(&child_id) else {
                continue;
            };
            let child_x = parent_origin_x + c.x as f64;
            let child_y = parent_origin_y + c.y as f64;
            let hit = cx >= child_x
                && cx < child_x + c.width as f64
                && cy >= child_y
                && cy < child_y + c.height as f64;
            log::trace!(
                "{pad}child 0x{child_id:x} mapped={} parent_rel=({},{}, {}x{}) abs=({},{}) children={}{}",
                c.mapped,
                c.x,
                c.y,
                c.width,
                c.height,
                child_x as i32,
                child_y as i32,
                c.children.len(),
                if hit { " HIT" } else { "" }
            );
            if hit && c.mapped {
                self.log_descend_diagnostic(child_id, child_x, child_y, cx, cy, indent + 1);
            }
        }
    }

    /// Apply X11 ConfigureWindow `stack_mode` to a window's position
    /// inside its parent's stacking list (or `top_level_order` if it's
    /// a top-level). Implements the common Above (0) / Below (1) modes;
    /// TopIf (2), BottomIf (3), Opposite (4) fall back to Above/Below
    /// without the conditional check (sufficient for fvwm/xterm popups).
    fn restack_window(&mut self, host_xid: u32, stack_mode: u8, sibling: Option<u32>) {
        let parent_xid = match self.windows.get(&host_xid).and_then(|w| w._parent) {
            Some(p) => p,
            None => return,
        };
        let stack: &mut Vec<u32> = if parent_xid == self.window_id {
            &mut self.top_level_order
        } else {
            match self.windows.get_mut(&parent_xid) {
                Some(p) => &mut p.children,
                None => return,
            }
        };
        // Remove the current entry; we'll reinsert at the right position.
        let Some(_pos) = stack.iter().position(|&x| x == host_xid) else {
            return;
        };
        stack.retain(|&x| x != host_xid);

        // Find sibling position if specified.
        let sibling_pos = sibling.and_then(|sib| stack.iter().position(|&x| x == sib));

        match stack_mode {
            // Above: place above sibling, or at top if no sibling.
            0 | 2 | 4 => {
                if let Some(sp) = sibling_pos {
                    stack.insert(sp + 1, host_xid);
                } else {
                    stack.push(host_xid);
                }
            }
            // Below: place below sibling, or at bottom if no sibling.
            1 | 3 => {
                if let Some(sp) = sibling_pos {
                    stack.insert(sp, host_xid);
                } else {
                    stack.insert(0, host_xid);
                }
            }
            _ => {
                stack.push(host_xid); // unknown mode → treat as Above
            }
        }
    }

    fn serialize_modifiers(&self) -> u16 {
        let state = &self.xkb_state.0;
        let flags = xkbcommon::xkb::STATE_MODS_EFFECTIVE;
        let mut mask: u16 = 0;
        if state.mod_name_is_active("Shift", flags) {
            mask |= 0x01;
        }
        if state.mod_name_is_active("Lock", flags) {
            mask |= 0x02;
        }
        if state.mod_name_is_active("Control", flags) {
            mask |= 0x04;
        }
        if state.mod_name_is_active("Mod1", flags) {
            mask |= 0x08;
        }
        if state.mod_name_is_active("Mod2", flags) {
            mask |= 0x10;
        }
        if state.mod_name_is_active("Mod3", flags) {
            mask |= 0x20;
        }
        if state.mod_name_is_active("Mod4", flags) {
            mask |= 0x40;
        }
        if state.mod_name_is_active("Mod5", flags) {
            mask |= 0x80;
        }
        mask
    }

    /// Take the libinput context out of `self`. The standalone yserver
    /// binary hands the context to `input_thread::run` so libinput
    /// dispatch happens off the core thread; events arrive at the
    /// backend through `Backend::on_host_input` instead.
    pub fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        self.input_ctx.take()
    }

    /// Update the local xkbcommon state from a libinput key event and
    /// build the cooked `HostKeyEvent` for fanout.
    ///
    /// `raw.keycode` is already the X11 keycode (evdev + 8) as supplied
    /// by the libinput thread. The xkb update needs the same value.
    fn cook_host_key(&mut self, raw: HostKeyEvent) -> HostKeyEvent {
        let xkb_keycode = xkbcommon::xkb::Keycode::new(u32::from(raw.keycode));
        let direction = if raw.pressed {
            xkbcommon::xkb::KeyDirection::Down
        } else {
            xkbcommon::xkb::KeyDirection::Up
        };
        self.xkb_state.0.update_key(xkb_keycode, direction);
        HostKeyEvent {
            state: self.serialize_modifiers(),
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x: self.cursor_x as i16,
            event_y: self.cursor_y as i16,
            time: Self::current_time_ms(),
            ..raw
        }
    }

    fn process_pointer_absolute(&mut self, x: f32, y: f32) {
        self.cursor_x = x.clamp(0.0, self.fb_w as f32 - 1.0);
        self.cursor_y = y.clamp(0.0, self.fb_h as f32 - 1.0);
        self.dispatch_motion_event();
    }

    /// Compute event-window-relative coords for an event whose `host_xid`
    /// is the topmost mapped top-level under the cursor. Per X11 spec
    /// `event_x` / `event_y` are relative to the event window
    /// (`host_xid`); the host backend gets these from the X server, but
    /// on KMS we have to compute them by subtracting the top-level's
    /// origin from `cursor_x` / `cursor_y` (which are root-relative).
    fn event_relative_coords(&self, host_xid: u32) -> (i16, i16) {
        if let Some(w) = self.windows.get(&host_xid) {
            let ex = (self.cursor_x as i32) - (w.x as i32);
            let ey = (self.cursor_y as i32) - (w.y as i32);
            (
                ex.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                ey.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            )
        } else {
            // host_xid == 0 (no window under cursor) — fall back to root
            // coords; nested.rs treats event_x/y as a positional hint and
            // re-derives target coords from its own tree walk anyway.
            (self.cursor_x as i16, self.cursor_y as i16)
        }
    }

    fn emit_pointer(&mut self, ev: HostPointerEvent) {
        // Buffer; the input thread drains and dispatches outside the
        // backend lock. See the doc on `pending_pointer_events`.
        self.pending_pointer_events.push(ev);
    }

    fn current_time_ms() -> u32 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32
    }

    /// Synthesize an EnterNotify/LeaveNotify on `host_xid` with the given
    /// `crossing_mode` (0=NotifyNormal, 1=NotifyGrab, 2=NotifyUngrab).
    fn emit_crossing(
        &mut self,
        host_xid: u32,
        kind: PointerEventKind,
        crossing_mode: u8,
        state: u16,
    ) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            kind,
            host_xid,
            detail: 0, // NotifyAncestor
            time: Self::current_time_ms(),
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode,
        };
        self.emit_pointer(ev);
    }

    /// If the top-level under the cursor changed since the last motion,
    /// emit Leave(NotifyNormal) on the old top-level and Enter(NotifyNormal)
    /// on the new one. Hover-tracking widgets (and toolkits' button-state
    /// machines) need these to know when the cursor enters/leaves them.
    fn update_pointer_window(&mut self, new_xid: u32, state: u16) {
        if self.prev_pointer_window == Some(new_xid) {
            return;
        }
        if let Some(prev) = self.prev_pointer_window {
            self.emit_crossing(prev, PointerEventKind::LeaveNotify, 0, state);
        }
        self.emit_crossing(new_xid, PointerEventKind::EnterNotify, 0, state);
        self.prev_pointer_window = Some(new_xid);
    }

    fn dispatch_motion_event(&mut self) {
        // Fall back to the root container so server.rs can deliver
        // to root-window subscribers (e16's right-click-desktop menu,
        // fvwm3's root bindings) when the cursor is over the
        // wallpaper / no top-level window.
        let host_xid = self.window_under_cursor().unwrap_or(self.window_id);
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        // X11 KeyButMask: low byte modifiers, bits 8..=12 button1..button5.
        let mask = self.serialize_modifiers() | self.button_mask;
        self.update_pointer_window(host_xid, mask);
        let ev = HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            host_xid,
            detail: 0,
            time: Self::current_time_ms(),
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x,
            event_y,
            state: mask,
            crossing_mode: 0,
        };
        self.emit_pointer(ev);
    }

    fn process_pointer_button(
        &mut self,
        code: u32,
        pressed: bool,
        server_state: &yserver_core::server::ServerState,
    ) {
        let detail = match code {
            0x110 => 1, // BTN_LEFT
            0x111 => 3, // BTN_RIGHT
            0x112 => 2, // BTN_MIDDLE
            0x113 => 8, // BTN_SIDE
            0x114 => 9, // BTN_EXTRA
            _ => {
                log::debug!("unmapped libinput button code 0x{code:x}, dropping");
                return;
            }
        };
        log::trace!("libinput button code=0x{code:x} pressed={pressed} → X11 detail={detail}");
        if pressed {
            self.log_hit_test_diagnostic();
        }
        // Fall back to the root container so server.rs can deliver
        // to root-window subscribers (e16's right-click-desktop menu,
        // fvwm3's root bindings) when the cursor is over the
        // wallpaper / no top-level window.
        let host_xid = self.window_under_cursor().unwrap_or(self.window_id);
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        // X11 KeyButMask: low byte modifier mask, bits 8..=12 for
        // ButtonNMask. Per X11 spec, the `state` field describes the
        // logical button state IMMEDIATELY BEFORE the event takes
        // effect, so:
        //   ButtonPress: button bit not yet set
        //   ButtonRelease: button bit still set
        //   MotionNotify: all currently-held buttons
        // Without these bits, fvwm's drag-detection on MotionNotify
        // sees state=0 and treats motion-during-press as idle motion.
        let button_bit = match detail {
            1 => 0x0100, // Button1Mask
            2 => 0x0200,
            3 => 0x0400,
            4 => 0x0800,
            5 => 0x1000,
            _ => 0,
        };
        let modifier_mask = self.serialize_modifiers();
        let state = if pressed {
            modifier_mask | self.button_mask
        } else {
            modifier_mask | self.button_mask | button_bit
        };
        // Update held-button state AFTER computing the event's `state`,
        // so subsequent motions see the new mask.
        if pressed {
            self.button_mask |= button_bit;
        } else {
            self.button_mask &= !button_bit;
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        let kind = if pressed {
            PointerEventKind::ButtonPress
        } else {
            PointerEventKind::ButtonRelease
        };
        let ptr_event = HostPointerEvent {
            kind,
            host_xid,
            detail,
            time,
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode: 0,
        };
        self.emit_pointer(ptr_event);
        // G3: spec-correct implicit-grab crossings. X11 protocol:
        // a press that creates an implicit pointer grab walks Leaves
        // up from the focus window to the deepest common ancestor of
        // focus + grab, then Enters down to the grab window; release
        // walks the symmetric Ungrab pairs back. Detail codes
        // (NotifyAncestor / NotifyVirtual / NotifyInferior /
        // NotifyNonlinear / NotifyNonlinearVirtual) come from the
        // pure helper `crossings::implicit_grab_crossings`. The
        // crossing-mode field marks the activation: 1 = NotifyGrab
        // on press, 2 = NotifyUngrab on release.
        let post_state = self.serialize_modifiers() | self.button_mask;
        let press_mode: u8 = if pressed { 1 } else { 2 };
        let press_kind = if pressed {
            PointerEventKind::LeaveNotify
        } else {
            PointerEventKind::EnterNotify
        };
        let post_kind = if pressed {
            PointerEventKind::EnterNotify
        } else {
            PointerEventKind::LeaveNotify
        };

        // Resolve focus + grab to nested ResourceIds via xid_map.
        let grab_id = self.xid_map.get(&host_xid).copied();
        let focus_id = self
            .prev_pointer_window
            .and_then(|prev| self.xid_map.get(&prev).copied());

        match (focus_id, grab_id) {
            (Some(focus), Some(grab)) => {
                let events =
                    yserver_core::crossings::implicit_grab_crossings(server_state, focus, grab);
                if events.is_empty() {
                    // focus == grab: a single mode-stamped crossing
                    // pair is sufficient (Leave→Enter on press,
                    // Enter→Leave on release) on the same window.
                    self.emit_crossing(host_xid, press_kind, press_mode, post_state);
                    self.emit_crossing(host_xid, post_kind, press_mode, post_state);
                } else {
                    for ev in events {
                        let win_host_xid = server_state
                            .resources
                            .window(ev.window)
                            .and_then(|w| w.host_xid.map(|h| h.as_raw()))
                            .unwrap_or(host_xid);
                        let kind = match ev.kind {
                            yserver_core::crossings::CrossingKind::Enter => {
                                PointerEventKind::EnterNotify
                            }
                            yserver_core::crossings::CrossingKind::Leave => {
                                PointerEventKind::LeaveNotify
                            }
                        };
                        self.emit_crossing(win_host_xid, kind, press_mode, post_state);
                    }
                }
            }
            _ => {
                // Either focus or grab isn't a known nested window;
                // fall back to the one-event approximation.
                self.emit_crossing(host_xid, press_kind, press_mode, post_state);
                self.emit_crossing(host_xid, post_kind, press_mode, post_state);
            }
        }
    }

    /// Acquire a swapchain buffer per output, composite all visible windows
    /// onto it (translated into per-output scanout coordinates), draw the
    /// software cursor, and submit the flip. Called by the epoll loop on
    /// page-flip completion or on a timer.
    pub fn composite_and_flip(&mut self) -> io::Result<()> {
        let top_levels: Vec<u32> = self.top_level_order.clone();

        // Pre-filter visible top-levels per output (spec §2.5: avoid
        // descending whole off-screen subtrees).
        let visible_per_output: Vec<Vec<u32>> = self
            .outputs
            .iter()
            .map(|layout| {
                let bbox = layout.rect();
                top_levels
                    .iter()
                    .copied()
                    .filter(|&id| self.window_intersects(id, bbox))
                    .collect()
            })
            .collect();

        #[allow(clippy::needless_range_loop)] // index needed to split &mut/& borrows on self
        for layout_idx in 0..self.outputs.len() {
            let visible = &visible_per_output[layout_idx];
            // Acquire swapchain buffer; if none available, skip this output
            // for this frame.
            let Some(buf_idx) = self.outputs[layout_idx].swapchain.acquire_idx() else {
                continue;
            };

            // Wrap the swapchain buffer as a transient PixmanImage. SAFETY:
            // the buffer is owned by the swapchain and outlives this image.
            let mut scanout = {
                let buf = self.outputs[layout_idx].swapchain.buffer_mut(buf_idx);
                let w = buf.width();
                let h = buf.height();
                let stride_bytes = buf.stride() as usize;
                let pixels = buf.pixels_mut().as_mut_ptr();
                unsafe {
                    PixmanImage::from_buffer(
                        FormatCode::X8R8G8B8,
                        w,
                        h,
                        pixels,
                        stride_bytes,
                        false,
                    )?
                }
            };

            self.paint_output(&mut scanout, layout_idx, visible);

            // Drop scanout (releases mutable borrow on the swapchain buffer)
            // before page-flip submit.
            drop(scanout);

            let fb_id = self.outputs[layout_idx].swapchain.buffer(buf_idx).fb_id();
            drm::page_flip::submit_flip(&self.device, &self.outputs[layout_idx].output, fb_id)?;
            self.outputs[layout_idx]
                .swapchain
                .submit(buf_idx)
                .map_err(|e| io::Error::other(format!("swapchain.submit: {e}")))?;
        }

        Ok(())
    }

    /// Paint a single output's scanout image. Translates virtual-screen
    /// coordinates by `(-layout.x, -layout.y)` so layout `(layout.x,
    /// layout.y)` lands at scanout `(0, 0)`. Pixman implicitly clips writes
    /// outside the destination image.
    fn paint_output(&self, scanout: &mut PixmanImage, layout_idx: usize, visible: &[u32]) {
        let layout_x = self.outputs[layout_idx].x;
        let layout_y = self.outputs[layout_idx].y;
        let layout_w = self.outputs[layout_idx].width;
        let layout_h = self.outputs[layout_idx].height;

        // Fill root background; fall back to mid-grey so client windows stand out.
        {
            let bg_color = self
                .bg_pixel
                .map(color_from_u32)
                .unwrap_or_else(|| Color::new(0x5050, 0x5050, 0x5050, 0xffff));
            let root_rect = Rectangle16 {
                x: 0,
                y: 0,
                width: layout_w,
                height: layout_h,
            };
            let _ = scanout
                .0
                .fill_rectangles(Operation::Src, bg_color, &[root_rect]);
        }

        // Overlay background pixmap if set (e.g. from Esetroot / fvwm-root).
        // The pixmap is sized to the virtual-screen extent (fb_w/fb_h); use
        // pixman's source offset so the pixmap pixel at (layout_x, layout_y)
        // lands at scanout (0, 0). composite32 src_x/src_y are documented as
        // source-image offsets by pixman_image_composite32.
        if let Some(pm) = self.bg_pixmap {
            let bg_img: Option<&PixmanImage> = self
                .pixmaps
                .get(&pm.as_raw())
                .map(|p| &p.image)
                .or(self.bg_pixmap_image.as_ref());
            if let Some(img) = bg_img {
                scanout.0.composite32(
                    Operation::Src,
                    &img.0,
                    None,
                    (layout_x, layout_y),
                    (0, 0),
                    (0, 0),
                    (i32::from(layout_w), i32::from(layout_h)),
                );
            }
        }

        // Composite each visible top-level in stacking order, translated into
        // scanout coordinates.
        for &window_id in visible {
            self.composite_window_into_offset(scanout, window_id, -layout_x, -layout_y);
        }

        // Draw the software cursor onto the scanout, translated by the
        // layout offset.
        self.draw_cursor_onto_offset(scanout, layout_x, layout_y);
    }

    /// Return whether the (mapped) top-level window's bounding box overlaps
    /// `rect` in virtual-screen coordinates. Used by the per-output painter
    /// to skip whole off-screen subtrees.
    fn window_intersects(&self, window_id: u32, rect: Rect) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if !window.mapped {
            return false;
        }
        let (ox, oy) = self.absolute_origin(window_id);
        // absolute_origin returns the top-left in virtual-screen coords; for a
        // top-level the parent is the root, so this equals (window.x, window.y).
        #[allow(clippy::cast_possible_truncation)]
        let wx = ox as i32;
        #[allow(clippy::cast_possible_truncation)]
        let wy = oy as i32;
        let wx2 = wx.saturating_add(i32::from(window.width));
        let wy2 = wy.saturating_add(i32::from(window.height));
        let bx2 = rect.x.saturating_add(rect.w);
        let by2 = rect.y.saturating_add(rect.h);
        wx < bx2 && rect.x < wx2 && wy < by2 && rect.y < wy2
    }

    /// Top-level entry point used by the per-output painter. Applies an
    /// `(ox, oy)` translation to the top-level window's `(x, y)`, so child
    /// recursion (which works in parent-relative coordinates) doesn't need
    /// to know about the layout offset.
    fn composite_window_into_offset(
        &self,
        parent_img: &mut PixmanImage,
        window_id: u32,
        ox: i32,
        oy: i32,
    ) {
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        if !window.mapped {
            return;
        }

        // Composite children into this window's image first (parent-relative;
        // no offset translation needed for descendants).
        let children: Vec<u32> = window.children.clone();
        for &child_id in &children {
            let child_target = &mut window.image.borrow_mut();
            self.composite_window_into(child_target, child_id);
        }

        let window = &self.windows[&window_id];
        let x = i32::from(window.x) + ox;
        let y = i32::from(window.y) + oy;
        let w = i32::from(window.width);
        let h = i32::from(window.height);

        if self
            .shape_bounding
            .get(&window_id)
            .is_some_and(|r| r.is_empty())
        {
            return;
        }
        let shape_clip = build_shape_clip(self.shape_bounding.get(&window_id), x, y);
        if let Some(ref region) = shape_clip {
            let _ = parent_img.0.set_clip_region32(Some(region));
        }
        let src_img = window.image.borrow();
        parent_img.0.composite32(
            Operation::Over,
            &src_img.0,
            None,
            (0, 0),
            (0, 0),
            (x, y),
            (w, h),
        );
        if shape_clip.is_some() {
            let _ = parent_img.0.set_clip_region32(None);
        }
    }

    /// Recursively composite a window and its children into the target image.
    /// Children are composited into the window's own image first (natural clipping),
    /// then the window is composited onto the target.
    fn composite_window_into(&self, parent_img: &mut PixmanImage, window_id: u32) {
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        if !window.mapped {
            return;
        }

        // Composite children into this window's image first
        let children: Vec<u32> = window.children.clone();
        for &child_id in &children {
            let child_target = &mut window.image.borrow_mut();
            self.composite_window_into(child_target, child_id);
        }

        // Now composite the window (with its children painted) onto the parent
        let window = &self.windows[&window_id];
        let x = i32::from(window.x);
        let y = i32::from(window.y);
        let w = i32::from(window.width);
        let h = i32::from(window.height);

        // Skip compositing if shape is explicitly empty.
        if self
            .shape_bounding
            .get(&window_id)
            .is_some_and(|r| r.is_empty())
        {
            return;
        }

        let shape_clip = build_shape_clip(self.shape_bounding.get(&window_id), x, y);
        if let Some(ref region) = shape_clip {
            let _ = parent_img.0.set_clip_region32(Some(region));
        }
        let src_img = window.image.borrow();
        parent_img.0.composite32(
            Operation::Over,
            &src_img.0,
            None,
            (0, 0),
            (0, 0),
            (x, y),
            (w, h),
        );
        if shape_clip.is_some() {
            let _ = parent_img.0.set_clip_region32(None);
        }
    }

    /// Resolve the effective cursor for the window currently under the
    /// pointer. Walks the window's parent chain looking for the
    /// closest non-None (non-zero) cursor attribute. Falls back to
    /// `self.active_cursor` (the root container's cursor) and finally
    /// to `None` (no cursor drawn).
    fn effective_cursor(&self) -> Option<u32> {
        // Start at the deepest window the pointer is inside, then walk
        // up. window_under_cursor returns a top-level; we descend into
        // children using the current cursor coordinates.
        let mut current = self.window_under_cursor();
        let cx = self.cursor_x as f64;
        let cy = self.cursor_y as f64;
        if let Some(top) = current {
            // Walk down to deepest descendant containing cursor.
            current = Some(self.descend_for_cursor(top, cx, cy));
        }
        let mut node = current;
        while let Some(xid) = node {
            if let Some(w) = self.windows.get(&xid) {
                if w.cursor != 0 {
                    return Some(w.cursor);
                }
                node = w._parent;
                continue;
            }
            break;
        }
        self.active_cursor
    }

    fn descend_for_cursor(&self, window: u32, cx: f64, cy: f64) -> u32 {
        let Some(w) = self.windows.get(&window) else {
            return window;
        };
        // Build absolute origin by walking up.
        let (ox, oy) = self.absolute_origin(window);
        for &child_id in w.children.iter().rev() {
            let Some(c) = self.windows.get(&child_id) else {
                continue;
            };
            if !c.mapped {
                continue;
            }
            let cx0 = ox + c.x as f64;
            let cy0 = oy + c.y as f64;
            if cx >= cx0 && cx < cx0 + c.width as f64 && cy >= cy0 && cy < cy0 + c.height as f64 {
                return self.descend_for_cursor(child_id, cx, cy);
            }
        }
        window
    }

    fn absolute_origin(&self, window: u32) -> (f64, f64) {
        let mut ox = 0.0;
        let mut oy = 0.0;
        let mut node = Some(window);
        while let Some(xid) = node {
            if let Some(w) = self.windows.get(&xid) {
                ox += w.x as f64;
                oy += w.y as f64;
                node = w._parent.filter(|p| *p != self.window_id);
            } else {
                break;
            }
        }
        (ox, oy)
    }

    /// Draw the cursor onto the scanout image, alpha-composited at the
    /// hotspot-adjusted position. The visible cursor is whatever the
    /// window under the pointer (or its closest non-None ancestor) has
    /// set via DefineCursor.
    /// Draw the cursor onto the scanout image, translated by `(-layout_x,
    /// -layout_y)` so virtual-screen cursor coordinates land in scanout
    /// coordinates. Pixman implicitly clips writes that fall outside the
    /// destination, so cursors that straddle output boundaries draw their
    /// visible portion on each scanout.
    fn draw_cursor_onto_offset(&self, scanout: &mut PixmanImage, layout_x: i32, layout_y: i32) {
        let Some(cursor_xid) = self.effective_cursor() else {
            return;
        };
        let Some(cs) = self.cursors.get(&cursor_xid) else {
            return;
        };
        let x = self.cursor_x as i32 - i32::from(cs.hot_x) - layout_x;
        let y = self.cursor_y as i32 - i32::from(cs.hot_y) - layout_y;
        let w = cs.image.0.width() as i32;
        let h = cs.image.0.height() as i32;
        scanout.0.composite32(
            Operation::Over,
            &cs.image.0,
            None,
            (0, 0),
            (0, 0),
            (x, y),
            (w, h),
        );
    }

    /// Render a string of character bytes onto a drawable using the current font.
    /// Each byte is treated as a character index into the loaded font.
    fn render_text_string(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[u8],
    ) -> io::Result<()> {
        let chars: Vec<char> = text.iter().map(|&b| b as char).collect();
        self.render_text_chars(host_xid, foreground, x, y, &chars)
    }

    fn render_text_chars(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[char],
    ) -> io::Result<()> {
        let Some(font_xid) = self.current_font else {
            return Ok(());
        };

        // Phase 1: render all glyphs into owned pixel buffers while holding
        // the RefCell borrow.  We must drop the borrow before phase 2 so that
        // with_image_mut (which requires &mut self) can be called.
        struct RenderedGlyph {
            dst_x: i32,
            dst_y: i32,
            w: usize,
            h: usize,
            pixels: Vec<u8>, // row-major, w*h bytes
            #[allow(dead_code)]
            advance: i32,
        }

        let mut rendered: Vec<RenderedGlyph> = Vec::new();
        let mut cursor_x = x;

        {
            let Some(fs) = self.fonts.get(&font_xid) else {
                return Ok(());
            };
            let face = fs.face.borrow();
            let char_cache = &fs.char_info_cache;

            for &ch in text {
                let Some(ci) = char_cache.get(&ch) else {
                    cursor_x += 6;
                    continue;
                };

                let _ = face
                    .0
                    .load_char(ch as usize, freetype::face::LoadFlag::RENDER);
                let glyph = face.0.glyph();
                let bitmap = glyph.bitmap();

                if bitmap.width() > 0 && bitmap.rows() > 0 {
                    let w = bitmap.width() as usize;
                    let h = bitmap.rows() as usize;
                    let stride = bitmap.pitch();
                    let buf = bitmap.buffer();

                    let mut pixels = vec![0u8; w * h];
                    for row in 0..h {
                        let src = if stride >= 0 {
                            row * stride as usize
                        } else {
                            (h - 1 - row) * (stride as isize).unsigned_abs()
                        };
                        pixels[row * w..row * w + w].copy_from_slice(&buf[src..src + w]);
                    }

                    rendered.push(RenderedGlyph {
                        dst_x: cursor_x + glyph.bitmap_left(),
                        dst_y: y - glyph.bitmap_top(),
                        w,
                        h,
                        pixels,
                        advance: ci.character_width as i32,
                    });
                }
                cursor_x += ci.character_width as i32;
            }
        } // RefCell borrow released here

        // Phase 2: composite each glyph onto the destination drawable.
        let fg_color = color_from_u32(foreground);
        for g in &rendered {
            let mut color_img = Image::new(FormatCode::A8R8G8B8, 1, 1, true)
                .map_err(|_| io::Error::other("pixman color image"))?;
            let _ = color_img.fill_rectangles(
                Operation::Src,
                fg_color,
                &[Rectangle16 {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                }],
            );
            // The 1×1 solid-colour source must tile across the full glyph
            // width/height.  Without REPEAT_NORMAL, pixman returns transparent
            // black for any source read outside (0, 0), making Operation::Over
            // a no-op for every column except the leftmost — producing scattered
            // dots (only pixels where glyph-col=0 has non-zero alpha are drawn).
            color_img.set_repeat(Repeat::Normal);

            // A8 image: pixman allocates with 4-byte row stride so we must
            // write byte-by-byte using the actual stride, not width.
            let glyph_img = Image::new(FormatCode::A8, g.w, g.h, true)
                .map_err(|_| io::Error::other("pixman glyph image"))?;
            let stride_bytes = glyph_img.stride();
            // SAFETY: gdata points into the pixman-allocated buffer for
            // glyph_img which lives for this block.  We write only within
            // [0, (h-1)*stride_bytes + (w-1)] which is inside the allocation.
            let gdata = unsafe { glyph_img.data() } as *mut u8;
            for row in 0..g.h {
                for col in 0..g.w {
                    unsafe {
                        *gdata.add(row * stride_bytes + col) = g.pixels[row * g.w + col];
                    }
                }
            }

            self.with_image_mut(host_xid, |dst| {
                let dst_w = dst.0.width() as i32;
                let dst_h = dst.0.height() as i32;
                // Skip glyphs that fall entirely outside the destination.
                // Some clients send extreme negative coords during probes;
                // pixman's composite32 has historically struggled with very
                // large negative offsets in our build, so guard explicitly.
                if g.dst_x + (g.w as i32) <= 0
                    || g.dst_y + (g.h as i32) <= 0
                    || g.dst_x >= dst_w
                    || g.dst_y >= dst_h
                {
                    return;
                }
                dst.0.composite32(
                    Operation::Over,
                    &color_img,
                    Some(&glyph_img),
                    (0, 0),
                    (0, 0),
                    (g.dst_x, g.dst_y),
                    (g.w as i32, g.h as i32),
                );
            });
        }
        Ok(())
    }

    /// Return the scanout framebuffer dimensions.
    pub fn fb_dimensions(&self) -> (u16, u16) {
        (self.fb_w, self.fb_h)
    }

    /// Build the RANDR output records for every connected DRM output.
    ///
    /// ID allocation per spec §2.6.1:
    /// - outputs `1..=N`
    /// - CRTCs `(N+1)..=2N`
    /// - modes `2N+1..` deduped by `(width, height, vrefresh)`
    #[must_use]
    pub fn randr_outputs(&self) -> Vec<yserver_core::randr::RandrOutput> {
        use yserver_core::randr::RandrOutput;
        let n = self.outputs.len();
        let mut mode_ids: HashMap<(u16, u16, u32), u32> = HashMap::new();
        #[allow(clippy::cast_possible_truncation)]
        let mut next_mode_id: u32 = (2 * n + 1) as u32;
        self.outputs
            .iter()
            .enumerate()
            .map(|(i, layout)| {
                let vrefresh = layout.output.picked.vrefresh;
                let key = (layout.width, layout.height, vrefresh);
                let mode_id = *mode_ids.entry(key).or_insert_with(|| {
                    let id = next_mode_id;
                    next_mode_id += 1;
                    id
                });
                #[allow(clippy::cast_possible_truncation)]
                let output_id = (i + 1) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let crtc_id = (n + i + 1) as u32;
                RandrOutput {
                    name: layout.output.connector_name.clone(),
                    output_id,
                    crtc_id,
                    mode_id,
                    x: i16::try_from(layout.x).unwrap_or(i16::MAX),
                    y: i16::try_from(layout.y).unwrap_or(i16::MAX),
                    width: layout.width,
                    height: layout.height,
                    vrefresh,
                }
            })
            .collect()
    }

    /// Return the raw libinput fd for epoll registration, if available.
    pub fn input_fd(&self) -> Option<std::os::unix::io::RawFd> {
        self.input_ctx.as_ref().map(|ctx| ctx.fd())
    }

    /// Return the DRM device fd for epoll registration.
    pub fn drm_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::fd::{AsFd, AsRawFd};
        self.device.as_fd().as_raw_fd()
    }

    /// Drain pending page-flip events, acquire the next swapchain buffer,
    /// composite all windows onto it, draw the cursor, and submit a new flip.
    pub fn drain_page_flips_and_composite(&mut self) -> io::Result<()> {
        use ::drm::control::crtc;
        let mut flipped: Vec<crtc::Handle> = Vec::new();
        drm::page_flip::drain_events(&self.device, |c| flipped.push(c))?;

        for c in flipped {
            if let Some(layout) = self.outputs.iter_mut().find(|o| o.output.crtc == c) {
                if let Some(idx) = layout.swapchain.submitted_idx() {
                    layout
                        .swapchain
                        .complete(idx)
                        .map_err(|e| io::Error::other(format!("swapchain.complete: {e}")))?;
                }
            } else {
                log::warn!("page-flip event for unknown CRTC {c:?}");
            }
        }
        // Always composite on flip completion (self-driving at vsync)
        self.composite_and_flip()
    }

    /// Disable each DRM output (CRTC + plane) for clean shutdown.
    /// Logs any per-output error and returns the last one so callers
    /// still see a failure, while attempting to tear down everything.
    pub fn disable_output(&self) -> io::Result<()> {
        let mut last_err: Option<io::Error> = None;
        for layout in &self.outputs {
            if let Err(e) = drm::modeset::disable_output(&self.device, &layout.output) {
                log::warn!(
                    "disable_output failed for {}: {e}",
                    layout.output.connector_name
                );
                last_err = Some(e);
            }
        }
        last_err.map_or(Ok(()), Err)
    }
}

impl Backend for KmsBackend {
    fn window_id(&self) -> u32 {
        self.window_id
    }

    fn root_visual_xid(&self) -> u32 {
        self.root_visual_xid
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        None
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        None
    }

    fn render_opcode(&self) -> Option<u8> {
        // X11 conventional major opcode for RENDER. Advertising RENDER as
        // present (with all 21 render_* trait methods stubbed below as
        // no-ops) is enough to flip fvwm3 from a two-level frame hierarchy
        // into a single-level one — without RENDER fvwm3 builds a deeper
        // frame, which makes GetGeometry on client windows return (0,0)
        // and traps FvwmPager's init loop. See:
        // docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md.
        Some(133)
    }

    fn xkb_opcode(&self) -> Option<u8> {
        None
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        None
    }

    fn composite_opcode(&self) -> Option<u8> {
        None
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        // Pass the format ID through as an opaque handle. For zero (no mask)
        // the caller (nested.rs) maps it to Some(0) directly; we only reach
        // here for nonzero values. Returning Some(ynest_fmt) is sufficient
        // for render_trapezoids to receive a non-None host_mask_format and
        // proceed; the actual format code is mapped to PIXMAN_a8 inside
        // render_trapezoids.
        if ynest_fmt == 0 {
            None
        } else {
            Some(ynest_fmt)
        }
    }

    fn ping(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// Translate a host input event into wire events and fan them out
    /// directly into `state`.
    ///
    /// Pointer events use the existing `process_pointer_*` methods,
    /// which buffer into `self.pending_pointer_events`. We drain that
    /// buffer immediately and route through
    /// `pointer_event_fanout_to_state`. The buffer is local to a
    /// single call — there is no cross-thread handoff.
    ///
    /// Key events update `xkb_state` here, then go through
    /// `key_event_fanout_to_state`. The event passed in by the libinput
    /// thread carries a placeholder modifier mask and cursor coords;
    /// we override both with the backend's authoritative values.
    fn on_host_input(
        &mut self,
        state: &mut yserver_core::server::ServerState,
        ev: yserver_core::core_loop::HostInputEvent,
    ) {
        use yserver_core::core_loop::{
            HostInputEvent, key_fanout::key_event_fanout_to_state,
            pointer_fanout::pointer_event_fanout_to_state,
        };

        match ev {
            HostInputEvent::PointerMotion { x, y, time: _ } => {
                self.process_pointer_absolute(x as f32, y as f32);
            }
            HostInputEvent::PointerButton {
                button,
                pressed,
                time: _,
            } => {
                self.process_pointer_button(u32::from(button), pressed, state);
            }
            HostInputEvent::Key(raw) => {
                let cooked = self.cook_host_key(raw);
                let _dropped = key_event_fanout_to_state(state, cooked);
                return;
            }
        }

        // Drain pointer events queued by the process_pointer_* call
        // and fan each one out directly into state. The buffer must
        // be empty after this — a future on_host_input call asserts
        // it via `take`.
        let pending = std::mem::take(&mut self.pending_pointer_events);
        for ev in pending {
            let _dropped = pointer_event_fanout_to_state(state, &self.xid_map, ev, true);
        }
    }

    /// Drain DRM completion events and submit the next composite/flip.
    /// Errors are logged; the trait method is infallible because the
    /// core loop has no useful place to propagate them yet (B3 stub —
    /// E3 wires this into the poller).
    fn on_page_flip_ready(&mut self, _state: &mut yserver_core::server::ServerState) {
        if let Err(e) = self.drain_page_flips_and_composite() {
            log::warn!("kms: drain_page_flips_and_composite: {e}");
        }
    }

    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, yserver_core::backend::BackendFdKind)> {
        let mut out = Vec::with_capacity(2);
        if let Some(fd) = self.input_fd() {
            out.push((fd, yserver_core::backend::BackendFdKind::Libinput));
        }
        out.push((self.drm_fd(), yserver_core::backend::BackendFdKind::Drm));
        out
    }

    fn create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        _background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let host_xid = self.next_host_xid();
        // Pre-fill the window image with its background pixel so clients
        // that expect the X server to auto-clear (e.g. xclock drawing black
        // tick marks on a "white" background) see the right backdrop.
        let mut img = PixmanImage::new(FormatCode::X8R8G8B8, width, height, true)?;
        if let Some(pixel) = background_pixel {
            let color = color_from_u32(pixel);
            let _ = img.0.fill_rectangles(
                Operation::Src,
                color,
                &[Rectangle16 {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }],
            );
        }
        let image = RefCell::new(img);
        let depth = match visual {
            HostSubwindowVisual::CopyFromParent => 24,
            HostSubwindowVisual::Explicit { depth, .. } => depth,
        };
        let visual_xid = match visual {
            HostSubwindowVisual::CopyFromParent => 0,
            HostSubwindowVisual::Explicit { visual_xid, .. } => visual_xid,
        };
        self.windows.insert(
            host_xid,
            WindowState {
                _nested_id: ResourceId(0x0000_0100),
                x,
                y,
                width,
                height,
                border_width,
                mapped: false,
                _override_redirect: false,
                _parent: Some(host_parent.as_raw()),
                children: Vec::new(),
                bg_pixel: background_pixel,
                bg_pixmap: None,
                cursor: 0,
                image,
                depth,
                visual: visual_xid,
            },
        );
        let parent_raw = host_parent.as_raw();
        if parent_raw == self.window_id {
            // Top-level: append to stacking order (newly created → on top).
            self.top_level_order.push(host_xid);
        } else if let Some(parent) = self.windows.get_mut(&parent_raw) {
            parent.children.push(host_xid);
        }
        WindowHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create window handle"))
    }

    fn destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        // Gather sibling info before removing
        let parent_xid = self.windows.get(&host_xid).and_then(|w| w._parent);
        let siblings = if let Some(parent) = parent_xid {
            self.windows
                .get(&parent)
                .map(|p| p.children.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        if self.windows.remove(&host_xid).is_some() {
            // Update parent's children list (or top-level stacking order
            // if this was a top-level window).
            if let Some(parent_xid) = parent_xid {
                if parent_xid == self.window_id {
                    self.top_level_order.retain(|&c| c != host_xid);
                } else if let Some(parent) = self.windows.get_mut(&parent_xid) {
                    parent.children.retain(|&c| c != host_xid);
                }
            }
            self.shape_bounding.remove(&host_xid);
            self.shape_clip.remove(&host_xid);
            self.shape_input.remove(&host_xid);
        }
        self.xid_map.remove(&host_xid);

        let _ = siblings;
        Ok(())
    }

    fn map_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(window) = self.windows.get_mut(&host_xid) {
            window.mapped = true;
        }
        Ok(())
    }

    fn unmap_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        // Gather info before unmapping
        let info = self
            .windows
            .get(&host_xid)
            .map(|w| (w._parent, w.children.clone(), w.x, w.y, w.width, w.height));
        let Some((parent_xid, _children, _wx, _wy, _ww, _wh)) = info else {
            return Ok(());
        };

        // Unmap the window
        if let Some(window) = self.windows.get_mut(&host_xid) {
            window.mapped = false;
        }

        let _ = parent_xid;
        Ok(())
    }

    fn configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        let resized = self
            .windows
            .get(&host_xid)
            .is_some_and(|_w| config.width.is_some() || config.height.is_some());
        if let Some(window) = self.windows.get_mut(&host_xid) {
            if let Some(w) = config.width {
                window.width = w;
            }
            if let Some(h) = config.height {
                window.height = h;
            }
            if let Some(x) = config.x {
                window.x = x;
            }
            if let Some(y) = config.y {
                window.y = y;
            }
            if let Some(bw) = config.border_width {
                window.border_width = bw;
            }
            if resized {
                let (w, h) = (window.width, window.height);
                let mut img = PixmanImage::new(FormatCode::X8R8G8B8, w, h, true)?;
                if let Some(pixel) = window.bg_pixel {
                    let color = color_from_u32(pixel);
                    let _ = img.0.fill_rectangles(
                        Operation::Src,
                        color,
                        &[Rectangle16 {
                            x: 0,
                            y: 0,
                            width: w,
                            height: h,
                        }],
                    );
                }
                window.image = RefCell::new(img);
            }
        }
        // Apply X11 stack_mode + sibling: restack the window in its
        // parent's stacking list. Without this, fvwm's "raise menu" path
        // (ConfigureWindow stack=Above on a freshly-mapped popup) leaves
        // the window in HashMap-iteration order — which can hide the
        // popup behind unrelated top-levels.
        if let Some(stack_mode) = config.stack_mode {
            self.restack_window(host_xid, stack_mode, config.sibling);
        }
        let _ = resized;
        Ok(())
    }

    fn reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        new_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        let Some(window) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        let old_parent = window._parent;
        window._parent = Some(new_parent);
        window.x = x;
        window.y = y;
        // Remove from old parent's stacking list (or top-level order).
        if let Some(old_parent_xid) = old_parent {
            if old_parent_xid == self.window_id {
                self.top_level_order.retain(|&c| c != host_xid);
            } else if let Some(parent) = self.windows.get_mut(&old_parent_xid) {
                parent.children.retain(|&c| c != host_xid);
            }
        }
        // Append to new parent's stacking list (top of stack — X11
        // ReparentWindow semantics).
        if new_parent == self.window_id {
            self.top_level_order.push(host_xid);
        } else if let Some(parent) = self.windows.get_mut(&new_parent) {
            parent.children.push(host_xid);
        }
        Ok(())
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        let Some(window) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        let mut idx = 0;
        if value_mask & 0x01 != 0 && !values.is_empty() {
            // CWBackPixmap
            window.bg_pixmap = PixmapHandle::from_raw(values[idx]);
            idx += 1;
        }
        if value_mask & 0x02 != 0 && values.len() > idx {
            // CWBackPixel
            window.bg_pixel = Some(values[idx]);
        }
        Ok(())
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _mask: u32,
        _enabled: bool,
    ) -> io::Result<()> {
        Ok(())
    }

    fn register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        Ok(())
    }

    fn register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        Ok(())
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.xid_map.remove(&host_xid);
    }

    fn xid_map(&self) -> &HostXidMap {
        &self.xid_map
    }

    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "name_window_pixmap not supported",
        ))
    }

    fn create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let host_xid = self.next_host_xid();
        let format = match depth {
            1 => FormatCode::A1,
            8 => FormatCode::A8,
            24 => FormatCode::X8R8G8B8,
            32 => FormatCode::A8R8G8B8,
            _ => FormatCode::X8R8G8B8,
        };
        let image = PixmanImage::new(format, width, height, true)?;
        self.pixmaps.insert(
            host_xid,
            PixmapState {
                handle: host_xid,
                image,
                depth,
            },
        );
        PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create pixmap handle"))
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(ps) = self.pixmaps.remove(&host_xid) {
            let mut image = Some(ps.image);

            // Rescue into bg_pixmap_image if this is the root wallpaper pixmap.
            // Esetroot frees its pixmap after setting the root background, expecting
            // the server to keep the image alive via reference counting.
            if self.bg_pixmap.map(|h| h.as_raw()) == Some(host_xid)
                && let Some(img) = image.take()
            {
                self.bg_pixmap_image = Some(img);
            }

            // Rescue into picture_rescued_images for any picture still referencing
            // this pixmap (e.g. fvwm frees cursor source pixmap before CreateCursor).
            if image.is_some() {
                for (&pic_xid, pic) in &self.pictures {
                    if let PictureState::Drawable { host_xid: xid, .. } = pic
                        && *xid == host_xid
                        && let Some(img) = image.take()
                    {
                        self.picture_rescued_images.insert(pic_xid, img);
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn open_font(
        &mut self,
        _origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        let (face, metrics, char_cache) = self.font_loader.open_font(name)?;
        let host_xid = self.next_host_xid();
        let handle = FontHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create font handle"))?;
        self.fonts.insert(
            host_xid,
            FontState {
                handle: host_xid,
                face: RefCell::new(FreetypeFace(face)),
                metrics: metrics.clone(),
                char_info_cache: char_cache,
            },
        );
        Ok((handle, metrics))
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.fonts.remove(&host_xid);
        Ok(())
    }

    fn create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _source_pixmap: PixmapHandle,
        _mask_pixmap: Option<PixmapHandle>,
        _fore: (u16, u16, u16),
        _back: (u16, u16, u16),
        _hot_x: u16,
        _hot_y: u16,
    ) -> io::Result<CursorHandle> {
        let host_xid = self.next_host_xid();
        CursorHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create cursor handle"))
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        // Per X11: each window has its own cursor attribute. The cursor
        // visible on screen is the one belonging to the deepest window
        // under the pointer that has a non-None cursor (walking up the
        // parent chain). cursor_host_xid == 0 means "inherit from
        // parent" (X11 `None`).
        if let Some(w) = self.windows.get_mut(&host_window_xid) {
            w.cursor = cursor_host_xid;
        }
        // `active_cursor` is the sticky fallback used by effective_cursor()
        // when the walk-up hits no explicit cursor (the root container
        // isn't tracked in self.windows, so the chain always runs out
        // there). It's seeded at startup with the built-in X-shaped
        // default; a DefineCursor on the root container overrides it.
        if cursor_host_xid != 0 && host_window_xid == self.window_id {
            self.active_cursor = Some(cursor_host_xid);
        }
        Ok(())
    }

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.bg_pixel = Some(pixel);
        Ok(())
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        self.bg_pixmap = PixmapHandle::from_raw(host_pixmap_xid);
        self.bg_pixmap_image = None; // cleared; rescue fills it if the pixmap is later freed
        Ok(())
    }

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _clip_x_origin: i16,
        _clip_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _tile_x_origin: i16,
        _tile_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_clip_state(
        &mut self,
        _origin: Option<OriginContext>,
        _clip: &ClipState,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        _fill: &FillState,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        if let Some(font) = state.font {
            self.current_font = Some(font.as_raw());
        }
        self.current_function = state.function;
        self.current_foreground = state.foreground;
        self.current_background = state.background;
        Ok(())
    }

    fn copy_area(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(src_ptr) = self.image_ptr_for_xid(src_host_xid) else {
            return Ok(());
        };
        // src_ptr lives for as long as src_host_xid stays in self.windows/pixmaps;
        // we don't mutate either map in this method, so the pointer is valid for
        // the duration of the composite32 call below.
        self.with_image_mut(dst_host_xid, |dst| {
            composite32(
                Operation::Src as u32,
                src_ptr,
                std::ptr::null_mut(),
                dst,
                src_x as i32,
                src_y as i32,
                0,
                0,
                dst_x as i32,
                dst_y as i32,
                width as i32,
                height as i32,
            );
        });
        Ok(())
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        let Some(src_depth) = self.drawable_depth(src_host_xid) else {
            return Ok(());
        };
        let Some(src_geom) = self.drawable_geometry(src_host_xid) else {
            return Ok(());
        };

        let mut foreground_rects = Vec::new();
        let mut background_rects = Vec::new();
        for row in 0..height {
            let sy = i32::from(src_y) + i32::from(row);
            let dy = dst_y.saturating_add(row as i16);
            if sy < 0 {
                continue;
            }
            for col in 0..width {
                let sx = i32::from(src_x) + i32::from(col);
                let dx = dst_x.saturating_add(col as i16);
                if sx < 0 {
                    continue;
                }
                let pixel =
                    read_drawable_pixel_for_plane(&src_geom, src_depth, sx as usize, sy as usize);
                let rect = Rectangle16 {
                    x: dx,
                    y: dy,
                    width: 1,
                    height: 1,
                };
                if pixel & plane != 0 {
                    foreground_rects.push(rect);
                } else {
                    background_rects.push(rect);
                }
            }
        }

        let function = self.current_function;
        let foreground = self.current_foreground;
        let background = self.current_background;
        self.with_image_mut(dst_host_xid, |dst| {
            fill_rects_with_gc_function(dst, function, background, &background_rects);
            fill_rects_with_gc_function(dst, function, foreground, &foreground_rects);
        });
        Ok(())
    }

    fn put_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        let Some(geom) = self.drawable_geometry(host_xid) else {
            return Ok(());
        };
        let img_w = geom.width;
        let img_h = geom.height;

        match depth {
            24 | 32 => {
                // X8R8G8B8 / A8R8G8B8 — 4 bytes per pixel.
                let stride_words = geom.stride_bytes / 4;
                let dst_words = geom.data_ptr as *mut u32;
                for row in 0..height as isize {
                    let dy = dst_y as isize + row;
                    if dy < 0 || dy >= img_h as isize {
                        continue;
                    }
                    for col in 0..width as isize {
                        let dx = dst_x as isize + col;
                        if dx < 0 || dx >= img_w as isize {
                            continue;
                        }
                        let src_offset = ((row * width as isize + col) * 4) as usize;
                        if src_offset + 3 >= data.len() {
                            continue;
                        }
                        let r = data[src_offset] as u32;
                        let g = data[src_offset + 1] as u32;
                        let b = data[src_offset + 2] as u32;
                        let a = if depth == 32 {
                            data[src_offset + 3] as u32
                        } else {
                            0xFF
                        };
                        let pixel = (a << 24) | (r << 16) | (g << 8) | b;
                        // SAFETY: bounds-checked above against geom.width/height;
                        // dy*stride_words+dx fits in the buffer of size
                        // height*stride_words u32s. geom borrows self so the
                        // pointer is valid for the duration of this loop.
                        unsafe {
                            *dst_words.add(dy as usize * stride_words + dx as usize) = pixel;
                        }
                    }
                }
            }
            8 => {
                // A8 — 1 byte per pixel. Rows in X11 ZPixmap are padded to
                // 4-byte boundaries; pixman picks its own dst stride.
                let src_row_stride = (width as usize + 3) & !3;
                let dst_stride_bytes = geom.stride_bytes;
                for row in 0..height as isize {
                    let dy = dst_y as isize + row;
                    if dy < 0 || dy >= img_h as isize {
                        continue;
                    }
                    for col in 0..width as isize {
                        let dx = dst_x as isize + col;
                        if dx < 0 || dx >= img_w as isize {
                            continue;
                        }
                        let src_offset = row as usize * src_row_stride + col as usize;
                        if src_offset >= data.len() {
                            continue;
                        }
                        // SAFETY: bounds-checked against geom.width/height;
                        // dy*stride_bytes+dx fits within the per-row stride
                        // chosen by pixman.
                        unsafe {
                            *geom
                                .data_ptr
                                .add(dy as usize * dst_stride_bytes + dx as usize) =
                                data[src_offset];
                        }
                    }
                }
            }
            1 => {
                // Depth-1 PutImage targets an A1 (1 bpp) pixmap. wmaker
                // uses these as icon shape masks via MIT-SHM —
                // skipping the upload leaves the masks all-zero so
                // RENDER composites against them produce empty/clipped
                // output (visible as the wmaker appicon and title-bar
                // close/minimize buttons rendering only partially).
                //
                // X11 ZPixmap depth-1: bits packed MSB-first per byte,
                // each scanline padded to a 32-bit boundary. Pixman A1
                // uses the same convention (machine-native u32, MSB
                // bit = leftmost pixel within each byte on
                // little-endian). Row strides therefore match for
                // common widths and we can memcpy row-by-row.
                let src_row_bytes = (width as usize).div_ceil(32) * 4;
                let dst_stride_bytes = geom.stride_bytes;
                let copy_bytes = src_row_bytes.min(dst_stride_bytes);
                for row in 0..height as isize {
                    let dy = dst_y as isize + row;
                    if dy < 0 || dy >= img_h as isize {
                        continue;
                    }
                    let src_row_off = row as usize * src_row_bytes;
                    if src_row_off + copy_bytes > data.len() {
                        continue;
                    }
                    let dst_row_off = dy as usize * dst_stride_bytes;
                    // SAFETY: dst row bounds checked above (dy in
                    // 0..img_h, dst_stride_bytes covers the row).
                    // Source range checked against data.len(). Row
                    // strides match (both X11 ZPixmap d1 and pixman
                    // A1 use 32-bit-aligned scanlines).
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            data.as_ptr().add(src_row_off),
                            (geom.data_ptr).add(dst_row_off),
                            copy_bytes,
                        );
                    }
                }
            }
            _ => {
                // Unsupported depth — skip.
            }
        }
        Ok(())
    }

    fn get_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        _format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        _plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        let Some(geom) = self.drawable_geometry(host_xid) else {
            return Ok(None);
        };
        // GetImage currently only supports 32bpp source drawables (the
        // only kind the rest of the backend uses for windows / regular
        // pixmaps); A8 cursor masks etc. would need a separate path.
        let stride_words = geom.stride_bytes / 4;
        let src_words = geom.data_ptr as *const u32;
        let pixel_bytes = (width as usize) * (height as usize) * 4;
        // X11 GetImage reply: 32-byte fixed header followed by the
        // pixel payload (already 4-byte-aligned for ZPixmap depth-24).
        // The header is partly populated by nested.rs (sequence at
        // bytes 2..4 and visual at 8..12 are patched there); we
        // populate everything else so byte 0 = 1 (Reply), byte 1 =
        // depth, and bytes 4..8 = reply length in 4-byte units.
        // Without this, callers like wmaker treat byte 0 = whatever
        // the first pixel is (often 0) as an error reply, abort
        // mid-setup, and leave windows unmapped.
        let mut result = Vec::with_capacity(32 + pixel_bytes);
        let reply_length_units = (pixel_bytes / 4) as u32;
        result.push(1); // 0: Reply
        result.push(24); // 1: depth (X8R8G8B8 / A8R8G8B8 → 24-bit RGB visible)
        result.extend_from_slice(&[0u8; 2]); // 2..4: sequence (patched by nested.rs)
        result.extend_from_slice(&reply_length_units.to_le_bytes()); // 4..8: length in u32 units
        result.extend_from_slice(&[0u8; 4]); // 8..12: visual (patched by nested.rs)
        result.extend_from_slice(&[0u8; 20]); // 12..32: padding
        debug_assert_eq!(result.len(), 32);
        for row in 0..height as isize {
            let dy = y as isize + row;
            if dy < 0 || dy >= geom.height as isize {
                result.resize(result.len() + width as usize * 4, 0);
                continue;
            }
            for col in 0..width as isize {
                let dx = x as isize + col;
                if dx < 0 || dx >= geom.width as isize {
                    result.extend_from_slice(&[0; 4]);
                } else {
                    // SAFETY: bounds-checked against geom.width/height;
                    // geom borrows self so the pointer is valid.
                    let pixel = unsafe { *src_words.add(dy as usize * stride_words + dx as usize) };
                    result.extend_from_slice(&pixel.to_le_bytes());
                }
            }
        }
        Ok(Some(result))
    }

    fn poly_line(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        // X11 PolyLine: connect consecutive points with line segments.
        // coordinate_mode 0 = Origin (absolute), 1 = Previous (each point is
        // a delta from the previous).  Rasterise each segment with Bresenham.
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut prev: Option<(i32, i32)> = None;
        let mut offset = 0;
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else {
                break;
            };
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 {
                if let Some((px, py)) = prev {
                    (px + x as i32, py + y as i32)
                } else {
                    (x as i32, y as i32)
                }
            } else {
                (x as i32, y as i32)
            };
            if let Some((px, py)) = prev {
                bresenham_segment(px, py, xi, yi, &mut rects);
            }
            prev = Some((xi, yi));
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let clipped = clip_rects_to_image(&rects, img.0.width() as i32, img.0.height() as i32);
            fill_rects_with_gc_function(img, function, foreground, &clipped);
        });
        Ok(())
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        // Each segment is (x1:i16, y1:i16, x2:i16, y2:i16). Bresenham
        // rasterises diagonals correctly (axis-aligned bbox would only work
        // for horizontal / vertical segments).
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= segments.len() {
            let Some((x1, y1)) = read_i16_pair(segments, offset) else {
                break;
            };
            let Some((x2, y2)) = read_i16_pair(segments, offset + 4) else {
                break;
            };
            offset += 8;
            bresenham_segment(x1 as i32, y1 as i32, x2 as i32, y2 as i32, &mut rects);
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let clipped = clip_rects_to_image(&rects, img.0.width() as i32, img.0.height() as i32);
            fill_rects_with_gc_function(img, function, foreground, &clipped);
        });
        Ok(())
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        // Draw rectangle outlines (4 thin rectangles per rect)
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            if r.width == 0 || r.height == 0 {
                continue;
            }
            // top edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y,
                width: r.width,
                height: 1,
            });
            // bottom edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y.wrapping_add(r.height as i16).wrapping_sub(1),
                width: r.width,
                height: 1,
            });
            // left edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y,
                width: 1,
                height: r.height,
            });
            // right edge
            rects.push(Rectangle16 {
                x: r.x.wrapping_add(r.width as i16).wrapping_sub(1),
                y: r.y,
                width: 1,
                height: r.height,
            });
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            fill_rects_with_gc_function(img, function, foreground, &rects);
        });
        Ok(())
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        // Draw arc outlines.  Each arc is 12 bytes:
        //   x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16)
        // Like poly_fill_arc we treat partial-angle arcs as full ellipses
        // for now (the angle-mask refinement is a follow-up).
        //
        // Algorithm: for each scanline `py` of the bounding box, compute the
        // ellipse's inside x-range [x0, x1] and emit:
        //   - the full horizontal span at the first/last interior scanline
        //     (the top/bottom caps),
        //   - segments connecting the prev row's left/right edges to this
        //     row's left/right edges otherwise (the side outlines).
        // This produces a closed 1-pixel outline.
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let iw = img.0.width() as i32;
            let ih = img.0.height() as i32;
            let mut rects: Vec<Rectangle16> = Vec::new();
            for chunk in arcs.chunks_exact(12) {
                let ax = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
                let ay = i16::from_le_bytes([chunk[2], chunk[3]]) as i32;
                let aw = u16::from_le_bytes([chunk[4], chunk[5]]) as i32;
                let ah = u16::from_le_bytes([chunk[6], chunk[7]]) as i32;
                if aw <= 0 || ah <= 0 {
                    continue;
                }
                let cx = ax as f64 + (aw as f64) * 0.5;
                let cy = ay as f64 + (ah as f64) * 0.5;
                let rx = (aw as f64) * 0.5;
                let ry = (ah as f64) * 0.5;

                let row_at = |py: i32| -> Option<(i32, i32)> {
                    let dy = (py as f64 + 0.5 - cy) / ry;
                    if dy.abs() > 1.0 {
                        return None;
                    }
                    let dx = (1.0 - dy * dy).sqrt() * rx;
                    let x0 = (cx - dx).floor() as i32;
                    let x1 = (cx + dx).ceil() as i32;
                    Some((x0, x1))
                };

                let mut prev: Option<(i32, i32)> = None;
                for py in ay..ay + ah {
                    let Some((x0, x1)) = row_at(py) else {
                        prev = None;
                        continue;
                    };
                    let next = row_at(py + 1);
                    let cap = prev.is_none() || next.is_none();
                    if cap {
                        // Full horizontal span (top or bottom of curve).
                        rects.push(Rectangle16 {
                            x: x0 as i16,
                            y: py as i16,
                            width: (x1 - x0 + 1) as u16,
                            height: 1,
                        });
                    } else {
                        // Side connectors: left edge and right edge runs
                        // bridging this row's edge to the previous row's.
                        let (px0, px1) = prev.unwrap();
                        let l_lo = px0.min(x0);
                        let l_hi = px0.max(x0);
                        rects.push(Rectangle16 {
                            x: l_lo as i16,
                            y: py as i16,
                            width: (l_hi - l_lo + 1) as u16,
                            height: 1,
                        });
                        let r_lo = px1.min(x1);
                        let r_hi = px1.max(x1);
                        rects.push(Rectangle16 {
                            x: r_lo as i16,
                            y: py as i16,
                            width: (r_hi - r_lo + 1) as u16,
                            height: 1,
                        });
                    }
                    prev = Some((x0, x1));
                }
            }
            let clipped = clip_rects_to_image(&rects, iw, ih);
            fill_rects_with_gc_function(img, function, foreground, &clipped);
        });
        Ok(())
    }

    fn poly_point(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        _coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else {
                break;
            };
            offset += 4;
            rects.push(Rectangle16 {
                x,
                y,
                width: 1,
                height: 1,
            });
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            fill_rects_with_gc_function(img, function, foreground, &rects);
        });
        Ok(())
    }

    fn poly_fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            rects.push(r);
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            fill_rects_with_gc_function(img, function, foreground, &rects);
        });
        Ok(())
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        // Each arc is 12 bytes: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // angles are in 64ths of a degree (X11 convention).
        // We treat any arc with |angle2| >= 360*64 as a full ellipse and fill it
        // with a scanline approach. Partial arcs fall back to filling the full
        // ellipse for now; xeyes uses full circles so this is sufficient.
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let img_w = img.0.width() as i32;
            let img_h = img.0.height() as i32;
            let mut rects: Vec<Rectangle16> = Vec::new();
            for chunk in arcs.chunks_exact(12) {
                let ax = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
                let ay = i16::from_le_bytes([chunk[2], chunk[3]]) as i32;
                let aw = u16::from_le_bytes([chunk[4], chunk[5]]) as i32;
                let ah = u16::from_le_bytes([chunk[6], chunk[7]]) as i32;
                if aw <= 0 || ah <= 0 {
                    continue;
                }
                let cx = ax as f64 + (aw as f64) * 0.5;
                let cy = ay as f64 + (ah as f64) * 0.5;
                let rx = (aw as f64) * 0.5;
                let ry = (ah as f64) * 0.5;
                let y_start = ay.max(0);
                let y_end = (ay + ah).min(img_h);
                for py in y_start..y_end {
                    let dy = (py as f64 + 0.5 - cy) / ry;
                    if dy.abs() > 1.0 {
                        continue;
                    }
                    let dx = (1.0 - dy * dy).sqrt() * rx;
                    let x0 = (cx - dx).floor().max(0.0) as i32;
                    let x1 = (cx + dx).ceil().min(img_w as f64) as i32;
                    if x1 <= x0 {
                        continue;
                    }
                    rects.push(Rectangle16 {
                        x: x0 as i16,
                        y: py as i16,
                        width: (x1 - x0) as u16,
                        height: 1,
                    });
                }
            }
            if !rects.is_empty() {
                fill_rects_with_gc_function(img, function, foreground, &rects);
            }
        });
        Ok(())
    }

    fn fill_poly(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        // Parse i16 vertex pairs.  coord_mode 0 = Origin (absolute), 1 =
        // Previous (deltas from prior vertex).
        let mut verts: Vec<(i32, i32)> = Vec::with_capacity(points.len() / 4);
        let mut offset = 0;
        let mut last = (0i32, 0i32);
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else {
                break;
            };
            offset += 4;
            let (xi, yi) = if coord_mode == 1 && !verts.is_empty() {
                (last.0 + x as i32, last.1 + y as i32)
            } else {
                (x as i32, y as i32)
            };
            verts.push((xi, yi));
            last = (xi, yi);
        }
        let mut rects: Vec<Rectangle16> = Vec::new();
        scanline_fill_polygon(&verts, &mut rects);
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let clipped = clip_rects_to_image(&rects, img.0.width() as i32, img.0.height() as i32);
            fill_rects_with_gc_function(img, function, foreground, &clipped);
        });
        Ok(())
    }

    fn fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let rect = Rectangle16 {
            x,
            y,
            width,
            height,
        };
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            fill_rects_with_gc_function(img, function, foreground, &[rect]);
        });
        Ok(())
    }

    fn poly_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + text_items
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut items = &body[12..];
        let mut cursor_x = x;

        while !items.is_empty() {
            let delta = items[0] as usize;
            items = &items[1..];
            if delta == 0 {
                break; // end of items
            } else if delta == 255 {
                // Font change: skip 3 pad bytes + 4 byte fontable
                if items.len() >= 7 {
                    let font_xid = u32::from_le_bytes([items[3], items[4], items[5], items[6]]);
                    self.current_font = Some(font_xid);
                    items = &items[7..];
                } else {
                    break;
                }
            } else if delta <= 254 {
                // String item: delta bytes follow
                if items.len() >= delta {
                    let text = &items[..delta];
                    self.render_text_string(host_xid, foreground, cursor_x, y, text)?;
                    cursor_x += delta as i32;
                    items = &items[delta..];
                } else {
                    break;
                }
            }
        }
        Ok(())
    }

    fn poly_text16(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + text_items.
        // Each item is len(u8), delta(i8), then len CHAR2B values.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut cursor_x = x;
        let mut pos = 12usize;

        while pos < body.len() {
            let len = body[pos] as usize;
            pos += 1;
            if len == 0 {
                break;
            }
            if len == 255 {
                if pos + 7 <= body.len() {
                    let font_xid = u32::from_le_bytes([
                        body[pos + 3],
                        body[pos + 4],
                        body[pos + 5],
                        body[pos + 6],
                    ]);
                    self.current_font = Some(font_xid);
                    pos += 7;
                } else {
                    break;
                }
                continue;
            }
            if pos >= body.len() {
                break;
            }
            let delta = body[pos] as i8;
            pos += 1;
            cursor_x += i32::from(delta);

            let mut chars = Vec::with_capacity(len);
            for _ in 0..len {
                if pos + 2 > body.len() {
                    break;
                }
                let codepoint = u16::from_be_bytes([body[pos], body[pos + 1]]) as u32;
                pos += 2;
                chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
            }
            self.render_text_chars(host_xid, foreground, cursor_x, y, &chars)?;
            if let Some(font_state) = self.current_font.and_then(|f| self.fonts.get(&f)) {
                cursor_x += chars
                    .iter()
                    .map(|ch| {
                        font_state
                            .char_info_cache
                            .get(ch)
                            .map(|ci| ci.character_width as i32)
                            .unwrap_or(6)
                    })
                    .sum::<i32>();
            }

            let item_len = 2 + len * 2;
            let pad = (4 - (item_len % 4)) % 4;
            pos = pos.saturating_add(pad).min(body.len());
        }
        Ok(())
    }

    fn image_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + string(text_len)
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;

        // Draw background rectangle first
        if let Some(font_state) = self.current_font.and_then(|f| self.fonts.get(&f)) {
            let total_width: i32 = body[12..]
                .iter()
                .take(text_len as usize)
                .map(|&b| {
                    font_state
                        .char_info_cache
                        .get(&(b as char))
                        .map(|ci| ci.character_width as i32)
                        .unwrap_or(6)
                })
                .sum();
            let ascent = font_state.metrics.font_ascent as i32;
            let descent = font_state.metrics.font_descent as i32;
            // Clamp to i16/u16 ranges so a buggy font (huge ascent) can't
            // produce a rect that overflows pixman's internal arithmetic.
            let rect = Rectangle16 {
                x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y: (y - ascent).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                width: total_width.clamp(0, u16::MAX as i32) as u16,
                height: (ascent + descent).clamp(0, u16::MAX as i32) as u16,
            };
            let function = self.current_function;
            self.with_image_mut(host_xid, |img| {
                fill_rects_with_gc_function(img, function, background, &[rect]);
            });
        }

        // Render the string (clamp to available body bytes)
        let end = (12usize + text_len as usize).min(body.len());
        let text = &body[12..end];
        self.render_text_string(host_xid, foreground, x, y, text)
    }

    fn image_text16(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + CHAR2B[text_len].
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut chars = Vec::with_capacity(text_len as usize);
        let mut pos = 12usize;
        for _ in 0..text_len {
            if pos + 2 > body.len() {
                break;
            }
            let codepoint = u16::from_be_bytes([body[pos], body[pos + 1]]) as u32;
            pos += 2;
            chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
        }

        if let Some(font_state) = self.current_font.and_then(|f| self.fonts.get(&f)) {
            let total_width: i32 = chars
                .iter()
                .map(|ch| {
                    font_state
                        .char_info_cache
                        .get(ch)
                        .map(|ci| ci.character_width as i32)
                        .unwrap_or(6)
                })
                .sum();
            let ascent = font_state.metrics.font_ascent as i32;
            let descent = font_state.metrics.font_descent as i32;
            let rect = Rectangle16 {
                x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y: (y - ascent).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                width: total_width.clamp(0, u16::MAX as i32) as u16,
                height: (ascent + descent).clamp(0, u16::MAX as i32) as u16,
            };
            let function = self.current_function;
            self.with_image_mut(host_xid, |img| {
                fill_rects_with_gc_function(img, function, background, &[rect]);
            });
        }

        self.render_text_chars(host_xid, foreground, x, y, &chars)
    }

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        _ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        let drawable_xid = host_drawable.as_raw();
        let picture_xid = self.next_host_xid();
        self.pictures
            .insert(picture_xid, default_drawable_picture(drawable_xid));
        if value_mask != 0 {
            let mut body = Vec::with_capacity(8 + values.len());
            body.extend_from_slice(&picture_xid.to_le_bytes());
            body.extend_from_slice(&value_mask.to_le_bytes());
            body.extend_from_slice(values);
            self.render_change_picture(None, picture_xid, &body)?;
        }
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 8 {
            return Ok(());
        }
        let value_mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let values = &body[8..];
        let mut off = 0usize;
        let next_u32 = |off: &mut usize| -> Option<u32> {
            let bytes = values.get(*off..*off + 4)?;
            *off += 4;
            Some(u32::from_le_bytes(bytes.try_into().ok()?))
        };
        for bit in 0..13 {
            let mask_bit = 1u32 << bit;
            if value_mask & mask_bit == 0 {
                continue;
            }
            let Some(v) = next_u32(&mut off) else {
                break;
            };
            match mask_bit {
                // CPRepeat
                0x0001 => {
                    let repeat = match v {
                        1 => Repeat::Normal,
                        2 => Repeat::Pad,
                        3 => Repeat::Reflect,
                        _ => Repeat::None,
                    };
                    match self.pictures.get_mut(&host_pic) {
                        Some(PictureState::Drawable { repeat: r, .. }) => *r = repeat,
                        Some(PictureState::SolidFill {
                            image, repeat: r, ..
                        }) => {
                            *r = repeat;
                            image.borrow_mut().0.set_repeat(repeat);
                        }
                        Some(PictureState::Gradient {
                            image, repeat: r, ..
                        }) => {
                            *r = repeat;
                            image.0.set_repeat(repeat);
                        }
                        None => {}
                    }
                }
                // CPAlphaMap
                0x0002 => {
                    if let Some(PictureState::Drawable { alpha_map, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *alpha_map = if v == 0 { None } else { Some(v) };
                    }
                }
                // CPAlphaXOrigin
                0x0004 => {
                    if let Some(PictureState::Drawable { alpha_x, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *alpha_x = v as i16;
                    }
                }
                // CPAlphaYOrigin
                0x0008 => {
                    if let Some(PictureState::Drawable { alpha_y, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *alpha_y = v as i16;
                    }
                }
                // CPClipXOrigin
                0x0010 => {
                    if let Some(PictureState::Drawable { clip_x, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *clip_x = v as i16;
                    }
                }
                // CPClipYOrigin
                0x0020 => {
                    if let Some(PictureState::Drawable { clip_y, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *clip_y = v as i16;
                    }
                }
                // CPClipMask
                0x0040 => {
                    let new_clip = if v == 0 {
                        None
                    } else {
                        self.pixmaps.get(&v).map(|px| {
                            vec![Rectangle16 {
                                x: 0,
                                y: 0,
                                width: px.image.width().min(u16::MAX as usize) as u16,
                                height: px.image.height().min(u16::MAX as usize) as u16,
                            }]
                        })
                    };
                    if let Some(PictureState::Drawable { clip, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *clip = new_clip;
                    }
                }
                // CPGraphicsExposure
                0x0080 => {
                    if let Some(PictureState::Drawable {
                        graphics_exposure, ..
                    }) = self.pictures.get_mut(&host_pic)
                    {
                        *graphics_exposure = v != 0;
                    }
                }
                // CPSubwindowMode
                0x0100 => {
                    if let Some(PictureState::Drawable { subwindow_mode, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *subwindow_mode = v as u8;
                    }
                }
                // CPPolyEdge
                0x0200 => {
                    if let Some(PictureState::Drawable { poly_edge, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *poly_edge = v as u8;
                    }
                }
                // CPPolyMode
                0x0400 => {
                    if let Some(PictureState::Drawable { poly_mode, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *poly_mode = v as u8;
                    }
                }
                // CPDither: consumed but intentionally not stored.
                0x0800 => {}
                // CPComponentAlpha
                0x1000 => match self.pictures.get_mut(&host_pic) {
                    Some(PictureState::Drawable {
                        component_alpha, ..
                    })
                    | Some(PictureState::SolidFill {
                        component_alpha, ..
                    }) => *component_alpha = v != 0,
                    Some(PictureState::Gradient { .. }) | None => {}
                },
                _ => {}
            }
        }
        Ok(())
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        self.pictures.remove(&host_pic);
        self.picture_rescued_images.remove(&host_pic);
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        let format = match ynest_format {
            RENDER_FMT_A8 => GlyphSetFormat::A8,
            RENDER_FMT_A1 => GlyphSetFormat::A1,
            _ => GlyphSetFormat::Other,
        };
        let id = self.next_host_xid();
        self.glyphsets.insert(
            id,
            GlyphSetState {
                format,
                glyphs: HashMap::new(),
            },
        );
        Ok(GlyphSetHandle::from_raw(id))
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        self.glyphsets.remove(&host_gs);
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        if let Some(gs) = self.glyphsets.get_mut(&host_gs) {
            parse_add_glyphs(gs, body_tail);
        }
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        let Some(gs) = self.glyphsets.get_mut(&host_gs) else {
            return Ok(());
        };
        for chunk in glyph_ids.chunks_exact(4) {
            let id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            gs.glyphs.remove(&id);
        }
        Ok(())
    }

    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let pixman_op = op as u32;

        // Extract src raw pointer while pictures is immutably borrowed.
        // Also capture the underlying drawable xid (if any) to guard against self-composite.
        let (src_ptr, src_drawable_xid, src_repeat, src_transform): (
            *mut pixman::ffi::pixman_image_t,
            Option<u32>,
            Repeat,
            Option<pixman::ffi::pixman_transform_t>,
        ) = match self.pictures.get(&host_src) {
            Some(PictureState::SolidFill { image, repeat, .. }) => {
                (image.borrow().0.as_ptr(), None, *repeat, None)
            }
            Some(PictureState::Gradient {
                image,
                repeat,
                transform,
            }) => (image.0.as_ptr(), None, *repeat, *transform),
            Some(PictureState::Drawable {
                host_xid,
                repeat,
                transform,
                ..
            }) => {
                let xid = *host_xid;
                match self.image_ptr_for_xid(xid) {
                    Some(ptr) => (ptr, Some(xid), *repeat, *transform),
                    None => {
                        log::debug!("render_composite: src drawable 0x{xid:x} has no image");
                        return Ok(());
                    }
                }
            }
            None => {
                log::debug!("render_composite: host_src 0x{host_src:x} not found");
                return Ok(());
            }
        };

        // Extract mask raw pointer (null_mut if no mask).
        let (mask_ptr, mask_drawable_xid, mask_repeat, mask_transform): (
            *mut pixman::ffi::pixman_image_t,
            Option<u32>,
            Repeat,
            Option<pixman::ffi::pixman_transform_t>,
        ) = if host_mask == 0 {
            (std::ptr::null_mut(), None, Repeat::None, None)
        } else {
            match self.pictures.get(&host_mask) {
                Some(PictureState::SolidFill { image, repeat, .. }) => {
                    (image.borrow().0.as_ptr(), None, *repeat, None)
                }
                Some(PictureState::Gradient {
                    image,
                    repeat,
                    transform,
                }) => (image.0.as_ptr(), None, *repeat, *transform),
                Some(PictureState::Drawable {
                    host_xid,
                    repeat,
                    transform,
                    ..
                }) => {
                    let xid = *host_xid;
                    match self.image_ptr_for_xid(xid) {
                        Some(ptr) => (ptr, Some(xid), *repeat, *transform),
                        None => {
                            log::debug!("render_composite: mask drawable 0x{xid:x} has no image");
                            return Ok(());
                        }
                    }
                }
                None => {
                    log::debug!("render_composite: host_mask 0x{host_mask:x} not found");
                    return Ok(());
                }
            }
        };

        let (dst_xid, clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => {
                log::debug!("render_composite: host_dst 0x{host_dst:x} is not a Drawable picture");
                return Ok(());
            }
        };

        // Guard: pixman_image_composite32 is undefined behaviour if src or mask alias dst.
        // This can happen when two RENDER pictures share the same underlying drawable xid.
        if src_drawable_xid == Some(dst_xid) {
            log::debug!(
                "render_composite: src and dst are the same drawable 0x{dst_xid:x}; skipping self-composite"
            );
            return Ok(());
        }
        if mask_drawable_xid == Some(dst_xid) {
            log::debug!(
                "render_composite: mask and dst are the same drawable 0x{dst_xid:x}; skipping"
            );
            return Ok(());
        }

        unsafe {
            pixman::ffi::pixman_image_set_repeat(src_ptr, src_repeat.into());
            pixman::ffi::pixman_image_set_transform(
                src_ptr,
                src_transform
                    .as_ref()
                    .map_or(std::ptr::null(), |t| t as *const _),
            );
            if !mask_ptr.is_null() {
                pixman::ffi::pixman_image_set_repeat(mask_ptr, mask_repeat.into());
                pixman::ffi::pixman_image_set_transform(
                    mask_ptr,
                    mask_transform
                        .as_ref()
                        .map_or(std::ptr::null(), |t| t as *const _),
                );
            }
        }

        self.with_image_mut(dst_xid, |dst| {
            if let Some(ref rects) = clip {
                use pixman::{Box32, Region32};
                let boxes: Vec<Box32> = rects
                    .iter()
                    .map(|r| Box32 {
                        x1: r.x as i32,
                        y1: r.y as i32,
                        x2: r.x as i32 + r.width as i32,
                        y2: r.y as i32 + r.height as i32,
                    })
                    .collect();
                let region = Region32::init_rects(&boxes);
                let _ = dst.0.set_clip_region32(Some(&region));
            }
            // src_ptr and mask_ptr are guaranteed distinct from dst by the
            // aliasing guards above (different drawable XIDs).
            composite32(
                pixman_op,
                src_ptr,
                mask_ptr,
                dst,
                src_x as i32,
                src_y as i32,
                mask_x as i32,
                mask_y as i32,
                dst_x as i32,
                dst_y as i32,
                width as i32,
                height as i32,
            );
            if clip.is_some() {
                let _ = dst.0.set_clip_region32(None);
            }
        });

        Ok(())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        _op: u8,
        host_src: u32,
        host_dst: u32,
        _mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        // Resolve src picture — must be SolidFill (Drawable fallback: opaque black).
        let src_img = match self.pictures.get(&host_src) {
            Some(PictureState::SolidFill { image, .. }) => {
                // Clone the colour so we can drop the pictures borrow.
                let argb = unsafe { *image.borrow().0.data() };
                let mut img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true)?;
                let a = (((argb >> 24) & 0xFF) as u16) * 0x101;
                let r = (((argb >> 16) & 0xFF) as u16) * 0x101;
                let g = (((argb >> 8) & 0xFF) as u16) * 0x101;
                let b = ((argb & 0xFF) as u16) * 0x101;
                let _ = img.0.fill_rectangles(
                    Operation::Src,
                    Color::new(r, g, b, a),
                    &[Rectangle16 {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                    }],
                );
                img.0.set_repeat(Repeat::Normal);
                img
            }
            _ => {
                log::debug!(
                    "render_composite_glyphs: host_src 0x{host_src:x} is not SolidFill; using black"
                );
                let mut img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true)?;
                let _ = img.0.fill_rectangles(
                    Operation::Src,
                    Color::new(0, 0, 0, 0xFFFF),
                    &[Rectangle16 {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                    }],
                );
                img.0.set_repeat(Repeat::Normal);
                img
            }
        };

        let (dst_xid, clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => return Ok(()),
        };

        if !self.glyphsets.contains_key(&host_gs) {
            return Ok(());
        }
        let pen_x = src_x as i32 + x_off as i32;
        let pen_y = src_y as i32 + y_off as i32;

        // SAFETY: glyphsets and the pixmap image data (dst) live in disjoint
        // fields of KmsBackend. No glyphset is freed during this closure because
        // KmsBackend is !Sync and this method holds &mut self.
        let glyphsets_ptr: *const HashMap<u32, GlyphSetState> = &self.glyphsets;

        self.with_image_mut(dst_xid, |dst| {
            if let Some(ref rects) = clip {
                use pixman::{Box32, Region32};
                let boxes: Vec<Box32> = rects
                    .iter()
                    .map(|r| Box32 {
                        x1: r.x as i32,
                        y1: r.y as i32,
                        x2: r.x as i32 + r.width as i32,
                        y2: r.y as i32 + r.height as i32,
                    })
                    .collect();
                let region = Region32::init_rects(&boxes);
                let _ = dst.0.set_clip_region32(Some(&region));
            }
            let glyphsets_ref = unsafe { &*glyphsets_ptr };
            composite_glyphs_onto(
                glyphsets_ref,
                host_gs,
                &src_img,
                dst,
                minor,
                pen_x,
                pen_y,
                items,
            );
            if clip.is_some() {
                let _ = dst.0.set_clip_region32(None);
            }
        });

        Ok(())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_dst: u32,
        _op: u8,
        _color: [u8; 8],
        _rects: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        if !traps.len().is_multiple_of(40) || traps.is_empty() {
            return Ok(());
        }

        // Translate X RENDER op code to pixman op. X RENDER and pixman share
        // the same numeric values (0=Clear, 1=Src, 2=Dst, 3=Over, …).
        let pixman_op = op as u32;

        // Decode the trap wire bytes element-by-element to avoid alignment UB.
        // Each trap is 40 bytes: top(4), bottom(4), left.p1.x(4), left.p1.y(4),
        // left.p2.x(4), left.p2.y(4), right.p1.x(4), right.p1.y(4),
        // right.p2.x(4), right.p2.y(4).
        let n_traps = traps.len() / 40;
        let mut trap_vec: Vec<pixman::ffi::pixman_trapezoid_t> = Vec::with_capacity(n_traps);
        for chunk in traps.chunks_exact(40) {
            let t = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let b = i32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let lp1x = i32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
            let lp1y = i32::from_le_bytes([chunk[12], chunk[13], chunk[14], chunk[15]]);
            let lp2x = i32::from_le_bytes([chunk[16], chunk[17], chunk[18], chunk[19]]);
            let lp2y = i32::from_le_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
            let rp1x = i32::from_le_bytes([chunk[24], chunk[25], chunk[26], chunk[27]]);
            let rp1y = i32::from_le_bytes([chunk[28], chunk[29], chunk[30], chunk[31]]);
            let rp2x = i32::from_le_bytes([chunk[32], chunk[33], chunk[34], chunk[35]]);
            let rp2y = i32::from_le_bytes([chunk[36], chunk[37], chunk[38], chunk[39]]);
            trap_vec.push(pixman::ffi::pixman_trapezoid_t {
                top: t,
                bottom: b,
                left: pixman::ffi::pixman_line_fixed_t {
                    p1: pixman::ffi::pixman_point_fixed_t { x: lp1x, y: lp1y },
                    p2: pixman::ffi::pixman_point_fixed_t { x: lp2x, y: lp2y },
                },
                right: pixman::ffi::pixman_line_fixed_t {
                    p1: pixman::ffi::pixman_point_fixed_t { x: rp1x, y: rp1y },
                    p2: pixman::ffi::pixman_point_fixed_t { x: rp2x, y: rp2y },
                },
            });
        }

        // Look up source picture — must be SolidFill. Borrow and get raw ptr.
        let src_ptr = match self.pictures.get(&host_src) {
            Some(PictureState::SolidFill { image, .. }) => image.borrow().0.as_ptr(),
            _ => {
                log::debug!(
                    "render_trapezoids: host_src 0x{:x} is not a SolidFill picture; skipping",
                    host_src
                );
                return Ok(());
            }
        };

        // Look up destination picture — must be Drawable. Extract host_xid
        // and any clip info, then release the pictures borrow before we
        // mutably borrow the drawable image below.
        let (drawable_xid, clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => {
                log::debug!(
                    "render_trapezoids: host_dst 0x{:x} is not a Drawable picture; skipping",
                    host_dst
                );
                return Ok(());
            }
        };

        // Apply clip if set, composite traps, then clear clip.
        // We need to borrow the dst image mutably; use with_image_mut.
        // src_ptr is valid for the duration of this call because self.pictures
        // is not modified between obtaining src_ptr and the composite call.
        self.with_image_mut(drawable_xid, |dst| {
            // SAFETY: dst.0.as_ptr() returns a valid *mut pixman_image_t that
            // pixman allocated and that we own (inside RefCell<PixmanImage>).
            // src_ptr was obtained from another PixmanImage we also own and
            // that outlives this call (src and dst are different pictures by
            // checked contract). trap_vec is a Vec we own. n_traps matches
            // trap_vec.len().
            // Apply clip region if present.
            if let Some(ref rects) = clip {
                use pixman::{Box32, Region32};
                let boxes: Vec<Box32> = rects
                    .iter()
                    .map(|r| Box32 {
                        x1: r.x as i32,
                        y1: r.y as i32,
                        x2: r.x as i32 + r.width as i32,
                        y2: r.y as i32 + r.height as i32,
                    })
                    .collect();
                let region = Region32::init_rects(boxes.as_slice());
                let _ = dst.0.set_clip_region32(Some(&region));
            }

            // PIXMAN_a8 as mask-format gives 256-level AA for trap
            // coverage — correct for all common RENDER use cases.
            composite_trapezoids(
                pixman_op,
                src_ptr,
                dst,
                pixman::ffi::pixman_format_code_t_PIXMAN_a8,
                src_x as i32,
                src_y as i32,
                x_off as i32,
                y_off as i32,
                &trap_vec,
            );

            // Clear clip after composite to avoid stale clip affecting
            // subsequent operations on this image.
            if clip.is_some() {
                let _ = dst.0.set_clip_region32(None);
            }
        });

        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        // X RENDER CreateSolidFill color: 16-bit per channel, little-endian.
        // Byte layout: red[0..2], green[2..4], blue[4..6], alpha[6..8].
        let r = u16::from_le_bytes([color[0], color[1]]);
        let g = u16::from_le_bytes([color[2], color[3]]);
        let b = u16::from_le_bytes([color[4], color[5]]);
        let a = u16::from_le_bytes([color[6], color[7]]);
        let pixman_color = Color::new(r, g, b, a);

        // Create a 1×1 A8R8G8B8 image, fill it, and set repeat so it tiles.
        let mut img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true)?;
        let _ = img.0.fill_rectangles(
            Operation::Src,
            pixman_color,
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        );
        img.0.set_repeat(Repeat::Normal);

        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::SolidFill {
                image: RefCell::new(img),
                repeat: Repeat::Normal,
                component_alpha: false,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        if body.len() < 24 {
            return Ok(None);
        }
        let p1x = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let p1y = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let p2x = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let p2y = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let n_stops = u32::from_le_bytes(body[20..24].try_into().unwrap()) as usize;
        let pos_base = 24usize;
        let color_base = pos_base + n_stops * 4;
        if body.len() < color_base + n_stops * 8 {
            return Ok(None);
        }
        let mut stops = Vec::with_capacity(n_stops);
        for i in 0..n_stops {
            let pos = i32::from_le_bytes(
                body[pos_base + i * 4..pos_base + i * 4 + 4]
                    .try_into()
                    .unwrap(),
            );
            let cb = color_base + i * 8;
            let r = u16::from_le_bytes(body[cb..cb + 2].try_into().unwrap());
            let g = u16::from_le_bytes(body[cb + 2..cb + 4].try_into().unwrap());
            let b = u16::from_le_bytes(body[cb + 4..cb + 6].try_into().unwrap());
            let a = u16::from_le_bytes(body[cb + 6..cb + 8].try_into().unwrap());
            stops.push(pixman::ffi::pixman_gradient_stop_t {
                x: pos,
                color: pixman::ffi::pixman_color_t {
                    red: r,
                    green: g,
                    blue: b,
                    alpha: a,
                },
            });
        }
        let p1 = pixman::ffi::pixman_point_fixed_t { x: p1x, y: p1y };
        let p2 = pixman::ffi::pixman_point_fixed_t { x: p2x, y: p2y };
        let raw = unsafe {
            pixman::ffi::pixman_image_create_linear_gradient(
                &p1,
                &p2,
                stops.as_ptr(),
                stops.len() as i32,
            )
        };
        if raw.is_null() {
            return Ok(None);
        }
        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::Gradient {
                image: PixmanImage(unsafe { Image::from_ptr(raw) }),
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        if body.len() < 32 {
            return Ok(None);
        }
        let icx = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let icy = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let ocx = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let ocy = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let ir = i32::from_le_bytes(body[20..24].try_into().unwrap());
        let or_ = i32::from_le_bytes(body[24..28].try_into().unwrap());
        let n_stops = u32::from_le_bytes(body[28..32].try_into().unwrap()) as usize;
        let pos_base = 32usize;
        let color_base = pos_base + n_stops * 4;
        if body.len() < color_base + n_stops * 8 {
            return Ok(None);
        }
        let mut stops = Vec::with_capacity(n_stops);
        for i in 0..n_stops {
            let pos = i32::from_le_bytes(
                body[pos_base + i * 4..pos_base + i * 4 + 4]
                    .try_into()
                    .unwrap(),
            );
            let cb = color_base + i * 8;
            let r = u16::from_le_bytes(body[cb..cb + 2].try_into().unwrap());
            let g = u16::from_le_bytes(body[cb + 2..cb + 4].try_into().unwrap());
            let b = u16::from_le_bytes(body[cb + 4..cb + 6].try_into().unwrap());
            let a = u16::from_le_bytes(body[cb + 6..cb + 8].try_into().unwrap());
            stops.push(pixman::ffi::pixman_gradient_stop_t {
                x: pos,
                color: pixman::ffi::pixman_color_t {
                    red: r,
                    green: g,
                    blue: b,
                    alpha: a,
                },
            });
        }
        let inner = pixman::ffi::pixman_point_fixed_t { x: icx, y: icy };
        let outer = pixman::ffi::pixman_point_fixed_t { x: ocx, y: ocy };
        let raw = unsafe {
            pixman::ffi::pixman_image_create_radial_gradient(
                &inner,
                &outer,
                ir,
                or_,
                stops.as_ptr(),
                stops.len() as i32,
            )
        };
        if raw.is_null() {
            return Ok(None);
        }
        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::Gradient {
                image: PixmanImage(unsafe { Image::from_ptr(raw) }),
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        let pic_xid = host_src_pic.as_raw();
        let host_xid = match self.pictures.get(&pic_xid) {
            Some(PictureState::Drawable { host_xid, .. }) => *host_xid,
            other => {
                log::debug!(
                    "render_create_cursor: pic {pic_xid} not found or not Drawable (got {:?})",
                    other.map(|_| "non-Drawable")
                );
                return Ok(None);
            }
        };

        // fvwm pattern: CreatePixmap → PutImage → CreatePicture → FreePixmap → CreateCursor.
        // The pixmap may already be freed; fall back to the rescued image saved by free_pixmap.
        let cursor_img = if let Some(pm) = self.pixmaps.get(&host_xid) {
            let w = pm.image.0.width() as u16;
            let h = pm.image.0.height() as u16;
            let mut img = PixmanImage::new(FormatCode::A8R8G8B8, w, h, true)?;
            img.0.composite32(
                Operation::Src,
                &pm.image.0,
                None,
                (0, 0),
                (0, 0),
                (0, 0),
                (w as i32, h as i32),
            );
            img
        } else if let Some(rescued) = self.picture_rescued_images.remove(&pic_xid) {
            log::debug!("render_create_cursor: using rescued image for pic {pic_xid}");
            rescued
        } else {
            log::debug!(
                "render_create_cursor: pixmap host_xid={host_xid} not found for pic {pic_xid}"
            );
            return Ok(None);
        };

        let id = self.next_host_xid();
        self.cursors.insert(
            id,
            CursorState {
                image: cursor_img,
                hot_x: x,
                hot_y: y,
            },
        );

        CursorHandle::from_raw(id)
            .map(Some)
            .ok_or_else(|| io::Error::other("cursor handle overflow"))
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body (passed through from nested.rs): picture(4) +
        // clip_x_origin(INT16) + clip_y_origin(INT16) + N × [x y w h].
        // The picture XID has already been resolved to host_pic; we just
        // skip past it. Origin offset is stored but not applied — xclock
        // sets it to (0,0) and that's all we currently exercise.
        if body.len() < 8 {
            return Ok(());
        }
        let _x_origin = i16::from_le_bytes([body[4], body[5]]);
        let _y_origin = i16::from_le_bytes([body[6], body[7]]);
        let rects_data = &body[8..];
        let mut rects = Vec::with_capacity(rects_data.len() / 8);
        for chunk in rects_data.chunks_exact(8) {
            let x = i16::from_le_bytes([chunk[0], chunk[1]]);
            let y = i16::from_le_bytes([chunk[2], chunk[3]]);
            let w = u16::from_le_bytes([chunk[4], chunk[5]]);
            let h = u16::from_le_bytes([chunk[6], chunk[7]]);
            rects.push(Rectangle16 {
                x,
                y,
                width: w,
                height: h,
            });
        }
        if let Some(PictureState::Drawable { clip, .. }) = self.pictures.get_mut(&host_pic) {
            *clip = if rects.is_empty() { None } else { Some(rects) };
        }
        // SolidFill pictures: clip is a no-op.
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 40 {
            return Ok(());
        }
        let mut matrix = [[0i32; 3]; 3];
        for (idx, slot) in matrix.iter_mut().flatten().enumerate() {
            let off = 4 + idx * 4;
            *slot = i32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        }
        let transform = if matrix == [[0x10000, 0, 0], [0, 0x10000, 0], [0, 0, 0x10000]] {
            None
        } else {
            Some(pixman::ffi::pixman_transform_t { matrix })
        };
        match self.pictures.get_mut(&host_pic) {
            Some(PictureState::Drawable { transform: t, .. })
            | Some(PictureState::Gradient { transform: t, .. }) => *t = transform,
            _ => {}
        }
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        Ok((1, 1))
    }

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        _body: &[u8],
    ) -> io::Result<Option<Vec<u8>>> {
        use crate::kms::xkb as xkb_replies;
        let reply = match minor {
            0 => Some(xkb_replies::reply_use_extension()),
            8 => Some(xkb_replies::reply_get_map(&self.xkb_keymap.0)),
            17 => Some(xkb_replies::reply_get_names(&self.xkb_keymap.0)),
            20 => Some(xkb_replies::reply_get_compat_map()),
            24 => Some(xkb_replies::reply_get_controls(&self.xkb_keymap.0)),
            _ => Some(xkb_replies::reply_minimal(minor)),
        };
        Ok(reply)
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        _origin: Option<OriginContext>,
        _host_cursor_xid: u32,
        _name_bytes: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()> {
        let map = match kind {
            0 => &mut self.shape_bounding,
            1 => &mut self.shape_clip,
            2 => &mut self.shape_input,
            _ => return Ok(()),
        };
        // Store always: server sends the full window rect when shape is cleared
        // (shape_rects_for fallback), and the actual rects otherwise.
        // Empty vec = window clips to nothing (explicitly shaped to empty region).
        map.insert(host_xid, rects.to_vec());
        Ok(())
    }

    fn warp_pointer(
        &mut self,
        _origin: Option<OriginContext>,
        dst_host_xid: u32,
        dst_x: i16,
        dst_y: i16,
    ) -> io::Result<()> {
        let (base_x, base_y) = if dst_host_xid == 0 {
            (self.cursor_x, self.cursor_y)
        } else if let Some(w) = self.windows.get(&dst_host_xid) {
            (w.x as f32, w.y as f32)
        } else {
            return Ok(());
        };

        self.cursor_x = (base_x + dst_x as f32).clamp(0.0, self.fb_w as f32 - 1.0);
        self.cursor_y = (base_y + dst_y as f32).clamp(0.0, self.fb_h as f32 - 1.0);
        self.dispatch_motion_event();
        Ok(())
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        Ok(PointerPosition {
            same_screen: true,
            win_x: self.cursor_x as i16,
            win_y: self.cursor_y as i16,
            mask: self.serialize_modifiers(),
        })
    }

    fn list_fonts_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        _pattern: &str,
    ) -> io::Result<Vec<u8>> {
        // Return a curated set of XLFD names for fonts our loader can open.
        // Any XLFD that reaches open_font is handled via the fallback font
        // loader, so the exact XLFD field values only need to be plausible.
        let all_names: &[&str] = &[
            "-misc-fixed-medium-r-normal--14-130-75-75-c-70-iso10646-1",
            "-misc-fixed-bold-r-normal--14-130-75-75-c-70-iso10646-1",
            "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso10646-1",
            "-misc-fixed-medium-r-normal--10-100-75-75-c-60-iso10646-1",
            "-misc-fixed-medium-r-normal--8-80-75-75-c-50-iso10646-1",
            "-bitstream-bitstream vera sans mono-medium-r-normal--12-120-75-75-m-70-iso10646-1",
            "-bitstream-bitstream vera sans-medium-r-normal--12-120-75-75-p-67-iso10646-1",
            "-adobe-helvetica-medium-r-normal--12-120-75-75-p-67-iso8859-1",
            "-adobe-courier-medium-r-normal--12-120-75-75-m-70-iso8859-1",
            "fixed",
        ];
        let count = all_names.len().min(max_names as usize);
        let names = &all_names[..count];

        // Build the ListFonts wire reply.
        // Layout: 32-byte header + string items, each: 1-byte length + name bytes.
        let mut name_data: Vec<u8> = Vec::new();
        for &name in names {
            name_data.push(name.len() as u8);
            name_data.extend_from_slice(name.as_bytes());
        }
        let pad = (4 - (name_data.len() % 4)) % 4;
        name_data.resize(name_data.len() + pad, 0);

        let extra_words = (name_data.len() / 4) as u32;
        let mut reply = vec![0u8; 32 + name_data.len()];
        reply[0] = 1;
        // bytes [2..4] sequence: rewritten by caller
        reply[4..8].copy_from_slice(&extra_words.to_le_bytes());
        reply[8..10].copy_from_slice(&(count as u16).to_le_bytes());
        reply[32..].copy_from_slice(&name_data);
        Ok(reply)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _max_names: u16,
        _pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>> {
        // ListFontsWithInfo sends one reply per font and a final
        // terminator reply with `name-length == 0` to signal end of list.
        // Send only the terminator so clients unblock.  Reply size is 60
        // bytes (32-byte header + 28 bytes of font-info fields, all zero).
        let mut term = vec![0u8; 60];
        term[0] = 1; // reply type
        Ok(vec![term])
    }

    fn get_atom_name(
        &mut self,
        _origin: Option<OriginContext>,
        _atom: u32,
    ) -> io::Result<Option<String>> {
        Ok(None)
    }

    fn get_keyboard_mapping(
        &mut self,
        _origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        // X11 GetKeyboardMapping: per keycode, return a flat row of
        // keysyms across shift levels in the order
        // unshifted/shifted/mode-switch-unshifted/mode-switch-shifted.
        // Apps combine the keycode they received with the modifier
        // bits in the event's `state` field to pick the right slot.
        // Returning only level 0 means apps can never produce
        // shifted characters — typing Shift+a yields 'a' instead of
        // 'A' because that's the only keysym we ever expose.
        const LEVELS: usize = 4;
        let max_kc = u16::from(first_keycode) + u16::from(count);
        let mut flat = Vec::with_capacity(usize::from(count) * LEVELS);
        for kc in u16::from(first_keycode)..max_kc {
            let xkb_kc = xkbcommon::xkb::Keycode::new(u32::from(kc));
            for level in 0..LEVELS as u32 {
                let syms = self.xkb_keymap.0.key_get_syms_by_level(xkb_kc, 0, level);
                flat.push(syms.first().map_or(0, |s| s.raw()));
            }
        }
        Ok((LEVELS as u8, flat))
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        // Conventional defaults: 8 rows, up to 4 keycodes each
        // Shift(0x32,0x3E), Lock(0x42), Control(0x25,0x69),
        // Mod1(0x40,0x6C), Mod2(0x4D), Mod3(0x73), Mod4(0x85,0x86), Mod5(empty)
        // Encoded as count + flat vec of 8*4 = 32 bytes
        let data: Vec<u8> = vec![
            0x32, 0x3E, 0, 0, // Shift
            0x42, 0, 0, 0, // Lock
            0x25, 0x69, 0, 0, // Control
            0x40, 0x6C, 0, 0, // Mod1
            0x4D, 0, 0, 0, // Mod2
            0x73, 0, 0, 0, // Mod3
            0x85, 0x86, 0, 0, // Mod4
            0, 0, 0, 0, // Mod5
        ];
        Ok((4, data))
    }
}

/// Parse an AddGlyphs `body_tail` and insert glyphs into `gs`.
/// `body_tail` is everything after the 4-byte glyphset XID.
pub(super) fn parse_add_glyphs(gs: &mut GlyphSetState, body_tail: &[u8]) {
    if !matches!(gs.format, GlyphSetFormat::A8 | GlyphSetFormat::A1) {
        return;
    }
    if body_tail.len() < 4 {
        return;
    }
    let n = u32::from_le_bytes([body_tail[0], body_tail[1], body_tail[2], body_tail[3]]) as usize;
    let ids_end = 4 + n * 4;
    let infos_end = ids_end + n * 12;
    if body_tail.len() < infos_end {
        return;
    }

    let id_chunks = body_tail[4..ids_end].chunks_exact(4);
    let info_chunks = body_tail[ids_end..infos_end].chunks_exact(12);
    let mut data_off = infos_end;

    for (id_b, info_b) in id_chunks.zip(info_chunks) {
        let id = u32::from_le_bytes([id_b[0], id_b[1], id_b[2], id_b[3]]);
        let width = u16::from_le_bytes([info_b[0], info_b[1]]);
        let height = u16::from_le_bytes([info_b[2], info_b[3]]);
        let x = i16::from_le_bytes([info_b[4], info_b[5]]);
        let y = i16::from_le_bytes([info_b[6], info_b[7]]);
        let x_off = i16::from_le_bytes([info_b[8], info_b[9]]);
        let y_off = i16::from_le_bytes([info_b[10], info_b[11]]);

        let w = width as usize;
        let h = height as usize;
        let stride = match gs.format {
            GlyphSetFormat::A8 => (w + 3) & !3,
            GlyphSetFormat::A1 => w.div_ceil(32) * 4,
            GlyphSetFormat::Other => return,
        };
        let nbytes = stride * h;
        if data_off + nbytes > body_tail.len() {
            break;
        }
        let wire = &body_tail[data_off..data_off + nbytes];
        let pixels = match gs.format {
            GlyphSetFormat::A8 => {
                let mut pixels = vec![0u8; w * h];
                for row in 0..h {
                    pixels[row * w..row * w + w]
                        .copy_from_slice(&wire[row * stride..row * stride + w]);
                }
                pixels
            }
            GlyphSetFormat::A1 => wire.to_vec(),
            GlyphSetFormat::Other => return,
        };
        data_off += nbytes;
        gs.glyphs.insert(
            id,
            StoredGlyph {
                width,
                height,
                x,
                y,
                x_off,
                y_off,
                pixels,
                format: gs.format,
            },
        );
    }
}

/// Composite a CompositeGlyphs item stream from `gs` using `src` as the colour
/// source onto `dst`. `minor` = 23/24/25 → id_size 1/2/4.
/// `pen_x`/`pen_y` are the starting pen position (already offset by src_x+x_off
/// and src_y+y_off at the call site in `render_composite_glyphs`).
pub(super) fn composite_glyphs_onto(
    glyphsets: &HashMap<u32, GlyphSetState>,
    gs_xid: u32,
    src: &PixmanImage,
    dst: &mut PixmanImage,
    minor: u8,
    pen_x: i32,
    pen_y: i32,
    items: &[u8],
) {
    let id_size = match minor {
        23 => 1usize,
        24 => 2,
        _ => 4,
    };

    // Read the solid colour from `src` (1×1 REPEAT_NORMAL image).
    // SAFETY: src is a 1×1 pixman image we own; data() returns a valid pointer.
    let argb: u32 = unsafe { *src.0.data() };
    let a = (((argb >> 24) & 0xFF) as u16) * 0x101;
    let r = (((argb >> 16) & 0xFF) as u16) * 0x101;
    let g = (((argb >> 8) & 0xFF) as u16) * 0x101;
    let b = ((argb & 0xFF) as u16) * 0x101;
    let pen_color = Color::new(r, g, b, a);

    let dst_w = dst.0.width() as i32;
    let dst_h = dst.0.height() as i32;
    let mut pen_x = pen_x;
    let mut pen_y = pen_y;
    let mut pos = 0usize;
    let mut active_gs_xid = gs_xid;

    // Build once: the 1×1 tiling source image is the same colour for every glyph.
    let Ok(mut color_img) = Image::new(FormatCode::A8R8G8B8, 1, 1, true) else {
        return;
    };
    let _ = color_img.fill_rectangles(
        Operation::Src,
        pen_color,
        &[Rectangle16 {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }],
    );
    color_img.set_repeat(Repeat::Normal);

    while pos + 8 <= items.len() {
        let count = items[pos] as usize;
        if count == 255 {
            // Glyphset-switch sentinel: 8 bytes (count, pad×3, new_gs_xid×4).
            if pos + 8 <= items.len() {
                let new_xid = u32::from_le_bytes([
                    items[pos + 4],
                    items[pos + 5],
                    items[pos + 6],
                    items[pos + 7],
                ]);
                if new_xid != 0 && glyphsets.contains_key(&new_xid) {
                    active_gs_xid = new_xid;
                }
            }
            pos += 8;
            continue;
        }
        let dx = i16::from_le_bytes([items[pos + 4], items[pos + 5]]) as i32;
        let dy = i16::from_le_bytes([items[pos + 6], items[pos + 7]]) as i32;
        pen_x += dx;
        pen_y += dy;

        let payload_start = pos + 8;
        let payload_bytes = count * id_size;
        let padded = (payload_bytes + 3) & !3;
        if payload_start + padded > items.len() {
            break;
        }

        let Some(active_gs) = glyphsets.get(&active_gs_xid) else {
            pos += 8 + padded;
            continue;
        };

        for i in 0..count {
            let id_off = payload_start + i * id_size;
            let glyph_id: u32 = match id_size {
                1 => items[id_off] as u32,
                2 => u16::from_le_bytes([items[id_off], items[id_off + 1]]) as u32,
                _ => u32::from_le_bytes([
                    items[id_off],
                    items[id_off + 1],
                    items[id_off + 2],
                    items[id_off + 3],
                ]),
            };

            let Some(glyph) = active_gs.glyphs.get(&glyph_id) else {
                continue;
            };
            let gw = glyph.width as usize;
            let gh = glyph.height as usize;
            // pen_x - glyph.x: wire x is -bitmap_left (see X RENDER spec §4.6).
            let dst_x = pen_x - glyph.x as i32;
            let dst_y = pen_y - glyph.y as i32;

            if dst_x + gw as i32 <= 0 || dst_y + gh as i32 <= 0 || dst_x >= dst_w || dst_y >= dst_h
            {
                pen_x += glyph.x_off as i32;
                pen_y += i32::from(glyph.y_off);
                continue;
            }

            let glyph_img = match glyph.format {
                GlyphSetFormat::A8 => {
                    let Ok(img) = Image::new(FormatCode::A8, gw, gh, true) else {
                        pen_x += glyph.x_off as i32;
                        pen_y += i32::from(glyph.y_off);
                        continue;
                    };
                    let stride_bytes = img.stride();
                    // SAFETY: img is freshly allocated; we write within its bounds.
                    let gdata = unsafe { img.data() }.cast::<u8>();
                    for row in 0..gh {
                        for col in 0..gw {
                            unsafe {
                                *gdata.add(row * stride_bytes + col) = glyph.pixels[row * gw + col];
                            }
                        }
                    }
                    img
                }
                GlyphSetFormat::A1 => {
                    let Ok(img) = Image::new(FormatCode::A1, gw, gh, true) else {
                        pen_x += i32::from(glyph.x_off);
                        pen_y += i32::from(glyph.y_off);
                        continue;
                    };
                    // Wire A1 rows are 32-bit padded MSB-first — same as pixman A1.
                    let wire_stride = gw.div_ceil(32) * 4;
                    let img_stride = img.stride();
                    // SAFETY: img is freshly allocated; pixel data fits in its bounds.
                    let gdata = unsafe { img.data() }.cast::<u8>();
                    for row in 0..gh {
                        let src_off = row * wire_stride;
                        let dst_off = row * img_stride;
                        let copy_len = wire_stride
                            .min(img_stride)
                            .min(glyph.pixels.len().saturating_sub(src_off));
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                glyph.pixels.as_ptr().add(src_off),
                                gdata.add(dst_off),
                                copy_len,
                            );
                        }
                    }
                    img
                }
                GlyphSetFormat::Other => {
                    pen_x += i32::from(glyph.x_off);
                    pen_y += i32::from(glyph.y_off);
                    continue;
                }
            };

            dst.0.composite32(
                Operation::Over,
                &color_img,
                Some(&glyph_img),
                (0, 0),
                (0, 0),
                (dst_x, dst_y),
                (gw as i32, gh as i32),
            );

            pen_x += glyph.x_off as i32;
            pen_y += i32::from(glyph.y_off);
        }

        pos += 8 + padded;
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, sync::Arc};

    use pixman::{Color, FormatCode, Image, Operation, Rectangle16, Repeat};
    use yserver_core::{
        backend::{Backend, GcFunction},
        host_x11::HostXidMap,
    };
    use yserver_protocol::x11::ResourceId;

    use super::{KmsBackend, PixmanImage, WindowState, fill_rects_with_gc_function};

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    fn make_test_backend() -> KmsBackend {
        let ctx = super::XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &ctx.0,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx.0,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .expect("test xkb keymap");
        let xkb_state = super::XkbState(xkbcommon::xkb::State::new(&keymap));

        KmsBackend {
            device: Arc::new(crate::drm::Device::for_tests().expect("test drm device")),
            outputs: vec![super::OutputLayout {
                output: crate::drm::modeset::Output {
                    connector: drm::control::from_u32(1).unwrap(),
                    connector_name: "test".to_string(),
                    crtc: drm::control::from_u32(1).unwrap(),
                    plane: drm::control::from_u32(1).unwrap(),
                    // SAFETY: tests never pass this mode to DRM; it is only
                    // present to satisfy KmsBackend's production fields.
                    mode: unsafe { std::mem::zeroed() },
                    picked: crate::drm::modeset::Mode {
                        name: "test".to_string(),
                        width: 800,
                        height: 600,
                        vrefresh: 60,
                        preferred: true,
                    },
                    plane_fb_id_prop: drm::control::from_u32(1).unwrap(),
                    plane_crtc_id_prop: drm::control::from_u32(1).unwrap(),
                },
                swapchain: crate::drm::Swapchain::empty_for_tests(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }],
            fb_w: 800,
            fb_h: 600,
            windows: HashMap::new(),
            next_host_xid: 0x0040_0000,
            top_level_order: Vec::new(),
            window_id: 1,
            root_visual_xid: 0x21,
            xid_map: HostXidMap::new(),
            xkb_context: ctx,
            xkb_keymap: super::XkbKeymap(keymap),
            xkb_state,
            input_ctx: None,
            font_loader: super::FontLoader::new().expect("test font loader"),
            fonts: HashMap::new(),
            pixmaps: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            bg_pixmap_image: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            cursors: HashMap::new(),
            active_cursor: None,
            button_mask: 0,
            prev_pointer_window: None,
            pending_pointer_events: Vec::new(),
            current_font: None,
            current_function: GcFunction::Copy,
            current_foreground: 0,
            current_background: 0x00ff_ffff,
            pictures: HashMap::new(),
            picture_rescued_images: HashMap::new(),
            glyphsets: HashMap::new(),
            shape_bounding: HashMap::new(),
            shape_clip: HashMap::new(),
            shape_input: HashMap::new(),
        }
    }

    fn make_test_window(x: i16, y: i16, width: u16, height: u16, mapped: bool) -> WindowState {
        WindowState {
            _nested_id: ResourceId(0x0000_0100),
            x,
            y,
            width,
            height,
            border_width: 0,
            mapped,
            _override_redirect: false,
            _parent: Some(1),
            children: Vec::new(),
            bg_pixel: None,
            bg_pixmap: None,
            image: RefCell::new(
                PixmanImage::new(FormatCode::X8R8G8B8, width, height, true).unwrap(),
            ),
            depth: 24,
            visual: 0,
            cursor: 0,
        }
    }

    /// Fill a PixmanImage with a solid 24-bit colour (X8R8G8B8 format).
    fn fill_image(img: &mut PixmanImage, pixel: u32) {
        let color = super::color_from_u32(pixel);
        let w = img.0.width() as u16;
        let h = img.0.height() as u16;
        let _ = img.0.fill_rectangles(
            Operation::Src,
            color,
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: w,
                height: h,
            }],
        );
    }

    /// Read the packed X8R8G8B8 pixel at (x, y) from a PixmanImage.
    fn read_pixel(img: &PixmanImage, x: usize, y: usize) -> u32 {
        let stride_words = img.0.stride() / 4;
        // SAFETY: x, y are within the image bounds (caller's responsibility).
        unsafe { *img.0.data().add(y * stride_words + x) }
    }

    fn has_nonzero_pixel(img: &PixmanImage) -> bool {
        (0..img.0.height())
            .any(|y| (0..img.0.width()).any(|x| read_pixel(img, x, y) & 0x00ff_ffff != 0))
    }

    #[test]
    fn copy_plane_depth1_substitutes_foreground_background() {
        let mut b = make_test_backend();

        let src_img = PixmanImage::new(FormatCode::A1, 2, 2, true).unwrap();
        let src_stride = src_img.0.stride();
        // SAFETY: src_img is freshly allocated. A1 pixels are MSB-first in
        // each row byte; set row0=[1,0], row1=[0,1].
        let src_data = unsafe { src_img.0.data() }.cast::<u8>();
        unsafe {
            *src_data.add(0) = 0b1000_0000;
            *src_data.add(src_stride) = 0b0100_0000;
        }
        let src_xid = 0x0040_1000;
        b.pixmaps.insert(
            src_xid,
            super::PixmapState {
                handle: src_xid,
                image: src_img,
                depth: 1,
            },
        );

        let dst_xid = 0x0040_1001;
        b.pixmaps.insert(
            dst_xid,
            super::PixmapState {
                handle: dst_xid,
                image: PixmanImage::new(FormatCode::A8R8G8B8, 2, 2, true).unwrap(),
                depth: 32,
            },
        );
        b.current_function = GcFunction::Copy;
        b.current_foreground = 0x00ff_0000;
        b.current_background = 0x0000_00ff;

        b.copy_plane(None, src_xid, dst_xid, 0, 0, 0, 0, 2, 2, 1)
            .unwrap();

        let dst = &b.pixmaps.get(&dst_xid).unwrap().image;
        assert_eq!(read_pixel(dst, 0, 0) & 0x00ff_ffff, 0x00ff_0000);
        assert_eq!(read_pixel(dst, 1, 0) & 0x00ff_ffff, 0x0000_00ff);
        assert_eq!(read_pixel(dst, 0, 1) & 0x00ff_ffff, 0x0000_00ff);
        assert_eq!(read_pixel(dst, 1, 1) & 0x00ff_ffff, 0x00ff_0000);
    }

    #[test]
    fn poly_text16_renders_char2b_text() {
        let mut b = make_test_backend();
        let (font, _) = b.open_font(None, "fixed").unwrap();
        b.current_font = Some(font.as_raw());
        let dst_xid = 0x0040_2000;
        b.pixmaps.insert(
            dst_xid,
            super::PixmapState {
                handle: dst_xid,
                image: PixmanImage::new(FormatCode::A8R8G8B8, 64, 24, true).unwrap(),
                depth: 32,
            },
        );

        let mut body = vec![0u8; 12];
        body[8..10].copy_from_slice(&2i16.to_le_bytes());
        body[10..12].copy_from_slice(&16i16.to_le_bytes());
        body.extend_from_slice(&[1, 0, 0, 0x41]);

        b.poly_text16(None, dst_xid, 0x00ff_ffff, &body).unwrap();

        assert!(has_nonzero_pixel(&b.pixmaps.get(&dst_xid).unwrap().image));
    }

    #[test]
    fn image_text16_draws_background_and_char2b_text() {
        let mut b = make_test_backend();
        let (font, _) = b.open_font(None, "fixed").unwrap();
        b.current_font = Some(font.as_raw());
        let dst_xid = 0x0040_2001;
        b.pixmaps.insert(
            dst_xid,
            super::PixmapState {
                handle: dst_xid,
                image: PixmanImage::new(FormatCode::A8R8G8B8, 64, 24, true).unwrap(),
                depth: 32,
            },
        );

        let mut body = vec![0u8; 12];
        body[8..10].copy_from_slice(&0i16.to_le_bytes());
        body[10..12].copy_from_slice(&16i16.to_le_bytes());
        body.extend_from_slice(&[0, 0x41]);

        b.image_text16(None, dst_xid, 0x00ff_ffff, 0x0000_00ff, 1, &body)
            .unwrap();

        assert!(has_nonzero_pixel(&b.pixmaps.get(&dst_xid).unwrap().image));
    }

    #[test]
    fn change_picture_cprepeat_updates_drawable_repeat() {
        let mut b = make_test_backend();
        let pixmap_xid = 0x0040_3000;
        let pic_xid = 0x0040_3001;
        b.pixmaps.insert(
            pixmap_xid,
            super::PixmapState {
                handle: pixmap_xid,
                image: PixmanImage::new(FormatCode::A8R8G8B8, 4, 4, true).unwrap(),
                depth: 32,
            },
        );
        b.pictures
            .insert(pic_xid, super::default_drawable_picture(pixmap_xid));

        let mut body = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0x0001u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        b.render_change_picture(None, pic_xid, &body).unwrap();

        match b.pictures.get(&pic_xid).unwrap() {
            super::PictureState::Drawable { repeat, .. } => {
                assert!(matches!(repeat, Repeat::Normal));
            }
            _ => panic!("expected drawable picture"),
        }
    }

    #[test]
    fn change_picture_cpclipmask_zero_clears_clip() {
        let mut b = make_test_backend();
        let pixmap_xid = 0x0040_3010;
        let pic_xid = 0x0040_3011;
        b.pixmaps.insert(
            pixmap_xid,
            super::PixmapState {
                handle: pixmap_xid,
                image: PixmanImage::new(FormatCode::A8R8G8B8, 4, 4, true).unwrap(),
                depth: 32,
            },
        );
        b.pictures
            .insert(pic_xid, super::default_drawable_picture(pixmap_xid));
        if let Some(super::PictureState::Drawable { clip, .. }) = b.pictures.get_mut(&pic_xid) {
            *clip = Some(vec![Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }]);
        }

        let mut body = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0x0040u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        b.render_change_picture(None, pic_xid, &body).unwrap();

        match b.pictures.get(&pic_xid).unwrap() {
            super::PictureState::Drawable { clip, .. } => assert!(clip.is_none()),
            _ => panic!("expected drawable picture"),
        }
    }

    #[test]
    fn linear_gradient_composite_produces_nonzero_pixels() {
        let mut b = make_test_backend();
        let mut body = Vec::new();
        body.extend_from_slice(&0x0010_0000u32.to_le_bytes());
        body.extend_from_slice(&0i32.to_le_bytes());
        body.extend_from_slice(&0i32.to_le_bytes());
        body.extend_from_slice(&(64i32 << 16).to_le_bytes());
        body.extend_from_slice(&0i32.to_le_bytes());
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&0i32.to_le_bytes());
        body.extend_from_slice(&(1i32 << 16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0xffffu16.to_le_bytes());
        body.extend_from_slice(&0xffffu16.to_le_bytes());
        body.extend_from_slice(&0xffffu16.to_le_bytes());
        body.extend_from_slice(&0xffffu16.to_le_bytes());
        body.extend_from_slice(&0xffffu16.to_le_bytes());

        let grad = b
            .render_create_linear_gradient(None, &body)
            .unwrap()
            .expect("gradient picture");
        let dst_xid = 0x0040_4000;
        let dst_pic = 0x0040_4001;
        b.pixmaps.insert(
            dst_xid,
            super::PixmapState {
                handle: dst_xid,
                image: PixmanImage::new(FormatCode::A8R8G8B8, 64, 1, true).unwrap(),
                depth: 32,
            },
        );
        b.pictures
            .insert(dst_pic, super::default_drawable_picture(dst_xid));

        b.render_composite(
            None,
            Operation::Src as u8,
            grad.as_raw(),
            0,
            dst_pic,
            0,
            0,
            0,
            0,
            0,
            0,
            64,
            1,
        )
        .unwrap();

        assert!(has_nonzero_pixel(&b.pixmaps.get(&dst_xid).unwrap().image));
    }

    #[test]
    fn set_picture_transform_stores_non_identity_matrix() {
        let mut b = make_test_backend();
        let pixmap_xid = 0x0040_5000;
        let pic_xid = 0x0040_5001;
        b.pixmaps.insert(
            pixmap_xid,
            super::PixmapState {
                handle: pixmap_xid,
                image: PixmanImage::new(FormatCode::A8R8G8B8, 4, 4, true).unwrap(),
                depth: 32,
            },
        );
        b.pictures
            .insert(pic_xid, super::default_drawable_picture(pixmap_xid));

        let mut body = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        for value in [0x20000i32, 0, 0, 0, 0x10000, 0, 0, 0, 0x10000] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        b.render_set_picture_transform(None, pic_xid, &body)
            .unwrap();

        match b.pictures.get(&pic_xid).unwrap() {
            super::PictureState::Drawable { transform, .. } => assert!(transform.is_some()),
            _ => panic!("expected drawable picture"),
        }
    }

    #[test]
    fn warp_pointer_updates_cursor_position() {
        let mut b = make_test_backend();
        let xid = b.next_host_xid;
        b.next_host_xid += 1;
        b.windows
            .insert(xid, make_test_window(100, 200, 300, 200, true));
        b.top_level_order.push(xid);

        b.warp_pointer(None, xid, 10, 20).unwrap();

        assert_eq!(b.cursor_x as i32, 110);
        assert_eq!(b.cursor_y as i32, 220);
    }

    // ---------------------------------------------------------------------------
    // Step 4 — multi-monitor: per-output bbox pre-filter
    // ---------------------------------------------------------------------------

    #[test]
    fn window_intersects_bbox_filters_off_screen_top_levels() {
        let mut b = make_test_backend();
        // Top-level placed off the default test layout (0,0,800,600).
        let xid = b.next_host_xid;
        b.next_host_xid += 1;
        b.windows
            .insert(xid, make_test_window(2000, 100, 100, 100, true));
        b.top_level_order.push(xid);

        // Whole virtual screen including (0..1024, 0..768) — does not reach x=2000.
        assert!(!b.window_intersects(
            xid,
            super::Rect {
                x: 0,
                y: 0,
                w: 1024,
                h: 768,
            }
        ));
        // Output positioned at virtual-screen x=1900 with width 1024 — overlaps window.
        assert!(b.window_intersects(
            xid,
            super::Rect {
                x: 1900,
                y: 0,
                w: 1024,
                h: 768,
            }
        ));
        // Top-left corner of virtual screen — far away from the window.
        assert!(!b.window_intersects(
            xid,
            super::Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            }
        ));
    }

    #[test]
    fn paint_output_offsets_window_into_scanout() {
        // Simulate a second-of-two-output layout positioned at virtual x=1024,
        // and assert paint_output translates a top-level window at virtual
        // (2000, 100) so it lands at (2000-1024, 100) = (976, 100) on the
        // scanout. Skips paint when no window intersects the bbox.
        let mut b = make_test_backend();
        b.outputs[0].x = 1024;
        b.outputs[0].y = 0;
        b.outputs[0].width = 1024;
        b.outputs[0].height = 768;
        b.fb_w = 1024 + 1024;
        b.fb_h = 768;

        // A 100x100 window at virtual (2000, 100) filled red.
        let xid = b.next_host_xid;
        b.next_host_xid += 1;
        let mut win = make_test_window(2000, 100, 100, 100, true);
        fill_image(win.image.get_mut(), 0x00ff_0000);
        b.windows.insert(xid, win);
        b.top_level_order.push(xid);

        // Scanout sized to layout dimensions.
        let mut scanout = PixmanImage::new(FormatCode::X8R8G8B8, 1024, 768, true).unwrap();
        b.paint_output(&mut scanout, 0, &[xid]);

        // Window pixel at virtual (2000, 100) should land at scanout (976, 100).
        assert_eq!(read_pixel(&scanout, 976, 100) & 0x00ff_ffff, 0x00ff_0000);
        // The pixel at scanout (0, 0) is bg only — definitely not the window red.
        assert_ne!(read_pixel(&scanout, 0, 0) & 0x00ff_ffff, 0x00ff_0000);

        // Empty visible list (window pre-filtered out) must not panic and must
        // not stamp the window onto the scanout.
        let mut scanout2 = PixmanImage::new(FormatCode::X8R8G8B8, 1024, 768, true).unwrap();
        b.paint_output(&mut scanout2, 0, &[]);
        assert_ne!(read_pixel(&scanout2, 976, 100) & 0x00ff_ffff, 0x00ff_0000);
    }

    // ---------------------------------------------------------------------------
    // GcFunction::Copy: fill_rects_with_gc_function must overwrite the destination
    // ---------------------------------------------------------------------------

    #[test]
    fn fill_rects_copy_overwrites_destination() {
        let mut img = PixmanImage::new(FormatCode::X8R8G8B8, 4, 4, true).unwrap();
        fill_image(&mut img, 0x00ff_ffff); // white
        let rect = Rectangle16 {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        fill_rects_with_gc_function(&mut img, GcFunction::Copy, 0x00ff_00ff, &[rect]);
        let pixel = read_pixel(&img, 1, 1);
        assert_eq!(
            pixel & 0x00ff_ffff,
            0x00ff_00ff,
            "Copy should overwrite with magenta"
        );
    }

    // ---------------------------------------------------------------------------
    // GcFunction::Xor: must produce bitwise XOR of destination and foreground.
    //
    // NOTE: KmsBackend requires DRM hardware and cannot be constructed in a unit
    // test.  This test verifies XOR semantics at the PixmanImage level by calling
    // fill_rects_with_gc_function() directly — the same helper invoked by every
    // client-draw primitive (poly_segment, poly_line, fill_rectangle, …).
    //
    // NOTE: pixman's Porter-Duff PIXMAN_OP_XOR produces zero for fully-opaque
    // images (src*(1-dst.a) + dst*(1-src.a) = 0 when both alphas are 1).
    // fill_rects_with_gc_function implements GcFunction::Xor as a manual bitwise
    // XOR over the RGB channels to match X11 GXxor semantics.
    // ---------------------------------------------------------------------------

    #[test]
    fn poly_segment_xor_inverts_destination_pixels() {
        // Create a 16×16 image pre-filled with white (0x00FFFFFF).
        let mut img = PixmanImage::new(FormatCode::X8R8G8B8, 16, 16, true).unwrap();
        fill_image(&mut img, 0x00ff_ffff); // white

        // Draw a horizontal line at y=8 with magenta (0x00FF00FF) using XOR.
        let row: Vec<Rectangle16> = (0..16_i16)
            .map(|x| Rectangle16 {
                x,
                y: 8,
                width: 1,
                height: 1,
            })
            .collect();
        fill_rects_with_gc_function(&mut img, GcFunction::Xor, 0x00ff_00ff, &row);

        // White (0xFFFFFF) XOR magenta (0xFF00FF) = green (0x00FF00).
        let pixel = read_pixel(&img, 8, 8);
        assert_eq!(
            pixel & 0x00ff_ffff,
            0x0000_ff00,
            "expected green (0x00FF00), got 0x{:08x}",
            pixel
        );

        // Pixels outside the drawn row must remain white.
        let untouched = read_pixel(&img, 8, 0);
        assert_eq!(
            untouched & 0x00ff_ffff,
            0x00ff_ffff,
            "pixel at (8,0) should be untouched white"
        );
    }

    // ---------------------------------------------------------------------------
    // Glyph rendering: verify freetype GRAY mode + pixman A8 stride handling.
    //
    // This test does NOT require DRM hardware.  It loads a font via freetype,
    // renders a single glyph, and composites it onto a white pixman image using
    // exactly the same path as render_text_string.
    // ---------------------------------------------------------------------------

    #[test]
    fn glyph_render_gray_pixels_land_on_correct_rows() {
        // ------------------------------------------------------------------
        // 1. Load font and render glyph 'A'.
        // ------------------------------------------------------------------
        let lib = freetype::Library::init().expect("freetype init");
        let candidates = [
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        ];
        let face = candidates
            .iter()
            .find_map(|p| lib.new_face(p, 0).ok())
            .expect("DejaVuSansMono.ttf not found — install dejavu fonts");
        let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
        let _ = face.load_char('A' as usize, freetype::face::LoadFlag::RENDER);
        let glyph = face.glyph();
        let bitmap = glyph.bitmap();

        // Must be GRAY (8bpp) — not MONO.  RENDER flag on an outline font
        // always produces GRAY; MONO would indicate an embedded bitmap strike.
        let pm = bitmap.pixel_mode().expect("pixel_mode");
        assert_eq!(
            pm,
            freetype::bitmap::PixelMode::Gray,
            "expected GRAY pixel mode, got {:?}",
            pm
        );

        let w = bitmap.width() as usize;
        let h = bitmap.rows() as usize;
        let pitch = bitmap.pitch();
        let buf = bitmap.buffer();

        assert!(w > 0 && h > 0, "glyph 'A' should have non-empty bitmap");
        // For GRAY the pitch in bytes >= width in pixels.
        assert!(pitch >= 0, "expected positive (downward) pitch");
        assert!(pitch as usize >= w, "pitch should be >= width for GRAY");

        // ------------------------------------------------------------------
        // 2. Copy glyph pixels into a flat Vec (same logic as render_text_string).
        // ------------------------------------------------------------------
        let mut pixels = vec![0u8; w * h];
        for row in 0..h {
            let src = if pitch >= 0 {
                row * pitch as usize
            } else {
                (h - 1 - row) * (pitch as isize).unsigned_abs()
            };
            pixels[row * w..row * w + w].copy_from_slice(&buf[src..src + w]);
        }

        // At least some pixels must be non-zero (the glyph is not blank).
        let has_nonzero = pixels.iter().any(|&b| b > 0);
        assert!(
            has_nonzero,
            "glyph pixels should contain non-zero alpha values"
        );

        // ------------------------------------------------------------------
        // 3. Write into a pixman A8 image using stride (same as phase 2).
        // ------------------------------------------------------------------
        let glyph_img = Image::new(FormatCode::A8, w, h, true).expect("pixman A8 image");
        let stride_bytes = glyph_img.stride();
        // stride_bytes must be >= w (pixman pads A8 rows to 4-byte alignment).
        assert!(stride_bytes >= w, "pixman A8 stride must be >= width");

        let gdata = unsafe { glyph_img.data() } as *mut u8;
        for row in 0..h {
            for col in 0..w {
                unsafe {
                    *gdata.add(row * stride_bytes + col) = pixels[row * w + col];
                }
            }
        }

        // Verify that the A8 image contains non-zero bytes in its first row.
        let first_row_nonzero = (0..w).any(|col| unsafe { *gdata.add(col) > 0 });
        assert!(
            first_row_nonzero,
            "A8 image first row should have non-zero alpha"
        );

        // ------------------------------------------------------------------
        // 4. Composite onto a white X8R8G8B8 image and verify pixels changed.
        //
        // We use bitmap_top to position the glyph correctly: the baseline is
        // at y = bitmap_top (so the glyph top is at row 0, baseline at
        // bitmap_top). With a foreground of black (0x000000) on white
        // (0xFFFFFF), composited pixels should be darker than 0xFFFFFF.
        // ------------------------------------------------------------------
        let baseline_y = glyph.bitmap_top(); // rows from top to baseline
        let img_h = (baseline_y + 4).max(h as i32 + 4) as u16;
        let img_w = (w + 4) as u16;
        let mut dst =
            PixmanImage::new(FormatCode::X8R8G8B8, img_w, img_h, true).expect("dst image");
        fill_image(&mut dst, 0x00ff_ffff); // white

        let mut color_img = Image::new(FormatCode::A8R8G8B8, 1, 1, true).expect("color image");
        let black = Color::new(0, 0, 0, 0xffff);
        let _ = color_img.fill_rectangles(
            Operation::Src,
            black,
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        );
        // Must tile across the glyph — same fix as render_text_string.
        color_img.set_repeat(Repeat::Normal);

        // dst_y = baseline_y - bitmap_top = 0 (glyph top lands on row 0).
        let dst_y = baseline_y - glyph.bitmap_top();
        dst.0.composite32(
            Operation::Over,
            &color_img,
            Some(&glyph_img),
            (0, 0),
            (0, 0),
            (0, dst_y),
            (w as i32, h as i32),
        );

        // The destination should no longer be all-white: the composited 'A'
        // glyph (black foreground) should have darkened some pixels.
        let any_changed = (0..img_w as usize).any(|x| {
            (0..img_h as usize).any(|y| read_pixel(&dst, x, y) & 0x00ff_ffff != 0x00ff_ffff)
        });
        assert!(
            any_changed,
            "composite should darken some white pixels with black 'A'"
        );
    }

    // ---------------------------------------------------------------------------
    // RENDER picture + trapezoid tests.
    //
    // KmsBackend requires DRM hardware so we cannot instantiate it here.
    // Instead we exercise the same Pixman logic that render_trapezoids uses,
    // calling pixman_composite_trapezoids directly with a solid-fill 1×1 source
    // image and an A8R8G8B8 destination.
    // ---------------------------------------------------------------------------

    /// Encode one X RENDER Trapezoid (40 bytes, little-endian 16.16 fixed).
    #[allow(clippy::too_many_arguments)]
    fn encode_trap(
        top: i32,
        bottom: i32,
        lp1x: i32,
        lp1y: i32,
        lp2x: i32,
        lp2y: i32,
        rp1x: i32,
        rp1y: i32,
        rp2x: i32,
        rp2y: i32,
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(40);
        for v in [top, bottom, lp1x, lp1y, lp2x, lp2y, rp1x, rp1y, rp2x, rp2y] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf
    }

    /// Decode the wire bytes for one trap into a pixman_trapezoid_t.
    fn decode_trap(bytes: &[u8]) -> pixman::ffi::pixman_trapezoid_t {
        assert_eq!(bytes.len(), 40);
        let i32_at = |off: usize| {
            i32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        };
        pixman::ffi::pixman_trapezoid_t {
            top: i32_at(0),
            bottom: i32_at(4),
            left: pixman::ffi::pixman_line_fixed_t {
                p1: pixman::ffi::pixman_point_fixed_t {
                    x: i32_at(8),
                    y: i32_at(12),
                },
                p2: pixman::ffi::pixman_point_fixed_t {
                    x: i32_at(16),
                    y: i32_at(20),
                },
            },
            right: pixman::ffi::pixman_line_fixed_t {
                p1: pixman::ffi::pixman_point_fixed_t {
                    x: i32_at(24),
                    y: i32_at(28),
                },
                p2: pixman::ffi::pixman_point_fixed_t {
                    x: i32_at(32),
                    y: i32_at(36),
                },
            },
        }
    }

    #[test]
    fn render_trapezoids_over_produces_nonzero_alpha_in_dst() {
        // Destination: 8×8 A8R8G8B8, cleared to transparent black.
        let dst_img = PixmanImage::new(FormatCode::A8R8G8B8, 8, 8, true).unwrap();

        // Source: 1×1 solid red (fully opaque), with REPEAT_NORMAL so it tiles.
        let mut src_img = Image::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
        let red = Color::new(0xFFFF, 0x0000, 0x0000, 0xFFFF);
        let _ = src_img.fill_rectangles(
            Operation::Src,
            red,
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        );
        src_img.set_repeat(Repeat::Normal);

        // A rectangle trap covering pixels (1,1)–(6,6).
        // In 16.16 fixed: pixel N → N << 16.
        let left_x = 1i32 << 16;
        let right_x = 6i32 << 16;
        let top_y = 1i32 << 16;
        let bot_y = 6i32 << 16;
        let wire = encode_trap(
            top_y, bot_y, left_x, top_y, left_x, bot_y, // left edge: vertical at x=1
            right_x, top_y, right_x, bot_y, // right edge: vertical at x=6
        );
        let trap_struct = decode_trap(&wire);

        // SAFETY: both images are valid, non-overlapping pixman images owned by
        // this stack frame.  trap_struct is POD constructed above.
        unsafe {
            pixman::ffi::pixman_composite_trapezoids(
                pixman::ffi::pixman_op_t_PIXMAN_OP_OVER,
                src_img.as_ptr(),
                dst_img.0.as_ptr(),
                pixman::ffi::pixman_format_code_t_PIXMAN_a8,
                0,
                0, // src_x, src_y
                0,
                0, // dst_x, dst_y
                1,
                &trap_struct,
            );
        }

        // Center pixel (3,3) must have nonzero alpha after the composite.
        let stride_words = dst_img.0.stride() / 4;
        let pixel = unsafe { *dst_img.0.data().add(3 * stride_words + 3) };
        let alpha = (pixel >> 24) & 0xFF;
        assert!(
            alpha > 0,
            "center pixel at (3,3) should have nonzero alpha after trap composite; got 0x{:08x}",
            pixel
        );

        // And pixels outside the trap (e.g. (0,0)) must remain transparent.
        let corner = unsafe { *dst_img.0.data().add(0) };
        assert_eq!(
            (corner >> 24) & 0xFF,
            0,
            "pixel at (0,0) outside trap should remain transparent; got 0x{:08x}",
            corner
        );
    }

    #[test]
    fn render_trapezoids_center_pixel_carries_source_color() {
        // Destination: 8×8 A8R8G8B8, cleared to transparent black.
        let dst_img = PixmanImage::new(FormatCode::A8R8G8B8, 8, 8, true).unwrap();

        // Source: solid green (0x00FF00), fully opaque.
        let mut src_img = Image::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
        let green = Color::new(0x0000, 0xFFFF, 0x0000, 0xFFFF);
        let _ = src_img.fill_rectangles(
            Operation::Src,
            green,
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        );
        src_img.set_repeat(Repeat::Normal);

        // Rectangular trap covering the full image interior (1,1)–(6,6).
        let l = 1i32 << 16;
        let r = 6i32 << 16;
        let t = 1i32 << 16;
        let b = 6i32 << 16;
        let trap_struct = decode_trap(&encode_trap(t, b, l, t, l, b, r, t, r, b));

        unsafe {
            pixman::ffi::pixman_composite_trapezoids(
                pixman::ffi::pixman_op_t_PIXMAN_OP_OVER,
                src_img.as_ptr(),
                dst_img.0.as_ptr(),
                pixman::ffi::pixman_format_code_t_PIXMAN_a8,
                0,
                0,
                0,
                0,
                1,
                &trap_struct,
            );
        }

        // Center pixel (3,3): alpha must be 0xFF and RGB must be pure green.
        let stride_words = dst_img.0.stride() / 4;
        let pixel = unsafe { *dst_img.0.data().add(3 * stride_words + 3) };
        let a = (pixel >> 24) & 0xFF;
        let r_ch = (pixel >> 16) & 0xFF;
        let g_ch = (pixel >> 8) & 0xFF;
        let b_ch = pixel & 0xFF;
        assert_eq!(a, 0xFF, "center alpha should be fully opaque");
        assert_eq!(r_ch, 0x00, "center red channel should be 0");
        assert_eq!(g_ch, 0xFF, "center green channel should be 0xFF");
        assert_eq!(b_ch, 0x00, "center blue channel should be 0");
    }

    #[test]
    fn add_glyphs_stores_pixel_data_correctly() {
        // Build a minimal AddGlyphs body_tail for 2 glyphs:
        //   Glyph 0 (id=1): 2×2 A8 bitmap, x=-1, y=-2, x_off=4, y_off=0
        //   Glyph 1 (id=2): 4×1 A8 bitmap, x=0,  y=-1, x_off=5, y_off=0
        //
        // Wire layout:
        //   num_glyphs(4) = 2
        //   ids: [1u32 LE, 2u32 LE]
        //   infos:
        //     glyph0: width=2 height=2 x=-1 y=-2 x_off=4 y_off=0  (12 bytes)
        //     glyph1: width=4 height=1 x=0  y=-1 x_off=5 y_off=0  (12 bytes)
        //   pixel data:
        //     glyph0: 2×2 A8, row-stride=4 (padded): [0x11,0x22, 0,0, 0x33,0x44, 0,0]
        //     glyph1: 4×1 A8, row-stride=4 (no pad):  [0x55,0x66,0x77,0x88]

        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes()); // num_glyphs
        body.extend_from_slice(&1u32.to_le_bytes()); // id[0]
        body.extend_from_slice(&2u32.to_le_bytes()); // id[1]
        // glyph0 info
        body.extend_from_slice(&2u16.to_le_bytes()); // width
        body.extend_from_slice(&2u16.to_le_bytes()); // height
        body.extend_from_slice(&(-1i16).to_le_bytes()); // x
        body.extend_from_slice(&(-2i16).to_le_bytes()); // y
        body.extend_from_slice(&4i16.to_le_bytes()); // x_off
        body.extend_from_slice(&0i16.to_le_bytes()); // y_off
        // glyph1 info
        body.extend_from_slice(&4u16.to_le_bytes()); // width
        body.extend_from_slice(&1u16.to_le_bytes()); // height
        body.extend_from_slice(&0i16.to_le_bytes()); // x
        body.extend_from_slice(&(-1i16).to_le_bytes()); // y
        body.extend_from_slice(&5i16.to_le_bytes()); // x_off
        body.extend_from_slice(&0i16.to_le_bytes()); // y_off
        // glyph0 pixels: 2×2, padded row stride 4
        body.extend_from_slice(&[0x11, 0x22, 0x00, 0x00]); // row 0
        body.extend_from_slice(&[0x33, 0x44, 0x00, 0x00]); // row 1
        // glyph1 pixels: 4×1, padded row stride 4
        body.extend_from_slice(&[0x55, 0x66, 0x77, 0x88]);

        let mut gs = super::GlyphSetState {
            format: super::GlyphSetFormat::A8,
            glyphs: std::collections::HashMap::new(),
        };
        super::parse_add_glyphs(&mut gs, &body);

        let g0 = gs.glyphs.get(&1).expect("glyph id=1 missing");
        assert_eq!(g0.width, 2);
        assert_eq!(g0.height, 2);
        assert_eq!(g0.x, -1);
        assert_eq!(g0.y, -2);
        assert_eq!(g0.x_off, 4);
        assert_eq!(g0.pixels, vec![0x11, 0x22, 0x33, 0x44]); // densely packed

        let g1 = gs.glyphs.get(&2).expect("glyph id=2 missing");
        assert_eq!(g1.width, 4);
        assert_eq!(g1.pixels, vec![0x55, 0x66, 0x77, 0x88]);
    }

    #[test]
    fn composite_glyphs_single_run_places_glyph_on_dst() {
        // Set up: 1×1 opaque-red solid colour source, 8×8 white destination.
        // Glyph: 2×2 fully-opaque A8 (all 0xFF), stored at id=1 in a GlyphSetState.
        // CompositeGlyphs8 item stream: one run of 1 glyph at dx=2, dy=3.
        // Expected: after composite, pixel at (2 - glyph.x, 3 - glyph.y) is red.

        // Build glyphset with a 2×2 all-opaque A8 glyph (id=1, x=-1, y=-1).
        let mut gs = super::GlyphSetState {
            format: super::GlyphSetFormat::A8,
            glyphs: std::collections::HashMap::new(),
        };
        gs.glyphs.insert(
            1,
            super::StoredGlyph {
                width: 2,
                height: 2,
                x: -1,
                y: -1,
                x_off: 3,
                y_off: 0,
                pixels: vec![0xFF; 4],
                format: super::GlyphSetFormat::A8,
            },
        );

        // Red solid-fill source (A8R8G8B8 = 0xFFFF0000).
        let mut src_img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
        let _ = src_img.0.fill_rectangles(
            Operation::Src,
            Color::new(0xFFFF, 0, 0, 0xFFFF),
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        );
        src_img.0.set_repeat(Repeat::Normal);

        // White destination.
        let mut dst_img = PixmanImage::new(FormatCode::X8R8G8B8, 8, 8, true).unwrap();
        fill_image(&mut dst_img, 0x00FF_FFFF);

        // Build a CompositeGlyphs8 item stream: one run with count=1, dx=2, dy=3, id=1.
        let mut items = Vec::new();
        items.push(1u8); // count
        items.extend_from_slice(&[0, 0, 0]); // pad
        items.extend_from_slice(&2i16.to_le_bytes()); // dx
        items.extend_from_slice(&3i16.to_le_bytes()); // dy
        items.push(1u8); // glyph id (8-bit for minor=23)
        items.extend_from_slice(&[0, 0, 0]); // pad to 4-byte boundary

        let gs_xid = 1u32;
        let mut glyphsets = std::collections::HashMap::new();
        glyphsets.insert(gs_xid, gs);
        super::composite_glyphs_onto(
            &glyphsets,
            gs_xid,
            &src_img,
            &mut dst_img,
            /*minor=*/ 23,
            /*pen_x=*/ 0,
            /*pen_y=*/ 0,
            &items,
        );

        // Pen after dx/dy = (0+2, 0+3) = (2, 3).
        // Draw at (pen_x - glyph.x, pen_y - glyph.y) = (2-(-1), 3-(-1)) = (3, 4).
        let p = read_pixel(&dst_img, 3, 4);
        assert_ne!(
            p & 0x00FF_0000,
            0,
            "pixel (3,4) should have red channel; got 0x{:08x}",
            p
        );
        assert_eq!(
            p & 0x0000_FFFF,
            0,
            "pixel (3,4) should have no blue/green; got 0x{:08x}",
            p
        );
    }

    #[test]
    fn composite_glyphs_multi_run_advances_pen() {
        // Two runs: first places glyph id=1 (x_off=5), second places glyph id=2.
        // After first run pen advances by x_off=5.
        let mut gs = super::GlyphSetState {
            format: super::GlyphSetFormat::A8,
            glyphs: std::collections::HashMap::new(),
        };
        gs.glyphs.insert(
            1,
            super::StoredGlyph {
                width: 1,
                height: 1,
                x: 0,
                y: 0,
                x_off: 5,
                y_off: 0,
                pixels: vec![0xFF],
                format: super::GlyphSetFormat::A8,
            },
        );
        gs.glyphs.insert(
            2,
            super::StoredGlyph {
                width: 1,
                height: 1,
                x: 0,
                y: 0,
                x_off: 3,
                y_off: 0,
                pixels: vec![0xFF],
                format: super::GlyphSetFormat::A8,
            },
        );

        let mut src_img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
        let _ = src_img.0.fill_rectangles(
            Operation::Src,
            Color::new(0, 0, 0xFFFF, 0xFFFF),
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        );
        src_img.0.set_repeat(Repeat::Normal);

        let mut dst_img = PixmanImage::new(FormatCode::X8R8G8B8, 20, 4, true).unwrap();
        fill_image(&mut dst_img, 0x00FF_FFFF);

        // Run 1: count=1, dx=2, dy=1, id=1
        // Run 2: count=1, dx=3, dy=0, id=2
        let mut items = Vec::new();
        items.push(1u8);
        items.extend_from_slice(&[0, 0, 0]);
        items.extend_from_slice(&2i16.to_le_bytes());
        items.extend_from_slice(&1i16.to_le_bytes());
        items.push(1u8);
        items.extend_from_slice(&[0, 0, 0]);
        items.push(1u8);
        items.extend_from_slice(&[0, 0, 0]);
        items.extend_from_slice(&3i16.to_le_bytes());
        items.extend_from_slice(&0i16.to_le_bytes());
        items.push(2u8);
        items.extend_from_slice(&[0, 0, 0]);

        let gs_xid = 1u32;
        let mut glyphsets = std::collections::HashMap::new();
        glyphsets.insert(gs_xid, gs);
        super::composite_glyphs_onto(&glyphsets, gs_xid, &src_img, &mut dst_img, 23, 0, 0, &items);

        // Glyph 1 at pen (2,1): draw at (2,1). After glyph, pen_x += x_off=5 → pen_x=7.
        // Glyph 2 at pen (7+3=10, 1+0=1): draw at (10,1).
        let p1 = read_pixel(&dst_img, 2, 1);
        let p2 = read_pixel(&dst_img, 10, 1);
        assert_ne!(
            p1 & 0x0000_FFFF,
            0,
            "glyph1 pixel (2,1) should have blue; got 0x{:08x}",
            p1
        );
        assert_ne!(
            p2 & 0x0000_FFFF,
            0,
            "glyph2 pixel (10,1) should have blue; got 0x{:08x}",
            p2
        );
    }

    #[test]
    fn render_composite_solid_fill_onto_drawable() {
        // Simulate: composite a 1×1 red SolidFill picture onto a 4×4 white drawable.
        // The SolidFill image is a 1×1 REPEAT_NORMAL red pixel.
        // We call pixman_image_composite32 directly (same path as the impl will use).

        let mut src_img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
        let _ = src_img.0.fill_rectangles(
            Operation::Src,
            Color::new(0xFFFF, 0, 0, 0xFFFF), // opaque red
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        );
        src_img.0.set_repeat(Repeat::Normal);

        let mut dst_img = PixmanImage::new(FormatCode::X8R8G8B8, 4, 4, true).unwrap();
        fill_image(&mut dst_img, 0x00FF_FFFF); // white

        let src_ptr = src_img.0.as_ptr();
        let dst_ptr = dst_img.0.as_ptr();

        unsafe {
            pixman::ffi::pixman_image_composite32(
                pixman::ffi::pixman_op_t_PIXMAN_OP_OVER,
                src_ptr,
                std::ptr::null_mut(),
                dst_ptr,
                0,
                0,
                0,
                0,
                1,
                1, // dst at (1,1)
                2,
                2, // 2×2 region
            );
        }

        let p = read_pixel(&dst_img, 1, 1);
        assert_ne!(
            p & 0x00FF_0000,
            0,
            "pixel (1,1) should have red; got 0x{:08x}",
            p
        );
        assert_eq!(
            p & 0x0000_FFFF,
            0,
            "pixel (1,1) should have no blue/green; got 0x{:08x}",
            p
        );
    }

    #[test]
    fn composite_glyphs_sentinel_does_not_panic() {
        // A sentinel-only item stream (count=255 + 4-byte gs XID) should be a no-op.
        let gs = super::GlyphSetState {
            format: super::GlyphSetFormat::A8,
            glyphs: std::collections::HashMap::new(),
        };
        let src_img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
        let mut dst_img = PixmanImage::new(FormatCode::X8R8G8B8, 4, 4, true).unwrap();
        fill_image(&mut dst_img, 0x00FF_FFFF);
        let gs_xid = 1u32;
        let mut glyphsets = std::collections::HashMap::new();
        glyphsets.insert(gs_xid, gs);
        let items = vec![255u8, 0, 0, 0, 0x99, 0, 0, 0]; // sentinel + fake gs xid
        // Should not panic:
        super::composite_glyphs_onto(&glyphsets, gs_xid, &src_img, &mut dst_img, 23, 0, 0, &items);
        // Mask off the X/A channel — X8R8G8B8 may store 0xFF in the top byte.
        assert_eq!(
            read_pixel(&dst_img, 0, 0) & 0x00FF_FFFF,
            0x00FF_FFFF,
            "sentinel must not modify dst"
        );
    }

    #[test]
    fn render_create_cursor_stores_image_and_hotspot() {
        use super::*;

        // KmsBackend::new requires live DRM hardware and cannot be constructed in unit tests.
        // This test exercises the same data-flow logic (picture→pixmap→cursor image copy)
        // that render_create_cursor uses, with an explicit pixel-content assertion.

        let mut pixmap_img = PixmanImage::new(FormatCode::A8R8G8B8, 4, 4, true).unwrap();
        let red = Color::new(0xFFFF, 0xFFFF, 0x0000, 0x0000);
        let full = Rectangle16 {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        pixmap_img
            .0
            .fill_rectangles(Operation::Src, red, &[full])
            .unwrap();

        let pixmap_xid: u32 = 10;
        let picture_xid: u32 = 20;
        let cursor_xid: u32 = 30;

        let mut cursors: HashMap<u32, CursorState> = HashMap::new();
        let mut pictures: HashMap<u32, PictureState> = HashMap::new();
        let mut pixmaps: HashMap<u32, PixmapState> = HashMap::new();

        pixmaps.insert(
            pixmap_xid,
            PixmapState {
                handle: pixmap_xid,
                image: pixmap_img,
                depth: 32,
            },
        );
        pictures.insert(picture_xid, default_drawable_picture(pixmap_xid));

        let hot_x: u16 = 1;
        let hot_y: u16 = 2;

        let (w, h) = {
            let pm = pixmaps.get(&pixmap_xid).unwrap();
            (pm.image.0.width() as u16, pm.image.0.height() as u16)
        };
        let mut cursor_img = PixmanImage::new(FormatCode::A8R8G8B8, w, h, true).unwrap();
        {
            let pm = pixmaps.get(&pixmap_xid).unwrap();
            cursor_img.0.composite32(
                Operation::Src,
                &pm.image.0,
                None,
                (0, 0),
                (0, 0),
                (0, 0),
                (w as i32, h as i32),
            );
        }
        cursors.insert(
            cursor_xid,
            CursorState {
                image: cursor_img,
                hot_x,
                hot_y,
            },
        );

        let cs = cursors.get(&cursor_xid).unwrap();
        assert_eq!(cs.hot_x, hot_x);
        assert_eq!(cs.hot_y, hot_y);
        assert_eq!(cs.image.0.width() as u16, 4);
        assert_eq!(cs.image.0.height() as u16, 4);

        // Verify the composite actually copied the red fill into the cursor image.
        // A8R8G8B8 in memory: alpha=0xFF, R=0xFF, G=0x00, B=0x00 → 0xFFFF0000.
        // SAFETY: cursor_img was just created and no other reference to it exists.
        let pixel = unsafe { *cs.image.0.data().add(0) };
        assert_eq!(
            pixel & 0x00FF_0000,
            0x00FF_0000,
            "red channel should be set"
        );
    }

    #[test]
    fn draw_cursor_onto_composites_at_hotspot_adjusted_position() {
        use super::*;

        // 2×2 all-red ARGB cursor image.
        let mut cursor_img = PixmanImage::new(FormatCode::A8R8G8B8, 2, 2, true).unwrap();
        let red = Color::new(0xFFFF, 0xFFFF, 0x0000, 0x0000);
        let full = Rectangle16 {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        cursor_img
            .0
            .fill_rectangles(Operation::Src, red, &[full])
            .unwrap();

        // 10×10 black destination.
        let mut dst = PixmanImage::new(FormatCode::A8R8G8B8, 10, 10, true).unwrap();

        // Cursor position (5,5) with hotspot (1,1) → image top-left lands at (4,4).
        let x = 5_i32 - 1;
        let y = 5_i32 - 1;
        dst.0.composite32(
            Operation::Over,
            &cursor_img.0,
            None,
            (0, 0),
            (0, 0),
            (x, y),
            (2, 2),
        );

        let pixel = read_pixel(&dst, x as usize, y as usize);
        assert_eq!(
            pixel & 0x00FF_0000,
            0x00FF_0000,
            "red channel at (4,4) should be 0xFF after composite"
        );
    }

    #[test]
    fn parse_xlfd_extracts_family_style_pixelsize() {
        // XLFD weight strings are lowercase; we pass them straight to fontconfig
        // which matches case-insensitively.
        let (fam, style, px) = super::FontLoader::parse_xlfd(
            "-adobe-helvetica-bold-i-normal--12-120-75-75-p-67-iso8859-1",
        );
        assert_eq!(fam.as_deref(), Some("helvetica"));
        assert_eq!(style.as_deref(), Some("bold Italic"));
        assert_eq!(px, Some(12));
    }

    #[test]
    fn parse_xlfd_treats_wildcards_as_unspecified() {
        // Wildcards in family/weight/slant ⇒ None; pixelsize "*" ⇒ no size.
        let (fam, style, px) = super::FontLoader::parse_xlfd("-*-*-*-*-*-*-*-*-*-*-*-*-*-*");
        assert!(fam.is_none());
        assert!(style.is_none());
        assert!(px.is_none());
    }

    #[test]
    fn parse_xlfd_roman_slant_no_italic() {
        // Slant "r" (roman) shouldn't pull in "Italic"; weight "medium" carries through.
        let (_, style, _) = super::FontLoader::parse_xlfd(
            "-adobe-courier-medium-r-normal--14-140-75-75-m-90-iso8859-1",
        );
        assert_eq!(style.as_deref(), Some("medium"));
    }

    #[test]
    fn open_font_accepts_x11_alias_via_fontconfig() {
        // "fixed" is a classic X11 alias. fontconfig knows it, or falls back
        // to monospace — either way we must get a usable face.
        let loader = super::FontLoader::new().expect("fontconfig+freetype init");
        let (_face, metrics, _cache) = loader.open_font("fixed").expect("resolve fixed");
        assert!(metrics.font_ascent + metrics.font_descent > 0);
    }
}
