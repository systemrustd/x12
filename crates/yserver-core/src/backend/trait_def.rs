//! `Backend` trait — the surface that `nested.rs` calls on the host
//! during request dispatch. Exists primarily as a seam for testing
//! (`RecordingBackend` lives next door, gated `#[cfg(test)]`) and so
//! that Phase 6.3+ can land a KMS backend without touching every call
//! site in `nested.rs`.
//!
//! Method signatures mirror the existing `HostX11Backend::*` methods
//! 1:1 — no `Param` structs, no parameter renaming. The pragmatic
//! guidance for Step 5 is that the trait surface follows existing
//! signatures rather than the plan's draft shape; bundling parameters
//! into structs would cascade into churn at every call site for no
//! gain. Several methods still take raw `u32` host xids rather than
//! handle newtypes for the same reason — call sites pass `u32` from
//! the `ResourceTable`'s `host_xid` field, and rewrapping/unwrapping
//! at every call boundary is noise.
//!
//! Phase 6.3 lands `set_event_sink` and `BackendEventSink` here too;
//! the dispatcher inside `HostX11Backend` feeds the sink via the
//! merged main connection (Step 4 "Big Flip"), and `register_top_level`
//! / `register_subwindow` / `unregister_host_window` migrated onto the
//! trait at Step 6 — replacing the deleted pump-handle wrapper.

use std::io;

use yserver_protocol::x11::{ClipRectangles, FontMetrics, xfixes};

use crate::{
    backend::{
        AnyHandle, ClipState, CursorHandle, DrawState, FillState, FontHandle, GlyphSetHandle,
        OriginContext, PictureHandle, PixmapHandle, WindowHandle,
    },
    host_x11::{
        HostEvent, HostKeyEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap,
        PointerPosition,
    },
};

use crossbeam_channel::Sender;
use yserver_protocol::x11::ResourceId;

#[derive(Debug)]
pub enum BackendEvent {
    HostEvent(HostEvent),
    HostError {
        origin: Option<OriginContext>,
        error: io::Error,
    },
}

pub trait BackendEventSink: Send {
    fn handle_backend_event(&mut self, event: BackendEvent);
}

/// The dynamic backend surface. `Send` is required so that
/// `Arc<Mutex<dyn Backend>>` is `Send + Sync` (`Mutex<T>` is Sync iff
/// `T: Send`). `Sync` on the trait itself is not required because all
/// `Backend` access is mediated through a `Mutex`.
pub trait Backend: Send {
    // ──────────────────────────────────────────────────────────────
    // Lifecycle / state accessors
    // ──────────────────────────────────────────────────────────────

    fn window_id(&self) -> u32;
    fn root_visual_xid(&self) -> u32;
    fn argb_visual_xid(&self) -> Option<u32>;
    fn argb_colormap_xid(&self) -> Option<u32>;
    fn render_opcode(&self) -> Option<u8>;
    fn xkb_opcode(&self) -> Option<u8>;
    fn xkb_info(&self) -> Option<(u8, u8, u8)>;
    fn composite_opcode(&self) -> Option<u8>;
    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32>;
    fn ping(&mut self, origin: Option<OriginContext>) -> io::Result<()>;
    fn set_event_sink(&mut self, sink: Option<Box<dyn BackendEventSink>>);

    // ──────────────────────────────────────────────────────────────
    // Subwindow lifecycle
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn create_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle>;

