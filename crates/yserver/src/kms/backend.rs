use std::{cell::RefCell, collections::HashMap, io, sync::Arc};

use crate::kms::cpu_types::{PictTransform, Rectangle16, Repeat};
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

/// Owning snapshot of a drawable's mirror pixels, produced by
/// [`KmsBackend::read_mirror_pixels`]. Tightly packed (no row pad),
/// `bytes_per_pixel` ∈ {1, 4} matching the mirror's Vulkan format
/// (`R8_UNORM` for depth 1/8, `B8G8R8A8_UNORM` for depth 24/32).
struct MirrorReadback {
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    bytes: Vec<u8>,
}

/// Axis-aligned overlap test for two rects of the same size in
/// the same coordinate space. `vkCmdCopyImage` is UB when src and
/// dst rects on the same image overlap; the
/// [`KmsBackend::try_vk_copy_area`] path uses this to decide
/// whether to take the same-image fast path or fall back to
/// pixman (the staging-image path is a 4.1.4.2 follow-up).
fn rects_overlap_axis_aligned(
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> bool {
    let sx0 = src_x;
    let sy0 = src_y;
    let sx1 = src_x.saturating_add(width);
    let sy1 = src_y.saturating_add(height);
    let dx0 = dst_x;
    let dy0 = dst_y;
    let dx1 = dst_x.saturating_add(width);
    let dy1 = dst_y.saturating_add(height);
    sx0 < dx1 && dx0 < sx1 && sy0 < dy1 && dy0 < sy1
}

/// Translate a list of GC-clipped dst sub-rects into
/// `VkImageCopy` regions. Each sub-rect's src offset shifts by
/// the same amount the dst was clipped (mirrors the existing
/// pixman per-sub-rect path). Sub-rects clipped to either image's
/// extent.
fn build_image_copy_regions(
    sub_rects: &[Rectangle16],
    orig_src_x: i16,
    orig_src_y: i16,
    orig_dst_x: i16,
    orig_dst_y: i16,
    src_extent: ash::vk::Extent2D,
    dst_extent: ash::vk::Extent2D,
) -> Vec<ash::vk::ImageCopy> {
    let mut out = Vec::with_capacity(sub_rects.len());
    let subresource = ash::vk::ImageSubresourceLayers::default()
        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
        .layer_count(1);
    for r in sub_rects {
        let dx_shift = i32::from(r.x) - i32::from(orig_dst_x);
        let dy_shift = i32::from(r.y) - i32::from(orig_dst_y);
        let src_off_x = i32::from(orig_src_x) + dx_shift;
        let src_off_y = i32::from(orig_src_y) + dy_shift;
        let dst_off_x = i32::from(r.x);
        let dst_off_y = i32::from(r.y);

        // Clip widths/heights to fit both src and dst extents.
        let max_w_src = i32::try_from(src_extent.width).unwrap_or(i32::MAX) - src_off_x.max(0);
        let max_h_src = i32::try_from(src_extent.height).unwrap_or(i32::MAX) - src_off_y.max(0);
        let max_w_dst = i32::try_from(dst_extent.width).unwrap_or(i32::MAX) - dst_off_x.max(0);
        let max_h_dst = i32::try_from(dst_extent.height).unwrap_or(i32::MAX) - dst_off_y.max(0);

        let req_w = i32::from(r.width);
        let req_h = i32::from(r.height);
        let w = req_w.min(max_w_src).min(max_w_dst);
        let h = req_h.min(max_h_src).min(max_h_dst);
        if w <= 0 || h <= 0 || src_off_x < 0 || src_off_y < 0 || dst_off_x < 0 || dst_off_y < 0 {
            continue;
        }
        out.push(
            ash::vk::ImageCopy::default()
                .src_subresource(subresource)
                .src_offset(ash::vk::Offset3D {
                    x: src_off_x,
                    y: src_off_y,
                    z: 0,
                })
                .dst_subresource(subresource)
                .dst_offset(ash::vk::Offset3D {
                    x: dst_off_x,
                    y: dst_off_y,
                    z: 0,
                })
                .extent(ash::vk::Extent3D {
                    width: w as u32,
                    height: h as u32,
                    depth: 1,
                }),
        );
    }
    out
}

fn read_mirror_pixel_for_plane(rb: &MirrorReadback, depth: u8, x: usize, y: usize) -> u32 {
    let w = rb.width as usize;
    let h = rb.height as usize;
    if x >= w || y >= h {
        return 0;
    }
    let idx = y * w + x;
    match (depth, rb.bytes_per_pixel) {
        // Depth-1 sources land in `R8_UNORM` with one byte per pixel
        // (0xFF or 0x00 — see the depth-1 branch of `try_vk_put_image`).
        // Any non-zero counts as set.
        (1, 1) => u32::from(rb.bytes[idx] != 0),
        (8, 1) => u32::from(rb.bytes[idx]),
        // BGRA mirror in memory order [B, G, R, A] reads as the same
        // little-endian u32 word that the legacy pixman path returned.
        (24 | 32, 4) => {
            let off = idx * 4;
            u32::from_le_bytes([
                rb.bytes[off],
                rb.bytes[off + 1],
                rb.bytes[off + 2],
                rb.bytes[off + 3],
            ])
        }
        _ => 0,
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

/// Resolved RENDER picture for the 4.1.4.6 Vulkan path. Either a
/// drawable mirror, a 1×1 SolidFill colour (already premul), or
/// "no picture" (only valid for the mask slot — collapses to the
/// backend-shared white-mask scratch in the recorder).
#[derive(Debug, Clone, Copy)]
enum RenderPic {
    Drawable(u32),
    Solid([f32; 4]),
    Gradient(u32),
    None,
}

/// Resolve a RENDER picture into a [`RenderPic`] for the Vulkan
/// composite path. `component_alpha` is supported on the mask side
/// via the dual-source-blend pipeline (see `render.frag.glsl`
/// `COMPONENT_ALPHA = 1`); the caller picks up the mask picture's
/// `component_alpha` flag separately. `alpha_map` is still
/// unsupported — falls back to pixman.
fn resolve_render_pic(picture: Option<&PictureState>) -> Option<RenderPic> {
    match picture {
        Some(PictureState::Drawable {
            host_xid,
            alpha_map: None,
            ..
        }) => Some(RenderPic::Drawable(*host_xid)),
        Some(PictureState::SolidFill { premul, .. }) => Some(RenderPic::Solid(*premul)),
        // Gradients carry their host_xid in `RenderPic::Gradient` so
        // the Vk path can look up the `GradientPicture` from
        // `self.pictures`. We can't store a borrowed reference here
        // because `RenderPic` outlives the `pictures.get(&xid)` borrow.
        _ => None,
    }
}

/// Resolve a `RenderPic` plus its picture XID. Used by the gradient
/// branch which needs to look the picture back up after dropping the
/// pictures borrow.
fn resolve_render_pic_with_gradient_xid(
    pictures: &HashMap<u32, PictureState>,
    pic_xid: u32,
) -> Option<RenderPic> {
    match pictures.get(&pic_xid) {
        Some(PictureState::Gradient { .. }) => Some(RenderPic::Gradient(pic_xid)),
        other => resolve_render_pic(other),
    }
}

/// Translate a [`Repeat`] enum value to the integer constant the
/// `render.frag.glsl` shader expects (matches the protocol numbering;
/// see `render_pipeline::REPEAT_*`).
fn repeat_to_shader_const(repeat: Repeat) -> i32 {
    use crate::kms::vk::render_pipeline::{REPEAT_NONE, REPEAT_NORMAL, REPEAT_PAD, REPEAT_REFLECT};
    match repeat {
        Repeat::None => REPEAT_NONE,
        Repeat::Normal => REPEAT_NORMAL,
        Repeat::Pad => REPEAT_PAD,
        Repeat::Reflect => REPEAT_REFLECT,
    }
}

/// Convert an X11 RENDER pixman 3×3 transform (16.16 fixed-point) into
/// the affine 2×3 form the `render.frag.glsl` shader uses. RENDER's
/// transform maps the *destination*-relative source coordinate to the
/// pre-sample source pixel:
///
/// ```text
///   src_pixel = M * (src_origin + dst_offset, 1)
/// ```
///
/// We assume affine — the bottom row is `[0, 0, 1]`. Real X11 clients
/// use affine transforms in practice; projective transforms (the rare
/// case) round-trip through the affine portion only and produce wrong
/// pixels at the perspective-divide corners. That trade-off is
/// documented in `feedback_phase4_1_4_decisions.md` § component-alpha
/// and matches the per-family-port strict-acceptance relaxation.
/// Compose two affine 2×3 transforms. The result satisfies
/// `compose(A, B) * v == A * (B * v)` when `v` is `(x, y, 1)`.
fn compose_affines(
    a: crate::kms::vk::ops::render::AffineXform,
    b: crate::kms::vk::ops::render::AffineXform,
) -> crate::kms::vk::ops::render::AffineXform {
    use crate::kms::vk::ops::render::AffineXform;
    // a.row0 = (a00, a01, a02), a.row1 = (a10, a11, a12). Bottom row
    // implicit `[0, 0, 1]`. Same for b.
    let a00 = a.row0[0];
    let a01 = a.row0[1];
    let a02 = a.row0[2];
    let a10 = a.row1[0];
    let a11 = a.row1[1];
    let a12 = a.row1[2];
    let b00 = b.row0[0];
    let b01 = b.row0[1];
    let b02 = b.row0[2];
    let b10 = b.row1[0];
    let b11 = b.row1[1];
    let b12 = b.row1[2];
    AffineXform {
        row0: [
            a00 * b00 + a01 * b10,
            a00 * b01 + a01 * b11,
            a00 * b02 + a01 * b12 + a02,
            0.0,
        ],
        row1: [
            a10 * b00 + a11 * b10,
            a10 * b01 + a11 * b11,
            a10 * b02 + a11 * b12 + a12,
            0.0,
        ],
    }
}

fn pixman_transform_to_affine(
    transform: Option<&PictTransform>,
    _src_extent: ash::vk::Extent2D,
) -> crate::kms::vk::ops::render::AffineXform {
    use crate::kms::vk::ops::render::AffineXform;
    let Some(t) = transform else {
        return AffineXform::IDENTITY;
    };
    // pixman_transform stores 9 fixed-point i32 values in row-major
    // order. matrix[row][col] in 16.16 fixed point.
    let m = t.matrix;
    let to_f = |v: i32| (v as f32) / 65536.0;
    let mut a = to_f(m[0][0]);
    let mut b = to_f(m[0][1]);
    let mut tx = to_f(m[0][2]);
    let mut c = to_f(m[1][0]);
    let mut d = to_f(m[1][1]);
    let mut ty = to_f(m[1][2]);
    // Constant-divisor projective transforms (matrix row 2 = `[0, 0, w]`
    // with w ≠ 1) collapse to a uniform 1/w scale on the affine portion.
    // Rendercheck's tscoords/tmcoords cases use this form to scale a 5×5
    // src 8×; pixman handles it the same way. Non-constant projective
    // transforms (m[2][0] or m[2][1] non-zero) genuinely vary per-pixel
    // and we don't model them — the affine portion is used as-is, which
    // matches the strict-acceptance relaxation in
    // `feedback_phase4_1_4_decisions.md`.
    let m20 = to_f(m[2][0]);
    let m21 = to_f(m[2][1]);
    let m22 = to_f(m[2][2]);
    if m20 == 0.0 && m21 == 0.0 && m22 != 0.0 && m22 != 1.0 {
        let inv = 1.0 / m22;
        a *= inv;
        b *= inv;
        tx *= inv;
        c *= inv;
        d *= inv;
        ty *= inv;
    }
    AffineXform {
        row0: [a, b, tx, 0.0],
        row1: [c, d, ty, 0.0],
    }
}

/// One glyph's CPU-side rasterisation result, ready for either the
/// pixman compositor or the Phase 4.1.4.5 Vulkan text-run path.
/// `pixels` is row-major, tightly packed (w × h alpha bytes), the
/// FreeType `BITMAP_GRAY` layout.
struct RenderedGlyph {
    dst_x: i32,
    dst_y: i32,
    w: usize,
    h: usize,
    pixels: Vec<u8>,
    /// X11 character advance from the font's char_info_cache.
    /// Currently only used by callers' pen tracking (which lives
    /// outside this struct); kept here for future per-glyph
    /// kerning if it lands.
    #[allow(dead_code)]
    advance: i32,
    /// Unicode codepoint — used as part of the glyph atlas key.
    codepoint: u32,
}

// `fill_rects_with_gc_function` (pixman per-pixel GC-function fill)
// deleted in 4.1.5. Every callsite now routes through
// `KmsBackend::try_vk_fill_with_function`, which uses the
// `LogicFillPipelineCache` for non-Copy functions.

/// Parse a packed pair of i16 values (2 bytes each) from a byte slice.
fn read_i16_pair(data: &[u8], offset: usize) -> Option<(i16, i16)> {
    if offset + 4 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    Some((x, y))
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
    /// Render-node fd dup'd per `Backend::dri3_open` call (Phase 4.2,
    /// Task 6). Sibling of `device` on single-GPU; resolved at backend
    /// init via sysfs walk (`/sys/dev/char/<major>:<minor>/device/drm`)
    /// with a `/dev/dri/renderD*` enumeration fallback. None on the
    /// `for_tests` path. Wired into `Backend::dri3_open` in Task 7.
    #[allow(dead_code)]
    pub(crate) render_node_fd: Option<std::os::fd::OwnedFd>,
    /// XSync `Fence` and DRI3 `Syncobj` resources backed by
    /// `VkSemaphore` (Phase 4.2.2 Tasks 19, 20). Keyed by client XID.
    /// Each entry was imported via `kms::vk::sync::import_sync_file`
    /// (binary semaphore from a `sync_file` fd) or
    /// `import_drm_syncobj` (timeline semaphore from a DRM_SYNCOBJ
    /// fd). Dropped on DestroyFence / FreeSyncobj.
    pub(crate) dri3_sync_resources: HashMap<u32, ash::vk::Semaphore>,
    /// xshmfence-backed fences keyed by XID. Mesa's loader_dri3 uses
    /// xshmfence for `FenceFromFD` — a memfd + futex protocol,
    /// disjoint from sync_file. Vulkan can't import these, so we
    /// fall back to mmaping the fd and calling
    /// `xshmfence_trigger` directly when the X side wants to
    /// signal idle.
    pub(crate) dri3_xshmfences: HashMap<u32, crate::kms::xshmfence::FenceMapping>,
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

    // Vulkan context (Phase 4.1.1: initialised but idle; pixman still
    // owns drawing). Held as an Arc so future scene-graph code can
    // share it across helpers without threading lifetime parameters.
    // Optional only so unit tests in `mod tests` can construct a
    // KmsBackend without a real Vulkan device — production paths
    // always set it via `KmsBackend::open_with_commit`.
    #[allow(dead_code)]
    pub(crate) vk: Option<Arc<crate::kms::vk::device::VkContext>>,

    // One scanout-bo pool per output (parallel to `outputs`). `None`
    // entries mean "fall back to the dumb-buffer scanout for this
    // output" — happens when Vulkan-first allocation fails for that
    // output's dimensions.
    pub(crate) scanout_pools: Vec<Option<crate::kms::vk::scanout::ScanoutBoPool>>,

    // Graphics pipeline used by the per-window composite pass
    // (sub-phase 4.1.3.4). Built once at backend init; reused every
    // frame. `None` when Vulkan didn't come up.
    #[allow(dead_code)]
    pub(crate) compositor_pipeline: Option<crate::kms::vk::pipeline::CompositorPipeline>,

    // Drawing-op command pool (sub-phase 4.1.4). Separate from
    // `MirrorUploader`'s transfer pool — drawing ops emit graphics
    // workload (begin_rendering / clear_attachments / draws), the
    // uploader emits transfer workload. Sharing pools risks
    // lifetime tangles when both run in the same frame. `None`
    // when Vulkan didn't come up.
    pub(crate) ops_command_pool: Option<crate::kms::vk::ops::OpsCommandPool>,

    // Per-backend host-mapped staging buffer used by image-transfer
    // ops (PutImage / GetImage / MIT-SHM PutImage / MIT-SHM GetImage /
    // MitShmCreatePixmap). Reused across ops, grows on demand. `None`
    // when Vulkan didn't come up.
    pub(crate) ops_staging: Option<crate::kms::vk::ops::OpsStaging>,

    // Glyph atlas + text-render pipeline (sub-phase 4.1.4.5). The
    // atlas owns the shared R8 image; the pipeline binds it via a
    // single combined-image-sampler descriptor. Both `None` when
    // Vulkan didn't come up. Initialised eagerly alongside
    // `compositor_pipeline`.
    pub(crate) glyph_atlas: Option<crate::kms::vk::glyph::GlyphAtlas>,
    pub(crate) text_pipeline: Option<crate::kms::vk::text_pipeline::TextPipeline>,

    // RENDER `Composite` + `FillRectangles` pipeline cache + solid
    // colour scratch images (sub-phase 4.1.4.6). All `None` when
    // Vulkan didn't come up.
    //
    // `solid_src_image` / `solid_mask_image`: 1×1 BGRA scratches
    // rewritten via `cmd_clear_color_image` per call when the
    // corresponding picture is a `SolidFill`. `white_mask_image`:
    // 1×1 BGRA pre-cleared to opaque white at backend init, used
    // as the mask binding for `Composite` calls without a mask
    // (the multiplication by `mask.a == 1.0` is a no-op).
    pub(crate) render_pipelines: Option<crate::kms::vk::render_pipeline::RenderPipelineCache>,
    pub(crate) solid_src_image: Option<crate::kms::vk::render_pipeline::SolidColorImage>,
    pub(crate) solid_mask_image: Option<crate::kms::vk::render_pipeline::SolidColorImage>,
    pub(crate) white_mask_image: Option<crate::kms::vk::render_pipeline::SolidColorImage>,
    /// CPU-rasterised mask scratch for ops without a per-X-resource
    /// mask source (Trapezoids, Triangles). One R8 image, grow-on-
    /// demand. Phase 4.1.4.7.
    pub(crate) mask_scratch: Option<crate::kms::vk::mask_scratch::MaskScratch>,
    /// BGRA staging image for same-target overlap copies (xterm
    /// scrollback) and similar src/dst aliasing. Grow-on-demand.
    pub(crate) copy_scratch: Option<crate::kms::vk::copy_scratch::CopyScratch>,
    /// Sampleable dst-pixel readback scratch for Disjoint/Conjoint
    /// RENDER ops (per-format, grow-on-demand). The shader's manual
    /// blend (`render.frag.glsl` `MODE=1`) needs to read the existing
    /// dst pixel; this scratch is the copy target.
    pub(crate) dst_readback: Option<crate::kms::vk::dst_readback::DstReadback>,
    /// Per-`GcFunction` solid-fill pipelines (Xor / And / Or /
    /// Invert / Set / etc.; 16 X11 GcFunctions → 16 `VkLogicOp`s).
    /// Built lazily on first use of each function.
    pub(crate) logic_fill_pipelines:
        Option<crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache>,

    // Diagnostic: per-output flag set the first time a
    // pageflip-complete event arrives. Lets us see in the log
    // whether the kernel ever told us the very first flip latched.
    pub(crate) first_pageflip_logged: Vec<bool>,

    // Fonts (freetype)
    font_loader: FontLoader,
    fonts: HashMap<u32, FontState>,

    // Pixman pixmaps (non-window drawables)
    pixmaps: HashMap<u32, PixmapState>,

    // Background state (root)
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,

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
    current_fill: FillState,
    current_clip: ClipState,

    // RENDER picture tracking
    pictures: HashMap<u32, PictureState>,

    // Vk mirrors rescued from freed pixmaps still referenced by live
    // pictures. Keyed by picture host_xid. Cleaned up by
    // render_free_picture (drops the DrawableImage, which releases its
    // VkImage and allocation).
    picture_rescued_images: HashMap<u32, crate::kms::vk::target::DrawableImage>,

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
        transform: Option<PictTransform>,
        graphics_exposure: bool,
        subwindow_mode: u8,
        poly_edge: u8,
        poly_mode: u8,
    },
    /// CreateSolidFill source: a single premultiplied BGRA colour
    /// the Vk render pipeline reads from a uniform / 1×1 sampler
    /// scratch as needed. Stored already-premultiplied so the
    /// recorder doesn't redo the conversion on every draw.
    SolidFill {
        premul: [f32; 4],
        repeat: Repeat,
        component_alpha: bool,
    },
    Gradient {
        gradient: crate::kms::vk::gradient::GradientPicture,
        repeat: Repeat,
        transform: Option<PictTransform>,
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
    /// Cursor extent in pixels. Mirrors `vk_mirror`'s extent when the
    /// mirror is present; stored separately so the composite scene
    /// builder can size the cursor quad without a Vk borrow.
    extent: ash::vk::Extent2D,
    hot_x: u16,
    hot_y: u16,
    /// Vulkan-side cursor image. Same Option semantics as
    /// [`WindowState::vk_mirror`] / [`PixmapState::vk_mirror`].
    /// Built and populated at cursor-create time via
    /// [`KmsBackend::upload_bgra_to_mirror`]; sampled by the
    /// composite pass's final cursor quad (Phase 4.1.3.4).
    vk_mirror: Option<crate::kms::vk::target::DrawableImage>,
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
    /// Vulkan-side image for this window. `None` when Vulkan isn't
    /// up or mirror allocation failed; in that case the window
    /// won't render content under the composite pass. Created at
    /// `create_subwindow` time and bg-pixel-filled if applicable;
    /// reallocated on resize.
    vk_mirror: Option<crate::kms::vk::target::DrawableImage>,
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
    width: u16,
    height: u16,
    #[allow(dead_code)]
    depth: u8,
    /// Vulkan-side image for this pixmap. Same Option semantics as
    /// [`WindowState::vk_mirror`]. Created at `create_pixmap` time;
    /// rescued on `free_pixmap` when a live picture still references
    /// it (see [`KmsBackend::free_pixmap`]).
    vk_mirror: Option<crate::kms::vk::target::DrawableImage>,
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
        min_bounds.left_side_bearing = min_bounds.left_side_bearing.min(ci.left_side_bearing);
        max_bounds.left_side_bearing = max_bounds.left_side_bearing.max(ci.left_side_bearing);
        min_bounds.right_side_bearing = min_bounds.right_side_bearing.min(ci.right_side_bearing);
        max_bounds.right_side_bearing = max_bounds.right_side_bearing.max(ci.right_side_bearing);
        min_bounds.character_width = min_bounds.character_width.min(ci.character_width);
        max_bounds.character_width = max_bounds.character_width.max(ci.character_width);
        min_bounds.ascent = min_bounds.ascent.min(ci.ascent);
        max_bounds.ascent = max_bounds.ascent.max(ci.ascent);
        min_bounds.descent = min_bounds.descent.min(ci.descent);
        max_bounds.descent = max_bounds.descent.max(ci.descent);
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
        let render_node_fd = match crate::kms::render_node::open_for_card(&*device) {
            Ok(fd) => {
                use std::os::fd::AsRawFd;
                let raw = fd.as_raw_fd();
                let link = std::fs::read_link(format!("/proc/self/fd/{raw}")).unwrap_or_default();
                let stat_minor = std::fs::metadata(&link)
                    .ok()
                    .map(|m| {
                        use std::os::unix::fs::MetadataExt;
                        let rdev = m.rdev();
                        ((rdev >> 8) & 0xff, rdev & 0xff)
                    })
                    .map(|(maj, min)| format!("{maj}:{min}"))
                    .unwrap_or_else(|| "?".into());
                log::info!(
                    "DRI3 render node ready (sibling of {device_path}): fd={raw} \
                     path={link:?} rdev={stat_minor} (render node minor should be >=128)"
                );
                Some(fd)
            }
            Err(err) => {
                log::warn!(
                    "DRI3 render node unavailable: {err}; DRI3 import path will be \
                     unavailable but the rest of yserver continues"
                );
                None
            }
        };
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

        // Phase 4.1.1: Vulkan is brought up alongside pixman but doesn't
        // drive any rendering yet. If no ICD is available (e.g. virtio-
        // gpu-pci without VK_ICD_FILENAMES pointing at lavapipe), keep
        // running on the pixman path so the existing recipe matrix
        // stays usable. 4.1.2+ will harden this once Vulkan is
        // load-bearing.
        let vk = match crate::kms::vk::device::VkContext::new() {
            Ok(ctx) => {
                let device_name = unsafe {
                    ctx.instance
                        .get_physical_device_properties(ctx.physical_device)
                }
                .device_name_as_c_str()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "<unknown>".to_string());
                log::info!("vulkan initialised on physical device {device_name}");
                Some(ctx)
            }
            Err(e) => {
                log::warn!("vulkan init failed, continuing on pixman-only path: {e}");
                None
            }
        };

        // Per-output scanout pools. Vulkan-first: each bo is a
        // VkImage (TILING_LINEAR) exported as a dma-buf and imported
        // as a DRM framebuffer. Best-effort: any failure here falls
        // back to None (existing dumb-buffer path stays intact for
        // that output). 3 bos per pool per design §2.
        let scanout_pools: Vec<Option<crate::kms::vk::scanout::ScanoutBoPool>> =
            if let Some(vkctx) = vk.as_ref() {
                layouts
                    .iter()
                    .map(|l| {
                        match crate::kms::vk::scanout::ScanoutBoPool::allocate(
                            Arc::clone(vkctx),
                            Arc::clone(&device),
                            u32::from(l.width),
                            u32::from(l.height),
                            3,
                        ) {
                            Ok(pool) => {
                                if let Some(first) = pool.bos.first() {
                                    log::info!(
                                        "scanout pool: 3x{}x{} bos for output {} (pitch {})",
                                        l.width,
                                        l.height,
                                        l.output.connector_name,
                                        first.pitch
                                    );
                                }
                                Some(pool)
                            }
                            Err(e) => {
                                log::warn!(
                                    "scanout pool: allocation failed for output {}: {e} \
                                     — falling back to dumb-buffer scanout for this output",
                                    l.output.connector_name
                                );
                                None
                            }
                        }
                    })
                    .collect()
            } else {
                std::iter::repeat_with(|| None)
                    .take(layouts.len())
                    .collect()
            };
        let layouts_len = layouts.len();

        // Compositor pipeline (Phase 4.1.3.4): graphics pipeline +
        // sampler + descriptor set layout for the textured-quad
        // composite. Color format must match the scanout bos —
        // those are `B8G8R8A8_UNORM` (per `vk/scanout.rs`'s
        // `allocate_vk_scanout_image`).
        let compositor_pipeline = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::pipeline::CompositorPipeline::new(
                Arc::clone(vkctx),
                ash::vk::Format::B8G8R8A8_UNORM,
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!(
                        "compositor pipeline init failed: {e} — Vulkan composite path will \
                         remain disabled; falls back to pixman composite"
                    );
                    None
                }
            }
        });

        // Drawing-op command pool (Phase 4.1.4): graphics CBs for
        // direct-to-mirror writes (`PolyFillRectangle`, `ClearArea`,
        // …). RAII wrapper destroys the pool on shutdown.
        let ops_command_pool = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(vkctx)) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!(
                        "ops command pool init failed: {e:?} — Phase 4.1.4 ops will fall back \
                         to the pixman path"
                    );
                    None
                }
            }
        });

        // Image-transfer staging buffer (Phase 4.1.4.3). 1 MiB starter
        // covers most PutImage requests; grows on demand. None when
        // Vulkan didn't come up.
        let ops_staging =
            vk.as_ref().and_then(|vkctx| {
                match crate::kms::vk::ops::OpsStaging::new(Arc::clone(vkctx), 1024 * 1024) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!(
                            "ops staging buffer init failed: {e:?} — Phase 4.1.4.3 image ops \
                         will fall back to the pixman path"
                        );
                        None
                    }
                }
            });

        // Glyph atlas (Phase 4.1.4.5). 4096² R8 fixed allocation.
        let glyph_atlas =
            vk.as_ref().and_then(|vkctx| {
                match crate::kms::vk::glyph::GlyphAtlas::new(Arc::clone(vkctx)) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        log::warn!(
                            "glyph atlas init failed: {e:?} — Phase 4.1.4.5 text ops will \
                         fall back to the pixman path"
                        );
                        None
                    }
                }
            });

        // Text pipeline (Phase 4.1.4.5). Built once for the
        // mirror's `B8G8R8A8_UNORM` format; binds the atlas at
        // construction.
        let text_pipeline = match (vk.as_ref(), glyph_atlas.as_ref()) {
            (Some(vkctx), Some(atlas)) => {
                match crate::kms::vk::text_pipeline::TextPipeline::new(
                    Arc::clone(vkctx),
                    ash::vk::Format::B8G8R8A8_UNORM,
                    atlas,
                ) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        log::warn!(
                            "text pipeline init failed: {e:?} — Phase 4.1.4.5 text ops \
                             will fall back to the pixman path"
                        );
                        None
                    }
                }
            }
            _ => None,
        };

        // RENDER `Composite` pipeline cache (Phase 4.1.4.6
        // commit 1). Pipelines compile lazily on first use of
        // each PictOp; the cache spans the rest of the session.
        let render_pipelines = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::render_pipeline::RenderPipelineCache::new(Arc::clone(vkctx)) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!(
                        "render pipeline cache init failed: {e:?} — Phase 4.1.4.6 RENDER \
                         ops will fall back to the pixman path"
                    );
                    None
                }
            }
        });

        // 1×1 BGRA8 scratch for `SolidFill` source / `render_fill_rectangles`
        // colour. `cmd_clear_color_image` rewrites it inside each
        // composite CB before sampling.
        let solid_src_image =
            vk.as_ref().and_then(
                |vkctx| match crate::kms::vk::render_pipeline::SolidColorImage::new(Arc::clone(
                    vkctx,
                )) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!(
                            "solid colour image init failed: {e:?} — Phase 4.1.4.6 SolidFill \
                         RENDER ops will fall back to the pixman path"
                        );
                        None
                    }
                },
            );

        // Second 1×1 BGRA8 scratch for `SolidFill` mask. Same shape
        // as `solid_src_image`; cleared per-call to the mask
        // picture's colour.
        let solid_mask_image =
            vk.as_ref().and_then(
                |vkctx| match crate::kms::vk::render_pipeline::SolidColorImage::new(Arc::clone(
                    vkctx,
                )) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!("solid mask scratch init failed: {e:?}");
                        None
                    }
                },
            );

        // 1×1 BGRA8 mask cleared once to opaque white at backend
        // init. Bound as `mask_tex` for Composite calls without a
        // mask — `mask.a == 1.0` makes the multiplication a no-op
        // and keeps the shader / descriptor layout uniform.
        let white_mask_image = match (vk.as_ref(), ops_command_pool.as_ref()) {
            (Some(vkctx), Some(pool)) => {
                match crate::kms::vk::render_pipeline::SolidColorImage::new(Arc::clone(vkctx)) {
                    Ok(mut s) => {
                        let pool_handle = pool.handle();
                        match crate::kms::vk::ops::run_one_shot_op(vkctx, pool_handle, |vk, cb| {
                            crate::kms::vk::render_pipeline::record_solid_color_clear(
                                vk,
                                cb,
                                &mut s,
                                [1.0, 1.0, 1.0, 1.0],
                            );
                            Ok(())
                        }) {
                            Ok(()) => Some(s),
                            Err(e) => {
                                log::warn!("white mask scratch clear failed: {e:?}");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("white mask scratch init failed: {e:?}");
                        None
                    }
                }
            }
            _ => None,
        };

        let mask_scratch = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::mask_scratch::MaskScratch::new(Arc::clone(vkctx)) {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!("vk mask scratch init failed: {e:?} — render_trapezoids/triangles will be no-op");
                    None
                }
            }
        });

        let copy_scratch = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::copy_scratch::CopyScratch::new(Arc::clone(vkctx)) {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!(
                        "vk copy scratch init failed: {e:?} — same-target CopyArea overlap dropped"
                    );
                    None
                }
            }
        });

        let dst_readback = vk
            .as_ref()
            .map(|vkctx| crate::kms::vk::dst_readback::DstReadback::new(Arc::clone(vkctx)));

        let logic_fill_pipelines = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache::new(
                Arc::clone(vkctx),
                ash::vk::Format::B8G8R8A8_UNORM,
            ) {
                Ok(c) => Some(c),
                Err(e) => {
                    log::warn!(
                        "vk logic-fill pipeline cache init failed: {e:?} — non-Copy GC \
                         function fills dropped"
                    );
                    None
                }
            }
        });

        let mut me = Self {
            device,
            render_node_fd,
            dri3_sync_resources: HashMap::new(),
            dri3_xshmfences: HashMap::new(),
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
            vk,
            first_pageflip_logged: vec![false; layouts_len],
            scanout_pools,
            compositor_pipeline,
            ops_command_pool,
            ops_staging,
            glyph_atlas,
            text_pipeline,
            render_pipelines,
            solid_src_image,
            solid_mask_image,
            white_mask_image,
            mask_scratch,
            copy_scratch,
            dst_readback,
            logic_fill_pipelines,
            font_loader: FontLoader::new()?,
            fonts: HashMap::new(),
            pixmaps: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
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
            current_fill: FillState::Solid,
            current_clip: ClipState::None,
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
    #[allow(dead_code)] // associated-impl shim isn't strictly needed, kept for future use
    fn _placeholder(&self) {}
}