    fn destroy_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32)
    -> io::Result<()>;

    fn map_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn unmap_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn configure_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()>;

    fn reparent_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()>;

    fn change_subwindow_attributes(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()>;

    fn update_host_event_mask(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: register a freshly-created host top-level so
    /// pointer / expose events on it route to `nested_id` through
    /// `pointer_event_fanout` / `expose_event_fanout`. Replaces the
    /// pre-Step-4 pump-handle `register_top_level`.
    fn register_top_level(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: same as [`Backend::register_top_level`] but
    /// for sub-windows — selects `Exposure` only so pointer events
    /// bubble up to the top-level ancestor where dispatch lives.
    fn register_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: drop a `host_xid → ResourceId` mapping at
    /// DestroyWindow / Reparent-out so stale host events never
    /// misroute.
    fn unregister_host_window(&mut self, host_xid: u32);

    /// Phase 6.3 Step 4: clone of the merged `host_xid → ResourceId`
    /// map. The sink uses this in `pointer_event_fanout` /
    /// `expose_event_fanout`. Pre-Step-4 the map lived behind the
    /// pump handle.
    fn xid_map(&self) -> HostXidMap;

    /// Phase 6.3 Step 4: per-client kb forwarder registers a Sender
    /// here so it receives every host KeyPress / KeyRelease the
    /// dispatcher decodes. Each subscriber applies its own focus
    /// state — mirrors the pre-Step-4 "every client kb pump sees
    /// every key event" shape, just on one merged connection.
    fn add_key_subscriber(&mut self, tx: Sender<HostKeyEvent>);

    fn name_window_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle>;

    // ──────────────────────────────────────────────────────────────
    // Resources (pixmap, font, cursor)
    // ──────────────────────────────────────────────────────────────

    fn create_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle>;

    fn free_pixmap(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn open_font(
        &mut self,
        origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)>;

    fn close_font(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle>;

    fn define_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Container background (root-mapped helpers)
    // ──────────────────────────────────────────────────────────────

    fn set_container_background_pixel(
        &mut self,
        origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()>;

    fn set_container_background_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // GC state (sync points feeding the host's shared GC)
    // ──────────────────────────────────────────────────────────────

    fn clear_clip_rectangles(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    fn set_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()>;

    fn set_clip_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()>;

    fn set_gc_fill_solid(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    fn set_gc_fill_tiled(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()>;

    fn apply_clip_state(
        &mut self,
        origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()>;

    fn apply_fill_state(
        &mut self,
        origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()>;

    fn apply_draw_state(
        &mut self,
        origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Drawing primitives
    //
    // These match the existing `HostX11Backend::*` signatures: they
    // take raw `u32` host xids and a foreground colour (the Phase 6.2
    // additive scope adds `&DrawState` propagation through the
    // `apply_draw_state` sync point above; the methods themselves are
    // unchanged). Phase 6.3+ may collapse the surface as the trait
    // grows additional impls, but for now matching the existing shape
    // keeps Step 5 churn-free at every call site.
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn copy_area(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn copy_plane(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn put_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn get_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>>;

    fn poly_line(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_segment(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()>;

    fn poly_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()>;

    fn poly_point(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()>;

    fn fill_poly(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    fn poly_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // RENDER
    // ──────────────────────────────────────────────────────────────

    fn render_create_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_change_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_free_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()>;

    fn render_create_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>>;

    fn render_free_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()>;

    fn render_add_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()>;

    fn render_free_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite(
        &mut self,
        origin: Option<OriginContext>,
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
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    fn render_fill_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_trapezoids(
        &mut self,
        origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    fn render_create_solid_fill(
        &mut self,
        origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_linear_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_radial_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>>;

    fn render_set_picture_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_set_picture_filter(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_set_picture_transform(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_query_version(&mut self, origin: Option<OriginContext>) -> io::Result<(u32, u32)>;

    // ──────────────────────────────────────────────────────────────
    // Other extensions
    // ──────────────────────────────────────────────────────────────

    fn xkb_proxy(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        body: &[u8],
    ) -> io::Result<Option<Vec<u8>>>;

    fn xfixes_change_cursor_by_name(
        &mut self,
        origin: Option<OriginContext>,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()>;

    fn set_shape_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Misc
    // ──────────────────────────────────────────────────────────────

    fn warp_pointer(
        &mut self,
        origin: Option<OriginContext>,
        dst_host_xid: u32,
        dst_x: i16,
        dst_y: i16,
    ) -> io::Result<()>;

    fn query_pointer(&mut self, origin: Option<OriginContext>) -> io::Result<PointerPosition>;

    fn list_fonts_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>>;

    fn list_fonts_with_info_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>>;

    fn get_atom_name(
        &mut self,
        origin: Option<OriginContext>,
        atom: u32,
    ) -> io::Result<Option<String>>;

    fn get_keyboard_mapping(
        &mut self,
        origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)>;

    fn get_modifier_mapping(&mut self, origin: Option<OriginContext>) -> io::Result<(u8, Vec<u8>)>;
}

// Compile-time assertion that `Backend` is object-safe and that the
// `Arc<Mutex<dyn Backend>>` shape used by the hot-path call sites is
// `Send + Sync` (so worker threads can hold it).
const _: fn() = || {
    fn assert_obj_safe(_: &dyn Backend) {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<std::sync::Arc<std::sync::Mutex<dyn Backend>>>();
    let _ = assert_obj_safe;
};