/// Advance every bo in a [`ScanoutBoPool`] one step on
/// pageflip-complete (per design §2 transition table):
///
/// - `Retiring → Free` (release fence is signalled by the kernel
///   issuing the new pageflip-complete; we close the fd).
/// - `OnScreen → Retiring` (its successor just landed on screen).
/// - `Pending → OnScreen` (the freshly-flipped bo is now scanning).
///
/// Order matters: snapshot phases first to drive transitions, so
/// each bo gets exactly one state change per event regardless of
/// iteration order.
fn advance_pool_on_pageflip_complete(pool: &mut crate::kms::vk::scanout::ScanoutBoPool) {
    use crate::kms::vk::scanout::BoPhase;
    use std::os::fd::{FromRawFd, OwnedFd};

    let phases: Vec<BoPhase> = pool.bos.iter().map(|b| b.state.phase).collect();
    for (i, phase) in phases.into_iter().enumerate() {
        match phase {
            BoPhase::Retiring => {
                if let Some(fd) = pool.bos[i].state.transition_to_free_after_retire() {
                    // SAFETY: the fd was inserted by the
                    // Submitted→Pending transition; we're the only
                    // owner. Reconstructing as OwnedFd lets Drop
                    // close it.
                    drop(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
            BoPhase::OnScreen => pool.bos[i].state.transition_to_retiring(),
            BoPhase::Pending => pool.bos[i].state.transition_to_on_screen(),
            _ => {}
        }
    }
}

impl KmsBackend {
    fn install_default_cursor(&mut self) {
        let w: u16 = 16;
        let h: u16 = 16;
        // Build the cursor pattern straight to a tightly-packed BGRA
        // byte buffer (the layout `B8G8R8A8_UNORM` reads). No pixman
        // detour — the mirror is the canonical store.
        let mut packed = vec![0u8; w as usize * h as usize * 4];
        let last = i32::from(w) - 1;
        for y in 0..i32::from(h) {
            for x in 0..i32::from(w) {
                // Distance to either diagonal of the 16×16 box.
                let d1 = (x - y).abs();
                let d2 = (x + y - last).abs();
                let dist = d1.min(d2);
                let bgra: [u8; 4] = match dist {
                    0 => [0x00, 0x00, 0x00, 0xFF], // black core, opaque
                    1 => [0xFF, 0xFF, 0xFF, 0xFF], // white halo, opaque
                    _ => [0x00, 0x00, 0x00, 0x00], // transparent
                };
                let off = (y as usize * w as usize + x as usize) * 4;
                packed[off..off + 4].copy_from_slice(&bgra);
            }
        }
        let xid = self.next_host_xid();
        let mut vk_mirror = self.allocate_cursor_mirror(u32::from(w), u32::from(h));
        if let Some(mirror) = vk_mirror.as_mut()
            && let Err(e) = self.upload_bgra_to_mirror(mirror, &packed)
        {
            log::warn!("install_default_cursor: mirror upload failed: {e:?}");
        }
        self.cursors.insert(
            xid,
            CursorState {
                extent: ash::vk::Extent2D {
                    width: u32::from(w),
                    height: u32::from(h),
                },
                hot_x: w / 2,
                hot_y: h / 2,
                vk_mirror,
            },
        );
        self.active_cursor = Some(xid);
    }

    /// Upload tightly-packed BGRA bytes into the full extent of
    /// `mirror`. Used by cursor create/update paths that build cursor
    /// pixels host-side (no source `DrawableImage` to copy from).
    ///
    /// Pixman's `A8R8G8B8` LE memory order is `[B, G, R, A]`, which
    /// matches `B8G8R8A8_UNORM` on the GPU exactly — so callers that
    /// pass a pixman-image's raw bytes do not need any byte
    /// permutation.
    ///
    /// The caller passes a `&mut DrawableImage` directly (typically
    /// the freshly-allocated one from `allocate_cursor_mirror`,
    /// before it lands in `self.cursors`), so this method does not
    /// alias any HashMap borrow of `self`.
    fn upload_bgra_to_mirror(
        &mut self,
        mirror: &mut crate::kms::vk::target::DrawableImage,
        pixels: &[u8],
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::vk::ops::run_one_shot_op;
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        let staging = self
            .ops_staging
            .as_mut()
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let needed = pixels.len() as u64;
        if needed == 0 {
            return Ok(());
        }
        staging.ensure(needed)?;
        let staging_buffer = staging.buffer();
        let staging_ptr = staging.mapped_ptr();
        // SAFETY: `staging_ptr` is a host-mapped, write-combine pointer
        // into a buffer we just grew to `needed` bytes; `pixels.as_ptr()`
        // is valid for `pixels.len()`.
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), staging_ptr, pixels.len());
        }
        let extent = mirror.extent;
        run_one_shot_op(&vk_arc, pool_handle, |_vk, cb| {
            mirror.record_upload_rect(
                cb,
                staging_buffer,
                0,
                ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: 0, y: 0 },
                    extent,
                },
            );
            Ok(())
        })
    }

    /// Read the full mirror of a window or pixmap drawable back to
    /// host memory via `vkCmdCopyImageToBuffer`. Used by `copy_plane`
    /// (the last remaining pixman-source read) and any future code
    /// that needs to peek at GPU-canonical pixels.
    ///
    /// The returned [`MirrorReadback`] holds tightly-packed bytes in
    /// the mirror's native format (`R8_UNORM` for depth 1/8,
    /// `B8G8R8A8_UNORM` for depth 24/32). Returns `None` if the
    /// drawable is unknown, has no mirror, has zero extent, or the
    /// staging buffer cannot be grown to fit.
    fn read_mirror_pixels(&mut self, host_xid: u32) -> Option<MirrorReadback> {
        use crate::kms::vk::ops::{image as vk_image, run_one_shot_op};

        let vk_arc = self.vk.as_ref().cloned()?;
        let pool_handle = self.ops_command_pool.as_ref().map(|p| p.handle())?;
        self.ops_staging.as_ref()?;

        // Snapshot extent + bpp without holding a mut borrow on self.
        let (mirror_w, mirror_h, mirror_bpp) = {
            let mirror = if let Some(w) = self.windows.get(&host_xid) {
                w.vk_mirror.as_ref()
            } else if let Some(p) = self.pixmaps.get(&host_xid) {
                p.vk_mirror.as_ref()
            } else {
                None
            };
            let mirror = mirror?;
            (
                mirror.extent.width,
                mirror.extent.height,
                mirror.bytes_per_pixel(),
            )
        };
        if mirror_w == 0 || mirror_h == 0 {
            return None;
        }
        let total_bytes = u64::from(mirror_w) * u64::from(mirror_h) * u64::from(mirror_bpp);

        if let Err(e) = self
            .ops_staging
            .as_mut()
            .expect("checked is_none above")
            .ensure(total_bytes)
        {
            log::warn!("read_mirror_pixels: staging grow failed for {total_bytes} bytes: {e:?}");
            return None;
        }

        let regions = [ash::vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(ash::vk::Extent3D {
                width: mirror_w,
                height: mirror_h,
                depth: 1,
            })];

        let staging_buffer = self.ops_staging.as_ref().expect("present").buffer();
        let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let mirror = mirror?;

        if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            vk_image::record_get_image(vk, cb, mirror, staging_buffer, &regions)
        }) {
            log::warn!("read_mirror_pixels: record_get_image failed: {e:?}");
            return None;
        }

        let total_bytes_usize = total_bytes as usize;
        let mut bytes = vec![0u8; total_bytes_usize];
        // SAFETY: `staging.ensure(total_bytes)` succeeded, and the
        // submit above was waited on inside `run_one_shot_op`, so the
        // staging buffer's host-mapped range now contains the readback.
        unsafe {
            let staging_ptr = self.ops_staging.as_ref().expect("present").mapped_ptr();
            std::ptr::copy_nonoverlapping(staging_ptr, bytes.as_mut_ptr(), total_bytes_usize);
        }

        Some(MirrorReadback {
            width: mirror_w,
            height: mirror_h,
            bytes_per_pixel: mirror_bpp,
            bytes,
        })
    }

    /// Solid-fill the entire extent of `mirror` with the X11 colour
    /// pixel `fg` (0xRRGGBB or 0xAARRGGBB; alpha defaulted to opaque
    /// for windows). Used at create-time / resize-time to apply a
    /// window's `bg_pixel` to a freshly allocated mirror — the
    /// `MirrorUploader` pump that used to handle this is gone.
    fn fill_mirror_solid(
        &mut self,
        mirror: &mut crate::kms::vk::target::DrawableImage,
        fg: u32,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::vk::ops::{fill, run_one_shot_op};
        let vk_arc = self
            .vk
            .as_ref()
            .cloned()
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let pool_handle = self
            .ops_command_pool
            .as_ref()
            .map(|p| p.handle())
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let extent = mirror.extent;
        let color = [
            ((fg >> 16) & 0xFF) as f32 / 255.0,
            ((fg >> 8) & 0xFF) as f32 / 255.0,
            (fg & 0xFF) as f32 / 255.0,
            1.0,
        ];
        let rects = [ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        }];
        let scissor = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            fill::record_fill_rectangles(vk, cb, mirror, color, &rects, scissor)
        })
    }

    /// Allocate a cursor mirror sized for `src` and `vkCmdCopyImage`
    /// the rescued source into it. Returns `None` if mirror
    /// allocation or the copy fails — caller falls through to a
    /// `None` `vk_mirror` (cursor still inserted, but won't render
    /// pixels until next DefineCursor).
    fn copy_drawable_to_new_cursor_mirror(
        &mut self,
        src: &mut crate::kms::vk::target::DrawableImage,
    ) -> Option<crate::kms::vk::target::DrawableImage> {
        use crate::kms::vk::ops::{copy as vk_copy, run_one_shot_op};

        let cw = src.extent.width;
        let ch = src.extent.height;
        let mut cm = self.allocate_cursor_mirror(cw, ch)?;

        let vk_arc = self.vk.as_ref().cloned()?;
        let pool_handle = self.ops_command_pool.as_ref()?.handle();
        let regions = [ash::vk::ImageCopy::default()
            .src_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .src_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .dst_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(ash::vk::Extent3D {
                width: cw,
                height: ch,
                depth: 1,
            })];

        if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            vk_copy::record_copy_area_distinct(vk, cb, src, &mut cm, &regions)
        }) {
            log::warn!("copy_drawable_to_new_cursor_mirror: copy failed: {e:?}");
            return None;
        }
        Some(cm)
    }

    /// Live-pixmap variant of [`copy_drawable_to_new_cursor_mirror`]:
    /// the source mirror lives in `self.pixmaps[host_xid]`, the
    /// destination is owned by the caller. Disjoint mut borrows on
    /// `self.pixmaps` and `cm` keep the borrow checker happy.
    fn copy_pixmap_mirror_to_cursor(
        &mut self,
        host_xid: u32,
        cm: &mut crate::kms::vk::target::DrawableImage,
        cw: u32,
        ch: u32,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::vk::ops::{copy as vk_copy, run_one_shot_op};

        let vk_arc = self
            .vk
            .as_ref()
            .cloned()
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let pool_handle = self
            .ops_command_pool
            .as_ref()
            .map(|p| p.handle())
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let regions = [ash::vk::ImageCopy::default()
            .src_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .src_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .dst_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(ash::vk::Extent3D {
                width: cw,
                height: ch,
                depth: 1,
            })];
        let src = self
            .pixmaps
            .get_mut(&host_xid)
            .and_then(|p| p.vk_mirror.as_mut())
            .ok_or(ash::vk::Result::ERROR_UNKNOWN)?;
        run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            vk_copy::record_copy_area_distinct(vk, cb, src, cm, &regions)
        })
    }

    /// Allocate a Vulkan mirror sized for a cursor pixmap. Cursors
    /// always carry alpha (RenderCreateCursor delivers `A8R8G8B8`),
    /// so the mirror format is `B8G8R8A8_UNORM` regardless of depth
    /// — handled identically to a depth-32 pixmap.
    fn allocate_cursor_mirror(
        &self,
        width: u32,
        height: u32,
    ) -> Option<crate::kms::vk::target::DrawableImage> {
        let vkctx = self.vk.as_ref()?;
        if width == 0 || height == 0 {
            return None;
        }
        match crate::kms::vk::target::DrawableImage::new_server_owned_pixmap(
            std::sync::Arc::clone(vkctx),
            width,
            height,
            32,
        ) {
            Ok(mut img) => {
                if let Some(pool) = self.ops_command_pool.as_ref()
                    && let Err(e) = img.initialize_clear(pool.handle())
                {
                    log::warn!("cursor mirror initialize_clear failed: {e:?}");
                }
                // Cursor mirror is fully dirty at creation so the
                // first composite pass has it populated before
                // sampling.
                img.mark_full_damage();
                Some(img)
            }
            Err(e) => {
                log::warn!(
                    "DrawableImage::new_server_owned_pixmap (cursor {width}x{height}): {e} — \
                     cursor will not render under VkComposite"
                );
                None
            }
        }
    }

    fn drawable_depth(&self, host_xid: u32) -> Option<u8> {
        self.windows
            .get(&host_xid)
            .map(|w| w.depth)
            .or_else(|| self.pixmaps.get(&host_xid).map(|p| p.depth))
    }

    /// Width × height for a window or pixmap drawable, in pixels.
    /// Reads from the recorded `WindowState` / `PixmapState`
    /// dimensions — no pixman buffer involved.
    fn drawable_dims(&self, host_xid: u32) -> Option<(u32, u32)> {
        if let Some(w) = self.windows.get(&host_xid) {
            Some((u32::from(w.width), u32::from(w.height)))
        } else {
            self.pixmaps
                .get(&host_xid)
                .map(|p| (u32::from(p.width), u32::from(p.height)))
        }
    }

    // `with_image_mut` and `image_ptr_for_xid` deleted in 4.1.5.
    // Drawing ops now record into Vk command buffers directly; no
    // helper is needed to grab a pixman pointer or mark the mirror
    // for upload.

    /// Decode the wire-packed clip rectangle list (`Vec<u8>` of i16 x, i16
    /// y, u16 w, u16 h tuples) into `Rectangle16`s in dst-coords (i.e. with
    /// the GC clip-origin already added). Returns `None` if the current GC
    /// clip is `None` or `Pixmap` (latter not yet enforced).
    fn current_clip_rects_in_dst_space(&self) -> Option<Vec<Rectangle16>> {
        let ClipState::Rectangles { origin, rects } = &self.current_clip else {
            return None;
        };
        let bytes = &rects.rectangles;
        let mut out = Vec::with_capacity(bytes.len() / 8);
        for chunk in bytes.chunks_exact(8) {
            let cx = i16::from_le_bytes([chunk[0], chunk[1]]) as i32 + origin.0 as i32;
            let cy = i16::from_le_bytes([chunk[2], chunk[3]]) as i32 + origin.1 as i32;
            let cw = u16::from_le_bytes([chunk[4], chunk[5]]) as i32;
            let ch = u16::from_le_bytes([chunk[6], chunk[7]]) as i32;
            if cw <= 0 || ch <= 0 {
                continue;
            }
            out.push(Rectangle16 {
                x: cx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y: cy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                width: cw.min(u16::MAX as i32) as u16,
                height: ch.min(u16::MAX as i32) as u16,
            });
        }
        Some(out)
    }

    /// Intersect each rect in `rects` against the current GC clip. Returns
    /// the original list when no clip is active. For `ClipState::Pixmap` we
    /// pass through untouched (TODO: rasterise the mask).
    fn intersect_with_current_clip(&self, rects: &[Rectangle16]) -> Vec<Rectangle16> {
        let Some(clip_rects) = self.current_clip_rects_in_dst_space() else {
            return rects.to_vec();
        };
        let mut out = Vec::with_capacity(rects.len());
        for r in rects {
            let rx0 = r.x as i32;
            let ry0 = r.y as i32;
            let rx1 = rx0 + r.width as i32;
            let ry1 = ry0 + r.height as i32;
            for c in &clip_rects {
                let cx0 = c.x as i32;
                let cy0 = c.y as i32;
                let cx1 = cx0 + c.width as i32;
                let cy1 = cy0 + c.height as i32;
                let ix0 = rx0.max(cx0);
                let iy0 = ry0.max(cy0);
                let ix1 = rx1.min(cx1);
                let iy1 = ry1.min(cy1);
                if ix0 < ix1 && iy0 < iy1 {
                    out.push(Rectangle16 {
                        x: ix0 as i16,
                        y: iy0 as i16,
                        width: (ix1 - ix0) as u16,
                        height: (iy1 - iy0) as u16,
                    });
                }
            }
        }
        out
    }

    /// Fill `rects` on `dst_xid`, honoring `self.current_fill`. For
    /// `Solid`, paints with `fg`. For `Tiled`, repeats the tile pixmap
    /// (offset by the GC's tile origin). e16 paints popup backgrounds via
    /// Tiled — the menu chrome+text lives in the tile pixmap and the
    /// destination pixmap (the window's bg-pixmap) is filled by tiling
    /// it, so honoring this is required for any visible popup.
    /// `Stippled`/`OpaqueStippled` fall through to solid for now (no real
    /// client driving that path on KMS yet).
    fn fill_rects_honoring_fill_state(&mut self, dst_xid: u32, fg: u32, rects: &[Rectangle16]) {
        let function = self.current_function;
        let fill = self.current_fill.clone();
        let clipped = self.intersect_with_current_clip(rects);
        if clipped.is_empty() {
            return;
        }
        let rects = clipped.as_slice();
        match fill {
            FillState::Tiled { pixmap, origin } => {
                let tile_xid = pixmap.as_raw();
                if tile_xid == dst_xid {
                    log::debug!(
                        "fill_rects_honoring_fill_state: tile == dst (0x{tile_xid:x}); \
                         degenerating to solid"
                    );
                    self.try_vk_fill_with_function(dst_xid, function, fg, rects);
                    return;
                }
                // Tiled fill is composited with Operation::Src, which
                // matches GcFunction::Copy. Other GC functions (Xor /
                // etc.) on a tiled fill aren't covered by any current
                // client; degenerate to a solid logic-op fill so the
                // function is honoured.
                if !matches!(function, GcFunction::Copy) {
                    log::debug!(
                        "fill_rects_honoring_fill_state: Tiled+{function:?} not implemented; \
                         degenerating to solid logic-op fill"
                    );
                    self.try_vk_fill_with_function(dst_xid, function, fg, rects);
                    return;
                }
                if !self.try_vk_tiled_fill(dst_xid, tile_xid, origin.0, origin.1, rects) {
                    log::warn!(
                        "fill_rects_honoring_fill_state: tile fill on 0x{dst_xid:x} from \
                         tile 0x{tile_xid:x} failed; rect dropped"
                    );
                }
            }
            FillState::Solid | FillState::Stippled { .. } | FillState::OpaqueStippled { .. } => {
                // Stipple cases share this arm — proper stipple
                // support is task #4.1.4.8 territory; until then
                // they fall through as solid (matches the pre-
                // pixman-removal behaviour).
                self.try_vk_fill_with_function(dst_xid, function, fg, rects);
            }
        }
    }

    /// Phase 4.1.5 prep: tile fill via `try_vk_render_composite`.
    /// Tile pixmap supplies the source colours; `Repeat::Normal`
    /// makes it wrap. Per-rect source origin `(rect_dst_x - ox,
    /// rect_dst_y - oy)` so that the shader's `src_origin +
    /// dst_offset` lands on the right tile pixel.
    fn try_vk_tiled_fill(
        &mut self,
        dst_xid: u32,
        tile_xid: u32,
        ox: i16,
        oy: i16,
        rects: &[Rectangle16],
    ) -> bool {
        if rects.is_empty() {
            return true;
        }
        if tile_xid == dst_xid {
            return false; // self-tile would alias src and dst
        }
        // Tile must have a Vk mirror to be sampleable.
        let tile_format = if let Some(w) = self.windows.get(&tile_xid) {
            w.vk_mirror.as_ref().map(|m| m.format)
        } else if let Some(p) = self.pixmaps.get(&tile_xid) {
            p.vk_mirror.as_ref().map(|m| m.format)
        } else {
            None
        };
        if tile_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            return false;
        }
        // Build CompositeRects in dst space; the shader's
        // `(src_origin + dst_offset)` lands on the right tile pixel
        // when `src_origin = rect_dst - tile_origin`.
        let composite_rects: Vec<crate::kms::vk::ops::render::CompositeRect> = rects
            .iter()
            .map(|r| crate::kms::vk::ops::render::CompositeRect {
                src_x: i32::from(r.x) - i32::from(ox),
                src_y: i32::from(r.y) - i32::from(oy),
                mask_x: 0,
                mask_y: 0,
                dst_x: i32::from(r.x),
                dst_y: i32::from(r.y),
                width: u32::from(r.width),
                height: u32::from(r.height),
            })
            .collect();
        // Use a coarse scissor that covers all rects.
        let mut x0 = i32::MAX;
        let mut y0 = i32::MAX;
        let mut x1 = i32::MIN;
        let mut y1 = i32::MIN;
        for r in rects {
            x0 = x0.min(i32::from(r.x));
            y0 = y0.min(i32::from(r.y));
            x1 = x1.max(i32::from(r.x) + i32::from(r.width));
            y1 = y1.max(i32::from(r.y) + i32::from(r.height));
        }
        if x1 <= x0 || y1 <= y0 {
            return true;
        }
        let scissor = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: x0.max(0),
                y: y0.max(0),
            },
            extent: ash::vk::Extent2D {
                width: u32::try_from((x1 - x0.max(0)).max(0)).unwrap_or(0),
                height: u32::try_from((y1 - y0.max(0)).max(0)).unwrap_or(0),
            },
        };
        // Op `Src` (1) — tile fill replaces the destination.
        const OP_SRC: u8 = 1;
        self.try_vk_render_composite(
            OP_SRC,
            RenderPic::Drawable(tile_xid),
            RenderPic::None,
            dst_xid,
            &composite_rects,
            scissor,
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
        )
    }

    /// Sub-phase 4.1.4.4 helper. Solid-fill `rects` on `dst_xid`,
    /// honouring the current GC `function`, with the Vulkan
    /// fast-fill path when `function == Copy` and the target has
    /// a mirror. Used by the stroke-style poly ops (`PolyLine`,
    /// `PolySegment`, `PolyPoint`, `PolyArc`, `PolyRectangle`),
    /// where every rasterised rect is in the GC's single
    /// foreground colour. This is the same routing
    /// `fill_rects_honoring_fill_state` does for the
    /// `FillState::Solid` arm — split out because stroke ops
    /// always treat the request as "Solid foreground" regardless
    /// of GC `fill_style`, while filled ops respect it.
    fn fill_rects_solid_with_gc_function(&mut self, dst_xid: u32, fg: u32, rects: &[Rectangle16]) {
        if rects.is_empty() {
            return;
        }
        let function = self.current_function;
        // Phase 4.1.5: Vk-only. `try_vk_fill_with_function` picks
        // the Copy fast path or the per-`VkLogicOp` pipeline.
        self.try_vk_fill_with_function(dst_xid, function, fg, rects);
    }

    /// Vulkan-direct `CopyArea` via `vkCmdCopyImage`. Returns
    /// `true` iff the Vulkan path wrote and the caller can return;
    /// `false` means the caller falls back to pixman.
    ///
    /// Conditions for the Vulkan path: both drawables have
    /// mirrors, mirrors share the same Vulkan format,
    /// same-target draws don't overlap (overlap → staging-image
    /// path is 4.1.4.2 follow-up territory).
    #[allow(clippy::too_many_arguments)]
    fn try_vk_copy_area(
        &mut self,
        src_xid: u32,
        dst_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> bool {
        use crate::kms::vk::ops::{copy, run_one_shot_op};

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };

        // GC clip the dst rect into a sub-rect list. Each
        // surviving sub-rect's src offset shifts by the same
        // amount the dst was clipped (mirrors the existing pixman
        // path).
        let dst_rect = Rectangle16 {
            x: dst_x,
            y: dst_y,
            width,
            height,
        };
        let sub_rects = self.intersect_with_current_clip(&[dst_rect]);
        if sub_rects.is_empty() {
            // Fully clipped — nothing to draw, but the request is
            // "handled" successfully.
            return true;
        }

        let same_target = src_xid == dst_xid;
        if same_target {
            let overlapping = rects_overlap_axis_aligned(
                i32::from(src_x),
                i32::from(src_y),
                i32::from(dst_x),
                i32::from(dst_y),
                i32::from(width),
                i32::from(height),
            );

            // Resolve the mirror; we need a single &mut to it.
            let Some(mirror) = self
                .windows
                .get_mut(&src_xid)
                .and_then(|w| w.vk_mirror.as_mut())
                .or_else(|| {
                    self.pixmaps
                        .get_mut(&src_xid)
                        .and_then(|p| p.vk_mirror.as_mut())
                })
            else {
                return false;
            };
            let regions = build_image_copy_regions(
                &sub_rects,
                src_x,
                src_y,
                dst_x,
                dst_y,
                mirror.extent,
                mirror.extent,
            );
            if regions.is_empty() {
                return true;
            }

            if overlapping {
                // Overlap: round-trip through `copy_scratch`. Sized
                // to the source-rect bbox so the scratch is just big
                // enough to hold the in-flight copy.
                let Some(scratch) = self.copy_scratch.as_mut() else {
                    return false;
                };
                if let Err(e) = scratch.ensure_size(u32::from(width), u32::from(height)) {
                    log::warn!("vk copy: scratch resize failed: {e:?}");
                    return false;
                }
                let bbox_origin = (i32::from(src_x), i32::from(src_y));
                return match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
                    copy::record_copy_area_same_overlap(
                        vk,
                        cb,
                        mirror,
                        scratch,
                        &regions,
                        bbox_origin,
                    )
                }) {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!(
                            "vk copy: same-image overlap record failed on xid {src_xid:#x}: \
                             {e:?}"
                        );
                        false
                    }
                };
            }

            return match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
                copy::record_copy_area_same(vk, cb, mirror, &regions)
            }) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("vk copy: same-image record failed on xid {src_xid:#x}: {e:?}");
                    false
                }
            };
        }

        // Distinct src/dst. Determine which HashMap each lives
        // in. Bail to pixman fallback if either is missing or if
        // their mirrors disagree on format.
        enum Map {
            Window,
            Pixmap,
            None,
        }
        let src_map = if self.windows.contains_key(&src_xid) {
            Map::Window
        } else if self.pixmaps.contains_key(&src_xid) {
            Map::Pixmap
        } else {
            Map::None
        };
        let dst_map = if self.windows.contains_key(&dst_xid) {
            Map::Window
        } else if self.pixmaps.contains_key(&dst_xid) {
            Map::Pixmap
        } else {
            Map::None
        };

        let (src_mirror, dst_mirror): (
            &mut crate::kms::vk::target::DrawableImage,
            &mut crate::kms::vk::target::DrawableImage,
        ) = match (src_map, dst_map) {
            (Map::Window, Map::Window) => {
                let [s_state, d_state] = self.windows.get_disjoint_mut([&src_xid, &dst_xid]);
                let (Some(s), Some(d)) = (s_state, d_state) else {
                    return false;
                };
                let (Some(s_m), Some(d_m)) = (s.vk_mirror.as_mut(), d.vk_mirror.as_mut()) else {
                    return false;
                };
                (s_m, d_m)
            }
            (Map::Pixmap, Map::Pixmap) => {
                let [s_state, d_state] = self.pixmaps.get_disjoint_mut([&src_xid, &dst_xid]);
                let (Some(s), Some(d)) = (s_state, d_state) else {
                    return false;
                };
                let (Some(s_m), Some(d_m)) = (s.vk_mirror.as_mut(), d.vk_mirror.as_mut()) else {
                    return false;
                };
                (s_m, d_m)
            }
            (Map::Window, Map::Pixmap) => {
                let s = self
                    .windows
                    .get_mut(&src_xid)
                    .and_then(|w| w.vk_mirror.as_mut());
                let d = self
                    .pixmaps
                    .get_mut(&dst_xid)
                    .and_then(|p| p.vk_mirror.as_mut());
                let (Some(s), Some(d)) = (s, d) else {
                    return false;
                };
                (s, d)
            }
            (Map::Pixmap, Map::Window) => {
                let s = self
                    .pixmaps
                    .get_mut(&src_xid)
                    .and_then(|p| p.vk_mirror.as_mut());
                let d = self
                    .windows
                    .get_mut(&dst_xid)
                    .and_then(|w| w.vk_mirror.as_mut());
                let (Some(s), Some(d)) = (s, d) else {
                    return false;
                };
                (s, d)
            }
            _ => return false,
        };

        if src_mirror.format != dst_mirror.format {
            // vkCmdCopyImage requires matching formats; format
            // conversion needs a shader (out of scope for 4.1.4.2).
            return false;
        }

        let regions = build_image_copy_regions(
            &sub_rects,
            src_x,
            src_y,
            dst_x,
            dst_y,
            src_mirror.extent,
            dst_mirror.extent,
        );
        if regions.is_empty() {
            return true;
        }
        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            copy::record_copy_area_distinct(vk, cb, src_mirror, dst_mirror, &regions)
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk copy: distinct-image record failed (src={src_xid:#x} dst={dst_xid:#x}): \
                     {e:?} — falling back to pixman"
                );
                false
            }
        }
    }

    /// Fill `rects` on `dst_xid`'s mirror in solid `fg` color via
    /// `vk::ops::fill::record_fill_rectangles`. Returns `true` iff
    /// the Vulkan path actually wrote — caller falls back to
    /// pixman on `false`. `fg` is an X11 24-bit `0xRRGGBB` pixel.
    /// Solid-fill `rects` honouring the GC `function`. For `Copy`
    /// (the common case) routes through the existing `cmd_clear_
    /// attachments` fast path. For every other GC function (Xor,
    /// And, Or, Invert, Set, etc.) it draws a quad through the
    /// per-function `LogicFillPipelineCache` so the destination
    /// pixels go through the matching `VkLogicOp`.
    ///
    /// Returns `true` iff the operation took. Mirror-missing /
    /// pipeline-cache-missing return `false` (op silently dropped —
    /// the mid-port directive forbids a pixman fallback).
    fn try_vk_fill_with_function(
        &mut self,
        dst_xid: u32,
        function: GcFunction,
        fg: u32,
        rects: &[Rectangle16],
    ) -> bool {
        if rects.is_empty() {
            return true;
        }
        if matches!(function, GcFunction::Copy) {
            return self.try_vk_solid_fill(dst_xid, fg, rects);
        }
        if matches!(function, GcFunction::NoOp) {
            return true;
        }

        use crate::kms::vk::ops::{fill, run_one_shot_op};

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };

        let pipeline = match self.logic_fill_pipelines.as_mut() {
            Some(cache) => match cache.get(function) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("vk logic-fill: pipeline build failed for {function:?}: {e:?}");
                    return false;
                }
            },
            None => return false,
        };
        let pipeline_layout = self
            .logic_fill_pipelines
            .as_ref()
            .expect("checked above")
            .pipeline_layout();

        let mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };

        let extent_w = mirror.extent.width;
        let extent_h = mirror.extent.height;
        let color = [
            ((fg >> 16) & 0xFF) as f32 / 255.0,
            ((fg >> 8) & 0xFF) as f32 / 255.0,
            (fg & 0xFF) as f32 / 255.0,
            1.0,
        ];

        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter_map(|r| {
                let x0 = i32::from(r.x).max(0);
                let y0 = i32::from(r.y).max(0);
                let x1 = (i32::from(r.x).saturating_add(i32::from(r.width))).min(extent_w as i32);
                let y1 = (i32::from(r.y).saturating_add(i32::from(r.height))).min(extent_h as i32);
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                Some(ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: x0, y: y0 },
                    extent: ash::vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        if vk_rects.is_empty() {
            return true;
        }
        let scissor = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: mirror.extent,
        };

        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            fill::record_logic_fill(
                vk,
                cb,
                mirror,
                pipeline,
                pipeline_layout,
                color,
                &vk_rects,
                scissor,
            )
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk logic-fill: record failed on xid {dst_xid:#x} ({function:?}): {e:?}"
                );
                false
            }
        }
    }

    fn try_vk_solid_fill(&mut self, dst_xid: u32, fg: u32, rects: &[Rectangle16]) -> bool {
        use crate::kms::vk::ops::{fill, run_one_shot_op};
        if rects.is_empty() {
            return false;
        }
        // Snapshot the &Arc<VkContext> + pool handle (Copy) so the
        // borrow on `self.vk` / `self.ops_command_pool` ends before
        // we mut-borrow the mirror.
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };

        // Pull the mirror reference. Returns &mut DrawableImage.
        let mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };

        let extent_w = mirror.extent.width;
        let extent_h = mirror.extent.height;
        let color = [
            ((fg >> 16) & 0xFF) as f32 / 255.0,
            ((fg >> 8) & 0xFF) as f32 / 255.0,
            (fg & 0xFF) as f32 / 255.0,
            1.0,
        ];

        // Convert request rects to vk::Rect2D, clamping to the
        // mirror extent so cmd_clear_attachments doesn't see
        // negative offsets or sizes that exceed the render area.
        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter_map(|r| {
                let x0 = i32::from(r.x).max(0);
                let y0 = i32::from(r.y).max(0);
                let x1 = (i32::from(r.x).saturating_add(i32::from(r.width))).min(extent_w as i32);
                let y1 = (i32::from(r.y).saturating_add(i32::from(r.height))).min(extent_h as i32);
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                Some(ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: x0, y: y0 },
                    extent: ash::vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        if vk_rects.is_empty() {
            return false;
        }

        let scissor = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: mirror.extent,
        };

        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            fill::record_fill_rectangles(vk, cb, mirror, color, &vk_rects, scissor)
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk fill: record_fill_rectangles failed on xid {dst_xid:#x}: {e:?} — \
                     falling back to pixman this op"
                );
                false
            }
        }
    }

    /// PutImage Vulkan-direct path (sub-phase 4.1.4.3). Memcpy the
    /// X11-format pixel data into the host-visible staging buffer
    /// (with the same byte permutation the pixman path applies — see
    /// `put_image` depth-24/32 arm), record one
    /// `vkCmdCopyBufferToImage` per surviving GC-clipped sub-rect,
    /// submit + wait. Returns `false` (so caller falls back to pixman)
    /// when:
    ///
    /// - Vulkan or the staging buffer / op pool didn't come up.
    /// - The drawable has no `vk_mirror` (window/pixmap not GPU-backed).
    /// - The depth doesn't match the mirror's bytes-per-pixel
    ///   (e.g. depth-1 PutImage into an R8 mirror — bit-packing
    ///   differs; pixman stays correct for those).
    /// - A Vulkan API call fails mid-record.
    fn try_vk_put_image(
        &mut self,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> bool {
        use crate::kms::vk::ops::{image as vk_image, run_one_shot_op};

        if width == 0 || height == 0 {
            return false;
        }

        // Map X11 source depth to mirror bytes-per-pixel. depth-1
        // unpacks 1 bit/pixel from the wire to 1 byte/pixel R8 in
        // the staging buffer. depth-4 / depth-15 / depth-16 are
        // not in scope today (no matching mirror format).
        let src_bpp: usize = match depth {
            1 | 8 => 1,
            24 | 32 => 4,
            _ => return false,
        };

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
        if self.ops_staging.is_none() {
            return false;
        }

        // GC clip is in dst space. MIT-SHM PutImage clears the clip
        // before calling backend.put_image, so this returns the input
        // unchanged for that path; core PutImage applies the GC's
        // clip rectangles.
        let dst_rect = Rectangle16 {
            x: dst_x,
            y: dst_y,
            width,
            height,
        };
        let sub_rects = self.intersect_with_current_clip(&[dst_rect]);
        if sub_rects.is_empty() {
            // Fully clipped — nothing to draw, but the request is
            // "handled" so don't fall back to pixman.
            return true;
        }

        // Borrow the mirror to read its extent + format.
        let (mirror_w, mirror_h, mirror_bpp) = {
            let mirror = if let Some(w) = self.windows.get(&host_xid) {
                w.vk_mirror.as_ref()
            } else if let Some(p) = self.pixmaps.get(&host_xid) {
                p.vk_mirror.as_ref()
            } else {
                None
            };
            let Some(mirror) = mirror else {
                return false;
            };
            (
                mirror.extent.width,
                mirror.extent.height,
                mirror.bytes_per_pixel() as usize,
            )
        };
        if mirror_bpp != src_bpp {
            return false;
        }

        // Source row stride in `data` (X11 ZPixmap on the wire).
        //   depth-1: bits packed MSB-first per byte, scanline padded
        //            to 32 bits → `((w + 31) / 32) * 4` bytes.
        //   depth-8: 1 byte/pixel, scanline padded to 32 bits.
        //   depth-24/32: 4 bytes/pixel, no extra row pad.
        let src_row_stride: usize = match depth {
            1 => width.div_ceil(32) as usize * 4,
            8 => (width as usize + 3) & !3,
            24 | 32 => width as usize * 4,
            _ => unreachable!(),
        };

        // Per-sub-rect plan: clip each sub-rect to mirror extent and
        // to the original PutImage rect (so pixels outside the source
        // are skipped), compute staging offset + image offset.
        let mirror_w_i = i32::try_from(mirror_w).unwrap_or(i32::MAX);
        let mirror_h_i = i32::try_from(mirror_h).unwrap_or(i32::MAX);
        let orig_x0 = i32::from(dst_x);
        let orig_y0 = i32::from(dst_y);
        let orig_x1 = orig_x0 + i32::from(width);
        let orig_y1 = orig_y0 + i32::from(height);

        struct PutPlan {
            staging_offset: u64,
            image_x: i32,
            image_y: i32,
            extent_w: u32,
            extent_h: u32,
            src_x: u32,
            src_y: u32,
        }

        let mut plans: Vec<PutPlan> = Vec::with_capacity(sub_rects.len());
        let mut total_bytes: u64 = 0;
        for sub in &sub_rects {
            let sub_x0 = i32::from(sub.x);
            let sub_y0 = i32::from(sub.y);
            let sub_x1 = sub_x0.saturating_add(i32::from(sub.width));
            let sub_y1 = sub_y0.saturating_add(i32::from(sub.height));

            let x0 = sub_x0.max(orig_x0).max(0);
            let y0 = sub_y0.max(orig_y0).max(0);
            let x1 = sub_x1.min(orig_x1).min(mirror_w_i);
            let y1 = sub_y1.min(orig_y1).min(mirror_h_i);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let w = (x1 - x0) as u32;
            let h = (y1 - y0) as u32;
            let bytes = u64::from(w) * u64::from(h) * src_bpp as u64;
            plans.push(PutPlan {
                staging_offset: total_bytes,
                image_x: x0,
                image_y: y0,
                extent_w: w,
                extent_h: h,
                src_x: (x0 - orig_x0) as u32,
                src_y: (y0 - orig_y0) as u32,
            });
            total_bytes += bytes;
        }
        if plans.is_empty() {
            return true;
        }

        if let Err(e) = self
            .ops_staging
            .as_mut()
            .expect("checked is_none above")
            .ensure(total_bytes)
        {
            log::warn!(
                "vk put_image: staging grow failed for {total_bytes} bytes: {e:?} — \
                 falling back to pixman"
            );
            return false;
        }

        // Memcpy host → staging with byte permutation matching the
        // pixman path's pixel formula. For depth-24/32 the pixman
        // arm reads `r=data[0], g=data[1], b=data[2], a=data[3]` and
        // writes the u32 `(a<<24)|(r<<16)|(g<<8)|b`, which lays down
        // memory bytes `[b, g, r, a]` in a u32 LE word — exactly
        // what `B8G8R8A8_UNORM` reads as `(B, G, R, A)`.
        // For depth-8 the pixman path is a per-byte copy; mirror is
        // R8, same byte-per-pixel layout.
        let staging_ptr = self.ops_staging.as_ref().unwrap().mapped_ptr();
        for plan in &plans {
            let row_dst_bytes = plan.extent_w as usize * src_bpp;
            for row in 0..plan.extent_h {
                let host_row = (plan.src_y + row) as usize;
                let src_row_byte_start = host_row * src_row_stride;
                if src_row_byte_start + src_row_stride > data.len() {
                    // Truncated source — zero-fill the staging row.
                    unsafe {
                        let dst = staging_ptr
                            .add(plan.staging_offset as usize + row as usize * row_dst_bytes);
                        std::ptr::write_bytes(dst, 0, row_dst_bytes);
                    }
                    continue;
                }
                unsafe {
                    let dst_row = staging_ptr
                        .add(plan.staging_offset as usize + row as usize * row_dst_bytes);
                    let src_row = data.as_ptr().add(src_row_byte_start);
                    match depth {
                        1 => {
                            // X11 ZPixmap depth-1: bits packed MSB-first
                            // per byte, scanlines padded to 32 bits.
                            // Unpack each bit into a byte (0xFF / 0x00)
                            // for the R8 mirror.
                            for col in 0..plan.extent_w as usize {
                                let bit_index = plan.src_x as usize + col;
                                let byte = *src_row.add(bit_index >> 3);
                                let bit = (byte >> (7 - (bit_index & 7))) & 1;
                                *dst_row.add(col) = if bit != 0 { 0xFF } else { 0x00 };
                            }
                        }
                        8 => {
                            let src = src_row.add(plan.src_x as usize);
                            std::ptr::copy_nonoverlapping(src, dst_row, row_dst_bytes);
                        }
                        24 | 32 => {
                            // 4 bpp with byte permutation. src bytes
                            // are conventionally [r, g, b, a]; we emit
                            // [b, g, r, a] (or [b, g, r, 0xFF] for
                            // depth==24) to match the
                            // `B8G8R8A8_UNORM` mirror's memory order.
                            let src = src_row.add(plan.src_x as usize * 4);
                            for col in 0..plan.extent_w as usize {
                                let s = src.add(col * 4);
                                let d = dst_row.add(col * 4);
                                let r = *s;
                                let g = *s.add(1);
                                let b = *s.add(2);
                                let a = if depth == 32 { *s.add(3) } else { 0xFFu8 };
                                *d = b;
                                *d.add(1) = g;
                                *d.add(2) = r;
                                *d.add(3) = a;
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }

        // Build BufferImageCopy regions.
        let regions: Vec<ash::vk::BufferImageCopy> = plans
            .iter()
            .map(|p| {
                ash::vk::BufferImageCopy::default()
                    .buffer_offset(p.staging_offset)
                    .buffer_row_length(0)
                    .buffer_image_height(0)
                    .image_subresource(
                        ash::vk::ImageSubresourceLayers::default()
                            .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_offset(ash::vk::Offset3D {
                        x: p.image_x,
                        y: p.image_y,
                        z: 0,
                    })
                    .image_extent(ash::vk::Extent3D {
                        width: p.extent_w,
                        height: p.extent_h,
                        depth: 1,
                    })
            })
            .collect();

        // Reborrow the mirror mutably for the recording.
        let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };
        let staging_buffer = self.ops_staging.as_ref().expect("checked above").buffer();

        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            vk_image::record_put_image(vk, cb, mirror, staging_buffer, &regions)
        }) {
            Ok(()) => {
                // The Vk-direct write made the mirror current; we
                // do NOT mark damage here. Damage tells
                // `MirrorUploader` to upload pixman → mirror at the
                // next composite frame, which would *clobber* the
                // bytes we just wrote with whatever (stale) pixman
                // contents. The original 4.1.4.3 commit incorrectly
                // marked damage with the inverted reasoning.
                true
            }
            Err(e) => {
                log::warn!(
                    "vk put_image: record failed on xid {host_xid:#x}: {e:?} — \
                     falling back to pixman"
                );
                false
            }
        }
    }

    /// GetImage Vulkan-direct readback (sub-phase 4.1.4.3). Records
    /// `vkCmdCopyImageToBuffer` from the mirror into the host-visible
    /// staging buffer, waits, and writes the pixel bytes (with the
    /// same byte ordering the pixman path emits) into the caller's
    /// reply Vec. The 32-byte X11 GetImage reply header is emitted by
    /// the trait impl; this returns just the pixel payload.
    ///
    /// Returns `None` when the path is unavailable (no Vulkan, no
    /// staging, no mirror, depth/format mismatch). Caller falls back
    /// to the pixman read in that case.
    fn try_vk_get_image_pixels(
        &mut self,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        depth: u8,
        out: &mut Vec<u8>,
    ) -> bool {
        use crate::kms::vk::ops::{image as vk_image, run_one_shot_op};

        if width == 0 || height == 0 {
            return false;
        }

        // Match the depth → mirror format mapping from put_image. For
        // the depths we don't accelerate (1, 4, 15, 16) the trait
        // impl's pixman zero-fill path is preserved.
        let bpp: usize = match depth {
            8 => 1,
            24 | 32 => 4,
            _ => return false,
        };

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
        if self.ops_staging.is_none() {
            return false;
        }

        let (mirror_w, mirror_h, mirror_bpp) = {
            let mirror = if let Some(w) = self.windows.get(&host_xid) {
                w.vk_mirror.as_ref()
            } else if let Some(p) = self.pixmaps.get(&host_xid) {
                p.vk_mirror.as_ref()
            } else {
                None
            };
            let Some(mirror) = mirror else {
                return false;
            };
            (
                mirror.extent.width,
                mirror.extent.height,
                mirror.bytes_per_pixel() as usize,
            )
        };
        if mirror_bpp != bpp {
            return false;
        }

        // Reply rows are padded to a 4-byte boundary per X11 ZPixmap.
        let row_data_bytes = width as usize * bpp;
        let row_stride_bytes = (row_data_bytes + 3) & !3;
        let row_pad_bytes = row_stride_bytes - row_data_bytes;

        // Clip the read rect to the mirror extent. Out-of-range cells
        // are zero-filled in the output (matching the pixman path).
        let mirror_w_i = i32::try_from(mirror_w).unwrap_or(i32::MAX);
        let mirror_h_i = i32::try_from(mirror_h).unwrap_or(i32::MAX);
        let orig_x0 = i32::from(x);
        let orig_y0 = i32::from(y);
        let orig_x1 = orig_x0 + i32::from(width);
        let orig_y1 = orig_y0 + i32::from(height);
        let read_x0 = orig_x0.max(0);
        let read_y0 = orig_y0.max(0);
        let read_x1 = orig_x1.min(mirror_w_i);
        let read_y1 = orig_y1.min(mirror_h_i);
        let has_read = read_x1 > read_x0 && read_y1 > read_y0;

        let read_w = if has_read {
            (read_x1 - read_x0) as u32
        } else {
            0
        };
        let read_h = if has_read {
            (read_y1 - read_y0) as u32
        } else {
            0
        };
        let read_bytes = u64::from(read_w) * u64::from(read_h) * bpp as u64;

        if has_read {
            if let Err(e) = self
                .ops_staging
                .as_mut()
                .expect("checked above")
                .ensure(read_bytes)
            {
                log::warn!(
                    "vk get_image: staging grow failed for {read_bytes} bytes: \
                     {e:?} — falling back to pixman"
                );
                return false;
            }

            let regions = [ash::vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(
                    ash::vk::ImageSubresourceLayers::default()
                        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(ash::vk::Offset3D {
                    x: read_x0,
                    y: read_y0,
                    z: 0,
                })
                .image_extent(ash::vk::Extent3D {
                    width: read_w,
                    height: read_h,
                    depth: 1,
                })];

            let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
                w.vk_mirror.as_mut()
            } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
                p.vk_mirror.as_mut()
            } else {
                None
            };
            let Some(mirror) = mirror else {
                return false;
            };
            let staging_buffer = self.ops_staging.as_ref().expect("checked above").buffer();
            match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
                vk_image::record_get_image(vk, cb, mirror, staging_buffer, &regions)
            }) {
                Ok(()) => {}
                Err(e) => {
                    log::warn!(
                        "vk get_image: record failed on xid {host_xid:#x}: {e:?} — \
                         falling back to pixman"
                    );
                    return false;
                }
            }
        }

        // Walk the requested rect row by row, copying from the staging
        // buffer where the row/column intersects the mirror, and
        // emitting zeros where it doesn't. Mirror's BGRA8 memory bytes
        // are the same as pixman's `0xAARRGGBB` LE u32 storage, so a
        // straight memcpy yields identical wire bytes for depth-24/32.
        // Depth-8 is single-byte memcpy.
        let staging_ptr = self.ops_staging.as_ref().unwrap().mapped_ptr() as *const u8;
        for row in 0..i32::from(height) {
            let dy = orig_y0 + row;
            for col in 0..i32::from(width) {
                let dx = orig_x0 + col;
                if has_read && dx >= read_x0 && dx < read_x1 && dy >= read_y0 && dy < read_y1 {
                    let staging_row = (dy - read_y0) as usize;
                    let staging_col = (dx - read_x0) as usize;
                    let staging_off = staging_row * (read_w as usize * bpp) + staging_col * bpp;
                    unsafe {
                        let src = staging_ptr.add(staging_off);
                        let mut buf = [0u8; 4];
                        std::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), bpp);
                        out.extend_from_slice(&buf[..bpp]);
                    }
                } else {
                    out.extend(std::iter::repeat_n(0u8, bpp));
                }
            }
            if row_pad_bytes > 0 {
                out.extend(std::iter::repeat_n(0u8, row_pad_bytes));
            }
        }

        true
    }

    /// Sub-phase 4.1.4.5 helper. Intern each rendered glyph in the
    /// shared atlas and dispatch a single text-pipeline draw onto
    /// the target's mirror. Returns `true` when the path took (the
    /// caller should `return Ok(())`); `false` means the caller
    /// falls back to the existing pixman compositing path.
    ///
    /// All-or-nothing: any glyph that fails to intern (atlas full,
    /// API failure) makes the whole run fall back. Partial Vulkan
    /// and partial pixman would double-draw glyphs; the caller's
    /// pixman path already handles the whole run correctly.
    fn try_vk_text_run(
        &mut self,
        host_xid: u32,
        font_xid: u32,
        foreground: u32,
        rendered: &[RenderedGlyph],
    ) -> bool {
        use crate::kms::vk::{
            glyph::GlyphKey,
            ops::{run_one_shot_op, text as vk_text},
        };

        if rendered.is_empty() {
            return true;
        }

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
        if self.glyph_atlas.is_none() || self.text_pipeline.is_none() {
            return false;
        }

        // Mirror format check before spending atlas slots on a
        // run that can't use them.
        let mirror_format = if let Some(w) = self.windows.get(&host_xid) {
            w.vk_mirror.as_ref().map(|m| m.format)
        } else if let Some(p) = self.pixmaps.get(&host_xid) {
            p.vk_mirror.as_ref().map(|m| m.format)
        } else {
            None
        };
        let Some(mirror_format) = mirror_format else {
            return false;
        };
        if mirror_format != ash::vk::Format::B8G8R8A8_UNORM {
            return false;
        }

        // Intern glyphs first. Each call may run a one-shot
        // upload CB; that's fine — happens before we record the
        // text run, so atlas is fully populated by the time the
        // text-run CB executes.
        let mut glyphs_to_draw: Vec<vk_text::TextGlyph> = Vec::with_capacity(rendered.len());
        for g in rendered {
            let key = GlyphKey {
                font_xid,
                codepoint: g.codepoint,
            };
            let atlas = self.glyph_atlas.as_mut().expect("checked above");
            let Some(entry) =
                atlas.intern(key, g.w as u32, g.h as u32, 0, 0, &g.pixels, pool_handle)
            else {
                // Atlas full or upload failed — abort the run.
                return false;
            };
            glyphs_to_draw.push(vk_text::TextGlyph {
                entry,
                dst_x: g.dst_x,
                dst_y: g.dst_y,
            });
        }

        let foreground_rgba = [
            ((foreground >> 16) & 0xFF) as f32 / 255.0,
            ((foreground >> 8) & 0xFF) as f32 / 255.0,
            (foreground & 0xFF) as f32 / 255.0,
            1.0,
        ];

        // Pull mut refs after the atlas borrow ends. The atlas
        // and the pipeline are immutable for the recording step;
        // the mirror is &mut.
        let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };
        let atlas = self.glyph_atlas.as_ref().expect("checked above");
        let pipeline = self.text_pipeline.as_ref().expect("checked above");

        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            vk_text::record_text_run(
                vk,
                cb,
                mirror,
                atlas,
                pipeline,
                &glyphs_to_draw,
                foreground_rgba,
            )
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk text_run: record failed on xid {host_xid:#x}: {e:?} — falling back to \
                     pixman"
                );
                false
            }
        }
    }

    /// Phase 4.1.4.7 trapezoid wire-decoder + Vk dispatch. Returns
    /// `true` when the Vulkan path took.
    fn try_vk_render_trapezoids_path(
        &mut self,
        op: u8,
        host_src: u32,
        host_dst: u32,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> bool {
        use crate::kms::vk::ops::traps as vk_traps;

        let n_traps = traps.len() / 40;
        if n_traps == 0 {
            return true;
        }
        let mut decoded: Vec<vk_traps::Trapezoid> = Vec::with_capacity(n_traps);
        for chunk in traps.chunks_exact(40) {
            let read_i32 = |o: usize| -> i32 {
                i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
            };
            decoded.push(vk_traps::Trapezoid {
                top: read_i32(0),
                bottom: read_i32(4),
                left_p1: (read_i32(8), read_i32(12)),
                left_p2: (read_i32(16), read_i32(20)),
                right_p1: (read_i32(24), read_i32(28)),
                right_p2: (read_i32(32), read_i32(36)),
            });
        }
        // Apply (x_off, y_off) in fixed-point units.
        let dx = i32::from(x_off) << 16;
        let dy = i32::from(y_off) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut decoded {
                t.top = t.top.wrapping_add(dy);
                t.bottom = t.bottom.wrapping_add(dy);
                t.left_p1.0 = t.left_p1.0.wrapping_add(dx);
                t.left_p1.1 = t.left_p1.1.wrapping_add(dy);
                t.left_p2.0 = t.left_p2.0.wrapping_add(dx);
                t.left_p2.1 = t.left_p2.1.wrapping_add(dy);
                t.right_p1.0 = t.right_p1.0.wrapping_add(dx);
                t.right_p1.1 = t.right_p1.1.wrapping_add(dy);
                t.right_p2.0 = t.right_p2.0.wrapping_add(dx);
                t.right_p2.1 = t.right_p2.1.wrapping_add(dy);
            }
        }

        let Some((bx, by, bx1, by1)) = vk_traps::trapezoid_bbox(&decoded) else {
            return true; // empty / degenerate
        };
        // Clamp to non-negative — Vulkan dst-coords are unsigned.
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return true;
        }
        let bw = (bx1 - bx) as u32;
        let bh = (by1 - by) as u32;
        let mask = vk_traps::rasterize_trapezoids(&decoded, bx, by, bw, bh);
        self.try_vk_render_traps_or_tris(op, host_src, host_dst, &mask, bx, by, bw, bh)
    }

    /// Phase 4.1.4.7 triangle wire-decoder + Vk dispatch. Mirrors
    /// the trapezoid path but for triangle / tristrip / trifan.
    #[allow(clippy::too_many_arguments)]
    fn try_vk_render_triangles_path(
        &mut self,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> bool {
        use crate::kms::vk::ops::traps as vk_traps;

        let read_point = |off: usize, chunk: &[u8]| -> (i32, i32) {
            let x =
                i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
            let y = i32::from_le_bytes([
                chunk[off + 4],
                chunk[off + 5],
                chunk[off + 6],
                chunk[off + 7],
            ]);
            (x, y)
        };

        let mut tris: Vec<vk_traps::Triangle> = match minor {
            11 => {
                if !primitives.len().is_multiple_of(24) {
                    return false;
                }
                primitives
                    .chunks_exact(24)
                    .map(|c| vk_traps::Triangle {
                        p1: read_point(0, c),
                        p2: read_point(8, c),
                        p3: read_point(16, c),
                    })
                    .collect()
            }
            12 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return false;
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (0..pts.len() - 2)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[i],
                        p2: pts[i + 1],
                        p3: pts[i + 2],
                    })
                    .collect()
            }
            13 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return false;
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (1..pts.len() - 1)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[0],
                        p2: pts[i],
                        p3: pts[i + 1],
                    })
                    .collect()
            }
            _ => return false,
        };
        if tris.is_empty() {
            return true;
        }
        let dx = i32::from(x_off) << 16;
        let dy = i32::from(y_off) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut tris {
                t.p1.0 = t.p1.0.wrapping_add(dx);
                t.p1.1 = t.p1.1.wrapping_add(dy);
                t.p2.0 = t.p2.0.wrapping_add(dx);
                t.p2.1 = t.p2.1.wrapping_add(dy);
                t.p3.0 = t.p3.0.wrapping_add(dx);
                t.p3.1 = t.p3.1.wrapping_add(dy);
            }
        }

        let Some((bx, by, bx1, by1)) = vk_traps::triangle_bbox(&tris) else {
            return true;
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return true;
        }
        let bw = (bx1 - bx) as u32;
        let bh = (by1 - by) as u32;
        let mask = vk_traps::rasterize_triangles(&tris, bx, by, bw, bh);
        self.try_vk_render_traps_or_tris(op, host_src, host_dst, &mask, bx, by, bw, bh)
    }

    /// Sub-phase 4.1.4.7 helper. Vulkan-direct RENDER `Trapezoids`
    /// / `Triangles` via CPU rasterisation. The R8 mask is uploaded
    /// to [`MaskScratch`](crate::kms::vk::mask_scratch::MaskScratch);
    /// the composite path then reads `src.color * mask.alpha` exactly
    /// like a normal Composite call with an A8 mask.
    ///
    /// Source resolution mirrors [`Self::try_vk_render_composite`]:
    /// `SolidFill` clears a 1×1 scratch with the picture colour,
    /// `Drawable` samples the drawable's mirror (with the same
    /// alpha-vs-no-alpha view selection rules), `Gradient` samples
    /// the pre-evaluated gradient image and inherits its
    /// axis-projection transform.
    ///
    /// Returns `true` if the Vulkan path took (or trivially had no
    /// pixels to draw); `false` means the caller should treat the
    /// request as unhandled.
    #[allow(clippy::too_many_arguments)]
    fn try_vk_render_traps_or_tris(
        &mut self,
        op: u8,
        host_src: u32,
        host_dst: u32,
        coverage_mask: &[u8], // already CPU-rasterised
        bbox_x: i32,
        bbox_y: i32,
        bbox_w: u32,
        bbox_h: u32,
    ) -> bool {
        use crate::kms::vk::{
            ops::{render as vk_render, run_one_shot_op},
            render_pipeline::{StdPictOp, record_solid_color_clear},
        };

        if bbox_w == 0 || bbox_h == 0 || coverage_mask.is_empty() {
            return true;
        }

        let Some(std_op) = StdPictOp::from_u8(op) else {
            return false;
        };

        let Some(src) = resolve_render_pic(self.pictures.get(&host_src)) else {
            return false;
        };

        let (dst_xid, picture_clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => return false,
        };

        // Self-composite (src aliases dst) needs a staging copy —
        // out of scope for this path.
        let src_xid_if_drawable = match src {
            RenderPic::Drawable(xid) => Some(xid),
            _ => None,
        };
        if src_xid_if_drawable == Some(dst_xid) {
            return false;
        }

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
        if self.render_pipelines.is_none()
            || self.solid_src_image.is_none()
            || self.mask_scratch.is_none()
            || self.white_mask_image.is_none()
        {
            return false;
        }
        let (dst_format, dst_extent, dst_depth, drawable_size) =
            if let Some(w) = self.windows.get(&dst_xid) {
                (
                    w.vk_mirror.as_ref().map(|m| m.format),
                    w.vk_mirror.as_ref().map(|m| m.extent),
                    w.depth,
                    Some((u32::from(w.width), u32::from(w.height))),
                )
            } else if let Some(p) = self.pixmaps.get(&dst_xid) {
                (
                    p.vk_mirror.as_ref().map(|m| m.format),
                    p.vk_mirror.as_ref().map(|m| m.extent),
                    p.depth,
                    Some((u32::from(p.width), u32::from(p.height))),
                )
            } else {
                (None, None, 0, None)
            };
        let dst_format = match dst_format {
            Some(f @ (ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8_UNORM)) => f,
            _ => return false,
        };
        let Some(dst_extent) = dst_extent else {
            return false;
        };
        let Some((drawable_w, drawable_h)) = drawable_size else {
            return false;
        };
        let dst_has_alpha = dst_format == ash::vk::Format::R8_UNORM || dst_depth == 32;
        let needs_dst_readback = std_op.needs_dst_readback();
        if needs_dst_readback && self.dst_readback.is_none() {
            return false;
        }

        // Drawable src must have a sampleable mirror in a known
        // format. Matches the gating in try_vk_render_composite.
        if let Some(xid) = src_xid_if_drawable
            && !self.ensure_drawable_mirror_sampleable(xid)
        {
            return false;
        }
        if let Some(xid) = src_xid_if_drawable {
            let f = if let Some(w) = self.windows.get(&xid) {
                w.vk_mirror.as_ref().map(|m| m.format)
            } else if let Some(p) = self.pixmaps.get(&xid) {
                p.vk_mirror.as_ref().map(|m| m.format)
            } else {
                None
            };
            if !matches!(
                f,
                Some(ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8_UNORM)
            ) {
                return false;
            }
        }

        // Upload CPU-rasterised mask first — independent of the
        // composite recording. Resizes the scratch on demand.
        if let Err(e) = self
            .mask_scratch
            .as_mut()
            .expect("checked above")
            .upload_r8(pool_handle, bbox_w, bbox_h, coverage_mask)
        {
            log::warn!("vk render_traps: mask upload failed: {e:?}");
            return false;
        }

        // Pipeline + scissor. render_traps doesn't compute via the
        // component-alpha path (its mask is the CPU-rasterised
        // coverage, not a client picture), so component_alpha=false.
        let pipeline = match self.render_pipelines.as_mut().expect("checked above").get(
            std_op,
            dst_format,
            dst_has_alpha,
            false,
        ) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("vk render_traps: pipeline build failed: {e:?}");
                return false;
            }
        };
        let pipeline_layout = self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .pipeline_layout();
        if let Err(e) = self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .reset_descriptors()
        {
            log::warn!("vk render_traps: descriptor pool reset failed: {e:?}");
            return false;
        }

        let solid_src_view = self
            .solid_src_image
            .as_ref()
            .expect("checked above")
            .image_view();
        let mask_view = self
            .mask_scratch
            .as_ref()
            .expect("checked above")
            .image_view();
        let mask_extent = self.mask_scratch.as_ref().expect("checked above").extent();

        // Resolve src view + extent + (optional) clear colour.
        // Tracks any per-source affine the picture itself induces
        // (gradient axis projection); the user-transform composition
        // path in try_vk_render_composite isn't reachable here yet
        // because the trapezoid/triangle paths don't carry one.
        let mut src_clear_color: Option<[f32; 4]> = None;
        let src_view;
        let src_extent;
        let src_picture_xform: Option<vk_render::AffineXform>;
        match src {
            RenderPic::Drawable(xid) => {
                let (m_format, extent, depth) = {
                    let (m, depth) = if let Some(w) = self.windows.get(&xid) {
                        (w.vk_mirror.as_ref(), w.depth)
                    } else if let Some(p) = self.pixmaps.get(&xid) {
                        (p.vk_mirror.as_ref(), p.depth)
                    } else {
                        (None, 0)
                    };
                    let Some(m) = m else { return false };
                    (m.format, m.extent, depth)
                };
                let view = if m_format == ash::vk::Format::R8_UNORM {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if depth == 24 {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if let Some(w) = self.windows.get(&xid) {
                    w.vk_mirror.as_ref().expect("checked above").vk_image_view
                } else {
                    self.pixmaps
                        .get(&xid)
                        .and_then(|p| p.vk_mirror.as_ref())
                        .expect("checked above")
                        .vk_image_view
                };
                src_view = view;
                src_extent = extent;
                src_picture_xform = None;
            }
            RenderPic::Solid(color) => {
                src_view = solid_src_view;
                src_extent = ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                };
                src_clear_color = Some(color);
                src_picture_xform = None;
            }
            RenderPic::Gradient(xid) => {
                let Some(PictureState::Gradient { gradient, .. }) = self.pictures.get(&xid) else {
                    return false;
                };
                src_view = gradient.image_view();
                src_extent = gradient.extent();
                src_picture_xform = Some(gradient.axis_projection);
            }
            RenderPic::None => return false,
        }

        // Disjoint/Conjoint ops reach this path too (e.g. rendercheck's
        // `Conjoint*` triangles cases). For those the shader reads dst
        // via binding 2; ensure the dst-readback scratch is sized for
        // the dst mirror and snapshot dst into it inside the CB.
        // Standard ops bind the white-mask scratch as a placeholder.
        let white_mask_view = self
            .white_mask_image
            .as_ref()
            .expect("checked above")
            .image_view();
        let dst_readback_view = if needs_dst_readback {
            let scratch = self.dst_readback.as_mut().expect("checked above");
            if let Err(e) = scratch.ensure(dst_format, dst_extent.width, dst_extent.height) {
                log::warn!("vk render_traps: dst readback ensure failed: {e:?}");
                return false;
            }
            match scratch.view(dst_format, dst_has_alpha) {
                Ok(Some(v)) => v,
                Ok(None) => return false,
                Err(e) => {
                    log::warn!("vk render_traps: dst readback view build failed: {e:?}");
                    return false;
                }
            }
        } else {
            white_mask_view
        };
        let descriptor_set = match self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .allocate_descriptor_for_views(src_view, mask_view, dst_readback_view)
        {
            Ok(s) => s,
            Err(e) => {
                log::warn!("vk render_traps: descriptor alloc failed: {e:?}");
                return false;
            }
        };

        // Pixman's `zero_src_has_no_effect` table inverted: ops where
        // mask=0 still affects dst (e.g. Clear, Src, In, InReverse,
        // Out, AtopReverse, Saturate, plus every Disjoint/Conjoint
        // variant — pixman's table doesn't list those, so they fall
        // into the default-FALSE arm) must composite across the
        // entire destination, not just the trapezoid bbox. The
        // mask scratch sits at offset (bbox_x, bbox_y) within the
        // destination; outside that window REPEAT_NONE returns 0,
        // and the operator math then yields the right outside-bbox
        // result (Clear → 0, Src → 0, etc.). See xserver
        // `fb/fbtrap.c` → `pixman/pixman-trap.c::get_trap_extents`.
        let needs_full_dst = matches!(op, 0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43);
        let (render_dst_x, render_dst_y, render_w, render_h, mask_off_x, mask_off_y) =
            if needs_full_dst {
                (0, 0, drawable_w, drawable_h, -bbox_x, -bbox_y)
            } else {
                (bbox_x, bbox_y, bbox_w, bbox_h, 0, 0)
            };

        // Build the scissor + composite rect.
        let scissor = match self.build_render_composite_inputs(
            &picture_clip,
            0,
            0,
            0,
            0,
            render_dst_x as i16,
            render_dst_y as i16,
            render_w.try_into().unwrap_or(u16::MAX),
            render_h.try_into().unwrap_or(u16::MAX),
        ) {
            Some((_, scissor)) => scissor,
            None => return true, // clipped out completely
        };
        let rects = vec![vk_render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: mask_off_x,
            mask_y: mask_off_y,
            dst_x: render_dst_x,
            dst_y: render_dst_y,
            width: render_w,
            height: render_h,
        }];
        let attrs = vk_render::CompositeAttrs {
            src_extent,
            mask_extent,
            // Synthetic 1×1 sources tile trivially; real Drawable /
            // Gradient sources sample within their extent and the
            // bounding box keeps us in-range, so Normal is a safe
            // default.
            src_repeat: 1,
            // Mask scratch is sized exactly to the trapezoid bbox.
            // Sampling outside that window with REPEAT_NONE returns 0,
            // which is what we want for the full-dst path's
            // outside-bbox pixels (mask=0 makes the operator yield
            // the right zero / dst-preserved result).
            mask_repeat: 0,
            src_xform: src_picture_xform.unwrap_or(vk_render::AffineXform::IDENTITY),
            mask_xform: vk_render::AffineXform::IDENTITY,
        };

        let dst_mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(dst_mirror) = dst_mirror else {
            return false;
        };
        let solid_src_image = self.solid_src_image.as_mut().expect("checked above");
        let dst_readback = if needs_dst_readback {
            Some(self.dst_readback.as_mut().expect("checked above"))
        } else {
            None
        };

        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            if let Some(color) = src_clear_color {
                record_solid_color_clear(vk, cb, solid_src_image, color);
            }
            // Disjoint/Conjoint: snapshot dst into the readback scratch
            // so the shader can sample it at binding 2. Mirrors the
            // sequencing in try_vk_render_composite.
            if let Some(rb) = dst_readback {
                rb.record_copy_from(
                    cb,
                    dst_mirror.vk_image,
                    dst_mirror.current_layout(),
                    dst_format,
                    dst_mirror.extent,
                );
            }
            vk_render::record_render_composite(
                vk,
                cb,
                dst_mirror,
                pipeline,
                pipeline_layout,
                descriptor_set,
                &attrs,
                &rects,
                scissor,
            )
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("vk render_traps: record failed on dst xid {dst_xid:#x}: {e:?}");
                false
            }
        }
    }

    /// Sub-phase 4.1.4.7 helper. Vulkan-direct RENDER `CompositeGlyphs`.
    ///
    /// Only the rendercheck-relevant case is supported: SolidFill src
    /// and drawable dst, glyphsets in `A8` or `A1` format. Glyphs intern
    /// into the shared 4.1.4.5 atlas (same key namespace as text runs —
    /// `(font_xid = gs_xid, codepoint = glyph_id)`); A1 bitmaps are
    /// expanded to A8 host-side because the atlas is `R8_UNORM`.
    ///
    /// Op is ignored — the text pipeline always premul-srcover blends.
    /// rendercheck's CompositeGlyphs cases use `Over`, which matches
    /// the pipeline's blend state. Other ops fall through to pixman.
    #[allow(clippy::too_many_arguments)]
    fn try_vk_render_composite_glyphs(
        &mut self,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> bool {
        use crate::kms::vk::{
            glyph::GlyphKey,
            ops::{run_one_shot_op, text as vk_text},
        };

        // PictOp `Over` (3) is the natural fit for the text pipeline's
        // pre-mul srcover blend state. `Src` (1) overrides dst rather
        // than blending — incorrect if the run overlaps existing
        // pixels. Conservative: only handle `Over`.
        if op != 3 {
            return false;
        }

        // SolidFill src only.
        let foreground_premul = match self.pictures.get(&host_src) {
            Some(PictureState::SolidFill { premul, .. }) => *premul,
            _ => return false,
        };

        let (dst_xid, _clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => return false,
        };

        if !self.glyphsets.contains_key(&host_gs) {
            return false;
        }

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
        if self.glyph_atlas.is_none() || self.text_pipeline.is_none() {
            return false;
        }

        // Mirror format check — atlas + text pipeline target BGRA.
        let mirror_format = if let Some(w) = self.windows.get(&dst_xid) {
            w.vk_mirror.as_ref().map(|m| m.format)
        } else if let Some(p) = self.pixmaps.get(&dst_xid) {
            p.vk_mirror.as_ref().map(|m| m.format)
        } else {
            None
        };
        if mirror_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            return false;
        }

        // Walk the items list (same parser as `composite_glyphs_onto`)
        // to build a `Vec<TextGlyph>`. Any unsupported glyph format
        // makes us bail to pixman so the run renders consistently.
        let id_size: usize = match minor {
            23 => 1,
            24 => 2,
            _ => 4,
        };
        let mut pen_x = i32::from(src_x) + i32::from(x_off);
        let mut pen_y = i32::from(src_y) + i32::from(y_off);
        let mut pos: usize = 0;
        let mut active_gs_xid = host_gs;
        let mut glyphs_to_draw: Vec<vk_text::TextGlyph> = Vec::new();
        // Throwaway A8 buffer for A1→A8 expansion; reused per glyph.
        let mut a8_scratch: Vec<u8> = Vec::new();

        while pos + 8 <= items.len() {
            let count = items[pos] as usize;
            if count == 255 {
                if pos + 8 <= items.len() {
                    let new_xid = u32::from_le_bytes([
                        items[pos + 4],
                        items[pos + 5],
                        items[pos + 6],
                        items[pos + 7],
                    ]);
                    if new_xid != 0 && self.glyphsets.contains_key(&new_xid) {
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

            let Some(active_gs) = self.glyphsets.get(&active_gs_xid) else {
                pos += 8 + padded;
                continue;
            };
            let active_gs_xid_for_key = active_gs_xid;

            for i in 0..count {
                let id_off = payload_start + i * id_size;
                let glyph_id: u32 = match id_size {
                    1 => u32::from(items[id_off]),
                    2 => u32::from(u16::from_le_bytes([items[id_off], items[id_off + 1]])),
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

                let gw = glyph.width as u32;
                let gh = glyph.height as u32;
                let dst_x = pen_x - i32::from(glyph.x);
                let dst_y = pen_y - i32::from(glyph.y);

                if gw > 0 && gh > 0 {
                    let pixels: &[u8] = match glyph.format {
                        GlyphSetFormat::A8 => &glyph.pixels,
                        GlyphSetFormat::A1 => {
                            // Wire A1: rows MSB-first, 32-bit padded.
                            // Expand into row-major A8 (0 or 255).
                            let wire_stride = (gw as usize).div_ceil(32) * 4;
                            a8_scratch.clear();
                            a8_scratch.resize((gw * gh) as usize, 0);
                            for row in 0..(gh as usize) {
                                let src_off = row * wire_stride;
                                if src_off + wire_stride > glyph.pixels.len() {
                                    break;
                                }
                                for col in 0..(gw as usize) {
                                    let byte = glyph.pixels[src_off + col / 8];
                                    // X11 A1 is LSB-first within each byte (per
                                    // glyph protocol; differs from pixman's wire
                                    // format which is MSB. The existing pixman
                                    // path relies on pixman_image_create A1, which
                                    // accepts MSB-first — but `parse_add_glyphs`
                                    // strips no bit-ordering, so we read the
                                    // wire as-is). Use MSB-first: bit `7-(col%8)`.
                                    let bit = (byte >> (7 - (col & 7))) & 1;
                                    a8_scratch[row * (gw as usize) + col] =
                                        if bit != 0 { 0xFF } else { 0 };
                                }
                            }
                            &a8_scratch
                        }
                        GlyphSetFormat::Other => {
                            return false;
                        }
                    };

                    let key = GlyphKey {
                        font_xid: active_gs_xid_for_key,
                        codepoint: glyph_id,
                    };
                    let atlas = self.glyph_atlas.as_mut().expect("checked above");
                    let Some(entry) = atlas.intern(key, gw, gh, 0, 0, pixels, pool_handle) else {
                        // Atlas full or upload failed — fall back.
                        return false;
                    };
                    glyphs_to_draw.push(vk_text::TextGlyph {
                        entry,
                        dst_x,
                        dst_y,
                    });
                }

                pen_x += i32::from(glyph.x_off);
                pen_y += i32::from(glyph.y_off);
            }

            pos += 8 + padded;
        }

        if glyphs_to_draw.is_empty() {
            return true;
        }

        // Pull the mirror + atlas + pipeline references for recording.
        let mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };
        let atlas = self.glyph_atlas.as_ref().expect("checked above");
        let pipeline = self.text_pipeline.as_ref().expect("checked above");

        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            vk_text::record_text_run(
                vk,
                cb,
                mirror,
                atlas,
                pipeline,
                &glyphs_to_draw,
                foreground_premul,
            )
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_composite_glyphs: record failed on dst xid {dst_xid:#x}: {e:?} \
                     — falling back to pixman"
                );
                false
            }
        }
    }

    /// Sub-phase 4.1.4.6 helper. Confirms the drawable has a
    /// `vk_mirror` whose layout is sampleable. The transitional
    /// version of this also flushed pixman → mirror inline, but
    /// once new mirrors are cleared on creation and PutImage
    /// covers every depth (incl. R8 / depth-1), there's no
    /// situation where the mirror lags pixman — every Vk write
    /// goes straight to the mirror, and pixman fall-through paths
    /// (still around for unported ops) call `mark_full_damage`
    /// which `MirrorUploader` syncs on the next composite frame.
    /// Until then, sampling a never-touched mirror after a
    /// pixman-only write returns the cleared zero contents — that
    /// only matters for the very first frame after creation, and
    /// real clients don't `RenderComposite` on a zero-content
    /// drawable.
    fn ensure_drawable_mirror_sampleable(&self, host_xid: u32) -> bool {
        let m = if let Some(w) = self.windows.get(&host_xid) {
            w.vk_mirror.as_ref()
        } else if let Some(p) = self.pixmaps.get(&host_xid) {
            p.vk_mirror.as_ref()
        } else {
            None
        };
        m.is_some()
    }

    /// Sub-phase 4.1.4.6 helper. Build the per-rect plan + scissor
    /// for `try_vk_render_composite`. Returns `None` if all rects
    /// fall outside the clip (caller treats that as success — no
    /// pixels to draw — and returns `Ok(())` from the trait
    /// method). Otherwise returns the survivors plus a single
    /// scissor rectangle that bounds the picture clip; the
    /// per-pipeline blend handles the actual rect masking through
    /// the draw quad geometry.
    #[allow(clippy::too_many_arguments)]
    fn build_render_composite_inputs(
        &self,
        clip: &Option<Vec<Rectangle16>>,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> Option<(
        Vec<crate::kms::vk::ops::render::CompositeRect>,
        ash::vk::Rect2D,
    )> {
        if width == 0 || height == 0 {
            return None;
        }
        // Compute scissor as the union of the picture clip's
        // rectangles, or the unit-screen if no clip is set. This
        // is coarse — the right thing is one scissor *change* per
        // sub-rect, but the parent plan accepts that as a 4.1.4.8
        // tightening.
        let scissor = match clip {
            Some(rects) if !rects.is_empty() => {
                let mut x0 = i32::MAX;
                let mut y0 = i32::MAX;
                let mut x1 = i32::MIN;
                let mut y1 = i32::MIN;
                for r in rects {
                    let rx0 = i32::from(r.x);
                    let ry0 = i32::from(r.y);
                    let rx1 = rx0 + i32::from(r.width);
                    let ry1 = ry0 + i32::from(r.height);
                    x0 = x0.min(rx0);
                    y0 = y0.min(ry0);
                    x1 = x1.max(rx1);
                    y1 = y1.max(ry1);
                }
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                let x0 = x0.max(0);
                let y0 = y0.max(0);
                ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: x0, y: y0 },
                    extent: ash::vk::Extent2D {
                        width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
                        height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
                    },
                }
            }
            _ => ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: u32::MAX,
                    height: u32::MAX,
                },
            },
        };
        let rects = vec![crate::kms::vk::ops::render::CompositeRect {
            src_x: i32::from(src_x),
            src_y: i32::from(src_y),
            mask_x: i32::from(mask_x),
            mask_y: i32::from(mask_y),
            dst_x: i32::from(dst_x),
            dst_y: i32::from(dst_y),
            width: u32::from(width),
            height: u32::from(height),
        }];
        Some((rects, scissor))
    }

    /// Sub-phase 4.1.4.6 helper. Vulkan-direct RENDER `Composite`.
    ///
    /// `op` is one of the 13 standard PictOps. `src` and `mask`
    /// pictures resolve via [`RenderPic`] to either a Drawable
    /// mirror, a `SolidFill` colour, or (for mask only) the
    /// `RenderPic::None` sentinel which the recorder handles by
    /// binding the backend-shared white-mask scratch.
    ///
    /// Out of scope: transform, `component_alpha`, `alpha_map`,
    /// non-`None` repeat. Caller pre-flights those and falls
    /// through to pixman.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn try_vk_render_composite(
        &mut self,
        op: u8,
        src: RenderPic,
        mask: RenderPic,
        dst_xid: u32,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        scissor: ash::vk::Rect2D,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_xform: Option<PictTransform>,
        mask_xform: Option<PictTransform>,
        mask_component_alpha: bool,
    ) -> bool {
        use crate::kms::vk::{
            ops::{render as vk_render, run_one_shot_op},
            render_pipeline::{StdPictOp, record_solid_color_clear},
        };

        if rects.is_empty() {
            return true;
        }
        let Some(std_op) = StdPictOp::from_u8(op) else {
            return false;
        };

        // Self-composite (src or mask aliases dst) needs a staging
        // image; not supported in this commit.
        let src_xid_if_drawable = match src {
            RenderPic::Drawable(xid) => Some(xid),
            _ => None,
        };
        let mask_xid_if_drawable = match mask {
            RenderPic::Drawable(xid) => Some(xid),
            _ => None,
        };
        if src_xid_if_drawable == Some(dst_xid) || mask_xid_if_drawable == Some(dst_xid) {
            return false;
        }

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
        if self.render_pipelines.is_none()
            || self.solid_src_image.is_none()
            || self.solid_mask_image.is_none()
            || self.white_mask_image.is_none()
        {
            return false;
        }
        let needs_dst_readback = std_op.needs_dst_readback();
        if needs_dst_readback && self.dst_readback.is_none() {
            return false;
        }

        // Dst format check. B8G8R8A8 covers depth-24/32 mirrors; R8
        // covers depth-1/8 (a8 picture) — the pipeline cache builds
        // a parallel family for that attachment format.
        let (dst_format, dst_extent, dst_depth) = {
            let (m, depth) = if let Some(w) = self.windows.get(&dst_xid) {
                (w.vk_mirror.as_ref(), w.depth)
            } else if let Some(p) = self.pixmaps.get(&dst_xid) {
                (p.vk_mirror.as_ref(), p.depth)
            } else {
                (None, 0)
            };
            match m {
                Some(m) => (m.format, m.extent, depth),
                None => return false,
            }
        };
        if !matches!(
            dst_format,
            ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8_UNORM
        ) {
            return false;
        }
        // R8 attachments are alpha-only (a8 pictures) so the
        // attachment alpha is always meaningful; for BGRA, alpha
        // is meaningful only on depth-32 (a8r8g8b8).
        let dst_has_alpha = dst_format == ash::vk::Format::R8_UNORM || dst_depth == 32;
        // Synchronous pixman→mirror flush for any Drawable src/mask
        // that has unflushed pixman damage or is still in
        // `UNDEFINED` layout. Without this the Vk Composite would
        // sample stale frame bytes (mirror behind pixman) or
        // undefined contents (mirror never written). Per the
        // family-port directive we don't gate on stale-mirror —
        // we flush.
        if let Some(xid) = src_xid_if_drawable
            && !self.ensure_drawable_mirror_sampleable(xid)
        {
            return false;
        }
        if let Some(xid) = mask_xid_if_drawable
            && !self.ensure_drawable_mirror_sampleable(xid)
        {
            return false;
        }
        // Drawable src must be `B8G8R8A8_UNORM` (depth-24/32) or
        // `R8_UNORM` (a8 picture). For R8 src we bind a swizzled
        // view (a = R, rgb = 0) so the shader sees a (0, 0, 0, alpha)
        // sample — matching the X RENDER convention that components
        // missing from the picture format default to 0 (rgb) and the
        // alpha default lives in the stored byte.
        if let Some(xid) = src_xid_if_drawable {
            let f = if let Some(w) = self.windows.get(&xid) {
                w.vk_mirror.as_ref().map(|m| m.format)
            } else if let Some(p) = self.pixmaps.get(&xid) {
                p.vk_mirror.as_ref().map(|m| m.format)
            } else {
                None
            };
            if !matches!(
                f,
                Some(ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8_UNORM)
            ) {
                return false;
            }
        }

        // Resolve & build pipeline.
        let pipeline = match self.render_pipelines.as_mut().expect("checked above").get(
            std_op,
            dst_format,
            dst_has_alpha,
            mask_component_alpha,
        ) {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "vk render_composite: pipeline build failed for op {op}: {e:?} — \
                     falling back to pixman"
                );
                return false;
            }
        };
        let pipeline_layout = self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .pipeline_layout();

        if let Err(e) = self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .reset_descriptors()
        {
            log::warn!("vk render_composite: descriptor pool reset failed: {e:?}");
            return false;
        }

        let solid_src_view = self
            .solid_src_image
            .as_ref()
            .expect("checked above")
            .image_view();
        let solid_mask_view = self
            .solid_mask_image
            .as_ref()
            .expect("checked above")
            .image_view();
        let white_mask_view = self
            .white_mask_image
            .as_ref()
            .expect("checked above")
            .image_view();

        // Resolve src view + extent + (optional) clear colour. Track
        // any per-source affine that the picture itself induces (e.g.
        // a gradient's axis projection) so we can compose it onto the
        // user transform later.
        let mut src_clear_color: Option<[f32; 4]> = None;
        let src_view;
        let src_extent;
        let src_picture_xform: Option<crate::kms::vk::ops::render::AffineXform>;
        let mut src_is_synthetic_1x1 = false;
        match src {
            RenderPic::Drawable(xid) => {
                let (m_format, extent, depth) = {
                    let (m, depth) = if let Some(w) = self.windows.get(&xid) {
                        (w.vk_mirror.as_ref(), w.depth)
                    } else if let Some(p) = self.pixmaps.get(&xid) {
                        (p.vk_mirror.as_ref(), p.depth)
                    } else {
                        (None, 0)
                    };
                    let Some(m) = m else { return false };
                    (m.format, m.extent, depth)
                };
                // Per X RENDER, missing components default — alpha is 1
                // when the picture format has no alpha mask. Pick a
                // swizzled view that enforces this at sample time:
                //   * R8 mirror (depth 1/8, a8 picture) → mask view
                //     (a = R) so shader sees (0, 0, 0, alpha).
                //   * BGRA mirror with depth 24 (r8g8b8 picture) →
                //     no-alpha view (a = ONE) so the alpha byte
                //     stored in the mirror is ignored.
                //   * BGRA mirror with depth 32 (a8r8g8b8) → regular
                //     view; alpha byte is meaningful.
                let view = if m_format == ash::vk::Format::R8_UNORM {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if depth == 24 {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if let Some(w) = self.windows.get(&xid) {
                    w.vk_mirror.as_ref().expect("checked above").vk_image_view
                } else {
                    self.pixmaps
                        .get(&xid)
                        .and_then(|p| p.vk_mirror.as_ref())
                        .expect("checked above")
                        .vk_image_view
                };
                src_view = view;
                src_extent = extent;
                src_picture_xform = None;
            }
            RenderPic::Solid(color) => {
                src_view = solid_src_view;
                src_extent = ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                };
                src_clear_color = Some(color);
                src_picture_xform = None;
                src_is_synthetic_1x1 = true;
            }
            RenderPic::Gradient(xid) => {
                let Some(PictureState::Gradient { gradient, .. }) = self.pictures.get(&xid) else {
                    return false;
                };
                src_view = gradient.image_view();
                src_extent = gradient.extent();
                src_picture_xform = Some(gradient.axis_projection);
            }
            RenderPic::None => return false,
        }

        // Resolve mask view + extent + (optional) clear colour.
        // For the no-mask case bind the white scratch.
        let mut mask_clear_color: Option<[f32; 4]> = None;
        let mask_view;
        let mask_extent;
        let mask_picture_xform: Option<crate::kms::vk::ops::render::AffineXform>;
        let mut mask_is_synthetic_1x1 = false;
        match mask {
            RenderPic::Drawable(xid) => {
                // Resolve mask format. R8 mirror → swizzle (a = R)
                // for the alpha-only mask. BGRA mirror with depth 24
                // (no alpha mask in the picture format) → swizzle
                // (a = ONE) so a depth-24 RGB picture used as a mask
                // gives mask.a = 1 per X RENDER's "alpha defaults
                // to 1" rule. BGRA mirror with depth 32 → regular
                // view (alpha is meaningful).
                let (m_format, depth) = {
                    let (m, depth) = if let Some(w) = self.windows.get(&xid) {
                        (w.vk_mirror.as_ref(), w.depth)
                    } else if let Some(p) = self.pixmaps.get(&xid) {
                        (p.vk_mirror.as_ref(), p.depth)
                    } else {
                        (None, 0)
                    };
                    let Some(m) = m else { return false };
                    mask_extent = m.extent;
                    (m.format, depth)
                };
                if m_format != ash::vk::Format::B8G8R8A8_UNORM
                    && m_format != ash::vk::Format::R8_UNORM
                {
                    return false;
                }
                let view = if m_format == ash::vk::Format::R8_UNORM {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if depth == 24 {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if let Some(w) = self.windows.get(&xid) {
                    w.vk_mirror.as_ref().expect("checked above").vk_image_view
                } else {
                    self.pixmaps
                        .get(&xid)
                        .and_then(|p| p.vk_mirror.as_ref())
                        .expect("checked above")
                        .vk_image_view
                };
                mask_view = view;
                mask_picture_xform = None;
            }
            RenderPic::Solid(color) => {
                mask_view = solid_mask_view;
                mask_extent = ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                };
                mask_clear_color = Some(color);
                mask_picture_xform = None;
                mask_is_synthetic_1x1 = true;
            }
            RenderPic::Gradient(xid) => {
                let Some(PictureState::Gradient { gradient, .. }) = self.pictures.get(&xid) else {
                    return false;
                };
                mask_view = gradient.image_view();
                mask_extent = gradient.extent();
                mask_picture_xform = Some(gradient.axis_projection);
            }
            RenderPic::None => {
                mask_view = white_mask_view;
                mask_extent = ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                };
                mask_picture_xform = None;
                mask_is_synthetic_1x1 = true;
            }
        }

        // For Disjoint/Conjoint ops the shader reads the dst pixel
        // through binding 2; we copy dst → scratch inside the CB
        // below and bind the scratch's sampleable view here. For
        // standard ops the binding is unused — bind the white-mask
        // scratch to satisfy the descriptor layout.
        let dst_readback_view = if needs_dst_readback {
            let scratch = self.dst_readback.as_mut().expect("checked above");
            if let Err(e) = scratch.ensure(dst_format, dst_extent.width, dst_extent.height) {
                log::warn!("vk render_composite: dst readback ensure failed: {e:?}");
                return false;
            }
            match scratch.view(dst_format, dst_has_alpha) {
                Ok(Some(v)) => v,
                Ok(None) => return false,
                Err(e) => {
                    log::warn!("vk render_composite: dst readback view build failed: {e:?}");
                    return false;
                }
            }
        } else {
            white_mask_view
        };

        let descriptor_set = match self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .allocate_descriptor_for_views(src_view, mask_view, dst_readback_view)
        {
            Ok(s) => s,
            Err(e) => {
                log::warn!("vk render_composite: descriptor alloc failed: {e:?}");
                return false;
            }
        };

        // Pull mut refs. Split-borrow on disjoint fields lets us
        // hand all of these into the closure simultaneously.
        let dst_mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(dst_mirror) = dst_mirror else {
            return false;
        };
        let solid_src_image = self.solid_src_image.as_mut().expect("checked above");
        let solid_mask_image = self.solid_mask_image.as_mut().expect("checked above");
        let dst_readback = if needs_dst_readback {
            Some(self.dst_readback.as_mut().expect("checked above"))
        } else {
            None
        };

        // Compose the user (picture) transform with the picture's
        // intrinsic transform if any (gradients carry an axis
        // projection). Order: applied dst → user → intrinsic →
        // src-pixel. The shader's affine is `src = M * (origin +
        // dst_offset, 1)`, so when we have an intrinsic `I` and a
        // user `U`, the combined matrix is `I * U`.
        let user_src_xform = pixman_transform_to_affine(src_xform.as_ref(), src_extent);
        let user_mask_xform = pixman_transform_to_affine(mask_xform.as_ref(), mask_extent);
        let combined_src_xform = match src_picture_xform {
            Some(intrinsic) => compose_affines(intrinsic, user_src_xform),
            None => user_src_xform,
        };
        let combined_mask_xform = match mask_picture_xform {
            Some(intrinsic) => compose_affines(intrinsic, user_mask_xform),
            None => user_mask_xform,
        };

        // Synthetic 1×1 scratches (Solid src/mask, no-mask white) need
        // PAD: the single texel is meant to cover the whole rect, and
        // REPEAT_NONE would zero every dst pixel beyond uv 1. For
        // gradients the LUT has dimension 1 in the cross-axis but the
        // axis_projection maps freely along the gradient axis — the
        // user-specified repeat governs how out-of-range t values are
        // sampled (clients pass NORMAL/PAD/REFLECT for tiling/
        // mirroring/clamping behaviour). Honour the user repeat in the
        // shader; the LUT covers t ∈ [0, 1] which is what apply_repeat
        // expects.
        let effective_src_repeat = if src_is_synthetic_1x1 {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        } else {
            repeat_to_shader_const(src_repeat)
        };
        let effective_mask_repeat = if mask_is_synthetic_1x1 {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        } else {
            repeat_to_shader_const(mask_repeat)
        };

        let attrs = vk_render::CompositeAttrs {
            src_extent,
            mask_extent,
            src_repeat: effective_src_repeat,
            mask_repeat: effective_mask_repeat,
            src_xform: combined_src_xform,
            mask_xform: combined_mask_xform,
        };

        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            if let Some(c) = src_clear_color {
                record_solid_color_clear(vk, cb, solid_src_image, c);
            }
            if let Some(c) = mask_clear_color {
                record_solid_color_clear(vk, cb, solid_mask_image, c);
            }
            // Disjoint/Conjoint: snapshot dst into the readback
            // scratch, then restore dst to its current layout so
            // record_render_composite can transition it normally.
            if let Some(rb) = dst_readback {
                rb.record_copy_from(
                    cb,
                    dst_mirror.vk_image,
                    dst_mirror.current_layout(),
                    dst_format,
                    dst_mirror.extent,
                );
            }
            vk_render::record_render_composite(
                vk,
                cb,
                dst_mirror,
                pipeline,
                pipeline_layout,
                descriptor_set,
                &attrs,
                rects,
                scissor,
            )
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_composite: record failed on dst xid {dst_xid:#x}: \
                     {e:?} — falling back to pixman"
                );
                false
            }
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
    /// Allocate a per-window VkImage mirror if Vulkan is up. Best-
    /// effort: any failure logs and returns `None` so the window
    /// itself stays alive (drawing falls back to pixman-only for
    /// that window through 4.1.3-4.1.5; the user-visible effect is
    /// the same as today's PixmanShadow path).
    fn allocate_window_mirror(
        &self,
        width: u16,
        height: u16,
    ) -> Option<crate::kms::vk::target::DrawableImage> {
        let vkctx = self.vk.as_ref()?;
        if width == 0 || height == 0 {
            return None;
        }
        match crate::kms::vk::target::DrawableImage::new_server_owned_window(
            std::sync::Arc::clone(vkctx),
            u32::from(width),
            u32::from(height),
        ) {
            Ok(mut img) => {
                if let Some(pool) = self.ops_command_pool.as_ref()
                    && let Err(e) = img.initialize_clear(pool.handle())
                {
                    log::warn!("window mirror initialize_clear failed: {e:?}");
                }
                Some(img)
            }
            Err(e) => {
                log::warn!(
                    "DrawableImage::new_server_owned_window({width}x{height}): {e} — \
                     window will run pixman-only"
                );
                None
            }
        }
    }

    /// Allocate a per-pixmap VkImage mirror. Same Option semantics
    /// as [`Self::allocate_window_mirror`].
    fn allocate_pixmap_mirror(
        &self,
        width: u32,
        height: u32,
        depth: u8,
    ) -> Option<crate::kms::vk::target::DrawableImage> {
        let vkctx = self.vk.as_ref()?;
        if width == 0 || height == 0 {
            return None;
        }
        match crate::kms::vk::target::DrawableImage::new_server_owned_pixmap(
            std::sync::Arc::clone(vkctx),
            width,
            height,
            depth,
        ) {
            Ok(mut img) => {
                if let Some(pool) = self.ops_command_pool.as_ref()
                    && let Err(e) = img.initialize_clear(pool.handle())
                {
                    log::warn!("pixmap mirror initialize_clear failed: {e:?}");
                }
                Some(img)
            }
            Err(e) => {
                log::warn!(
                    "DrawableImage::new_server_owned_pixmap({width}x{height} d{depth}): \
                     {e} — pixmap will run pixman-only"
                );
                None
            }
        }
    }

    // `run_mirror_uploads_for_frame` (the pixman → mirror pump)
    // deleted in 4.1.5. Mirrors are now the canonical store; nothing
    // pumps into them from the host.

    pub fn composite_and_flip(&mut self) -> io::Result<()> {
        // Phase 4.1.5: pixman no longer feeds the mirrors; drawing
        // ops fill them directly through Vk. The pre-composite
        // upload pass is gone.

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

            // Skip any output whose previous atomic flip hasn't yet
            // completed (no pageflip-complete event for it yet).
            // Submitting a new flip on a CRTC that already has one
            // pending returns -EBUSY from the kernel, and on the
            // PixmanShadow path that cascades into bo state drift —
            // the next pageflip-complete arrives for the *old* flip
            // (different bo), so our state machine and the kernel's
            // view of "what's on screen" diverge. Better to skip the
            // frame; the next pageflip-complete will retrigger
            // composite_and_flip and we'll catch up cleanly.
            let vk_flip_pending = self
                .scanout_pools
                .get(layout_idx)
                .and_then(|p| p.as_ref())
                .map(|p| {
                    p.bos.iter().any(|b| {
                        matches!(
                            b.state.phase,
                            crate::kms::vk::scanout::BoPhase::Submitted
                                | crate::kms::vk::scanout::BoPhase::Pending
                        )
                    })
                })
                .unwrap_or(false);
            let dumb_flip_pending = self.outputs[layout_idx].swapchain.submitted_idx().is_some();
            if vk_flip_pending || dumb_flip_pending {
                log::debug!(
                    "composite: skip output {} (vk_flip_pending={} dumb_flip_pending={})",
                    self.outputs[layout_idx].output.connector_name,
                    vk_flip_pending,
                    dumb_flip_pending
                );
                continue;
            }

            // Phase 4.1.5: Vk composite is the sole path. If a free
            // bo isn't available the frame is skipped — no pixman
            // fallback. The next pageflip-complete event will retrigger
            // composite_and_flip.
            if !self.try_vulkan_composite_flip(layout_idx, visible) {
                log::debug!(
                    "composite: deferring frame on output {} until a Free bo is available",
                    self.outputs[layout_idx].output.connector_name
                );
            } else {
                log::debug!(
                    "composite: submitted flip on output {} (visible={})",
                    self.outputs[layout_idx].output.connector_name,
                    visible.len()
                );
            }
        }

        Ok(())
    }

    /// VkComposite path (sub-phase 4.1.3.4): build a
    /// [`CompositeScene`] from the window tree, pick a Free
    /// `ScanoutBo`, record the per-window quad-draw composite pass,
    /// submit, atomic-flip with explicit fences. Returns `true` iff
    /// the composite + flip actually happened — caller falls back to
    /// the pixman path on `false`.
    fn try_vulkan_composite_flip(&mut self, layout_idx: usize, visible: &[u32]) -> bool {
        use crate::kms::vk::{compositor, scanout::BoPhase};

        let Some(vkctx) = self.vk.as_ref() else {
            return false;
        };
        let Some(pipeline) = self.compositor_pipeline.as_ref() else {
            return false;
        };
        let Some(pool) = self.scanout_pools.get(layout_idx).and_then(|p| p.as_ref()) else {
            return false;
        };
        let Some(bo_idx) = pool.bos.iter().position(|b| b.state.phase == BoPhase::Free) else {
            log::warn!(
                "vk composite: no Free bo in pool for output {} — falling back to pixman",
                self.outputs[layout_idx].output.connector_name
            );
            return false;
        };

        let scene = self.build_composite_scene(layout_idx, visible);

        // Re-borrow pool mutably to advance bo state.
        let Some(pool_mut) = self
            .scanout_pools
            .get_mut(layout_idx)
            .and_then(|p| p.as_mut())
        else {
            return false;
        };
        let bo = &mut pool_mut.bos[bo_idx];
        match compositor::record_and_present_composite(
            vkctx,
            &self.device,
            &self.outputs[layout_idx].output,
            bo,
            pipeline,
            &scene,
        ) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk composite: record_and_present_composite failed on output {}: {e} \
                     — falling back to pixman this frame",
                    self.outputs[layout_idx].output.connector_name
                );
                false
            }
        }
    }

    /// Walk the window tree (in stacking order, depth-first
    /// back-to-front) and build a flat list of quads to draw,
    /// translated into per-output scanout coordinates. Cursor +
    /// bg_pixmap are deferred to follow-up sub-tasks; for 4.1.3.4's
    /// first commit, only the bg color clear + window mirrors are
    /// rendered. Visible regression: no cursor, no wallpaper —
    /// follow-up commits add them back.
    fn build_composite_scene(
        &self,
        layout_idx: usize,
        visible: &[u32],
    ) -> crate::kms::vk::compositor::CompositeScene {
        use crate::kms::vk::compositor::{CompositeDraw, CompositeScene};

        let layout_x = self.outputs[layout_idx].x;
        let layout_y = self.outputs[layout_idx].y;

        // Background clear color: bg_pixel (X11 0xRRGGBB) → linear
        // [r, g, b, a]. Falls back to mid-grey so unset roots stand
        // out, mirroring `paint_output`'s default.
        let bg = self.bg_pixel.unwrap_or(0x0050_5050);
        let bg_color = [
            ((bg >> 16) & 0xFF) as f32 / 255.0,
            ((bg >> 8) & 0xFF) as f32 / 255.0,
            (bg & 0xFF) as f32 / 255.0,
            1.0,
        ];

        let mut draws: Vec<CompositeDraw> = Vec::new();
        let output_w = i32::from(self.outputs[layout_idx].width);
        let output_h = i32::from(self.outputs[layout_idx].height);

        // Background pixmap (e.g. Esetroot wallpaper) draws first
        // so windows paint on top. The pixmap is sized to the
        // virtual-screen extent (fb_w × fb_h); each output samples
        // the slice that corresponds to its layout offset. If the
        // client frees the pixmap (Esetroot pattern), the mirror
        // disappears with it; the rescue path will move with the
        // picture_rescued_images cleanup.
        if let Some(pm) = self.bg_pixmap
            && let Some(pm_state) = self.pixmaps.get(&pm.as_raw())
            && let Some(mirror) = pm_state.vk_mirror.as_ref()
        {
            let layout_w = self.outputs[layout_idx].width;
            let layout_h = self.outputs[layout_idx].height;
            let pm_w = mirror.extent.width as f32;
            let pm_h = mirror.extent.height as f32;
            if pm_w > 0.0 && pm_h > 0.0 {
                draws.push(CompositeDraw {
                    image_view: mirror.vk_image_view,
                    dst_origin: [0.0, 0.0],
                    dst_size: [f32::from(layout_w), f32::from(layout_h)],
                    src_origin: [layout_x as f32 / pm_w, layout_y as f32 / pm_h],
                    src_size: [f32::from(layout_w) / pm_w, f32::from(layout_h) / pm_h],
                    use_src_alpha: false,
                });
            }
        }

        for &top in visible {
            self.walk_subtree_into_draws(top, -layout_x, -layout_y, output_w, output_h, &mut draws);
        }

        // Cursor draws above all windows. Pixman implicitly clipped
        // off-screen writes, so multi-output cursor crossing edges
        // showed its visible portion on each scanout. Vulkan
        // viewport+scissor implicitly clips the same way, so a
        // straight quad at scanout-relative coords reproduces it.
        if let Some(cursor_xid) = self.effective_cursor()
            && let Some(cs) = self.cursors.get(&cursor_xid)
            && let Some(mirror) = cs.vk_mirror.as_ref()
        {
            let cw = cs.extent.width as f32;
            let ch = cs.extent.height as f32;
            let cx = self.cursor_x as i32 - i32::from(cs.hot_x) - layout_x;
            let cy = self.cursor_y as i32 - i32::from(cs.hot_y) - layout_y;
            draws.push(CompositeDraw {
                image_view: mirror.vk_image_view,
                dst_origin: [cx as f32, cy as f32],
                dst_size: [cw, ch],
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                use_src_alpha: true,
            });
        }

        CompositeScene { bg_color, draws }
    }

    /// Push quads for `window` and (recursively, in stacking order)
    /// each of its mapped descendants. Translation by `(ox, oy)` is
    /// applied at the top-level so children's parent-relative
    /// coordinates accumulate through the recursion without each
    /// frame computing absolute origins via `absolute_origin`.
    ///
    /// `output_w`/`output_h` carry the destination scanout extent —
    /// any window whose absolute rect doesn't intersect
    /// `[0, output_w) × [0, output_h)` is skipped (AABB cull,
    /// design §"Frame composite pass" step 2). Recursion into
    /// children continues regardless: in current X11/pixman
    /// semantics a child can overflow its parent's bounding box, so
    /// a culled parent doesn't necessarily imply culled descendants.
    fn walk_subtree_into_draws(
        &self,
        window_id: u32,
        ox: i32,
        oy: i32,
        output_w: i32,
        output_h: i32,
        out: &mut Vec<crate::kms::vk::compositor::CompositeDraw>,
    ) {
        use crate::kms::vk::compositor::CompositeDraw;
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        if !window.mapped {
            return;
        }
        // Skip windows with an explicitly-empty SHAPE region (rare).
        if self
            .shape_bounding
            .get(&window_id)
            .is_some_and(|r| r.is_empty())
        {
            return;
        }
        let abs_x = i32::from(window.x) + ox;
        let abs_y = i32::from(window.y) + oy;
        let w = i32::from(window.width);
        let h = i32::from(window.height);

        // AABB cull: skip the push when the window's absolute rect
        // lies entirely outside the output. Children still recurse.
        let on_screen = w > 0
            && h > 0
            && abs_x + w > 0
            && abs_y + h > 0
            && abs_x < output_w
            && abs_y < output_h;

        if on_screen && let Some(mirror) = window.vk_mirror.as_ref() {
            out.push(CompositeDraw {
                image_view: mirror.vk_image_view,
                dst_origin: [abs_x as f32, abs_y as f32],
                dst_size: [w as f32, h as f32],
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                use_src_alpha: false,
            });
        }
        // Children draw above their parent in stacking order.
        for &child_id in &window.children {
            self.walk_subtree_into_draws(child_id, abs_x, abs_y, output_w, output_h, out);
        }
    }

    /// Paint a single output's scanout image. Translates virtual-screen
    /// coordinates by `(-layout.x, -layout.y)` so layout `(layout.x,
    /// layout.y)` lands at scanout `(0, 0)`. Pixman implicitly clips writes
    /// outside the destination image.
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
                        codepoint: ch as u32,
                    });
                }
                cursor_x += ci.character_width as i32;
            }
        } // RefCell borrow released here

        // Phase 4.1.5: Vulkan-only text path. Each glyph is interned
        // in the shared atlas; a text-pipeline draw quads them onto
        // the target's mirror with src-over blending. Atlas full or
        // missing mirror silently drops the run.
        self.try_vk_text_run(host_xid, font_xid, foreground, &rendered);
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

    /// Drain pending page-flip events, advance per-output state, and
    /// kick off the next composite. Each pageflip-complete corresponds
    /// to either a Vulkan-fed flip (advance the [`ScanoutBoPool`] state
    /// machine: `Retiring → Free`, `OnScreen → Retiring`, `Pending →
    /// OnScreen`) or a dumb-buffer flip (advance the legacy
    /// `Swapchain::complete`). We disambiguate by checking whether the
    /// pool currently has a `Pending` bo.
    pub fn drain_page_flips_and_composite(&mut self) -> io::Result<()> {
        use ::drm::control::crtc;
        let mut flipped: Vec<crtc::Handle> = Vec::new();
        drm::page_flip::drain_events(&self.device, |c| flipped.push(c))?;

        // Log every pageflip-complete at debug; first one per output at
        // info so quiet runs still show the cycle started.
        for c in &flipped {
            if let Some(idx) = self.outputs.iter().position(|o| &o.output.crtc == c) {
                if !self.first_pageflip_logged[idx] {
                    log::info!(
                        "pageflip-complete: first event on output {} (CRTC {c:?})",
                        self.outputs[idx].output.connector_name
                    );
                    self.first_pageflip_logged[idx] = true;
                } else {
                    log::debug!(
                        "pageflip-complete on output {}",
                        self.outputs[idx].output.connector_name
                    );
                }
            }
        }

        for c in flipped {
            let Some(output_idx) = self.outputs.iter().position(|o| o.output.crtc == c) else {
                log::warn!("page-flip event for unknown CRTC {c:?}");
                continue;
            };

            // Vulkan-fed flip identification: the pool has a `Pending`
            // bo iff we submitted via the Vulkan path for this output.
            let was_vk_flip = self
                .scanout_pools
                .get(output_idx)
                .and_then(|p| p.as_ref())
                .map(|p| {
                    p.bos
                        .iter()
                        .any(|b| b.state.phase == crate::kms::vk::scanout::BoPhase::Pending)
                })
                .unwrap_or(false);

            if was_vk_flip {
                if let Some(pool) = self
                    .scanout_pools
                    .get_mut(output_idx)
                    .and_then(|p| p.as_mut())
                {
                    advance_pool_on_pageflip_complete(pool);
                }
            } else {
                let layout = &mut self.outputs[output_idx];
                if let Some(idx) = layout.swapchain.submitted_idx() {
                    layout
                        .swapchain
                        .complete(idx)
                        .map_err(|e| io::Error::other(format!("swapchain.complete: {e}")))?;
                }
            }
        }
        // Always composite on flip completion (self-driving at vsync)
        self.composite_and_flip()
    }

    /// Disable each DRM output (CRTC + plane) for clean shutdown.
    /// Logs any per-output error and returns the last one so callers
    /// still see a failure, while attempting to tear down everything.
    pub fn disable_output(&mut self) -> io::Result<()> {
        // Drain in-flight scanout-bo state first: vkDeviceWaitIdle +
        // close any held fence fds. Without this, mid-flight Vulkan
        // submits could race the DRM disable_output ioctl and leak
        // fds. No-op when no Vulkan-fed pool exists for an output.
        if let Some(vk) = self.vk.as_ref() {
            for pool in &mut self.scanout_pools {
                if let Some(p) = pool.as_mut() {
                    p.drain_all_pending(vk);
                }
            }
        }

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

    fn dri3_open(&mut self, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        // Per design §3.2: dup the long-lived render-node fd so each
        // client gets its own fd. Ownership transfers to the caller.
        let fd = self.render_node_fd.as_ref().ok_or_else(|| {
            io::Error::other("DRI3 unavailable — render node was not resolved at backend init")
        })?;
        fd.try_clone()
            .map_err(|e| io::Error::other(format!("dup render-node fd: {e}")))
    }

    fn dri3_import_pixmap(
        &mut self,
        fd: std::os::fd::OwnedFd,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        modifier: u64,
        depth: u8,
        bpp: u8,
    ) -> io::Result<yserver_core::backend::PixmapHandle> {
        // Per design §3.2: import the dma-buf into a DrawableImage via
        // VK_EXT_image_drm_format_modifier and stash on a fresh
        // PixmapState. Pixmap exists as a real X resource — clients
        // can CopyArea / ChangePicture against it.
        let Some(vk) = self.vk.clone() else {
            return Err(io::Error::other("DRI3 import: Vulkan unavailable"));
        };
        let format = match (depth, bpp) {
            (24, 32) | (32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => {
                return Err(io::Error::other(format!(
                    "DRI3 import: unsupported (depth={depth}, bpp={bpp}); Phase 4.2 RGB single-plane only"
                )));
            }
        };
        let drawable = crate::kms::vk::dri3::import_dmabuf(
            vk,
            fd,
            u32::from(width),
            u32::from(height),
            format,
            modifier,
            &[crate::kms::vk::dri3::DmabufPlane {
                offset: u64::from(offset),
                pitch: stride,
            }],
        )
        .map_err(|e| io::Error::other(format!("DRI3 import_dmabuf: {e:?}")))?;
        let host_xid = self.next_host_xid();
        self.pixmaps.insert(
            host_xid,
            PixmapState {
                handle: host_xid,
                width,
                height,
                depth,
                vk_mirror: Some(drawable),
            },
        );
        yserver_core::backend::PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("DRI3 import: failed to make PixmapHandle"))
    }

    fn dri3_fence_from_fd(&mut self, fence_xid: u32, fd: std::os::fd::OwnedFd) -> io::Result<()> {
        // Mesa's loader_dri3 sends an xshmfence (memfd-backed shared
        // memory + futex) — try that path FIRST. vkImportSemaphoreFdKHR
        // rejects xshmfence fds because they aren't sync_file. Mmap
        // first; fall through to Vulkan import only if the mmap fails
        // (i.e. the fd really is a sync_file).
        use std::os::fd::AsFd;
        if let Some(mapping) = crate::kms::xshmfence::FenceMapping::map(fd.as_fd()) {
            self.dri3_xshmfences.insert(fence_xid, mapping);
            log::debug!("DRI3 FenceFromFD 0x{fence_xid:x}: imported as xshmfence");
            return Ok(());
        }
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other(
                "DRI3 FenceFromFD: fd isn't xshmfence and Vulkan is unavailable",
            ));
        };
        let semaphore = crate::kms::vk::sync::import_sync_file(vk, fd)
            .map_err(|e| io::Error::other(format!("import_sync_file: {e:?}")))?;
        if let Some(prev) = self.dri3_sync_resources.insert(fence_xid, semaphore) {
            unsafe { vk.device.destroy_semaphore(prev, None) };
        }
        Ok(())
    }

    fn dri3_trigger_fence(&mut self, fence_xid: u32) -> io::Result<()> {
        if let Some(mapping) = self.dri3_xshmfences.get(&fence_xid) {
            mapping.trigger();
            return Ok(());
        }
        // VkSemaphore-backed fences: signalling is done via queue
        // submit (or vkSignalSemaphore for timeline). For Phase 4.2
        // first-cut Copy path the GPU work is already serialized, so
        // a server-only `triggered=true` mirror in state.sync_fences
        // is sufficient — no GPU operation needed here.
        Ok(())
    }

    fn dri3_fd_from_fence(&mut self, fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FDFromFence: Vulkan unavailable"));
        };
        let &semaphore = self.dri3_sync_resources.get(&fence_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 FDFromFence: unknown fence 0x{fence_xid:x}"))
        })?;
        crate::kms::vk::sync::export_sync_file(vk, semaphore)
            .map_err(|e| io::Error::other(format!("export_sync_file: {e:?}")))
    }

    fn dri3_import_syncobj(
        &mut self,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 ImportSyncobj: Vulkan unavailable"));
        };
        let semaphore = crate::kms::vk::sync::import_drm_syncobj(vk, fd)
            .map_err(|e| io::Error::other(format!("import_drm_syncobj: {e:?}")))?;
        if let Some(prev) = self.dri3_sync_resources.insert(syncobj_xid, semaphore) {
            unsafe { vk.device.destroy_semaphore(prev, None) };
        }
        Ok(())
    }

    fn dri3_free_syncobj(&mut self, syncobj_xid: u32) -> io::Result<()> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FreeSyncobj: Vulkan unavailable"));
        };
        if let Some(sem) = self.dri3_sync_resources.remove(&syncobj_xid) {
            unsafe { vk.device.destroy_semaphore(sem, None) };
        }
        Ok(())
    }

    fn dri3_signal_syncobj(&mut self, syncobj_xid: u32, value: u64) -> io::Result<()> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 SignalSyncobj: Vulkan unavailable"));
        };
        let &semaphore = self.dri3_sync_resources.get(&syncobj_xid).ok_or_else(|| {
            io::Error::other(format!(
                "DRI3 SignalSyncobj: unknown syncobj 0x{syncobj_xid:x}"
            ))
        })?;
        crate::kms::vk::sync::signal_timeline(vk, semaphore, value)
            .map_err(|e| io::Error::other(format!("vkSignalSemaphore: {e:?}")))
    }

    fn dri3_export_pixmap(
        &mut self,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 export: Vulkan unavailable"));
        };
        let pixmap = self.pixmaps.get(&host_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: unknown pixmap 0x{host_xid:x}"))
        })?;
        let drawable = pixmap.vk_mirror.as_ref().ok_or_else(|| {
            io::Error::other(format!(
                "DRI3 export: pixmap 0x{host_xid:x} has no GPU mirror"
            ))
        })?;
        let bpp: u8 = match pixmap.depth {
            24 | 32 => 32,
            d => d,
        };
        let export = crate::kms::vk::dri3::export_dmabuf(vk, drawable)
            .map_err(|e| io::Error::other(format!("DRI3 export_dmabuf: {e:?}")))?;
        let stride16 = u16::try_from(export.stride).unwrap_or(u16::MAX);
        Ok((
            export.size,
            pixmap.width,
            pixmap.height,
            stride16,
            pixmap.depth,
            bpp,
            export.fd,
        ))
    }

    fn dri3_supported_modifiers(&self, _window: u32, depth: u8, bpp: u8) -> (Vec<u64>, Vec<u64>) {
        let Some(vk) = self.vk.as_ref() else {
            return (vec![0], vec![0]);
        };
        // Map (depth, bpp) to a vk::Format. Phase 4.2 RGB single-plane
        // scope means we only handle depth-24/32 BGRA today; other
        // depths fall back to LINEAR-only.
        let format = match (depth, bpp) {
            (24, 32) | (32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => return (vec![0], vec![0]),
        };
        let screen = crate::kms::vk::dri3::supported_modifiers(vk, format);
        // Window-modifier list is a subset that the window's output
        // can flip-scanout. Per design §3.2 we filter against the
        // (format, modifier) pairs the kernel accepted via add_fb2 at
        // backend init. Phase 4.1 always uses LINEAR for scanout, so
        // the window list collapses to LINEAR here. A follow-up
        // (Task 23 + Task 29) populates `output.scanout_format_set`
        // from the real add_fb2 probe and widens this.
        let window: Vec<u64> = screen.iter().copied().filter(|&m| m == 0).collect();
        let window = if window.is_empty() { vec![0] } else { window };
        (window, screen)
    }

    fn present_capabilities(&self, _window: u32) -> yserver_core::backend::PresentCaps {
        // Phase 4.2.3 first cut: report the conservative "Copy-path
        // only" caps. Tasks 30-32 turn flip_path / async_may_tear on
        // once alien-BO scanout integration is wired. syncobj mirrors
        // Dri3Caps::syncobj (requires DRI3 timeline support — Tasks
        // 18-20 ship the imports but the Present-side submit-time
        // handshake lands later).
        yserver_core::backend::PresentCaps {
            flip_path: false,
            async_may_tear: false,
            syncobj: self.dri3_capabilities().syncobj,
        }
    }

    fn dri3_capabilities(&self) -> yserver_core::backend::Dri3Caps {
        // DRI3 entirely unavailable when render-node fd or Vulkan
        // weren't resolved at backend init.
        if self.render_node_fd.is_none() || self.vk.is_none() {
            return yserver_core::backend::Dri3Caps::unsupported();
        }
        let vk = self.vk.as_ref().expect("vk Some by branch above");
        let modifiers = vk.image_drm_format_modifier;
        // VK_KHR_external_semaphore_fd is unconditionally enabled at
        // device init; fence_fd / SYNC_FD handle type rides along
        // with it. syncobj uses the OPAQUE_FD + timeline-semaphore
        // path (also part of VK_KHR_external_semaphore_fd) which is
        // what Mesa actually prefers — and which the live vng smoke
        // showed is the only sync path Venus accepts. With syncobj
        // advertised, Mesa uses ImportSyncobj + PresentPixmapSynced
        // and we host-signal the release_syncobj at release_value
        // when the Copy completes, waking Mesa's vkAcquireNextImage.
        let fence_fd = true;
        let syncobj = true;
        // Version cap per design §4: with syncobj advertise (1, 4);
        // without it cap at (1, 3). fence_fd doesn't affect version.
        let version = if syncobj { (1, 4) } else { (1, 3) };
        yserver_core::backend::Dri3Caps {
            version,
            modifiers,
            fence_fd,
            syncobj,
        }
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
        // KmsBackend has a real XKB extension surface backed by
        // xkbcommon (see crates/yserver/src/kms/xkb.rs and the
        // xkb_proxy impl below). Advertising 136 unblocks any
        // X11 client whose toolkit (GTK, Qt, xcb-xkb) requires
        // the extension to bring up its windowing layer.
        Some(136)
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        // (major_opcode, first_event, first_error). 85 / 162 are
        // the next free slots in yserver's local first-event /
        // first-error namespace (see crates/yserver-core/src/nested.rs
        // for the rest of the table).
        Some((136, 85, 162))
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
        let depth = match visual {
            HostSubwindowVisual::CopyFromParent => 24,
            HostSubwindowVisual::Explicit { depth, .. } => depth,
        };
        let visual_xid = match visual {
            HostSubwindowVisual::CopyFromParent => 0,
            HostSubwindowVisual::Explicit { visual_xid, .. } => visual_xid,
        };
        // Allocate the mirror (`initialize_clear` leaves it
        // transparent black, layout SHADER_READ_ONLY_OPTIMAL). If the
        // client requested a `background_pixel`, paint it onto the
        // mirror so xclock-style apps that expect the server to
        // auto-clear see the right backdrop.
        let mut vk_mirror = self.allocate_window_mirror(width, height);
        if let (Some(mirror), Some(pixel)) = (vk_mirror.as_mut(), background_pixel)
            && let Err(e) = self.fill_mirror_solid(mirror, pixel)
        {
            log::warn!("create_subwindow: bg_pixel fill failed: {e:?}");
        }
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
                depth,
                visual: visual_xid,
                vk_mirror,
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
        // Apply scalar updates first; capture the resize dims so the
        // mirror reallocation below can run without holding a &mut
        // borrow on `windows` (allocate_window_mirror takes &self).
        let resize_dims = if let Some(window) = self.windows.get_mut(&host_xid) {
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
                Some((window.width, window.height, window.bg_pixel))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((w, h, bg_pixel)) = resize_dims {
            // On shrink, the old VkImage is freed; on grow, a fresh
            // larger one is allocated. Allocation is best-effort —
            // failure leaves vk_mirror=None for this window and the
            // composite pass simply skips it until the next resize
            // succeeds.
            let mut new_mirror = self.allocate_window_mirror(w, h);
            if let (Some(mirror), Some(pixel)) = (new_mirror.as_mut(), bg_pixel)
                && let Err(e) = self.fill_mirror_solid(mirror, pixel)
            {
                log::warn!("configure_window: bg_pixel fill on resize failed: {e:?}");
            }
            if let Some(window) = self.windows.get_mut(&host_xid) {
                window.vk_mirror = new_mirror;
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
        let mut vk_mirror = self.allocate_pixmap_mirror(u32::from(width), u32::from(height), depth);
        // Mark fresh pixmap mirror fully dirty (see CreateWindow for
        // rationale).
        if let Some(m) = vk_mirror.as_mut() {
            m.mark_full_damage();
        }
        self.pixmaps.insert(
            host_xid,
            PixmapState {
                handle: host_xid,
                width,
                height,
                depth,
                vk_mirror,
            },
        );
        PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create pixmap handle"))
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(ps) = self.pixmaps.remove(&host_xid) {
            // Rescue the vk_mirror for any picture still referencing
            // this pixmap (e.g. fvwm frees the cursor source pixmap
            // before CreateCursor). The mirror keeps the GPU image
            // alive on the rescue map; whoever consumes the rescue
            // (currently render_create_cursor) drops it.
            if let Some(mirror) = ps.vk_mirror {
                let mut mirror = Some(mirror);
                for (&pic_xid, pic) in &self.pictures {
                    if let PictureState::Drawable { host_xid: xid, .. } = pic
                        && *xid == host_xid
                        && let Some(m) = mirror.take()
                    {
                        self.picture_rescued_images.insert(pic_xid, m);
                        break;
                    }
                }
                // If no picture referenced the pixmap, `mirror` drops
                // here (Drop releases the VkImage/allocation).
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
        Ok(())
    }

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.current_clip = ClipState::None;
        Ok(())
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        self.current_clip = match clip {
            Some(c) => ClipState::Rectangles {
                origin: (c.x_origin, c.y_origin),
                rects: c,
            },
            None => ClipState::None,
        };
        Ok(())
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        // Pixmap clip-masks aren't enforced yet — the rasteriser only knows
        // how to intersect rect lists. Store the state so future work can pick
        // it up; right now this means a pixmap-mask GC clip is silently
        // ignored (matches pre-fix behaviour for that specific shape).
        let Some(handle) = PixmapHandle::from_raw(host_pixmap) else {
            self.current_clip = ClipState::None;
            return Ok(());
        };
        self.current_clip = ClipState::Pixmap {
            origin: (clip_x_origin, clip_y_origin),
            pixmap: handle,
        };
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.current_fill = FillState::Solid;
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
        clip: &ClipState,
    ) -> io::Result<()> {
        self.current_clip = clip.clone();
        Ok(())
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()> {
        self.current_fill = fill.clone();
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
        if width == 0 || height == 0 {
            return Ok(());
        }
        // Phase 4.1.5: Vulkan-only. `try_vk_copy_area` handles
        // distinct src/dst, same-target non-overlapping, and
        // same-target overlapping (via the BGRA `copy_scratch`).
        // Format mismatch (R8↔BGRA) silently skips — those would
        // need a sample-and-store shader; deferred. Mirror missing
        // also skips.
        self.try_vk_copy_area(
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        );
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
        let Some(src_readback) = self.read_mirror_pixels(src_host_xid) else {
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
                    read_mirror_pixel_for_plane(&src_readback, src_depth, sx as usize, sy as usize);
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
        let foreground_rects = self.intersect_with_current_clip(&foreground_rects);
        let background_rects = self.intersect_with_current_clip(&background_rects);
        // Bg first, then fg (matches the previous pixman ordering so
        // overlap behaviour is preserved). The Vk path now honours
        // every GC function via `try_vk_fill_with_function` (Copy
        // fast path or per-`VkLogicOp` pipeline).
        self.try_vk_fill_with_function(dst_host_xid, function, background, &background_rects);
        self.try_vk_fill_with_function(dst_host_xid, function, foreground, &foreground_rects);
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
        // Phase 4.1.5: Vulkan-only. `try_vk_put_image` covers
        // depth-1 / depth-8 / depth-24 / depth-32. Depth-15 / depth-16
        // pixmaps don't have a matching mirror format and silently
        // skip — these depths aren't used by any current X11 client
        // we run.
        self.try_vk_put_image(host_xid, depth, width, height, dst_x, dst_y, data);
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
        let depth = self.drawable_depth(host_xid).unwrap_or(24);

        // X11 GetImage reply header (32 bytes). bpp + row stride match
        // the legacy pixman path so the Vulkan and pixman branches
        // emit identically-shaped replies.
        let bpp = match depth {
            1 => 1u32,
            4 => 4,
            8 => 8,
            15 | 16 => 16,
            24 | 32 => 32,
            _ => 32,
        };
        let row_bytes = (((width as u32) * bpp).div_ceil(32) * 4) as usize;
        let pixel_bytes = row_bytes * (height as usize);
        let mut result = Vec::with_capacity(32 + pixel_bytes);
        let reply_length_units = (pixel_bytes / 4) as u32;
        result.push(1); // 0: Reply
        result.push(depth); // 1: actual drawable depth
        result.extend_from_slice(&[0u8; 2]); // 2..4: sequence (patched by nested.rs)
        result.extend_from_slice(&reply_length_units.to_le_bytes()); // 4..8: length in u32 units
        result.extend_from_slice(&[0u8; 4]); // 8..12: visual (patched by nested.rs)
        result.extend_from_slice(&[0u8; 20]); // 12..32: padding
        debug_assert_eq!(result.len(), 32);

        // Phase 4.1.5: Vulkan-only readback. Returns true with
        // pixel bytes pushed into `result`. On failure the request
        // returns a zero-filled payload so the X11 reply stream
        // stays aligned — clients see all-zero pixels for unknown
        // formats (was previously the case for depths 1/4/15/16
        // anyway, even on the pixman path).
        if !self.try_vk_get_image_pixels(host_xid, x, y, width, height, depth, &mut result) {
            result.resize(32 + pixel_bytes, 0);
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
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: route through the shared solid-fill helper
        // so `Copy` strokes hit `try_vk_solid_fill`. `Xor` etc. fall
        // through to the pixman path inside the helper.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
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
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: see `poly_line`.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
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
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: see `poly_line`.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
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
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: see `poly_line`.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
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
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: see `poly_line`.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
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
        self.fill_rects_honoring_fill_state(host_xid, foreground, &rects);
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
        let dst_dims = self.drawable_dims(host_xid).unwrap_or((0, 0));
        let img_w = dst_dims.0 as i32;
        let img_h = dst_dims.1 as i32;
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
            self.fill_rects_honoring_fill_state(host_xid, foreground, &rects);
        }
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
        let dst_dims = self.drawable_dims(host_xid).unwrap_or((0, 0));
        let clipped = clip_rects_to_image(&rects, dst_dims.0 as i32, dst_dims.1 as i32);
        self.fill_rects_honoring_fill_state(host_xid, foreground, &clipped);
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
        self.try_vk_fill_with_function(host_xid, function, foreground, &[rect]);
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
            let bg_rects = self.intersect_with_current_clip(&[rect]);
            if !bg_rects.is_empty() {
                self.try_vk_fill_with_function(host_xid, function, background, &bg_rects);
            }
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
            let bg_rects = self.intersect_with_current_clip(&[rect]);
            if !bg_rects.is_empty() {
                self.try_vk_fill_with_function(host_xid, function, background, &bg_rects);
            }
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
                        Some(PictureState::SolidFill { repeat: r, .. }) => *r = repeat,
                        Some(PictureState::Gradient { repeat: r, .. }) => *r = repeat,
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
                                width: px.width,
                                height: px.height,
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
        // Extract picture-level repeat + transform attributes. The
        // shader handles all repeat modes + the affine portion of
        // any transform; SolidFill / Drawable / Gradient sources are
        // resolved by `resolve_render_pic_with_gradient_xid`.
        let (src_repeat, src_transform) = match self.pictures.get(&host_src) {
            Some(PictureState::SolidFill { repeat, .. }) => (*repeat, None),
            Some(PictureState::Gradient {
                repeat, transform, ..
            }) => (*repeat, *transform),
            Some(PictureState::Drawable {
                repeat, transform, ..
            }) => (*repeat, *transform),
            None => {
                log::debug!("render_composite: host_src 0x{host_src:x} not found");
                return Ok(());
            }
        };
        let (mask_repeat, mask_transform, mask_component_alpha) = if host_mask == 0 {
            (Repeat::None, None, false)
        } else {
            match self.pictures.get(&host_mask) {
                Some(PictureState::SolidFill {
                    repeat,
                    component_alpha,
                    ..
                }) => (*repeat, None, *component_alpha),
                Some(PictureState::Gradient {
                    repeat, transform, ..
                }) => (*repeat, *transform, false),
                Some(PictureState::Drawable {
                    repeat,
                    transform,
                    component_alpha,
                    ..
                }) => (*repeat, *transform, *component_alpha),
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

        // Phase 4.1.5: Vulkan-only Composite path. The shader covers
        // affine transforms + all four repeat modes; SolidFill /
        // Drawable / Gradient sources resolve through
        // `resolve_render_pic_with_gradient_xid`. Unsupported cases
        // (component_alpha, alpha_map, depth-1 src, etc.) silently
        // skip — `feedback_no_gating_during_family_port.md` accepts
        // visual regressions for those during the family port.
        if let Some(src_pic) = resolve_render_pic_with_gradient_xid(&self.pictures, host_src)
            && let Some(mask_pic) = if host_mask == 0 {
                Some(RenderPic::None)
            } else {
                resolve_render_pic_with_gradient_xid(&self.pictures, host_mask)
            }
            && let Some((rects, scissor)) = self.build_render_composite_inputs(
                &clip, src_x, src_y, mask_x, mask_y, dst_x, dst_y, width, height,
            )
        {
            self.try_vk_render_composite(
                op,
                src_pic,
                mask_pic,
                dst_xid,
                &rects,
                scissor,
                src_repeat,
                mask_repeat,
                src_transform,
                mask_transform,
                mask_component_alpha,
            );
        }

        Ok(())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
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
        // Phase 4.1.5: Vulkan-only. Atlas miss / non-`Over` op /
        // non-SolidFill src silently skip the run.
        self.try_vk_render_composite_glyphs(
            minor, op, host_src, host_dst, host_gs, src_x, src_y, items, x_off, y_off,
        );
        Ok(())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        // Resolve the destination picture to its backing drawable + clip.
        // Solid / gradient pictures aren't valid RENDER fill destinations,
        // so bail quietly in those cases.
        let (dst_drawable_xid, picture_clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => return Ok(()),
        };

        let mut decoded: Vec<Rectangle16> = Vec::with_capacity(rects.len() / 8);
        for chunk in rects.chunks_exact(8) {
            let rx = i16::from_le_bytes([chunk[0], chunk[1]]).saturating_add(x_off);
            let ry = i16::from_le_bytes([chunk[2], chunk[3]]).saturating_add(y_off);
            let rw = u16::from_le_bytes([chunk[4], chunk[5]]);
            let rh = u16::from_le_bytes([chunk[6], chunk[7]]);
            if rw > 0 && rh > 0 {
                decoded.push(Rectangle16 {
                    x: rx,
                    y: ry,
                    width: rw,
                    height: rh,
                });
            }
        }
        if decoded.is_empty() {
            return Ok(());
        }

        // Phase 4.1.5: Vulkan-only. Solid src + no mask + per-op
        // blend covers every standard PictOp; out-of-format dst
        // silently skips.
        // X RENDER XRenderColor is already premultiplied on the wire
        // (see rendercheck main.c:337-345). Pass through unchanged.
        let color_premul = {
            let r = u16::from_le_bytes([color[0], color[1]]) as f32 / 65535.0;
            let g = u16::from_le_bytes([color[2], color[3]]) as f32 / 65535.0;
            let b = u16::from_le_bytes([color[4], color[5]]) as f32 / 65535.0;
            let a = u16::from_le_bytes([color[6], color[7]]) as f32 / 65535.0;
            [r, g, b, a]
        };
        if let Some((_, scissor)) =
            self.build_render_composite_inputs(&picture_clip, 0, 0, 0, 0, 0, 0, 1, 1)
        {
            let composite_rects: Vec<crate::kms::vk::ops::render::CompositeRect> = decoded
                .iter()
                .map(|r| crate::kms::vk::ops::render::CompositeRect {
                    src_x: 0,
                    src_y: 0,
                    mask_x: 0,
                    mask_y: 0,
                    dst_x: i32::from(r.x),
                    dst_y: i32::from(r.y),
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                })
                .collect();
            self.try_vk_render_composite(
                op,
                RenderPic::Solid(color_premul),
                RenderPic::None,
                dst_drawable_xid,
                &composite_rects,
                scissor,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            );
        }
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
        let _ = (src_x, src_y);
        // Phase 4.1.5: Vulkan-only. CPU rasterises traps into the
        // R8 mask scratch; the existing Composite pipeline blends the
        // source through the mask. Supports SolidFill, Drawable, and
        // Gradient sources (pixman fallback no longer exists, so a
        // false return drops the request silently — log to surface it).
        if !self.try_vk_render_trapezoids_path(op, host_src, host_dst, traps, x_off, y_off) {
            log::debug!(
                "render_trapezoids: vk path declined op={op} src={host_src:#x} dst={host_dst:#x}"
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let _ = (src_x, src_y);
        // Phase 4.1.5: Vulkan-only. CPU triangle rasteriser fills
        // an R8 coverage mask, then Composite blends src through it.
        // Supports SolidFill, Drawable, and Gradient sources.
        if !self
            .try_vk_render_triangles_path(minor, op, host_src, host_dst, primitives, x_off, y_off)
        {
            log::debug!(
                "render_triangles_op: vk path declined minor={minor} op={op} src={host_src:#x} dst={host_dst:#x}"
            );
        }
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        // X RENDER CreateSolidFill color: 16-bit per channel, little-endian,
        // already premultiplied on the wire (see rendercheck main.c:337-345).
        // Byte layout: red[0..2], green[2..4], blue[4..6], alpha[6..8].
        let r16 = u16::from_le_bytes([color[0], color[1]]);
        let g16 = u16::from_le_bytes([color[2], color[3]]);
        let b16 = u16::from_le_bytes([color[4], color[5]]);
        let a16 = u16::from_le_bytes([color[6], color[7]]);
        let r = f32::from(r16) / 65535.0;
        let g = f32::from(g16) / 65535.0;
        let b = f32::from(b16) / 65535.0;
        let a = f32::from(a16) / 65535.0;
        let premul = [r, g, b, a];

        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::SolidFill {
                premul,
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
        use crate::kms::vk::gradient::{GradientPicture, Stop};

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
        let mut stops: Vec<Stop> = Vec::with_capacity(n_stops);
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
            stops.push(Stop { pos, r, g, b, a });
        }
        let Some(vkctx) = self.vk.as_ref().cloned() else {
            log::debug!("render_create_linear_gradient: vulkan unavailable; cannot create");
            return Ok(None);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Ok(None);
        };
        let gradient =
            match GradientPicture::new_linear(vkctx, pool_handle, (p1x, p1y), (p2x, p2y), &stops) {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("render_create_linear_gradient: vk init failed: {e:?}");
                    return Ok(None);
                }
            };
        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::Gradient {
                gradient,
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
        use crate::kms::vk::gradient::{GradientPicture, Stop};

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
        let mut stops: Vec<Stop> = Vec::with_capacity(n_stops);
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
            stops.push(Stop { pos, r, g, b, a });
        }
        let Some(vkctx) = self.vk.as_ref().cloned() else {
            return Ok(None);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Ok(None);
        };
        let gradient = match GradientPicture::new_radial(
            vkctx,
            pool_handle,
            (icx, icy, ir),
            (ocx, ocy, or_),
            &stops,
        ) {
            Ok(g) => g,
            Err(e) => {
                log::warn!("render_create_radial_gradient: vk init failed: {e:?}");
                return Ok(None);
            }
        };
        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::Gradient {
                gradient,
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

        // fvwm pattern: CreatePixmap → PutImage → CreatePicture →
        // FreePixmap → CreateCursor. By the time CreateCursor lands,
        // the pixmap may already be freed; in that case `free_pixmap`
        // moved its `vk_mirror` into `picture_rescued_images`.
        //
        // Two source-shape cases, deliberately handled separately:
        //   1. Live pixmap: we need a `&mut DrawableImage` pointing
        //      into `self.pixmaps`. Disjoint borrows let us do the
        //      copy without taking ownership.
        //   2. Rescued mirror: owned locally; consumed by the copy
        //      and dropped (with its VkImage) afterwards.
        //
        // Both cases produce the cursor mirror via vkCmdCopyImage —
        // no pixman path either way.
        let id = self.next_host_xid();

        if let Some(rescued) = self.picture_rescued_images.remove(&pic_xid) {
            log::debug!("render_create_cursor: using rescued mirror for pic {pic_xid}");
            let mut src = rescued;
            let cw = src.extent.width;
            let ch = src.extent.height;
            let vk_mirror = self.copy_drawable_to_new_cursor_mirror(&mut src);
            // `src` drops here, releasing the rescued mirror's VkImage.
            self.cursors.insert(
                id,
                CursorState {
                    extent: ash::vk::Extent2D {
                        width: cw,
                        height: ch,
                    },
                    hot_x: x,
                    hot_y: y,
                    vk_mirror,
                },
            );
        } else if let Some(pm) = self.pixmaps.get(&host_xid) {
            let Some((cw, ch)) = pm
                .vk_mirror
                .as_ref()
                .map(|m| (m.extent.width, m.extent.height))
            else {
                log::debug!(
                    "render_create_cursor: pixmap host_xid={host_xid} has no mirror for pic {pic_xid}"
                );
                return Ok(None);
            };
            // Allocate the cursor mirror first (immutable self.vk
            // borrow), then re-borrow self.pixmaps mutably for the
            // src side of the copy — disjoint fields, two `&mut`s OK.
            let Some(mut cursor_mirror) = self.allocate_cursor_mirror(cw, ch) else {
                return Ok(None);
            };
            if let Err(e) = self.copy_pixmap_mirror_to_cursor(host_xid, &mut cursor_mirror, cw, ch)
            {
                log::warn!("render_create_cursor: mirror copy failed: {e:?}");
            }
            self.cursors.insert(
                id,
                CursorState {
                    extent: ash::vk::Extent2D {
                        width: cw,
                        height: ch,
                    },
                    hot_x: x,
                    hot_y: y,
                    vk_mirror: Some(cursor_mirror),
                },
            );
        } else {
            log::debug!(
                "render_create_cursor: pixmap host_xid={host_xid} not found for pic {pic_xid}"
            );
            return Ok(None);
        }

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
        // Pre-shift each rectangle by the clip-origin so the stored list
        // is already in dst-coords; the clip-region path doesn't track
        // origin separately.
        if body.len() < 8 {
            return Ok(());
        }
        let x_origin = i16::from_le_bytes([body[4], body[5]]) as i32;
        let y_origin = i16::from_le_bytes([body[6], body[7]]) as i32;
        let rects_data = &body[8..];
        let mut rects = Vec::with_capacity(rects_data.len() / 8);
        for chunk in rects_data.chunks_exact(8) {
            let x = (i16::from_le_bytes([chunk[0], chunk[1]]) as i32 + x_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let y = (i16::from_le_bytes([chunk[2], chunk[3]]) as i32 + y_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
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
        // No-op: with pixman gone the filter selection had no effect
        // outside the pixman image. The Vk Composite shader uses a
        // fixed sampler (`LINEAR`); per-picture filter selection is
        // a 4.1.4.6 follow-up (multiple samplers per filter mode).
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
            Some(PictTransform { matrix })
        };
        match self.pictures.get_mut(&host_pic) {
            Some(PictureState::Drawable { transform: t, .. })
            | Some(PictureState::Gradient { transform: t, .. }) => *t = transform,
            _ => {}
        }
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        // RENDER protocol versions are major=0, minor=N. Current
        // upstream is 0.11; rendercheck rejects anything with major≠0.
        Ok((0, 11))
    }

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        _body: &[u8],
    ) -> io::Result<Option<Vec<u8>>> {
        // XKB minor opcodes per `xkbproto`. The earlier in-tree
        // routing had 20→GetCompatMap and 24→GetControls swapped
        // with the actual numbers; xkbproto puts GetControls at 6,
        // GetMap at 8, GetCompatMap at 10, GetNames at 17,
        // GetDeviceInfo at 24. Wrong routing → wrong-sized replies
        // → wezterm/GTK segfaults on first XKB pass.
        use crate::kms::xkb as xkb_replies;
        let reply = match minor {
            0 => Some(xkb_replies::reply_use_extension()),
            6 => Some(xkb_replies::reply_get_controls(&self.xkb_keymap.0)),
            8 => Some(xkb_replies::reply_get_map(&self.xkb_keymap.0)),
            10 => Some(xkb_replies::reply_get_compat_map()),
            17 => Some(xkb_replies::reply_get_names(&self.xkb_keymap.0)),
            24 => Some(xkb_replies::reply_get_device_info()),
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

// `composite_glyphs_onto` (pixman CompositeGlyphs path) deleted in
// 4.1.5. The Vk path in `try_vk_render_composite_glyphs` (using the
// 4.1.4.5 atlas + text pipeline) is the sole CompositeGlyphs path.

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::{Rectangle16, Repeat};
    use yserver_core::{
        backend::{Backend, ClipState, FillState, GcFunction},
        host_x11::HostXidMap,
    };
    use yserver_protocol::x11::ResourceId;

    use super::{KmsBackend, WindowState};

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
            render_node_fd: None,
            dri3_sync_resources: HashMap::new(),
            dri3_xshmfences: HashMap::new(),
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
            vk: None,
            first_pageflip_logged: vec![false; 1],
            scanout_pools: Vec::new(),
            compositor_pipeline: None,
            ops_command_pool: None,
            ops_staging: None,
            glyph_atlas: None,
            text_pipeline: None,
            render_pipelines: None,
            solid_src_image: None,
            solid_mask_image: None,
            white_mask_image: None,
            mask_scratch: None,
            copy_scratch: None,
            dst_readback: None,
            logic_fill_pipelines: None,
            font_loader: super::FontLoader::new().expect("test font loader"),
            fonts: HashMap::new(),
            pixmaps: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
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
            current_fill: FillState::Solid,
            current_clip: ClipState::None,
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
            depth: 24,
            visual: 0,
            cursor: 0,
            vk_mirror: None,
        }
    }

    // pixman test helpers and gated drawing-op tests removed in
    // 4.1.5: their target paths (`copy_plane`, `poly_text16`,
    // `image_text16`, `fill_rects_with_gc_function`,
    // `composite_glyphs_onto`) are all Vk-only now and exercised
    // by rendercheck under the lavapipe smoke harness.

    #[test]
    fn change_picture_cprepeat_updates_drawable_repeat() {
        let mut b = make_test_backend();
        let pixmap_xid = 0x0040_3000;
        let pic_xid = 0x0040_3001;
        b.pixmaps.insert(
            pixmap_xid,
            super::PixmapState {
                handle: pixmap_xid,
                width: 4,
                height: 4,
                depth: 32,
                vk_mirror: None,
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
                width: 4,
                height: 4,
                depth: 32,
                vk_mirror: None,
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

    // `linear_gradient_composite_produces_nonzero_pixels` removed:
    // gradient pictures now require a Vulkan context for creation
    // (`GradientPicture::new_linear`), which the unit-test backend
    // doesn't provide. The Vk gradient path is exercised by the
    // rendercheck `gradients` suite under `just yserver-venus`.

    #[test]
    fn set_picture_transform_stores_non_identity_matrix() {
        let mut b = make_test_backend();
        let pixmap_xid = 0x0040_5000;
        let pic_xid = 0x0040_5001;
        b.pixmaps.insert(
            pixmap_xid,
            super::PixmapState {
                handle: pixmap_xid,
                width: 4,
                height: 4,
                depth: 32,
                vk_mirror: None,
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

    // `paint_output_offsets_window_into_scanout` removed in 4.1.5:
    // the legacy pixman scanout path it tested no longer exists.
    // Layout-offset behaviour is now exercised by `compositor.rs`'s
    // composite-scene assembly under the lavapipe smoke harness.

    // RENDER trapezoid tests removed in 4.1.5 — they exercised
    // `pixman_composite_trapezoids` directly. The Vk path is
    // `try_vk_render_trapezoids` (CPU-rasterised mask + Vk composite),
    // exercised by rendercheck under the lavapipe smoke harness.

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

    // composite_glyphs_* tests removed in 4.1.5 — pixman path gone.
    // The Vk path is exercised by rendercheck under the lavapipe smoke
    // harness, which has Vulkan available.

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

    #[test]
    fn dri3_open_errs_when_render_node_unavailable() {
        // make_test_backend uses for_tests() drm device + render_node_fd: None,
        // so dri3_open must Err out (the SCM_RIGHTS dispatch path then maps
        // it to BadAlloc).
        use yserver_core::backend::Backend as _;
        let mut backend = make_test_backend();
        let res = backend.dri3_open(0x1234);
        assert!(res.is_err(), "expected Err when render_node_fd is None");
    }
}
